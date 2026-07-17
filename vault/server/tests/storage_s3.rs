use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{delete, get, head, put};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};
use tokio::time::timeout;
use vault_server::blob_lifecycle::{begin_blob_publication, collect_unreferenced_blobs};
use vault_server::db;
use vault_server::storage::{
    BlobReadRange, BlobStorageBackend, BlobWriteKind, S3_UPLOAD_STAGE_FILENAME,
    S3CompatibleBlobStorage, S3StorageSettings, STORAGE_CHUNK_SIZE, StorageError,
    remove_s3_upload_stage_file, sweep_legacy_s3_stage_files,
};

type ObjectMap = Arc<Mutex<HashMap<String, Vec<u8>>>>;

#[derive(Clone, Default)]
struct BlockedPutState {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[tokio::test]
async fn s3_compatible_storage_puts_reads_ranges_and_deletes_objects() {
    let endpoint_url = start_s3_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");

    let content = b"hello remote storage";
    let digest = sha256_hex(content);
    let stored = storage.put_bytes(content).await.expect("put bytes");

    assert_eq!(stored.backend, "s3");
    assert_eq!(stored.bucket, "vault-test");
    assert_eq!(stored.hash_algo, "sha256");
    assert_eq!(stored.digest, digest);
    assert_eq!(stored.object_key, format!("objects/sha256/{digest}"));
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("read bytes"),
        content,
    );
    assert_eq!(
        storage
            .read_range(&stored.object_key, 6, 11)
            .await
            .expect("read range"),
        b"remote",
    );
    let mut stream = storage
        .stream_range(
            &stored.object_key,
            BlobReadRange {
                expected_size: content.len() as u64,
                offset: 6,
                length: 6,
            },
        )
        .await
        .expect("range stream");
    let mut streamed = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("range stream chunk");
        assert!(chunk.len() <= STORAGE_CHUNK_SIZE);
        streamed.extend_from_slice(&chunk);
    }
    assert_eq!(streamed, b"remote");

    storage
        .delete_object(&stored.object_key)
        .await
        .expect("delete object");
    assert!(matches!(
        storage.read_bytes(&stored.object_key).await,
        Err(StorageError::NotFound),
    ));
}

#[tokio::test]
async fn s3_compatible_storage_overwrites_existing_digest_key_with_new_bytes() {
    let (endpoint_url, objects) = start_s3_mock_with_objects().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let content = b"correct remote bytes";
    let digest = sha256_hex(content);
    let object_key = format!("objects/sha256/{digest}");
    objects
        .lock()
        .await
        .insert(object_key.clone(), b"wrong remote bytes".to_vec());

    let stored = storage.put_bytes(content).await.expect("put bytes");

    assert_eq!(stored.object_key, object_key);
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("read repaired remote"),
        content,
    );
}

#[tokio::test]
async fn s3_full_object_stream_is_bounded_across_multiple_mebibytes() {
    let (endpoint_url, objects) = start_s3_mock_with_objects().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let size = STORAGE_CHUNK_SIZE * 2 + 17;
    let object_key = "objects/large-stream".to_string();
    objects
        .lock()
        .await
        .insert(object_key.clone(), vec![b'z'; size]);

    let mut stream = storage
        .stream_range(
            &object_key,
            BlobReadRange {
                expected_size: size as u64,
                offset: 0,
                length: size as u64,
            },
        )
        .await
        .expect("full object stream");
    let mut streamed = 0_usize;
    let mut chunks = 0_usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("full object chunk");
        assert!(!chunk.is_empty());
        assert!(chunk.len() <= STORAGE_CHUNK_SIZE);
        streamed += chunk.len();
        chunks += 1;
    }

    assert_eq!(streamed, size);
    assert!(chunks >= 3);
}

#[tokio::test]
async fn s3_range_stream_rejects_a_provider_that_ignores_the_range() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let addr = listener.local_addr().expect("listener address");
    let app = Router::new().route(
        "/{bucket}/{*key}",
        get(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, 6)
                .body(Body::from("abcdef"))
                .expect("ignored range response")
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("ignored range mock");
    });
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-test".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url(addr)),
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");

    let result = storage
        .stream_range(
            "object",
            BlobReadRange {
                expected_size: 6,
                offset: 2,
                length: 2,
            },
        )
        .await;

    assert!(matches!(result, Err(StorageError::ContentMismatch)));
}

