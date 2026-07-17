use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::time::timeout;
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::http::{self, AppState};
use vault_server::storage::{
    BlobByteStream, BlobReadRange, BlobStorageBackend, BlobWriteKind, LocalBlobStorage,
    SharedBlobStorage, StorageError, StoredBlob,
};

#[derive(Debug)]
enum ReadinessBehavior {
    Local(LocalBlobStorage),
    Failure,
    Busy,
    Pending,
}

#[derive(Debug)]
struct ReadinessStorage {
    behavior: ReadinessBehavior,
    checks: AtomicUsize,
}

impl ReadinessStorage {
    fn new(behavior: ReadinessBehavior) -> Self {
        Self {
            behavior,
            checks: AtomicUsize::new(0),
        }
    }

    fn check_count(&self) -> usize {
        self.checks.load(Ordering::SeqCst)
    }
}

fn unexpected_storage_operation() -> StorageError {
    StorageError::UnsupportedOperation("health test readiness probe only".to_string())
}

#[async_trait]
impl BlobStorageBackend for ReadinessStorage {
    fn name(&self) -> &'static str {
        "local"
    }

    fn bucket(&self) -> &'static str {
        ""
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        Err(unexpected_storage_operation())
    }

    async fn readiness_check(&self) -> Result<(), StorageError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        match &self.behavior {
            ReadinessBehavior::Local(storage) => storage.readiness_check().await,
            ReadinessBehavior::Failure => Err(StorageError::Remote(
                "secret.internal.example/storage".to_string(),
            )),
            ReadinessBehavior::Busy => Err(StorageError::Busy),
            ReadinessBehavior::Pending => pending().await,
        }
    }

    async fn put_bytes(&self, _data: &[u8]) -> Result<StoredBlob, StorageError> {
        Err(unexpected_storage_operation())
    }

    async fn put_file(
        &self,
        _source_path: &Path,
        _digest: &str,
        _size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        Err(unexpected_storage_operation())
    }

    async fn put_part_files(
        &self,
        _part_paths: &[PathBuf],
        _expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        Err(unexpected_storage_operation())
    }

    fn planned_object_key(
        &self,
        _hash_algo: &str,
        _digest: &str,
        _write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        Err(unexpected_storage_operation())
    }

    async fn read_bytes(&self, _object_key: &str) -> Result<Vec<u8>, StorageError> {
        Err(unexpected_storage_operation())
    }

    async fn read_range(
        &self,
        _object_key: &str,
        _start: u64,
        _end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        Err(unexpected_storage_operation())
    }

    async fn stream_range(
        &self,
        _object_key: &str,
        _range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        Err(unexpected_storage_operation())
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        Err(unexpected_storage_operation())
    }

    async fn delete_object(&self, _object_key: &str) -> Result<(), StorageError> {
        Err(unexpected_storage_operation())
    }
}

#[tokio::test]
async fn health_remains_process_only_when_dependencies_are_unavailable() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp_dir.path());
    let storage = Arc::new(ReadinessStorage::new(ReadinessBehavior::Pending));
    let state = test_state(config, storage.clone()).await;
    state.db.close().await;

    let (status, body) = get(http::router(state), "/health", Duration::from_secs(1)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
    assert_eq!(storage.check_count(), 0);
}

#[tokio::test]
async fn readiness_checks_database_and_storage_without_leaving_probe_artifacts() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp_dir.path());
    let objects_path = config.objects_path();
    let local_storage = LocalBlobStorage::new(&objects_path, &config.storage_prefix);
    local_storage
        .ensure()
        .await
        .expect("startup storage ensure");
    let storage = Arc::new(ReadinessStorage::new(ReadinessBehavior::Local(
        local_storage,
    )));
    let state = test_state(config, storage.clone()).await;

    let (status, body) = get(http::router(state), "/api/health", Duration::from_secs(3)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_body(&body),
        json!({
            "ok": true,
            "database": true,
            "storage": true,
            "storage_backend": "local"
        })
    );
    assert_eq!(storage.check_count(), 1);
    assert!(objects_path.is_dir());
    assert!(
        std::fs::read_dir(objects_path)
            .expect("objects directory")
            .next()
            .is_none(),
        "readiness probe left a storage artifact"
    );
}

#[tokio::test]
async fn readiness_reports_database_failure_without_leaking_details() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp_dir.path());
    let local_storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    local_storage
        .ensure()
        .await
        .expect("startup storage ensure");
    let storage = Arc::new(ReadinessStorage::new(ReadinessBehavior::Local(
        local_storage,
    )));
    let state = test_state(config, storage.clone()).await;
    state.db.close().await;

    let (status, body) = get(http::router(state), "/api/health", Duration::from_secs(3)).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        json_body(&body),
        json!({
            "ok": false,
            "database": false,
            "storage": true,
            "storage_backend": "local"
        })
    );
    assert_eq!(storage.check_count(), 1);
}

