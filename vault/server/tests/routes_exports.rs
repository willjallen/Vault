use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use flate2::read::DeflateDecoder;
use serde_json::{Value, json};
use std::io::Read;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time::{sleep, timeout};
use tower::ServiceExt;
use vault_server::auth::{AuthSettings, UserContext};
use vault_server::config::Config;
use vault_server::db;
use vault_server::exports::{self, ExportSelectionItem, ExportZipOptions};
use vault_server::folders::{
    VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path, get_root_folder,
};
use vault_server::http::{self, AppState};
use vault_server::storage::{
    BlobStorageBackend, LocalBlobStorage, SharedBlobStorage, StorageError, StoredBlob,
};
use vault_server::transfers::sweep_expired_transfers;

async fn test_state() -> (AppState, tempfile::TempDir) {
    test_state_with_export_settings(86_400, 1, 3 * 1024 * 1024 * 1024, 1).await
}

async fn test_state_with_export_settings(
    export_ttl_seconds: i64,
    export_workers: i64,
    export_zip_compression_threshold_bytes: i64,
    export_zip_compresslevel: i64,
) -> (AppState, tempfile::TempDir) {
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
        export_ttl_seconds,
        export_workers,
        export_zip_compression_threshold_bytes,
        export_zip_compresslevel,
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

#[derive(Debug)]
struct BlockingPutFileStorage {
    inner: LocalBlobStorage,
    entered_put_file: Arc<Notify>,
    release_put_file: Arc<Notify>,
}

#[async_trait]
impl BlobStorageBackend for BlockingPutFileStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    async fn put_bytes(&self, data: &[u8]) -> Result<StoredBlob, StorageError> {
        self.inner.put_bytes(data).await
    }

    async fn put_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        self.entered_put_file.notify_one();
        self.release_put_file.notified().await;
        self.inner.put_file(source_path, digest, size_bytes).await
    }

    async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        self.inner.put_part_files(part_paths, expected_digest).await
    }

    async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.read_bytes(object_key).await
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        self.inner.read_range(object_key, start, end).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.inner.delete_object(object_key).await
    }
}

#[derive(Debug)]
struct BlockAfterPutFileStorage {
    inner: LocalBlobStorage,
    stored_file: Arc<Notify>,
    release_return: Arc<Notify>,
}

#[async_trait]
impl BlobStorageBackend for BlockAfterPutFileStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    async fn put_bytes(&self, data: &[u8]) -> Result<StoredBlob, StorageError> {
        self.inner.put_bytes(data).await
    }

    async fn put_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        let stored = self.inner.put_file(source_path, digest, size_bytes).await?;
        self.stored_file.notify_one();
        self.release_return.notified().await;
        Ok(stored)
    }

    async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        self.inner.put_part_files(part_paths, expected_digest).await
    }

    async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.read_bytes(object_key).await
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        self.inner.read_range(object_key, start, end).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.inner.delete_object(object_key).await
    }
}

#[derive(Debug)]
struct CancelAfterReadStorage {
    inner: LocalBlobStorage,
    pool: sqlx::SqlitePool,
    job_id: Arc<AsyncMutex<Option<String>>>,
    entered_read: Arc<Notify>,
    release_read: Arc<Notify>,
}

#[async_trait]
impl BlobStorageBackend for CancelAfterReadStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    async fn put_bytes(&self, data: &[u8]) -> Result<StoredBlob, StorageError> {
        self.inner.put_bytes(data).await
    }

    async fn put_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        self.inner.put_file(source_path, digest, size_bytes).await
    }

    async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        self.inner.put_part_files(part_paths, expected_digest).await
    }

    async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.read_bytes(object_key).await
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        self.inner.read_range(object_key, start, end).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.inner.delete_object(object_key).await
    }

    async fn read_location_bytes(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
    ) -> Result<Vec<u8>, StorageError> {
        self.entered_read.notify_one();
        self.release_read.notified().await;
        let bytes = self
            .inner
            .read_location_bytes(backend, bucket, object_key)
            .await?;
        let pool = self.pool.clone();
        let job_id = self.job_id.lock().await.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            if let Some(job_id) = job_id {
                let _ = sqlx::query(
                    r"
                    UPDATE export_jobs
                    SET status = 'cancelled',
                        cancelled_at = CURRENT_TIMESTAMP,
                        updated_at = CURRENT_TIMESTAMP
                    WHERE id = ?
                    ",
                )
                .bind(job_id)
                .execute(&pool)
                .await;
            }
        });
        Ok(bytes)
    }
}

async fn insert_stored_document(
    pool: &sqlx::SqlitePool,
    storage: &SharedBlobStorage,
    folder_id: i64,
    name: &str,
    content: &[u8],
) -> i64 {
    insert_stored_document_with_mime(pool, storage, folder_id, name, content, "text/plain").await
}