#[tokio::test]
async fn s3_compatible_storage_rejects_missing_bucket_configuration() {
    let error = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "r2".to_string(),
        bucket: String::new(),
        region: "auto".to_string(),
        endpoint_url: Some("http://127.0.0.1:1".to_string()),
        access_key_id: Some("access".to_string()),
        secret_access_key: Some("secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect_err("missing bucket error");

    assert!(matches!(error, StorageError::Configuration(_)));
    assert!(error.to_string().contains("VAULT_R2_BUCKET"));
}

#[test]
fn s3_storage_settings_use_vault_env_with_aws_credential_fallbacks() {
    let env = HashMap::from([
        ("VAULT_S3_BUCKET", "vault-prod"),
        ("VAULT_S3_REGION", "us-west-2"),
        ("VAULT_S3_ENDPOINT_URL", "https://s3.example.test"),
        ("AWS_ACCESS_KEY_ID", "aws-access"),
        ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
        ("AWS_SESSION_TOKEN", "aws-session"),
    ]);

    let settings = S3StorageSettings::s3_from_env_with("tenant-a", |name| {
        env.get(name).map(|value| (*value).to_string())
    });

    assert_eq!(settings.name, "s3");
    assert_eq!(settings.bucket, "vault-prod");
    assert_eq!(settings.region, "us-west-2");
    assert_eq!(
        settings.endpoint_url.as_deref(),
        Some("https://s3.example.test")
    );
    assert_eq!(settings.access_key_id.as_deref(), Some("aws-access"));
    assert_eq!(settings.secret_access_key.as_deref(), Some("aws-secret"));
    assert_eq!(settings.session_token.as_deref(), Some("aws-session"));
    assert_eq!(settings.prefix, "tenant-a");
}

#[test]
fn r2_storage_settings_derive_endpoint_from_account_id() {
    let env = HashMap::from([
        ("VAULT_R2_BUCKET", "vault-r2"),
        ("VAULT_R2_ACCOUNT_ID", "acct123"),
        ("VAULT_R2_ACCESS_KEY_ID", "r2-access"),
        ("VAULT_R2_SECRET_ACCESS_KEY", "r2-secret"),
    ]);

    let settings = S3StorageSettings::r2_from_env_with("objects", |name| {
        env.get(name).map(|value| (*value).to_string())
    });

    assert_eq!(settings.name, "r2");
    assert_eq!(settings.bucket, "vault-r2");
    assert_eq!(settings.region, "auto");
    assert_eq!(
        settings.endpoint_url.as_deref(),
        Some("https://acct123.r2.cloudflarestorage.com"),
    );
    assert_eq!(settings.access_key_id.as_deref(), Some("r2-access"));
    assert_eq!(settings.secret_access_key.as_deref(), Some("r2-secret"));
    assert_eq!(settings.session_token, None);
    assert_eq!(settings.prefix, "objects");
}

#[tokio::test]
async fn s3_compatible_storage_promotes_part_files_as_content_addressed_object() {
    let endpoint_url = start_s3_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "r2".to_string(),
        bucket: "vault-parts".to_string(),
        region: "auto".to_string(),
        endpoint_url: Some(endpoint_url),
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "tenant-a".to_string(),
    })
    .await
    .expect("r2 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let first = temp_dir.path().join("00000001.part");
    let second = temp_dir.path().join("00000002.part");
    tokio::fs::write(&first, b"hello ")
        .await
        .expect("first part");
    tokio::fs::write(&second, b"world")
        .await
        .expect("second part");
    let combined = b"hello world";
    let digest = sha256_hex(combined);

    let stored = storage
        .put_part_files_in_staging(&[first, second], Some(&digest), temp_dir.path())
        .await
        .expect("put part files");

    assert_eq!(stored.backend, "r2");
    assert_eq!(stored.bucket, "vault-parts");
    assert_eq!(stored.digest, digest);
    assert_eq!(stored.size_bytes, combined.len() as u64);
    assert_eq!(stored.object_key, format!("tenant-a/sha256/{digest}"));
    assert!(!temp_dir.path().join(S3_UPLOAD_STAGE_FILENAME).exists());
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("uploaded object"),
        combined,
    );
}

