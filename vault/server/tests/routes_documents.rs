use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::documents::sweep_expired_documents;
use vault_server::folders::{
    ARCHIVE_ROOT_KEY, VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path,
    get_root_folder,
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
    let state = AppState::new(config, AuthSettings::default(), db, Arc::new(storage));
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

async fn insert_versioned_document(pool: &sqlx::SqlitePool, folder_id: i64) -> i64 {
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', '2bb80d537b1da3e38bd30361aa855686bde0ba5b6d9dc2675a3fb1d8c1b41ef6', 6)
        ",
    )
    .execute(pool)
    .await
    .expect("blob")
    .last_insert_rowid();
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, 'plan.txt', 'admin', 'Admin', 'admin')
        ",
    )
    .bind(folder_id)
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
                created_via,
                committed_at
            )
        VALUES
            ('version-one', ?, ?, 1, 'admin', 'Admin', 'Uploaded plan.txt', 'text/plain', 'plan.txt', 'upload', '2026-06-26 19:03:00')
        ",
    )
    .bind(document_id)
    .bind(blob_id)
    .execute(pool)
    .await
    .expect("version");
    sqlx::query(
        r"
        UPDATE documents
        SET current_version_id = 'version-one',
            latest_version_number = 1,
            version_count = 1
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(pool)
    .await
    .expect("current version");
    document_id
}

async fn insert_named_versioned_document(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    version_id: &str,
) -> i64 {
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', ?, 6)
        ",
    )
    .bind(format!("{version_id:0<64}"))
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
    .bind(version_id)
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

async fn insert_stored_versioned_document(
    pool: &sqlx::SqlitePool,
    storage: &SharedBlobStorage,
    folder_id: i64,
    content: &[u8],
    original_filename: &str,
) -> i64 {
    let stored = storage.put_bytes(content).await.expect("stored blob");
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
            (?, 'plan.txt', 'admin', 'Admin', 'admin')
        ",
    )
    .bind(folder_id)
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
            ('stored-version-one', ?, ?, 1, 'admin', 'Admin', 'Uploaded file', 'text/plain', ?, 'upload')
        ",
    )
    .bind(document_id)
    .bind(blob_id)
    .bind(original_filename)
    .execute(pool)
    .await
    .expect("version");
    sqlx::query(
        r"
        UPDATE documents
        SET current_version_id = 'stored-version-one',
            latest_version_number = 1,
            version_count = 1
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(pool)
    .await
    .expect("current version");
    document_id
}

async fn insert_document_event_at(
    pool: &sqlx::SqlitePool,
    document_id: i64,
    event_type: &str,
    created_at: &str,
    actor_name: &str,
    message: &str,
) {
    sqlx::query(
        r"
        INSERT INTO document_events
            (document_id, event_type, created_at, actor, actor_name, message)
        VALUES
            (?, ?, ?, 'admin', ?, ?)
        ",
    )
    .bind(document_id)
    .bind(event_type)
    .bind(created_at)
    .bind(actor_name)
    .bind(message)
    .execute(pool)
    .await
    .expect("history event");
}

async fn mark_document_archived(
    pool: &sqlx::SqlitePool,
    document_id: i64,
    archive_folder_id: i64,
    archived_access: &Value,
) {
    sqlx::query(
        r"
        UPDATE documents
        SET
            folder_id = ?,
            archived_from_folder = 'Project',
            archived_original_name = name,
            archived_access = ?
        WHERE id = ?
        ",
    )
    .bind(archive_folder_id)
    .bind(archived_access.to_string())
    .bind(document_id)
    .execute(pool)
    .await
    .expect("archive document");
}

async fn allow_non_admin_archive_delete(pool: &sqlx::SqlitePool) {
    sqlx::query(
        r"
        INSERT INTO vault_settings (key, value)
        VALUES ('archivePermanentDeleteAdminOnly', 'false')
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        ",
    )
    .execute(pool)
    .await
    .expect("relax archive delete policy");
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

async fn assert_inconsistent_current_version_routes(app: &Router, document_id: i64) {
    let detail = app
        .clone()
        .oneshot(authed_get(
            &format!("/api/documents/{document_id}/detail"),
            "reader",
            "readers",
        ))
        .await
        .expect("detail response");
    let detail_status = detail.status();
    let detail_json = response_json(detail).await;
    assert_eq!(detail_status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        detail_json["detail"],
        "Current document version metadata is inconsistent",
    );

    let download = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents/{document_id}/download"),
            "reader",
            "readers",
        ))
        .await
        .expect("download response");
    let download_status = download.status();
    let download_json = response_json(download).await;
    assert_eq!(download_status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        download_json["detail"],
        "Current document version metadata is inconsistent",
    );
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

fn authed_get_with_header(
    uri: &str,
    user: &str,
    groups: &str,
    name: &str,
    value: &str,
) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", user)
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups)
        .header(name, value)
        .body(Body::empty())
        .expect("request")
}

fn authed_json_post(uri: &str, user: &str, groups: &str, payload: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", user)
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups)
        .header("Content-Type", "application/json")
        .header("X-Forwarded-For", "203.0.113.9")
        .header("User-Agent", "vault-test")
        .body(Body::from(
            serde_json::to_vec(payload).expect("json payload"),
        ))
        .expect("request")
}

async fn assert_active_writer_lock(pool: &sqlx::SqlitePool, document_id: i64) {
    let lock_row = sqlx::query_as::<_, (String, String, String, String)>(
        r"
        SELECT locked_by, locked_by_name, locked_ip, locked_user_agent
        FROM document_locks
        WHERE document_id = ? AND is_active = 1
        ",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("lock row");
    assert_eq!(
        lock_row,
        (
            "1".to_string(),
            "writer".to_string(),
            "203.0.113.9".to_string(),
            "vault-test".to_string(),
        ),
    );
}

async fn assert_lock_event(pool: &sqlx::SqlitePool, document_id: i64) {
    let lock_event = sqlx::query_as::<_, (String, String, String, String, String)>(
        r"
        SELECT event_type, actor, message, ip, user_agent
        FROM document_events
        WHERE document_id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("lock event");
    assert_eq!(lock_event.0, "lock");
    assert_eq!(lock_event.1, "1");
    assert_eq!(lock_event.2, "Locked Project/plan.txt");
    assert_eq!(lock_event.3, "203.0.113.9");
    assert_eq!(lock_event.4, "vault-test");
}

async fn assert_unlock_released_lock_and_emitted_state(pool: &sqlx::SqlitePool) {
    let active_locks =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_locks WHERE is_active = 1")
            .fetch_one(pool)
            .await
            .expect("active locks");
    let event_types =
        sqlx::query_scalar::<_, String>("SELECT event_type FROM document_events ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("event types");
    let state_events = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, resources FROM state_events ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("state events");

    assert_eq!(active_locks, 0);
    assert_eq!(event_types, vec!["lock".to_string(), "release".to_string()]);
    assert_eq!(state_events[0].0, "batch.lock");
    assert_eq!(state_events[1].0, "batch.unlock");
    assert_eq!(
        serde_json::from_str::<Value>(&state_events[0].1).expect("resources json"),
        json!([
            "contents",
            "document_detail",
            "my_edits",
            "preferences",
            "sidebar"
        ]),
    );
}

fn assert_document_detail_history_payload(reader_json: &Value, document_id: i64) {
    assert_eq!(reader_json["id"], document_id);
    assert_eq!(reader_json["path"], "Project/plan.txt");
    assert_eq!(reader_json["access"]["read"], true);
    assert_eq!(reader_json["latest_by"], "admin");
    assert_eq!(reader_json["modified_at"], "2026-06-26T19:03:00+00:00",);
    assert_eq!(reader_json["modified_display"], "Jun 26, 2026 at 7:03 pm");
    assert_eq!(
        reader_json["versions"].as_array().expect("versions").len(),
        3,
    );
    assert_eq!(reader_json["versions"][0]["id"], "event-1");
    assert_eq!(reader_json["versions"][0]["type"], "note");
    assert_eq!(
        reader_json["versions"][0]["timestamp"],
        "2026-06-26T19:04:00.123456",
    );
    assert_eq!(reader_json["versions"][0]["display"], "Jun 26, 2026 19:04");
    assert_eq!(reader_json["versions"][0]["by"], "admin");
    assert_eq!(reader_json["versions"][0]["note"], "Reviewed plan");
    assert_eq!(reader_json["versions"][1]["id"], "version-one");
    assert_eq!(reader_json["versions"][1]["type"], "version");
    assert_eq!(
        reader_json["versions"][1]["timestamp"],
        "2026-06-26T19:03:00",
    );
    assert_eq!(reader_json["versions"][1]["display"], "Jun 26, 2026 19:03");
    assert_eq!(reader_json["versions"][1]["by"], "admin");
    assert_eq!(reader_json["versions"][1]["size_bytes"], 6);
    assert_eq!(
        reader_json["versions"][1]["download_url"],
        format!("/documents/{document_id}/versions/version-one/download"),
    );
    assert_eq!(reader_json["versions"][2]["id"], "event-2");
    assert_eq!(reader_json["versions"][2]["type"], "document.download");
    assert_eq!(reader_json["versions"][2]["timestamp"], Value::Null);
    assert_eq!(reader_json["versions"][2]["display"], "Document.Download");
    assert_eq!(reader_json["versions"][2]["by"], "admin");
    assert_eq!(reader_json["versions"][2]["note"], "Legacy event");
}

#[tokio::test]
async fn document_detail_requires_read_access_and_returns_version_history() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let viewers = create_group(&state.db, "viewers").await;
    let outsiders = create_group(&state.db, "outsiders").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    add_folder_permission(&state.db, root.id, viewers, true, true, false)
        .await
        .expect("viewer root");
    add_folder_permission(&state.db, root.id, outsiders, true, true, false)
        .await
        .expect("outsider root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, readers, true, true, false)
        .await
        .expect("reader project");
    add_folder_permission(&state.db, project.id, viewers, true, false, false)
        .await
        .expect("viewer project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    sqlx::query("UPDATE document_versions SET committed_by_name = '' WHERE id = 'version-one'")
        .execute(&state.db)
        .await
        .expect("blank version actor display name");
    insert_document_event_at(
        &state.db,
        document_id,
        "note",
        "2026-06-26 19:04:00.123456",
        "",
        "Reviewed plan",
    )
    .await;
    insert_document_event_at(
        &state.db,
        document_id,
        "document.download",
        "",
        "",
        "Legacy event",
    )
    .await;
    let app = http::router(state);

    let reader_response = app
        .clone()
        .oneshot(authed_get(
            &format!("/api/documents/{document_id}/detail"),
            "reader",
            "readers",
        ))
        .await
        .expect("reader response");
    let reader_status = reader_response.status();
    let reader_json = response_json(reader_response).await;
    let viewer_response = app
        .clone()
        .oneshot(authed_get(
            &format!("/api/documents/{document_id}/detail"),
            "viewer",
            "viewers",
        ))
        .await
        .expect("viewer response");
    let viewer_status = viewer_response.status();
    let viewer_json = response_json(viewer_response).await;
    let outsider_response = app
        .oneshot(authed_get(
            &format!("/api/documents/{document_id}/detail"),
            "outsider",
            "outsiders",
        ))
        .await
        .expect("outsider response");
    let outsider_status = outsider_response.status();
    let outsider_json = response_json(outsider_response).await;

    assert_eq!(reader_status, StatusCode::OK);
    assert_document_detail_history_payload(&reader_json, document_id);
    assert_eq!(viewer_status, StatusCode::FORBIDDEN);
    assert_eq!(viewer_json["detail"], "Insufficient document access");
    assert_eq!(outsider_status, StatusCode::NOT_FOUND);
    assert_eq!(outsider_json["detail"], "Document not found");
}

#[tokio::test]
async fn document_detail_dedupes_matching_version_events_by_normalized_timestamp() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, readers, true, true, false)
        .await
        .expect("reader project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    sqlx::query(
        r"
        INSERT INTO document_events
            (document_id, event_type, created_at, actor, actor_name, message)
        VALUES
            (?, 'upload', '2026-06-26T19:03:00+00:00', 'admin', 'Admin', ' Uploaded plan.txt ')
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("matching upload event");
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(
            &format!("/api/documents/{document_id}/detail"),
            "reader",
            "readers",
        ))
        .await
        .expect("document detail");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    let versions = json["versions"].as_array().expect("history items");
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["id"], "version-one");
    assert_eq!(versions[0]["type"], "version");
}

#[tokio::test]
async fn document_detail_dedupes_version_checksums_after_version_number_ordering() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, readers, true, true, false)
        .await
        .expect("reader project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    let first_blob_id = sqlx::query_scalar::<_, i64>(
        "SELECT blob_id FROM document_versions WHERE id = 'version-one'",
    )
    .fetch_one(&state.db)
    .await
    .expect("first blob id");
    let second_blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 7)
        ",
    )
    .execute(&state.db)
    .await
    .expect("second blob")
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
                created_via,
                committed_at
            )
        VALUES
            ('version-two', ?, ?, 2, 'admin', 'Admin', 'Uploaded second', 'text/plain', 'plan.txt', 'upload', '2026-06-26 19:05:00'),
            ('version-three', ?, ?, 3, 'admin', 'Admin', 'Uploaded duplicate', 'text/plain', 'plan.txt', 'upload', '2026-06-26 19:04:00')
        ",
    )
    .bind(document_id)
    .bind(second_blob_id)
    .bind(document_id)
    .bind(first_blob_id)
    .execute(&state.db)
    .await
    .expect("additional versions");
    sqlx::query(
        r"
        UPDATE documents
        SET current_version_id = 'version-two',
            latest_version_number = 3,
            version_count = 3
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("document version metadata");
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(
            &format!("/api/documents/{document_id}/detail"),
            "reader",
            "readers",
        ))
        .await
        .expect("document detail");
    let status = response.status();
    let json = response_json(response).await;
    let versions = json["versions"].as_array().expect("history items");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        versions
            .iter()
            .map(|item| item["id"].as_str().expect("version id"))
            .collect::<Vec<_>>(),
        vec!["version-two", "version-three", "version-one"],
    );
}