async fn insert_unversioned_document(pool: &sqlx::SqlitePool, folder_id: i64, name: &str) -> i64 {
    sqlx::query(
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
    .expect("unversioned document")
    .last_insert_rowid()
}

async fn insert_stored_document_with_mime(
    pool: &sqlx::SqlitePool,
    storage: &SharedBlobStorage,
    folder_id: i64,
    name: &str,
    content: &[u8],
    mime_type: &str,
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
            (?, ?, 'admin', 'Admin', 'admin')
        ",
    )
    .bind(folder_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("document")
    .last_insert_rowid();
    let version_id = format!("export-version-{document_id}");
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

fn authed_get_with_headers(
    uri: &str,
    user: &str,
    groups: &str,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", user)
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("request")
}

fn authed_delete(uri: &str, user: &str, groups: &str) -> Request<Body> {
    Request::builder()
        .method(Method::DELETE)
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", user)
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups)
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
        .body(Body::from(
            serde_json::to_vec(payload).expect("json payload"),
        ))
        .expect("request")
}

fn body_contains(body: &[u8], needle: &[u8]) -> bool {
    body.windows(needle.len()).any(|window| window == needle)
}

#[derive(Debug)]
struct LocalZipEntry {
    name: String,
    method: u16,
    data: Vec<u8>,
}

async fn wait_for_export_status(
    app: axum::Router,
    job_id: &str,
    user: &str,
    groups: &str,
    expected: &str,
) -> Value {
    for _ in 0..50 {
        let response = app
            .clone()
            .oneshot(authed_get(&format!("/api/exports/{job_id}"), user, groups))
            .await
            .expect("export status response");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response_json(response).await;
        if payload["status"] == expected {
            return payload;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("export {job_id} did not reach {expected}");
}

async fn wait_for_export_status_in_db(pool: &sqlx::SqlitePool, job_id: &str, expected: &str) {
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

async fn wait_for_cancelled_export_cleanup(
    pool: &sqlx::SqlitePool,
    storage: &SharedBlobStorage,
    job_id: &str,
    expected_keys: &[String],
) {
    for _ in 0..50 {
        let status: String = sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = ?")
            .bind(job_id)
            .fetch_one(pool)
            .await
            .expect("export status");
        let artifact_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
                .bind(job_id)
                .fetch_one(pool)
                .await
                .expect("artifact count");
        let orphan_blob_count: i64 = sqlx::query_scalar(
            r"
            SELECT COUNT(*)
            FROM blobs b
            WHERE NOT EXISTS (SELECT 1 FROM document_versions v WHERE v.blob_id = b.id)
              AND NOT EXISTS (SELECT 1 FROM export_artifacts a WHERE a.blob_id = b.id)
            ",
        )
        .fetch_one(pool)
        .await
        .expect("orphan blob count");
        let mut keys = storage.list_object_keys().await.expect("object keys");
        keys.sort();
        if status == "cancelled"
            && artifact_count == 0
            && orphan_blob_count == 0
            && keys == expected_keys
        {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    let mut keys = storage.list_object_keys().await.expect("final object keys");
    keys.sort();
    panic!("cancelled export left artifact/blob metadata or object keys behind: {keys:?}");
}

async fn wait_for_path_missing(path: &Path) {
    for _ in 0..50 {
        if tokio::fs::metadata(path).await.is_err() {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("path still exists: {}", path.display());
}

async fn export_artifact_location(pool: &sqlx::SqlitePool, job_id: &str) -> (i64, String) {
    sqlx::query_as(
        r"
        SELECT a.blob_id, l.object_key
        FROM export_artifacts a
        JOIN blob_locations l ON l.blob_id = a.blob_id
        WHERE a.job_id = ?
        ",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await
    .expect("artifact location")
}

async fn expire_export_job_and_artifacts(pool: &sqlx::SqlitePool, job_id: &str) {
    sqlx::query("UPDATE export_jobs SET expires_at = '2001-01-01T00:00:00Z' WHERE id = ?")
        .bind(job_id)
        .execute(pool)
        .await
        .expect("expire export job");
    sqlx::query("UPDATE export_artifacts SET expires_at = '2001-01-01T00:00:00Z' WHERE job_id = ?")
        .bind(job_id)
        .execute(pool)
        .await
        .expect("expire export artifact");
}

async fn assert_expired_export_swept(
    pool: &sqlx::SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    job_id: &str,
    blob_id: i64,
    object_key: &str,
) {
    let swept = sweep_expired_transfers(pool, storage, transfers_path)
        .await
        .expect("sweep transfers");
    assert_eq!(swept.deleted_exports, vec![job_id.to_string()]);
    assert_eq!(swept.deleted_export_objects, vec![object_key.to_string()]);
    let job_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM export_jobs WHERE id = ?")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .expect("job count");
    let blob_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE id = ?")
        .bind(blob_id)
        .fetch_one(pool)
        .await
        .expect("blob count");
    let location_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM blob_locations WHERE object_key = ?")
            .bind(object_key)
            .fetch_one(pool)
            .await
            .expect("location count");
    assert_eq!(job_count, 0);
    assert_eq!(blob_count, 0);
    assert_eq!(location_count, 0);
    assert!(
        !storage
            .list_object_keys()
            .await
            .expect("object keys")
            .contains(&object_key.to_string())
    );
}

async fn assert_export_artifact_range_response(
    app: axum::Router,
    download_url: &str,
    size_bytes: i64,
) {
    let response = app
        .oneshot(authed_get_with_headers(
            download_url,
            "reader",
            "readers",
            &[("Accept-Encoding", "gzip"), ("Range", "bytes=0-1")],
        ))
        .await
        .expect("range download response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("range body");
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-encoding"], "identity");
    assert_eq!(headers["content-range"], format!("bytes 0-1/{size_bytes}"));
    assert_eq!(body, b"PK".as_slice());
}

async fn assert_export_zip_body_contains_project_files(app: axum::Router, download_url: &str) {
    let response = app
        .oneshot(authed_get(download_url, "reader", "readers"))
        .await
        .expect("download response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("zip body");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/zip");
    assert!(
        headers["content-disposition"]
            .to_str()
            .expect("content disposition")
            .contains("filename=\"Project.zip\"")
    );
    assert!(body_contains(&body, b"Project/alpha.txt"));
    assert!(body_contains(&body, b"alpha bytes"));
    assert!(body_contains(&body, b"Project/beta.txt"));
    assert!(body_contains(&body, b"beta bytes"));
}

async fn wait_for_export_event_count(pool: &sqlx::SqlitePool, expected: i64) {
    for _ in 0..50 {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM document_events WHERE event_type = 'download' AND message LIKE 'Exported Project/%'",
        )
        .fetch_one(pool)
        .await
        .expect("export events");
        if count == expected {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("export event count did not reach {expected}");
}

fn local_zip_entries(bytes: &[u8]) -> Vec<LocalZipEntry> {
    let mut entries = Vec::new();
    let mut offset = 0_usize;
    while offset + 30 <= bytes.len() && &bytes[offset..offset + 4] == b"PK\x03\x04" {
        let method = le_u16(bytes, offset + 8);
        let compressed_size = le_u32(bytes, offset + 18) as usize;
        let name_len = le_u16(bytes, offset + 26) as usize;
        let extra_len = le_u16(bytes, offset + 28) as usize;
        let name_start = offset + 30;
        let name_end = name_start + name_len;
        let data_start = name_end + extra_len;
        let data_end = data_start + compressed_size;
        assert!(
            data_end <= bytes.len(),
            "local ZIP entry exceeds archive length"
        );
        let name = std::str::from_utf8(&bytes[name_start..name_end])
            .expect("zip entry name")
            .to_string();
        entries.push(LocalZipEntry {
            name,
            method,
            data: bytes[data_start..data_end].to_vec(),
        });
        offset = data_end;
    }
    entries
}

fn zip_entry_payload(entry: &LocalZipEntry) -> Vec<u8> {
    match entry.method {
        0 => entry.data.clone(),
        8 => {
            let mut decoder = DeflateDecoder::new(entry.data.as_slice());
            let mut output = Vec::new();
            decoder.read_to_end(&mut output).expect("deflated entry");
            output
        }
        other => panic!("unexpected ZIP compression method {other}"),
    }
}

fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[tokio::test]
async fn export_job_creates_downloadable_zip_for_folder() {
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
    insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "alpha.txt",
        b"alpha bytes",
    )
    .await;
    insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "beta.txt",
        b"beta bytes",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "reader",
            "readers",
            &json!({
                "items": [
                    {"type": "folder", "id": project.id}
                ]
            }),
        ))
        .await
        .expect("export response");
    let status = response.status();
    let payload = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "queued");
    assert_eq!(payload["filename"], "Project.zip");
    assert_eq!(payload["total_items"], 2);
    assert_eq!(payload["processed_items"], 0);
    assert!(payload["download_url"].is_null());

    let completed = wait_for_export_status(
        app.clone(),
        payload["id"].as_str().expect("id"),
        "reader",
        "readers",
        "complete",
    )
    .await;
    assert_eq!(completed["processed_items"], 2);
    assert_eq!(
        completed["download_url"],
        format!(
            "/api/exports/{}/download",
            completed["id"].as_str().expect("id")
        )
    );
    assert!(completed["size_bytes"].as_i64().expect("zip size") > 0);
    assert_export_artifact_range_response(
        app.clone(),
        completed["download_url"].as_str().expect("download url"),
        completed["size_bytes"].as_i64().expect("zip size"),
    )
    .await;
    assert_export_zip_body_contains_project_files(
        app,
        completed["download_url"].as_str().expect("download url"),
    )
    .await;

    let artifact_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM export_artifacts")
        .fetch_one(&pool)
        .await
        .expect("artifact count");
    assert_eq!(artifact_count, 1);
    wait_for_export_event_count(&pool, 2).await;
}

#[tokio::test]
async fn export_job_prunes_child_documents_from_folder_selection() {
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
    let document_id = insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "nested.txt",
        b"nested bytes",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/exports",
            "reader",
            "readers",
            &json!({
                "items": [
                    {"type": "folder", "id": project.id},
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("export response");
    let status = response.status();
    let payload = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["filename"], "Project.zip");
    assert_eq!(payload["total_items"], 1);

    let request_payload_json =
        sqlx::query_scalar::<_, String>("SELECT request_payload FROM export_jobs WHERE id = ?")
            .bind(payload["id"].as_str().expect("job id"))
            .fetch_one(&pool)
            .await
            .expect("request payload");
    let request_payload: Value =
        serde_json::from_str(&request_payload_json).expect("stored request payload");
    assert_eq!(
        request_payload["items"],
        json!([{"type": "folder", "id": project.id, "path": "Project"}])
    );
}

#[tokio::test]
async fn folder_export_excludes_inaccessible_descendant_documents() {
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
    let private = get_or_create_folder_path(&state.db, Some("Project/Private"))
        .await
        .expect("private");
    add_folder_permission(&state.db, project.id, readers, true, true, false)
        .await
        .expect("reader project");
    add_folder_permission(&state.db, private.id, confidential, true, true, false)
        .await
        .expect("confidential private");
    insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "visible.txt",
        b"visible",
    )
    .await;
    insert_stored_document(
        &state.db,
        &state.storage,
        private.id,
        "secret.txt",
        b"secret",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "reader",
            "readers",
            &json!({
                "items": [
                    {"type": "folder", "id": project.id}
                ]
            }),
        ))
        .await
        .expect("export response");
    let status = response.status();
    let payload = response_json(response).await;

    assert_eq!(status, StatusCode::OK, "{payload}");
    assert_eq!(payload["total_items"], 1);
    let completed = wait_for_export_status(
        app.clone(),
        payload["id"].as_str().expect("id"),
        "reader",
        "readers",
        "complete",
    )
    .await;
    assert_eq!(completed["processed_items"], 1);

    let download = app
        .oneshot(authed_get(
            completed["download_url"].as_str().expect("download url"),
            "reader",
            "readers",
        ))
        .await
        .expect("download response");
    assert_eq!(download.status(), StatusCode::OK);
    let zip_body = to_bytes(download.into_body(), usize::MAX)
        .await
        .expect("zip body");

    assert!(body_contains(&zip_body, b"Project/visible.txt"));
    assert!(body_contains(&zip_body, b"visible"));
    assert!(!body_contains(&zip_body, b"Project/Private/secret.txt"));
    assert!(!body_contains(&zip_body, b"secret"));
    wait_for_export_event_count(&pool, 1).await;
}

#[tokio::test]
async fn api_download_multi_selection_returns_accepted_export_job() {
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
    let first = insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "one.txt",
        b"one bytes",
    )
    .await;
    let second = insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "two.txt",
        b"two bytes",
    )
    .await;
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/download",
            "reader",
            "readers",
            &json!({
                "items": [
                    {"type": "document", "id": first},
                    {"type": "document", "id": second}
                ]
            }),
        ))
        .await
        .expect("api download response");
    let status = response.status();
    let payload = response_json(response).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(payload["status"], "queued");
    assert_eq!(payload["filename"], "vault-download.zip");
    assert_eq!(payload["total_items"], 0);
    assert_eq!(payload["total_bytes"], 0);
    assert!(payload["download_url"].is_null());

    let completed = wait_for_export_status(
        app,
        payload["id"].as_str().expect("id"),
        "reader",
        "readers",
        "complete",
    )
    .await;
    assert_eq!(completed["total_items"], 2);
    assert_eq!(completed["processed_items"], 2);
    assert!(
        completed["download_url"]
            .as_str()
            .expect("download url")
            .starts_with("/api/exports/")
    );
}

#[tokio::test]
async fn api_download_empty_folder_completes_as_empty_zip() {
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
    let app = http::router(state);

    let rejected_export = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "reader",
            "readers",
            &json!({"items": [{"type": "folder", "id": project.id}]}),
        ))
        .await
        .expect("export response");
    let rejected_export_status = rejected_export.status();
    let rejected_export_payload = response_json(rejected_export).await;
    assert_eq!(rejected_export_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        rejected_export_payload["detail"],
        "export has no downloadable files",
    );

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/download",
            "reader",
            "readers",
            &json!({"items": [{"type": "folder", "id": project.id}]}),
        ))
        .await
        .expect("api download response");
    let status = response.status();
    let payload = response_json(response).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(payload["status"], "queued");
    assert_eq!(payload["filename"], "Project.zip");
    assert_eq!(payload["total_items"], 0);
    assert_eq!(payload["total_bytes"], 0);
    assert!(payload["download_url"].is_null());

    let completed = wait_for_export_status(
        app.clone(),
        payload["id"].as_str().expect("id"),
        "reader",
        "readers",
        "complete",
    )
    .await;
    assert_eq!(completed["processed_items"], 0);
    assert_eq!(completed["processed_bytes"], 0);

    let download = app
        .oneshot(authed_get(
            completed["download_url"].as_str().expect("download url"),
            "reader",
            "readers",
        ))
        .await
        .expect("download response");
    let download_status = download.status();
    let zip_body = to_bytes(download.into_body(), usize::MAX)
        .await
        .expect("zip body");

    assert_eq!(download_status, StatusCode::OK);
    assert_eq!(local_zip_entries(&zip_body).len(), 0);
    assert_eq!(&zip_body[..4], b"PK\x05\x06");
    assert_eq!(zip_body.len(), 22);
}

