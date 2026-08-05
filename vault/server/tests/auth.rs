use std::collections::BTreeSet;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use sqlx::Row;
use vault_server::auth::{
    AuthError, AuthMode, AuthSettings, SessionSecretRequirement, SessionSecretSource,
    SigningKeyring, TrustedProxySet, UserContext, cookie_value, dev_identity,
    effective_admin_from_parts, folder_permission_count_for_group, header_identity, oidc_identity,
    oidc_token_urlsafe, session_identity, sign_session_payload, split_groups,
    verify_session_payload,
};
use vault_server::db;

const TEST_SIGNING_ROOT: &str = "a3f1c9e72b840d56ff196ab30ce2d785914b8c6230e7fa5d4921bc68e30fd754";
const PREVIOUS_SIGNING_ROOT: &str =
    "7d92b4e10a6fc83531de709bca4825f06e13d97a58c02bf46a91e53c7bd80462";
const ROTATION_ROOTS: [&str; 5] = [
    "20ee3593b37f5968968a070c8125dbaa3ab2c5e4c3c7fca1b200c61a3362dc25",
    "1336544b9c387eff17187d18e5418f4afe8d4acf15c8ca550801d670b08c37c7",
    "8e22758496353cb2d3c709657c9432123fff38ff5b2691e073b889a837be9296",
    "671022217d3c023fdbd446f1e98a285f8d2dda481c247533b8c276b06f95f3a9",
    "f21bde1a392007c78dc4bcf1a0a9a74a91ad13de195eda53a5f814b07c993bd7",
];

fn signing_keys(current: &str, previous: &[&str]) -> SigningKeyring {
    SigningKeyring::from_configured(
        current,
        previous.iter().map(|value| (*value).to_string()).collect(),
    )
}

fn explicit_dev_settings(current: &str, previous: &[&str]) -> AuthSettings {
    AuthSettings {
        mode: AuthMode::Dev,
        auth_mode_raw: "dev".to_string(),
        dev_mode: true,
        signing_keys: signing_keys(current, previous),
        session_secret_source: SessionSecretSource::Explicit,
        ..AuthSettings::default()
    }
}

async fn test_pool() -> sqlx::SqlitePool {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    let pool = db::connect(&db_path).await.expect("db connect");
    // Keep the temp directory alive for the life of the process by leaking it;
    // integration tests use one short-lived SQLite database per test.
    let _ = Box::leak(Box::new(temp_dir));
    pool
}

async fn hold_sqlite_writer(pool: &sqlx::SqlitePool) -> sqlx::pool::PoolConnection<sqlx::Sqlite> {
    let mut connection = pool.acquire().await.expect("writer connection");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *connection)
        .await
        .expect("hold SQLite writer");
    connection
}

async fn release_sqlite_writer(mut connection: sqlx::pool::PoolConnection<sqlx::Sqlite>) {
    sqlx::query("ROLLBACK")
        .execute(&mut *connection)
        .await
        .expect("release SQLite writer");
}

fn headers(values: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    headers
}

fn token_urlsafe_len(nbytes: usize) -> usize {
    (nbytes * 4).div_ceil(3)
}

fn assert_urlsafe_token(token: &str) {
    assert!(!token.contains('='));
    assert!(
        token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    );
}

#[test]
fn oidc_token_urlsafe_uses_python_compatible_nonce_byte_lengths() {
    /*
     * Generates URL-safe OIDC tokens at the default, a configured, and a below-minimum byte
     * count. It checks output length follows unpadded base64 expansion, uses only URL-safe
     * characters, and floors undersized requests to the 16-byte security minimum.
     */
    let default_token = oidc_token_urlsafe(24).expect("default token");
    assert_eq!(default_token.len(), token_urlsafe_len(24));
    assert_urlsafe_token(&default_token);

    let configured_token = oidc_token_urlsafe(18).expect("configured token");
    assert_eq!(configured_token.len(), token_urlsafe_len(18));
    assert_urlsafe_token(&configured_token);

    let floored_token = oidc_token_urlsafe(1).expect("floored token");
    assert_eq!(floored_token.len(), token_urlsafe_len(16));
    assert_urlsafe_token(&floored_token);
}