#[tokio::test]
async fn document_detail_lock_payload_uses_python_datetime_iso_shape() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, readers, true, true, false)
        .await
        .expect("reader project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    sqlx::query(
        r"
        INSERT INTO document_locks
            (
                document_id,
                locked_by,
                locked_by_name,
                locked_at,
                locked_ip,
                locked_user_agent
            )
        VALUES
            (?, 'editor', 'Editor', '2026-06-26 19:03:00.123456', '203.0.113.7', 'vault-test')
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("active lock");
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(
            &format!("/api/documents/{document_id}/detail"),
            "reader",
            "readers",
        ))
        .await
        .expect("document detail");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["lock"]["by"], "editor");
    assert_eq!(json["lock"]["name"], "Editor");
    assert_eq!(json["lock"]["at"], "2026-06-26T19:03:00.123456");
    assert_eq!(json["lock"]["ip"], "203.0.113.7");
    assert_eq!(json["lock"]["user_agent"], "vault-test");
    assert_eq!(json["lock"]["force_acquired"], false);
}

#[tokio::test]
async fn legacy_document_detail_redirect_requires_visible_access() {
    let (state, _temp_dir) = test_state().await;
    let viewers = create_group(&state.db, "viewers").await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, viewers, true, false, false)
        .await
        .expect("viewer project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    let app = http::router(state);

    let visible_response = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents/{document_id}"),
            "viewer",
            "viewers",
        ))
        .await
        .expect("visible response");
    let hidden_response = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents/{document_id}"),
            "outsider",
            "outsiders",
        ))
        .await
        .expect("hidden response");
    let missing_response = app
        .oneshot(authed_get("/documents/999999", "viewer", "viewers"))
        .await
        .expect("missing response");

    assert_eq!(visible_response.status(), StatusCode::SEE_OTHER);
    assert_eq!(visible_response.headers()["location"], "/");
    assert_eq!(hidden_response.status(), StatusCode::NOT_FOUND);
    assert_eq!(missing_response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn current_version_routes_reject_inconsistent_metadata() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let missing_pointer = insert_named_versioned_document(
        &state.db,
        project.id,
        "missing-pointer.txt",
        "missing-pointer-version",
    )
    .await;
    let empty_pointer = insert_named_versioned_document(
        &state.db,
        project.id,
        "empty-pointer.txt",
        "empty-pointer-version",
    )
    .await;
    let empty_string_pointer = insert_named_versioned_document(
        &state.db,
        project.id,
        "empty-string-pointer.txt",
        "empty-string-pointer-version",
    )
    .await;
    let unversioned_empty_string = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, current_version_id, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, 'unversioned-empty-string.txt', '', 'admin', 'Admin', 'admin')
        ",
    )
    .bind(project.id)
    .execute(&state.db)
    .await
    .expect("unversioned empty string pointer")
    .last_insert_rowid();
    sqlx::query("UPDATE documents SET current_version_id = 'missing-version' WHERE id = ?")
        .bind(missing_pointer)
        .execute(&state.db)
        .await
        .expect("missing pointer");
    sqlx::query("UPDATE documents SET current_version_id = NULL WHERE id = ?")
        .bind(empty_pointer)
        .execute(&state.db)
        .await
        .expect("empty pointer");
    sqlx::query("UPDATE documents SET current_version_id = '' WHERE id = ?")
        .bind(empty_string_pointer)
        .execute(&state.db)
        .await
        .expect("empty string pointer");
    let app = http::router(state);

    for document_id in [missing_pointer, empty_pointer, empty_string_pointer] {
        assert_inconsistent_current_version_routes(&app, document_id).await;
    }

    let unversioned_download = app
        .oneshot(authed_get(
            &format!("/documents/{unversioned_empty_string}/download"),
            "reader",
            "readers",
        ))
        .await
        .expect("unversioned download response");
    let unversioned_status = unversioned_download.status();
    let unversioned_json = response_json(unversioned_download).await;
    assert_eq!(unversioned_status, StatusCode::NOT_FOUND);
    assert_eq!(unversioned_json["detail"], "Document has no versions");
}

#[tokio::test]
async fn legacy_create_document_returns_gone_after_authentication() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/documents",
            "writer",
            "writers",
            &json!({}),
        ))
        .await
        .expect("legacy create response");
    let status = response.status();
    let body = response_json(response).await;

    assert_eq!(status, StatusCode::GONE);
    assert_eq!(body["detail"], "Use resumable upload sessions");
}

#[tokio::test]
async fn legacy_create_document_returns_gone_for_authenticated_read_only_user() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    add_folder_permission(&state.db, project.id, readers, true, true, false)
        .await
        .expect("reader project");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/documents",
            "reader",
            "readers",
            &json!({"folder": "Project"}),
        ))
        .await
        .expect("legacy create response");
    let status = response.status();
    let body = response_json(response).await;

    assert_eq!(status, StatusCode::GONE);
    assert_eq!(body["detail"], "Use resumable upload sessions");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents")
            .fetch_one(&pool)
            .await
            .expect("documents"),
        0,
    );
}

