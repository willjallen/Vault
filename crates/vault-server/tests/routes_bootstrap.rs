use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use vault_server::auth::{AuthMode, AuthSettings};
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{
    VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path, get_root_folder,
};
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
        site_name: "Test Vault".to_string(),
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

async fn insert_versioned_document(pool: &sqlx::SqlitePool, folder_id: i64, name: &str) -> i64 {
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', ?, 3)
        ",
    )
    .bind(format!("test-hash-{name}"))
    .execute(pool)
    .await
    .expect("blob")
    .last_insert_rowid();
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, ?, 'admin', 'Admin', 'admin')
        ",
    )
    .bind(folder_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("document")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO document_versions
            (
                id,
                document_id,
                blob_id,
                version_number,
                committed_by,
                committed_by_name,
                message,
                mime_type,
                original_filename,
                created_via
            )
        VALUES
            (?, ?, ?, 1, 'admin', 'Admin', 'Uploaded file', 'text/plain', ?, 'upload')
        ",
    )
    .bind(format!("version-{document_id}"))
    .bind(document_id)
    .bind(blob_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("version");
    sqlx::query(
        r"
        UPDATE documents
        SET current_version_id = ?,
            latest_version_number = 1,
            version_count = 1
        WHERE id = ?
        ",
    )
    .bind(format!("version-{document_id}"))
    .bind(document_id)
    .execute(pool)
    .await
    .expect("current version");
    document_id
}

async fn insert_user_preferences(pool: &sqlx::SqlitePool, subject: &str, preferences: Value) {
    sqlx::query(
        r"
        INSERT INTO vault_users
            (issuer, subject, email, name, is_admin, is_active, preferences)
        VALUES
            ('headers', ?, ?, ?, 0, 1, ?)
        ",
    )
    .bind(subject)
    .bind(format!("{subject}@example.com"))
    .bind(subject)
    .bind(preferences.to_string())
    .execute(pool)
    .await
    .expect("user preferences");
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

fn authed_get(uri: &str, user: &str, groups: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", user)
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups)
        .body(Body::empty())
        .expect("request")
}

#[tokio::test]
async fn dev_auth_mode_rejects_when_disabled_even_with_identity_headers() {
    let auth = AuthSettings {
        mode: AuthMode::Dev,
        auth_mode_raw: "dev".to_string(),
        dev_auth_enabled: false,
        ..AuthSettings::default()
    };
    let (state, _temp_dir) = test_state_with_auth(auth).await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_get("/api/bootstrap", "spoofed", "vault-admin"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["detail"], "Development auth is disabled");
}

#[tokio::test]
async fn dev_auth_mode_ignores_identity_headers_and_uses_configured_local_user() {
    let auth = AuthSettings {
        mode: AuthMode::Dev,
        auth_mode_raw: "dev".to_string(),
        dev_auth_enabled: true,
        base_domain: "localhost".to_string(),
        dev_user: "local-dev".to_string(),
        dev_name: "Local Dev".to_string(),
        dev_email: "local-dev@example.com".to_string(),
        dev_groups: vault_server::auth::split_groups("vault-users"),
        ..AuthSettings::default()
    };
    let (state, _temp_dir) = test_state_with_auth(auth).await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_get("/api/bootstrap", "spoofed", "vault-admin"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["auth_mode"], "dev");
    assert_eq!(json["user"]["subject"], "local-dev");
    assert_eq!(json["user"]["name"], "Local Dev");
    assert_eq!(json["user"]["email"], "local-dev@example.com");
    assert_eq!(json["user"]["groups"], json!(["vault-users"]));
    assert_eq!(json["user"]["is_admin"], false);
}