#[tokio::test]
async fn missing_identity_headers_reject_without_dev_auth() {
    /*
     * Calls header authentication with no upstream identity headers under normal settings. It
     * checks the absence is treated as missing authentication rather than creating an anonymous
     * or development user.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();

    let error = header_identity(&settings, &pool, &HeaderMap::new())
        .await
        .expect_err("missing user should reject");

    assert!(matches!(error, AuthError::AuthenticationRequired));
}

#[tokio::test]
async fn header_identity_is_stripped_and_groups_are_synced() {
    /*
     * Authenticates whitespace-padded identity headers containing ordinary and administrator
     * groups. It checks profile values are trimmed, groups are sorted and synchronized,
     * effective admin status is derived, and the user group receives permissions on both
     * roots.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();
    let request_headers = headers(&[
        ("Remote-User", "  alice  "),
        ("Remote-Name", "  Alice Example  "),
        ("Remote-Email", "  alice@example.com  "),
        ("Remote-Groups", " vault-users, vault-admin "),
    ]);

    let user = header_identity(&settings, &pool, &request_headers)
        .await
        .expect("user");

    assert_eq!(user.subject, "alice");
    assert_eq!(user.name, "Alice Example");
    assert_eq!(user.email, "alice@example.com");
    assert_eq!(user.groups, ["vault-admin", "vault-users"]);
    assert!(user.is_admin);
    assert_eq!(
        folder_permission_count_for_group(&pool, "vault-users")
            .await
            .expect("permissions"),
        2,
    );
}

#[tokio::test]
async fn unchanged_header_identity_does_not_wait_for_sqlite_writer() {
    /*
     * Synchronizes a header user once, holds an unrelated SQLite writer, and authenticates the
     * same claims again. It checks the unchanged fast path completes without a write lock
     * and leaves the stored last-seen timestamp untouched.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();
    let request_headers = headers(&[
        ("Remote-User", "reader"),
        ("Remote-Name", "Read Only"),
        ("Remote-Email", "reader@example.com"),
        ("Remote-Groups", "artists,writers"),
    ]);
    header_identity(&settings, &pool, &request_headers)
        .await
        .expect("initial identity sync");
    let last_seen_before: String = sqlx::query_scalar(
        "SELECT last_seen_at FROM vault_users WHERE issuer = 'headers' AND subject = 'reader'",
    )
    .fetch_one(&pool)
    .await
    .expect("initial last seen");

    let writer = hold_sqlite_writer(&pool).await;
    let authentication = tokio::time::timeout(
        Duration::from_secs(2),
        header_identity(&settings, &pool, &request_headers),
    )
    .await;
    release_sqlite_writer(writer).await;

    let user = authentication
        .expect("unchanged header identity must not wait for the SQLite writer")
        .expect("unchanged header identity");
    assert_eq!(user.groups, ["artists", "writers"]);
    let last_seen_after: String = sqlx::query_scalar(
        "SELECT last_seen_at FROM vault_users WHERE issuer = 'headers' AND subject = 'reader'",
    )
    .fetch_one(&pool)
    .await
    .expect("unchanged last seen");
    assert_eq!(last_seen_after, last_seen_before);
}

#[tokio::test]
async fn unchanged_header_claims_repair_a_missing_root_permission_row() {
    /*
     * Synchronizes a group, manually removes one of its required root permissions, then presents
     * unchanged identity claims. It checks authentication detects and repairs incomplete derived
     * permissions even though the user profile and memberships did not change.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();
    let request_headers = headers(&[
        ("Remote-User", "repair"),
        ("Remote-Name", "Repair User"),
        ("Remote-Email", "repair@example.com"),
        ("Remote-Groups", "artists"),
    ]);
    header_identity(&settings, &pool, &request_headers)
        .await
        .expect("initial identity sync");
    sqlx::query(
        r"
        DELETE FROM folder_permissions
        WHERE id = (
            SELECT folder_permissions.id
            FROM folder_permissions
            JOIN vault_groups ON vault_groups.id = folder_permissions.group_id
            WHERE vault_groups.name = 'artists'
            ORDER BY folder_permissions.id
            LIMIT 1
        )
        ",
    )
    .execute(&pool)
    .await
    .expect("remove one root permission");
    assert_eq!(
        folder_permission_count_for_group(&pool, "artists")
            .await
            .expect("incomplete permissions"),
        1,
    );

    header_identity(&settings, &pool, &request_headers)
        .await
        .expect("repair root permission");

    assert_eq!(
        folder_permission_count_for_group(&pool, "artists")
            .await
            .expect("repaired permissions"),
        2,
    );
}

#[tokio::test]
async fn missing_header_email_stays_null_and_cannot_match_a_bootstrap_admin_email() {
    /*
     * Authenticates multiple header users without an email while an email-based bootstrap admin
     * is configured. It checks missing values remain SQL `NULL`, appear empty only in the
     * runtime context, and cannot accidentally match the configured administrator address.
     */
    let pool = test_pool().await;
    let settings = AuthSettings {
        bootstrap_admin_emails: ["admin@example.com".to_string()].into_iter().collect(),
        ..AuthSettings::default()
    };

    for subject in ["alice", "bob"] {
        let request_headers = headers(&[
            ("Remote-User", subject),
            ("Remote-Name", subject),
            ("Remote-Groups", "artists"),
        ]);
        let user = header_identity(&settings, &pool, &request_headers)
            .await
            .expect("email-less header identity");

        assert_eq!(user.email, "");
        assert!(!user.is_admin);
        let stored_email: Option<String> = sqlx::query_scalar(
            "SELECT email FROM vault_users WHERE issuer = 'headers' AND subject = ?",
        )
        .bind(subject)
        .fetch_one(&pool)
        .await
        .expect("stored email");
        assert_eq!(stored_email, None);
    }
}

#[tokio::test]
async fn missing_header_email_clears_a_legacy_synthetic_bootstrap_email() {
    /*
     * Seeds a legacy header user with a bootstrap-admin email, then authenticates authoritative
     * headers that omit email. It checks the stale synthetic value is cleared durably and no
     * longer grants effective administrator access.
     */
    let pool = test_pool().await;
    let settings = AuthSettings {
        bootstrap_admin_emails: ["admin@example.com".to_string()].into_iter().collect(),
        ..AuthSettings::default()
    };
    sqlx::query(
        r"
        INSERT INTO vault_users (issuer, subject, email, name, is_admin, is_active)
        VALUES ('headers', 'legacy', 'admin@example.com', 'Legacy User', 0, 1)
        ",
    )
    .execute(&pool)
    .await
    .expect("legacy synthetic email");
    let request_headers = headers(&[
        ("Remote-User", "legacy"),
        ("Remote-Name", "Legacy User"),
        ("Remote-Groups", "artists"),
    ]);

    let user = header_identity(&settings, &pool, &request_headers)
        .await
        .expect("legacy identity refresh");

    assert_eq!(user.email, "");
    assert!(!user.is_admin);
    let stored_email: Option<String> =
        sqlx::query_scalar("SELECT email FROM vault_users WHERE subject = 'legacy'")
            .fetch_one(&pool)
            .await
            .expect("cleared email");
    assert_eq!(stored_email, None);
}

#[tokio::test]
async fn missing_header_email_blocks_to_revoke_then_stable_none_is_read_only() {
    /*
     * Starts with an email-derived bootstrap administrator, then removes the authoritative email
     * while another writer holds SQLite. It checks that revocation waits for a durable write,
     * clears admin access and stored email, and that later unchanged email-less logins use the
     * read-only fast path.
     */
    let pool = test_pool().await;
    let settings = AuthSettings {
        bootstrap_admin_emails: ["owner@example.com".to_string()].into_iter().collect(),
        ..AuthSettings::default()
    };
    let email_headers = headers(&[
        ("Remote-User", "owner"),
        ("Remote-Name", "Owner"),
        ("Remote-Email", "owner@example.com"),
        ("Remote-Groups", "artists"),
    ]);
    let missing_email_headers = headers(&[
        ("Remote-User", "owner"),
        ("Remote-Name", "Owner"),
        ("Remote-Groups", "artists"),
    ]);
    let initial = header_identity(&settings, &pool, &email_headers)
        .await
        .expect("initial email identity");
    assert!(initial.is_admin);

    let writer = hold_sqlite_writer(&pool).await;
    let transition_pool = pool.clone();
    let transition_settings = settings.clone();
    let transition_headers = missing_email_headers.clone();
    let mut transition = tokio::spawn(async move {
        header_identity(&transition_settings, &transition_pool, &transition_headers).await
    });
    let blocked = tokio::time::timeout(Duration::from_millis(200), &mut transition).await;
    release_sqlite_writer(writer).await;
    assert!(
        blocked.is_err(),
        "Some-to-None email transition must wait for a durable write"
    );

    let transitioned = tokio::time::timeout(Duration::from_secs(2), transition)
        .await
        .expect("email transition after writer release")
        .expect("email transition task")
        .expect("email transition identity");
    assert_eq!(transitioned.email, "");
    assert!(!transitioned.is_admin);
    let stored_email: Option<String> = sqlx::query_scalar(
        "SELECT email FROM vault_users WHERE issuer = 'headers' AND subject = 'owner'",
    )
    .fetch_one(&pool)
    .await
    .expect("cleared authoritative email");
    assert_eq!(stored_email, None);

    let writer = hold_sqlite_writer(&pool).await;
    let stable = tokio::time::timeout(
        Duration::from_secs(2),
        header_identity(&settings, &pool, &missing_email_headers),
    )
    .await;
    release_sqlite_writer(writer).await;
    let stable = stable
        .expect("stable missing email must not wait for the SQLite writer")
        .expect("stable missing email identity");
    assert_eq!(stable.email, "");
    assert!(!stable.is_admin);
}