#[tokio::test]
async fn s3_compatible_storage_rejects_part_file_checksum_mismatch_without_uploading() {
    let endpoint_url = start_s3_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-parts".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let part = temp_dir.path().join("00000001.part");
    tokio::fs::write(&part, b"actual bytes")
        .await
        .expect("part");
    let actual_digest = sha256_hex(b"actual bytes");
    let wrong_digest = sha256_hex(b"different bytes");

    let error = storage
        .put_part_files_in_staging(&[part], Some(&wrong_digest), temp_dir.path())
        .await
        .expect_err("checksum mismatch");

    assert!(matches!(error, StorageError::ChecksumMismatch));
    assert!(!temp_dir.path().join(S3_UPLOAD_STAGE_FILENAME).exists());
    assert!(matches!(
        storage
            .read_bytes(&format!("objects/sha256/{actual_digest}"))
            .await,
        Err(StorageError::NotFound),
    ));
}

#[tokio::test]
async fn s3_staged_part_upload_supports_empty_objects() {
    let endpoint_url = start_s3_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-parts".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let digest = sha256_hex(&[]);

    let stored = storage
        .put_part_files_in_staging(&[], Some(&digest), temp_dir.path())
        .await
        .expect("empty staged upload");

    assert_eq!(stored.digest, digest);
    assert_eq!(stored.size_bytes, 0);
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("empty object"),
        b"",
    );
    assert!(!temp_dir.path().join(S3_UPLOAD_STAGE_FILENAME).exists());
}

#[tokio::test]
async fn s3_part_staging_is_session_local_and_cancel_safe() {
    let (endpoint_url, blocked_put) = start_blocked_s3_put_mock().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-parts".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let session_dir = temp_dir.path().join("transfers/uploads/session");
    tokio::fs::create_dir_all(&session_dir)
        .await
        .expect("session dir");
    let first = session_dir.join("00000001.part");
    let second = session_dir.join("00000002.part");
    tokio::fs::write(&first, b"hello ")
        .await
        .expect("first part");
    tokio::fs::write(&second, b"world")
        .await
        .expect("second part");
    let digest = sha256_hex(b"hello world");
    let task_session_dir = session_dir.clone();
    let upload = tokio::spawn(async move {
        storage
            .put_part_files_in_staging(&[first, second], Some(&digest), &task_session_dir)
            .await
    });

    timeout(Duration::from_secs(5), blocked_put.entered.notified())
        .await
        .expect("S3 PUT started");
    let stage_path = session_dir.join(S3_UPLOAD_STAGE_FILENAME);
    assert_eq!(
        tokio::fs::read(&stage_path).await.expect("stage bytes"),
        b"hello world",
    );

    upload.abort();
    let error = upload.await.expect_err("cancelled upload task");
    assert!(error.is_cancelled());
    assert!(!stage_path.exists());
    blocked_put.release.notify_waiters();
}

