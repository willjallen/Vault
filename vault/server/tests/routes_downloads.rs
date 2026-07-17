use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures_util::{StreamExt, stream};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio::time::timeout;
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{
    VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path, get_root_folder,
};
use vault_server::http::{self, AppState};
use vault_server::storage::{
    BlobByteStream, BlobReadRange, BlobStorageBackend, STORAGE_CHUNK_SIZE, StorageError, StoredBlob,
};

#[derive(Debug)]
struct RangeOnlyStorage {
    data: Vec<u8>,
    full_read_called: Arc<AtomicBool>,
    object_key: String,
    stream_calls: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct GatedStreamStorage {
    active_streams: Arc<AtomicUsize>,
    chunk_bytes: usize,
    dropped_streams: Arc<AtomicUsize>,
    fail_after_first: bool,
    legacy_read_called: Arc<AtomicBool>,
    logical_size: u64,
    object_key: String,
    produced_chunks: Arc<AtomicUsize>,
    release: Arc<Notify>,
    waiting: Arc<Notify>,
}

#[derive(Debug)]
struct FailoverStorage {
    calls: Arc<Mutex<Vec<String>>>,
    data: Vec<u8>,
}

#[async_trait]
impl BlobStorageBackend for FailoverStorage {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn bucket(&self) -> &'static str {
        "active-bucket"
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn put_bytes(&self, _data: &[u8]) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage is read-only".to_string(),
        ))
    }

    async fn put_file(
        &self,
        _source_path: &Path,
        _digest: &str,
        _size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage is read-only".to_string(),
        ))
    }

    async fn put_part_files(
        &self,
        _part_paths: &[PathBuf],
        _expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage is read-only".to_string(),
        ))
    }

    async fn read_bytes(&self, _object_key: &str) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "failover must use the canonical stream".to_string(),
        ))
    }

    async fn read_range(
        &self,
        _object_key: &str,
        _start: u64,
        _end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "failover must use the canonical stream".to_string(),
        ))
    }

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.calls
            .lock()
            .expect("failover calls")
            .push(object_key.to_string());
        if range.expected_size != self.data.len() as u64
            || range.offset.saturating_add(range.length) > range.expected_size
        {
            return Err(StorageError::InvalidRange);
        }
        match object_key {
            "busy-exact" => Err(StorageError::Busy),
            "bad-first-frame" => Ok(Box::pin(stream::once(async {
                Err(StorageError::Remote(
                    "injected first-frame failure".to_string(),
                ))
            }))),
            "midstream-failure" => {
                let split = self.data.len().min(4);
                let first = self.data[..split].to_vec();
                Ok(Box::pin(stream::iter([
                    Ok(first.into()),
                    Err(StorageError::Remote(
                        "injected midstream failure".to_string(),
                    )),
                ])))
            }
            "healthy-exact" | "healthy-legacy" | "healthy-after-midstream" => {
                let start =
                    usize::try_from(range.offset).map_err(|_| StorageError::InvalidRange)?;
                let end = usize::try_from(range.offset + range.length)
                    .map_err(|_| StorageError::InvalidRange)?;
                let bytes = self
                    .data
                    .get(start..end)
                    .ok_or(StorageError::InvalidRange)?
                    .to_vec();
                if bytes.is_empty() {
                    Ok(Box::pin(stream::empty()))
                } else {
                    Ok(Box::pin(stream::once(async move { Ok(bytes.into()) })))
                }
            }
            _ => Err(StorageError::NotFound),
        }
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage cannot list objects".to_string(),
        ))
    }

    async fn delete_object(&self, _object_key: &str) -> Result<(), StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage cannot delete objects".to_string(),
        ))
    }
}

#[derive(Debug)]
struct StreamDropProbe {
    active_streams: Arc<AtomicUsize>,
    dropped_streams: Arc<AtomicUsize>,
}