#[tokio::test]
async fn header_admin_group_removal_revokes_admin_context() {
    /*
     * Authenticates the same header user first with and then without the administrator group. It
     * checks current group claims immediately revoke effective admin status and synchronize the
     * persisted admin flag back to false.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();
    let admin_headers = headers(&[
        ("Remote-User", "alice"),
        ("Remote-Name", "Alice Example"),
        ("Remote-Email", "alice@example.com"),
        ("Remote-Groups", "vault-users,vault-admin"),
    ]);
    let user_headers = headers(&[
        ("Remote-User", "alice"),
        ("Remote-Name", "Alice Example"),
        ("Remote-Email", "alice@example.com"),
        ("Remote-Groups", "vault-users"),
    ]);

    let first = header_identity(&settings, &pool, &admin_headers)
        .await
        .expect("admin");
    let second = header_identity(&settings, &pool, &user_headers)
        .await
        .expect("user");

    assert!(first.is_admin);
    assert!(!second.is_admin);
    assert_eq!(second.groups, ["vault-users"]);
    let stored_admin: i64 =
        sqlx::query_scalar("SELECT is_admin FROM vault_users WHERE subject = 'alice'")
            .fetch_one(&pool)
            .await
            .expect("stored user");
    assert_eq!(stored_admin, 0);
}

#[tokio::test]
async fn bootstrap_admin_email_grants_effective_admin_without_persisting_admin_flag() {
    /*
     * Authenticates a mixed-case email that matches the configured bootstrap administrator after
     * normalization. It checks the request receives effective admin rights while the user's
     * stored `is_admin` flag remains false, keeping bootstrap policy separate from durable
     * assignment.
     */
    let pool = test_pool().await;
    let settings = AuthSettings {
        bootstrap_admin_emails: ["alice@example.com".to_string()].into_iter().collect(),
        ..AuthSettings::default()
    };
    let request_headers = headers(&[
        ("Remote-User", "alice"),
        ("Remote-Name", "Alice Example"),
        ("Remote-Email", " Alice@Example.com "),
        ("Remote-Groups", "artists"),
    ]);

    let user = header_identity(&settings, &pool, &request_headers)
        .await
        .expect("bootstrap admin");

    assert!(user.is_admin);
    assert_eq!(user.email, "Alice@Example.com");
    assert_eq!(user.groups, ["artists"]);
    let stored_admin: i64 =
        sqlx::query_scalar("SELECT is_admin FROM vault_users WHERE subject = 'alice'")
            .fetch_one(&pool)
            .await
            .expect("stored user");
    assert_eq!(stored_admin, 0);
}

#[test]
fn oidc_bootstrap_admin_subject_is_exact_and_bound_to_the_configured_issuer() {
    /*
     * Evaluates bootstrap-subject policy across exact, case-changed, whitespace-changed, wrong
     * issuer, header, and development identities. It checks only the opaque subject from the
     * configured OIDC issuer grants admin and that legacy email bootstrap policy does not apply.
     */
    let settings = AuthSettings {
        bootstrap_admin_emails: ["owner@example.com".to_string()].into_iter().collect(),
        mode: AuthMode::Oidc,
        oidc_bootstrap_admin_subjects: ["Kevin".to_string()].into_iter().collect(),
        oidc_issuer: "https://issuer.example.com".to_string(),
        ..AuthSettings::default()
    };

    assert!(effective_admin_from_parts(
        &settings,
        false,
        "https://issuer.example.com",
        "Kevin",
        None,
        &[],
    ));
    assert!(!effective_admin_from_parts(
        &settings,
        false,
        "https://issuer.example.com",
        "kevin",
        Some("owner@example.com"),
        &[],
    ));
    assert!(!effective_admin_from_parts(
        &settings,
        false,
        "https://issuer.example.com",
        " Kevin",
        None,
        &[],
    ));
    assert!(!effective_admin_from_parts(
        &settings,
        false,
        "https://other.example.com",
        "Kevin",
        Some("owner@example.com"),
        &[],
    ));

    let header_settings = AuthSettings {
        mode: AuthMode::Headers,
        oidc_bootstrap_admin_subjects: ["Kevin".to_string()].into_iter().collect(),
        oidc_issuer: "https://issuer.example.com".to_string(),
        ..AuthSettings::default()
    };
    assert!(!effective_admin_from_parts(
        &header_settings,
        false,
        "headers",
        "Kevin",
        None,
        &[],
    ));
    let dev_settings = AuthSettings {
        mode: AuthMode::Dev,
        oidc_bootstrap_admin_subjects: ["Kevin".to_string()].into_iter().collect(),
        oidc_issuer: "https://issuer.example.com".to_string(),
        ..AuthSettings::default()
    };
    assert!(!effective_admin_from_parts(
        &dev_settings,
        false,
        "dev",
        "Kevin",
        None,
        &[],
    ));
}

#[tokio::test]
async fn legacy_oidc_email_does_not_grant_bootstrap_admin_to_an_existing_session() {
    /*
     * Seeds a non-admin OIDC user whose stored email matches the header-mode bootstrap list and
     * resolves an existing signed session. It checks legacy email data cannot elevate an OIDC
     * session now that OIDC administration is subject-based.
     */
    let pool = test_pool().await;
    let settings = AuthSettings {
        bootstrap_admin_emails: ["owner@example.com".to_string()].into_iter().collect(),
        mode: AuthMode::Oidc,
        oidc_issuer: "https://issuer.example.com".to_string(),
        signing_keys: signing_keys(TEST_SIGNING_ROOT, &[]),
        ..AuthSettings::default()
    };
    sqlx::query(
        r"
        INSERT INTO vault_users (issuer, subject, email, name, is_admin, is_active)
        VALUES ('https://issuer.example.com', 'legacy', 'owner@example.com', 'Legacy', 0, 1)
        ",
    )
    .execute(&pool)
    .await
    .expect("legacy OIDC user");
    let session = sign_session_payload(
        &settings,
        &Map::from_iter([
            ("uid".to_string(), json!(1)),
            ("exp".to_string(), json!(4_102_444_800_i64)),
        ]),
    )
    .expect("session");

    let user = session_identity(
        &settings,
        &pool,
        Some(&format!("{}={session}", settings.session_cookie_name)),
    )
    .await
    .expect("session lookup")
    .expect("user");

    assert!(!user.is_admin);
}

