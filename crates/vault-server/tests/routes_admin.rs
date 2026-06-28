use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use vault_server::admin::{
    AdminError, AdminGroupRequest, AdminUserUpdatePayload, delete_group, remove_group_member,
    update_group, update_user,
};
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{VAULT_ROOT_KEY, add_folder_permission, get_root_folder};
use vault_server::http::{self, AppState};
use vault_server::storage::LocalBlobStorage;

async fn test_state() -> (AppState, tempfile::TempDir) {
    test_state_with_auth(AuthSettings::default()).await
}

async fn test_state_with_auth(auth: AuthSettings) -> (AppState, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        host: "127.0.0.1".parse().expect("host"),
        port: 0,
        data_dir: temp_dir.path().to_path_buf(),
        db_path: Some(temp_dir.path().join("vault.db")),
        objects_path: None,
        transfers_path: None,
        static_dir: "app/static".into(),
        storage_backend: "local".to_string(),
        storage_prefix: String::new(),
        site_name: "Vault".to_string(),
        max_upload_bytes: 5 * 1024 * 1024 * 1024,
        transfer_chunk_bytes: 32 * 1024 * 1024,
        transfer_session_ttl_seconds: 86_400,
        export_ttl_seconds: 86_400,
        export_workers: 1,
        export_zip_compression_threshold_bytes: 3 * 1024 * 1024 * 1024,
        export_zip_compresslevel: 1,
        ttl_sweep_interval_seconds: 60,
        gzip_minimum_size: 1024,
        gzip_compresslevel: 6,
    };
    let db = db::connect(&config.db_path()).await.expect("db");
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    let state = AppState::new(config, auth, db, Arc::new(storage));
    (state, temp_dir)
}

async fn create_group(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO vault_groups (name) VALUES (?)")
        .bind(name)
        .execute(pool)
        .await
        .expect("create group")
        .last_insert_rowid()
}

async fn user_id(pool: &sqlx::SqlitePool, subject: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT id FROM vault_users WHERE subject = ?")
        .bind(subject)
        .fetch_one(pool)
        .await
        .expect("user id")
}

async fn group_id(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT id FROM vault_groups WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("group id")
}

async fn assert_admin_events(pool: &sqlx::SqlitePool, expected: &[&str]) {
    let events = sqlx::query_scalar::<_, String>("SELECT event_type FROM state_events ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("events");
    assert_eq!(events, expected);
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

fn group_named<'a>(payload: &'a Value, name: &str) -> &'a Value {
    payload["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|group| group["name"] == name)
        .expect("named group")
}

fn user_named<'a>(payload: &'a Value, subject: &str) -> &'a Value {
    payload["users"]
        .as_array()
        .expect("users")
        .iter()
        .find(|user| user["subject"] == subject)
        .expect("named user")
}

fn authed_get(uri: &str, user: &str, groups: &str) -> Request<Body> {
    authed_request(Method::GET, uri, user, groups, Body::empty())
}

fn authed_json(
    method: Method,
    uri: &str,
    user: &str,
    groups: &str,
    payload: &Value,
) -> Request<Body> {
    authed_request(
        method,
        uri,
        user,
        groups,
        Body::from(serde_json::to_vec(payload).expect("json payload")),
    )
}

fn authed_delete(uri: &str, user: &str, groups: &str) -> Request<Body> {
    authed_request(Method::DELETE, uri, user, groups, Body::empty())
}

fn authed_request(
    method: Method,
    uri: &str,
    user: &str,
    groups: &str,
    body: Body,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", user)
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups)
        .header("Content-Type", "application/json")
        .body(body)
        .expect("request")
}