impl Drop for StreamDropProbe {
    fn drop(&mut self) {
        self.active_streams.fetch_sub(1, Ordering::SeqCst);
        self.dropped_streams.fetch_add(1, Ordering::SeqCst);
    }
}

struct GatedStreamState {
    chunk_bytes: usize,
    fail_after_first: bool,
    first: bool,
    _probe: StreamDropProbe,
    produced_chunks: Arc<AtomicUsize>,
    release: Arc<Notify>,
    remaining: u64,
    waiting: Arc<Notify>,
}

#[async_trait]
impl BlobStorageBackend for RangeOnlyStorage {
    fn name(&self) -> &'static str {
        "local"
    }

    fn bucket(&self) -> &'static str {
        ""
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn put_bytes(&self, _data: &[u8]) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage is read-only".to_string(),
        ))
    }

    async fn put_file(
        &self,
        _source_path: &Path,
        _digest: &str,
        _size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage is read-only".to_string(),
        ))
    }

    async fn put_part_files(
        &self,
        _part_paths: &[PathBuf],
        _expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage is read-only".to_string(),
        ))
    }

    async fn read_bytes(&self, _object_key: &str) -> Result<Vec<u8>, StorageError> {
        self.full_read_called.store(true, Ordering::SeqCst);
        Err(StorageError::UnsupportedOperation(
            "range download used full object read".to_string(),
        ))
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        if object_key != self.object_key || end < start {
            return Err(StorageError::InvalidRange);
        }
        let start = usize::try_from(start).map_err(|_| StorageError::InvalidRange)?;
        let end = usize::try_from(end).map_err(|_| StorageError::InvalidRange)?;
        self.data
            .get(start..=end)
            .map(<[u8]>::to_vec)
            .ok_or(StorageError::InvalidRange)
    }

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        if object_key != self.object_key
            || range.expected_size != self.data.len() as u64
            || range.offset.saturating_add(range.length) > range.expected_size
        {
            return Err(StorageError::InvalidRange);
        }
        let start = usize::try_from(range.offset).map_err(|_| StorageError::InvalidRange)?;
        let end =
            usize::try_from(range.offset + range.length).map_err(|_| StorageError::InvalidRange)?;
        let bytes = self
            .data
            .get(start..end)
            .ok_or(StorageError::InvalidRange)?
            .to_vec();
        if bytes.is_empty() {
            Ok(Box::pin(stream::empty()))
        } else {
            Ok(Box::pin(stream::once(async move { Ok(bytes.into()) })))
        }
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage cannot list objects".to_string(),
        ))
    }

    async fn delete_object(&self, _object_key: &str) -> Result<(), StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage cannot delete objects".to_string(),
        ))
    }
}