#[tokio::test]
async fn oidc_relogin_without_a_verified_email_preserves_the_existing_profile_email() {
    /*
     * Creates an OIDC profile with a verified email, then logs the same subject in again without
     * an email claim. It checks the trusted stored email is retained while the login
     * timestamp still advances, rather than treating claim omission as revocation.
     */
    let pool = test_pool().await;
    let settings = AuthSettings {
        mode: AuthMode::Oidc,
        oidc_issuer: "https://issuer.example.com".to_string(),
        ..AuthSettings::default()
    };
    let groups = BTreeSet::default();
    oidc_identity(
        &settings,
        &pool,
        "subject",
        Some("verified@example.com"),
        Some("Verified User"),
        &groups,
    )
    .await
    .expect("initial identity");
    sqlx::query(
        "UPDATE vault_users SET last_login_at = '2000-01-01T00:00:00Z' WHERE subject = 'subject'",
    )
    .execute(&pool)
    .await
    .expect("old login timestamp");

    let user = oidc_identity(
        &settings,
        &pool,
        "subject",
        None,
        Some("Verified User"),
        &groups,
    )
    .await
    .expect("relogin identity");

    assert_eq!(user.email, "verified@example.com");
    let last_login_at: String =
        sqlx::query_scalar("SELECT last_login_at FROM vault_users WHERE subject = 'subject'")
            .fetch_one(&pool)
            .await
            .expect("OIDC login timestamp");
    assert_ne!(last_login_at, "2000-01-01T00:00:00Z");
}

#[tokio::test]
async fn disabled_header_user_request_does_not_sync_groups_or_profile() {
    /*
     * Presents updated profile and group headers for a user already marked inactive. It checks
     * authentication fails before synchronizing anything, preserving the old name and email and
     * avoiding creation of the newly claimed group.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();
    sqlx::query(
        r"
        INSERT INTO vault_users
            (issuer, subject, email, name, is_admin, is_active, preferences)
        VALUES
            ('headers', 'disabled', 'old@example.com', 'Disabled User', 0, 0, '{}')
        ",
    )
    .execute(&pool)
    .await
    .expect("insert disabled user");
    let request_headers = headers(&[
        ("Remote-User", "disabled"),
        ("Remote-Name", "Updated Name"),
        ("Remote-Email", "updated@example.com"),
        ("Remote-Groups", "new-disabled-group"),
    ]);

    let error = header_identity(&settings, &pool, &request_headers)
        .await
        .expect_err("disabled user should reject");

    assert!(matches!(error, AuthError::UserDisabled));
    let row: (String, String) =
        sqlx::query_as("SELECT name, email FROM vault_users WHERE subject = 'disabled'")
            .fetch_one(&pool)
            .await
            .expect("disabled user");
    assert_eq!(
        row,
        ("Disabled User".to_string(), "old@example.com".to_string())
    );
    let group_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM vault_groups WHERE name = 'new-disabled-group'")
            .fetch_one(&pool)
            .await
            .expect("group count");
    assert_eq!(group_count, 0);
}

#[tokio::test]
async fn concurrent_header_identity_upserts_create_one_user_and_membership() {
    /*
     * Authenticates the same new subject concurrently from sixteen tasks with varying display
     * names and one shared group. It checks every caller resolves the identity while uniqueness
     * and transaction handling leave exactly one user, group, and membership.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();
    let mut handles = Vec::new();

    for index in 0..16 {
        let pool = pool.clone();
        let settings = settings.clone();
        handles.push(tokio::spawn(async move {
            let mut request_headers = HeaderMap::new();
            request_headers.insert("Remote-User", HeaderValue::from_static("race"));
            request_headers.insert(
                "Remote-Name",
                HeaderValue::from_str(&format!("Race User {index}")).expect("name header"),
            );
            request_headers.insert("Remote-Email", HeaderValue::from_static("race@example.com"));
            request_headers.insert("Remote-Groups", HeaderValue::from_static("vault-users"));

            header_identity(&settings, &pool, &request_headers)
                .await
                .expect("concurrent header identity")
        }));
    }

    for handle in handles {
        let user = handle.await.expect("identity task");
        assert_eq!(user.subject, "race");
        assert_eq!(user.email, "race@example.com");
        assert_eq!(user.groups, ["vault-users"]);
    }

    let row = sqlx::query(
        r"
        SELECT
            COUNT(DISTINCT vault_users.id) AS user_count,
            COUNT(DISTINCT vault_groups.id) AS group_count,
            COUNT(vault_group_memberships.user_id) AS membership_count
        FROM vault_users
        LEFT JOIN vault_group_memberships
            ON vault_group_memberships.user_id = vault_users.id
        LEFT JOIN vault_groups
            ON vault_groups.id = vault_group_memberships.group_id
        WHERE vault_users.issuer = 'headers'
            AND vault_users.subject = 'race'
        ",
    )
    .fetch_one(&pool)
    .await
    .expect("identity rows");

    assert_eq!(row.get::<i64, _>("user_count"), 1);
    assert_eq!(row.get::<i64, _>("group_count"), 1);
    assert_eq!(row.get::<i64, _>("membership_count"), 1);
}

#[tokio::test]
async fn dev_auth_requires_local_base_domain() {
    /*
     * Enables development authentication but configures a nonlocal base domain. It checks the
     * shortcut declines to create or return a development identity outside an explicitly local
     * deployment.
     */
    let pool = test_pool().await;
    let settings = AuthSettings {
        mode: AuthMode::Dev,
        auth_mode_raw: "dev".to_string(),
        dev_auth_enabled: true,
        base_domain: "vault.example.com".to_string(),
        ..AuthSettings::default()
    };

    let user = dev_identity(&settings, &pool)
        .await
        .expect("development identity check");

    assert_eq!(user, None);
}

#[tokio::test]
async fn dev_auth_syncs_configured_groups_on_local_domain() {
    /*
     * Enables development authentication on localhost with configured user and admin groups. It
     * checks the synthetic identity and memberships are synchronized correctly, then confirms an
     * unchanged repeat can authenticate while another SQLite writer is active.
     */
    let pool = test_pool().await;
    let settings = AuthSettings {
        mode: AuthMode::Dev,
        auth_mode_raw: "dev".to_string(),
        dev_auth_enabled: true,
        base_domain: "localhost".to_string(),
        dev_user: "dev-user".to_string(),
        dev_name: "Dev User".to_string(),
        dev_groups: split_groups("vault-users,vault-admin"),
        ..AuthSettings::default()
    };

    let user = dev_identity(&settings, &pool)
        .await
        .expect("development identity check")
        .expect("dev user");

    assert_eq!(user.subject, "dev-user");
    assert_eq!(user.name, "Dev User");
    assert_eq!(user.groups, ["vault-admin", "vault-users"]);
    assert!(user.is_admin);

    let writer = hold_sqlite_writer(&pool).await;
    let repeated =
        tokio::time::timeout(Duration::from_secs(2), dev_identity(&settings, &pool)).await;
    release_sqlite_writer(writer).await;
    let repeated = repeated
        .expect("unchanged development identity must not wait for the SQLite writer")
        .expect("development identity check")
        .expect("development user");
    assert_eq!(repeated.groups, ["vault-admin", "vault-users"]);
}