#[tokio::test]
async fn admin_group_routes_manage_groups_members_and_state_events() {
    let (state, _temp_dir) = test_state().await;
    let pool = state.db.clone();
    let app = http::router(state);
    app.clone()
        .oneshot(authed_get("/api/settings", "artist", "artists"))
        .await
        .expect("seed user");

    let created = app
        .clone()
        .oneshot(authed_json(
            Method::POST,
            "/api/admin/groups",
            "admin",
            "vault-admin",
            &json!({"name": "  Artists   Team  ", "description": "  Modelers  "}),
        ))
        .await
        .expect("create response");
    let created_status = created.status();
    let created_json = response_json(created).await;
    let group_id = group_id(&pool, "Artists Team").await;
    let artist_id = user_id(&pool, "artist").await;

    assert_eq!(created_status, StatusCode::OK);
    assert_eq!(
        group_named(&created_json, "Artists Team")["description"],
        "Modelers"
    );

    let added = app
        .clone()
        .oneshot(authed_json(
            Method::POST,
            &format!("/api/admin/groups/{group_id}/members"),
            "admin",
            "vault-admin",
            &json!({"user_id": artist_id}),
        ))
        .await
        .expect("add member response");
    let added_json = response_json(added).await;
    assert_eq!(
        group_named(&added_json, "Artists Team")["members"][0]["name"],
        "artist",
    );

    let updated = app
        .clone()
        .oneshot(authed_json(
            Method::PATCH,
            &format!("/api/admin/groups/{group_id}"),
            "admin",
            "vault-admin",
            &json!({"name": "Production", "description": "  "}),
        ))
        .await
        .expect("update group response");
    let updated_json = response_json(updated).await;
    assert_eq!(group_named(&updated_json, "Production")["description"], "");

    let removed = app
        .clone()
        .oneshot(authed_delete(
            &format!("/api/admin/groups/{group_id}/members/{artist_id}"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("remove member response");
    let removed_json = response_json(removed).await;
    assert_eq!(
        group_named(&removed_json, "Production")["members"]
            .as_array()
            .expect("members")
            .len(),
        0,
    );

    let deleted = app
        .oneshot(authed_delete(
            &format!("/api/admin/groups/{group_id}"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("delete group response");
    let deleted_json = response_json(deleted).await;
    assert!(
        deleted_json["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .all(|group| group["name"] != "Production")
    );

    assert_admin_events(
        &pool,
        &[
            "admin.group.created",
            "admin.group.member.added",
            "admin.group.updated",
            "admin.group.member.removed",
            "admin.group.deleted",
        ],
    )
    .await;
}

#[tokio::test]
async fn admin_group_routes_allow_bootstrap_admin_email_without_stored_admin_flag() {
    let (state, _temp_dir) = test_state_with_auth(AuthSettings {
        bootstrap_admin_emails: ["owner@example.com".to_string()].into_iter().collect(),
        ..AuthSettings::default()
    })
    .await;
    let pool = state.db.clone();
    let app = http::router(state);
    app.clone()
        .oneshot(authed_get("/api/settings", "artist", "artists"))
        .await
        .expect("seed user");
    let artist_id = user_id(&pool, "artist").await;

    let created = app
        .clone()
        .oneshot(authed_json(
            Method::POST,
            "/api/admin/groups",
            "owner",
            "artists",
            &json!({"name": "Bootstrap Managers", "description": "  Owners  "}),
        ))
        .await
        .expect("create response");
    let created_status = created.status();
    let created_json = response_json(created).await;
    let group_id = group_id(&pool, "Bootstrap Managers").await;

    assert_eq!(created_status, StatusCode::OK);
    assert_eq!(
        group_named(&created_json, "Bootstrap Managers")["description"],
        "Owners"
    );

    let added = app
        .clone()
        .oneshot(authed_json(
            Method::POST,
            &format!("/api/admin/groups/{group_id}/members"),
            "owner",
            "artists",
            &json!({"user_id": artist_id}),
        ))
        .await
        .expect("add member response");
    let added_json = response_json(added).await;
    let group = group_named(&added_json, "Bootstrap Managers");
    let stored_owner_admin =
        sqlx::query_scalar::<_, i64>("SELECT is_admin FROM vault_users WHERE subject = 'owner'")
            .fetch_one(&pool)
            .await
            .expect("stored owner admin flag");

    assert_eq!(group["members"][0]["name"], "artist");
    assert_eq!(stored_owner_admin, 0);
    assert_admin_events(&pool, &["admin.group.created", "admin.group.member.added"]).await;
}

#[tokio::test]
async fn admin_directory_user_timestamps_use_python_datetime_iso_shape() {
    let (state, _temp_dir) = test_state().await;
    sqlx::query(
        r"
        INSERT INTO vault_users
            (
                issuer,
                subject,
                email,
                name,
                is_admin,
                is_active,
                created_at,
                last_login_at,
                last_seen_at
            )
        VALUES
            (
                'oidc',
                'artist',
                'artist@example.com',
                'Artist',
                0,
                1,
                '2026-06-26 19:03:00.123456',
                '2026-06-26T19:04:00Z',
                '2026-06-26T19:05:00.654321+00:00'
            )
        ",
    )
    .execute(&state.db)
    .await
    .expect("seed artist user");
    let app = http::router(state);

    let response = app
        .oneshot(authed_get("/api/admin/directory", "admin", "vault-admin"))
        .await
        .expect("directory response");
    let status = response.status();
    let json = response_json(response).await;
    let artist = user_named(&json, "artist");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(artist["created_at"], "2026-06-26T19:03:00.123456");
    assert_eq!(artist["last_login_at"], "2026-06-26T19:04:00+00:00");
    assert_eq!(artist["last_seen_at"], "2026-06-26T19:05:00.654321+00:00",);
}

#[tokio::test]
async fn admin_settings_route_enforces_admin_validation_persistence_and_state_event() {
    let (state, _temp_dir) = test_state().await;
    let pool = state.db.clone();
    let app = http::router(state);

    let non_admin = app
        .clone()
        .oneshot(authed_json(
            Method::PATCH,
            "/api/admin/settings",
            "writer",
            "writers",
            &json!({"settings": {"archivePermanentDeleteAdminOnly": false}}),
        ))
        .await
        .expect("non-admin settings response");
    let non_admin_status = non_admin.status();
    let non_admin_json = response_json(non_admin).await;
    assert_eq!(non_admin_status, StatusCode::FORBIDDEN);
    assert_eq!(non_admin_json["detail"], "Admin access required");

    let invalid = app
        .clone()
        .oneshot(authed_json(
            Method::PATCH,
            "/api/admin/settings",
            "admin",
            "vault-admin",
            &json!({"settings": {"deleteAnything": true}}),
        ))
        .await
        .expect("invalid settings response");
    let invalid_status = invalid.status();
    let invalid_json = response_json(invalid).await;
    assert_eq!(invalid_status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_json["detail"], "Unknown setting: deleteAnything");

    let settings_before = app
        .clone()
        .oneshot(authed_get("/api/settings", "writer", "writers"))
        .await
        .expect("settings before");
    let settings_before = response_json(settings_before).await;
    let event_count_before = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("event count before");
    assert_eq!(
        settings_before["settings"]["archivePermanentDeleteAdminOnly"],
        true,
    );
    assert_eq!(event_count_before, 0);

    let updated = app
        .clone()
        .oneshot(authed_json(
            Method::PATCH,
            "/api/admin/settings",
            "admin",
            "vault-admin",
            &json!({"settings": {"archivePermanentDeleteAdminOnly": false}}),
        ))
        .await
        .expect("settings update response");
    let updated_status = updated.status();
    let updated_json = response_json(updated).await;
    assert_eq!(updated_status, StatusCode::OK);
    assert_eq!(
        updated_json["settings"]["archivePermanentDeleteAdminOnly"],
        false,
    );

    let synced = app
        .oneshot(authed_get("/api/settings", "writer", "writers"))
        .await
        .expect("synced settings");
    let synced = response_json(synced).await;
    let state_event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, resources FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("settings state event");

    assert_eq!(synced["settings"]["archivePermanentDeleteAdminOnly"], false,);
    assert_eq!(state_event.0, "admin.settings.updated");
    assert_eq!(
        serde_json::from_str::<Value>(&state_event.1).expect("resources"),
        json!(["admin", "settings"]),
    );
}

#[tokio::test]
async fn admin_user_routes_toggle_flags_and_preserve_active_admin() {
    let (state, _temp_dir) = test_state().await;
    let pool = state.db.clone();
    let app = http::router(state);
    app.clone()
        .oneshot(authed_get("/api/admin/directory", "admin", "vault-admin"))
        .await
        .expect("seed admin");
    app.clone()
        .oneshot(authed_get("/api/admin/directory", "backup", "vault-admin"))
        .await
        .expect("seed backup admin");
    let admin_id = user_id(&pool, "admin").await;
    let backup_id = user_id(&pool, "backup").await;

    let demoted = app
        .clone()
        .oneshot(authed_json(
            Method::PATCH,
            &format!("/api/admin/users/{admin_id}"),
            "admin",
            "vault-admin",
            &json!({"is_admin": false, "is_active": false}),
        ))
        .await
        .expect("demote response");
    let demoted_json = response_json(demoted).await;
    let admin_row = user_named(&demoted_json, "admin");

    assert_eq!(admin_row["is_active"], false);
    assert_eq!(admin_row["is_admin"], true);

    let denied = app
        .oneshot(authed_json(
            Method::PATCH,
            &format!("/api/admin/users/{backup_id}"),
            "backup",
            "vault-admin",
            &json!({"is_active": false}),
        ))
        .await
        .expect("denied response");
    let denied_status = denied.status();
    let denied_json = response_json(denied).await;

    assert_eq!(denied_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        denied_json["detail"],
        "At least one active admin is required",
    );
}

#[tokio::test]
async fn admin_user_routes_allow_bootstrap_admin_email_without_stored_admin_flag() {
    let (state, _temp_dir) = test_state_with_auth(AuthSettings {
        bootstrap_admin_emails: ["owner@example.com".to_string()].into_iter().collect(),
        ..AuthSettings::default()
    })
    .await;
    let pool = state.db.clone();
    let app = http::router(state);
    app.clone()
        .oneshot(authed_get("/api/settings", "editor", "artists"))
        .await
        .expect("seed editable user");
    let editor_id = user_id(&pool, "editor").await;

    let updated = app
        .clone()
        .oneshot(authed_json(
            Method::PATCH,
            &format!("/api/admin/users/{editor_id}"),
            "owner",
            "artists",
            &json!({"is_active": false}),
        ))
        .await
        .expect("update response");
    let updated_status = updated.status();
    let updated_json = response_json(updated).await;
    let editor_row = user_named(&updated_json, "editor");
    let owner_row = user_named(&updated_json, "owner");
    let stored_owner_admin =
        sqlx::query_scalar::<_, i64>("SELECT is_admin FROM vault_users WHERE subject = 'owner'")
            .fetch_one(&pool)
            .await
            .expect("stored owner admin flag");

    assert_eq!(updated_status, StatusCode::OK);
    assert_eq!(editor_row["is_active"], false);
    assert_eq!(owner_row["is_admin"], true);
    assert_eq!(stored_owner_admin, 0);
    assert_admin_events(&pool, &["admin.user.updated"]).await;
}

#[tokio::test]
async fn admin_last_admin_guard_normalizes_persisted_group_names() {
    let (state, _temp_dir) = test_state().await;
    let pool = state.db.clone();
    let auth = state.auth.clone();
    let group_id = create_group(&pool, "Vault-Admin").await;
    let user_id = sqlx::query(
        r"
        INSERT INTO vault_users
            (issuer, subject, email, name, is_admin, is_active)
        VALUES
            ('oidc', 'bob', 'bob@example.com', 'Bob', 0, 1)
        ",
    )
    .execute(&pool)
    .await
    .expect("insert user")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO vault_group_memberships (user_id, group_id)
        VALUES (?, ?)
        ",
    )
    .bind(user_id)
    .bind(group_id)
    .execute(&pool)
    .await
    .expect("insert membership");

    let error = update_user(
        &pool,
        &auth,
        user_id,
        &AdminUserUpdatePayload {
            is_admin: None,
            is_active: Some(false),
        },
    )
    .await
    .expect_err("last admin update should fail");
    let is_active = sqlx::query_scalar::<_, i64>("SELECT is_active FROM vault_users WHERE id = ?")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("user active flag");
    let event_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("state event count");

    assert!(matches!(error, AdminError::LastActiveAdminRequired));
    assert_eq!(is_active, 1);
    assert_eq!(event_count, 0);
}

#[tokio::test]
async fn admin_group_last_admin_guards_normalize_persisted_group_names() {
    let (state, _temp_dir) = test_state().await;
    let pool = state.db.clone();
    let auth = state.auth.clone();
    let group_id = create_group(&pool, "Vault-Admin").await;
    let user_id = sqlx::query(
        r"
        INSERT INTO vault_users
            (issuer, subject, email, name, is_admin, is_active)
        VALUES
            ('oidc', 'bob', 'bob@example.com', 'Bob', 0, 1)
        ",
    )
    .execute(&pool)
    .await
    .expect("insert user")
    .last_insert_rowid();
    sqlx::query("INSERT INTO vault_group_memberships (user_id, group_id) VALUES (?, ?)")
        .bind(user_id)
        .bind(group_id)
        .execute(&pool)
        .await
        .expect("insert membership");

    let delete_error = delete_group(&pool, &auth, group_id)
        .await
        .expect_err("delete should preserve last admin");
    let rename_error = update_group(
        &pool,
        &auth,
        group_id,
        &AdminGroupRequest {
            name: "staff".to_string(),
            description: Some("Staff".to_string()),
        },
    )
    .await
    .expect_err("rename should preserve last admin");
    let remove_error = remove_group_member(&pool, &auth, group_id, user_id)
        .await
        .expect_err("membership removal should preserve last admin");

    let group_name = sqlx::query_scalar::<_, String>("SELECT name FROM vault_groups WHERE id = ?")
        .bind(group_id)
        .fetch_one(&pool)
        .await
        .expect("group name");
    let membership_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM vault_group_memberships WHERE group_id = ? AND user_id = ?",
    )
    .bind(group_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("membership count");
    let event_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("state event count");

    assert!(matches!(delete_error, AdminError::LastActiveAdminRequired));
    assert!(matches!(rename_error, AdminError::LastActiveAdminRequired));
    assert!(matches!(remove_error, AdminError::LastActiveAdminRequired));
    assert_eq!(group_name, "Vault-Admin");
    assert_eq!(membership_count, 1);
    assert_eq!(event_count, 0);
}

#[tokio::test]
async fn admin_group_routes_validate_errors_and_last_admin_guards() {
    let (state, _temp_dir) = test_state().await;
    let pool = state.db.clone();
    let app = http::router(state);
    app.clone()
        .oneshot(authed_get("/api/admin/directory", "admin", "vault-admin"))
        .await
        .expect("seed admin");
    let vault_admin_group = group_id(&pool, "vault-admin").await;
    let admin_id = user_id(&pool, "admin").await;

    let invalid_name = app
        .clone()
        .oneshot(authed_json(
            Method::POST,
            "/api/admin/groups",
            "admin",
            "vault-admin",
            &json!({"name": "../bad"}),
        ))
        .await
        .expect("invalid name");
    let invalid_name_status = invalid_name.status();
    let invalid_name_json = response_json(invalid_name).await;
    assert_eq!(invalid_name_status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_name_json["detail"], "Invalid group name");

    let duplicate = app
        .clone()
        .oneshot(authed_json(
            Method::POST,
            "/api/admin/groups",
            "admin",
            "vault-admin",
            &json!({"name": "Vault-Admin"}),
        ))
        .await
        .expect("duplicate");
    let duplicate_status = duplicate.status();
    let duplicate_json = response_json(duplicate).await;
    assert_eq!(duplicate_status, StatusCode::CONFLICT);
    assert_eq!(duplicate_json["detail"], "Group already exists");

    let rename_guard = app
        .clone()
        .oneshot(authed_json(
            Method::PATCH,
            &format!("/api/admin/groups/{vault_admin_group}"),
            "admin",
            "vault-admin",
            &json!({"name": "staff"}),
        ))
        .await
        .expect("rename guard");
    let rename_guard_status = rename_guard.status();
    let rename_guard_json = response_json(rename_guard).await;
    assert_eq!(rename_guard_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        rename_guard_json["detail"],
        "At least one active admin is required",
    );

    let remove_guard = app
        .clone()
        .oneshot(authed_delete(
            &format!("/api/admin/groups/{vault_admin_group}/members/{admin_id}"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("remove guard");
    let remove_guard_status = remove_guard.status();
    let remove_guard_json = response_json(remove_guard).await;
    assert_eq!(remove_guard_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        remove_guard_json["detail"],
        "At least one active admin is required",
    );

    let locked_group = create_group(&pool, "confidential").await;
    let root = get_root_folder(&pool, VAULT_ROOT_KEY).await.expect("root");
    add_folder_permission(&pool, root.id, locked_group, true, true, true)
        .await
        .expect("folder permission");
    let locked_delete = app
        .oneshot(authed_delete(
            &format!("/api/admin/groups/{locked_group}"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("locked delete");
    let locked_delete_status = locked_delete.status();
    let locked_delete_json = response_json(locked_delete).await;
    assert_eq!(locked_delete_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        locked_delete_json["detail"],
        "Group is used by folder permissions",
    );
}