#[tokio::test]
async fn bootstrap_returns_runtime_user_preferences_settings_and_current_folder() {
    let (state, _temp_dir) = test_state().await;
    let artists = create_group(&state.db, "artists").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, artists, true, true, false)
        .await
        .expect("artist root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, artists, true, true, false)
        .await
        .expect("artist project");
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(
            "/api/bootstrap?folder=Project",
            "artist",
            "artists",
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["auth_mode"], "headers");
    assert_eq!(json["base_domain"], "localhost");
    assert_eq!(json["dev_mode"], false);
    assert_eq!(json["site_name"], "Test Vault");
    assert_eq!(json["version"], "1.0.0");
    assert_eq!(json["current_folder"], "Project");
    assert_eq!(json["user"]["subject"], "artist");
    assert_eq!(json["preferences"]["themePreference"], "system");
    assert_eq!(json["preferences"]["palettePreference"], "cozy");
    assert_eq!(
        json["preferences"]["favoriteItems"],
        Value::Array(Vec::new())
    );
    assert_eq!(json["preferences"]["sidebarSectionSizes"]["folders"], 180);
    assert_eq!(
        json["preferences"]["sidebarSectionCollapsed"]["archive"],
        true,
    );
    assert_eq!(json["settings"]["archivePermanentDeleteAdminOnly"], true);
}

#[tokio::test]
async fn bootstrap_hides_inaccessible_folder_as_not_found() {
    let (state, _temp_dir) = test_state().await;
    let viewers = create_group(&state.db, "viewers").await;
    let outsiders = create_group(&state.db, "outsiders").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, viewers, true, true, false)
        .await
        .expect("viewer root");
    add_folder_permission(&state.db, root.id, outsiders, true, true, false)
        .await
        .expect("outsider root");
    let hidden = get_or_create_folder_path(&state.db, Some("Hidden"))
        .await
        .expect("hidden");
    add_folder_permission(&state.db, hidden.id, viewers, true, true, false)
        .await
        .expect("viewer hidden");
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(
            "/api/bootstrap?folder=Hidden",
            "outsider",
            "outsiders",
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["detail"], "Folder not found");
}

#[tokio::test]
async fn bootstrap_expands_visible_favorites_and_filters_inaccessible_targets() {
    let (state, _temp_dir) = test_state().await;
    let artists = create_group(&state.db, "artists").await;
    let confidential = create_group(&state.db, "confidential").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, artists, true, true, false)
        .await
        .expect("artist root");
    let art = get_or_create_folder_path(&state.db, Some("Art"))
        .await
        .expect("art");
    let hidden = get_or_create_folder_path(&state.db, Some("Hidden"))
        .await
        .expect("hidden");
    add_folder_permission(&state.db, art.id, artists, true, true, false)
        .await
        .expect("artist art");
    add_folder_permission(&state.db, hidden.id, confidential, true, true, false)
        .await
        .expect("confidential hidden");
    let visible_doc = insert_versioned_document(&state.db, art.id, "crate.txt").await;
    let hidden_doc = insert_versioned_document(&state.db, hidden.id, "secret.txt").await;
    insert_user_preferences(
        &state.db,
        "artist",
        json!({
            "favoriteItems": [
                {"type": "folder", "id": art.id},
                {"type": "document", "id": visible_doc},
                {"type": "folder", "id": hidden.id},
                {"type": "document", "id": hidden_doc},
                {"type": "document", "id": 999_999}
            ],
            "sidebarSectionCollapsed": {"favorites": true}
        }),
    )
    .await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_get("/api/bootstrap", "artist", "artists"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;
    let favorites = json["preferences"]["favoriteItems"]
        .as_array()
        .expect("favorites");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(favorites.len(), 2);
    assert_eq!(favorites[0]["type"], "folder");
    assert_eq!(favorites[0]["id"], art.id);
    assert_eq!(favorites[0]["path"], "Art");
    assert_eq!(favorites[0]["archived"], false);
    assert_eq!(favorites[0]["access"]["visible"], true);
    assert_eq!(favorites[1]["type"], "document");
    assert_eq!(favorites[1]["id"], visible_doc);
    assert_eq!(favorites[1]["path"], "Art/crate.txt");
    assert_eq!(favorites[1]["folder"], "Art");
    assert_eq!(favorites[1]["access"]["read"], true);
    assert_eq!(
        json["preferences"]["sidebarSectionCollapsed"]["favorites"],
        true,
    );
}