#[tokio::test]
async fn session_payload_requires_expiration_and_numeric_user_id() {
    /*
     * Signs payloads with no expiry, an expired value, boolean expiry or user ID, and also
     * probes a non-ASCII token. It checks verification accepts neither malformed claim types
     * nor expired or syntactically invalid sessions.
     */
    let settings = AuthSettings::default();
    let mut missing_exp = Map::new();
    missing_exp.insert("uid".to_string(), json!(1));
    let cookie = sign_session_payload(&settings, &missing_exp).expect("sign");
    assert!(verify_session_payload(&settings, &cookie).is_none());

    let mut expired = Map::new();
    expired.insert("uid".to_string(), json!(1));
    expired.insert("exp".to_string(), json!(1.0));
    let expired_cookie = sign_session_payload(&settings, &expired).expect("sign");
    assert!(verify_session_payload(&settings, &expired_cookie).is_none());

    let mut bool_exp = Map::new();
    bool_exp.insert("uid".to_string(), json!(1));
    bool_exp.insert("exp".to_string(), Value::Bool(true));
    let bool_exp_cookie = sign_session_payload(&settings, &bool_exp).expect("sign");
    assert!(verify_session_payload(&settings, &bool_exp_cookie).is_none());

    let mut bool_uid = Map::new();
    bool_uid.insert("uid".to_string(), Value::Bool(true));
    bool_uid.insert("exp".to_string(), json!(4_102_444_800.0));
    let bool_cookie = sign_session_payload(&settings, &bool_uid).expect("sign");
    let pool = test_pool().await;
    assert!(
        session_identity(
            &settings,
            &pool,
            Some(&format!("{}={bool_cookie}", settings.session_cookie_name)),
        )
        .await
        .expect("session lookup")
        .is_none(),
    );
    assert!(verify_session_payload(&settings, "not-ascii-\u{2603}.signature").is_none());
}

#[test]
fn session_cookie_lookup_uses_exact_cookie_name_from_multi_cookie_header() {
    /*
     * Parses a multi-cookie header containing both the configured session name and a longer name
     * that shares its prefix. It checks only an exact cookie-name match is returned and missing
     * headers or prefix-only matches produce no session.
     */
    assert_eq!(
        cookie_value(
            Some("theme=dark; vault_session_extra=wrong; vault_session=payload.signature"),
            "vault_session",
        ),
        Some("payload.signature".to_string()),
    );
    assert_eq!(
        cookie_value(Some("vault_session_extra=wrong"), "vault_session"),
        None,
    );
    assert_eq!(cookie_value(None, "vault_session"), None);
}

#[tokio::test]
async fn session_identity_resolves_active_user() {
    /*
     * Stores an active user, signs a future-expiring session for that row ID, and resolves the
     * cookie through normal session lookup. It checks the complete runtime user context is
     * loaded from persisted identity data rather than trusted from cookie fields.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();
    let user_id = sqlx::query(
        r"
        INSERT INTO vault_users
            (issuer, subject, email, name, is_admin, is_active, preferences)
        VALUES
            ('issuer', 'alice', 'alice@example.com', 'Alice', 0, 1, '{}')
        ",
    )
    .execute(&pool)
    .await
    .expect("insert user")
    .last_insert_rowid();
    let mut payload = Map::new();
    payload.insert("uid".to_string(), json!(user_id));
    payload.insert("exp".to_string(), json!(4_102_444_800.0));
    let cookie = sign_session_payload(&settings, &payload).expect("sign");

    let user = session_identity(
        &settings,
        &pool,
        Some(&format!("{}={cookie}", settings.session_cookie_name)),
    )
    .await
    .expect("session")
    .expect("user");

    assert_eq!(
        user,
        UserContext {
            id: user_id.to_string(),
            vault_user_id: user_id,
            issuer: "issuer".to_string(),
            subject: "alice".to_string(),
            name: "Alice".to_string(),
            email: "alice@example.com".to_string(),
            groups: Vec::new(),
            is_admin: false,
        },
    );
}

#[tokio::test]
async fn stale_session_refreshes_once_then_fresh_session_is_read_only() {
    /*
     * Resolves a session whose stored last-seen timestamp is stale, then resolves it again while
     * a writer lock is held. It checks the first access refreshes activity durably and the
     * fresh access performs no redundant write or lock wait.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();
    let stale_last_seen = "2000-01-01T00:00:00Z";
    let user_id = sqlx::query(
        r"
        INSERT INTO vault_users
            (issuer, subject, email, name, is_admin, is_active, preferences, last_seen_at)
        VALUES
            ('issuer', 'returning', 'returning@example.com', 'Returning', 0, 1, '{}', ?)
        ",
    )
    .bind(stale_last_seen)
    .execute(&pool)
    .await
    .expect("insert stale session user")
    .last_insert_rowid();
    let cookie = sign_session_payload(
        &settings,
        &Map::from_iter([
            ("uid".to_string(), json!(user_id)),
            ("exp".to_string(), json!(4_102_444_800_i64)),
        ]),
    )
    .expect("session");
    let cookie_header = format!("{}={cookie}", settings.session_cookie_name);

    session_identity(&settings, &pool, Some(&cookie_header))
        .await
        .expect("stale session refresh")
        .expect("active session");
    let refreshed_last_seen: String =
        sqlx::query_scalar("SELECT last_seen_at FROM vault_users WHERE id = ?")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("refreshed last seen");
    assert_ne!(refreshed_last_seen, stale_last_seen);

    let writer = hold_sqlite_writer(&pool).await;
    let authentication = tokio::time::timeout(
        Duration::from_secs(2),
        session_identity(&settings, &pool, Some(&cookie_header)),
    )
    .await;
    release_sqlite_writer(writer).await;
    authentication
        .expect("fresh signed session must not wait for the SQLite writer")
        .expect("fresh session lookup")
        .expect("fresh active session");
    let unchanged_last_seen: String =
        sqlx::query_scalar("SELECT last_seen_at FROM vault_users WHERE id = ?")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("unchanged last seen");
    assert_eq!(unchanged_last_seen, refreshed_last_seen);
}

#[tokio::test]
async fn session_identity_ignores_inactive_users() {
    /*
     * Signs a structurally valid, unexpired session for a user whose persisted account is
     * inactive. It checks session resolution returns no identity even though the token
     * itself verifies.
     */
    let pool = test_pool().await;
    let settings = AuthSettings::default();
    let user_id = sqlx::query(
        r"
        INSERT INTO vault_users
            (issuer, subject, email, name, is_admin, is_active, preferences)
        VALUES
            ('issuer', 'disabled-session', 'disabled@example.com', 'Disabled User', 0, 0, '{}')
        ",
    )
    .execute(&pool)
    .await
    .expect("insert inactive user")
    .last_insert_rowid();
    let mut payload = Map::new();
    payload.insert("uid".to_string(), json!(user_id));
    payload.insert("exp".to_string(), json!(4_102_444_800.0));
    let cookie = sign_session_payload(&settings, &payload).expect("sign");

    let user = session_identity(
        &settings,
        &pool,
        Some(&format!("{}={cookie}", settings.session_cookie_name)),
    )
    .await
    .expect("session lookup");

    assert_eq!(user, None);
}

