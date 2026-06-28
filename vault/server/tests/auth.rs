use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use sqlx::Row;
use vault_server::auth::{
    AuthError, AuthSettings, SessionSecretRequirement, SessionSecretSource, UserContext,
    cookie_value, folder_permission_count_for_group, header_identity, oidc_token_urlsafe,
    session_identity, sign_session_payload, split_groups, verify_session_payload,
};
use vault_server::db;

async fn test_pool() -> sqlx::SqlitePool {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    let pool = db::connect(&db_path).await.expect("db connect");
    // Keep the temp directory alive for the life of the process by leaking it;
    // integration tests use one short-lived SQLite database per test.
    let _ = Box::leak(Box::new(temp_dir));
    pool
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
    let pool = test_pool().await;
    let settings = AuthSettings::default();

    let error = header_identity(&settings, &pool, &HeaderMap::new())
        .await
        .expect_err("missing user should reject");

    assert!(matches!(error, AuthError::AuthenticationRequired));
}

#[tokio::test]
async fn header_identity_is_stripped_and_groups_are_synced() {
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
async fn header_admin_group_removal_revokes_admin_context() {
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

#[tokio::test]
async fn disabled_header_user_request_does_not_sync_groups_or_profile() {
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
    let pool = test_pool().await;
    let settings = AuthSettings {
        dev_auth_enabled: true,
        base_domain: "vault.example.com".to_string(),
        ..AuthSettings::default()
    };

    let error = header_identity(&settings, &pool, &HeaderMap::new())
        .await
        .expect_err("non-local dev auth should reject");

    assert!(matches!(error, AuthError::AuthenticationRequired));
}

#[tokio::test]
async fn dev_auth_syncs_configured_groups_on_local_domain() {
    let pool = test_pool().await;
    let settings = AuthSettings {
        dev_auth_enabled: true,
        base_domain: "localhost".to_string(),
        dev_user: "dev-user".to_string(),
        dev_name: "Dev User".to_string(),
        dev_groups: split_groups("vault-users,vault-admin"),
        ..AuthSettings::default()
    };

    let user = header_identity(&settings, &pool, &HeaderMap::new())
        .await
        .expect("dev user");

    assert_eq!(user.subject, "dev-user");
    assert_eq!(user.name, "Dev User");
    assert_eq!(user.groups, ["vault-admin", "vault-users"]);
    assert!(user.is_admin);
}

#[tokio::test]
async fn session_payload_requires_expiration_and_numeric_user_id() {
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
async fn session_identity_ignores_inactive_users() {
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
    let settings = AuthSettings {
        session_secret_requirement: SessionSecretRequirement::Required,
        session_secret: "oidc-client-secret".to_string(),
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
}

#[test]
fn runtime_validation_rejects_development_session_secret_outside_dev() {
    let settings = AuthSettings {
        session_secret: "dev-insecure-session-secret".to_string(),
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
    let settings = AuthSettings {
        dev_mode: true,
        session_secret: "dev-insecure-session-secret".to_string(),
        session_secret_source: SessionSecretSource::Fallback,
        ..AuthSettings::default()
    };

    settings
        .validate_runtime_config()
        .expect("dev mode may use development secret");
}

#[test]
fn runtime_validation_rejects_invalid_auth_cookie_and_oidc_client_modes() {
    let settings = AuthSettings {
        auth_mode_raw: "bogus".to_string(),
        session_cookie_name: "vault session".to_string(),
        session_cookie_secure: "sometimes".to_string(),
        oidc_state_cookie_name: "vault;oidc".to_string(),
        oidc_client_auth: "implicit".to_string(),
        session_secret: "configured-session-secret".to_string(),
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
    let settings = AuthSettings {
        public_url: "http://vault.example.com".to_string(),
        session_secret: "configured-session-secret".to_string(),
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
    let settings = AuthSettings {
        mode: vault_server::auth::AuthMode::Oidc,
        auth_mode_raw: "oidc".to_string(),
        oidc_issuer: "http://idp.example.com".to_string(),
        oidc_client_id: String::new(),
        oidc_client_secret: String::new(),
        oidc_redirect_uri: "http://vault.example.com/auth/callback".to_string(),
        session_secret: "configured-session-secret".to_string(),
        session_secret_source: SessionSecretSource::Explicit,
        ..AuthSettings::default()
    };

    let error = settings
        .validate_runtime_config()
        .expect_err("incomplete oidc config should reject")
        .to_string();

    assert!(error.contains("VAULT_OIDC_ISSUER must use https outside local development"));
    assert!(error.contains("VAULT_OIDC_CLIENT_ID is required when VAULT_AUTH_MODE=oidc"));
    assert!(
        error.contains("VAULT_OIDC_CLIENT_SECRET is required for confidential OIDC client auth")
    );
    assert!(error.contains("OIDC redirect/public URL must use https outside local development"));
}

#[test]
fn runtime_validation_allows_local_http_oidc_in_production() {
    let settings = AuthSettings {
        mode: vault_server::auth::AuthMode::Oidc,
        auth_mode_raw: "oidc".to_string(),
        oidc_issuer: "http://localhost:8080".to_string(),
        oidc_client_id: "vault".to_string(),
        oidc_client_secret: "oidc-secret".to_string(),
        oidc_redirect_uri: "http://localhost:8000/auth/callback".to_string(),
        session_secret: "configured-session-secret".to_string(),
        session_secret_source: SessionSecretSource::Explicit,
        ..AuthSettings::default()
    };

    settings
        .validate_runtime_config()
        .expect("local OIDC development origin may use http");
}
