use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tower::ServiceExt;
use vault_server::auth::{AuthMode, AuthSettings};
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{VAULT_ROOT_KEY, get_root_folder};
use vault_server::http::{self, AppState};
use vault_server::storage::{LocalBlobStorage, S3_UPLOAD_STAGE_FILENAME, StoredBlob};
use vault_server::transfers::{
    TransferSweepResult, cleanup_upload_session_resources, recover_interrupted_transfers,
    sweep_expired_transfers, sweep_orphaned_upload_directories,
};

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
        export_max_active_jobs: 256,
        export_max_active_jobs_per_user: 16,
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

async fn wait_for_hash_state(state: &AppState, session_id: &str) {
    for _ in 0..100 {
        if state
            .upload_hash_coordinator
            .preverified_bytes(session_id)
            .await
            .is_some()
        {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("hash state was not created for {session_id}");
}

async fn sweep_state_transfers(
    state: &AppState,
    transfers_path: &std::path::Path,
) -> TransferSweepResult {
    sweep_expired_transfers(
        &state.db,
        &state.storage,
        transfers_path,
        &state.upload_hash_coordinator,
        &state.transfer_maintenance,
    )
    .await
    .expect("sweep")
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

async fn upload_sweep_survivors(pool: &sqlx::SqlitePool) -> Vec<(String, String, String)> {
    sqlx::query_as(
        r"
        SELECT id, status, expires_at
        FROM upload_sessions
        WHERE id != 'driver-upload'
        ORDER BY id
        ",
    )
    .fetch_all(pool)
    .await
    .expect("surviving upload rows")
}

async fn export_sweep_survivors(pool: &sqlx::SqlitePool) -> Vec<(String, String, String)> {
    sqlx::query_as(
        r"
        SELECT id, status, expires_at
        FROM export_jobs
        WHERE id != 'driver-export'
        ORDER BY id
        ",
    )
    .fetch_all(pool)
    .await
    .expect("surviving export rows")
}

async fn export_artifact_and_blob_counts(
    pool: &sqlx::SqlitePool,
    job_id: &str,
    blob_id: i64,
) -> (i64, i64) {
    let artifact_count =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
            .bind(job_id)
            .fetch_one(pool)
            .await
            .expect("preserved artifact");
    let blob_count = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE id = ?")
        .bind(blob_id)
        .fetch_one(pool)
        .await
        .expect("preserved blob");
    (artifact_count, blob_count)
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
    let recoverable_stage = transfers_path
        .join("uploads/recoverable-upload")
        .join(S3_UPLOAD_STAGE_FILENAME);
    let missing_stage = missing_dir.join(S3_UPLOAD_STAGE_FILENAME);
    tokio::fs::write(&recoverable_stage, b"partial remote stage")
        .await
        .expect("recoverable stage");
    tokio::fs::write(&missing_stage, b"partial remote stage")
        .await
        .expect("missing stage");

    let mut result =
        recover_interrupted_transfers(&state.db, &state.storage, &transfers_path, false)
            .await
            .expect("recover");
    result.deleted_upload_temps.sort();
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
    assert_eq!(
        result.deleted_upload_temps,
        vec![
            format!("missing-upload/{S3_UPLOAD_STAGE_FILENAME}"),
            format!("recoverable-upload/{S3_UPLOAD_STAGE_FILENAME}"),
        ]
    );
    assert_eq!(recoverable, ("active".to_string(), 0, 0, None));
    assert_eq!(
        missing,
        (
            "failed".to_string(),
            0,
            0,
            Some(
                "Upload completion interrupted and staged parts are missing or invalid".to_string()
            ),
        ),
    );
    assert!(
        tokio::fs::metadata(transfers_path.join("uploads/recoverable-upload/00000001.part"))
            .await
            .is_ok()
    );
    assert!(
        tokio::fs::metadata(missing_dir).await.is_ok(),
        "failed recovery must preserve staging until normal expiry"
    );
    assert!(!recoverable_stage.exists());
    assert!(!missing_stage.exists());
}

#[tokio::test]
async fn recovery_rejects_invalid_layouts_without_deleting_staging() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    for session_id in [
        "noncanonical-upload",
        "oversized-sidecar-upload",
        "zero-upload",
    ] {
        insert_upload_session_with_expiration(&state.db, session_id, "completing", FUTURE_AT).await;
    }
    sqlx::query("UPDATE upload_sessions SET part_count = 2 WHERE id = 'noncanonical-upload'")
        .execute(&state.db)
        .await
        .expect("noncanonical layout");
    sqlx::query(
        "UPDATE upload_sessions SET total_size = 0, part_count = 0 WHERE id = 'zero-upload'",
    )
    .execute(&state.db)
    .await
    .expect("zero-byte layout");
    let transfers_path = state.config.transfers_path();
    let upload_root = transfers_path.join("uploads");
    let noncanonical_dir = upload_root.join("noncanonical-upload");
    tokio::fs::create_dir_all(&noncanonical_dir)
        .await
        .expect("noncanonical directory");
    for part_number in 1..=2 {
        tokio::fs::write(
            noncanonical_dir.join(format!("{part_number:08}.part")),
            b"x",
        )
        .await
        .expect("noncanonical part");
    }
    let oversized_dir = upload_root.join("oversized-sidecar-upload");
    tokio::fs::create_dir_all(&oversized_dir)
        .await
        .expect("oversized sidecar directory");
    tokio::fs::write(oversized_dir.join("00000001.part"), b"x")
        .await
        .expect("oversized sidecar part");
    tokio::fs::write(oversized_dir.join("00000001.json"), vec![b'x'; 4097])
        .await
        .expect("oversized sidecar");

    let mut result =
        recover_interrupted_transfers(&state.db, &state.storage, &transfers_path, false)
            .await
            .expect("recover");
    result.resumed_uploads.sort();
    result.failed_uploads.sort();
    let statuses =
        sqlx::query_as::<_, (String, String)>("SELECT id, status FROM upload_sessions ORDER BY id")
            .fetch_all(&state.db)
            .await
            .expect("recovery statuses");

    assert_eq!(result.resumed_uploads, vec!["zero-upload"]);
    assert_eq!(
        result.failed_uploads,
        vec!["noncanonical-upload", "oversized-sidecar-upload"]
    );
    assert_eq!(
        statuses,
        vec![
            ("noncanonical-upload".to_string(), "failed".to_string()),
            ("oversized-sidecar-upload".to_string(), "failed".to_string()),
            ("zero-upload".to_string(), "active".to_string()),
        ]
    );
    assert!(noncanonical_dir.join("00000001.part").is_file());
    assert!(oversized_dir.join("00000001.part").is_file());
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
    assert_eq!(
        result.deleted_export_objects,
        vec![stored.object_key.clone()]
    );
    assert_eq!(status, "queued");
    assert_eq!(artifact_count, 0);
    assert_eq!(blob_count, 0);
    assert!(
        tokio::fs::metadata(export_dir.join("running-export.zip.tmp"))
            .await
            .is_err()
    );
    assert!(
        !state
            .storage
            .list_object_keys()
            .await
            .expect("object keys")
            .contains(&stored.object_key)
    );
}

#[tokio::test]
async fn recovery_requiring_a_dispatcher_fails_before_mutating_state() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_export_job_with_expiration(&state.db, "untouched-export", "running", FUTURE_AT).await;
    let transfers_path = state.config.transfers_path();
    let export_dir = transfers_path.join("exports");
    tokio::fs::create_dir_all(&export_dir)
        .await
        .expect("export dir");
    let temp_path = export_dir.join("untouched-export.zip.tmp");
    tokio::fs::write(&temp_path, b"partial")
        .await
        .expect("partial export");

    let error = recover_interrupted_transfers(&state.db, &state.storage, &transfers_path, true)
        .await
        .expect_err("recovery without a persistent dispatcher must fail closed");
    let status: String = sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = ?")
        .bind("untouched-export")
        .fetch_one(&state.db)
        .await
        .expect("untouched status");

    assert_eq!(
        error.to_string(),
        "export startup requires a persistent dispatcher runtime"
    );
    assert_eq!(status, "running");
    assert!(temp_path.is_file());
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

    let result = vault_server::transfers::recover_interrupted_transfers_with_export_runtime(
        &state.db,
        &state.storage,
        &transfers_path,
        true,
        &state.export_execution,
    )
    .await
    .expect("recover");
    wait_for_export_status(&state.db, "startup-export", "complete").await;
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
            .bind("startup-export")
            .fetch_one(&state.db)
            .await
            .expect("artifact count");

    assert!(result.requeued_exports.is_empty());
    assert_eq!(artifact_count, 1);
}

#[tokio::test]
async fn recovery_dispatcher_drains_beyond_the_legacy_startup_page() {
    const QUEUED_JOBS: i64 = 1_001;
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let mut transaction = state.db.begin().await.expect("queue transaction");
    for index in 0..QUEUED_JOBS {
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
            VALUES (?, 'queued', 'poison.zip', 0, 0, 'admin', 'Admin', '{}', '{', ?)
            ",
        )
        .bind(format!("queued-{index:04}"))
        .bind(FUTURE_AT)
        .execute(&mut *transaction)
        .await
        .expect("queued export");
    }
    transaction.commit().await.expect("commit queued exports");
    let transfers_path = state.config.transfers_path();

    vault_server::transfers::recover_interrupted_transfers_with_export_runtime(
        &state.db,
        &state.storage,
        &transfers_path,
        true,
        &state.export_execution,
    )
    .await
    .expect("recover");
    timeout(Duration::from_secs(20), async {
        loop {
            let queued: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM export_jobs WHERE status IN ('queued', 'running')",
            )
            .fetch_one(&state.db)
            .await
            .expect("remaining queued exports");
            if queued == 0 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dispatcher should drain every queued row");
    let failed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_jobs WHERE status = 'failed'")
            .fetch_one(&state.db)
            .await
            .expect("failed export count");
    assert_eq!(failed, QUEUED_JOBS);
}

#[tokio::test]
async fn idle_dispatcher_polls_for_db_only_queued_jobs() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let transfers_path = state.config.transfers_path();
    let execution = vault_server::exports::ExportExecutionContext::new_with_poll_interval(
        state.export_execution.settings().clone(),
        Duration::from_millis(50),
    );
    execution.start_dispatcher(&state.db, &state.storage, &transfers_path);
    // Let the startup notification drain so this insertion has no in-process wake-up paired
    // with it; the fixed periodic poller must discover the durable row.
    sleep(Duration::from_millis(100)).await;
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
        VALUES ('db-only-export', 'queued', 'poison.zip', 0, 0, 'admin', 'Admin', '{}', '{', ?)
        ",
    )
    .bind(FUTURE_AT)
    .execute(&state.db)
    .await
    .expect("DB-only queued export");

    timeout(Duration::from_secs(3), async {
        loop {
            let status: String =
                sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = 'db-only-export'")
                    .fetch_one(&state.db)
                    .await
                    .expect("DB-only export status");
            if status == "failed" {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("periodic dispatcher poll should claim DB-only work");
}

#[tokio::test]
async fn shutdown_requested_before_start_never_claims_queued_work() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
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
        VALUES ('shutdown-before-start', 'queued', 'poison.zip', 0, 0, 'admin', 'Admin', '{}', '{', ?)
        ",
    )
    .bind(FUTURE_AT)
    .execute(&state.db)
    .await
    .expect("queued export");
    state.export_execution.request_dispatcher_shutdown();
    state.export_execution.start_dispatcher(
        &state.db,
        &state.storage,
        &state.config.transfers_path(),
    );
    timeout(
        Duration::from_millis(100),
        state.export_execution.shutdown_dispatcher(),
    )
    .await
    .expect("shutdown without spawned handles should return promptly");
    sleep(Duration::from_millis(100)).await;

    let status: String =
        sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = 'shutdown-before-start'")
            .fetch_one(&state.db)
            .await
            .expect("queued status");
    assert_eq!(status, "queued");
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
    state.upload_hash_coordinator.schedule(
        state.db.clone(),
        transfers_path.clone(),
        "active-upload".to_string(),
    );
    wait_for_hash_state(&state, "active-upload").await;

    let result = sweep_state_transfers(&state, &transfers_path).await;
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
    assert!(
        state
            .upload_hash_coordinator
            .preverified_bytes("active-upload")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn orphan_upload_sweep_removes_only_old_validated_unreferenced_directories() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_upload_session_with_expiration(&state.db, "live-upload", "active", FUTURE_AT).await;
    let upload_root = state.config.transfers_path().join("uploads");
    let orphan = upload_root.join("orphan-upload");
    let live = upload_root.join("live-upload");
    let unsafe_name = upload_root.join("not.an-upload");
    tokio::fs::create_dir_all(&orphan)
        .await
        .expect("orphan directory");
    tokio::fs::create_dir_all(&live)
        .await
        .expect("live directory");
    tokio::fs::create_dir_all(&unsafe_name)
        .await
        .expect("unrecognized directory");

    let recent = sweep_orphaned_upload_directories(
        &state.db,
        &state.upload_hash_coordinator,
        &state.transfer_maintenance,
        &state.config.transfers_path(),
        Duration::from_hours(1),
        250,
    )
    .await
    .expect("age-gated orphan sweep");
    assert!(recent.is_empty());
    assert!(orphan.is_dir());

    #[cfg(unix)]
    let symlink = {
        let external = state.config.data_dir.join("must-not-delete");
        tokio::fs::create_dir_all(&external)
            .await
            .expect("external directory");
        tokio::fs::write(external.join("sentinel"), b"safe")
            .await
            .expect("external sentinel");
        let symlink = upload_root.join("symlink-upload");
        std::os::unix::fs::symlink(&external, &symlink).expect("upload symlink");
        (symlink, external)
    };

    let removed = sweep_orphaned_upload_directories(
        &state.db,
        &state.upload_hash_coordinator,
        &state.transfer_maintenance,
        &state.config.transfers_path(),
        Duration::ZERO,
        250,
    )
    .await
    .expect("orphan sweep");

    assert_eq!(removed, vec!["orphan-upload"]);
    assert!(!orphan.exists());
    assert!(live.is_dir());
    assert!(unsafe_name.is_dir());
    #[cfg(unix)]
    {
        assert!(symlink.0.is_symlink());
        assert_eq!(
            tokio::fs::read(symlink.1.join("sentinel"))
                .await
                .expect("external sentinel survives"),
            b"safe"
        );
    }
}

#[tokio::test]
async fn orphan_upload_sweep_advances_a_bounded_cursor_across_calls() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let upload_root = state.config.transfers_path().join("uploads");
    for session_id in ["orphan-a", "orphan-b", "orphan-c"] {
        tokio::fs::create_dir_all(upload_root.join(session_id))
            .await
            .expect("orphan directory");
    }
    let mut removed = Vec::new();
    for _ in 0..4 {
        removed.extend(
            sweep_orphaned_upload_directories(
                &state.db,
                &state.upload_hash_coordinator,
                &state.transfer_maintenance,
                &state.config.transfers_path(),
                Duration::ZERO,
                1,
            )
            .await
            .expect("bounded orphan sweep"),
        );
    }
    removed.sort();
    assert_eq!(removed, ["orphan-a", "orphan-b", "orphan-c"]);
}

#[cfg(unix)]
#[tokio::test]
async fn upload_cleanup_refuses_a_symlinked_upload_root() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let transfers_path = state.config.data_dir.join("symlinked-transfers");
    let external_root = state.config.data_dir.join("external-upload-root");
    let external_upload = external_root.join("external-upload");
    tokio::fs::create_dir_all(&transfers_path)
        .await
        .expect("transfer root");
    tokio::fs::create_dir_all(&external_upload)
        .await
        .expect("external upload");
    tokio::fs::write(external_upload.join("sentinel"), b"safe")
        .await
        .expect("external sentinel");
    std::os::unix::fs::symlink(&external_root, transfers_path.join("uploads"))
        .expect("symlinked upload root");

    cleanup_upload_session_resources(
        &state.upload_hash_coordinator,
        &transfers_path,
        &["external-upload".to_string()],
    )
    .await;
    let removed = sweep_orphaned_upload_directories(
        &state.db,
        &state.upload_hash_coordinator,
        &state.transfer_maintenance,
        &transfers_path,
        Duration::ZERO,
        250,
    )
    .await
    .expect("symlink-safe orphan sweep");

    assert!(removed.is_empty());
    assert_eq!(
        tokio::fs::read(external_upload.join("sentinel"))
            .await
            .expect("external sentinel survives"),
        b"safe"
    );
}

#[tokio::test]
async fn sweep_expired_uploads_ignores_stale_status_and_expiration_snapshots() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_upload_session_with_expiration(
        &state.db,
        "driver-upload",
        "active",
        "1997-01-01T00:00:00Z",
    )
    .await;
    insert_upload_session_with_expiration(
        &state.db,
        "status-changed-upload",
        "active",
        "1998-01-01T00:00:00Z",
    )
    .await;
    insert_upload_session_with_expiration(
        &state.db,
        "renewed-upload",
        "active",
        "1999-01-01T00:00:00Z",
    )
    .await;
    insert_upload_session_with_expiration(
        &state.db,
        "terminal-changed-upload",
        "failed",
        EXPIRED_AT,
    )
    .await;
    sqlx::query(
        r"
        CREATE TRIGGER mutate_later_uploads_during_sweep
        AFTER UPDATE OF status ON upload_sessions
        WHEN NEW.id = 'driver-upload' AND NEW.status = 'expired'
        BEGIN
            UPDATE upload_sessions
            SET status = 'complete'
            WHERE id = 'status-changed-upload';
            UPDATE upload_sessions
            SET expires_at = '2999-01-01T00:00:00Z'
            WHERE id = 'renewed-upload';
            UPDATE upload_sessions
            SET status = 'active'
            WHERE id = 'terminal-changed-upload';
        END
        ",
    )
    .execute(&state.db)
    .await
    .expect("stale upload snapshot trigger");
    let transfers_path = state.config.transfers_path();
    let upload_root = transfers_path.join("uploads");
    for session_id in [
        "driver-upload",
        "status-changed-upload",
        "renewed-upload",
        "terminal-changed-upload",
    ] {
        let session_dir = upload_root.join(session_id);
        tokio::fs::create_dir_all(&session_dir)
            .await
            .expect("upload scratch dir");
        tokio::fs::write(session_dir.join("00000001.part"), b"x")
            .await
            .expect("upload scratch part");
    }
    let result = sweep_state_transfers(&state, &transfers_path).await;
    let rows = upload_sweep_survivors(&state.db).await;

    assert_eq!(result.expired_uploads, vec!["driver-upload"]);
    assert!(result.deleted_uploads.is_empty());
    assert_eq!(
        rows,
        vec![
            (
                "renewed-upload".to_string(),
                "active".to_string(),
                FUTURE_AT.to_string(),
            ),
            (
                "status-changed-upload".to_string(),
                "complete".to_string(),
                "1998-01-01T00:00:00Z".to_string(),
            ),
            (
                "terminal-changed-upload".to_string(),
                "active".to_string(),
                EXPIRED_AT.to_string(),
            ),
        ]
    );
    assert!(!upload_root.join("driver-upload").exists());
    for session_id in [
        "status-changed-upload",
        "renewed-upload",
        "terminal-changed-upload",
    ] {
        assert!(upload_root.join(session_id).join("00000001.part").is_file());
    }
}