#[tokio::test]
async fn readiness_does_not_recreate_an_absent_local_storage_root() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp_dir.path());
    let objects_path = config.objects_path();
    assert!(!objects_path.exists());
    let storage = LocalBlobStorage::new(&objects_path, &config.storage_prefix);
    let state = test_state(config, Arc::new(storage)).await;

    let (status, body) = get(http::router(state), "/api/health", Duration::from_secs(3)).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        json_body(&body),
        json!({
            "ok": false,
            "database": true,
            "storage": false,
            "storage_backend": "local"
        })
    );
    assert!(!objects_path.exists());
}

#[tokio::test]
async fn readiness_redacts_storage_failure_busy_and_timeout() {
    for behavior in [
        ReadinessBehavior::Failure,
        ReadinessBehavior::Busy,
        ReadinessBehavior::Pending,
    ] {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(temp_dir.path());
        let storage = Arc::new(ReadinessStorage::new(behavior));
        let state = test_state(config, storage.clone()).await;
        let (status, body) = get(http::router(state), "/api/health", Duration::from_secs(3)).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(&body),
            json!({
                "ok": false,
                "database": true,
                "storage": false,
                "storage_backend": "local"
            })
        );
        assert_eq!(storage.check_count(), 1);
        assert!(!String::from_utf8_lossy(&body).contains("secret.internal"));
    }
}

#[tokio::test]
async fn readiness_reports_unusable_local_storage_without_probe_debris() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(temp_dir.path());
    let objects_path = temp_dir.path().join("objects-is-a-file");
    std::fs::write(&objects_path, b"sentinel").expect("unusable objects path");
    config.objects_path = Some(objects_path.clone());
    let storage = LocalBlobStorage::new(&objects_path, &config.storage_prefix);
    let state = test_state(config, Arc::new(storage)).await;

    let (status, body) = get(http::router(state), "/api/health", Duration::from_secs(3)).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        json_body(&body),
        json!({
            "ok": false,
            "database": true,
            "storage": false,
            "storage_backend": "local"
        })
    );
    assert_eq!(
        std::fs::read(&objects_path).expect("sentinel path"),
        b"sentinel"
    );
    assert!(!String::from_utf8_lossy(&body).contains("objects-is-a-file"));
}

#[cfg(unix)]
#[tokio::test]
async fn readiness_rejects_symlinked_storage_root_without_touching_target() {
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let outside = temp_dir.path().join("outside");
    std::fs::create_dir(&outside).expect("outside directory");
    let sentinel = outside.join("sentinel");
    std::fs::write(&sentinel, b"outside bytes").expect("outside sentinel");
    let objects_path = temp_dir.path().join("objects-link");
    symlink(&outside, &objects_path).expect("objects symlink");
    let mut config = test_config(temp_dir.path());
    config.objects_path = Some(objects_path.clone());
    let storage = LocalBlobStorage::new(&objects_path, &config.storage_prefix);
    let state = test_state(config, Arc::new(storage)).await;

    let (status, body) = get(http::router(state), "/api/health", Duration::from_secs(3)).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        json_body(&body),
        json!({
            "ok": false,
            "database": true,
            "storage": false,
            "storage_backend": "local"
        })
    );
    assert_eq!(
        std::fs::read(&sentinel).expect("outside bytes"),
        b"outside bytes"
    );
    assert_eq!(
        std::fs::read_dir(&outside)
            .expect("outside directory")
            .count(),
        1
    );
    assert!(
        std::fs::symlink_metadata(&objects_path)
            .expect("objects symlink")
            .file_type()
            .is_symlink()
    );
}

#[tokio::test]
async fn production_router_does_not_register_benchmark_sink() {
    let (_temp_dir, app) = test_app().await;

    for method in [Method::GET, Method::PUT] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/api/bench/sink")
                    .body(Body::from("small probe"))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

async fn test_app() -> (tempfile::TempDir, axum::Router) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp_dir.path());
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    let state = test_state(config, Arc::new(storage)).await;
    (temp_dir, http::router(state))
}

fn test_config(data_dir: &Path) -> Config {
    Config {
        host: "127.0.0.1".parse().expect("host"),
        port: 0,
        data_dir: data_dir.to_path_buf(),
        db_path: Some(data_dir.join("vault.db")),
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
    }
}

async fn test_state(config: Config, storage: SharedBlobStorage) -> AppState {
    let db = db::connect(&config.db_path()).await.expect("db");
    AppState::new(config, AuthSettings::default(), db, storage)
}

async fn get(app: axum::Router, path: &str, deadline: Duration) -> (StatusCode, Vec<u8>) {
    let response = timeout(
        deadline,
        app.oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        ),
    )
    .await
    .expect("response timeout")
    .expect("response");
    let status = response.status();
    let body = to_bytes(response.into_body(), 4096)
        .await
        .expect("response body")
        .to_vec();
    (status, body)
}

fn json_body(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("JSON response")
}