#[test]
fn runtime_validation_rejects_missing_docker_session_secret() {
    /*
     * Models a runtime that requires an explicit session secret but only has an unrelated OIDC
     * client-secret fallback. It checks validation demands `VAULT_SESSION_SECRET` and explains
     * that fallback is reserved for the built-in development secret.
     */
    let settings = AuthSettings {
        session_secret_requirement: SessionSecretRequirement::Required,
        signing_keys: SigningKeyring::from_configured("oidc-client-secret", vec![]),
        session_secret_source: SessionSecretSource::Fallback,
        oidc_client_secret: "oidc-client-secret".to_string(),
        ..AuthSettings::default()
    };

    let error = settings
        .validate_runtime_config()
        .expect_err("docker runtime should require explicit session secret");

    assert!(
        error
            .to_string()
            .contains("VAULT_SESSION_SECRET is required when VAULT_REQUIRE_SESSION_SECRET=1")
    );
    assert!(
        error
            .to_string()
            .contains("fallback is available only for the built-in development secret")
    );
}

#[test]
fn runtime_validation_rejects_development_session_secret_outside_dev() {
    /*
     * Configures the built-in insecure signing secret while development mode is off. It checks
     * runtime validation prevents that fallback from protecting production sessions.
     */
    let settings = AuthSettings {
        signing_keys: SigningKeyring::from_configured("dev-insecure-session-secret", vec![]),
        session_secret_source: SessionSecretSource::Fallback,
        dev_mode: false,
        ..AuthSettings::default()
    };

    let error = settings
        .validate_runtime_config()
        .expect_err("production default secret should reject");

    assert!(
        error
            .to_string()
            .contains("VAULT_SESSION_SECRET is required outside development mode")
    );
}

#[test]
fn runtime_validation_allows_development_session_secret_in_dev_mode() {
    /*
     * Combines development authentication, development mode, and the built-in fallback signing
     * secret. It checks this intentionally local configuration passes runtime validation.
     */
    let settings = AuthSettings {
        mode: AuthMode::Dev,
        auth_mode_raw: "dev".to_string(),
        dev_mode: true,
        signing_keys: SigningKeyring::from_configured("dev-insecure-session-secret", vec![]),
        session_secret_source: SessionSecretSource::Fallback,
        ..AuthSettings::default()
    };

    settings
        .validate_runtime_config()
        .expect("dev mode may use development secret");
}

#[test]
fn runtime_validation_requires_canonical_high_diversity_explicit_signing_roots() {
    /*
     * Validates short, non-hex, repeated, periodic, skewed, and predictable 32-byte signing
     * roots alongside one canonical random-looking root. It checks explicit secrets must be
     * exactly 64 hexadecimal characters and pass the diversity checks intended to reject
     * human-generated or patterned material.
     */
    let weak_roots = [
        "password".to_string(),
        "g".repeat(64),
        "0".repeat(64),
        // Sixteen distinct bytes repeated exactly: periodic without low diversity.
        "000102030405060708090a0b0c0d0e0f".repeat(2),
        // A 17-byte period truncated to 32 bytes exercises non-divisor periods.
        "000102030405060708090a0b0c0d0e0f10000102030405060708090a0b0c0d0e".to_string(),
        // Sixteen distinct bytes, with one occurring nine times.
        "0000000000000000000102030405060708090a0b0c0d0e0f0102030405060708".to_string(),
        // A full-width arithmetic sequence.
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".to_string(),
    ];
    for root in weak_roots {
        let error = explicit_dev_settings(&root, &[])
            .validate_runtime_config()
            .expect_err("weak explicit signing root must reject")
            .to_string();
        assert!(error.contains(
            "VAULT_SESSION_SECRET must be exactly 64 hexadecimal characters generated from 32 random bytes",
        ));
    }

    explicit_dev_settings(TEST_SIGNING_ROOT, &[])
        .validate_runtime_config()
        .expect("canonical 32-byte signing root");
}

#[test]
fn runtime_validation_bounds_and_deduplicates_previous_signing_roots() {
    /*
     * Exercises malformed, low-diversity, current-key duplicate, repeated, case-variant, and
     * overlong previous-key lists. It checks at most four distinct canonical roots are accepted
     * and that verification honors the fourth configured key but never an out-of-bound fifth
     * key.
     */
    let invalid = explicit_dev_settings(TEST_SIGNING_ROOT, &["not-hex"])
        .validate_runtime_config()
        .expect_err("invalid previous root must reject")
        .to_string();
    assert!(invalid.contains("VAULT_SESSION_SECRET_PREVIOUS must be exactly 64 hexadecimal"));

    let low_diversity_root = "0".repeat(64);
    let low_diversity = explicit_dev_settings(TEST_SIGNING_ROOT, &[low_diversity_root.as_str()])
        .validate_runtime_config()
        .expect_err("low-diversity previous root must reject")
        .to_string();
    assert!(low_diversity.contains("VAULT_SESSION_SECRET_PREVIOUS must be exactly 64 hexadecimal"));

    let duplicate = explicit_dev_settings(TEST_SIGNING_ROOT, &[TEST_SIGNING_ROOT])
        .validate_runtime_config()
        .expect_err("duplicate previous root must reject")
        .to_string();
    assert!(duplicate.contains("must not repeat the current or another previous secret"));

    let duplicate_previous =
        explicit_dev_settings(TEST_SIGNING_ROOT, &[ROTATION_ROOTS[0], ROTATION_ROOTS[0]])
            .validate_runtime_config()
            .expect_err("duplicate previous roots must reject")
            .to_string();
    assert!(duplicate_previous.contains("must not repeat the current or another previous secret"));

    let uppercase_duplicate = ROTATION_ROOTS[0].to_ascii_uppercase();
    let duplicate_previous_case = explicit_dev_settings(
        TEST_SIGNING_ROOT,
        &[ROTATION_ROOTS[0], uppercase_duplicate.as_str()],
    )
    .validate_runtime_config()
    .expect_err("case-insensitive duplicate previous roots must reject")
    .to_string();
    assert!(
        duplicate_previous_case.contains("must not repeat the current or another previous secret")
    );

    explicit_dev_settings(TEST_SIGNING_ROOT, &ROTATION_ROOTS[..4])
        .validate_runtime_config()
        .expect("four distinct previous roots are supported");

    let too_many = AuthSettings {
        signing_keys: SigningKeyring::from_configured(
            TEST_SIGNING_ROOT,
            ROTATION_ROOTS
                .iter()
                .map(|root| (*root).to_string())
                .collect(),
        ),
        ..explicit_dev_settings(TEST_SIGNING_ROOT, &[])
    }
    .validate_runtime_config()
    .expect_err("unbounded previous roots must reject")
    .to_string();
    assert!(too_many.contains("VAULT_SESSION_SECRET_PREVIOUS may contain at most 4 secrets"));

    let payload = Map::from_iter([
        ("uid".to_string(), json!(42)),
        ("exp".to_string(), json!(4_102_444_800_i64)),
    ]);
    let fourth = explicit_dev_settings(ROTATION_ROOTS[3], &[]);
    let fifth = explicit_dev_settings(ROTATION_ROOTS[4], &[]);
    let bounded = AuthSettings {
        signing_keys: SigningKeyring::from_configured(
            TEST_SIGNING_ROOT,
            ROTATION_ROOTS
                .iter()
                .map(|root| (*root).to_string())
                .collect(),
        ),
        ..explicit_dev_settings(TEST_SIGNING_ROOT, &[])
    };
    let fourth_token = sign_session_payload(&fourth, &payload).expect("fourth previous token");
    let fifth_token = sign_session_payload(&fifth, &payload).expect("fifth previous token");
    assert!(verify_session_payload(&bounded, &fourth_token).is_some());
    assert!(verify_session_payload(&bounded, &fifth_token).is_none());
}

