use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Method, Request, StatusCode};
use crc::{CRC_32_ISO_HDLC, Crc};
use flate2::read::DeflateDecoder;
use futures_util::{Stream, StreamExt, future::join_all, stream};
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
use vault_server::exports::{self, ExportSelectionItem};
use vault_server::folders::{
    VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path, get_root_folder,
};
use vault_server::http::{self, AppState};
use vault_server::storage::{
    BlobByteStream, BlobReadRange, BlobStorageBackend, BlobWriteKind, LocalBlobStorage,
    STORAGE_CHUNK_SIZE, SharedBlobStorage, StorageError, StoredBlob,
};
use vault_server::transfers::{TransferMaintenanceCoordinator, sweep_expired_transfers};
use vault_server::uploads::UploadHashCoordinator;

async fn test_state() -> (AppState, tempfile::TempDir) {
    test_state_with_export_settings(86_400, 1, 3 * 1024 * 1024 * 1024, 1).await
}

async fn test_state_with_export_settings(
    export_ttl_seconds: i64,
    export_workers: i64,
    export_zip_compression_threshold_bytes: i64,
    export_zip_compresslevel: i64,
) -> (AppState, tempfile::TempDir) {
    test_state_with_export_limits(
        export_ttl_seconds,
        export_workers,
        export_zip_compression_threshold_bytes,
        export_zip_compresslevel,
        256,
        16,
    )
    .await
}

async fn test_state_with_export_limits(
    export_ttl_seconds: i64,
    export_workers: i64,
    export_zip_compression_threshold_bytes: i64,
    export_zip_compresslevel: i64,
    export_max_active_jobs: i64,
    export_max_active_jobs_per_user: i64,
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
        export_max_active_jobs,
        export_max_active_jobs_per_user,
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
struct FailoverExportStorage {
    inner: LocalBlobStorage,
    stream_calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl BlobStorageBackend for FailoverExportStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
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

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.stream_calls
            .lock()
            .expect("export failover calls")
            .push(object_key.to_string());
        match object_key {
            "bad-first-source" => Ok(Box::pin(stream::once(async {
                Err(StorageError::Remote(
                    "injected export source first-frame failure".to_string(),
                ))
            }))),
            "missing-artifact" => Err(StorageError::NotFound),
            _ => self.inner.stream_range(object_key, range).await,
        }
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.inner.delete_object(object_key).await
    }
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

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
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

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner.stream_range(object_key, range).await
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

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
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

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner.stream_range(object_key, range).await
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

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
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

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner.stream_range(object_key, range).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.inner.delete_object(object_key).await
    }

    async fn read_location_bytes(
        &self,
        _backend: &str,
        _bucket: &str,
        _object_key: &str,
    ) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "export must use the bounded blob stream".to_string(),
        ))
    }

    async fn stream_location_range(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        let source = self
            .inner
            .stream_location_range(backend, bucket, object_key, range)
            .await?;
        let pool = self.pool.clone();
        let job_id = self.job_id.clone();
        let entered_read = self.entered_read.clone();
        let release_read = self.release_read.clone();
        Ok(Box::pin(stream::unfold(
            (source, 0_usize, false),
            move |(mut source, mut frames_read, mut cancelled)| {
                let pool = pool.clone();
                let job_id = job_id.clone();
                let entered_read = entered_read.clone();
                let release_read = release_read.clone();
                async move {
                    let item = source.next().await?;
                    frames_read += 1;
                    if frames_read == 2 && !cancelled {
                        entered_read.notify_one();
                        release_read.notified().await;
                        let job_id_value = job_id.lock().await.clone();
                        if let Some(job_id) = job_id_value
                            && let Err(error) = sqlx::query(
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
                            .await
                        {
                            return Some((
                                Err(StorageError::Remote(format!(
                                    "failed to cancel export job: {error}"
                                ))),
                                (source, frames_read, true),
                            ));
                        }
                        cancelled = true;
                    }
                    Some((item, (source, frames_read, cancelled)))
                }
            },
        )))
    }
}

#[derive(Debug)]
struct BlockAfterProgressRangeStorage {
    inner: LocalBlobStorage,
    entered_after_progress: Arc<Notify>,
    release_range: Arc<Notify>,
}

#[async_trait]
impl BlobStorageBackend for BlockAfterProgressRangeStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
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

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner.stream_range(object_key, range).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.inner.delete_object(object_key).await
    }

    async fn stream_location_range(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        let source = self
            .inner
            .stream_location_range(backend, bucket, object_key, range)
            .await?;
        let entered_after_progress = self.entered_after_progress.clone();
        let release_range = self.release_range.clone();
        Ok(Box::pin(stream::unfold(
            (source, 0_usize),
            move |(mut source, mut frames_read)| {
                let entered_after_progress = entered_after_progress.clone();
                let release_range = release_range.clone();
                async move {
                    let item = source.next().await?;
                    frames_read += 1;
                    if frames_read == 34 {
                        entered_after_progress.notify_one();
                        release_range.notified().await;
                    }
                    Some((item, (source, frames_read)))
                }
            },
        )))
    }
}

#[derive(Debug)]
struct StreamOnlyExportStorage {
    inner: LocalBlobStorage,
}

#[async_trait]
impl BlobStorageBackend for StreamOnlyExportStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
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

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner.stream_range(object_key, range).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.inner.delete_object(object_key).await
    }

    async fn read_location_bytes(
        &self,
        _backend: &str,
        _bucket: &str,
        _object_key: &str,
    ) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "export must not buffer an entire source blob".to_string(),
        ))
    }

    async fn read_location_range(
        &self,
        _backend: &str,
        _bucket: &str,
        _object_key: &str,
        _start: u64,
        _end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "export must use the canonical blob stream".to_string(),
        ))
    }

    async fn stream_location_range(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner
            .stream_location_range(backend, bucket, object_key, range)
            .await
    }
}

struct PendingOpenDropGuard {
    dropped: Arc<AtomicBool>,
    dropped_notify: Arc<Notify>,
}

impl Drop for PendingOpenDropGuard {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
        self.dropped_notify.notify_one();
    }
}