#[tokio::test]
async fn api_download_folder_selection_excludes_inaccessible_descendants() {
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
    let private = get_or_create_folder_path(&state.db, Some("Project/Private"))
        .await
        .expect("private");
    add_folder_permission(&state.db, project.id, readers, true, true, false)
        .await
        .expect("reader project");
    add_folder_permission(&state.db, private.id, confidential, true, true, false)
        .await
        .expect("confidential private");
    insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "visible.txt",
        b"visible",
    )
    .await;
    insert_stored_document(
        &state.db,
        &state.storage,
        private.id,
        "secret.txt",
        b"secret",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/download",
            "reader",
            "readers",
            &json!({"items": [{"type": "folder", "id": project.id}]}),
        ))
        .await
        .expect("api download response");
    let status = response.status();
    let payload = response_json(response).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(payload["status"], "queued");
    assert_eq!(payload["filename"], "Project.zip");
    assert_eq!(payload["total_items"], 0);
    assert_eq!(payload["total_bytes"], 0);
    assert!(payload["download_url"].is_null());

    let completed = wait_for_export_status(
        app.clone(),
        payload["id"].as_str().expect("id"),
        "reader",
        "readers",
        "complete",
    )
    .await;
    assert_eq!(completed["total_items"], 1);
    assert_eq!(completed["processed_items"], 1);

    let download = app
        .oneshot(authed_get(
            completed["download_url"].as_str().expect("download url"),
            "reader",
            "readers",
        ))
        .await
        .expect("download response");
    let download_status = download.status();
    let zip_body = to_bytes(download.into_body(), usize::MAX)
        .await
        .expect("zip body");
    let entries = local_zip_entries(&zip_body);

    assert_eq!(download_status, StatusCode::OK);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "Project/visible.txt");
    assert_eq!(zip_entry_payload(&entries[0]), b"visible");
    assert!(!body_contains(&zip_body, b"Project/Private/secret.txt"));
    assert!(!body_contains(&zip_body, b"secret"));
    wait_for_export_event_count(&pool, 1).await;
}