#[cfg(unix)]
#[tokio::test]
async fn s3_stage_cleanup_refuses_symlinks() {
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let session_dir = temp_dir.path().join("session");
    tokio::fs::create_dir_all(&session_dir)
        .await
        .expect("session dir");
    let outside = temp_dir.path().join("outside");
    tokio::fs::write(&outside, b"outside")
        .await
        .expect("outside file");
    let stage_path = session_dir.join(S3_UPLOAD_STAGE_FILENAME);
    symlink(&outside, &stage_path).expect("stage symlink");

    let error = remove_s3_upload_stage_file(&session_dir)
        .await
        .expect_err("symlink must be refused");

    assert!(matches!(error, StorageError::InvalidStoragePath));
    assert!(
        tokio::fs::symlink_metadata(&stage_path)
            .await
            .expect("stage metadata")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        tokio::fs::read(&outside).await.expect("outside bytes"),
        b"outside",
    );
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_s3_stage_sweep_is_aged_bounded_and_symlink_safe() {
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let legacy_name = |digit: char| format!("vault-s3-upload-{}.tmp", digit.to_string().repeat(32));
    let old_name = legacy_name('0');
    let fresh_name = legacy_name('1');
    let symlink_name = legacy_name('2');
    let directory_name = legacy_name('3');
    let near_miss_name = format!("vault-s3-upload-{}.tmp", "A".repeat(32));
    let old_path = temp_dir.path().join(&old_name);
    tokio::fs::write(&old_path, b"old stage")
        .await
        .expect("old stage");
    tokio::fs::write(temp_dir.path().join(&fresh_name), b"fresh stage")
        .await
        .expect("fresh stage");
    tokio::fs::create_dir(temp_dir.path().join(&directory_name))
        .await
        .expect("lookalike directory");
    tokio::fs::write(temp_dir.path().join(&near_miss_name), b"near miss")
        .await
        .expect("near-miss stage");
    let outside = temp_dir.path().join("outside");
    tokio::fs::write(&outside, b"outside")
        .await
        .expect("outside file");
    symlink(&outside, temp_dir.path().join(&symlink_name)).expect("legacy stage symlink");
    std::fs::File::open(&old_path)
        .expect("old stage handle")
        .set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_hours(2)),
        )
        .expect("old stage mtime");

    assert!(
        sweep_legacy_s3_stage_files(temp_dir.path(), Duration::ZERO, 0)
            .await
            .expect("zero-work sweep")
            .is_empty()
    );
    assert!(old_path.is_file());
    assert!(matches!(
        sweep_legacy_s3_stage_files(std::path::Path::new("/."), Duration::ZERO, 1,).await,
        Err(StorageError::InvalidStoragePath)
    ));

    let deleted = sweep_legacy_s3_stage_files(temp_dir.path(), Duration::from_hours(1), 128)
        .await
        .expect("legacy stage sweep");

    assert_eq!(deleted, vec![old_name]);
    assert!(temp_dir.path().join(fresh_name).is_file());
    assert!(temp_dir.path().join(directory_name).is_dir());
    assert!(temp_dir.path().join(near_miss_name).is_file());
    assert!(
        tokio::fs::symlink_metadata(temp_dir.path().join(symlink_name))
            .await
            .expect("legacy symlink metadata")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        tokio::fs::read(outside).await.expect("outside bytes"),
        b"outside"
    );
}

#[tokio::test]
async fn blob_lifecycle_garbage_collection_deletes_s3_object_and_metadata() {
    let (endpoint_url, objects) = start_s3_mock_with_objects().await;
    let storage = S3CompatibleBlobStorage::from_settings(S3StorageSettings {
        name: "s3".to_string(),
        bucket: "vault-gc".to_string(),
        region: "us-east-1".to_string(),
        endpoint_url: Some(endpoint_url),
        access_key_id: Some("test-access".to_string()),
        secret_access_key: Some("test-secret".to_string()),
        session_token: None,
        prefix: "objects".to_string(),
    })
    .await
    .expect("s3 storage");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp_dir.path().join("vault.db"))
        .await
        .expect("database");
    let content = b"remote garbage collection";
    let digest = sha256_hex(content);
    let publication = begin_blob_publication(
        &pool,
        &storage,
        "sha256",
        &digest,
        content.len() as u64,
        BlobWriteKind::Bytes,
    )
    .await
    .expect("publication lease");
    let stored = publication
        .run_storage(storage.put_bytes(content))
        .await
        .expect("put object");
    let mut transaction = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("metadata transaction");
    let blob_id = publication
        .prepare_metadata_in_tx(&mut transaction, &stored)
        .await
        .expect("prepare metadata");
    publication
        .finish_metadata_in_tx(&mut transaction)
        .await
        .expect("finish metadata");
    transaction.commit().await.expect("commit metadata");
    drop(publication);

    let result = collect_unreferenced_blobs(&pool, &storage)
        .await
        .expect("garbage collection");

    assert_eq!(result.deleted_blob_ids, vec![blob_id]);
    assert_eq!(result.deleted_objects, vec![stored.object_key.clone()]);
    assert!(result.failures.is_empty());
    assert!(!objects.lock().await.contains_key(&stored.object_key));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM blobs WHERE id = ?")
            .bind(blob_id)
            .fetch_one(&pool)
            .await
            .expect("blob count"),
        0,
    );
}