#[tokio::test]
async fn legacy_checkin_document_requires_authentication_before_gone_response() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let unauthenticated = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/documents/123/checkin")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("unauthenticated checkin response");
    let unauthenticated_status = unauthenticated.status();
    let unauthenticated_body = response_json(unauthenticated).await;

    let authenticated = app
        .oneshot(authed_json_post(
            "/documents/123/checkin",
            "reader",
            "readers",
            &json!({}),
        ))
        .await
        .expect("authenticated checkin response");
    let authenticated_status = authenticated.status();
    let authenticated_body = response_json(authenticated).await;

    assert_eq!(unauthenticated_status, StatusCode::UNAUTHORIZED);
    assert_eq!(unauthenticated_body["detail"], "Authentication required",);
    assert_eq!(authenticated_status, StatusCode::GONE);
    assert_eq!(
        authenticated_body["detail"],
        "Use resumable upload sessions",
    );
}

#[tokio::test]
async fn current_document_download_streams_range_headers_and_records_event() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_stored_versioned_document(
        &state.db,
        &state.storage,
        project.id,
        b"hello world",
        "download-name.txt",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_get_with_header(
            &format!("/documents/{document_id}/download"),
            "reader",
            "readers",
            "Range",
            "bytes=1-4",
        ))
        .await
        .expect("download response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&body[..], b"ello");
    assert_eq!(headers["content-length"], "4");
    assert_eq!(headers["content-range"], "bytes 1-4/11");
    assert_eq!(headers["accept-ranges"], "bytes");
    assert_eq!(headers["content-encoding"], "identity");
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert!(
        headers["content-disposition"]
            .to_str()
            .expect("content disposition")
            .contains("filename=\"download-name.txt\""),
    );

    let event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, message FROM document_events WHERE document_id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("download event");
    let state_event = sqlx::query_scalar::<_, String>(
        "SELECT event_type FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");
    assert_eq!(
        event,
        (
            "download".to_string(),
            "Downloaded Project/plan.txt".to_string()
        )
    );
    assert_eq!(state_event, "document.download");
}

#[tokio::test]
async fn api_download_single_document_streams_current_version_and_records_event() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_stored_versioned_document(
        &state.db,
        &state.storage,
        project.id,
        b"download via api",
        "api-download.txt",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/download",
            "reader",
            "readers",
            &json!({
                "items": [
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("download response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"download via api");
    assert_eq!(headers["content-length"], "16");
    assert!(
        headers["content-disposition"]
            .to_str()
            .expect("content disposition")
            .contains("filename=\"api-download.txt\""),
    );

    let event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, message FROM document_events WHERE document_id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("download event");
    let state_event = sqlx::query_scalar::<_, String>(
        "SELECT event_type FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");
    assert_eq!(
        event,
        (
            "download".to_string(),
            "Downloaded Project/plan.txt".to_string(),
        ),
    );
    assert_eq!(state_event, "document.download");
}

#[tokio::test]
async fn api_download_missing_folder_path_reports_normalized_python_detail() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/download",
            "admin",
            "vault-admin",
            &json!({
                "items": [
                    {"type": "folder", "path": " /Missing\\Folder/ "}
                ]
            }),
        ))
        .await
        .expect("download response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["detail"], "Folder not found: Missing/Folder");
}

#[tokio::test]
async fn api_download_rejects_visible_only_document_access_without_recording_events() {
    let (state, _temp_dir) = test_state().await;
    let viewers = create_group(&state.db, "viewers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, viewers, true, true, false)
        .await
        .expect("viewer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, viewers, true, false, false)
        .await
        .expect("viewer project");
    let document_id = insert_stored_versioned_document(
        &state.db,
        &state.storage,
        project.id,
        b"visible only",
        "private.txt",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/download",
            "viewer",
            "viewers",
            &json!({
                "items": [
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("download response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["detail"], "Insufficient document access");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_events")
            .fetch_one(&pool)
            .await
            .expect("document events"),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
            .fetch_one(&pool)
            .await
            .expect("state events"),
        0,
    );
}

#[tokio::test]
async fn explicit_version_download_uses_original_filename_and_records_version_event() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_stored_versioned_document(
        &state.db,
        &state.storage,
        project.id,
        b"version bytes",
        "original.txt",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(
            &format!("/documents/{document_id}/versions/stored-version-one/download"),
            "reader",
            "readers",
        ))
        .await
        .expect("version download");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"version bytes");
    assert_eq!(headers["content-length"], "13");
    assert!(
        headers["content-disposition"]
            .to_str()
            .expect("content disposition")
            .contains("filename=\"original.txt\""),
    );

    let event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, message FROM document_events WHERE document_id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("download event");
    assert_eq!(
        event,
        (
            "download".to_string(),
            "Downloaded version v1 of Project/plan.txt".to_string(),
        ),
    );
}

#[tokio::test]
async fn download_routes_recheck_current_access_after_folder_acl_move() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let confidential = create_group(&state.db, "confidential").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let secret = get_or_create_folder_path(&state.db, Some("Secret"))
        .await
        .expect("secret");
    add_folder_permission(&state.db, secret.id, confidential, true, true, true)
        .await
        .expect("confidential secret");
    let document_id = insert_stored_versioned_document(
        &state.db,
        &state.storage,
        project.id,
        b"secret",
        "plan.txt",
    )
    .await;
    let app = http::router(state.clone());

    let visible = app
        .clone()
        .oneshot(authed_json_post(
            "/api/download",
            "reader",
            "readers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("visible download");
    assert_eq!(visible.status(), StatusCode::OK);

    sqlx::query("UPDATE documents SET folder_id = ? WHERE id = ?")
        .bind(secret.id)
        .bind(document_id)
        .execute(&state.db)
        .await
        .expect("move document under hidden ACL");

    let hidden_api_download = app
        .clone()
        .oneshot(authed_json_post(
            "/api/download",
            "reader",
            "readers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("hidden api download");
    let hidden_api_status = hidden_api_download.status();
    let hidden_api_json = response_json(hidden_api_download).await;
    assert_eq!(hidden_api_status, StatusCode::NOT_FOUND);
    assert_eq!(hidden_api_json["detail"], "Document not found");

    let hidden_version_download = app
        .oneshot(authed_get(
            &format!("/documents/{document_id}/versions/stored-version-one/download"),
            "reader",
            "readers",
        ))
        .await
        .expect("hidden version download");
    let hidden_version_status = hidden_version_download.status();
    let hidden_version_json = response_json(hidden_version_download).await;
    assert_eq!(hidden_version_status, StatusCode::NOT_FOUND);
    assert_eq!(hidden_version_json["detail"], "Document not found");
}

#[tokio::test]
async fn download_routes_return_not_found_for_missing_versions_and_locations() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    let app = http::router(state);

    let missing_location = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents/{document_id}/download"),
            "reader",
            "readers",
        ))
        .await
        .expect("missing location");
    let missing_location_status = missing_location.status();
    let missing_location_json = response_json(missing_location).await;
    assert_eq!(missing_location_status, StatusCode::NOT_FOUND);
    assert_eq!(
        missing_location_json["detail"],
        "Blob has no storage location",
    );

    let missing_version = app
        .oneshot(authed_get(
            &format!("/documents/{document_id}/versions/missing/download"),
            "reader",
            "readers",
        ))
        .await
        .expect("missing version");
    let missing_version_status = missing_version.status();
    let missing_version_json = response_json(missing_version).await;
    assert_eq!(missing_version_status, StatusCode::NOT_FOUND);
    assert_eq!(missing_version_json["detail"], "Version not found");
}

#[tokio::test]
async fn download_routes_reject_corrupt_stored_blob_bytes() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_stored_versioned_document(
        &state.db,
        &state.storage,
        project.id,
        b"trusted content",
        "plan.txt",
    )
    .await;
    let object_key: String = sqlx::query_scalar(
        r"
        SELECT blob_locations.object_key
        FROM document_versions
        JOIN blob_locations ON blob_locations.blob_id = document_versions.blob_id
        WHERE document_versions.document_id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&state.db)
    .await
    .expect("object key");
    let object_path = state.config.objects_path().join(object_key);
    tokio::fs::write(object_path, b"corrupt content")
        .await
        .expect("corrupt object");
    let pool = state.db.clone();
    let app = http::router(state);

    let current_response = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents/{document_id}/download"),
            "reader",
            "readers",
        ))
        .await
        .expect("current corrupt download");
    let current_status = current_response.status();
    let current_json = response_json(current_response).await;
    assert_eq!(current_status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        current_json["detail"],
        "Blob content does not match metadata",
    );

    let range_response = app
        .oneshot(authed_get_with_header(
            &format!("/documents/{document_id}/versions/stored-version-one/download"),
            "reader",
            "readers",
            "Range",
            "bytes=0-6",
        ))
        .await
        .expect("version corrupt range download");
    let range_status = range_response.status();
    let range_json = response_json(range_response).await;
    assert_eq!(range_status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(range_json["detail"], "Blob content does not match metadata",);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_events")
            .fetch_one(&pool)
            .await
            .expect("document events"),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
            .fetch_one(&pool)
            .await
            .expect("state events"),
        0,
    );
}

#[tokio::test]
async fn checkout_document_streams_current_version_locks_and_records_event() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_stored_versioned_document(
        &state.db,
        &state.storage,
        project.id,
        b"checkout bytes",
        "checkout.txt",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(
            &format!("/documents/{document_id}/checkout"),
            "writer",
            "writers",
        ))
        .await
        .expect("checkout response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"checkout bytes");
    assert_eq!(headers["content-length"], "14");
    assert!(
        headers["content-disposition"]
            .to_str()
            .expect("content disposition")
            .contains("filename=\"checkout.txt\""),
    );

    let lock = sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        r"
        SELECT locked_by, locked_by_name, locked_ip, locked_user_agent
        FROM document_locks
        WHERE document_id = ? AND is_active = 1
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("checkout lock");
    let event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, message FROM document_events WHERE document_id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("checkout event");
    let state_event = sqlx::query_scalar::<_, String>(
        "SELECT event_type FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");

    assert_eq!(lock, ("1".to_string(), "writer".to_string(), None, None,),);
    assert_eq!(
        event,
        (
            "checkout".to_string(),
            "Checked out Project/plan.txt".to_string(),
        ),
    );
    assert_eq!(state_event, "document.checkout");
}

#[tokio::test]
async fn checkout_document_rejects_archived_and_other_user_locks() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let archived_source = get_or_create_folder_path(&state.db, Some("ArchivedSource"))
        .await
        .expect("archived source");
    let locked_doc = insert_stored_versioned_document(
        &state.db,
        &state.storage,
        project.id,
        b"locked bytes",
        "locked.txt",
    )
    .await;
    let archived_doc = insert_named_versioned_document(
        &state.db,
        archived_source.id,
        "archived.txt",
        "checkout-archived",
    )
    .await;
    mark_document_archived(
        &state.db,
        archived_doc,
        archive_root.id,
        &json!({writers.to_string(): 3}),
    )
    .await;
    sqlx::query(
        r"
        INSERT INTO document_locks (document_id, locked_by, locked_by_name)
        VALUES (?, 'other', 'Other')
        ",
    )
    .bind(locked_doc)
    .execute(&state.db)
    .await
    .expect("other lock");
    let pool = state.db.clone();
    let app = http::router(state);

    let archived = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents/{archived_doc}/checkout"),
            "writer",
            "writers",
        ))
        .await
        .expect("archived checkout");
    let archived_status = archived.status();
    let archived_json = response_json(archived).await;
    assert_eq!(archived_status, StatusCode::BAD_REQUEST);
    assert_eq!(archived_json["detail"], "Restore this file before editing");

    let locked = app
        .oneshot(authed_get(
            &format!("/documents/{locked_doc}/checkout"),
            "writer",
            "writers",
        ))
        .await
        .expect("locked checkout");
    let locked_status = locked.status();
    let locked_json = response_json(locked).await;
    assert_eq!(locked_status, StatusCode::FORBIDDEN);
    assert_eq!(locked_json["detail"], "Document is locked by another user");

    let event_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_events WHERE event_type = 'checkout'",
    )
    .fetch_one(&pool)
    .await
    .expect("checkout events");
    let archived_lock_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_locks WHERE document_id = ? AND is_active = 1",
    )
    .bind(archived_doc)
    .fetch_one(&pool)
    .await
    .expect("archived active locks");
    let state_event_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("state events");
    assert_eq!(event_count, 0);
    assert_eq!(archived_lock_count, 0);
    assert_eq!(state_event_count, 0);
}