#[tokio::test]
async fn export_job_counts_readable_unversioned_documents_before_worker_skips_them() {
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
    let document_id = insert_unversioned_document(&state.db, project.id, "draft.txt").await;
    let app = http::router(state);

    let direct = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "reader",
            "readers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("direct export response");
    let direct_status = direct.status();
    let direct_payload = response_json(direct).await;

    assert_eq!(direct_status, StatusCode::OK);
    assert_eq!(direct_payload["status"], "queued");
    assert_eq!(direct_payload["total_items"], 1);
    assert_eq!(direct_payload["total_bytes"], 0);

    let direct_completed = wait_for_export_status(
        app.clone(),
        direct_payload["id"].as_str().expect("id"),
        "reader",
        "readers",
        "complete",
    )
    .await;
    assert_eq!(direct_completed["total_items"], 0);
    assert_eq!(direct_completed["processed_items"], 0);

    let direct_download = app
        .clone()
        .oneshot(authed_get(
            direct_completed["download_url"]
                .as_str()
                .expect("download url"),
            "reader",
            "readers",
        ))
        .await
        .expect("direct artifact response");
    let direct_zip = to_bytes(direct_download.into_body(), usize::MAX)
        .await
        .expect("direct zip");
    assert_eq!(local_zip_entries(&direct_zip).len(), 0);

    let folder = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "reader",
            "readers",
            &json!({"items": [{"type": "folder", "id": project.id}]}),
        ))
        .await
        .expect("folder export response");
    let folder_status = folder.status();
    let folder_payload = response_json(folder).await;

    assert_eq!(folder_status, StatusCode::OK);
    assert_eq!(folder_payload["filename"], "Project.zip");
    assert_eq!(folder_payload["total_items"], 1);
    assert_eq!(folder_payload["total_bytes"], 0);

    let folder_completed = wait_for_export_status(
        app,
        folder_payload["id"].as_str().expect("id"),
        "reader",
        "readers",
        "complete",
    )
    .await;
    assert_eq!(folder_completed["total_items"], 0);
    assert_eq!(folder_completed["processed_items"], 0);
}

