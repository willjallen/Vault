use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
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
        static_dir: "vault/client".into(),
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

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

fn authed_get(uri: &str, user: &str, groups: &str) -> Request<Body> {
    authed_request(Method::GET, uri, user, groups, Body::empty())
}

fn authed_patch(uri: &str, user: &str, groups: &str, payload: &Value) -> Request<Body> {
    authed_request(
        Method::PATCH,
        uri,
        user,
        groups,
        Body::from(serde_json::to_vec(payload).expect("json payload")),
    )
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
async fn settings_route_requires_auth_and_returns_defaults() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let unauthenticated = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/settings")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("unauthenticated response");
    let unauthenticated_status = unauthenticated.status();
    let unauthenticated_json = response_json(unauthenticated).await;

    let response = app
        .oneshot(authed_get("/api/settings", "writer", "writers"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(unauthenticated_status, StatusCode::UNAUTHORIZED);
    assert_eq!(unauthenticated_json["detail"], "Authentication required");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["settings"]["archivePermanentDeleteAdminOnly"], true);
}

#[tokio::test]
async fn admin_directory_requires_admin_and_returns_users_groups_and_settings() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let denied = app
        .clone()
        .oneshot(authed_get("/api/admin/directory", "writer", "writers"))
        .await
        .expect("denied response");
    let denied_status = denied.status();
    let denied_json = response_json(denied).await;

    let response = app
        .oneshot(authed_get("/api/admin/directory", "admin", "vault-admin"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(denied_status, StatusCode::FORBIDDEN);
    assert_eq!(denied_json["detail"], "Admin access required");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["dev_mode"], false);
    assert_eq!(json["settings"]["archivePermanentDeleteAdminOnly"], true);
    assert_eq!(json["users"][0]["subject"], "admin");
    assert_eq!(json["users"][0]["is_admin"], true);
    assert_eq!(json["users"][0]["groups"][0]["name"], "vault-admin");
    assert_eq!(json["groups"][0]["name"], "vault-admin");
    assert_eq!(json["groups"][0]["members"][0]["name"], "admin");
}

#[tokio::test]
async fn admin_directory_allows_bootstrap_admin_email_without_stored_admin_flag() {
    let (state, _temp_dir) = test_state_with_auth(AuthSettings {
        bootstrap_admin_emails: ["owner@example.com".to_string()].into_iter().collect(),
        ..AuthSettings::default()
    })
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_get("/api/admin/directory", "owner", "artists"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    let owner = json["users"]
        .as_array()
        .expect("users")
        .iter()
        .find(|user| user["subject"] == "owner")
        .expect("owner row");
    assert_eq!(owner["email"], "owner@example.com");
    assert_eq!(owner["is_admin"], true);
    assert_eq!(owner["groups"][0]["name"], "artists");
    let stored_admin: i64 =
        sqlx::query_scalar("SELECT is_admin FROM vault_users WHERE subject = 'owner'")
            .fetch_one(&pool)
            .await
            .expect("stored user");
    assert_eq!(stored_admin, 0);
}

#[tokio::test]
async fn admin_settings_patch_persists_setting_and_emits_state_event() {
    let (state, _temp_dir) = test_state().await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_patch(
            "/api/admin/settings",
            "admin",
            "vault-admin",
            &json!({"settings": {"archivePermanentDeleteAdminOnly": false}}),
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["settings"]["archivePermanentDeleteAdminOnly"], false);

    let reader_response = app
        .clone()
        .oneshot(authed_get("/api/settings", "writer", "writers"))
        .await
        .expect("reader response");
    let reader_json = response_json(reader_response).await;
    assert_eq!(
        reader_json["settings"]["archivePermanentDeleteAdminOnly"],
        false,
    );

    let stored = sqlx::query_scalar::<_, String>(
        "SELECT value FROM vault_settings WHERE key = 'archivePermanentDeleteAdminOnly'",
    )
    .fetch_one(&pool)
    .await
    .expect("stored setting");
    assert_eq!(
        serde_json::from_str::<Value>(&stored).expect("setting json"),
        false
    );

    let event =
        sqlx::query_as::<_, (String, String)>("SELECT event_type, resources FROM state_events")
            .fetch_one(&pool)
            .await
            .expect("state event");
    assert_eq!(event.0, "admin.settings.updated");
    assert_eq!(
        serde_json::from_str::<Value>(&event.1).expect("resources json"),
        json!(["admin", "settings"]),
    );
}

#[tokio::test]
async fn admin_settings_patch_allows_bootstrap_admin_email_without_stored_admin_flag() {
    let (state, _temp_dir) = test_state_with_auth(AuthSettings {
        bootstrap_admin_emails: ["owner@example.com".to_string()].into_iter().collect(),
        ..AuthSettings::default()
    })
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_patch(
            "/api/admin/settings",
            "owner",
            "artists",
            &json!({"settings": {"archivePermanentDeleteAdminOnly": false}}),
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["settings"]["archivePermanentDeleteAdminOnly"], false);
    let owner = json["users"]
        .as_array()
        .expect("users")
        .iter()
        .find(|user| user["subject"] == "owner")
        .expect("owner row");
    assert_eq!(owner["is_admin"], true);
    let stored_admin: i64 =
        sqlx::query_scalar("SELECT is_admin FROM vault_users WHERE subject = 'owner'")
            .fetch_one(&pool)
            .await
            .expect("stored user");
    let event =
        sqlx::query_as::<_, (String, String)>("SELECT event_type, resources FROM state_events")
            .fetch_one(&pool)
            .await
            .expect("state event");

    assert_eq!(stored_admin, 0);
    assert_eq!(event.0, "admin.settings.updated");
    assert_eq!(
        serde_json::from_str::<Value>(&event.1).expect("resources json"),
        json!(["admin", "settings"]),
    );
}

#[tokio::test]
async fn admin_settings_patch_rejects_non_admin_and_invalid_settings() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let denied = app
        .clone()
        .oneshot(authed_patch(
            "/api/admin/settings",
            "writer",
            "writers",
            &json!({"settings": {"archivePermanentDeleteAdminOnly": false}}),
        ))
        .await
        .expect("denied response");
    let denied_status = denied.status();
    let denied_json = response_json(denied).await;

    let unknown = app
        .clone()
        .oneshot(authed_patch(
            "/api/admin/settings",
            "admin",
            "vault-admin",
            &json!({"settings": {"deleteAnything": true}}),
        ))
        .await
        .expect("unknown response");
    let unknown_status = unknown.status();
    let unknown_json = response_json(unknown).await;

    let invalid_type = app
        .oneshot(authed_patch(
            "/api/admin/settings",
            "admin",
            "vault-admin",
            &json!({"settings": {"archivePermanentDeleteAdminOnly": "no"}}),
        ))
        .await
        .expect("invalid type response");
    let invalid_type_status = invalid_type.status();
    let invalid_type_json = response_json(invalid_type).await;

    assert_eq!(denied_status, StatusCode::FORBIDDEN);
    assert_eq!(denied_json["detail"], "Admin access required");
    assert_eq!(unknown_status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown_json["detail"], "Unknown setting: deleteAnything");
    assert_eq!(invalid_type_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid_type_json["detail"],
        "archivePermanentDeleteAdminOnly must be a boolean",
    );
}