#[tokio::test]
async fn lock_unlock_routes_manage_locks_events_and_state() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    let pool = state.db.clone();
    let app = http::router(state);

    let lock = app
        .clone()
        .oneshot(authed_json_post(
            "/api/lock",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("lock response");
    let lock_status = lock.status();
    let lock_json = response_json(lock).await;

    assert_eq!(lock_status, StatusCode::OK);
    assert_eq!(
        lock_json["ok"][0]["item"],
        json!({"type": "document", "id": document_id})
    );
    assert_eq!(lock_json["ok"][0]["detail"], "writer");
    assert_eq!(lock_json["failed"], json!([]));

    assert_active_writer_lock(&pool, document_id).await;
    assert_lock_event(&pool, document_id).await;

    let unlock = app
        .oneshot(authed_json_post(
            "/api/unlock",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("unlock response");
    let unlock_status = unlock.status();
    let unlock_json = response_json(unlock).await;

    assert_eq!(unlock_status, StatusCode::OK);
    assert_eq!(unlock_json["ok"][0]["detail"], "Unlocked");

    assert_unlock_released_lock_and_emitted_state(&pool).await;
}

#[tokio::test]
async fn lock_unlock_routes_return_item_level_failures() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    let pool = state.db.clone();
    let app = http::router(state);

    let reader_lock = app
        .clone()
        .oneshot(authed_json_post(
            "/api/lock",
            "reader",
            "readers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("reader lock");
    let reader_lock_json = response_json(reader_lock).await;
    assert_eq!(reader_lock_json["ok"], json!([]));
    assert_eq!(
        reader_lock_json["failed"][0]["detail"],
        "Insufficient document access",
    );

    app.clone()
        .oneshot(authed_json_post(
            "/api/lock",
            "owner",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("owner lock");

    let other_unlock = app
        .clone()
        .oneshot(authed_json_post(
            "/api/unlock",
            "other",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("other unlock");
    let other_unlock_json = response_json(other_unlock).await;
    assert_eq!(other_unlock_json["ok"], json!([]));
    assert_eq!(
        other_unlock_json["failed"][0]["detail"],
        "Document is locked by another user",
    );

    let folder_lock = app
        .oneshot(authed_json_post(
            "/api/lock",
            "owner",
            "writers",
            &json!({"items": [{"type": "folder", "id": project.id}]}),
        ))
        .await
        .expect("folder lock");
    let folder_lock_json = response_json(folder_lock).await;
    assert_eq!(folder_lock_json["ok"], json!([]));
    assert_eq!(
        folder_lock_json["failed"][0]["detail"],
        "Only files can be locked"
    );

    let active_locks = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_locks WHERE document_id = ? AND is_active = 1",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("active locks");
    assert_eq!(active_locks, 1);
}

#[tokio::test]
async fn lock_route_prunes_child_documents_when_folder_is_selected() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/lock",
            "writer",
            "writers",
            &json!({
                "items": [
                    {"type": "folder", "id": project.id},
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("lock response");
    let payload = response_json(response).await;

    assert_eq!(payload["ok"], json!([]));
    assert_eq!(payload["failed"].as_array().expect("failed").len(), 1);
    assert_eq!(
        payload["failed"][0]["item"],
        json!({"type": "folder", "id": project.id, "path": "Project"})
    );
    assert_eq!(payload["failed"][0]["detail"], "Only files can be locked");

    let active_locks = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_locks WHERE document_id = ? AND is_active = 1",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("active locks");
    let state_events = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("state events");
    assert_eq!(active_locks, 0);
    assert_eq!(state_events, 0);
}

#[tokio::test]
async fn unlock_route_rechecks_current_folder_access_after_acl_move() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let private = get_or_create_folder_path(&state.db, Some("Private"))
        .await
        .expect("private");
    add_folder_permission(&state.db, private.id, writers, false, false, false)
        .await
        .expect("deny private");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    sqlx::query(
        r"
        INSERT INTO document_locks (document_id, locked_by, locked_by_name)
        VALUES (?, 'artist', 'Artist')
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("insert lock");
    let app = http::router(state.clone());

    sqlx::query("UPDATE documents SET folder_id = ? WHERE id = ?")
        .bind(private.id)
        .bind(document_id)
        .execute(&state.db)
        .await
        .expect("move under hidden ACL");

    let response = app
        .oneshot(authed_json_post(
            "/api/unlock",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("unlock response");
    let status = response.status();
    let json = response_json(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"], json!([]));
    assert_eq!(json["failed"][0]["detail"], "Document not found");

    let active_locks = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_locks WHERE document_id = ? AND is_active = 1",
    )
    .bind(document_id)
    .fetch_one(&state.db)
    .await
    .expect("active locks");
    let release_events = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_events WHERE document_id = ? AND event_type = 'release'",
    )
    .bind(document_id)
    .fetch_one(&state.db)
    .await
    .expect("release events");
    assert_eq!(active_locks, 1);
    assert_eq!(release_events, 0);
}

#[tokio::test]
async fn lock_routes_validate_payload_and_reject_archived_documents() {
    let (state, _temp_dir) = test_state().await;
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    let document_id = insert_versioned_document(&state.db, archive_root.id).await;
    sqlx::query(
        "UPDATE documents SET archived_from_folder = 'Project', archived_original_name = name WHERE id = ?",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("archive document");
    let pool = state.db.clone();
    let app = http::router(state);

    let empty = app
        .clone()
        .oneshot(authed_json_post(
            "/api/lock",
            "admin",
            "vault-admin",
            &json!({"items": []}),
        ))
        .await
        .expect("empty response");
    let empty_status = empty.status();
    let empty_json = response_json(empty).await;
    assert_eq!(empty_status, StatusCode::BAD_REQUEST);
    assert_eq!(empty_json["detail"], "Select at least one item");

    let archived = app
        .oneshot(authed_json_post(
            "/api/lock",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("archived response");
    let archived_status = archived.status();
    let archived_json = response_json(archived).await;
    assert_eq!(archived_status, StatusCode::OK);
    assert_eq!(archived_json["ok"], json!([]));
    assert_eq!(
        archived_json["failed"][0]["detail"],
        "Restore this file before editing",
    );
    let active_locks =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_locks WHERE is_active = 1")
            .fetch_one(&pool)
            .await
            .expect("active locks");
    let lock_events = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_events WHERE document_id = ? AND event_type = 'lock'",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("lock events");
    let state_events = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("state events");
    assert_eq!(active_locks, 0);
    assert_eq!(lock_events, 0);
    assert_eq!(state_events, 0);
}

#[tokio::test]
async fn my_edits_returns_owned_active_locks_sorted_by_path() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let alpha = get_or_create_folder_path(&state.db, Some("Alpha"))
        .await
        .expect("alpha");
    let beta = get_or_create_folder_path(&state.db, Some("Beta"))
        .await
        .expect("beta");
    let zeta = get_or_create_folder_path(&state.db, Some("Zeta"))
        .await
        .expect("zeta");
    let zeta_doc =
        insert_named_versioned_document(&state.db, zeta.id, "zeta.txt", "zeta-version").await;
    let alpha_doc =
        insert_named_versioned_document(&state.db, alpha.id, "alpha.txt", "alpha-version").await;
    let beta_doc =
        insert_named_versioned_document(&state.db, beta.id, "beta.txt", "beta-version").await;
    let app = http::router(state);

    for document_id in [zeta_doc, alpha_doc] {
        let response = app
            .clone()
            .oneshot(authed_json_post(
                "/api/lock",
                "owner",
                "writers",
                &json!({"items": [{"type": "document", "id": document_id}]}),
            ))
            .await
            .expect("owner lock");
        assert_eq!(response.status(), StatusCode::OK);
    }
    let other_lock = app
        .clone()
        .oneshot(authed_json_post(
            "/api/lock",
            "other",
            "writers",
            &json!({"items": [{"type": "document", "id": beta_doc}]}),
        ))
        .await
        .expect("other lock");
    assert_eq!(other_lock.status(), StatusCode::OK);

    let response = app
        .oneshot(authed_get("/api/my-edits", "owner", "writers"))
        .await
        .expect("my edits response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["documents"].as_array().expect("documents").len(), 2);
    assert_eq!(json["documents"][0]["id"], alpha_doc);
    assert_eq!(json["documents"][0]["path"], "Alpha/alpha.txt");
    assert_eq!(json["documents"][0]["lock"]["name"], "owner");
    assert_eq!(json["documents"][0]["access"]["write"], true);
    assert_eq!(json["documents"][1]["id"], zeta_doc);
    assert_eq!(json["documents"][1]["path"], "Zeta/zeta.txt");
}

#[tokio::test]
async fn my_edits_requires_auth_and_hides_locks_without_current_write_access() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let confidential = create_group(&state.db, "confidential").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id =
        insert_named_versioned_document(&state.db, project.id, "plan.txt", "plan-version").await;
    let app = http::router(state.clone());

    let unauthenticated = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/my-edits")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("unauthenticated response");
    let unauthenticated_status = unauthenticated.status();
    let unauthenticated_json = response_json(unauthenticated).await;
    assert_eq!(unauthenticated_status, StatusCode::UNAUTHORIZED);
    assert_eq!(unauthenticated_json["detail"], "Authentication required");

    let lock = app
        .clone()
        .oneshot(authed_json_post(
            "/api/lock",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("writer lock");
    assert_eq!(lock.status(), StatusCode::OK);

    sqlx::query("DELETE FROM folder_permissions WHERE folder_id = ?")
        .bind(project.id)
        .execute(&state.db)
        .await
        .expect("clear project permissions");
    add_folder_permission(&state.db, project.id, confidential, true, true, true)
        .await
        .expect("confidential project");

    let response = app
        .oneshot(authed_get("/api/my-edits", "writer", "writers"))
        .await
        .expect("my edits response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["documents"], json!([]));
}

#[tokio::test]
async fn delete_forever_defaults_to_admin_only_and_deletes_archived_documents() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id =
        insert_named_versioned_document(&state.db, project.id, "plan.txt", "delete-version").await;
    mark_document_archived(
        &state.db,
        document_id,
        archive_root.id,
        &json!({writers.to_string(): 3}),
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let denied = app
        .clone()
        .oneshot(authed_json_post(
            "/api/delete-forever",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("denied response");
    let denied_status = denied.status();
    let denied_json = response_json(denied).await;
    assert_eq!(denied_status, StatusCode::FORBIDDEN);
    assert_eq!(denied_json["detail"], "Admin access required");

    let deleted = app
        .oneshot(authed_json_post(
            "/api/delete-forever",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("deleted response");
    let deleted_status = deleted.status();
    let deleted_json = response_json(deleted).await;
    assert_eq!(deleted_status, StatusCode::OK);
    assert_eq!(deleted_json["failed"], json!([]));
    assert_eq!(
        deleted_json["ok"][0]["item"],
        json!({"type": "document", "id": document_id})
    );
    assert_eq!(deleted_json["ok"][0]["detail"], "Archive/plan.txt");

    let document_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document count");
    let version_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_versions WHERE document_id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("version count");
    let state_event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, resources FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");

    assert_eq!(document_count, 0);
    assert_eq!(version_count, 0);
    assert_eq!(state_event.0, "document.deleted");
    assert_eq!(
        serde_json::from_str::<Value>(&state_event.1).expect("resources json"),
        json!([
            "contents",
            "document_detail",
            "my_edits",
            "preferences",
            "sidebar"
        ]),
    );
}

#[tokio::test]
async fn delete_forever_relaxed_policy_checks_access_archive_state_and_item_type() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    add_folder_permission(&state.db, archive_root.id, readers, true, true, false)
        .await
        .expect("reader archive root");
    allow_non_admin_archive_delete(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let active_doc =
        insert_named_versioned_document(&state.db, project.id, "active.txt", "active-version")
            .await;
    let archived_doc =
        insert_named_versioned_document(&state.db, project.id, "archived.txt", "archived-version")
            .await;
    mark_document_archived(
        &state.db,
        archived_doc,
        archive_root.id,
        &json!({readers.to_string(): 2, writers.to_string(): 3}),
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let reader_denied = app
        .clone()
        .oneshot(authed_json_post(
            "/api/delete-forever",
            "reader",
            "readers",
            &json!({"items": [{"type": "document", "id": archived_doc}]}),
        ))
        .await
        .expect("reader denied");
    let reader_denied_json = response_json(reader_denied).await;
    assert_eq!(reader_denied_json["ok"], json!([]));
    assert_eq!(
        reader_denied_json["failed"][0]["detail"],
        "Insufficient document access",
    );

    let active_denied = app
        .clone()
        .oneshot(authed_json_post(
            "/api/delete-forever",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": active_doc}]}),
        ))
        .await
        .expect("active denied");
    let active_denied_json = response_json(active_denied).await;
    assert_eq!(
        active_denied_json["failed"][0]["detail"],
        "Move the document to Archive before deleting",
    );

    let folder_denied = app
        .oneshot(authed_json_post(
            "/api/delete-forever",
            "writer",
            "writers",
            &json!({"items": [{"type": "folder", "id": project.id}]}),
        ))
        .await
        .expect("folder denied");
    let folder_denied_json = response_json(folder_denied).await;
    assert_eq!(
        folder_denied_json["failed"][0]["detail"],
        "Delete forever is only available for archived files",
    );

    let document_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE id IN (?, ?)")
            .bind(active_doc)
            .bind(archived_doc)
            .fetch_one(&pool)
            .await
            .expect("document count");
    assert_eq!(document_count, 2);
}

async fn create_stored_project_document(state: &AppState) -> (i64, i64) {
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_stored_versioned_document(
        &state.db,
        &state.storage,
        project.id,
        b"restored bytes",
        "plan.txt",
    )
    .await;
    (project.id, document_id)
}

async fn archive_and_restore_document(app: &Router, document_id: i64) {
    let archive = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("archive response");
    assert_eq!(
        response_json(archive).await["ok"][0]["detail"],
        "Archive/plan.txt",
    );

    let restore = app
        .clone()
        .oneshot(authed_json_post(
            "/api/restore",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("restore response");
    assert_eq!(
        response_json(restore).await["ok"][0]["detail"],
        "Project/plan.txt",
    );
}

async fn archive_folder_and_restore_document(
    app: &Router,
    pool: &sqlx::SqlitePool,
    source_folder_id: i64,
    document_id: i64,
) -> i64 {
    let archive = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({"items": [{"type": "folder", "id": source_folder_id}]}),
        ))
        .await
        .expect("archive folder response");
    assert_eq!(response_json(archive).await["ok"][0]["detail"], "Archive");

    let source_folder_exists =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE id = ?")
            .bind(source_folder_id)
            .fetch_one(pool)
            .await
            .expect("source folder count");
    assert_eq!(source_folder_exists, 0);

    let restore = app
        .clone()
        .oneshot(authed_json_post(
            "/api/restore",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("restore response");
    assert_eq!(
        response_json(restore).await["ok"][0]["detail"],
        "Project/plan.txt",
    );
    sqlx::query_scalar::<_, i64>("SELECT id FROM folders WHERE root_key = ? AND name = 'Project'")
        .bind(VAULT_ROOT_KEY)
        .fetch_one(pool)
        .await
        .expect("restored project folder")
}

async fn assert_restored_document_storage_intact(
    pool: &sqlx::SqlitePool,
    storage: &SharedBlobStorage,
    document_id: i64,
    project_id: i64,
) {
    let document = sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
        "SELECT folder_id, archived_from_folder, archived_original_name FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("document row");
    let row_counts = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        r"
        SELECT
            (SELECT COUNT(*) FROM document_versions WHERE document_id = ?),
            (SELECT COUNT(*) FROM blobs),
            (SELECT COUNT(*) FROM blob_locations),
            (SELECT COUNT(*) FROM state_events)
        ",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("row counts");
    let events = sqlx::query_scalar::<_, String>(
        "SELECT event_type FROM document_events WHERE document_id = ? ORDER BY id",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await
    .expect("document events");
    let object_key = sqlx::query_scalar::<_, String>(
        r"
        SELECT blob_locations.object_key
        FROM blob_locations
        JOIN document_versions ON document_versions.blob_id = blob_locations.blob_id
        WHERE document_versions.document_id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("object key");
    let object_keys = storage.list_object_keys().await.expect("local object keys");
    let stored_bytes = storage.read_bytes(&object_key).await.expect("stored bytes");

    assert_eq!(document, (project_id, None, None));
    assert_eq!(row_counts, (1, 1, 1, 2));
    assert_eq!(events, vec!["archive".to_string(), "unarchive".to_string()],);
    assert_eq!(object_keys, vec![object_key]);
    assert_eq!(stored_bytes, b"restored bytes");
}

#[tokio::test]
async fn delete_forever_rejects_restored_document_without_deleting_storage() {
    let (state, _temp_dir) = test_state().await;
    let (project_id, document_id) = create_stored_project_document(&state).await;
    let pool = state.db.clone();
    let storage = state.storage.clone();
    let app = http::router(state);
    archive_and_restore_document(&app, document_id).await;

    let delete = app
        .oneshot(authed_json_post(
            "/api/delete-forever",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("delete response");
    let delete_json = response_json(delete).await;
    assert_eq!(delete_json["ok"], json!([]));
    assert_eq!(
        delete_json["failed"][0]["detail"],
        "Move the document to Archive before deleting",
    );
    assert_restored_document_storage_intact(&pool, &storage, document_id, project_id).await;
}

#[tokio::test]
async fn delete_forever_rejects_document_restored_after_folder_archive() {
    let (state, _temp_dir) = test_state().await;
    let (old_project_id, document_id) = create_stored_project_document(&state).await;
    let pool = state.db.clone();
    let storage = state.storage.clone();
    let app = http::router(state);
    let restored_project_id =
        archive_folder_and_restore_document(&app, &pool, old_project_id, document_id).await;

    let delete = app
        .oneshot(authed_json_post(
            "/api/delete-forever",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("delete response");
    let delete_json = response_json(delete).await;

    assert_eq!(delete_json["ok"], json!([]));
    assert_eq!(
        delete_json["failed"][0]["detail"],
        "Move the document to Archive before deleting",
    );
    assert_restored_document_storage_intact(&pool, &storage, document_id, restored_project_id)
        .await;
}

#[tokio::test]
async fn delete_forever_rejects_document_locked_by_other_user() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    allow_non_admin_archive_delete(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id =
        insert_named_versioned_document(&state.db, project.id, "locked.txt", "locked-version")
            .await;
    mark_document_archived(
        &state.db,
        document_id,
        archive_root.id,
        &json!({writers.to_string(): 3}),
    )
    .await;
    sqlx::query(
        r"
        INSERT INTO document_locks (document_id, locked_by, locked_by_name)
        VALUES (?, 'editor', 'Editor')
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("lock document");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/delete-forever",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("delete response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"], json!([]));
    assert_eq!(
        json["failed"][0]["detail"],
        "Document is locked by another user",
    );

    let remaining = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE id = ?")
        .bind(document_id)
        .fetch_one(&pool)
        .await
        .expect("remaining document");
    let active_locks =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_locks WHERE is_active = 1")
            .fetch_one(&pool)
            .await
            .expect("active locks");
    assert_eq!(remaining, 1);
    assert_eq!(active_locks, 1);
}

#[tokio::test]
async fn delete_forever_rejects_folder_archived_document_locked_by_other_user() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    allow_non_admin_archive_delete(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id =
        insert_named_versioned_document(&state.db, project.id, "locked.txt", "locked-version")
            .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let archive = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "folder", "id": project.id}]}),
        ))
        .await
        .expect("archive folder response");
    assert_eq!(response_json(archive).await["ok"][0]["detail"], "Archive");
    sqlx::query(
        r"
        INSERT INTO document_locks (document_id, locked_by, locked_by_name)
        VALUES (?, 'editor', 'Editor')
        ",
    )
    .bind(document_id)
    .execute(&pool)
    .await
    .expect("lock archived document");

    let response = app
        .oneshot(authed_json_post(
            "/api/delete-forever",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("delete response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"], json!([]));
    assert_eq!(
        json["failed"][0]["detail"],
        "Document is locked by another user",
    );

    let document = sqlx::query_as::<_, (String, i64)>(
        r"
        SELECT folders.root_key, documents.folder_id
        FROM documents
        JOIN folders ON folders.id = documents.folder_id
        WHERE documents.id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document remains");
    let source_folder_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE id = ?")
            .bind(project.id)
            .fetch_one(&pool)
            .await
            .expect("source folder count");
    let active_locks =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_locks WHERE is_active = 1")
            .fetch_one(&pool)
            .await
            .expect("active locks");
    let version_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_versions WHERE document_id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("version count");

    assert_eq!(document, (ARCHIVE_ROOT_KEY.to_string(), archive_root.id));
    assert_eq!(source_folder_count, 0);
    assert_eq!(active_locks, 1);
    assert_eq!(version_count, 1);
}

#[tokio::test]
async fn rename_document_updates_name_history_ttl_and_state() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    sqlx::query(
        "UPDATE folders SET default_ttl_days = 14, default_ttl_action = 'archive' WHERE id = ?",
    )
    .bind(project.id)
    .execute(&state.db)
    .await
    .expect("set ttl");
    let document_id =
        insert_named_versioned_document(&state.db, project.id, "asset.fbx", "rename-version").await;
    sqlx::query(
        r"
        UPDATE documents
        SET latest_modified_at = '2026-06-01 00:00:00',
            expires_at = '2026-06-02 00:00:00',
            expiry_action = 'delete'
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("seed expiry");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}], "name": "asset-renamed.fbx"}),
        ))
        .await
        .expect("rename response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"][0]["detail"], "Project/asset-renamed.fbx");
    assert_eq!(json["failed"], json!([]));

    let document = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT name, expiry_action, expires_at FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document");
    let event = sqlx::query_as::<_, (String, String, String, String)>(
        r"
        SELECT event_type, message, ip, user_agent
        FROM document_events
        WHERE document_id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document event");
    let state_event = sqlx::query_scalar::<_, String>(
        "SELECT event_type FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");

    assert_eq!(document.0, "asset-renamed.fbx");
    assert_eq!(document.1, "archive");
    assert_ne!(document.2.as_deref(), Some("2026-06-02 00:00:00"));
    assert_eq!(event.0, "move");
    assert_eq!(
        event.1,
        "Moved from Project/asset.fbx to Project/asset-renamed.fbx",
    );
    assert_eq!(event.2, "203.0.113.9");
    assert_eq!(event.3, "vault-test");
    assert_eq!(state_event, "batch.rename");
}

#[tokio::test]
async fn rename_document_in_delete_ttl_scope_refreshes_expiry_before_sweep() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let temp = get_or_create_folder_path(&state.db, Some("Temp"))
        .await
        .expect("temp");
    sqlx::query(
        "UPDATE folders SET default_ttl_days = 7, default_ttl_action = 'delete' WHERE id = ?",
    )
    .bind(temp.id)
    .execute(&state.db)
    .await
    .expect("set ttl");
    let document_id =
        insert_named_versioned_document(&state.db, temp.id, "draft.txt", "rename-delete-ttl").await;
    sqlx::query(
        r"
        UPDATE documents
        SET latest_modified_at = '2025-06-01 00:00:00',
            expires_at = '2025-06-08 00:00:00',
            expiry_action = 'delete'
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("seed expired ttl");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "document", "id": document_id}],
                "name": "draft-renamed.txt"
            }),
        ))
        .await
        .expect("rename response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"][0]["detail"], "Temp/draft-renamed.txt");
    assert_eq!(json["failed"], json!([]));

    let document = sqlx::query_as::<_, (String, Option<String>, i64)>(
        r"
        SELECT
            name,
            expiry_action,
            datetime(expires_at) > datetime('now', '+6 days') AS future_expiry
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document");
    assert_eq!(
        document,
        (
            "draft-renamed.txt".to_string(),
            Some("delete".to_string()),
            1
        )
    );

    let sweep = sweep_expired_documents(&pool, 250).await.expect("sweep");
    assert!(sweep.deleted.is_empty());
    assert!(sweep.archived.is_empty());
    assert!(sweep.skipped.is_empty());
}

#[tokio::test]
async fn rename_document_rejects_archived_locked_and_duplicate_targets() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let archived_doc =
        insert_named_versioned_document(&state.db, project.id, "archived.txt", "ren-archived")
            .await;
    let locked_doc =
        insert_named_versioned_document(&state.db, project.id, "locked.txt", "ren-locked").await;
    let source_doc =
        insert_named_versioned_document(&state.db, project.id, "source.txt", "ren-source").await;
    insert_named_versioned_document(&state.db, project.id, "taken.txt", "ren-taken").await;
    mark_document_archived(
        &state.db,
        archived_doc,
        archive_root.id,
        &json!({writers.to_string(): 3}),
    )
    .await;
    sqlx::query(
        r"
        INSERT INTO document_locks (document_id, locked_by, locked_by_name)
        VALUES (?, 'other', 'Other')
        ",
    )
    .bind(locked_doc)
    .execute(&state.db)
    .await
    .expect("lock document");
    let pool = state.db.clone();
    let app = http::router(state);

    let archived = app
        .clone()
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": archived_doc}], "name": "restored.txt"}),
        ))
        .await
        .expect("archived rename");
    assert_eq!(
        response_json(archived).await["failed"][0]["detail"],
        "Restore archived files before renaming",
    );

    let locked = app
        .clone()
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": locked_doc}], "name": "locked-new.txt"}),
        ))
        .await
        .expect("locked rename");
    assert_eq!(
        response_json(locked).await["failed"][0]["detail"],
        "Document is locked by another user",
    );

    assert_document_rename_invalid_destination(app.clone(), source_doc).await;

    assert_document_rename_duplicate_target(app, source_doc).await;

    let names = sqlx::query_scalar::<_, String>(
        r"
        SELECT name
        FROM documents
        WHERE id IN (?, ?, ?)
        ORDER BY id
        ",
    )
    .bind(archived_doc)
    .bind(locked_doc)
    .bind(source_doc)
    .fetch_all(&pool)
    .await
    .expect("document names");
    assert_eq!(names, vec!["archived.txt", "locked.txt", "source.txt"]);
}

async fn assert_document_rename_invalid_destination(app: Router, source_doc: i64) {
    let invalid_destination = app
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "document", "id": source_doc}],
                "destination_folder": "..",
                "name": "invalid-destination.txt"
            }),
        ))
        .await
        .expect("invalid destination rename");
    let invalid_status = invalid_destination.status();
    let invalid_json = response_json(invalid_destination).await;

    assert_eq!(invalid_status, StatusCode::OK);
    assert_eq!(invalid_json["ok"], json!([]));
    assert_eq!(invalid_json["failed"][0]["detail"], "Invalid folder path",);
}

async fn assert_document_rename_duplicate_target(app: Router, source_doc: i64) {
    let duplicate = app
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": source_doc}], "name": "taken.txt"}),
        ))
        .await
        .expect("duplicate rename");

    assert_eq!(
        response_json(duplicate).await["failed"][0]["detail"],
        "A document already exists at that path",
    );
}

#[tokio::test]
async fn move_document_updates_folder_history_ttl_and_state() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let destination = get_or_create_folder_path(&state.db, Some("Destination"))
        .await
        .expect("destination");
    sqlx::query(
        "UPDATE folders SET default_ttl_days = 21, default_ttl_action = 'archive' WHERE id = ?",
    )
    .bind(destination.id)
    .execute(&state.db)
    .await
    .expect("set ttl");
    let document_id =
        insert_named_versioned_document(&state.db, project.id, "asset.fbx", "move-version").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "document", "id": document_id}],
                "destination_folder": "Destination"
            }),
        ))
        .await
        .expect("move response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"][0]["detail"], "Destination/asset.fbx");
    assert_eq!(json["failed"], json!([]));

    let document = sqlx::query_as::<_, (i64, String, Option<String>)>(
        "SELECT folder_id, name, expiry_action FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document");
    let event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, message FROM document_events WHERE document_id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document event");
    let state_event = sqlx::query_scalar::<_, String>(
        "SELECT event_type FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");

    assert_eq!(
        document,
        (
            destination.id,
            "asset.fbx".to_string(),
            Some("archive".to_string())
        )
    );
    assert_eq!(event.0, "move");
    assert_eq!(
        event.1,
        "Moved from Project/asset.fbx to Destination/asset.fbx",
    );
    assert_eq!(state_event, "batch.move");
}

