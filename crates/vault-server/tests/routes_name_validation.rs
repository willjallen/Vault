use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{
    VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path, get_root_folder,
};
use vault_server::http::{self, AppState};
use vault_server::storage::{LocalBlobStorage, SharedBlobStorage};

async fn test_state() -> (AppState, tempfile::TempDir) {
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
    let state = AppState::new(config, AuthSettings::default(), db, Arc::new(storage));
    (state, temp_dir)
}

async fn grant_writer_root(pool: &sqlx::SqlitePool) {
    let writers = sqlx::query("INSERT INTO vault_groups (name) VALUES ('writers')")
        .execute(pool)
        .await
        .expect("writer group")
        .last_insert_rowid();
    let root = get_root_folder(pool, VAULT_ROOT_KEY).await.expect("root");
    add_folder_permission(pool, root.id, writers, true, true, true)
        .await
        .expect("writer root");
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

fn authed_request(method: Method, uri: &str, body: Body, content_type: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Remote-User", "writer")
        .header("Remote-Name", "Writer")
        .header("Remote-Email", "writer@example.com")
        .header("Remote-Groups", "writers")
        .header("Content-Type", content_type)
        .body(body)
        .expect("request")
}

fn authed_get(uri: &str) -> Request<Body> {
    authed_request(Method::GET, uri, Body::empty(), "application/octet-stream")
}

fn authed_json_post(uri: &str, payload: &Value) -> Request<Body> {
    authed_request(
        Method::POST,
        uri,
        Body::from(payload.to_string()),
        "application/json",
    )
}

fn authed_form_post(uri: &str, body: &str) -> Request<Body> {
    authed_request(
        Method::POST,
        uri,
        Body::from(body.to_string()),
        "application/x-www-form-urlencoded",
    )
}

async fn insert_downloadable_document(
    pool: &sqlx::SqlitePool,
    storage: &SharedBlobStorage,
    name: &str,
    mime_type: &str,
) -> i64 {
    let project = get_or_create_folder_path(pool, Some("Project"))
        .await
        .expect("project");
    let content = format!("data:{name}");
    let stored = storage
        .put_bytes(content.as_bytes())
        .await
        .expect("stored blob");
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES (?, ?, ?)
        ",
    )
    .bind(&stored.hash_algo)
    .bind(&stored.digest)
    .bind(i64::try_from(stored.size_bytes).expect("blob size"))
    .execute(pool)
    .await
    .expect("blob")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO blob_locations (blob_id, backend, bucket, object_key)
        VALUES (?, ?, ?, ?)
        ",
    )
    .bind(blob_id)
    .bind(&stored.backend)
    .bind(&stored.bucket)
    .bind(&stored.object_key)
    .execute(pool)
    .await
    .expect("blob location");
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, ?, 'admin', 'Admin', 'admin')
        ",
    )
    .bind(project.id)
    .bind(name)
    .execute(pool)
    .await
    .expect("document")
    .last_insert_rowid();
    let version_id = format!("version-{document_id}");
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
            (?, ?, ?, 1, 'admin', 'Admin', 'Uploaded file', ?, ?, 'upload')
        ",
    )
    .bind(&version_id)
    .bind(document_id)
    .bind(blob_id)
    .bind(mime_type)
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

async fn created_upload_session_id(app: Router, filename: &str, mime_type: &str) -> String {
    let response = app
        .oneshot(authed_json_post(
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": filename,
                "mime_type": mime_type,
                "size_bytes": 4
            }),
        ))
        .await
        .expect("upload session response");
    assert_eq!(response.status(), StatusCode::OK);
    response_json(response).await["id"]
        .as_str()
        .expect("upload session id")
        .to_string()
}

async fn stored_upload_mime(pool: &sqlx::SqlitePool, session_id: &str) -> String {
    sqlx::query_scalar::<_, String>("SELECT mime_type FROM upload_sessions WHERE id = ?")
        .bind(session_id)
        .fetch_one(pool)
        .await
        .expect("stored upload mime")
}

#[tokio::test]
async fn folder_creation_rejects_control_characters_and_archive_paths() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);

    let control = app
        .clone()
        .oneshot(authed_form_post("/folders", "folder=safe%2Fbad%0Afolder"))
        .await
        .expect("control response");
    let control_status = control.status();
    let control_json = response_json(control).await;
    assert_eq!(control_status, StatusCode::BAD_REQUEST);
    assert_eq!(control_json["detail"], "Invalid folder path");

    let archive = app
        .oneshot(authed_form_post("/folders", "folder=Archive%2FProject"))
        .await
        .expect("archive response");
    let archive_status = archive.status();
    let archive_json = response_json(archive).await;
    assert_eq!(archive_status, StatusCode::BAD_REQUEST);
    assert_eq!(archive_json["detail"], "Create folders in Vault");
}

