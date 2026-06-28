use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{
    VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path, get_root_folder,
};
use vault_server::http::{self, AppState};
use vault_server::storage::{BlobStorageBackend, StorageError, StoredBlob};

#[derive(Debug)]
struct RangeOnlyStorage {
    data: Vec<u8>,
    full_read_called: Arc<AtomicBool>,
    object_key: String,
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
    let state = AppState::new(config, AuthSettings::default(), db, storage);
    (state, temp_dir)
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

async fn create_group(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO vault_groups (name) VALUES (?)")
        .bind(name)
        .execute(pool)
        .await
        .expect("create group")
        .last_insert_rowid()
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
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', ?, ?)
        ",
    )
    .bind(sha256_hex(data))
    .bind(i64::try_from(data.len()).expect("blob size"))
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
    });
    let (state, _temp_dir) = test_state(storage).await;
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
    let document_id =
        insert_downloadable_document(&state.db, project.id, &object_key, b"hello world").await;
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