#[tokio::test]
async fn move_document_out_of_delete_ttl_scope_clears_expiry_before_sweep() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let temp = get_or_create_folder_path(&state.db, Some("Temp"))
        .await
        .expect("temp");
    let safe = get_or_create_folder_path(&state.db, Some("Safe"))
        .await
        .expect("safe");
    sqlx::query(
        "UPDATE folders SET default_ttl_days = 1, default_ttl_action = 'delete' WHERE id = ?",
    )
    .bind(temp.id)
    .execute(&state.db)
    .await
    .expect("set ttl");
    let document_id =
        insert_named_versioned_document(&state.db, temp.id, "rescue.txt", "move-clear-ttl").await;
    sqlx::query(
        r"
        UPDATE documents
        SET latest_modified_at = '2025-06-01 00:00:00',
            expires_at = '2025-06-02 00:00:00',
            expiry_action = 'delete'
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("seed expired ttl");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "document", "id": document_id}],
                "destination_folder": "Safe"
            }),
        ))
        .await
        .expect("move response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"][0]["detail"], "Safe/rescue.txt");
    assert_eq!(json["failed"], json!([]));

    let document = sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
        "SELECT folder_id, expires_at, expiry_action FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document");
    assert_eq!(document, (safe.id, None, None));

    let sweep = sweep_expired_documents(&pool, 250).await.expect("sweep");
    assert!(sweep.deleted.is_empty());
    assert!(sweep.archived.is_empty());
    assert!(sweep.skipped.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document count"),
        1,
    );
}