#[tokio::test]
async fn upload_session_rejects_control_character_file_names_and_archive_paths() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);

    let bad_name = app
        .clone()
        .oneshot(authed_json_post(
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "Project",
                "filename": "bad\nname.txt",
                "mime_type": "text/plain",
                "size_bytes": 4
            }),
        ))
        .await
        .expect("bad name response");
    let bad_name_status = bad_name.status();
    let bad_name_json = response_json(bad_name).await;
    assert_eq!(bad_name_status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_name_json["detail"], "Invalid file name");

    let archive = app
        .oneshot(authed_json_post(
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "Archive/manual",
                "filename": "report.txt",
                "mime_type": "text/plain",
                "size_bytes": 4
            }),
        ))
        .await
        .expect("archive upload response");
    let archive_status = archive.status();
    let archive_json = response_json(archive).await;
    assert_eq!(archive_status, StatusCode::BAD_REQUEST);
    assert_eq!(archive_json["detail"], "Upload new documents to Vault");
}

#[tokio::test]
async fn upload_session_sanitizes_non_ascii_mime_types() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);

    let bad_mime_id = created_upload_session_id(app.clone(), "file.bin", "text/😀").await;
    let good_mime_id =
        created_upload_session_id(app.clone(), "file.txt", "text/plain; charset=utf-8").await;
    let markdown_mime_id = created_upload_session_id(app.clone(), "readme.md", "text/😀").await;
    let log_mime_id = created_upload_session_id(app, "server.log", "text/😀").await;

    assert_eq!(
        stored_upload_mime(&pool, &bad_mime_id).await,
        "application/octet-stream"
    );
    assert_eq!(
        stored_upload_mime(&pool, &good_mime_id).await,
        "text/plain; charset=utf-8",
    );
    assert_eq!(
        stored_upload_mime(&pool, &markdown_mime_id).await,
        "text/markdown"
    );
    assert_eq!(
        stored_upload_mime(&pool, &log_mime_id).await,
        "application/octet-stream"
    );
}

#[tokio::test]
async fn downloads_sanitize_legacy_filenames_and_malformed_mime_types() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let bad_name_id =
        insert_downloadable_document(&state.db, &state.storage, "bad\nname.txt", "text/plain")
            .await;
    let unicode_name_id =
        insert_downloadable_document(&state.db, &state.storage, "計画😀.txt", "text/plain").await;
    let bad_mime_id = insert_downloadable_document(
        &state.db,
        &state.storage,
        "report.txt",
        "text/plain\nX-Bad: y",
    )
    .await;
    let markdown_mime_id =
        insert_downloadable_document(&state.db, &state.storage, "readme.md", "text/😀").await;
    let log_mime_id =
        insert_downloadable_document(&state.db, &state.storage, "server.log", "text/😀").await;
    let app = http::router(state);

    let bad_name = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{bad_name_id}/download")))
        .await
        .expect("bad name download");
    assert_eq!(bad_name.status(), StatusCode::OK);
    let disposition = bad_name.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .expect("content disposition");
    assert!(!disposition.contains('\n'));
    assert!(disposition.contains("filename=\"bad_name.txt\""));

    let unicode_name = app
        .clone()
        .oneshot(authed_get(&format!(
            "/documents/{unicode_name_id}/download"
        )))
        .await
        .expect("unicode download");
    assert_eq!(unicode_name.status(), StatusCode::OK);
    let disposition = unicode_name.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .expect("content disposition");
    assert!(disposition.contains("filename=\"___.txt\""));
    assert!(disposition.contains("filename*=UTF-8''%E8%A8%88%E7%94%BB%F0%9F%98%80.txt"));

    let bad_mime = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{bad_mime_id}/download")))
        .await
        .expect("bad mime download");
    assert_eq!(bad_mime.status(), StatusCode::OK);
    let content_type = bad_mime.headers()[header::CONTENT_TYPE]
        .to_str()
        .expect("content type");
    assert!(!content_type.contains('\n'));
    assert_eq!(content_type, "text/plain; charset=utf-8");

    let markdown_mime = app
        .clone()
        .oneshot(authed_get(&format!(
            "/documents/{markdown_mime_id}/download"
        )))
        .await
        .expect("markdown fallback download");
    assert_eq!(markdown_mime.status(), StatusCode::OK);
    assert_eq!(
        markdown_mime.headers()[header::CONTENT_TYPE]
            .to_str()
            .expect("markdown content type"),
        "text/markdown; charset=utf-8",
    );

    let log_mime = app
        .oneshot(authed_get(&format!("/documents/{log_mime_id}/download")))
        .await
        .expect("log fallback download");
    assert_eq!(log_mime.status(), StatusCode::OK);
    assert_eq!(
        log_mime.headers()[header::CONTENT_TYPE]
            .to_str()
            .expect("log content type"),
        "application/octet-stream",
    );
}