#[async_trait]
impl BlobStorageBackend for GatedStreamStorage {
    fn name(&self) -> &'static str {
        "local"
    }

    fn bucket(&self) -> &'static str {
        ""
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn put_bytes(&self, _data: &[u8]) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage is read-only".to_string(),
        ))
    }

    async fn put_file(
        &self,
        _source_path: &Path,
        _digest: &str,
        _size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage is read-only".to_string(),
        ))
    }

    async fn put_part_files(
        &self,
        _part_paths: &[PathBuf],
        _expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage is read-only".to_string(),
        ))
    }

    async fn read_bytes(&self, _object_key: &str) -> Result<Vec<u8>, StorageError> {
        self.legacy_read_called.store(true, Ordering::SeqCst);
        Err(StorageError::UnsupportedOperation(
            "streaming download used a whole-object read".to_string(),
        ))
    }

    async fn read_range(
        &self,
        _object_key: &str,
        _start: u64,
        _end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        self.legacy_read_called.store(true, Ordering::SeqCst);
        Err(StorageError::UnsupportedOperation(
            "streaming download used a buffered range read".to_string(),
        ))
    }

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        if object_key != self.object_key
            || range.expected_size != self.logical_size
            || range.offset.checked_add(range.length).is_none()
            || range.offset + range.length > range.expected_size
        {
            return Err(StorageError::InvalidRange);
        }
        self.active_streams.fetch_add(1, Ordering::SeqCst);
        let state = GatedStreamState {
            chunk_bytes: self.chunk_bytes,
            fail_after_first: self.fail_after_first,
            first: true,
            _probe: StreamDropProbe {
                active_streams: Arc::clone(&self.active_streams),
                dropped_streams: Arc::clone(&self.dropped_streams),
            },
            produced_chunks: Arc::clone(&self.produced_chunks),
            release: Arc::clone(&self.release),
            remaining: range.length,
            waiting: Arc::clone(&self.waiting),
        };
        Ok(Box::pin(stream::unfold(state, |mut state| async move {
            if state.remaining == 0 {
                return None;
            }
            if !state.first {
                state.waiting.notify_one();
                if state.fail_after_first {
                    return Some((
                        Err(StorageError::Remote(
                            "injected midstream failure".to_string(),
                        )),
                        state,
                    ));
                }
                state.release.notified().await;
            }
            let chunk_len = usize::try_from(
                state
                    .remaining
                    .min(u64::try_from(state.chunk_bytes).unwrap_or(u64::MAX)),
            )
            .unwrap_or(state.chunk_bytes);
            state.remaining -= chunk_len as u64;
            state.first = false;
            state.produced_chunks.fetch_add(1, Ordering::SeqCst);
            Some((Ok(vec![b'x'; chunk_len].into()), state))
        })))
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage cannot list objects".to_string(),
        ))
    }

    async fn delete_object(&self, _object_key: &str) -> Result<(), StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage cannot delete objects".to_string(),
        ))
    }
}

async fn test_state(storage: Arc<dyn BlobStorageBackend>) -> (AppState, tempfile::TempDir) {
    test_state_with_limit(storage, 64).await
}

async fn test_state_with_limit(
    storage: Arc<dyn BlobStorageBackend>,
    download_limit: usize,
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
        export_ttl_seconds: 86_400,
        export_workers: 1,
        export_zip_compression_threshold_bytes: 3 * 1024 * 1024 * 1024,
        export_zip_compresslevel: 1,
        ttl_sweep_interval_seconds: 60,
        gzip_minimum_size: 1024,
        gzip_compresslevel: 6,
    };
    let db = db::connect(&config.db_path()).await.expect("db");
    let state = AppState::new_with_download_limit(
        config,
        AuthSettings::default(),
        db,
        storage,
        download_limit,
    );
    (state, temp_dir)
}

fn authed_get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("Remote-User", "reader")
        .header("Remote-Name", "reader")
        .header("Remote-Email", "reader@example.com")
        .header("Remote-Groups", "readers")
        .body(Body::empty())
        .expect("request")
}

fn authed_get_with_range(uri: &str, range: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("Remote-User", "reader")
        .header("Remote-Name", "reader")
        .header("Remote-Email", "reader@example.com")
        .header("Remote-Groups", "readers")
        .header("Range", range)
        .body(Body::empty())
        .expect("request")
}

fn authed_get_with_headers(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = authed_get(uri);
    for (name, value) in headers {
        request.headers_mut().insert(
            name.parse::<axum::http::HeaderName>().expect("header name"),
            value.parse().expect("header value"),
        );
    }
    request
}

async fn create_group(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO vault_groups (name) VALUES (?)")
        .bind(name)
        .execute(pool)
        .await
        .expect("create group")
        .last_insert_rowid()
}

