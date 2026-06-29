use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{delete, get, head, put};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use vault_server::storage::{
    BlobStorageBackend, S3CompatibleBlobStorage, S3StorageSettings, StorageError,
};

type ObjectMap = Arc<Mutex<HashMap<String, Vec<u8>>>>;

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
        .put_part_files(&[first, second], Some(&digest))
        .await
        .expect("put part files");

    assert_eq!(stored.backend, "r2");
    assert_eq!(stored.bucket, "vault-parts");
    assert_eq!(stored.digest, digest);
    assert_eq!(stored.size_bytes, combined.len() as u64);
    assert_eq!(stored.object_key, format!("tenant-a/sha256/{digest}"));
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
        .put_part_files(&[part], Some(&wrong_digest))
        .await
        .expect_err("checksum mismatch");

    assert!(matches!(error, StorageError::ChecksumMismatch));
    assert!(matches!(
        storage
            .read_bytes(&format!("objects/sha256/{actual_digest}"))
            .await,
        Err(StorageError::NotFound),
    ));
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
            .body(Body::from(bytes))
            .expect("response");
    };
    if range.0 > range.1 || range.1 >= bytes.len() {
        return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
    }
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .body(Body::from(Bytes::copy_from_slice(
            &bytes[range.0..=range.1],
        )))
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