#[tokio::test]
async fn api_download_multi_selection_defers_inconsistent_version_failure_to_worker() {
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
    let corrupt = insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "corrupt.txt",
        b"corrupt bytes",
    )
    .await;
    let valid = insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "valid.txt",
        b"valid bytes",
    )
    .await;
    sqlx::query("UPDATE documents SET current_version_id = 'missing-version' WHERE id = ?")
        .bind(corrupt)
        .execute(&state.db)
        .await
        .expect("corrupt current version");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/download",
            "reader",
            "readers",
            &json!({
                "items": [
                    {"type": "document", "id": corrupt},
                    {"type": "document", "id": valid}
                ]
            }),
        ))
        .await
        .expect("download response");
    let status = response.status();
    let payload = response_json(response).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(payload["status"], "queued");
    assert_eq!(payload["total_items"], 0);
    assert_eq!(payload["total_bytes"], 0);

    let failed = wait_for_export_status(
        app,
        payload["id"].as_str().expect("id"),
        "reader",
        "readers",
        "failed",
    )
    .await;
    assert_eq!(
        failed["error"],
        "current document version metadata is inconsistent",
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?",)
            .bind(payload["id"].as_str().expect("id"))
            .fetch_one(&pool)
            .await
            .expect("artifact count"),
        0,
    );
}