async fn grant_reader_project(state: &AppState) -> i64 {
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project")
        .id
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
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

async fn insert_downloadable_document(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    object_key: &str,
    data: &[u8],
) -> i64 {
    insert_downloadable_document_metadata(
        pool,
        folder_id,
        object_key,
        &sha256_hex(data),
        i64::try_from(data.len()).expect("blob size"),
    )
    .await
}

async fn insert_downloadable_document_metadata(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    object_key: &str,
    hash: &str,
    size_bytes: i64,
) -> i64 {
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', ?, ?)
        ",
    )
    .bind(hash)
    .bind(size_bytes)
    .execute(pool)
    .await
    .expect("blob")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO blob_locations (blob_id, backend, bucket, object_key)
        VALUES (?, 'local', '', ?)
        ",
    )
    .bind(blob_id)
    .bind(object_key)
    .execute(pool)
    .await
    .expect("blob location");
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, 'download-name.txt', 'admin', 'Admin', 'admin')
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
            ('version-one', ?, ?, 1, 'admin', 'Admin', 'Uploaded file', 'text/plain', 'download-name.txt', 'upload')
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

#[tokio::test]
async fn browser_range_probe_does_not_read_full_blob() {
    let full_read_called = Arc::new(AtomicBool::new(false));
    let object_key = "fixture-object".to_string();
    let data = b"hello world".to_vec();
    let storage = Arc::new(RangeOnlyStorage {
        data,
        full_read_called: Arc::clone(&full_read_called),
        object_key: object_key.clone(),
        stream_calls: Arc::new(AtomicUsize::new(0)),
    });
    let (state, _temp_dir) = test_state(storage).await;
    let project_id = grant_reader_project(&state).await;
    let document_id =
        insert_downloadable_document(&state.db, project_id, &object_key, b"hello world").await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_get_with_range(
            &format!("/documents/{document_id}/download"),
            "bytes=0-0",
        ))
        .await
        .expect("download response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 0-0/11");
    assert_eq!(&body[..], b"h");
    assert!(!full_read_called.load(Ordering::SeqCst));
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // The cases intentionally share one ETag and backend call counter.
async fn download_range_contract_handles_suffix_if_range_and_malformed_headers() {
    let object_key = "range-contract".to_string();
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let storage = Arc::new(RangeOnlyStorage {
        data: b"hello world".to_vec(),
        full_read_called: Arc::new(AtomicBool::new(false)),
        object_key: object_key.clone(),
        stream_calls: Arc::clone(&stream_calls),
    });
    let (state, _temp_dir) = test_state(storage).await;
    let project_id = grant_reader_project(&state).await;
    let document_id =
        insert_downloadable_document(&state.db, project_id, &object_key, b"hello world").await;
    let uri = format!("/documents/{document_id}/download");
    let app = http::router(state);

    let full = app
        .clone()
        .oneshot(authed_get(&uri))
        .await
        .expect("full response");
    assert_eq!(full.status(), StatusCode::OK);
    let etag = full.headers()["etag"].to_str().expect("etag").to_string();
    assert_eq!(
        to_bytes(full.into_body(), 32).await.expect("full body"),
        b"hello world"[..]
    );

    for (range, expected_range) in [("bytes=-5", "bytes 6-10/11"), ("bytes=6-", "bytes 6-10/11")] {
        let response = app
            .clone()
            .oneshot(authed_get_with_range(&uri, range))
            .await
            .expect("range response");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["content-range"], expected_range);
        assert_eq!(
            to_bytes(response.into_body(), 16)
                .await
                .expect("range body"),
            b"world"[..]
        );
    }

    let case_insensitive = app
        .clone()
        .oneshot(authed_get_with_range(&uri, "Bytes=0-0"))
        .await
        .expect("case-insensitive unit");
    assert_eq!(case_insensitive.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        to_bytes(case_insensitive.into_body(), 1)
            .await
            .expect("case-insensitive body"),
        b"h"[..]
    );

    let matching = app
        .clone()
        .oneshot(authed_get_with_headers(
            &uri,
            &[("Range", "bytes=1-2"), ("If-Range", &etag)],
        ))
        .await
        .expect("matching if-range");
    assert_eq!(matching.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        to_bytes(matching.into_body(), 2)
            .await
            .expect("matching body"),
        b"el"[..]
    );

    let mismatched = app
        .clone()
        .oneshot(authed_get_with_headers(
            &uri,
            &[("Range", "bytes=1-2"), ("If-Range", "\"different\"")],
        ))
        .await
        .expect("mismatched if-range");
    assert_eq!(mismatched.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(mismatched.into_body(), 32)
            .await
            .expect("mismatched body"),
        b"hello world"[..]
    );
    let valid_stream_calls = stream_calls.load(Ordering::SeqCst);

    for range in ["bytes=bytes=0-1", "bytes=0-1,3-4", "bytes=99-100"] {
        let response = app
            .clone()
            .oneshot(authed_get_with_range(&uri, range))
            .await
            .expect("invalid range response");
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()["content-range"], "bytes */11");
    }
    let mut duplicate = authed_get_with_range(&uri, "bytes=0-1");
    duplicate
        .headers_mut()
        .append("Range", "bytes=2-3".parse().expect("duplicate range"));
    let duplicate = app
        .clone()
        .oneshot(duplicate)
        .await
        .expect("duplicate range response");
    assert_eq!(duplicate.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    let mut non_utf8 = authed_get(&uri);
    non_utf8.headers_mut().insert(
        "Range",
        axum::http::HeaderValue::from_bytes(b"bytes=\xff").expect("opaque range"),
    );
    let non_utf8 = app
        .oneshot(non_utf8)
        .await
        .expect("non-utf8 range response");
    assert_eq!(non_utf8.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(stream_calls.load(Ordering::SeqCst), valid_stream_calls);
}

#[tokio::test]
async fn empty_download_is_stream_validated_and_rejects_byte_ranges() {
    let object_key = "empty-object".to_string();
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let storage = Arc::new(RangeOnlyStorage {
        data: Vec::new(),
        full_read_called: Arc::new(AtomicBool::new(false)),
        object_key: object_key.clone(),
        stream_calls: Arc::clone(&stream_calls),
    });
    let (state, _temp_dir) = test_state(storage).await;
    let project_id = grant_reader_project(&state).await;
    let document_id = insert_downloadable_document(&state.db, project_id, &object_key, b"").await;
    let uri = format!("/documents/{document_id}/download");
    let app = http::router(state);

    let full = app
        .clone()
        .oneshot(authed_get(&uri))
        .await
        .expect("empty response");
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(full.headers()["content-length"], "0");
    assert!(
        to_bytes(full.into_body(), 0)
            .await
            .expect("empty body")
            .is_empty()
    );
    assert_eq!(stream_calls.load(Ordering::SeqCst), 1);

    let ranged = app
        .oneshot(authed_get_with_range(&uri, "bytes=0-0"))
        .await
        .expect("empty range response");
    assert_eq!(ranged.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(ranged.headers()["content-range"], "bytes */0");
    assert_eq!(stream_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn logical_five_gibibyte_download_streams_before_eof_and_releases_capacity_on_drop() {
    let logical_size = 5 * 1024 * 1024 * 1024_u64;
    let active_streams = Arc::new(AtomicUsize::new(0));
    let dropped_streams = Arc::new(AtomicUsize::new(0));
    let legacy_read_called = Arc::new(AtomicBool::new(false));
    let produced_chunks = Arc::new(AtomicUsize::new(0));
    let waiting = Arc::new(Notify::new());
    let object_key = "logical-five-gibibytes".to_string();
    let storage = Arc::new(GatedStreamStorage {
        active_streams: Arc::clone(&active_streams),
        chunk_bytes: 64 * 1024,
        dropped_streams: Arc::clone(&dropped_streams),
        fail_after_first: false,
        legacy_read_called: Arc::clone(&legacy_read_called),
        logical_size,
        object_key: object_key.clone(),
        produced_chunks: Arc::clone(&produced_chunks),
        release: Arc::new(Notify::new()),
        waiting: Arc::clone(&waiting),
    });
    let (state, _temp_dir) = test_state_with_limit(storage, 1).await;
    let project_id = grant_reader_project(&state).await;
    let document_id = insert_downloadable_document_metadata(
        &state.db,
        project_id,
        &object_key,
        &"0".repeat(64),
        i64::try_from(logical_size).expect("logical size"),
    )
    .await;
    let uri = format!("/documents/{document_id}/download");
    let app = http::router(state);

    let response = timeout(
        Duration::from_secs(5),
        app.clone().oneshot(authed_get(&uri)),
    )
    .await
    .expect("response returned before source EOF")
    .expect("download response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-length"],
        logical_size.to_string()
    );
    let mut body = response.into_body().into_data_stream();
    let first = timeout(Duration::from_secs(5), body.next())
        .await
        .expect("first chunk arrived before source EOF")
        .expect("first chunk")
        .expect("first chunk result");
    assert_eq!(first.len(), 64 * 1024);
    assert_eq!(produced_chunks.load(Ordering::SeqCst), 1);
    assert_eq!(active_streams.load(Ordering::SeqCst), 1);
    assert!(!legacy_read_called.load(Ordering::SeqCst));

    let blocked_read = tokio::spawn(async move {
        let _ = body.next().await;
    });
    timeout(Duration::from_secs(5), waiting.notified())
        .await
        .expect("second source read became pending");

    let at_capacity = app
        .clone()
        .oneshot(authed_get(&uri))
        .await
        .expect("capacity response");
    assert_eq!(at_capacity.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(at_capacity.headers()["retry-after"], "1");
    assert_eq!(active_streams.load(Ordering::SeqCst), 1);

    blocked_read.abort();
    let _ = blocked_read.await;
    timeout(Duration::from_secs(5), async {
        while active_streams.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropped response cancelled source");
    assert_eq!(dropped_streams.load(Ordering::SeqCst), 1);

    let replacement = app
        .oneshot(authed_get(&uri))
        .await
        .expect("replacement response");
    assert_eq!(replacement.status(), StatusCode::OK);
    drop(replacement);
}

#[tokio::test]
async fn midstream_storage_failure_is_not_reported_as_clean_eof_and_releases_capacity() {
    let logical_size = 128 * 1024_u64;
    let active_streams = Arc::new(AtomicUsize::new(0));
    let object_key = "midstream-failure".to_string();
    let storage = Arc::new(GatedStreamStorage {
        active_streams: Arc::clone(&active_streams),
        chunk_bytes: 64 * 1024,
        dropped_streams: Arc::new(AtomicUsize::new(0)),
        fail_after_first: true,
        legacy_read_called: Arc::new(AtomicBool::new(false)),
        logical_size,
        object_key: object_key.clone(),
        produced_chunks: Arc::new(AtomicUsize::new(0)),
        release: Arc::new(Notify::new()),
        waiting: Arc::new(Notify::new()),
    });
    let (state, _temp_dir) = test_state_with_limit(storage, 1).await;
    let project_id = grant_reader_project(&state).await;
    let document_id = insert_downloadable_document_metadata(
        &state.db,
        project_id,
        &object_key,
        &"1".repeat(64),
        i64::try_from(logical_size).expect("logical size"),
    )
    .await;
    let uri = format!("/documents/{document_id}/download");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_get(&uri))
        .await
        .expect("download response");
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.expect("first chunk").is_ok());
    assert!(body.next().await.expect("failure frame").is_err());
    assert_eq!(active_streams.load(Ordering::SeqCst), 0);
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT message FROM document_events WHERE document_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(document_id)
        .fetch_one(&pool)
        .await
        .expect("download initiation event"),
        "Started download of Project/download-name.txt",
    );

    let retry = app.oneshot(authed_get(&uri)).await.expect("retry response");
    assert_eq!(retry.status(), StatusCode::OK);
}

#[tokio::test]
async fn download_prefers_active_bucket_and_fails_over_before_exposing_bytes() {
    let data = b"healthy failover bytes".to_vec();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let storage = Arc::new(FailoverStorage {
        calls: Arc::clone(&calls),
        data: data.clone(),
    });
    let (state, _temp_dir) = test_state(storage).await;
    let project_id = grant_reader_project(&state).await;
    let document_id =
        insert_downloadable_document(&state.db, project_id, "stale-local-copy", &data).await;
    let blob_id: i64 =
        sqlx::query_scalar("SELECT blob_id FROM document_versions WHERE document_id = ?")
            .bind(document_id)
            .fetch_one(&state.db)
            .await
            .expect("download blob id");
    for (bucket, object_key) in [
        ("", "healthy-legacy"),
        ("other-bucket", "wrong-bucket"),
        ("active-bucket", "missing-exact"),
        ("active-bucket", "bad-first-frame"),
        ("active-bucket", "healthy-exact"),
    ] {
        sqlx::query(
            "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 's3', ?, ?)",
        )
        .bind(blob_id)
        .bind(bucket)
        .bind(object_key)
        .execute(&state.db)
        .await
        .expect("alternate blob location");
    }

    let pool = state.db.clone();
    let app = http::router(state);
    let response = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{document_id}/download")))
        .await
        .expect("failover response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), data.len())
            .await
            .expect("failover body"),
        data
    );
    assert_eq!(
        *calls.lock().expect("failover calls"),
        ["missing-exact", "bad-first-frame", "healthy-exact"]
    );

    calls.lock().expect("failover calls").clear();
    sqlx::query(
        "UPDATE blob_locations SET object_key = 'missing-exact-two' WHERE object_key = 'healthy-exact'",
    )
    .execute(&pool)
    .await
    .expect("make exact copies unavailable");
    let legacy = app
        .oneshot(authed_get(&format!("/documents/{document_id}/download")))
        .await
        .expect("legacy fallback response");
    assert_eq!(legacy.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(legacy.into_body(), data.len())
            .await
            .expect("legacy fallback body"),
        data
    );
    assert_eq!(
        *calls.lock().expect("failover calls"),
        [
            "missing-exact",
            "bad-first-frame",
            "missing-exact-two",
            "healthy-legacy",
        ]
    );
}

#[tokio::test]
async fn download_never_switches_locations_after_the_first_frame() {
    let data = b"midstream failover must not splice".to_vec();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let storage = Arc::new(FailoverStorage {
        calls: Arc::clone(&calls),
        data: data.clone(),
    });
    let (state, _temp_dir) = test_state(storage).await;
    let project_id = grant_reader_project(&state).await;
    let document_id = insert_downloadable_document(&state.db, project_id, "old-local", &data).await;
    let blob_id: i64 =
        sqlx::query_scalar("SELECT blob_id FROM document_versions WHERE document_id = ?")
            .bind(document_id)
            .fetch_one(&state.db)
            .await
            .expect("download blob id");
    for object_key in ["midstream-failure", "healthy-after-midstream"] {
        sqlx::query(
            "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 's3', 'active-bucket', ?)",
        )
        .bind(blob_id)
        .bind(object_key)
        .execute(&state.db)
        .await
        .expect("alternate blob location");
    }

    let response = http::router(state)
        .oneshot(authed_get(&format!("/documents/{document_id}/download")))
        .await
        .expect("midstream response");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    assert_eq!(
        body.next()
            .await
            .expect("prefetched frame")
            .expect("prefetched bytes"),
        &data[..4]
    );
    assert!(body.next().await.expect("failure frame").is_err());
    assert_eq!(
        *calls.lock().expect("failover calls"),
        ["midstream-failure"]
    );
}

#[tokio::test]
async fn transient_alternate_failure_is_not_masked_by_a_missing_copy() {
    let data = b"busy failover".to_vec();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let storage = Arc::new(FailoverStorage {
        calls: Arc::clone(&calls),
        data: data.clone(),
    });
    let (state, _temp_dir) = test_state(storage).await;
    let project_id = grant_reader_project(&state).await;
    let document_id = insert_downloadable_document(&state.db, project_id, "old-local", &data).await;
    let blob_id: i64 =
        sqlx::query_scalar("SELECT blob_id FROM document_versions WHERE document_id = ?")
            .bind(document_id)
            .fetch_one(&state.db)
            .await
            .expect("download blob id");
    for object_key in ["missing-exact", "busy-exact"] {
        sqlx::query(
            "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 's3', 'active-bucket', ?)",
        )
        .bind(blob_id)
        .bind(object_key)
        .execute(&state.db)
        .await
        .expect("alternate blob location");
    }

    let response = http::router(state)
        .oneshot(authed_get(&format!("/documents/{document_id}/download")))
        .await
        .expect("busy response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(
        *calls.lock().expect("failover calls"),
        ["missing-exact", "busy-exact"]
    );
}

#[tokio::test]
async fn oversized_backend_frame_is_rejected_and_releases_capacity() {
    let logical_size = u64::try_from(STORAGE_CHUNK_SIZE + 1).expect("logical size");
    let active_streams = Arc::new(AtomicUsize::new(0));
    let object_key = "oversized-frame".to_string();
    let storage = Arc::new(GatedStreamStorage {
        active_streams: Arc::clone(&active_streams),
        chunk_bytes: STORAGE_CHUNK_SIZE + 1,
        dropped_streams: Arc::new(AtomicUsize::new(0)),
        fail_after_first: false,
        legacy_read_called: Arc::new(AtomicBool::new(false)),
        logical_size,
        object_key: object_key.clone(),
        produced_chunks: Arc::new(AtomicUsize::new(0)),
        release: Arc::new(Notify::new()),
        waiting: Arc::new(Notify::new()),
    });
    let (state, _temp_dir) = test_state_with_limit(storage, 1).await;
    let project_id = grant_reader_project(&state).await;
    let document_id = insert_downloadable_document_metadata(
        &state.db,
        project_id,
        &object_key,
        &"2".repeat(64),
        i64::try_from(logical_size).expect("database size"),
    )
    .await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(&format!("/documents/{document_id}/download")))
        .await
        .expect("download response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(active_streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn final_frame_releases_source_but_holds_capacity_until_body_drop() {
    let logical_size = 64 * 1024_u64;
    let active_streams = Arc::new(AtomicUsize::new(0));
    let object_key = "single-frame".to_string();
    let storage = Arc::new(GatedStreamStorage {
        active_streams: Arc::clone(&active_streams),
        chunk_bytes: 64 * 1024,
        dropped_streams: Arc::new(AtomicUsize::new(0)),
        fail_after_first: false,
        legacy_read_called: Arc::new(AtomicBool::new(false)),
        logical_size,
        object_key: object_key.clone(),
        produced_chunks: Arc::new(AtomicUsize::new(0)),
        release: Arc::new(Notify::new()),
        waiting: Arc::new(Notify::new()),
    });
    let (state, _temp_dir) = test_state_with_limit(storage, 1).await;
    let project_id = grant_reader_project(&state).await;
    let document_id = insert_downloadable_document_metadata(
        &state.db,
        project_id,
        &object_key,
        &"3".repeat(64),
        i64::try_from(logical_size).expect("database size"),
    )
    .await;
    let uri = format!("/documents/{document_id}/download");
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_get(&uri))
        .await
        .expect("download response");
    let mut body = response.into_body().into_data_stream();
    assert_eq!(
        body.next()
            .await
            .expect("final frame")
            .expect("final frame bytes")
            .len(),
        64 * 1024
    );
    assert_eq!(active_streams.load(Ordering::SeqCst), 0);

    let at_capacity = app
        .clone()
        .oneshot(authed_get(&uri))
        .await
        .expect("capacity response");
    assert_eq!(at_capacity.status(), StatusCode::SERVICE_UNAVAILABLE);

    drop(body);
    let next = app
        .oneshot(authed_get(&uri))
        .await
        .expect("next download response");
    assert_eq!(next.status(), StatusCode::OK);
}