#[tokio::test]
async fn move_document_rejects_duplicate_locked_and_archive_root_moves() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let destination = get_or_create_folder_path(&state.db, Some("Destination"))
        .await
        .expect("destination");
    let duplicate =
        insert_named_versioned_document(&state.db, project.id, "taken.txt", "move-duplicate").await;
    let locked =
        insert_named_versioned_document(&state.db, project.id, "locked.txt", "move-locked").await;
    let archive_move =
        insert_named_versioned_document(&state.db, project.id, "archive.txt", "move-archive").await;
    insert_named_versioned_document(&state.db, destination.id, "taken.txt", "move-taken").await;
    sqlx::query(
        r"
        INSERT INTO document_locks (document_id, locked_by, locked_by_name)
        VALUES (?, 'other', 'Other')
        ",
    )
    .bind(locked)
    .execute(&state.db)
    .await
    .expect("lock document");
    let pool = state.db.clone();
    let app = http::router(state);

    let duplicate_response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": duplicate}], "destination_folder": "Destination"}),
        ))
        .await
        .expect("duplicate move");
    assert_eq!(
        response_json(duplicate_response).await["failed"][0]["detail"],
        "A document already exists at that path",
    );

    let locked_response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": locked}], "destination_folder": "Destination"}),
        ))
        .await
        .expect("locked move");
    assert_eq!(
        response_json(locked_response).await["failed"][0]["detail"],
        "Document is locked by another user",
    );

    let archive_response = app
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": archive_move}], "destination_folder": "Archive"}),
        ))
        .await
        .expect("archive move");
    assert_eq!(
        response_json(archive_response).await["failed"][0]["detail"],
        "Use archive or restore for Archive moves",
    );

    let project_documents =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE folder_id = ?")
            .bind(project.id)
            .fetch_one(&pool)
            .await
            .expect("project documents");
    assert_eq!(project_documents, 3);
}