#[test]
fn session_signing_key_rotation_verifies_previous_but_signs_only_with_current() {
    /*
     * Rotates from an old signing root to a new root while retaining the old one for
     * verification. It checks old sessions survive only during the grace period, new
     * sessions use only the current derived key, secrets are redacted from debug output, and
     * legacy raw-key signatures are not accepted.
     */
    let old = explicit_dev_settings(PREVIOUS_SIGNING_ROOT, &[]);
    let rotated = explicit_dev_settings(TEST_SIGNING_ROOT, &[PREVIOUS_SIGNING_ROOT]);
    let current_only = explicit_dev_settings(TEST_SIGNING_ROOT, &[]);
    let mut payload = Map::new();
    payload.insert("uid".to_string(), json!(42));
    payload.insert("exp".to_string(), json!(4_102_444_800_i64));

    let old_token = sign_session_payload(&old, &payload).expect("old session token");
    assert!(verify_session_payload(&rotated, &old_token).is_some());
    assert!(verify_session_payload(&current_only, &old_token).is_none());

    let new_token = sign_session_payload(&rotated, &payload).expect("new session token");
    assert_eq!(new_token.matches('.').count(), 1);
    assert!(verify_session_payload(&current_only, &new_token).is_some());
    assert!(verify_session_payload(&old, &new_token).is_none());
    let debug_keyring = format!("{:?}", rotated.signing_keys);
    assert!(!debug_keyring.contains(TEST_SIGNING_ROOT));
    assert!(!debug_keyring.contains(PREVIOUS_SIGNING_ROOT));

    let (body, _) = new_token.rsplit_once('.').expect("session token parts");
    let mut legacy_mac =
        Hmac::<Sha256>::new_from_slice(TEST_SIGNING_ROOT.as_bytes()).expect("legacy raw HMAC key");
    legacy_mac.update(body.as_bytes());
    let legacy_signature = URL_SAFE_NO_PAD.encode(legacy_mac.finalize().into_bytes());
    let legacy_token = format!("{body}.{legacy_signature}");
    assert!(verify_session_payload(&rotated, &legacy_token).is_none());
}

#[test]
fn trusted_proxy_set_matches_exact_cidrs_ipv6_and_mapped_ipv6() {
    /*
     * Parses exact IPv4, IPv4 CIDR, IPv6 CIDR, and IPv4-mapped IPv6 trust entries. It checks
     * peers inside each boundary match while adjacent networks and mapped addresses outside
     * the declared range do not.
     */
    let proxies =
        TrustedProxySet::parse("127.0.0.1, 10.20.0.0/16, 2001:db8::/48, ::ffff:192.0.2.0/120");

    assert!(proxies.contains("127.0.0.1".parse().expect("loopback")));
    assert!(proxies.contains("10.20.4.5".parse().expect("private CIDR")));
    assert!(!proxies.contains("10.21.4.5".parse().expect("outside CIDR")));
    assert!(proxies.contains("2001:db8::1234".parse().expect("IPv6 CIDR")));
    assert!(!proxies.contains("2001:db9::1".parse().expect("outside IPv6 CIDR")));
    assert!(proxies.contains("::ffff:192.0.2.44".parse().expect("mapped peer")));
    assert!(!proxies.contains("::ffff:192.0.3.44".parse().expect("mapped outside")));
}

#[test]
fn runtime_validation_requires_valid_proxy_trust_for_header_auth() {
    /*
     * Validates header authentication with missing trust, wildcard and all-network ranges,
     * malformed lists, hostnames, and a bounded set of IP/CIDR entries. It checks only
     * explicit, syntactically valid proxy boundaries are accepted for identity-bearing
     * headers.
     */
    let base = AuthSettings {
        signing_keys: signing_keys(TEST_SIGNING_ROOT, &[]),
        session_secret_source: SessionSecretSource::Explicit,
        ..AuthSettings::default()
    };

    let missing = base
        .validate_runtime_config()
        .expect_err("header auth must require proxy trust")
        .to_string();
    assert!(missing.contains("FORWARDED_ALLOW_IPS is required"));

    for invalid in [
        "*",
        "0.0.0.0/0",
        "::/0",
        "::ffff:0:0/96",
        "127.0.0.1,,10.0.0.1",
        "proxy.local",
    ] {
        let settings = AuthSettings {
            trusted_proxies: TrustedProxySet::parse(invalid),
            ..base.clone()
        };
        let error = settings
            .validate_runtime_config()
            .expect_err("invalid proxy trust must reject")
            .to_string();
        assert!(error.contains("FORWARDED_ALLOW_IPS contains invalid IP/CIDR entries"));
    }

    AuthSettings {
        trusted_proxies: TrustedProxySet::parse("127.0.0.1,10.0.0.0/8"),
        ..base
    }
    .validate_runtime_config()
    .expect("valid proxy trust");
}

