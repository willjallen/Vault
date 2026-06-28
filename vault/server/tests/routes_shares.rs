use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{
    VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path, get_root_folder,
};
use vault_server::http::{self, AppState};
use vault_server::storage::LocalBlobStorage;

async fn test_state(public_url: Option<&str>) -> (AppState, tempfile::TempDir) {
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
    let auth = AuthSettings {
        public_url: public_url.unwrap_or_default().to_string(),
        ..Default::default()
    };
    let state = AppState::new(config, auth, db, Arc::new(storage));
    (state, temp_dir)
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

fn authed_json_request(
    method: Method,
    uri: &str,
    payload: &Value,
    user: &str,
    groups: &str,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", display_name(user))
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("request")
}

fn authed_get(uri: &str, user: &str, groups: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", display_name(user))
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups)
        .body(Body::empty())
        .expect("request")
}

async fn post_share(
    app: &axum::Router,
    payload: Value,
    user: &str,
    groups: &str,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/share-links",
            &payload,
            user,
            groups,
        ))
        .await
        .expect("post share");
    let status = response.status();
    (status, response_json(response).await)
}

async fn resolve_share_code(
    app: &axum::Router,
    code: &str,
    user: &str,
    groups: &str,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(authed_get(
            &format!("/api/share-links/{code}"),
            user,
            groups,
        ))
        .await
        .expect("resolve share");
    let status = response.status();
    (status, response_json(response).await)
}

fn display_name(user: &str) -> String {
    let mut chars = user.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "User".to_string(),
    }
}

async fn create_group(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO vault_groups (name) VALUES (?)")
        .bind(name)
        .execute(pool)
        .await
        .expect("create group")
        .last_insert_rowid()
}

async fn grant_access(pool: &sqlx::SqlitePool, folder_id: i64, group_id: i64) {
    add_folder_permission(pool, folder_id, group_id, true, true, false)
        .await
        .expect("folder permission");
}

