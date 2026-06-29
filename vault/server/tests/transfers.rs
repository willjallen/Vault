use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::time::sleep;
use tower::ServiceExt;
use vault_server::auth::{AuthMode, AuthSettings};
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{VAULT_ROOT_KEY, get_root_folder};
use vault_server::http::{self, AppState};
use vault_server::storage::{LocalBlobStorage, StoredBlob};
use vault_server::transfers::{recover_interrupted_transfers, sweep_expired_transfers};

const EXPIRED_AT: &str = "2000-01-01T00:00:00Z";
const FUTURE_AT: &str = "2999-01-01T00:00:00Z";

async fn test_state(auth: AuthSettings) -> (AppState, tempfile::TempDir) {
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

fn dev_auth() -> AuthSettings {
    AuthSettings {
        mode: AuthMode::Dev,
        dev_mode: true,
        dev_auth_enabled: true,
        base_domain: "localhost".to_string(),
        ..AuthSettings::default()
    }
}

fn dev_post(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

async fn insert_upload_session(pool: &sqlx::SqlitePool, id: &str, status: &str) {
    insert_upload_session_with_expiration(pool, id, status, EXPIRED_AT).await;
}

async fn insert_upload_session_with_expiration(
    pool: &sqlx::SqlitePool,
    id: &str,
    status: &str,
    expires_at: &str,
) {
    sqlx::query(
        r"
        INSERT INTO upload_sessions
            (
                id,
                mode,
                status,
                filename,
                total_size,
                chunk_size,
                part_count,
                created_by,
                created_by_name,
                user_context,
                expires_at
            )
        VALUES
            (?, 'create', ?, 'expired.txt', 1, 1, 1, 'owner', 'Owner', '{}', ?)
        ",
    )
    .bind(id)
    .bind(status)
    .bind(expires_at)
    .execute(pool)
    .await
    .expect("upload session");
}

async fn insert_export_job(pool: &sqlx::SqlitePool, id: &str, status: &str) {
    insert_export_job_with_expiration(pool, id, status, EXPIRED_AT).await;
}

async fn insert_export_job_with_expiration(
    pool: &sqlx::SqlitePool,
    id: &str,
    status: &str,
    expires_at: &str,
) {
    sqlx::query(
        r"
        INSERT INTO export_jobs
            (
                id,
                status,
                filename,
                total_items,
                total_bytes,
                created_by,
                created_by_name,
                user_context,
                expires_at
            )
        VALUES
            (?, ?, 'expired.zip', 1, 1, 'owner', 'Owner', '{}', ?)
        ",
    )
    .bind(id)
    .bind(status)
    .bind(expires_at)
    .execute(pool)
    .await
    .expect("export job");
}

async fn insert_stored_blob(state: &AppState, content: &[u8]) -> (i64, StoredBlob) {
    let stored = state
        .storage
        .put_bytes(content)
        .await
        .expect("stored bytes");
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES (?, ?, ?)
        ",
    )
    .bind(&stored.hash_algo)
    .bind(&stored.digest)
    .bind(i64::try_from(stored.size_bytes).expect("stored size"))
    .execute(&state.db)
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
    .execute(&state.db)
    .await
    .expect("blob location");
    (blob_id, stored)
}

async fn insert_export_artifact(
    pool: &sqlx::SqlitePool,
    job_id: &str,
    blob_id: i64,
    blob: &StoredBlob,
) {
    sqlx::query(
        r"
        INSERT INTO export_artifacts
            (job_id, blob_id, filename, mime_type, size_bytes, hash_algo, hash, expires_at)
        VALUES
            (?, ?, 'expired.zip', 'application/zip', ?, ?, ?, ?)
        ",
    )
    .bind(job_id)
    .bind(blob_id)
    .bind(i64::try_from(blob.size_bytes).expect("artifact size"))
    .bind(&blob.hash_algo)
    .bind(&blob.digest)
    .bind(EXPIRED_AT)
    .execute(pool)
    .await
    .expect("export artifact");
}

async fn write_recoverable_part(transfers_path: &std::path::Path, session_id: &str) {
    let upload_dir = transfers_path.join("uploads").join(session_id);
    tokio::fs::create_dir_all(&upload_dir)
        .await
        .expect("upload dir");
    tokio::fs::write(upload_dir.join("00000001.part"), b"x")
        .await
        .expect("part file");
    tokio::fs::write(
        upload_dir.join("00000001.json"),
        serde_json::to_vec(&json!({
            "part_number": 1,
            "offset_bytes": 0,
            "size_bytes": 1,
            "sha256": "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
        }))
        .expect("part json"),
    )
    .await
    .expect("part metadata");
}

async fn insert_document_with_current_version(state: &AppState, name: &str, content: &[u8]) -> i64 {
    let (blob_id, _stored) = insert_stored_blob(state, content).await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, ?, 'admin', 'Admin', 'admin')
        ",
    )
    .bind(root.id)
    .bind(name)
    .execute(&state.db)
    .await
    .expect("document")
    .last_insert_rowid();
    let version_id = format!("startup-version-{document_id}");
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
            (?, ?, ?, 1, 'admin', 'Admin', 'Uploaded startup export', 'text/plain', ?, 'upload')
        ",
    )
    .bind(&version_id)
    .bind(document_id)
    .bind(blob_id)
    .bind(name)
    .execute(&state.db)
    .await
    .expect("document version");
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
    .execute(&state.db)
    .await
    .expect("current version");
    document_id
}