#[tokio::test]
async fn move_document_rejects_archived_document_without_creating_destination() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id =
        insert_named_versioned_document(&state.db, project.id, "plan.txt", "stale-move").await;
    mark_document_archived(
        &state.db,
        document_id,
        archive_root.id,
        &json!({writers.to_string(): 3}),
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "document", "id": document_id}],
                "destination_folder": "Other"
            }),
        ))
        .await
        .expect("archived move");
    let json = response_json(response).await;
    assert_eq!(json["ok"], json!([]));
    assert_eq!(
        json["failed"][0]["detail"],
        "Use archive or restore for Archive moves",
    );
    assert_archived_document_move_to_archive_child_fails(app.clone(), document_id).await;

    let archived = sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
        "SELECT folder_id, archived_from_folder, archived_original_name FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("archived document");
    let other_exists =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE name = 'Other'")
            .fetch_one(&pool)
            .await
            .expect("other folder count");
    let move_events = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_events WHERE document_id = ? AND event_type = 'move'",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("move event count");
    let state_events = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM state_events WHERE event_type = 'batch.move'",
    )
    .fetch_one(&pool)
    .await
    .expect("state event count");

    assert_eq!(
        archived,
        (
            archive_root.id,
            Some("Project".to_string()),
            Some("plan.txt".to_string()),
        ),
    );
    assert_eq!(other_exists, 0);
    assert_eq!(move_events, 0);
    assert_eq!(state_events, 0);
}

async fn assert_archived_document_move_to_archive_child_fails(app: Router, document_id: i64) {
    let response = app
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "document", "id": document_id}],
                "destination_folder": "Archive/Subfolder"
            }),
        ))
        .await
        .expect("archive child move");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"], json!([]));
    assert_eq!(
        json["failed"][0]["detail"],
        "Archive does not contain folders",
    );
}