async fn start_s3_mock() -> String {
    start_s3_mock_with_objects().await.0
}

async fn start_s3_mock_with_objects() -> (String, ObjectMap) {
    let objects = ObjectMap::default();
    let app = Router::new()
        .route("/{bucket}/{*key}", head(mock_head_object))
        .route("/{bucket}/{*key}", put(mock_put_object))
        .route("/{bucket}/{*key}", get(mock_get_object))
        .route("/{bucket}/{*key}", delete(mock_delete_object))
        .with_state(objects.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("s3 mock");
    });
    (endpoint_url(addr), objects)
}

async fn start_blocked_s3_put_mock() -> (String, BlockedPutState) {
    let state = BlockedPutState::default();
    let app = Router::new()
        .route("/{bucket}/{*key}", put(mock_blocked_put))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("blocked S3 mock");
    });
    (endpoint_url(addr), state)
}

async fn mock_blocked_put(State(state): State<BlockedPutState>, _body: Body) -> StatusCode {
    state.entered.notify_one();
    state.release.notified().await;
    StatusCode::OK
}

fn endpoint_url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

async fn mock_head_object(
    State(objects): State<ObjectMap>,
    Path((_bucket, key)): Path<(String, String)>,
) -> StatusCode {
    if objects.lock().await.contains_key(&key) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn mock_put_object(
    State(objects): State<ObjectMap>,
    Path((_bucket, key)): Path<(String, String)>,
    body: Body,
) -> Result<StatusCode, StatusCode> {
    let bytes = to_bytes(body, usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    objects
        .lock()
        .await
        .insert(key, decode_aws_chunked_body(&bytes));
    Ok(StatusCode::OK)
}

async fn mock_get_object(
    State(objects): State<ObjectMap>,
    Path((_bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(bytes) = objects.lock().await.get(&key).cloned() else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let Some(range) = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_byte_range)
    else {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(Body::from(bytes))
            .expect("response");
    };
    if range.0 > range.1 || range.1 >= bytes.len() {
        return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
    }
    let total_size = bytes.len();
    let range_bytes = Bytes::copy_from_slice(&bytes[range.0..=range.1]);
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_LENGTH, range_bytes.len())
        .header(
            header::CONTENT_RANGE,
            format!("Bytes {:02}-{:02}/{total_size:03}", range.0, range.1),
        )
        .body(Body::from(range_bytes))
        .expect("response")
}

async fn mock_delete_object(
    State(objects): State<ObjectMap>,
    Path((_bucket, key)): Path<(String, String)>,
) -> StatusCode {
    objects.lock().await.remove(&key);
    StatusCode::NO_CONTENT
}

fn parse_byte_range(raw: &str) -> Option<(usize, usize)> {
    let range = raw.strip_prefix("bytes=")?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn empty_response(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("response")
}

fn decode_aws_chunked_body(bytes: &[u8]) -> Vec<u8> {
    if !bytes
        .windows(b";chunk-signature=".len())
        .any(|window| window == b";chunk-signature=")
    {
        return bytes.to_vec();
    }
    let mut output = Vec::new();
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let Some(line_end) = find_crlf(bytes, offset) else {
            return bytes.to_vec();
        };
        let line = &bytes[offset..line_end];
        let size_end = line
            .iter()
            .position(|byte| *byte == b';')
            .unwrap_or(line.len());
        let Ok(size_text) = std::str::from_utf8(&line[..size_end]) else {
            return bytes.to_vec();
        };
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            return bytes.to_vec();
        };
        offset = line_end + 2;
        if size == 0 {
            break;
        }
        let data_end = offset.saturating_add(size);
        if data_end + 2 > bytes.len() {
            return bytes.to_vec();
        }
        output.extend_from_slice(&bytes[offset..data_end]);
        if &bytes[data_end..data_end + 2] != b"\r\n" {
            return bytes.to_vec();
        }
        offset = data_end + 2;
    }
    output
}

fn find_crlf(bytes: &[u8], start: usize) -> Option<usize> {
    bytes
        .get(start..)?
        .windows(2)
        .position(|window| window == b"\r\n")
        .map(|relative| start + relative)
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