#[derive(Debug)]
struct PendingOpenStorage {
    inner: LocalBlobStorage,
    source_object_key: String,
    source_size: u64,
    entered_open: Arc<Notify>,
    open_call_count: Arc<AtomicUsize>,
    open_dropped: Arc<AtomicBool>,
    open_dropped_notify: Arc<Notify>,
    legacy_read_count: Arc<AtomicUsize>,
    frame_poll_count: Arc<AtomicUsize>,
}

#[async_trait]
impl BlobStorageBackend for PendingOpenStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
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
        if object_key == self.source_object_key {
            self.legacy_read_count.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::UnsupportedOperation(
                "pending-open source rejects whole-object reads".to_string(),
            ));
        }
        self.inner.read_bytes(object_key).await
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        if object_key == self.source_object_key {
            self.legacy_read_count.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::UnsupportedOperation(
                "pending-open source rejects buffered range reads".to_string(),
            ));
        }
        self.inner.read_range(object_key, start, end).await
    }

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner.stream_range(object_key, range).await
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
        if object_key == self.source_object_key {
            self.legacy_read_count.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::UnsupportedOperation(
                "pending-open source rejects whole-object reads".to_string(),
            ));
        }
        self.inner
            .read_location_bytes(backend, bucket, object_key)
            .await
    }

    async fn read_location_range(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        if object_key == self.source_object_key {
            self.legacy_read_count.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::UnsupportedOperation(
                "pending-open source rejects buffered range reads".to_string(),
            ));
        }
        self.inner
            .read_location_range(backend, bucket, object_key, start, end)
            .await
    }

    async fn stream_location_range(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        if object_key != self.source_object_key {
            return self
                .inner
                .stream_location_range(backend, bucket, object_key, range)
                .await;
        }
        self.require_location(backend, bucket)?;
        if range
            != (BlobReadRange {
                expected_size: self.source_size,
                offset: 0,
                length: self.source_size,
            })
        {
            return Err(StorageError::InvalidRange);
        }
        self.open_call_count.fetch_add(1, Ordering::SeqCst);
        let _guard = PendingOpenDropGuard {
            dropped: self.open_dropped.clone(),
            dropped_notify: self.open_dropped_notify.clone(),
        };
        self.entered_open.notify_one();
        std::future::pending::<()>().await;
        let frame_poll_count = self.frame_poll_count.clone();
        Ok(Box::pin(stream::poll_fn(move |_context| {
            frame_poll_count.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(None::<Result<Bytes, StorageError>>)
        })))
    }
}

const LOGICAL_LARGE_SOURCE_SIZE: i64 = 5 * 1024 * 1024 * 1024;
const LOGICAL_LARGE_PREFIX_FRAMES: usize = 8;

struct LogicalLargeSourceStream {
    frames_emitted: usize,
    frame_count: Arc<AtomicUsize>,
    poll_count: Arc<AtomicUsize>,
    entered_pending: Arc<Notify>,
    pending_notified: bool,
    dropped: Arc<AtomicBool>,
    dropped_notify: Arc<Notify>,
}

impl Stream for LogicalLargeSourceStream {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_count.fetch_add(1, Ordering::SeqCst);
        if self.frames_emitted < LOGICAL_LARGE_PREFIX_FRAMES {
            self.frames_emitted += 1;
            self.frame_count.fetch_add(1, Ordering::SeqCst);
            return Poll::Ready(Some(Ok(Bytes::from(vec![b'x'; STORAGE_CHUNK_SIZE]))));
        }
        if !self.pending_notified {
            self.pending_notified = true;
            self.entered_pending.notify_one();
        }
        Poll::Pending
    }
}

impl Drop for LogicalLargeSourceStream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
        self.dropped_notify.notify_one();
    }
}

#[derive(Debug)]
struct LogicalLargeCancelStorage {
    inner: LocalBlobStorage,
    logical_object_key: String,
    legacy_read_count: Arc<AtomicUsize>,
    frame_count: Arc<AtomicUsize>,
    poll_count: Arc<AtomicUsize>,
    entered_pending: Arc<Notify>,
    dropped: Arc<AtomicBool>,
    dropped_notify: Arc<Notify>,
}