async fn wait_for_export_status(pool: &sqlx::SqlitePool, job_id: &str, expected: &str) {
    for _ in 0..50 {
        let status = sqlx::query_scalar::<_, String>("SELECT status FROM export_jobs WHERE id = ?")
            .bind(job_id)
            .fetch_one(pool)
            .await
            .expect("export status");
        if status == expected {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("export {job_id} did not reach {expected}");
}

#[tokio::test]
async fn recovery_resumes_recoverable_completing_uploads_and_fails_missing_parts() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_upload_session_with_expiration(&state.db, "recoverable-upload", "completing", FUTURE_AT)
        .await;
    insert_upload_session_with_expiration(&state.db, "missing-upload", "completing", FUTURE_AT)
        .await;
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET verification_total_bytes = 1,
            verification_processed_bytes = 1
        WHERE id IN ('recoverable-upload', 'missing-upload')
        ",
    )
    .execute(&state.db)
    .await
    .expect("verification state");
    let transfers_path = state.config.transfers_path();
    write_recoverable_part(&transfers_path, "recoverable-upload").await;
    let missing_dir = transfers_path.join("uploads").join("missing-upload");
    tokio::fs::create_dir_all(&missing_dir)
        .await
        .expect("missing dir");

    let result = recover_interrupted_transfers(&state.db, &state.storage, &transfers_path, false)
        .await
        .expect("recover");
    let recoverable: (String, i64, i64, Option<String>) = sqlx::query_as(
        r"
        SELECT status, verification_total_bytes, verification_processed_bytes, error
        FROM upload_sessions
        WHERE id = 'recoverable-upload'
        ",
    )
    .fetch_one(&state.db)
    .await
    .expect("recoverable status");
    let missing: (String, i64, i64, Option<String>) = sqlx::query_as(
        r"
        SELECT status, verification_total_bytes, verification_processed_bytes, error
        FROM upload_sessions
        WHERE id = 'missing-upload'
        ",
    )
    .fetch_one(&state.db)
    .await
    .expect("missing status");

    assert_eq!(result.resumed_uploads, vec!["recoverable-upload"]);
    assert_eq!(result.failed_uploads, vec!["missing-upload"]);
    assert_eq!(recoverable, ("active".to_string(), 0, 0, None));
    assert_eq!(
        missing,
        (
            "failed".to_string(),
            0,
            0,
            Some("Upload completion interrupted and part files are missing".to_string()),
        ),
    );
    assert!(
        tokio::fs::metadata(transfers_path.join("uploads/recoverable-upload/00000001.part"))
            .await
            .is_ok()
    );
    assert!(tokio::fs::metadata(missing_dir).await.is_err());
}