async fn assert_document_archived_and_lock_released(
    pool: &sqlx::SqlitePool,
    document_id: i64,
    archive_root_id: i64,
) {
    let archived = sqlx::query_as::<_, (i64, String, String, String)>(
        r"
        SELECT folder_id, name, archived_from_folder, archived_original_name
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("archived document");
    let active_locks =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_locks WHERE is_active = 1")
            .fetch_one(pool)
            .await
            .expect("active locks");

    assert_eq!(
        archived,
        (
            archive_root_id,
            "plan.txt".to_string(),
            "Project".to_string(),
            "plan.txt".to_string(),
        ),
    );
    assert_eq!(active_locks, 0);
}

async fn assert_document_restored_with_archive_events(
    pool: &sqlx::SqlitePool,
    document_id: i64,
    project_id: i64,
) {
    let restored = sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
        "SELECT folder_id, archived_from_folder, archived_original_name FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("restored document");
    let events = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, message FROM document_events WHERE document_id = ? ORDER BY id",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await
    .expect("document events");
    let states = sqlx::query_scalar::<_, String>("SELECT event_type FROM state_events ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("state events");

    assert_eq!(restored, (project_id, None, None));
    assert_eq!(
        events[0],
        (
            "archive".to_string(),
            "Archived from Project/plan.txt".to_string(),
        ),
    );
    assert_eq!(
        events[1],
        (
            "unarchive".to_string(),
            "Restored to Vault from Archive/plan.txt".to_string(),
        ),
    );
    assert_eq!(
        states,
        vec!["batch.archive".to_string(), "batch.restore".to_string()],
    );
}

#[tokio::test]
async fn archive_restore_document_round_trip_preserves_metadata_and_events() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_versioned_document(&state.db, project.id).await;
    sqlx::query("INSERT INTO document_locks (document_id, locked_by) VALUES (?, '1')")
        .bind(document_id)
        .execute(&state.db)
        .await
        .expect("writer lock");
    let pool = state.db.clone();
    let app = http::router(state);

    let archive = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("archive response");
    let archive_json = response_json(archive).await;
    assert_eq!(archive_json["ok"][0]["detail"], "Archive/plan.txt");
    assert_document_archived_and_lock_released(&pool, document_id, archive_root.id).await;

    let restore = app
        .oneshot(authed_json_post(
            "/api/restore",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("restore response");
    let restore_json = response_json(restore).await;
    assert_eq!(restore_json["ok"][0]["detail"], "Project/plan.txt");
    assert_document_restored_with_archive_events(&pool, document_id, project.id).await;
}

#[tokio::test]
async fn archived_document_hidden_when_source_acl_snapshot_denies_access() {
    let (state, _temp_dir) = test_state().await;
    let outsiders = create_group(&state.db, "outsiders").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, outsiders, true, true, true)
        .await
        .expect("outsider vault root");
    add_folder_permission(&state.db, archive_root.id, outsiders, true, true, true)
        .await
        .expect("outsider archive root");
    let secret = get_or_create_folder_path(&state.db, Some("Secret"))
        .await
        .expect("secret folder");
    add_folder_permission(&state.db, secret.id, outsiders, false, false, false)
        .await
        .expect("deny outsider secret");
    let document_id =
        insert_named_versioned_document(&state.db, secret.id, "roadmap.txt", "archive-acl").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let archive = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("archive response");
    assert_eq!(
        response_json(archive).await["ok"][0]["detail"],
        "Archive/roadmap.txt",
    );

    let archive_contents = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Archive",
            "outsider",
            "outsiders",
        ))
        .await
        .expect("archive contents");
    let archive_json = response_json(archive_contents).await;
    let archived = sqlx::query_as::<_, (i64, String, String, String)>(
        r"
        SELECT folder_id, name, archived_from_folder, archived_original_name
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("archived row");

    assert_eq!(archive_json["folders"], json!([]));
    assert_eq!(archive_json["documents"], json!([]));
    assert_eq!(
        archived,
        (
            archive_root.id,
            "roadmap.txt".to_string(),
            "Secret".to_string(),
            "roadmap.txt".to_string(),
        ),
    );
}

#[tokio::test]
async fn restore_document_preserves_current_vault_folder_acl() {
    let (state, _temp_dir) = test_state().await;
    let outsiders = create_group(&state.db, "outsiders").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, outsiders, true, true, true)
        .await
        .expect("outsider vault root");
    add_folder_permission(&state.db, archive_root.id, outsiders, true, true, true)
        .await
        .expect("outsider archive root");
    let secret = get_or_create_folder_path(&state.db, Some("Secret"))
        .await
        .expect("secret folder");
    let document_id =
        insert_named_versioned_document(&state.db, secret.id, "roadmap.txt", "restore-acl").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let archive = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("archive response");
    assert_eq!(
        response_json(archive).await["ok"][0]["detail"],
        "Archive/roadmap.txt",
    );
    add_folder_permission(&pool, secret.id, outsiders, false, false, false)
        .await
        .expect("deny outsider secret");

    let restore = app
        .clone()
        .oneshot(authed_json_post(
            "/api/restore",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("restore response");
    assert_eq!(
        response_json(restore).await["ok"][0]["detail"],
        "Secret/roadmap.txt",
    );

    let denied_contents = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Secret",
            "outsider",
            "outsiders",
        ))
        .await
        .expect("denied contents");
    let denied_status = denied_contents.status();
    let denied_json = response_json(denied_contents).await;
    let permissions = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        r"
        SELECT group_id, can_view, can_read, can_write
        FROM folder_permissions
        WHERE folder_id = ? AND group_id = ?
        ",
    )
    .bind(secret.id)
    .bind(outsiders)
    .fetch_one(&pool)
    .await
    .expect("secret permission");
    let restored = sqlx::query_as::<_, (i64, Option<String>, Option<String>, Option<String>)>(
        r"
        SELECT folder_id, archived_from_folder, archived_original_name, archived_access
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("restored document");

    assert_eq!(denied_status, StatusCode::NOT_FOUND);
    assert_eq!(denied_json["detail"], "Folder not found");
    assert_eq!(permissions, (outsiders, 0, 0, 0));
    assert_eq!(restored, (secret.id, None, None, None));
}

const STABLE_LOCATION_COMMITTED_AT_SQL: &str = "2026-06-20 18:00:00";
const STABLE_LOCATION_MODIFIED_AT: &str = "2026-06-20T18:00:00+00:00";
const STABLE_LOCATION_MODIFIED_DISPLAY: &str = "Jun 20, 2026 at 6:00 pm";

async fn location_timestamp_test_state() -> (AppState, tempfile::TempDir, i64) {
    let (state, temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id =
        insert_named_versioned_document(&state.db, project.id, "plan.txt", "stable-location-ts")
            .await;
    sqlx::query(
        r"
        UPDATE document_versions
        SET committed_at = ?
        WHERE document_id = ?
        ",
    )
    .bind(STABLE_LOCATION_COMMITTED_AT_SQL)
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("stable version timestamp");
    sqlx::query(
        r"
        UPDATE documents
        SET latest_modified_at = '2030-01-01 00:00:00'
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("different location timestamp");
    (state, temp_dir, document_id)
}

fn assert_stable_location_modified_payload(payload: &Value) {
    assert_eq!(payload["modified_at"], STABLE_LOCATION_MODIFIED_AT);
    assert_eq!(
        payload["modified_display"],
        STABLE_LOCATION_MODIFIED_DISPLAY
    );
}

async fn assert_document_detail_keeps_stable_location_modified(
    app: Router,
    document_id: i64,
    expected_response: &str,
) {
    let detail = app
        .oneshot(authed_get(
            &format!("/api/documents/{document_id}/detail"),
            "writer",
            "writers",
        ))
        .await
        .expect(expected_response);
    let detail_json = response_json(detail).await;
    assert_stable_location_modified_payload(&detail_json);
}

async fn assert_folder_properties_keep_stable_location_modified(app: Router, path: &str) {
    let properties = app
        .oneshot(authed_get(
            &format!("/api/folders/properties?path={path}"),
            "writer",
            "writers",
        ))
        .await
        .expect("folder properties");
    let properties_json = response_json(properties).await;
    assert_eq!(properties_json["modified_at"], STABLE_LOCATION_MODIFIED_AT,);
}

#[tokio::test]
async fn archive_restore_document_rows_keep_version_commit_modified_time() {
    let (state, _temp_dir, document_id) = location_timestamp_test_state().await;
    let pool = state.db.clone();
    let app = http::router(state);

    assert_document_detail_keeps_stable_location_modified(
        app.clone(),
        document_id,
        "detail response",
    )
    .await;

    let archive = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("archive response");
    assert_eq!(
        response_json(archive).await["ok"][0]["detail"],
        "Archive/plan.txt"
    );
    let archive_contents = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Archive",
            "writer",
            "writers",
        ))
        .await
        .expect("archive contents");
    let archive_contents_json = response_json(archive_contents).await;
    assert_stable_location_modified_payload(&archive_contents_json["documents"][0]);
    assert_folder_properties_keep_stable_location_modified(app.clone(), "Archive").await;

    let restore = app
        .clone()
        .oneshot(authed_json_post(
            "/api/restore",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("restore response");
    assert_eq!(
        response_json(restore).await["ok"][0]["detail"],
        "Project/plan.txt"
    );
    assert_document_detail_keeps_stable_location_modified(
        app.clone(),
        document_id,
        "restored detail response",
    )
    .await;
    assert_folder_properties_keep_stable_location_modified(app, "Project").await;

    let location_modified_at =
        sqlx::query_scalar::<_, String>("SELECT latest_modified_at FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document location timestamp");
    assert_ne!(location_modified_at, STABLE_LOCATION_COMMITTED_AT_SQL);
}

#[tokio::test]
async fn archive_restore_document_returns_item_level_failures() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let active_doc =
        insert_named_versioned_document(&state.db, project.id, "active.txt", "arch-active").await;
    let archived_doc =
        insert_named_versioned_document(&state.db, project.id, "archived.txt", "arch-archived")
            .await;
    mark_document_archived(
        &state.db,
        archived_doc,
        archive_root.id,
        &json!({writers.to_string(): 3}),
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let already_archived = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": archived_doc}]}),
        ))
        .await
        .expect("already archived");
    assert_eq!(
        response_json(already_archived).await["failed"][0]["detail"],
        "Document is already archived",
    );

    let not_archived = app
        .clone()
        .oneshot(authed_json_post(
            "/api/restore",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": active_doc}]}),
        ))
        .await
        .expect("not archived");
    assert_eq!(
        response_json(not_archived).await["failed"][0]["detail"],
        "Document is not archived",
    );

    sqlx::query("UPDATE documents SET archived_from_folder = NULL WHERE id = ?")
        .bind(archived_doc)
        .execute(&pool)
        .await
        .expect("missing metadata");
    let missing_metadata = app
        .oneshot(authed_json_post(
            "/api/restore",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": archived_doc}]}),
        ))
        .await
        .expect("missing metadata restore");
    assert_eq!(
        response_json(missing_metadata).await["failed"][0]["detail"],
        "Archived document is missing restore metadata",
    );
}

#[tokio::test]
async fn restore_document_rejects_duplicate_target_name() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer vault root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let archived_doc =
        insert_named_versioned_document(&state.db, project.id, "taken.txt", "restore-archived")
            .await;
    mark_document_archived(
        &state.db,
        archived_doc,
        archive_root.id,
        &json!({writers.to_string(): 3}),
    )
    .await;
    insert_named_versioned_document(&state.db, project.id, "taken.txt", "restore-taken").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let duplicate = app
        .oneshot(authed_json_post(
            "/api/restore",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": archived_doc}]}),
        ))
        .await
        .expect("duplicate restore");
    assert_eq!(
        response_json(duplicate).await["failed"][0]["detail"],
        "A document already exists at that path",
    );

    let still_archived =
        sqlx::query_scalar::<_, i64>("SELECT folder_id FROM documents WHERE id = ?")
            .bind(archived_doc)
            .fetch_one(&pool)
            .await
            .expect("archived folder id");
    assert_eq!(still_archived, archive_root.id);
}