#[async_trait]
impl BlobStorageBackend for LogicalLargeCancelStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
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
        if object_key == self.logical_object_key {
            self.legacy_read_count.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::UnsupportedOperation(
                "logical source rejects whole-object reads".to_string(),
            ));
        }
        self.inner.read_bytes(object_key).await
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        if object_key == self.logical_object_key {
            self.legacy_read_count.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::UnsupportedOperation(
                "logical source rejects buffered range reads".to_string(),
            ));
        }
        self.inner.read_range(object_key, start, end).await
    }

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner.stream_range(object_key, range).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.inner.delete_object(object_key).await
    }

    async fn read_location_bytes(
        &self,
        _backend: &str,
        _bucket: &str,
        object_key: &str,
    ) -> Result<Vec<u8>, StorageError> {
        if object_key == self.logical_object_key {
            self.legacy_read_count.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::UnsupportedOperation(
                "logical source rejects whole-object reads".to_string(),
            ));
        }
        Err(StorageError::UnsupportedOperation(
            "unexpected location read".to_string(),
        ))
    }

    async fn read_location_range(
        &self,
        _backend: &str,
        _bucket: &str,
        object_key: &str,
        _start: u64,
        _end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        if object_key == self.logical_object_key {
            self.legacy_read_count.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::UnsupportedOperation(
                "logical source rejects buffered range reads".to_string(),
            ));
        }
        Err(StorageError::UnsupportedOperation(
            "unexpected location range read".to_string(),
        ))
    }

    async fn stream_location_range(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        if object_key != self.logical_object_key {
            return self
                .inner
                .stream_location_range(backend, bucket, object_key, range)
                .await;
        }
        self.require_location(backend, bucket)?;
        let expected_size = u64::try_from(LOGICAL_LARGE_SOURCE_SIZE).expect("logical source size");
        if range
            != (BlobReadRange {
                expected_size,
                offset: 0,
                length: expected_size,
            })
        {
            return Err(StorageError::InvalidRange);
        }
        Ok(Box::pin(LogicalLargeSourceStream {
            frames_emitted: 0,
            frame_count: self.frame_count.clone(),
            poll_count: self.poll_count.clone(),
            entered_pending: self.entered_pending.clone(),
            pending_notified: false,
            dropped: self.dropped.clone(),
            dropped_notify: self.dropped_notify.clone(),
        }))
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

async fn insert_logical_document(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    size_bytes: i64,
    backend: &str,
    bucket: &str,
    object_key: &str,
) -> i64 {
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', ?, ?)
        ",
    )
    .bind("0".repeat(64))
    .bind(size_bytes)
    .execute(pool)
    .await
    .expect("logical blob")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO blob_locations (blob_id, backend, bucket, object_key)
        VALUES (?, ?, ?, ?)
        ",
    )
    .bind(blob_id)
    .bind(backend)
    .bind(bucket)
    .bind(object_key)
    .execute(pool)
    .await
    .expect("logical blob location");
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
    .expect("logical document")
    .last_insert_rowid();
    let version_id = format!("logical-export-version-{document_id}");
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
            (?, ?, ?, 1, 'admin', 'Admin', 'Logical test blob', 'text/plain', ?, 'upload')
        ",
    )
    .bind(&version_id)
    .bind(document_id)
    .bind(blob_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("logical version");
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
    .expect("logical current version");
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

fn deterministic_pseudorandom_bytes(length: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(length);
    let mut state = 0x4d59_5df4_d0f3_3173_u64;
    while bytes.len() < length {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let block = state.to_le_bytes();
        let remaining = length - bytes.len();
        bytes.extend_from_slice(&block[..remaining.min(block.len())]);
    }
    bytes
}

#[derive(Debug)]
struct LocalZipEntry {
    name: String,
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    data: Vec<u8>,
    data_descriptor: Option<Vec<u8>>,
}

// Real file and directory durability barriers can take longer than one second
// when the export tests publish several artifacts concurrently on a busy host.
const EXPORT_EVENTUAL_ASSERTION_ATTEMPTS: usize = 250;

async fn wait_for_export_status(
    app: axum::Router,
    job_id: &str,
    user: &str,
    groups: &str,
    expected: &str,
) -> Value {
    for _ in 0..EXPORT_EVENTUAL_ASSERTION_ATTEMPTS {
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
    for _ in 0..EXPORT_EVENTUAL_ASSERTION_ATTEMPTS {
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
    for _ in 0..EXPORT_EVENTUAL_ASSERTION_ATTEMPTS {
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
        let expected_keys_retained = expected_keys.iter().all(|key| keys.contains(key));
        if status == "cancelled"
            && artifact_count == 0
            && orphan_blob_count == 0
            && expected_keys_retained
        {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    let mut keys = storage.list_object_keys().await.expect("final object keys");
    keys.sort();
    panic!(
        "cancelled export left artifact/blob metadata behind or lost expected object keys: {keys:?}"
    );
}

async fn wait_for_path_missing(path: &Path) {
    for _ in 0..EXPORT_EVENTUAL_ASSERTION_ATTEMPTS {
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
    let swept = sweep_expired_transfers(
        pool,
        storage,
        transfers_path,
        &UploadHashCoordinator::new(),
        &TransferMaintenanceCoordinator::default(),
    )
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
    for _ in 0..EXPORT_EVENTUAL_ASSERTION_ATTEMPTS {
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

#[allow(clippy::too_many_lines)] // Keeping central, local, and descriptor offsets together aids auditability.
fn local_zip_entries(bytes: &[u8]) -> Vec<LocalZipEntry> {
    let end_record_offset = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .expect("end of central directory");
    assert!(
        end_record_offset + 22 <= bytes.len(),
        "truncated end of central directory"
    );
    let expected_entries = le_u16(bytes, end_record_offset + 10);
    assert_ne!(
        expected_entries,
        u16::MAX,
        "test ZIP parser does not support ZIP64 entry counts"
    );
    let central_directory_size = le_u32(bytes, end_record_offset + 12);
    let central_directory_offset = le_u32(bytes, end_record_offset + 16);
    assert_ne!(
        central_directory_size,
        u32::MAX,
        "test ZIP parser does not support ZIP64 directory sizes"
    );
    assert_ne!(
        central_directory_offset,
        u32::MAX,
        "test ZIP parser does not support ZIP64 archive offsets"
    );
    let mut entries = Vec::new();
    let mut offset = usize::try_from(central_directory_offset).expect("central directory offset");
    let central_directory_end =
        offset + usize::try_from(central_directory_size).expect("central directory size");
    assert_eq!(central_directory_end, end_record_offset);
    while offset + 46 <= central_directory_end && &bytes[offset..offset + 4] == b"PK\x01\x02" {
        let flags = le_u16(bytes, offset + 8);
        let method = le_u16(bytes, offset + 10);
        let crc32 = le_u32(bytes, offset + 16);
        let compressed_size_32 = le_u32(bytes, offset + 20);
        let uncompressed_size_32 = le_u32(bytes, offset + 24);
        let name_len = le_u16(bytes, offset + 28) as usize;
        let extra_len = le_u16(bytes, offset + 30) as usize;
        let comment_len = le_u16(bytes, offset + 32) as usize;
        let local_header_offset_32 = le_u32(bytes, offset + 42);
        let name_start = offset + 46;
        let name_end = name_start + name_len;
        let extra_end = name_end + extra_len;
        let record_end = extra_end + comment_len;
        assert!(
            record_end <= central_directory_end,
            "central ZIP entry exceeds central directory"
        );
        let mut zip64_values = central_zip64_values(&bytes[name_end..extra_end]).into_iter();
        let uncompressed_size = zip_u32_or_zip64(uncompressed_size_32, &mut zip64_values);
        let compressed_size = zip_u32_or_zip64(compressed_size_32, &mut zip64_values);
        let local_header_offset = zip_u32_or_zip64(local_header_offset_32, &mut zip64_values);
        let local_header_offset =
            usize::try_from(local_header_offset).expect("local ZIP header offset");
        assert!(
            local_header_offset + 30 <= bytes.len()
                && &bytes[local_header_offset..local_header_offset + 4] == b"PK\x03\x04",
            "missing local ZIP header"
        );
        assert_eq!(le_u16(bytes, local_header_offset + 6), flags);
        assert_eq!(le_u16(bytes, local_header_offset + 8), method);
        let local_name_len = le_u16(bytes, local_header_offset + 26) as usize;
        let local_extra_len = le_u16(bytes, local_header_offset + 28) as usize;
        let local_name_start = local_header_offset + 30;
        let local_name_end = local_name_start + local_name_len;
        let data_start = local_name_end + local_extra_len;
        let data_end =
            data_start + usize::try_from(compressed_size).expect("compressed ZIP entry size");
        assert!(
            local_name_end <= bytes.len() && data_end <= bytes.len(),
            "local ZIP entry exceeds archive length"
        );
        let name = std::str::from_utf8(&bytes[name_start..name_end])
            .expect("zip entry name")
            .to_string();
        assert_eq!(
            &bytes[local_name_start..local_name_end],
            &bytes[name_start..name_end],
            "local and central ZIP entry names differ"
        );
        let data_descriptor = if flags & 0x0008 == 0 {
            None
        } else {
            let uses_zip64_descriptor =
                compressed_size_32 == u32::MAX || uncompressed_size_32 == u32::MAX;
            let descriptor_len = if uses_zip64_descriptor { 24 } else { 16 };
            let descriptor_end = data_end + descriptor_len;
            assert!(
                descriptor_end <= bytes.len(),
                "truncated ZIP data descriptor"
            );
            assert_eq!(&bytes[data_end..data_end + 4], b"PK\x07\x08");
            assert_eq!(le_u32(bytes, data_end + 4), crc32);
            if uses_zip64_descriptor {
                assert_eq!(le_u64(bytes, data_end + 8), compressed_size);
                assert_eq!(le_u64(bytes, data_end + 16), uncompressed_size);
            } else {
                assert_eq!(u64::from(le_u32(bytes, data_end + 8)), compressed_size);
                assert_eq!(u64::from(le_u32(bytes, data_end + 12)), uncompressed_size);
            }
            Some(bytes[data_end..descriptor_end].to_vec())
        };
        entries.push(LocalZipEntry {
            name,
            flags,
            method,
            crc32,
            compressed_size,
            uncompressed_size,
            data: bytes[data_start..data_end].to_vec(),
            data_descriptor,
        });
        offset = record_end;
    }
    assert_eq!(offset, central_directory_end);
    assert_eq!(entries.len(), usize::from(expected_entries));
    entries
}

fn central_zip64_values(extra: &[u8]) -> Vec<u64> {
    let mut offset = 0_usize;
    while offset + 4 <= extra.len() {
        let field_id = le_u16(extra, offset);
        let field_len = le_u16(extra, offset + 2) as usize;
        let field_end = offset + 4 + field_len;
        assert!(field_end <= extra.len(), "truncated ZIP extra field");
        if field_id == 0x0001 {
            assert_eq!(field_len % 8, 0, "invalid ZIP64 extra field");
            return extra[offset + 4..field_end]
                .chunks_exact(8)
                .map(|chunk| u64::from_le_bytes(chunk.try_into().expect("ZIP64 value")))
                .collect();
        }
        offset = field_end;
    }
    Vec::new()
}

fn zip_u32_or_zip64(classic_value: u32, zip64_values: &mut impl Iterator<Item = u64>) -> u64 {
    if classic_value == u32::MAX {
        zip64_values.next().expect("missing ZIP64 value")
    } else {
        u64::from(classic_value)
    }
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

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("little-endian u64"),
    )
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
#[allow(clippy::too_many_lines)] // One coupled E2E scenario covers source and artifact failover.
async fn export_sources_and_artifacts_fail_over_before_exposing_bytes() {
    let (mut state, _temp_dir) = test_state().await;
    let stream_calls = Arc::new(Mutex::new(Vec::new()));
    state.storage = Arc::new(FailoverExportStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
        stream_calls: Arc::clone(&stream_calls),
    });
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let source = b"the prefetched export source frame appears exactly once";
    let document_id =
        insert_stored_document(&state.db, &state.storage, root.id, "failover.txt", source).await;
    let (source_location_id, source_blob_id, source_object_key) =
        sqlx::query_as::<_, (i64, i64, String)>(
            r"
            SELECT l.id, l.blob_id, l.object_key
            FROM document_versions v
            JOIN blob_locations l ON l.blob_id = v.blob_id
            WHERE v.document_id = ?
            ORDER BY l.id
            LIMIT 1
            ",
        )
        .bind(document_id)
        .fetch_one(&state.db)
        .await
        .expect("source location");
    sqlx::query("UPDATE blob_locations SET object_key = 'bad-first-source' WHERE id = ?")
        .bind(source_location_id)
        .execute(&state.db)
        .await
        .expect("stale source location");
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 'local', '', ?)",
    )
    .bind(source_blob_id)
    .bind(&source_object_key)
    .execute(&state.db)
    .await
    .expect("healthy source location");
    let pool = state.db.clone();
    let app = http::router(state);

    let queued = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("export response");
    assert_eq!(queued.status(), StatusCode::OK);
    let job_id = response_json(queued).await["id"]
        .as_str()
        .expect("job id")
        .to_string();
    let completed =
        wait_for_export_status(app.clone(), &job_id, "admin", "vault-admin", "complete").await;

    let (artifact_location_id, artifact_blob_id, artifact_object_key) =
        sqlx::query_as::<_, (i64, i64, String)>(
            r"
            SELECT l.id, l.blob_id, l.object_key
            FROM export_artifacts a
            JOIN blob_locations l ON l.blob_id = a.blob_id
            WHERE a.job_id = ?
            ORDER BY l.id
            LIMIT 1
            ",
        )
        .bind(&job_id)
        .fetch_one(&pool)
        .await
        .expect("artifact location");
    sqlx::query("UPDATE blob_locations SET object_key = 'missing-artifact' WHERE id = ?")
        .bind(artifact_location_id)
        .execute(&pool)
        .await
        .expect("stale artifact location");
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 'local', '', ?)",
    )
    .bind(artifact_blob_id)
    .bind(&artifact_object_key)
    .execute(&pool)
    .await
    .expect("healthy artifact location");

    let download = app
        .oneshot(authed_get(
            completed["download_url"].as_str().expect("download url"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("artifact failover response");
    assert_eq!(download.status(), StatusCode::OK);
    let zip = to_bytes(download.into_body(), usize::MAX)
        .await
        .expect("artifact body");
    let entries = local_zip_entries(&zip);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "failover.txt");
    assert_eq!(zip_entry_payload(&entries[0]), source);
    assert_eq!(
        *stream_calls.lock().expect("export failover calls"),
        [
            "bad-first-source",
            source_object_key.as_str(),
            "missing-artifact",
            artifact_object_key.as_str(),
        ]
    );
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
        .clone()
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
#[allow(
    clippy::too_many_lines,
    reason = "the admission contract and cleanup assertions share one blocked-worker scenario"
)]
async fn export_admission_bounds_active_jobs_globally_and_per_user() {
    let (mut state, _temp_dir) =
        test_state_with_export_limits(86_400, 1, 3 * 1024 * 1024 * 1024, 1, 2, 1).await;
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
        "bounded-export.txt",
        b"bounded export bytes",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);
    let request_body = json!({"items": [{"type": "document", "id": document_id}]});

    let first = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "first-user",
            "vault-admin",
            &request_body,
        ))
        .await
        .expect("first export response");
    assert_eq!(first.status(), StatusCode::OK);
    let first_id = response_json(first).await["id"]
        .as_str()
        .expect("first job id")
        .to_string();
    timeout(Duration::from_secs(5), entered_put_file.notified())
        .await
        .expect("first export should occupy the worker");

    let per_user_rejected = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "first-user",
            "vault-admin",
            &request_body,
        ))
        .await
        .expect("per-user rejection");
    assert_eq!(per_user_rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        per_user_rejected.headers().get("retry-after"),
        Some(&"1".parse().expect("retry header"))
    );

    let second = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "second-user",
            "vault-admin",
            &request_body,
        ))
        .await
        .expect("second export response");
    assert_eq!(second.status(), StatusCode::OK);
    let second_id = response_json(second).await["id"]
        .as_str()
        .expect("second job id")
        .to_string();

    let globally_rejected = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "third-user",
            "vault-admin",
            &request_body,
        ))
        .await
        .expect("global rejection");
    assert_eq!(globally_rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        globally_rejected.headers().get("retry-after"),
        Some(&"1".parse().expect("retry header"))
    );
    let active_jobs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM export_jobs WHERE status IN ('queued', 'running', 'finalizing')",
    )
    .fetch_one(&pool)
    .await
    .expect("active export count");
    assert_eq!(active_jobs, 2);
    let active_statuses: Vec<String> = sqlx::query_scalar(
        "SELECT status FROM export_jobs WHERE status IN ('queued', 'running', 'finalizing') ORDER BY status",
    )
    .fetch_all(&pool)
    .await
    .expect("active export statuses");
    assert_eq!(active_statuses, vec!["finalizing", "queued"]);

    let cancelled = app
        .clone()
        .oneshot(authed_delete(
            &format!("/api/exports/{second_id}"),
            "second-user",
            "vault-admin",
        ))
        .await
        .expect("cancel queued export");
    assert_eq!(cancelled.status(), StatusCode::OK);
    release_put_file.notify_one();
    wait_for_export_status(app, &first_id, "first-user", "vault-admin", "complete").await;
    let second_status: String = sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = ?")
        .bind(&second_id)
        .fetch_one(&pool)
        .await
        .expect("cancelled second status");
    let second_artifacts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
            .bind(&second_id)
            .fetch_one(&pool)
            .await
            .expect("second artifact count");
    assert_eq!(second_status, "cancelled");
    assert_eq!(second_artifacts, 0);
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the concurrent admission race must inspect and clean every accepted response"
)]
async fn concurrent_export_admission_never_oversubscribes_the_global_cap() {
    const SUBMISSIONS: usize = 16;
    const GLOBAL_CAP: usize = 4;
    let (mut state, _temp_dir) = test_state_with_export_limits(
        86_400,
        1,
        3 * 1024 * 1024 * 1024,
        1,
        i64::try_from(GLOBAL_CAP).expect("global cap"),
        1,
    )
    .await;
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
        "concurrent-admission.txt",
        b"concurrent admission bytes",
    )
    .await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let app = http::router(state);

    let responses = join_all((0..SUBMISSIONS).map(|index| {
        let app = app.clone();
        async move {
            let user_id = format!("concurrent-user-{index:02}");
            let response = app
                .oneshot(authed_json_post(
                    "/api/exports",
                    &user_id,
                    "vault-admin",
                    &json!({"items": [{"type": "document", "id": document_id}]}),
                ))
                .await
                .expect("concurrent export response");
            (user_id, response)
        }
    }))
    .await;
    let mut accepted = Vec::new();
    for (user_id, response) in responses {
        if response.status() == StatusCode::OK {
            let job_id = response_json(response).await["id"]
                .as_str()
                .expect("accepted job id")
                .to_string();
            accepted.push((user_id, job_id));
        } else {
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok()),
                Some("1")
            );
        }
    }
    assert_eq!(accepted.len(), GLOBAL_CAP);
    let active_jobs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM export_jobs WHERE status IN ('queued', 'running', 'finalizing')",
    )
    .fetch_one(&pool)
    .await
    .expect("active export count");
    assert_eq!(active_jobs, i64::try_from(GLOBAL_CAP).expect("global cap"));
    timeout(Duration::from_secs(5), entered_put_file.notified())
        .await
        .expect("one accepted export should occupy the worker");

    let accepted_ids = accepted
        .iter()
        .map(|(_, job_id)| job_id.clone())
        .collect::<Vec<_>>();
    for (user_id, job_id) in accepted {
        let response = app
            .clone()
            .oneshot(authed_delete(
                &format!("/api/exports/{job_id}"),
                &user_id,
                "vault-admin",
            ))
            .await
            .expect("cancel accepted export");
        assert_eq!(response.status(), StatusCode::OK);
    }
    release_put_file.notify_one();
    for job_id in &accepted_ids {
        wait_for_path_missing(
            &transfers_path
                .join("exports")
                .join(format!("{job_id}.zip.tmp")),
        )
        .await;
    }
    let artifact_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts")
        .fetch_one(&pool)
        .await
        .expect("cancelled race artifacts");
    assert_eq!(artifact_count, 0);
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "shutdown ordering, queue preservation, and publication cleanup form one scenario"
)]
async fn graceful_dispatcher_shutdown_drains_finalizing_work_and_exact_temp() {
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
        "shutdown-export.txt",
        b"shutdown export bytes",
    )
    .await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let execution = state.export_execution.clone();
    let app = http::router(state);
    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "admin",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("export response");
    let job_id = response_json(response).await["id"]
        .as_str()
        .expect("job id")
        .to_string();
    timeout(Duration::from_secs(5), entered_put_file.notified())
        .await
        .expect("artifact promotion should begin");
    let finalizing_status: String =
        sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = ?")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("finalizing status");
    assert_eq!(finalizing_status, "finalizing");
    let queued_response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/exports",
            "queued-user",
            "vault-admin",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("queued export response");
    let queued_job_id = response_json(queued_response).await["id"]
        .as_str()
        .expect("queued job id")
        .to_string();

    execution.request_dispatcher_shutdown();
    sleep(Duration::from_millis(50)).await;
    let still_queued: String = sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = ?")
        .bind(&queued_job_id)
        .fetch_one(&pool)
        .await
        .expect("queued status immediately after shutdown request");
    assert_eq!(still_queued, "queued");
    let mut shutdown = tokio::spawn(async move { execution.shutdown_dispatcher().await });
    assert!(
        timeout(Duration::from_millis(100), &mut shutdown)
            .await
            .is_err(),
        "graceful shutdown must not abandon an in-flight publication"
    );
    release_put_file.notify_one();
    timeout(Duration::from_secs(5), &mut shutdown)
        .await
        .expect("dispatcher should drain after storage returns")
        .expect("shutdown task");
    wait_for_export_status_in_db(&pool, &job_id, "complete").await;
    let queued_status: String = sqlx::query_scalar("SELECT status FROM export_jobs WHERE id = ?")
        .bind(&queued_job_id)
        .fetch_one(&pool)
        .await
        .expect("queued status");
    assert_eq!(queued_status, "queued");

    assert!(
        tokio::fs::symlink_metadata(
            transfers_path
                .join("exports")
                .join(format!("{job_id}.zip.tmp")),
        )
        .await
        .is_err()
    );
    let pending_leases: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM blob_locations WHERE backend GLOB '_vault_pending:*'",
    )
    .fetch_one(&pool)
    .await
    .expect("pending publication leases");
    assert_eq!(pending_leases, 0);
    let artifact_jobs: Vec<String> =
        sqlx::query_scalar("SELECT job_id FROM export_artifacts ORDER BY job_id")
            .fetch_all(&pool)
            .await
            .expect("artifact jobs");
    assert_eq!(artifact_jobs, vec![job_id]);
    assert!(
        tokio::fs::symlink_metadata(
            transfers_path
                .join("exports")
                .join(format!("{queued_job_id}.zip.tmp")),
        )
        .await
        .is_err()
    );
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the atomic-claim fixture and exactly-once assertions are intentionally colocated"
)]
async fn concurrent_dispatcher_workers_claim_each_queued_job_once() {
    const JOBS: i64 = 12;
    let (state, _temp_dir) =
        test_state_with_export_settings(86_400, 4, 3 * 1024 * 1024 * 1024, 1).await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let document_id = insert_stored_document(
        &state.db,
        &state.storage,
        root.id,
        "atomic-claim.txt",
        b"atomic claim bytes",
    )
    .await;
    let user = UserContext {
        id: "claim-user".to_string(),
        vault_user_id: 0,
        issuer: "test".to_string(),
        subject: "claim-user".to_string(),
        name: "Claim User".to_string(),
        email: "claim@example.com".to_string(),
        groups: Vec::new(),
        is_admin: true,
    };
    let user_context = serde_json::to_string(&user).expect("user context");
    let request_payload = serde_json::to_string(&json!({
        "items": [{"type": "document", "id": document_id}]
    }))
    .expect("request payload");
    let mut transaction = state.db.begin().await.expect("queue transaction");
    for index in 0..JOBS {
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
            VALUES (?, 'queued', 'claim.zip', 1, 18, ?, ?, ?, ?, '2999-01-01T00:00:00Z')
            ",
        )
        .bind(format!("atomic-claim-{index:02}"))
        .bind(&user.id)
        .bind(&user.name)
        .bind(&user_context)
        .bind(&request_payload)
        .execute(&mut *transaction)
        .await
        .expect("queued claim job");
    }
    transaction.commit().await.expect("commit claim jobs");
    let transfers_path = state.config.transfers_path();
    state
        .export_execution
        .start_dispatcher(&state.db, &state.storage, &transfers_path);

    timeout(Duration::from_secs(10), async {
        loop {
            let complete: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM export_jobs WHERE status = 'complete'")
                    .fetch_one(&state.db)
                    .await
                    .expect("complete claim jobs");
            if complete == JOBS {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all claim jobs should complete");
    timeout(Duration::from_secs(5), async {
        loop {
            let events: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM document_events WHERE document_id = ? AND event_type = 'download'",
            )
            .bind(document_id)
            .fetch_one(&state.db)
            .await
            .expect("claim download events");
            if events == JOBS {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("each completed job should record one event");
    let artifacts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts")
        .fetch_one(&state.db)
        .await
        .expect("claim artifacts");
    let failed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_jobs WHERE status = 'failed'")
            .fetch_one(&state.db)
            .await
            .expect("failed claim jobs");
    assert_eq!(artifacts, JOBS);
    assert_eq!(failed, 0);
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
async fn export_artifact_failure_rolls_back_blob_and_location_metadata() {
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

    timeout(Duration::from_secs(5), entered_put_file.notified())
        .await
        .expect("artifact promotion should be blocked before storage write");
    sqlx::query(
        r"
        CREATE TRIGGER reject_export_artifact_metadata
        BEFORE INSERT ON export_artifacts
        BEGIN
            SELECT RAISE(ABORT, 'forced artifact metadata failure');
        END
        ",
    )
    .execute(&pool)
    .await
    .expect("install artifact failure trigger");

    release_put_file.notify_one();
    wait_for_export_status_in_db(&pool, &job_id, "failed").await;
    wait_for_path_missing(
        &transfers_path
            .join("exports")
            .join(format!("{job_id}.zip.tmp")),
    )
    .await;
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
            .bind(&job_id)
            .fetch_one(&pool)
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
    .fetch_one(&pool)
    .await
    .expect("orphan blob count");
    let mut keys = storage.list_object_keys().await.expect("object keys");
    keys.sort();
    assert_eq!(artifact_count, 0);
    assert_eq!(orphan_blob_count, 0);
    assert_eq!(keys, expected_keys);
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One pending-open race retains all drop and cleanup assertions.
async fn cancelled_export_drops_pending_source_open_and_cleans_temp() {
    let (mut state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let source = b"small compressible source";
    let document_id = insert_stored_document(
        &state.db,
        &state.storage,
        root.id,
        "pending-open.txt",
        source,
    )
    .await;
    let mut expected_keys = state
        .storage
        .list_object_keys()
        .await
        .expect("initial physical keys");
    expected_keys.sort();
    assert_eq!(expected_keys.len(), 1);
    let entered_open = Arc::new(Notify::new());
    let open_call_count = Arc::new(AtomicUsize::new(0));
    let open_dropped = Arc::new(AtomicBool::new(false));
    let open_dropped_notify = Arc::new(Notify::new());
    let legacy_read_count = Arc::new(AtomicUsize::new(0));
    let frame_poll_count = Arc::new(AtomicUsize::new(0));
    state.storage = Arc::new(PendingOpenStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
        source_object_key: expected_keys[0].clone(),
        source_size: u64::try_from(source.len()).expect("source size"),
        entered_open: entered_open.clone(),
        open_call_count: open_call_count.clone(),
        open_dropped: open_dropped.clone(),
        open_dropped_notify: open_dropped_notify.clone(),
        legacy_read_count: legacy_read_count.clone(),
        frame_poll_count: frame_poll_count.clone(),
    });
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
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("export response");
    assert_eq!(response.status(), StatusCode::OK);
    let job_id = response_json(response).await["id"]
        .as_str()
        .expect("job id")
        .to_string();
    let temp_path = transfers_path
        .join("exports")
        .join(format!("{job_id}.zip.tmp"));
    timeout(Duration::from_secs(5), entered_open.notified())
        .await
        .expect("source open should become pending");
    assert!(
        tokio::fs::metadata(&temp_path)
            .await
            .expect("pending export temp file")
            .is_file()
    );
    assert_eq!(open_call_count.load(Ordering::SeqCst), 1);
    assert_eq!(legacy_read_count.load(Ordering::SeqCst), 0);
    assert_eq!(frame_poll_count.load(Ordering::SeqCst), 0);

    let cancelled = app
        .oneshot(authed_delete(
            &format!("/api/exports/{job_id}"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("cancel response");
    assert_eq!(cancelled.status(), StatusCode::OK);
    assert_eq!(response_json(cancelled).await["status"], "cancelled");
    if !open_dropped.load(Ordering::SeqCst) {
        timeout(Duration::from_secs(5), open_dropped_notify.notified())
            .await
            .expect("cancelled export should drop the pending open future");
    }
    assert!(open_dropped.load(Ordering::SeqCst));
    wait_for_cancelled_export_cleanup(&pool, &storage, &job_id, &expected_keys).await;
    wait_for_path_missing(&temp_path).await;

    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("artifact count");
    let mut final_keys = storage
        .list_object_keys()
        .await
        .expect("final physical keys");
    final_keys.sort();
    assert_eq!(artifact_count, 0);
    assert_eq!(final_keys, expected_keys);
    assert_eq!(open_call_count.load(Ordering::SeqCst), 1);
    assert_eq!(legacy_read_count.load(Ordering::SeqCst), 0);
    assert_eq!(frame_poll_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_export_during_streaming_entry_write_cleans_partial_zip() {
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
    let data = vec![b'x'; 2 * 1024 * 1024];
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
        .clone()
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
#[allow(clippy::too_many_lines)] // One cancellation race must retain all stream and cleanup assertions.
async fn logical_five_gib_export_cancels_while_source_stream_is_pending() {
    let (mut state, _temp_dir) = test_state().await;
    let logical_object_key = "logical/five-gib-text".to_string();
    let legacy_read_count = Arc::new(AtomicUsize::new(0));
    let frame_count = Arc::new(AtomicUsize::new(0));
    let poll_count = Arc::new(AtomicUsize::new(0));
    let entered_pending = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let dropped_notify = Arc::new(Notify::new());
    state.storage = Arc::new(LogicalLargeCancelStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
        logical_object_key: logical_object_key.clone(),
        legacy_read_count: legacy_read_count.clone(),
        frame_count: frame_count.clone(),
        poll_count: poll_count.clone(),
        entered_pending: entered_pending.clone(),
        dropped: dropped.clone(),
        dropped_notify: dropped_notify.clone(),
    });
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let _ = insert_stored_document(
        &state.db,
        &state.storage,
        root.id,
        "retained.txt",
        b"physical object that must remain",
    )
    .await;
    let mut expected_keys = state
        .storage
        .list_object_keys()
        .await
        .expect("initial physical keys");
    expected_keys.sort();
    assert!(!expected_keys.is_empty());
    let document_id = insert_logical_document(
        &state.db,
        root.id,
        "logical-five-gib.txt",
        LOGICAL_LARGE_SOURCE_SIZE,
        state.storage.name(),
        state.storage.bucket(),
        &logical_object_key,
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
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("export response");
    assert_eq!(response.status(), StatusCode::OK);
    let job_id = response_json(response).await["id"]
        .as_str()
        .expect("job id")
        .to_string();
    let temp_path = transfers_path
        .join("exports")
        .join(format!("{job_id}.zip.tmp"));
    timeout(Duration::from_secs(5), entered_pending.notified())
        .await
        .expect("logical source should become pending after its bounded prefix");
    assert!(
        tokio::fs::metadata(&temp_path)
            .await
            .expect("partial export temp file")
            .is_file()
    );
    assert_eq!(
        frame_count.load(Ordering::SeqCst),
        LOGICAL_LARGE_PREFIX_FRAMES
    );
    assert_eq!(legacy_read_count.load(Ordering::SeqCst), 0);

    let cancelled = app
        .oneshot(authed_delete(
            &format!("/api/exports/{job_id}"),
            "admin",
            "vault-admin",
        ))
        .await
        .expect("cancel response");
    assert_eq!(cancelled.status(), StatusCode::OK);
    assert_eq!(response_json(cancelled).await["status"], "cancelled");
    if !dropped.load(Ordering::SeqCst) {
        timeout(Duration::from_secs(5), dropped_notify.notified())
            .await
            .expect("cancelled export should drop the pending source stream");
    }
    assert!(dropped.load(Ordering::SeqCst));
    wait_for_cancelled_export_cleanup(&pool, &storage, &job_id, &expected_keys).await;
    wait_for_path_missing(&temp_path).await;

    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM export_artifacts WHERE job_id = ?")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .expect("artifact count");
    let mut final_keys = storage
        .list_object_keys()
        .await
        .expect("final physical keys");
    final_keys.sort();
    let observed_polls = poll_count.load(Ordering::SeqCst);
    assert_eq!(artifact_count, 0);
    assert_eq!(final_keys, expected_keys);
    assert_eq!(legacy_read_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        frame_count.load(Ordering::SeqCst),
        LOGICAL_LARGE_PREFIX_FRAMES
    );
    assert!(
        (LOGICAL_LARGE_PREFIX_FRAMES + 1..=32).contains(&observed_polls),
        "pending source was polled an unexpected number of times: {observed_polls}"
    );
}

#[tokio::test]
async fn large_stored_export_reports_byte_progress_before_entry_finishes() {
    let (mut state, _temp_dir) = test_state().await;
    let entered_after_progress = Arc::new(Notify::new());
    let release_range = Arc::new(Notify::new());
    state.storage = Arc::new(BlockAfterProgressRangeStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
        entered_after_progress: entered_after_progress.clone(),
        release_range: release_range.clone(),
    });
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let data = vec![b'x'; 34 * 1024 * 1024];
    let document_id = insert_stored_document_with_mime(
        &state.db,
        &state.storage,
        root.id,
        "large.bin",
        &data,
        "application/octet-stream",
    )
    .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
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

    if timeout(Duration::from_secs(5), entered_after_progress.notified())
        .await
        .is_err()
    {
        let stalled = sqlx::query_as::<_, (String, Option<String>, i64, i64)>(
            "SELECT status, error, processed_bytes, total_bytes FROM export_jobs WHERE id = ?",
        )
        .bind(&job_id)
        .fetch_one(&pool)
        .await
        .expect("stalled export status");
        panic!("export should report progress before finishing the large entry: {stalled:?}");
    }
    let progress = sqlx::query_as::<_, (String, i64, i64, i64)>(
        r"
        SELECT status, processed_items, processed_bytes, total_bytes
        FROM export_jobs
        WHERE id = ?
        ",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("export progress");
    assert_eq!(progress.0, "running");
    assert_eq!(progress.1, 0);
    assert!(progress.2 >= 32 * 1024 * 1024, "{progress:?}");
    assert!(progress.2 < progress.3, "{progress:?}");

    release_range.notify_one();
    wait_for_export_status_in_db(&pool, &job_id, "complete").await;
    let completed = sqlx::query_as::<_, (i64, i64, i64)>(
        r"
        SELECT processed_items, processed_bytes, total_bytes
        FROM export_jobs
        WHERE id = ?
        ",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("completed export progress");
    assert_eq!(completed.0, 1);
    assert_eq!(completed.1, completed.2);
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
async fn export_route_streams_configured_compression_with_data_descriptor() {
    let (mut state, _temp_dir) = test_state_with_export_settings(86_400, 1, 1, 1).await;
    state.storage = Arc::new(StreamOnlyExportStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
    });
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let text = "configured route compression\n"
        .repeat(128 * 1024)
        .into_bytes();
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
    assert_ne!(entry.flags & 0x0008, 0);
    assert_eq!(
        entry.crc32,
        Crc::<u32>::new(&CRC_32_ISO_HDLC).checksum(&text)
    );
    assert_eq!(
        entry.uncompressed_size,
        u64::try_from(text.len()).expect("text length")
    );
    assert_eq!(
        entry.compressed_size,
        u64::try_from(entry.data.len()).expect("compressed length")
    );
    assert!(entry.compressed_size < entry.uncompressed_size);
    assert!(entry.data_descriptor.is_some());
    assert_eq!(zip_entry_payload(entry), text);
}

#[tokio::test]
async fn export_streams_multiple_incompressible_deflate_batches_without_losing_output() {
    let (mut state, _temp_dir) = test_state_with_export_settings(86_400, 1, 1, 1).await;
    state.storage = Arc::new(StreamOnlyExportStorage {
        inner: LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix),
    });
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let text = deterministic_pseudorandom_bytes(9 * 1024 * 1024);
    let document_id = insert_stored_document_with_mime(
        &state.db,
        &state.storage,
        root.id,
        "pseudorandom.txt",
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
            &json!({"items": [{"type": "document", "id": document_id}]}),
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
        .find(|entry| entry.name == "pseudorandom.txt")
        .expect("zip entry");

    assert_eq!(entry.method, 8);
    assert_ne!(entry.flags & 0x0008, 0);
    assert_eq!(
        entry.crc32,
        Crc::<u32>::new(&CRC_32_ISO_HDLC).checksum(&text)
    );
    assert_eq!(
        entry.uncompressed_size,
        u64::try_from(text.len()).expect("text length")
    );
    assert_eq!(
        entry.compressed_size,
        u64::try_from(entry.data.len()).expect("compressed length")
    );
    assert!(entry.compressed_size > 8 * 1024 * 1024);
    assert_eq!(
        entry
            .data_descriptor
            .as_ref()
            .expect("data descriptor")
            .len(),
        24
    );
    assert_eq!(zip_entry_payload(entry), text);
}

#[tokio::test]
async fn export_runtime_settings_are_normalized_in_app_state() {
    let (state, _temp_dir) = test_state_with_export_settings(10, -2, -1, 12).await;
    let settings = state.export_execution.settings();

    assert_eq!(settings.ttl_seconds, 60);
    assert_eq!(settings.workers, 1);
    assert_eq!(settings.max_active_jobs, 256);
    assert_eq!(settings.max_active_jobs_per_user, 16);
    assert_eq!(settings.zip_options.compression_threshold_bytes, 0);
    assert_eq!(settings.zip_options.compresslevel, 9);
}

#[tokio::test]
async fn export_zip_deflates_text_and_stores_precompressed_entries_when_threshold_allows() {
    let (state, _temp_dir) = test_state_with_export_settings(86_400, 1, 1, 1).await;
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

    let payload = exports::create_export_job_with_runtime(
        &state.db,
        &state.storage,
        &state.config.transfers_path(),
        &[
            ExportSelectionItem::Document { id: text_id },
            ExportSelectionItem::Document { id: png_id },
        ],
        &user,
        &state.export_execution,
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