#[tokio::test]
async fn export_job_rejects_visible_only_document_without_queueing_work() {
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
    let document_id = insert_stored_document(
        &state.db,
        &state.storage,
        project.id,
        "private.txt",
        b"private bytes",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/exports",
            "viewer",
            "viewers",
            &json!({
                "items": [
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("export response");
    let status = response.status();
    let payload = response_json(response).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(payload["detail"], "Insufficient document access");
    for table in [
        "export_jobs",
        "export_artifacts",
        "document_events",
        "state_events",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .expect("row count");
        assert_eq!(count, 0, "{table} should stay empty");
    }
}

#[tokio::test]
async fn export_routes_hide_other_users_jobs_and_cancel_queued_jobs() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    sqlx::query(
        r"
        INSERT INTO vault_users (id, issuer, subject, email, name)
        VALUES (42, 'headers', 'owner', 'owner@example.com', 'owner')
        ",
    )
    .execute(&state.db)
    .await
    .expect("owner user");
    sqlx::query(
        r"
        INSERT INTO export_jobs
            (id, status, filename, total_items, created_by, created_by_name, user_context, expires_at)
        VALUES
            ('queued-export', 'queued', 'queued.zip', 1, '42', 'owner', '{}', '2999-01-01T00:00:00Z')
        ",
    )
    .execute(&state.db)
    .await
    .expect("queued export");
    let pool = state.db.clone();
    let app = http::router(state);

    let hidden = app
        .clone()
        .oneshot(authed_get("/api/exports/queued-export", "other", "readers"))
        .await
        .expect("hidden response");
    let cancelled = app
        .clone()
        .oneshot(authed_delete(
            "/api/exports/queued-export",
            "owner",
            "readers",
        ))
        .await
        .expect("cancel response");
    let cancel_status = cancelled.status();
    let cancel_payload = response_json(cancelled).await;

    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    assert_eq!(cancel_status, StatusCode::OK);
    assert_eq!(cancel_payload["status"], "cancelled");
    let download = app
        .oneshot(authed_get(
            "/api/exports/queued-export/download",
            "owner",
            "readers",
        ))
        .await
        .expect("cancelled download response");
    let download_status = download.status();
    let download_payload = response_json(download).await;
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
            .bind("queued-export")
            .fetch_one(&pool)
            .await
            .expect("artifact count");
    assert_eq!(download_status, StatusCode::CONFLICT);
    assert_eq!(download_payload["detail"], "Export is not complete");
    assert_eq!(artifact_count, 0);
}

#[tokio::test]
async fn export_job_reports_finalizing_while_artifact_is_promoted() {
    let (mut state, _temp_dir) = test_state().await;
    let entered_put_file = Arc::new(Notify::new());
    let release_put_file = Arc::new(Notify::new());
    state.storage = Arc::new(BlockingPutFileStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
        entered_put_file: entered_put_file.clone(),
        release_put_file: release_put_file.clone(),
    });
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let document_id = insert_stored_document(
        &state.db,
        &state.storage,
        root.id,
        "finalizing.txt",
        b"export bytes",
    )
    .await;
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "admin",
            "vault-admin",
            &json!({
                "items": [
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("export response");
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    let job_id = payload["id"].as_str().expect("job id").to_string();

    timeout(Duration::from_secs(5), entered_put_file.notified())
        .await
        .expect("export artifact promotion should begin");
    let status_response = app
        .clone()
        .oneshot(authed_get(
            &format!("/api/exports/{job_id}"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("status response");
    assert_eq!(status_response.status(), StatusCode::OK);
    let status_payload = response_json(status_response).await;
    assert_eq!(status_payload["status"], "finalizing");

    release_put_file.notify_one();
    let completed = wait_for_export_status(app, &job_id, "admin", "vault-admin", "complete").await;
    assert_eq!(completed["status"], "complete");
}

#[tokio::test]
async fn cancelled_export_cleans_object_promoted_before_artifact_metadata() {
    let (mut state, _temp_dir) = test_state().await;
    let stored_file = Arc::new(Notify::new());
    let release_return = Arc::new(Notify::new());
    state.storage = Arc::new(BlockAfterPutFileStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
        stored_file: stored_file.clone(),
        release_return: release_return.clone(),
    });
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let document_id = insert_stored_document(
        &state.db,
        &state.storage,
        root.id,
        "cancelled-finalize.txt",
        b"source bytes",
    )
    .await;
    let mut expected_keys = state
        .storage
        .list_object_keys()
        .await
        .expect("initial keys");
    expected_keys.sort();
    let pool = state.db.clone();
    let storage = state.storage.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "admin",
            "vault-admin",
            &json!({
                "items": [
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("export response");
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    let job_id = payload["id"].as_str().expect("job id").to_string();

    timeout(Duration::from_secs(5), stored_file.notified())
        .await
        .expect("export artifact should be stored before the race is released");
    let cancelled = app
        .clone()
        .oneshot(authed_delete(
            &format!("/api/exports/{job_id}"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("cancel response");
    assert_eq!(cancelled.status(), StatusCode::OK);
    assert_eq!(response_json(cancelled).await["status"], "cancelled");

    release_return.notify_one();
    wait_for_cancelled_export_cleanup(&pool, &storage, &job_id, &expected_keys).await;
}

#[tokio::test]
async fn cancelled_export_cleans_blob_metadata_created_before_artifact_metadata() {
    let (mut state, _temp_dir) = test_state().await;
    let entered_put_file = Arc::new(Notify::new());
    let release_put_file = Arc::new(Notify::new());
    state.storage = Arc::new(BlockingPutFileStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
        entered_put_file: entered_put_file.clone(),
        release_put_file: release_put_file.clone(),
    });
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let document_id = insert_stored_document(
        &state.db,
        &state.storage,
        root.id,
        "cancelled-metadata.txt",
        b"canonical source bytes",
    )
    .await;
    let mut expected_keys = state
        .storage
        .list_object_keys()
        .await
        .expect("initial keys");
    expected_keys.sort();
    let pool = state.db.clone();
    let storage = state.storage.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "admin",
            "vault-admin",
            &json!({
                "items": [
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("export response");
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    let job_id = payload["id"].as_str().expect("job id").to_string();

    timeout(Duration::from_secs(5), entered_put_file.notified())
        .await
        .expect("artifact promotion should be blocked before storage write");
    sqlx::query(&format!(
        r"
        CREATE TRIGGER cancel_export_after_artifact_blob_location
        AFTER INSERT ON blob_locations
        BEGIN
            UPDATE export_jobs
            SET status = 'cancelled',
                cancelled_at = CURRENT_TIMESTAMP,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = '{}';
        END
        ",
        job_id.replace('\'', "''")
    ))
    .execute(&pool)
    .await
    .expect("install cancellation trigger");

    release_put_file.notify_one();
    wait_for_cancelled_export_cleanup(&pool, &storage, &job_id, &expected_keys).await;
}

#[tokio::test]
async fn cancelled_export_during_large_entry_write_cleans_partial_zip() {
    let (mut state, _temp_dir) = test_state().await;
    let job_id_slot = Arc::new(AsyncMutex::new(None));
    let entered_read = Arc::new(Notify::new());
    let release_read = Arc::new(Notify::new());
    state.storage = Arc::new(CancelAfterReadStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
        pool: state.db.clone(),
        job_id: job_id_slot.clone(),
        entered_read: entered_read.clone(),
        release_read: release_read.clone(),
    });
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let data = vec![b'x'; 20 * 1024 * 1024];
    let document_id =
        insert_stored_document(&state.db, &state.storage, root.id, "large.bin", &data).await;
    let mut expected_keys = state
        .storage
        .list_object_keys()
        .await
        .expect("initial keys");
    expected_keys.sort();
    let pool = state.db.clone();
    let storage = state.storage.clone();
    let transfers_path = state.config.transfers_path();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/exports",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("export response");
    assert_eq!(response.status(), StatusCode::OK);
    let job_id = response_json(response).await["id"]
        .as_str()
        .expect("job id")
        .to_string();
    *job_id_slot.lock().await = Some(job_id.clone());

    timeout(Duration::from_secs(5), entered_read.notified())
        .await
        .expect("export should begin reading the large entry");
    release_read.notify_one();
    wait_for_cancelled_export_cleanup(&pool, &storage, &job_id, &expected_keys).await;
    wait_for_path_missing(
        &transfers_path
            .join("exports")
            .join(format!("{job_id}.zip.tmp")),
    )
    .await;
}

#[tokio::test]
async fn expired_export_artifact_download_returns_gone_and_sweep_deletes_artifact() {
    let (state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let document_id = insert_stored_document(
        &state.db,
        &state.storage,
        root.id,
        "expired-export.txt",
        b"expired export bytes",
    )
    .await;
    let pool = state.db.clone();
    let storage = state.storage.clone();
    let transfers_path = state.config.transfers_path();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "admin",
            "vault-admin",
            &json!({
                "items": [
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("export response");
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    let job_id = payload["id"].as_str().expect("job id").to_string();
    let completed =
        wait_for_export_status(app.clone(), &job_id, "admin", "vault-admin", "complete").await;
    let download_url = completed["download_url"].as_str().expect("download url");
    let (blob_id, object_key) = export_artifact_location(&pool, &job_id).await;
    expire_export_job_and_artifacts(&pool, &job_id).await;

    let expired_download = app
        .oneshot(authed_get(download_url, "admin", "vault-admin"))
        .await
        .expect("expired download response");
    let expired_status = expired_download.status();
    let expired_body = response_json(expired_download).await;
    assert_eq!(expired_status, StatusCode::GONE);
    assert_eq!(expired_body["detail"], "Export expired");

    assert_expired_export_swept(
        &pool,
        &storage,
        &transfers_path,
        &job_id,
        blob_id,
        &object_key,
    )
    .await;
}

#[tokio::test]
async fn export_route_uses_configured_ttl_for_created_jobs() {
    let before = OffsetDateTime::now_utc();
    let (state, _temp_dir) =
        test_state_with_export_settings(120, 1, 3 * 1024 * 1024 * 1024, 1).await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let document_id =
        insert_stored_document(&state.db, &state.storage, root.id, "ttl.txt", b"ttl bytes").await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/exports",
            "admin",
            "vault-admin",
            &json!({
                "items": [
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("export response");
    let status = response.status();
    let payload = response_json(response).await;
    let expires_at = OffsetDateTime::parse(
        payload["expires_at"].as_str().expect("expires_at"),
        &Rfc3339,
    )
    .expect("expires_at timestamp");
    let ttl_seconds = (expires_at - before).whole_seconds();

    assert_eq!(status, StatusCode::OK);
    assert!(
        (110..=130).contains(&ttl_seconds),
        "expected configured 120s export TTL, got {ttl_seconds}s"
    );
}

#[tokio::test]
async fn export_route_uses_configured_zip_compression_settings() {
    let (state, _temp_dir) = test_state_with_export_settings(86_400, 1, 1, 1).await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let text = "configured route compression\n".repeat(4096).into_bytes();
    let document_id = insert_stored_document_with_mime(
        &state.db,
        &state.storage,
        root.id,
        "route-compressible.txt",
        &text,
        "text/plain",
    )
    .await;
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "admin",
            "vault-admin",
            &json!({
                "items": [
                    {"type": "document", "id": document_id}
                ]
            }),
        ))
        .await
        .expect("export response");
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    let completed = wait_for_export_status(
        app.clone(),
        payload["id"].as_str().expect("id"),
        "admin",
        "vault-admin",
        "complete",
    )
    .await;

    let download = app
        .oneshot(authed_get(
            completed["download_url"].as_str().expect("download url"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("download response");
    assert_eq!(download.status(), StatusCode::OK);
    let zip_body = to_bytes(download.into_body(), usize::MAX)
        .await
        .expect("zip body");
    let entries = local_zip_entries(&zip_body);
    let entry = entries
        .iter()
        .find(|entry| entry.name == "route-compressible.txt")
        .expect("zip entry");

    assert_eq!(entry.method, 8);
    assert_eq!(zip_entry_payload(entry), text);
}

#[tokio::test]
async fn export_runtime_settings_are_normalized_in_app_state() {
    let (state, _temp_dir) = test_state_with_export_settings(10, -2, -1, 12).await;
    let settings = state.export_execution.settings();

    assert_eq!(settings.ttl_seconds, 60);
    assert_eq!(settings.workers, 1);
    assert_eq!(settings.zip_options.compression_threshold_bytes, 0);
    assert_eq!(settings.zip_options.compresslevel, 9);
}

#[tokio::test]
async fn export_zip_deflates_text_and_stores_precompressed_entries_when_threshold_allows() {
    let (state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let text = "hello vault export\n".repeat(8192).into_bytes();
    let png = vec![0_u8; 8192];
    let text_id = insert_stored_document_with_mime(
        &state.db,
        &state.storage,
        root.id,
        "notes.txt",
        &text,
        "text/plain",
    )
    .await;
    let png_id = insert_stored_document_with_mime(
        &state.db,
        &state.storage,
        root.id,
        "preview.png",
        &png,
        "image/png",
    )
    .await;
    let user = UserContext {
        id: "admin".to_string(),
        vault_user_id: 0,
        issuer: "headers".to_string(),
        subject: "admin".to_string(),
        name: "Admin".to_string(),
        email: "admin@example.com".to_string(),
        groups: Vec::new(),
        is_admin: true,
    };

    let payload = exports::create_export_job_with_options(
        &state.db,
        &state.storage,
        &state.config.transfers_path(),
        &[
            ExportSelectionItem::Document { id: text_id },
            ExportSelectionItem::Document { id: png_id },
        ],
        &user,
        ExportZipOptions {
            compression_threshold_bytes: 1,
            compresslevel: 1,
        },
    )
    .await
    .expect("export job");
    wait_for_export_status_in_db(&state.db, &payload.id, "complete").await;

    let object_key = sqlx::query_scalar::<_, String>(
        r"
        SELECT l.object_key
        FROM export_artifacts a
        JOIN blob_locations l ON l.blob_id = a.blob_id
        WHERE a.job_id = ?
        ",
    )
    .bind(&payload.id)
    .fetch_one(&state.db)
    .await
    .expect("artifact location");
    let zip_bytes = state
        .storage
        .read_bytes(&object_key)
        .await
        .expect("zip bytes");
    let entries = local_zip_entries(&zip_bytes);
    let notes = entries
        .iter()
        .find(|entry| entry.name == "notes.txt")
        .expect("notes entry");
    let preview = entries
        .iter()
        .find(|entry| entry.name == "preview.png")
        .expect("preview entry");

    assert_eq!(notes.method, 8);
    assert_eq!(zip_entry_payload(notes), text);
    assert_eq!(preview.method, 0);
    assert_eq!(zip_entry_payload(preview), png);
}