#[tokio::test]
async fn recovery_requeues_interrupted_exports_and_removes_partial_artifacts() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_export_job_with_expiration(&state.db, "running-export", "running", FUTURE_AT).await;
    let (blob_id, stored) = insert_stored_blob(&state, b"partial export bytes").await;
    insert_export_artifact(&state.db, "running-export", blob_id, &stored).await;
    let transfers_path = state.config.transfers_path();
    let export_dir = transfers_path.join("exports");
    tokio::fs::create_dir_all(&export_dir)
        .await
        .expect("export dir");
    tokio::fs::write(export_dir.join("running-export.zip.tmp"), b"partial")
        .await
        .expect("partial export");

    let result = recover_interrupted_transfers(&state.db, &state.storage, &transfers_path, false)
        .await
        .expect("recover");
    let status: String = sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = ?")
        .bind("running-export")
        .fetch_one(&state.db)
        .await
        .expect("status");
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
            .bind("running-export")
            .fetch_one(&state.db)
            .await
            .expect("artifact count");
    let blob_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE id = ?")
        .bind(blob_id)
        .fetch_one(&state.db)
        .await
        .expect("blob count");

    assert_eq!(result.requeued_exports, vec!["running-export"]);
    assert_eq!(result.deleted_export_temps, vec!["running-export.zip.tmp"]);
    assert_eq!(result.deleted_export_objects, Vec::<String>::new());
    assert_eq!(status, "queued");
    assert_eq!(artifact_count, 0);
    assert_eq!(blob_count, 0);
    assert!(
        tokio::fs::metadata(export_dir.join("running-export.zip.tmp"))
            .await
            .is_err()
    );
    assert!(
        state
            .storage
            .list_object_keys()
            .await
            .expect("object keys")
            .contains(&stored.object_key)
    );
}

#[tokio::test]
async fn recovery_starts_pending_queued_exports() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let document_id =
        insert_document_with_current_version(&state, "startup.txt", b"startup bytes").await;
    sqlx::query(
        r"
        INSERT INTO export_jobs
            (
                id,
                status,
                filename,
                total_items,
                total_bytes,
                created_by,
                created_by_name,
                user_context,
                request_payload,
                expires_at
            )
        VALUES
            (?, 'queued', 'startup.zip', 1, 13, 'admin', 'Admin', ?, ?, ?)
        ",
    )
    .bind("startup-export")
    .bind(
        serde_json::to_string(&json!({
            "id": "admin",
            "vault_user_id": 0,
            "issuer": "headers",
            "subject": "admin",
            "name": "Admin",
            "email": "admin@example.com",
            "groups": [],
            "is_admin": true
        }))
        .expect("user context"),
    )
    .bind(
        serde_json::to_string(&json!({
            "items": [
                {"type": "document", "id": document_id}
            ]
        }))
        .expect("request payload"),
    )
    .bind(FUTURE_AT)
    .execute(&state.db)
    .await
    .expect("queued export");
    let transfers_path = state.config.transfers_path();

    let result = recover_interrupted_transfers(&state.db, &state.storage, &transfers_path, true)
        .await
        .expect("recover");
    wait_for_export_status(&state.db, "startup-export", "complete").await;
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
            .bind("startup-export")
            .fetch_one(&state.db)
            .await
            .expect("artifact count");

    assert_eq!(result.queued_exports, vec!["startup-export"]);
    assert_eq!(artifact_count, 1);
}

#[tokio::test]
async fn sweep_expired_uploads_marks_active_and_removes_terminal_sessions() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_upload_session(&state.db, "active-upload", "active").await;
    insert_upload_session(&state.db, "failed-upload", "failed").await;
    insert_upload_session_with_expiration(
        &state.db,
        "future-python-timestamp-upload",
        "active",
        "2999-01-01 00:00:00",
    )
    .await;
    let transfers_path = state.config.transfers_path();
    let active_dir = transfers_path.join("uploads").join("active-upload");
    let failed_dir = transfers_path.join("uploads").join("failed-upload");
    tokio::fs::create_dir_all(&active_dir)
        .await
        .expect("active dir");
    tokio::fs::create_dir_all(&failed_dir)
        .await
        .expect("failed dir");
    tokio::fs::write(active_dir.join("00000001.part"), b"active")
        .await
        .expect("active scratch");
    tokio::fs::write(failed_dir.join("00000001.part"), b"failed")
        .await
        .expect("failed scratch");

    let result = sweep_expired_transfers(&state.db, &state.storage, &transfers_path)
        .await
        .expect("sweep");
    let active_status: String =
        sqlx::query_scalar("SELECT status FROM upload_sessions WHERE id = ?")
            .bind("active-upload")
            .fetch_one(&state.db)
            .await
            .expect("active status");
    let failed_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_sessions WHERE id = ?")
        .bind("failed-upload")
        .fetch_one(&state.db)
        .await
        .expect("failed count");
    let future_status: String =
        sqlx::query_scalar("SELECT status FROM upload_sessions WHERE id = ?")
            .bind("future-python-timestamp-upload")
            .fetch_one(&state.db)
            .await
            .expect("future status");

    assert_eq!(result.expired_uploads, vec!["active-upload"]);
    assert_eq!(result.deleted_uploads, vec!["failed-upload"]);
    assert_eq!(active_status, "expired");
    assert_eq!(failed_count, 0);
    assert_eq!(future_status, "active");
    assert!(tokio::fs::metadata(active_dir).await.is_err());
    assert!(tokio::fs::metadata(failed_dir).await.is_err());
}