#[tokio::test]
async fn sweep_expired_exports_cancels_active_and_deletes_terminal_artifacts() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_export_job(&state.db, "queued-export", "queued").await;
    insert_export_job(&state.db, "complete-export", "complete").await;
    let (blob_id, stored) = insert_stored_blob(&state, b"expired export bytes").await;
    insert_export_artifact(&state.db, "complete-export", blob_id, &stored).await;
    let transfers_path = state.config.transfers_path();

    let result = sweep_state_transfers(&state, &transfers_path).await;
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
    assert_eq!(
        result.deleted_export_objects,
        vec![stored.object_key.clone()]
    );
    assert_eq!(queued_status, "cancelled");
    assert_eq!(complete_count, 0);
    assert_eq!(blob_count, 0);
    assert!(
        !state
            .storage
            .list_object_keys()
            .await
            .expect("object keys")
            .contains(&stored.object_key)
    );
}

#[tokio::test]
async fn sweep_expired_exports_ignores_stale_status_and_expiration_snapshots() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    insert_export_job_with_expiration(&state.db, "driver-export", "queued", "1997-01-01T00:00:00Z")
        .await;
    insert_export_job_with_expiration(
        &state.db,
        "status-changed-export",
        "running",
        "1998-01-01T00:00:00Z",
    )
    .await;
    insert_export_job_with_expiration(
        &state.db,
        "renewed-export",
        "queued",
        "1999-01-01T00:00:00Z",
    )
    .await;
    insert_export_job(&state.db, "terminal-changed-export", "complete").await;
    let (blob_id, stored) = insert_stored_blob(&state, b"stale export artifact").await;
    insert_export_artifact(&state.db, "terminal-changed-export", blob_id, &stored).await;
    sqlx::query(
        r"
        CREATE TRIGGER mutate_later_exports_during_sweep
        AFTER UPDATE OF status ON export_jobs
        WHEN NEW.id = 'driver-export' AND NEW.status = 'cancelled'
        BEGIN
            UPDATE export_jobs
            SET status = 'complete'
            WHERE id = 'status-changed-export';
            UPDATE export_jobs
            SET expires_at = '2999-01-01T00:00:00Z'
            WHERE id = 'renewed-export';
            UPDATE export_jobs
            SET status = 'cancelled'
            WHERE id = 'terminal-changed-export';
        END
        ",
    )
    .execute(&state.db)
    .await
    .expect("stale export snapshot trigger");
    let transfers_path = state.config.transfers_path();
    let export_root = transfers_path.join("exports");
    tokio::fs::create_dir_all(&export_root)
        .await
        .expect("export scratch dir");
    for job_id in [
        "driver-export",
        "status-changed-export",
        "renewed-export",
        "terminal-changed-export",
    ] {
        tokio::fs::write(export_root.join(format!("{job_id}.zip.tmp")), b"partial")
            .await
            .expect("export scratch file");
    }

    let result = sweep_state_transfers(&state, &transfers_path).await;
    let rows = export_sweep_survivors(&state.db).await;
    let (artifact_count, blob_count) =
        export_artifact_and_blob_counts(&state.db, "terminal-changed-export", blob_id).await;

    assert_eq!(result.cancelled_exports, vec!["driver-export"]);
    assert!(result.deleted_exports.is_empty());
    assert_eq!(
        rows,
        vec![
            (
                "renewed-export".to_string(),
                "queued".to_string(),
                FUTURE_AT.to_string(),
            ),
            (
                "status-changed-export".to_string(),
                "complete".to_string(),
                "1998-01-01T00:00:00Z".to_string(),
            ),
            (
                "terminal-changed-export".to_string(),
                "cancelled".to_string(),
                EXPIRED_AT.to_string(),
            ),
        ]
    );
    assert_eq!(artifact_count, 1);
    assert_eq!(blob_count, 1);
    assert!(!export_root.join("driver-export.zip.tmp").exists());
    for job_id in [
        "status-changed-export",
        "renewed-export",
        "terminal-changed-export",
    ] {
        assert!(export_root.join(format!("{job_id}.zip.tmp")).is_file());
    }
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

    let result = sweep_state_transfers(&state, &transfers_path).await;
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