#[test]
fn runtime_validation_rejects_dev_auth_mixed_with_header_auth() {
    /*
     * Enables the development identity shortcut while leaving the configured authentication mode
     * on trusted headers. It checks runtime validation rejects the mixed mode before either
     * authentication source can be used ambiguously.
     */
    let settings = AuthSettings {
        dev_mode: true,
        dev_auth_enabled: true,
        trusted_proxies: TrustedProxySet::parse("127.0.0.1"),
        ..AuthSettings::default()
    };

    let error = settings
        .validate_runtime_config()
        .expect_err("mixed header and dev auth must reject")
        .to_string();
    assert!(error.contains("VAULT_DEV_AUTH requires VAULT_AUTH_MODE=dev"));
}

#[test]
fn malformed_optional_proxy_trust_rejects_outside_header_mode() {
    /*
     * Supplies malformed proxy trust while running in development mode, then compares a
     * development configuration that omits proxy trust entirely. It checks optional trust
     * may be absent outside header mode but can never be present in an invalid form.
     */
    let settings = AuthSettings {
        mode: AuthMode::Dev,
        auth_mode_raw: "dev".to_string(),
        dev_mode: true,
        trusted_proxies: TrustedProxySet::parse("proxy.local"),
        ..AuthSettings::default()
    };

    let error = settings
        .validate_runtime_config()
        .expect_err("malformed optional proxy trust must reject")
        .to_string();
    assert!(error.contains("FORWARDED_ALLOW_IPS contains invalid IP/CIDR entries"));

    AuthSettings {
        mode: AuthMode::Dev,
        auth_mode_raw: "dev".to_string(),
        dev_mode: true,
        ..AuthSettings::default()
    }
    .validate_runtime_config()
    .expect("dev auth may omit proxy trust");
}

#[test]
fn runtime_validation_rejects_invalid_auth_cookie_and_oidc_client_modes() {
    /*
     * Combines an unknown authentication mode, invalid secure-cookie setting, unsafe cookie
     * names, and unsupported OIDC client authentication. It checks validation reports every
     * independent configuration error rather than stopping after the first one.
     */
    let settings = AuthSettings {
        auth_mode_raw: "bogus".to_string(),
        session_cookie_name: "vault session".to_string(),
        session_cookie_secure: "sometimes".to_string(),
        oidc_state_cookie_name: "vault;oidc".to_string(),
        oidc_client_auth: "implicit".to_string(),
        signing_keys: signing_keys(TEST_SIGNING_ROOT, &[]),
        session_secret_source: SessionSecretSource::Explicit,
        ..AuthSettings::default()
    };

    let error = settings
        .validate_runtime_config()
        .expect_err("invalid modes should reject")
        .to_string();

    assert!(error.contains("VAULT_AUTH_MODE must be one of dev, headers, oidc"));
    assert!(error.contains("VAULT_SESSION_COOKIE_SECURE must be auto, true, or false"));
    assert!(error.contains(
        "VAULT_SESSION_COOKIE_NAME must contain only letters, digits, underscores, hyphens, or dots"
    ));
    assert!(error.contains(
        "VAULT_OIDC_STATE_COOKIE_NAME must contain only letters, digits, underscores, hyphens, or dots"
    ));
    assert!(error.contains(
        "VAULT_OIDC_CLIENT_AUTH must be client_secret_basic, client_secret_post, or none",
    ));
}

#[test]
fn runtime_validation_rejects_insecure_production_urls() {
    /*
     * Configures a nonlocal HTTP public URL with otherwise explicit production signing material.
     * It checks runtime validation requires HTTPS before the deployment can emit cookies or
     * redirects against that origin.
     */
    let settings = AuthSettings {
        public_url: "http://vault.example.com".to_string(),
        signing_keys: signing_keys(TEST_SIGNING_ROOT, &[]),
        session_secret_source: SessionSecretSource::Explicit,
        ..AuthSettings::default()
    };

    let error = settings
        .validate_runtime_config()
        .expect_err("insecure production public url should reject");

    assert!(
        error
            .to_string()
            .contains("VAULT_PUBLIC_URL must use https outside local development")
    );
}

#[test]
fn runtime_validation_rejects_incomplete_or_insecure_oidc_config() {
    /*
     * Builds an OIDC configuration with an insecure and colliding issuer, missing client
     * credentials, the wrong bootstrap policy, and an insecure redirect. It checks validation
     * reports all of those deployment hazards together with actionable setting names.
     */
    let settings = AuthSettings {
        bootstrap_admin_emails: ["owner@example.com".to_string()].into_iter().collect(),
        mode: vault_server::auth::AuthMode::Oidc,
        auth_mode_raw: "oidc".to_string(),
        header_auth_issuer: "http://idp.example.com".to_string(),
        oidc_issuer: "http://idp.example.com".to_string(),
        oidc_client_id: String::new(),
        oidc_client_secret: String::new(),
        oidc_redirect_uri: "http://vault.example.com/auth/callback".to_string(),
        signing_keys: signing_keys(TEST_SIGNING_ROOT, &[]),
        session_secret_source: SessionSecretSource::Explicit,
        ..AuthSettings::default()
    };

    let error = settings
        .validate_runtime_config()
        .expect_err("incomplete oidc config should reject")
        .to_string();

    assert!(error.contains("VAULT_OIDC_ISSUER must use https outside local development"));
    assert!(error.contains(
        "VAULT_BOOTSTRAP_ADMIN_EMAILS does not apply to OIDC; use VAULT_OIDC_BOOTSTRAP_ADMIN_SUBJECTS"
    ));
    assert!(
        error
            .contains("VAULT_OIDC_ISSUER must differ from header and development identity issuers")
    );
    assert!(error.contains("VAULT_OIDC_CLIENT_ID is required when VAULT_AUTH_MODE=oidc"));
    assert!(
        error.contains("VAULT_OIDC_CLIENT_SECRET is required for confidential OIDC client auth")
    );
    assert!(error.contains("OIDC redirect/public URL must use https outside local development"));
}

#[test]
fn runtime_validation_allows_local_http_oidc_in_production() {
    /*
     * Configures a complete OIDC client whose issuer and callback both use loopback HTTP. It
     * checks the local-development exception remains valid even when the broader runtime is
     * not in development mode.
     */
    let settings = AuthSettings {
        mode: vault_server::auth::AuthMode::Oidc,
        auth_mode_raw: "oidc".to_string(),
        oidc_issuer: "http://localhost:8080".to_string(),
        oidc_client_id: "vault".to_string(),
        oidc_client_secret: "oidc-secret".to_string(),
        oidc_redirect_uri: "http://localhost:8000/auth/callback".to_string(),
        signing_keys: signing_keys(TEST_SIGNING_ROOT, &[]),
        session_secret_source: SessionSecretSource::Explicit,
        ..AuthSettings::default()
    };

    settings
        .validate_runtime_config()
        .expect("local OIDC development origin may use http");
}