#[tokio::test]
async fn sweep_expired_exports_cancels_active_and_deletes_terminal_artifacts() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_export_job(&state.db, "queued-export", "queued").await;
    insert_export_job(&state.db, "complete-export", "complete").await;
    let (blob_id, stored) = insert_stored_blob(&state, b"expired export bytes").await;
    insert_export_artifact(&state.db, "complete-export", blob_id, &stored).await;
    let transfers_path = state.config.transfers_path();

    let result = sweep_expired_transfers(&state.db, &state.storage, &transfers_path)
        .await
        .expect("sweep");
    let queued_status: String = sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = ?")
        .bind("queued-export")
        .fetch_one(&state.db)
        .await
        .expect("queued status");
    let complete_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM export_jobs WHERE id = ?")
        .bind("complete-export")
        .fetch_one(&state.db)
        .await
        .expect("complete count");
    let blob_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE id = ?")
        .bind(blob_id)
        .fetch_one(&state.db)
        .await
        .expect("blob count");

    assert_eq!(result.cancelled_exports, vec!["queued-export"]);
    assert_eq!(result.deleted_exports, vec!["complete-export"]);
    assert_eq!(result.deleted_export_objects, Vec::<String>::new());
    assert_eq!(queued_status, "cancelled");
    assert_eq!(complete_count, 0);
    assert_eq!(blob_count, 0);
    assert!(
        state
            .storage
            .list_object_keys()
            .await
            .expect("object keys")
            .contains(&stored.object_key)
    );
}

#[tokio::test]
async fn sweep_expired_export_preserves_artifact_blob_when_document_references_it() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_export_job(&state.db, "complete-export", "complete").await;
    let (blob_id, stored) = insert_stored_blob(&state, b"shared bytes").await;
    insert_export_artifact(&state.db, "complete-export", blob_id, &stored).await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, 'shared.txt', 'owner', 'Owner', 'owner')
        ",
    )
    .bind(root.id)
    .execute(&state.db)
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
            ('shared-version', ?, ?, 1, 'owner', 'Owner', 'Uploaded shared.txt', 'text/plain', 'shared.txt', 'upload')
        ",
    )
    .bind(document_id)
    .bind(blob_id)
    .execute(&state.db)
    .await
    .expect("version");
    let transfers_path = state.config.transfers_path();

    let result = sweep_expired_transfers(&state.db, &state.storage, &transfers_path)
        .await
        .expect("sweep");
    let location_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM blob_locations WHERE blob_id = ?")
            .bind(blob_id)
            .fetch_one(&state.db)
            .await
            .expect("location count");

    assert_eq!(result.deleted_exports, vec!["complete-export"]);
    assert_eq!(result.deleted_export_objects, Vec::<String>::new());
    assert_eq!(location_count, 1);
    assert_eq!(
        state
            .storage
            .read_bytes(&stored.object_key)
            .await
            .expect("shared object"),
        b"shared bytes",
    );
}

#[tokio::test]
async fn debug_sweep_ttl_route_returns_real_transfer_cleanup_result() {
    let (state, _temp_dir) = test_state(dev_auth()).await;
    insert_upload_session(&state.db, "route-upload", "active").await;
    let app = http::router(state);

    let response = app
        .oneshot(dev_post("/api/admin/debug/sweep-ttl"))
        .await
        .expect("sweep route");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;

    assert_eq!(body["action"], "sweep-ttl");
    assert_eq!(
        body["result"]["transfers"]["expired_uploads"],
        json!(["route-upload"]),
    );
}