async fn insert_versioned_document(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    data: &[u8],
) -> i64 {
    let hash = sha256_hex(data);
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', ?, ?)
        ",
    )
    .bind(&hash)
    .bind(i64::try_from(data.len()).expect("blob size"))
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
    let version_id = format!("share-version-{document_id}");
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
    .bind(&version_id)
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
    .bind(version_id)
    .bind(document_id)
    .execute(pool)
    .await
    .expect("current version");
    document_id
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    lower_hex(&digest)
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[tokio::test]
async fn share_routes_resolve_current_targets_and_enforce_access() {
    let (state, _temp_dir) = test_state(None).await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let project = get_or_create_folder_path(&state.db, Some("Art"))
        .await
        .expect("project");
    let artists = create_group(&state.db, "artists").await;
    let outsiders = create_group(&state.db, "outsiders").await;
    grant_access(&state.db, root.id, artists).await;
    grant_access(&state.db, root.id, outsiders).await;
    grant_access(&state.db, project.id, artists).await;
    let document_id =
        insert_versioned_document(&state.db, project.id, "mesh.blend", b"mesh bytes").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let (status, doc_share) = post_share(
        &app,
        json!({"target_type": "document", "document_id": document_id}),
        "admin",
        "vault-admin",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(doc_share["url"].as_str().expect("url").starts_with("/s/"));
    assert_eq!(doc_share["access_mode"], "internal");

    let (status, folder_share) = post_share(
        &app,
        json!({"target_type": "folder", "path": "Art"}),
        "admin",
        "vault-admin",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    sqlx::query("UPDATE folders SET name = 'Concepts' WHERE id = ?")
        .bind(project.id)
        .execute(&pool)
        .await
        .expect("rename folder");

    let doc_code = doc_share["code"].as_str().expect("code");
    let folder_code = folder_share["code"].as_str().expect("code");
    let (status, resolved_doc) = resolve_share_code(&app, doc_code, "artist", "artists").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolved_doc["target_type"], "document");
    assert_eq!(resolved_doc["document_id"], document_id);
    assert_eq!(resolved_doc["folder"], "Concepts");
    assert_eq!(resolved_doc["document"]["path"], "Concepts/mesh.blend");

    let (status, resolved_folder) =
        resolve_share_code(&app, folder_code, "artist", "artists").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolved_folder["target_type"], "folder");
    assert_eq!(resolved_folder["folder"], "Concepts");
    assert!(resolved_folder["folder_item"].get("access").is_none());

    let (status, _) = resolve_share_code(&app, doc_code, "outsider", "outsiders").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = resolve_share_code(&app, "not-a-valid-code!", "admin", "vault-admin").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    sqlx::query("UPDATE share_links SET disabled_at = CURRENT_TIMESTAMP WHERE code = ?")
        .bind(doc_code)
        .execute(&pool)
        .await
        .expect("disable link");
    sqlx::query("UPDATE share_links SET expires_at = datetime('now', '-1 second') WHERE code = ?")
        .bind(folder_code)
        .execute(&pool)
        .await
        .expect("expire link");

    let (status, _) = resolve_share_code(&app, doc_code, "artist", "artists").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = resolve_share_code(&app, folder_code, "artist", "artists").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn document_share_rechecks_current_folder_access_after_move() {
    let (state, _temp_dir) = test_state(None).await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let secret = get_or_create_folder_path(&state.db, Some("Secret"))
        .await
        .expect("secret");
    let artists = create_group(&state.db, "artists").await;
    let confidential = create_group(&state.db, "confidential").await;
    grant_access(&state.db, root.id, artists).await;
    grant_access(&state.db, project.id, artists).await;
    grant_access(&state.db, secret.id, confidential).await;
    let document_id =
        insert_versioned_document(&state.db, project.id, "brief.txt", b"visible before move").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let (status, share) = post_share(
        &app,
        json!({"target_type": "document", "document_id": document_id}),
        "admin",
        "vault-admin",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let code = share["code"].as_str().expect("code");

    let (status, visible) = resolve_share_code(&app, code, "artist", "artists").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(visible["document"]["path"], "Project/brief.txt");

    sqlx::query("UPDATE documents SET folder_id = ? WHERE id = ?")
        .bind(secret.id)
        .bind(document_id)
        .execute(&pool)
        .await
        .expect("move document");

    let (status, _) = resolve_share_code(&app, code, "artist", "artists").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, admin_visible) = resolve_share_code(&app, code, "admin", "vault-admin").await;
    let link_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM share_links WHERE code = ?")
        .bind(code)
        .fetch_one(&pool)
        .await
        .expect("share link count");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(admin_visible["document"]["path"], "Secret/brief.txt");
    assert_eq!(link_count, 1);
}

#[tokio::test]
async fn share_creation_uses_public_url_and_rejects_bad_targets() {
    let (state, _temp_dir) = test_state(Some("https://vault.example.com/")).await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let artists = create_group(&state.db, "artists").await;
    let outsiders = create_group(&state.db, "outsiders").await;
    grant_access(&state.db, root.id, artists).await;
    grant_access(&state.db, root.id, outsiders).await;
    grant_access(&state.db, project.id, artists).await;
    let document_id =
        insert_versioned_document(&state.db, project.id, "concept.png", b"concept").await;
    let app = http::router(state);

    let (status, invalid_target) = post_share(
        &app,
        json!({"target_type": "planet", "document_id": document_id}),
        "artist",
        "artists",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_target["detail"], "Invalid share target");

    let (status, missing_doc_id) = post_share(
        &app,
        json!({"target_type": "document"}),
        "artist",
        "artists",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(missing_doc_id["detail"], "Document id is required");

    let (status, _) = post_share(
        &app,
        json!({"target_type": "folder", "path": "Missing"}),
        "artist",
        "artists",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = post_share(
        &app,
        json!({"target_type": "document", "document_id": document_id}),
        "outsider",
        "outsiders",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = post_share(
        &app,
        json!({"target_type": "folder", "path": "Project"}),
        "outsider",
        "outsiders",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, visible_document) = post_share(
        &app,
        json!({"target_type": "file", "document_id": document_id}),
        "artist",
        "artists",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(visible_document["target_type"], "document");
    assert_eq!(
        visible_document["url"],
        format!(
            "https://vault.example.com/s/{}",
            visible_document["code"].as_str().expect("code")
        )
    );

    let (status, _) = post_share(
        &app,
        json!({"target_type": "folder", "folder_id": project.id}),
        "artist",
        "artists",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn folder_share_stats_exclude_inaccessible_descendants_and_recheck_access() {
    let (state, _temp_dir) = test_state(None).await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let private = get_or_create_folder_path(&state.db, Some("Project/Private"))
        .await
        .expect("private");
    let secret = get_or_create_folder_path(&state.db, Some("Secret"))
        .await
        .expect("secret");
    let artists = create_group(&state.db, "artists").await;
    let confidential = create_group(&state.db, "confidential").await;
    grant_access(&state.db, root.id, artists).await;
    grant_access(&state.db, private.id, confidential).await;
    grant_access(&state.db, secret.id, confidential).await;
    insert_versioned_document(&state.db, project.id, "visible.txt", b"ok").await;
    insert_versioned_document(&state.db, private.id, "secret.txt", b"topsecret").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let share = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/share-links",
            &json!({"target_type": "folder", "folder_id": project.id}),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("folder share");
    assert_eq!(share.status(), StatusCode::OK);
    let share = response_json(share).await;
    let code = share["code"].as_str().expect("code");

    let resolved = app
        .clone()
        .oneshot(authed_get(
            &format!("/api/share-links/{code}"),
            "artist",
            "artists",
        ))
        .await
        .expect("resolve folder");
    assert_eq!(resolved.status(), StatusCode::OK);
    let resolved = response_json(resolved).await;
    assert_eq!(resolved["folder_item"]["size_bytes"], 2);

    sqlx::query("UPDATE folders SET parent_id = ? WHERE id = ?")
        .bind(secret.id)
        .bind(project.id)
        .execute(&pool)
        .await
        .expect("move folder");

    let hidden_after_move = app
        .oneshot(authed_get(
            &format!("/api/share-links/{code}"),
            "artist",
            "artists",
        ))
        .await
        .expect("hidden after move");
    assert_eq!(hidden_after_move.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleted_share_targets_cascade_links() {
    let (state, _temp_dir) = test_state(None).await;
    let folder = get_or_create_folder_path(&state.db, Some("Temp"))
        .await
        .expect("temp");
    let document_id =
        insert_versioned_document(&state.db, folder.id, "delete-me.txt", b"delete me").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let doc_share = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/share-links",
            &json!({"target_type": "document", "document_id": document_id}),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("document share");
    assert_eq!(doc_share.status(), StatusCode::OK);
    let doc_share = response_json(doc_share).await;
    let folder_share = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/share-links",
            &json!({"target_type": "folder", "path": "Temp"}),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("folder share");
    assert_eq!(folder_share.status(), StatusCode::OK);
    let folder_share = response_json(folder_share).await;

    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(document_id)
        .execute(&pool)
        .await
        .expect("delete document");
    sqlx::query("DELETE FROM folders WHERE id = ?")
        .bind(folder.id)
        .execute(&pool)
        .await
        .expect("delete folder");

    let doc_resolve = app
        .clone()
        .oneshot(authed_get(
            &format!(
                "/api/share-links/{}",
                doc_share["code"].as_str().expect("code")
            ),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("resolve deleted document");
    assert_eq!(doc_resolve.status(), StatusCode::NOT_FOUND);
    let folder_resolve = app
        .oneshot(authed_get(
            &format!(
                "/api/share-links/{}",
                folder_share["code"].as_str().expect("code")
            ),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("resolve deleted folder");
    assert_eq!(folder_resolve.status(), StatusCode::NOT_FOUND);
    let link_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM share_links")
        .fetch_one(&pool)
        .await
        .expect("share link count");
    assert_eq!(link_count, 0);
}
