use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{StreamExt, stream};
use hmac::{Hmac, Mac};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::Notify;
use tower::ServiceExt;
use vault_server::auth::{
    AuthMode, AuthSettings, SessionSecretSource, SigningKeyring, UserContext, sign_session_payload,
    verify_session_payload,
};
use vault_server::blob_lifecycle::BlobLifecycleError;
use vault_server::config::Config;
use vault_server::db;
use vault_server::documents::{ClientMeta, sweep_expired_documents};
use vault_server::folders::{
    ARCHIVE_ROOT_KEY, VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path,
    get_root_folder,
};
use vault_server::http::{self, AppState};
use vault_server::reconciliation::storage_reconciliation_report;
use vault_server::storage::{
    BlobByteStream, BlobReadRange, BlobStorageBackend, BlobWriteKind, LocalBlobStorage,
    SharedBlobStorage, StorageError, StoredBlob, multipart_manifest_key_for_hash,
};
use vault_server::transfers::recover_interrupted_transfers;
use vault_server::uploads::{
    self, CreateUploadRequest, UploadPartHeaders, UploadPartIngest, UploadRuntimeSettings,
};

async fn test_state() -> (AppState, tempfile::TempDir) {
    test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 32 * 1024 * 1024, 86_400).await
}

async fn test_state_with_upload_settings(
    max_upload_bytes: i64,
    transfer_chunk_bytes: i64,
    transfer_session_ttl_seconds: i64,
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
        max_upload_bytes,
        transfer_chunk_bytes,
        transfer_session_ttl_seconds,
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
    let state = AppState::new(config, AuthSettings::default(), db, Arc::new(storage));
    (state, temp_dir)
}

#[derive(Debug)]
struct BlockingPartStorage {
    release: Arc<Notify>,
    stored: StoredBlob,
    waiting: Arc<Notify>,
}

#[async_trait]
impl BlobStorageBackend for BlockingPartStorage {
    fn name(&self) -> &'static str {
        "local"
    }

    fn bucket(&self) -> &'static str {
        ""
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        Ok(())
    }

    fn planned_object_key(
        &self,
        _hash_algo: &str,
        _digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        if write_kind == BlobWriteKind::PartFiles {
            Ok(self.stored.object_key.clone())
        } else {
            Err(StorageError::UnsupportedOperation(
                "test storage only supports part promotion".to_string(),
            ))
        }
    }

    async fn put_bytes(&self, _data: &[u8]) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage only supports part promotion".to_string(),
        ))
    }

    async fn put_file(
        &self,
        _source_path: &Path,
        _digest: &str,
        _size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "test storage only supports part promotion".to_string(),
        ))
    }

    async fn put_part_files(
        &self,
        _part_paths: &[PathBuf],
        _expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        self.waiting.notify_one();
        self.release.notified().await;
        Ok(self.stored.clone())
    }

    async fn read_bytes(&self, _object_key: &str) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::NotFound)
    }

    async fn read_range(
        &self,
        _object_key: &str,
        _start: u64,
        _end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        Err(StorageError::NotFound)
    }

    async fn stream_range(
        &self,
        _object_key: &str,
        _range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        Err(StorageError::NotFound)
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        Ok(Vec::new())
    }

    async fn delete_object(&self, _object_key: &str) -> Result<(), StorageError> {
        Ok(())
    }
}

async fn create_group(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO vault_groups (name) VALUES (?)")
        .bind(name)
        .execute(pool)
        .await
        .expect("create group")
        .last_insert_rowid()
}

async fn grant_writer_root(pool: &sqlx::SqlitePool) {
    let writers = create_group(pool, "writers").await;
    let root = get_root_folder(pool, VAULT_ROOT_KEY).await.expect("root");
    let archive_root = get_root_folder(pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(pool, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(pool, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
}

async fn insert_stored_versioned_document(
    pool: &sqlx::SqlitePool,
    storage: &SharedBlobStorage,
    folder_id: i64,
    name: &str,
    content: &[u8],
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
    let version_id = format!("version-{document_id}");
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
    .bind(&version_id)
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

async fn mark_document_archived_for_writer(
    pool: &sqlx::SqlitePool,
    document_id: i64,
    archive_folder_id: i64,
) {
    let writer_group_id: i64 =
        sqlx::query_scalar("SELECT id FROM vault_groups WHERE name = 'writers'")
            .fetch_one(pool)
            .await
            .expect("writer group");
    let mut archived_access = serde_json::Map::new();
    archived_access.insert(writer_group_id.to_string(), json!(3));
    sqlx::query(
        r"
        UPDATE documents
        SET folder_id = ?,
            archived_from_folder = 'Project',
            archived_original_name = name,
            archived_access = ?
        WHERE id = ?
        ",
    )
    .bind(archive_folder_id)
    .bind(Value::Object(archived_access).to_string())
    .bind(document_id)
    .execute(pool)
    .await
    .expect("archive");
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

async fn response_bytes(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec()
}

fn authed_request(method: Method, uri: &str, body: Body) -> Request<Body> {
    authed_request_for(
        method,
        uri,
        "writer",
        "Writer",
        "writer@example.com",
        "writers",
        body,
    )
}

fn authed_request_for(
    method: Method,
    uri: &str,
    user: &str,
    name: &str,
    email: &str,
    groups: &str,
    body: Body,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", name)
        .header("Remote-Email", email)
        .header("Remote-Groups", groups)
        .body(body)
        .expect("request")
}

fn authed_json_request(method: Method, uri: &str, payload: &Value) -> Request<Body> {
    authed_json_request_for(
        method,
        uri,
        "writer",
        "Writer",
        "writer@example.com",
        "writers",
        payload,
    )
}

fn authed_json_request_for(
    method: Method,
    uri: &str,
    user: &str,
    name: &str,
    email: &str,
    groups: &str,
    payload: &Value,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", name)
        .header("Remote-Email", email)
        .header("Remote-Groups", groups)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("request")
}

fn writer_context() -> UserContext {
    UserContext {
        id: "writer".to_string(),
        vault_user_id: 1,
        issuer: "headers".to_string(),
        subject: "writer".to_string(),
        name: "Writer".to_string(),
        email: "writer@example.com".to_string(),
        groups: vec!["writers".to_string()],
        is_admin: false,
    }
}

fn upload_part_request(session_id: &str, part_number: i64, data: &[u8]) -> Request<Body> {
    Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/uploads/{session_id}/parts/{part_number}"))
        .header("Remote-User", "writer")
        .header("Remote-Name", "Writer")
        .header("Remote-Email", "writer@example.com")
        .header("Remote-Groups", "writers")
        .header("content-type", "application/octet-stream")
        .header("x-upload-offset", "0")
        .header("x-upload-size", data.len().to_string())
        .header("x-upload-sha256", sha256_hex(data))
        .body(Body::from(data.to_vec()))
        .expect("request")
}

fn upload_part_request_without_checksum(
    session_id: &str,
    part_number: i64,
    offset: i64,
    data: &[u8],
) -> Request<Body> {
    Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/uploads/{session_id}/parts/{part_number}"))
        .header("Remote-User", "writer")
        .header("Remote-Name", "Writer")
        .header("Remote-Email", "writer@example.com")
        .header("Remote-Groups", "writers")
        .header("content-type", "application/octet-stream")
        .header("x-upload-offset", offset.to_string())
        .header("x-upload-size", data.len().to_string())
        .body(Body::from(data.to_vec()))
        .expect("request")
}

fn upload_part_request_at_offset(
    session_id: &str,
    part_number: i64,
    offset: i64,
    data: &[u8],
) -> Request<Body> {
    Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/uploads/{session_id}/parts/{part_number}"))
        .header("Remote-User", "writer")
        .header("Remote-Name", "Writer")
        .header("Remote-Email", "writer@example.com")
        .header("Remote-Groups", "writers")
        .header("content-type", "application/octet-stream")
        .header("x-upload-offset", offset.to_string())
        .header("x-upload-size", data.len().to_string())
        .header("x-upload-sha256", sha256_hex(data))
        .body(Body::from(data.to_vec()))
        .expect("request")
}

fn upload_part_token_request(
    session_id: &str,
    part_number: i64,
    data: &[u8],
    token: &str,
) -> Request<Body> {
    Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/uploads/{session_id}/parts/{part_number}"))
        .header("content-type", "application/octet-stream")
        .header("x-upload-offset", "0")
        .header("x-upload-size", data.len().to_string())
        .header("x-upload-sha256", sha256_hex(data))
        .header("x-upload-token", token)
        .body(Body::from(data.to_vec()))
        .expect("request")
}

async fn create_upload_session_for_size(
    app: axum::Router,
    size_bytes: i64,
    client_upload_parallelism: Option<i64>,
) -> Value {
    let mut payload = json!({
        "mode": "create",
        "folder": "",
        "filename": format!("asset-{size_bytes}.bin"),
        "mime_type": "application/octet-stream",
        "size_bytes": size_bytes
    });
    if let Some(parallelism) = client_upload_parallelism {
        payload["client_upload_parallelism"] = json!(parallelism);
    }
    let response = app
        .oneshot(authed_json_request(Method::POST, "/api/uploads", &payload))
        .await
        .expect("create upload session");
    assert_eq!(response.status(), StatusCode::OK);
    response_json(response).await
}

async fn create_direct_upload_session(state: &AppState, filename: &str, size_bytes: i64) -> String {
    let user = writer_context();
    uploads::create_upload_session(
        &state.db,
        &state.config.transfers_path(),
        &state.auth.signing_keys,
        UploadRuntimeSettings {
            max_upload_bytes: state.config.max_upload_bytes,
            transfer_chunk_bytes: state.config.transfer_chunk_bytes,
            transfer_session_ttl_seconds: state.config.transfer_session_ttl_seconds,
        },
        CreateUploadRequest {
            mode: "create".to_string(),
            filename: filename.to_string(),
            size_bytes,
            mime_type: Some("application/octet-stream".to_string()),
            folder: String::new(),
            document_id: None,
            note: None,
            rename_to_upload: false,
            client_upload_parallelism: Some(2),
            resume_identity_sha256: None,
        },
        &user,
        &ClientMeta {
            ip: None,
            user_agent: None,
        },
    )
    .await
    .expect("upload session")
    .id
}

async fn upload_temp_file_exists(session_dir: &Path) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(session_dir).await else {
        return false;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().contains(".part.tmp-") {
            return true;
        }
    }
    false
}

async fn upload_part_with_checksum_and_resume(
    app: axum::Router,
    session_id: &str,
    data: &[u8],
) -> Value {
    let uploaded = app
        .clone()
        .oneshot(upload_part_request(session_id, 1, data))
        .await
        .expect("upload part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);

    let resumed = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("resume upload");
    response_json(resumed).await
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    lower_hex(&digest)
}

fn part_manifest_sha256_hex(data: &[u8], chunk_size: usize) -> String {
    let part_count = if data.is_empty() {
        0
    } else {
        data.len().div_ceil(chunk_size)
    };
    let mut manifest = format!(
        "vault-upload-part-manifest-v1\nsize={}\nchunk={chunk_size}\nparts={part_count}\n",
        data.len()
    );
    for (index, chunk) in data.chunks(chunk_size).enumerate() {
        let part_number = index + 1;
        let offset = index * chunk_size;
        writeln!(
            &mut manifest,
            "part={part_number}:{offset}:{}:{}",
            chunk.len(),
            sha256_hex(chunk)
        )
        .expect("write part manifest descriptor");
    }
    sha256_hex(manifest.as_bytes())
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

async fn assert_rejected_completion_preserves_part(
    app: &axum::Router,
    session_id: &str,
    session_dir: &Path,
    payload: Value,
    expected_detail: &str,
) {
    let response = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &payload,
        ))
        .await
        .expect("rejected completion");
    let status = response.status();
    let body = response_json(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["detail"], expected_detail);
    assert!(session_dir.join("00000001.part").is_file());
}

async fn complete_upload_with_manifest_and_assert_download(
    app: axum::Router,
    session_id: &str,
    data: &[u8],
) {
    let completed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"part_manifest_sha256": part_manifest_sha256_hex(data, data.len())}),
        ))
        .await
        .expect("complete upload");
    assert_eq!(completed.status(), StatusCode::OK);
    let document_id = response_json(completed).await["id"]
        .as_i64()
        .expect("document id");
    let downloaded = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/documents/{document_id}/download"),
            Body::empty(),
        ))
        .await
        .expect("download");
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert_eq!(response_bytes(downloaded).await, data);
}

async fn assert_completed_manifest_is_durable(
    app: axum::Router,
    pool: &sqlx::SqlitePool,
    session_dir: &Path,
    session_id: &str,
    manifest: &str,
) {
    let completed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"part_manifest_sha256": manifest}),
        ))
        .await
        .expect("manifest completion retry");
    assert_eq!(completed.status(), StatusCode::OK);
    assert!(
        !session_dir.exists(),
        "successful completion must remove upload staging"
    );

    let persisted_manifest: Option<String> = sqlx::query_scalar(
        "SELECT json_extract(user_context, '$._upload_part_manifest_sha256') FROM upload_sessions WHERE id = ?",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .expect("persisted upload part manifest");
    assert_eq!(persisted_manifest.as_deref(), Some(manifest));

    for _ in 0..2 {
        let resumed = app
            .clone()
            .oneshot(authed_request(
                Method::GET,
                &format!("/api/uploads/{session_id}"),
                Body::empty(),
            ))
            .await
            .expect("completed upload status");
        let resumed = response_json(resumed).await;
        assert_eq!(resumed["status"], "complete");
        assert_eq!(resumed["part_manifest_sha256"], manifest);
        assert_eq!(resumed["uploaded_parts"], json!([]));
    }

    let delete_complete = app
        .oneshot(authed_request(
            Method::DELETE,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("delete completed upload status");
    let delete_complete = response_json(delete_complete).await;
    assert_eq!(delete_complete["status"], "complete");
    assert_eq!(delete_complete["part_manifest_sha256"], manifest);
}

async fn assert_lightweight_upload_status(app: &axum::Router, session_id: &str, expected: Value) {
    let response = app
        .clone()
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}/status"),
            Body::empty(),
        ))
        .await
        .expect("lightweight upload status");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store, max-age=0");
    assert_eq!(response_json(response).await, expected);
}

#[tokio::test]
async fn lightweight_upload_status_uses_only_persisted_state() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let app = http::router(state);
    let created = create_upload_session_for_size(app.clone(), 4, None).await;
    let session_id = created["id"].as_str().expect("session id").to_string();
    let session_dir = transfers_path.join("uploads").join(&session_id);

    tokio::fs::write(session_dir.join("00000001.part"), b"data")
        .await
        .expect("status test part");
    tokio::fs::write(session_dir.join("00000001.json"), b"{")
        .await
        .expect("malformed status test sidecar");
    assert_lightweight_upload_status(
        &app,
        &session_id,
        json!({
            "status": "active",
            "verification": null,
            "result": null,
            "part_manifest_sha256": null
        }),
    )
    .await;

    let detached_staging = transfers_path.join(format!("detached-status-{session_id}"));
    tokio::fs::rename(&session_dir, &detached_staging)
        .await
        .expect("detach upload staging");
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'completing',
            verification_total_bytes = 99,
            verification_processed_bytes = -7
        WHERE id = ?
        ",
    )
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("mark upload completing");
    assert_lightweight_upload_status(
        &app,
        &session_id,
        json!({
            "status": "completing",
            "verification": {"processed_bytes": 0, "total_bytes": 4},
            "result": null,
            "part_manifest_sha256": null
        }),
    )
    .await;

    let manifest = "ab".repeat(32);
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'complete',
            result_document_id = 42,
            result_version_id = 'version-42',
            result_path = 'Project/status.bin',
            user_context = json_set(user_context, '$._upload_part_manifest_sha256', ?)
        WHERE id = ?
        ",
    )
    .bind(&manifest)
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("mark upload complete");
    assert_lightweight_upload_status(
        &app,
        &session_id,
        json!({
            "status": "complete",
            "verification": {"processed_bytes": 4, "total_bytes": 4},
            "result": {
                "id": 42,
                "version": "version-42",
                "path": "Project/status.bin"
            },
            "part_manifest_sha256": manifest
        }),
    )
    .await;
    assert!(detached_staging.join("00000001.part").is_file());
    assert!(detached_staging.join("00000001.json").is_file());
}

#[tokio::test]
async fn lightweight_upload_status_is_hidden_from_other_owners_and_visible_to_admins() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);
    let created = create_upload_session_for_size(app.clone(), 4, None).await;
    let session_id = created["id"].as_str().expect("session id");

    let missing = app
        .clone()
        .oneshot(authed_request(
            Method::GET,
            "/api/uploads/missing-session/status",
            Body::empty(),
        ))
        .await
        .expect("missing lightweight upload status");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(missing.headers()["cache-control"], "no-store, max-age=0");
    assert_eq!(
        response_json(missing).await["detail"],
        "Upload session not found"
    );

    let blocked = app
        .clone()
        .oneshot(authed_request_for(
            Method::GET,
            &format!("/api/uploads/{session_id}/status"),
            "intruder",
            "Intruder",
            "intruder@example.com",
            "",
            Body::empty(),
        ))
        .await
        .expect("other-owner lightweight upload status");
    assert_eq!(blocked.status(), StatusCode::NOT_FOUND);
    assert_eq!(blocked.headers()["cache-control"], "no-store, max-age=0");
    assert_eq!(
        response_json(blocked).await["detail"],
        "Upload session not found"
    );

    let admin = app
        .oneshot(authed_request_for(
            Method::GET,
            &format!("/api/uploads/{session_id}/status"),
            "admin",
            "Admin",
            "admin@example.com",
            "vault-admin",
            Body::empty(),
        ))
        .await
        .expect("admin lightweight upload status");
    assert_eq!(admin.status(), StatusCode::OK);
    assert_eq!(admin.headers()["cache-control"], "no-store, max-age=0");
    assert_eq!(response_json(admin).await["status"], "active");
}

#[tokio::test]
async fn upload_size_limit_rejects_before_metadata_or_blob_write() {
    let (state, temp_dir) = test_state_with_upload_settings(5, 32 * 1024 * 1024, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let local_storage = LocalBlobStorage::new(temp_dir.path().join("objects"), "");
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "too-large.txt",
                "mime_type": "text/plain",
                "size_bytes": 6
            }),
        ))
        .await
        .expect("create upload session");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json["detail"], "Upload exceeds limit of 5 bytes");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents")
            .fetch_one(&pool)
            .await
            .expect("documents"),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM blobs")
            .fetch_one(&pool)
            .await
            .expect("blobs"),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM blob_locations")
            .fetch_one(&pool)
            .await
            .expect("locations"),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM upload_sessions")
            .fetch_one(&pool)
            .await
            .expect("sessions"),
        0,
    );
    assert!(
        local_storage
            .list_object_keys()
            .await
            .expect("objects")
            .is_empty()
    );
}

#[tokio::test]
async fn upload_session_stores_basename_from_client_file_paths() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "Project",
                "filename": "C:\\Users\\Kevin\\ScoutMaster.plasticity",
                "mime_type": "application/octet-stream",
                "size_bytes": 4
            }),
        ))
        .await
        .expect("create upload session");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["filename"], "ScoutMaster.plasticity");
    let stored_filename: String =
        sqlx::query_scalar("SELECT filename FROM upload_sessions WHERE id = ?")
            .bind(json["id"].as_str().expect("session id"))
            .fetch_one(&pool)
            .await
            .expect("stored filename");
    assert_eq!(stored_filename, "ScoutMaster.plasticity");
}

#[tokio::test]
async fn upload_session_mode_is_strict_while_missing_mode_defaults_to_create() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);

    for mode in ["CREATE", " create ", "CheckIn"] {
        let response = app
            .clone()
            .oneshot(authed_json_request(
                Method::POST,
                "/api/uploads",
                &json!({
                    "mode": mode,
                    "folder": "",
                    "filename": format!("invalid-{mode}.txt"),
                    "mime_type": "text/plain",
                    "size_bytes": 1
                }),
            ))
            .await
            .expect("invalid mode response");
        let status = response.status();
        let json = response_json(response).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["detail"], "Unsupported upload session mode");
    }

    let default_mode = app
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "folder": "",
                "filename": "default-mode.txt",
                "mime_type": "text/plain",
                "size_bytes": 1
            }),
        ))
        .await
        .expect("default mode response");
    let default_json = response_json(default_mode).await;

    assert_eq!(default_json["mode"], "create");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM upload_sessions")
            .fetch_one(&pool)
            .await
            .expect("upload sessions"),
        1,
    );
}

#[tokio::test]
async fn upload_route_uses_python_compatible_runtime_numeric_bounds() {
    let (state, _temp_dir) = test_state_with_upload_settings(0, 0, 1).await;
    assert_eq!(state.config.max_upload_bytes, 1);
    assert_eq!(state.config.transfer_chunk_bytes, 1);
    assert_eq!(state.config.transfer_session_ttl_seconds, 60);
    grant_writer_root(&state.db).await;
    let app = http::router(state);
    let before = OffsetDateTime::now_utc();

    let response = app
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "small.txt",
                "mime_type": "text/plain",
                "size_bytes": 1
            }),
        ))
        .await
        .expect("create upload session");
    let status = response.status();
    let json = response_json(response).await;
    let expires_at =
        OffsetDateTime::parse(json["expires_at"].as_str().expect("expires_at"), &Rfc3339)
            .expect("expires_at timestamp");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["chunk_size"], 1);
    assert!(expires_at >= before + Duration::seconds(50));
}

#[tokio::test]
async fn upload_session_create_requires_write_access_on_target_folder() {
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
        .oneshot(authed_json_request_for(
            Method::POST,
            "/api/uploads",
            "reader",
            "Reader",
            "reader@example.com",
            "readers",
            &json!({
                "mode": "create",
                "folder": "Project",
                "filename": "reader.txt",
                "mime_type": "text/plain",
                "size_bytes": 6
            }),
        ))
        .await
        .expect("read-only upload response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["detail"], "Insufficient folder access");
    for table in ["upload_sessions", "upload_parts", "documents", "blobs"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .expect("row count");
        assert_eq!(count, 0, "{table} should stay empty");
    }
}

#[tokio::test]
async fn upload_session_adapts_chunk_size_from_runtime_config_and_client_parallelism() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 32 * 1024 * 1024, 86_400).await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);

    let thirty_eight_mib =
        create_upload_session_for_size(app.clone(), 38 * 1024 * 1024, None).await;
    assert_eq!(thirty_eight_mib["chunk_size"], 4 * 1024 * 1024);
    assert_eq!(thirty_eight_mib["part_count"], 10);

    let sixty_four_mib = create_upload_session_for_size(app.clone(), 64 * 1024 * 1024, None).await;
    assert_eq!(sixty_four_mib["chunk_size"], 8 * 1024 * 1024);
    assert_eq!(sixty_four_mib["part_count"], 8);

    let remote_sized = create_upload_session_for_size(app.clone(), 109 * 1024 * 1024, None).await;
    assert_eq!(remote_sized["chunk_size"], 8 * 1024 * 1024);
    assert_eq!(remote_sized["part_count"], 14);

    let low_latency_remote_sized =
        create_upload_session_for_size(app.clone(), 109 * 1024 * 1024, Some(8)).await;
    assert_eq!(low_latency_remote_sized["chunk_size"], 16 * 1024 * 1024);
    assert_eq!(low_latency_remote_sized["part_count"], 7);

    let large = create_upload_session_for_size(app, 512 * 1024 * 1024, None).await;
    assert_eq!(large["chunk_size"], 32 * 1024 * 1024);
    assert_eq!(large["part_count"], 16);
}

#[tokio::test]
async fn upload_session_rejects_an_unbounded_part_count_without_inflating_chunks() {
    let size = 5 * 1024 * 1024 * 1024_i64;
    let (state, _temp_dir) = test_state_with_upload_settings(size, 1, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "too-many-parts.bin",
                "mime_type": "application/octet-stream",
                "size_bytes": size
            }),
        ))
        .await
        .expect("create upload response");
    let status = response.status();
    let payload = response_json(response).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        payload["detail"],
        "Upload requires more than 1024 parts; increase the transfer chunk size"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM upload_sessions")
            .fetch_one(&pool)
            .await
            .expect("session count"),
        0
    );
}

#[tokio::test]
async fn upload_session_caps_chunks_for_bounded_client_integrity_hashing() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 512 * 1024 * 1024, 86_400).await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);

    let created = create_upload_session_for_size(app, 512 * 1024 * 1024, None).await;
    assert_eq!(created["chunk_size"], 32 * 1024 * 1024);
    assert_eq!(created["part_count"], 16);
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One resume flow covers checksum-free upload and sidecar repair.
async fn upload_part_does_not_require_client_checksum_header() {
    let (state, temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);
    let data = b"abcd";

    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "no-client-hash.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload session");
    assert_eq!(created.status(), StatusCode::OK);
    let session_id = response_json(created).await["id"]
        .as_str()
        .expect("session id")
        .to_string();

    let uploaded = app
        .clone()
        .oneshot(upload_part_request_without_checksum(
            &session_id,
            1,
            0,
            data,
        ))
        .await
        .expect("upload part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);
    let session_dir = temp_dir
        .path()
        .join("transfers")
        .join("uploads")
        .join(&session_id);
    let part_path = session_dir.join("00000001.part");
    for _ in 0..100 {
        if part_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(session_dir.join("00000001.part").exists());
    assert!(!session_dir.join("00000001.json").exists());

    let resumed = app
        .clone()
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("resume upload");
    let resumed_json = response_json(resumed).await;
    assert_eq!(resumed_json["uploaded_bytes"], data.len());
    assert_eq!(
        resumed_json["uploaded_parts"],
        json!([{
            "offset": 0,
            "part_number": 1,
            "sha256": Value::Null,
            "size_bytes": data.len()
        }]),
    );

    let checksum_duplicate = app
        .clone()
        .oneshot(upload_part_request(&session_id, 1, data))
        .await
        .expect("checksum duplicate");
    assert_eq!(checksum_duplicate.status(), StatusCode::NO_CONTENT);
    assert!(session_dir.join("00000001.json").is_file());
    let repaired = app
        .clone()
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("resume repaired upload");
    let repaired_json = response_json(repaired).await;
    assert_eq!(
        repaired_json["uploaded_parts"][0]["sha256"],
        sha256_hex(data)
    );

    assert_rejected_completion_preserves_part(
        &app,
        &session_id,
        &session_dir,
        json!({"part_manifest_sha256": "not-a-sha256"}),
        "Upload integrity digest is invalid",
    )
    .await;
    assert_rejected_completion_preserves_part(
        &app,
        &session_id,
        &session_dir,
        json!({}),
        "Upload completion requires an integrity expectation",
    )
    .await;
    complete_upload_with_manifest_and_assert_download(app, &session_id, data).await;
}

#[tokio::test]
async fn upload_resume_ignores_sidecars_without_a_matching_final_part() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let transfers_path = state.config.transfers_path();
    let app = http::router(state);
    let data = b"abcd";
    let created = create_upload_session_for_size(
        app.clone(),
        i64::try_from(data.len()).expect("upload size"),
        None,
    )
    .await;
    let session_id = created["id"].as_str().expect("session id");
    let session_dir = transfers_path.join("uploads").join(session_id);
    tokio::fs::write(
        session_dir.join("00000001.json"),
        serde_json::to_vec(&json!({
            "part_number": 1,
            "offset_bytes": 0,
            "size_bytes": data.len(),
            "sha256": sha256_hex(data)
        }))
        .expect("part metadata"),
    )
    .await
    .expect("write sidecar without part");

    let sidecar_only = app
        .clone()
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("resume sidecar-only upload");
    let sidecar_only = response_json(sidecar_only).await;
    assert_eq!(sidecar_only["uploaded_bytes"], 0);
    assert_eq!(sidecar_only["uploaded_parts"], json!([]));

    tokio::fs::write(session_dir.join("00000001.part"), b"abc")
        .await
        .expect("write wrong-sized final part");
    let wrong_size = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("resume wrong-sized upload");
    let wrong_size = response_json(wrong_size).await;
    assert_eq!(wrong_size["uploaded_bytes"], 0);
    assert_eq!(wrong_size["uploaded_parts"], json!([]));
}

#[tokio::test]
async fn restart_recovers_checksum_free_browser_upload_parts() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let transfers_path = state.config.transfers_path();
    let app = http::router(state.clone());
    let data = b"abcdefgh";
    let session_id =
        uploaded_session_without_part_checksums(app.clone(), "restart-browser-upload.txt", data, 4)
            .await;
    let session_dir = transfers_path.join("uploads").join(&session_id);
    for part_number in 1..=2 {
        assert!(session_dir.join(format!("{part_number:08}.part")).is_file());
        assert!(!session_dir.join(format!("{part_number:08}.json")).exists());
    }

    let restarted = restart_after_interrupted_upload_completion(
        state,
        app,
        &session_id,
        i64::try_from(data.len()).expect("data size"),
    )
    .await;
    for part_number in 1..=2 {
        assert!(session_dir.join(format!("{part_number:08}.part")).is_file());
    }
    let app = http::router(restarted);
    let resumed = app
        .clone()
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("resumed upload");
    let resumed_json = response_json(resumed).await;
    assert_eq!(resumed_json["status"], "active");
    assert_eq!(resumed_json["uploaded_bytes"], data.len());
    assert_eq!(
        resumed_json["uploaded_parts"].as_array().map(Vec::len),
        Some(2)
    );
    assert!(
        resumed_json["uploaded_parts"]
            .as_array()
            .expect("uploaded parts")
            .iter()
            .all(|part| part["sha256"].is_null())
    );

    let completed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"part_manifest_sha256": part_manifest_sha256_hex(data, 4)}),
        ))
        .await
        .expect("complete recovered upload");
    assert_eq!(completed.status(), StatusCode::OK);
    let document_id = response_json(completed).await["id"]
        .as_i64()
        .expect("document id");
    let downloaded = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/documents/{document_id}/download"),
            Body::empty(),
        ))
        .await
        .expect("download recovered upload");
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert_eq!(response_bytes(downloaded).await, data);
}

#[tokio::test]
async fn upload_part_checksum_failure_leaves_no_part_or_canonical_metadata() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let transfers_path = state.config.transfers_path();
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"abcd";
    let create = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "partial.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload");
    assert_eq!(create.status(), StatusCode::OK);
    let created = response_json(create).await;
    let session_id = created["id"].as_str().expect("session id");

    let bad_part = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/api/uploads/{session_id}/parts/1"))
                .header("Remote-User", "writer")
                .header("Remote-Name", "Writer")
                .header("Remote-Email", "writer@example.com")
                .header("Remote-Groups", "writers")
                .header("content-type", "application/octet-stream")
                .header("x-upload-offset", "0")
                .header("x-upload-size", data.len().to_string())
                .header("x-upload-sha256", sha256_hex(b"wrong"))
                .body(Body::from(data.to_vec()))
                .expect("bad part request"),
        )
        .await
        .expect("bad part");
    let bad_status = bad_part.status();
    let bad_json = response_json(bad_part).await;
    assert_eq!(bad_status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_json["detail"], "Upload part checksum does not match");

    let session_dir = transfers_path.join("uploads").join(session_id);
    let mut entries = tokio::fs::read_dir(&session_dir)
        .await
        .expect("session dir after checksum failure");
    assert!(
        entries
            .next_entry()
            .await
            .expect("session dir entry")
            .is_none(),
        "failed checksum path should remove temporary part files"
    );

    let counts = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        r"
        SELECT
            (SELECT COUNT(*) FROM documents),
            (SELECT COUNT(*) FROM blobs),
            (SELECT COUNT(*) FROM blob_locations),
            (SELECT COUNT(*) FROM upload_parts)
        ",
    )
    .fetch_one(&pool)
    .await
    .expect("canonical counts");
    assert_eq!(counts, (0, 0, 0, 0));

    let resumed = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("resume after checksum failure");
    let resumed_json = response_json(resumed).await;
    assert_eq!(resumed_json["status"], "active");
    assert_eq!(resumed_json["uploaded_bytes"], 0);
    assert_eq!(resumed_json["uploaded_parts"], json!([]));
}

#[tokio::test]
async fn idle_upload_part_times_out_and_removes_its_partial_temp_file() {
    const PART_SIZE: i64 = 8 * 1024 * 1024 + 1;
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, PART_SIZE, 86_400).await;
    grant_writer_root(&state.db).await;
    let transfers_path = state.config.transfers_path();
    let session_id = create_direct_upload_session(&state, "idle-part.bin", PART_SIZE).await;
    let body = stream::iter([Ok::<Bytes, std::io::Error>(Bytes::from_static(b"partial"))])
        .chain(stream::pending());

    let result = uploads::ingest_upload_part_for_owner(
        &state.db,
        UploadPartIngest {
            transfers_path: &transfers_path,
            session_id: &session_id,
            part_number: 1,
            headers: UploadPartHeaders {
                offset: 0,
                size: PART_SIZE,
                sha256: None,
            },
            body_idle_timeout: StdDuration::from_millis(20),
        },
        &writer_context().id,
        body,
    )
    .await;

    assert!(matches!(
        result,
        Err(uploads::UploadError::UploadPartBodyIdleTimeout)
    ));
    let session_dir = transfers_path.join("uploads").join(session_id);
    assert!(!upload_temp_file_exists(&session_dir).await);
    assert!(!session_dir.join("00000001.part").exists());
}

#[tokio::test]
async fn dropping_pending_upload_part_ingest_removes_its_partial_temp_file() {
    const PART_SIZE: i64 = 8 * 1024 * 1024 + 1;
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, PART_SIZE, 86_400).await;
    grant_writer_root(&state.db).await;
    let transfers_path = state.config.transfers_path();
    let session_id = create_direct_upload_session(&state, "cancelled-part.bin", PART_SIZE).await;
    let session_dir = transfers_path.join("uploads").join(&session_id);
    let pool = state.db.clone();
    let owner_id = writer_context().id;
    let ingest_transfers_path = transfers_path.clone();
    let ingest_session_id = session_id.clone();
    let upload = tokio::spawn(async move {
        let body = stream::iter([Ok::<Bytes, std::io::Error>(Bytes::from_static(b"partial"))])
            .chain(stream::pending());
        uploads::ingest_upload_part_for_owner(
            &pool,
            UploadPartIngest {
                transfers_path: &ingest_transfers_path,
                session_id: &ingest_session_id,
                part_number: 1,
                headers: UploadPartHeaders {
                    offset: 0,
                    size: PART_SIZE,
                    sha256: None,
                },
                body_idle_timeout: StdDuration::from_mins(1),
            },
            &owner_id,
            body,
        )
        .await
    });

    for _ in 0..100 {
        if upload_temp_file_exists(&session_dir).await {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    assert!(
        upload_temp_file_exists(&session_dir).await,
        "pending ingest did not create its temporary part"
    );

    upload.abort();
    assert!(
        upload
            .await
            .expect_err("pending ingest should be cancelled")
            .is_cancelled()
    );
    for _ in 0..100 {
        if !upload_temp_file_exists(&session_dir).await {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    assert!(
        !upload_temp_file_exists(&session_dir).await,
        "cancelled ingest left its temporary part behind"
    );
    assert!(!session_dir.join("00000001.part").exists());
}

#[tokio::test]
async fn duplicate_part_upload_is_idempotent_but_conflicting_content_is_rejected() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);
    let data = b"abcdef";

    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "retry.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload session");
    assert_eq!(created.status(), StatusCode::OK);
    let created_json = response_json(created).await;
    let session_id = created_json["id"].as_str().expect("session id");
    assert_eq!(created_json["chunk_size"], 4);

    let first = app
        .clone()
        .oneshot(upload_part_request_at_offset(session_id, 1, 0, b"abcd"))
        .await
        .expect("first part");
    assert_eq!(first.status(), StatusCode::NO_CONTENT);

    let duplicate = app
        .clone()
        .oneshot(upload_part_request_at_offset(session_id, 1, 0, b"abcd"))
        .await
        .expect("duplicate part");
    assert_eq!(duplicate.status(), StatusCode::NO_CONTENT);

    let conflict = app
        .oneshot(upload_part_request_at_offset(session_id, 1, 0, b"wxyz"))
        .await
        .expect("conflicting part");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn concurrent_part_promotion_does_not_overwrite_existing_part() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let user = writer_context();
    let transfers_path = state.config.transfers_path();
    let session = uploads::create_upload_session(
        &state.db,
        &transfers_path,
        &state.auth.signing_keys,
        UploadRuntimeSettings {
            max_upload_bytes: state.config.max_upload_bytes,
            transfer_chunk_bytes: state.config.transfer_chunk_bytes,
            transfer_session_ttl_seconds: state.config.transfer_session_ttl_seconds,
        },
        CreateUploadRequest {
            mode: "create".to_string(),
            filename: "race.txt".to_string(),
            size_bytes: 4,
            mime_type: Some("text/plain".to_string()),
            folder: String::new(),
            document_id: None,
            note: None,
            rename_to_upload: false,
            client_upload_parallelism: Some(2),
            resume_identity_sha256: None,
        },
        &user,
        &ClientMeta {
            ip: None,
            user_agent: None,
        },
    )
    .await
    .expect("upload session");
    let session_id = session.id;
    let first_sha = sha256_hex(b"abcd");
    let second_sha = sha256_hex(b"wxyz");

    let first = uploads::ingest_upload_part_for_owner(
        &state.db,
        UploadPartIngest {
            transfers_path: &transfers_path,
            session_id: &session_id,
            part_number: 1,
            headers: UploadPartHeaders {
                offset: 0,
                size: 4,
                sha256: Some(&first_sha),
            },
            body_idle_timeout: uploads::DEFAULT_UPLOAD_PART_BODY_IDLE_TIMEOUT,
        },
        &user.id,
        stream::iter([Ok::<Bytes, std::io::Error>(Bytes::from_static(b"abcd"))]),
    );
    let second = uploads::ingest_upload_part_for_owner(
        &state.db,
        UploadPartIngest {
            transfers_path: &transfers_path,
            session_id: &session_id,
            part_number: 1,
            headers: UploadPartHeaders {
                offset: 0,
                size: 4,
                sha256: Some(&second_sha),
            },
            body_idle_timeout: uploads::DEFAULT_UPLOAD_PART_BODY_IDLE_TIMEOUT,
        },
        &user.id,
        stream::iter([Ok::<Bytes, std::io::Error>(Bytes::from_static(b"wxyz"))]),
    );
    let (first, second) = tokio::join!(first, second);
    let successes = usize::from(first.is_ok()) + usize::from(second.is_ok());
    assert_eq!(successes, 1);
    assert!(first.is_err() || second.is_err());

    let stored_part = tokio::fs::read(
        transfers_path
            .join("uploads")
            .join(&session_id)
            .join("00000001.part"),
    )
    .await
    .expect("stored part");
    let expected = if first.is_ok() { b"abcd" } else { b"wxyz" };
    assert_eq!(stored_part, expected);
}

#[tokio::test]
async fn upload_session_resume_reports_existing_parts_and_completes_without_final_hash() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);
    let data = b"abcdefgh";

    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "resume.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload session");
    assert_eq!(created.status(), StatusCode::OK);
    let created_json = response_json(created).await;
    let session_id = created_json["id"].as_str().expect("session id");
    assert_eq!(created_json["chunk_size"], 4);

    let first_part = &data[..4];
    let uploaded = app
        .clone()
        .oneshot(upload_part_request_at_offset(session_id, 1, 0, first_part))
        .await
        .expect("upload first part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);

    let resumed = app
        .clone()
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("resume session");
    let resumed_json = response_json(resumed).await;
    assert_eq!(resumed_json["uploaded_bytes"], first_part.len());
    assert_eq!(
        resumed_json["uploaded_parts"],
        json!([{
            "offset": 0,
            "part_number": 1,
            "sha256": sha256_hex(first_part),
            "size_bytes": first_part.len()
        }]),
    );

    let second_part = &data[4..];
    let uploaded_second = app
        .clone()
        .oneshot(upload_part_request_at_offset(session_id, 2, 4, second_part))
        .await
        .expect("upload second part");
    assert_eq!(uploaded_second.status(), StatusCode::NO_CONTENT);

    let completed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"part_manifest_sha256": part_manifest_sha256_hex(data, 4)}),
        ))
        .await
        .expect("complete upload");
    assert_eq!(completed.status(), StatusCode::OK);
    let document_id = response_json(completed).await["id"]
        .as_i64()
        .expect("document id");
    let downloaded = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/documents/{document_id}/download"),
            Body::empty(),
        ))
        .await
        .expect("download");
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert_eq!(response_bytes(downloaded).await, data);
}

#[tokio::test]
async fn upload_completion_promotes_parts_without_assembled_blob() {
    let (state, temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"abcdefgh";
    let digest = sha256_hex(data);
    let objects_path = temp_dir.path().join("objects");

    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "direct-finalize.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload session");
    assert_eq!(created.status(), StatusCode::OK);
    let created_json = response_json(created).await;
    let session_id = created_json["id"].as_str().expect("session id");
    assert_eq!(created_json["part_count"], 2);

    for (part_number, offset) in [0, 4].into_iter().enumerate() {
        let part_number = i64::try_from(part_number + 1).expect("part number");
        let chunk = &data[offset..offset + 4];
        let uploaded = app
            .clone()
            .oneshot(upload_part_request_at_offset(
                session_id,
                part_number,
                i64::try_from(offset).expect("offset"),
                chunk,
            ))
            .await
            .expect("upload part");
        assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);
    }

    let completed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": digest}),
        ))
        .await
        .expect("complete upload");
    assert_eq!(completed.status(), StatusCode::OK);

    let object_key: String = sqlx::query_scalar("SELECT object_key FROM blob_locations LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("blob location");
    assert_eq!(
        object_key,
        format!("multipart/sha256/{digest}/manifest.json")
    );
    assert!(objects_path.join(&object_key).exists());
    let local_storage = LocalBlobStorage::new(&objects_path, "");
    let manifest = local_storage
        .read_multipart_manifest(&object_key)
        .await
        .expect("multipart manifest");
    assert_eq!(manifest.parts.len(), 2);
    assert!(manifest.parts.iter().all(|part| part.path.exists()));
    assert!(
        manifest
            .parts
            .iter()
            .all(|part| part.object_key.contains("/parts/"))
    );
    assert!(
        !objects_path.join(format!("sha256/{digest}")).exists(),
        "completion must not assemble a second full-size blob on the local storage path"
    );
    assert!(
        !temp_dir
            .path()
            .join("transfers")
            .join("uploads")
            .join(session_id)
            .exists()
    );

    let document_id: i64 = sqlx::query_scalar("SELECT id FROM documents LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("document");
    let downloaded = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/documents/{document_id}/download"),
            Body::empty(),
        ))
        .await
        .expect("download");
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert_eq!(response_bytes(downloaded).await, data);
}

#[tokio::test]
async fn upload_session_creates_document_without_part_database_writes() {
    let (state, temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"created bytes";
    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "Project",
                "filename": "new.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len(),
                "client_upload_parallelism": 16
            }),
        ))
        .await
        .expect("create upload session");
    assert_eq!(created.status(), StatusCode::OK);
    let created = response_json(created).await;
    let session_id = created["id"].as_str().expect("session id");
    assert_eq!(created["part_count"], 1);
    assert_eq!(created["uploaded_bytes"], 0);
    assert!(
        created["upload_token"]
            .as_str()
            .is_some_and(|token| token.contains('.'))
    );
    let original_updated_at = "2001-02-03T04:05:06Z";
    sqlx::query("UPDATE upload_sessions SET updated_at = ? WHERE id = ?")
        .bind(original_updated_at)
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("pin session timestamp");

    let resumed = upload_part_with_checksum_and_resume(app.clone(), session_id, data).await;
    assert_eq!(resumed["uploaded_bytes"], data.len());
    assert_eq!(resumed["uploaded_parts"][0]["sha256"], sha256_hex(data));
    let part_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_parts")
        .fetch_one(&pool)
        .await
        .expect("part rows");
    assert_eq!(part_rows, 0);
    let updated_at: String =
        sqlx::query_scalar("SELECT updated_at FROM upload_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("session timestamp");
    assert_eq!(updated_at, original_updated_at);
    assert!(
        temp_dir
            .path()
            .join("transfers")
            .join("uploads")
            .join(session_id)
            .exists()
    );

    let completed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("complete upload");
    let completed_status = completed.status();
    let completed = response_json(completed).await;
    assert_eq!(completed_status, StatusCode::OK, "{completed}");
    assert_eq!(completed["path"], "Project/new.txt");
    assert!(
        !temp_dir
            .path()
            .join("transfers")
            .join("uploads")
            .join(session_id)
            .exists()
    );

    let document_id = completed["id"].as_i64().expect("document id");
    let downloaded = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/documents/{document_id}/download"),
            Body::empty(),
        ))
        .await
        .expect("download");
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert_eq!(response_bytes(downloaded).await, data);
}

#[tokio::test]
async fn create_completion_rechecks_duplicate_path_and_cleans_promoted_object() {
    let (state, temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let race_folder = get_or_create_folder_path(&state.db, Some("Race"))
        .await
        .expect("race folder");
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"lose";

    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "Race",
                "filename": "race.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload session");
    assert_eq!(created.status(), StatusCode::OK);
    let session_id = response_json(created).await["id"]
        .as_str()
        .expect("session id")
        .to_string();

    let uploaded = app
        .clone()
        .oneshot(upload_part_request(&session_id, 1, data))
        .await
        .expect("upload part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);

    let local_storage = LocalBlobStorage::new(temp_dir.path().join("objects"), "");
    let shared_local_storage: SharedBlobStorage =
        Arc::new(LocalBlobStorage::new(temp_dir.path().join("objects"), ""));
    insert_stored_versioned_document(
        &pool,
        &shared_local_storage,
        race_folder.id,
        "race.txt",
        b"winner",
    )
    .await;

    let completed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("complete upload");
    let completed_status = completed.status();
    let completed = response_json(completed).await;
    assert_eq!(completed_status, StatusCode::BAD_REQUEST, "{completed}");
    assert_eq!(
        completed["detail"],
        "A document already exists at that path"
    );

    let documents: Vec<(String, String)> =
        sqlx::query_as("SELECT name, created_by FROM documents ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("documents");
    assert_eq!(
        documents,
        vec![("race.txt".to_string(), "admin".to_string())]
    );
    let version_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM document_versions")
        .fetch_one(&pool)
        .await
        .expect("versions");
    assert_eq!(version_count, 1);
    let blob_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
        .fetch_one(&pool)
        .await
        .expect("blobs");
    assert_eq!(blob_count, 1);
    let location_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_locations")
        .fetch_one(&pool)
        .await
        .expect("locations");
    assert_eq!(location_count, 1);
    let loser_key = multipart_manifest_key_for_hash("", "sha256", &sha256_hex(data));
    let object_keys = local_storage.list_object_keys().await.expect("objects");
    assert_eq!(object_keys.len(), 1);
    assert!(!object_keys.contains(&loser_key));

    let report = storage_reconciliation_report(&pool, &local_storage, false)
        .await
        .expect("reconciliation report");
    assert!(report.orphan_blob_ids.is_empty());
    assert!(report.unreferenced_local_keys.is_empty());
}

#[tokio::test]
async fn upload_part_accepts_signed_token_without_user_headers() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"token upload";
    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "Project",
                "filename": "token.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload session");
    assert_eq!(created.status(), StatusCode::OK);
    let created = response_json(created).await;
    let session_id = created["id"].as_str().expect("session id");
    let token = created["upload_token"].as_str().expect("upload token");

    let invalid = app
        .clone()
        .oneshot(upload_part_token_request(
            session_id,
            1,
            data,
            "not-a-valid-token",
        ))
        .await
        .expect("invalid token response");
    let invalid_status = invalid.status();
    let invalid_json = response_json(invalid).await;
    assert_eq!(invalid_status, StatusCode::UNAUTHORIZED);
    assert_eq!(invalid_json["detail"], "Upload token is required");

    let wrong_session = app
        .clone()
        .oneshot(upload_part_token_request("another-session", 1, data, token))
        .await
        .expect("wrong session response");
    let wrong_session_status = wrong_session.status();
    let wrong_session_json = response_json(wrong_session).await;
    assert_eq!(wrong_session_status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        wrong_session_json["detail"],
        "Upload token is not valid for this session",
    );

    let uploaded = app
        .clone()
        .oneshot(upload_part_token_request(session_id, 1, data, token))
        .await
        .expect("token upload response");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);

    sqlx::query("UPDATE upload_sessions SET status = 'aborted' WHERE id = ?")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("abort token upload session");
    let rejected = app
        .oneshot(upload_part_token_request(session_id, 1, data, token))
        .await
        .expect("terminal token upload response");
    let rejected_status = rejected.status();
    let rejected_json = response_json(rejected).await;
    assert_eq!(rejected_status, StatusCode::CONFLICT);
    assert_eq!(rejected_json["detail"], "Upload session is aborted");
}

#[tokio::test]
async fn upload_tokens_use_domain_separation_and_bounded_key_rotation() {
    const OLD_ROOT: &str = "7d92b4e10a6fc83531de709bca4825f06e13d97a58c02bf46a91e53c7bd80462";
    const NEW_ROOT: &str = "a3f1c9e72b840d56ff196ab30ce2d785914b8c6230e7fa5d4921bc68e30fd754";

    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let user = writer_context();
    let transfers_path = state.config.transfers_path();
    let old_keys = SigningKeyring::from_configured(OLD_ROOT, vec![]);
    let rotated_keys = SigningKeyring::from_configured(NEW_ROOT, vec![OLD_ROOT.to_string()]);
    let current_keys = SigningKeyring::from_configured(NEW_ROOT, vec![]);
    let session = uploads::create_upload_session(
        &state.db,
        &transfers_path,
        &old_keys,
        UploadRuntimeSettings::default(),
        CreateUploadRequest {
            mode: "create".to_string(),
            filename: "rotated-token.txt".to_string(),
            size_bytes: 1,
            mime_type: Some("text/plain".to_string()),
            folder: String::new(),
            document_id: None,
            note: None,
            rename_to_upload: false,
            client_upload_parallelism: None,
            resume_identity_sha256: None,
        },
        &user,
        &ClientMeta {
            ip: None,
            user_agent: None,
        },
    )
    .await
    .expect("old-key upload session");
    let old_token = session.upload_token.expect("old-key upload token");
    assert_eq!(old_token.matches('.').count(), 1);
    assert!(uploads::verify_upload_token_claims(&rotated_keys, &old_token, &session.id).is_ok());
    assert!(uploads::verify_upload_token_claims(&current_keys, &old_token, &session.id).is_err());

    let (old_body, _) = old_token.rsplit_once('.').expect("upload token parts");
    let mut legacy_mac =
        Hmac::<Sha256>::new_from_slice(OLD_ROOT.as_bytes()).expect("legacy raw HMAC key");
    legacy_mac.update(old_body.as_bytes());
    let legacy_signature = URL_SAFE_NO_PAD.encode(legacy_mac.finalize().into_bytes());
    let legacy_token = format!("{old_body}.{legacy_signature}");
    assert!(uploads::verify_upload_token_claims(&old_keys, &legacy_token, &session.id).is_err());

    let refreshed = uploads::get_upload_session(
        &state.db,
        &transfers_path,
        &rotated_keys,
        &session.id,
        &user,
    )
    .await
    .expect("new-key upload token");
    let new_token = refreshed.upload_token.expect("refreshed upload token");
    assert!(uploads::verify_upload_token_claims(&current_keys, &new_token, &session.id).is_ok());
    assert!(uploads::verify_upload_token_claims(&old_keys, &new_token, &session.id).is_err());

    let auth = AuthSettings {
        mode: AuthMode::Dev,
        auth_mode_raw: "dev".to_string(),
        dev_mode: true,
        signing_keys: old_keys.clone(),
        session_secret_source: SessionSecretSource::Explicit,
        ..AuthSettings::default()
    };
    assert!(verify_session_payload(&auth, &old_token).is_none());

    let mut upload_shaped_payload = Map::new();
    upload_shaped_payload.insert("exp".to_string(), json!(4_102_444_800_i64));
    upload_shaped_payload.insert("expires_at".to_string(), json!("2100-01-01T00:00:00Z"));
    upload_shaped_payload.insert("owner".to_string(), json!(user.id));
    upload_shaped_payload.insert("sid".to_string(), json!(session.id));
    upload_shaped_payload.insert("mode".to_string(), json!("create"));
    upload_shaped_payload.insert("name".to_string(), json!("rotated-token.txt"));
    upload_shaped_payload.insert("size".to_string(), json!(1));
    upload_shaped_payload.insert("chunk".to_string(), json!(1));
    upload_shaped_payload.insert("parts".to_string(), json!(1));
    upload_shaped_payload.insert("resume".to_string(), Value::Null);
    upload_shaped_payload.insert("typ".to_string(), json!("upload-part"));
    let wrong_domain_token =
        sign_session_payload(&auth, &upload_shaped_payload).expect("session-domain token");
    assert!(
        uploads::verify_upload_token_claims(&old_keys, &wrong_domain_token, &session.id).is_err()
    );
}

#[tokio::test]
async fn v2_upload_identity_is_echoed_and_bound_to_part_token() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"identity-bound upload";
    let identity = "ab".repeat(32);
    let malformed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "invalid-identity.txt",
                "size_bytes": data.len(),
                "resume_identity_sha256": "not-a-sha256"
            }),
        ))
        .await
        .expect("create upload with malformed identity");
    let malformed_status = malformed.status();
    let malformed_json = response_json(malformed).await;
    assert_eq!(malformed_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        malformed_json["detail"],
        "Upload integrity digest is invalid"
    );
    let upload_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_sessions")
        .fetch_one(&pool)
        .await
        .expect("upload session count");
    assert_eq!(upload_count, 0);

    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "identity.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len(),
                "resume_identity_sha256": identity
            }),
        ))
        .await
        .expect("create v2 upload");
    assert_eq!(created.status(), StatusCode::OK);
    let created = response_json(created).await;
    let session_id = created["id"].as_str().expect("session id");
    let token = created["upload_token"].as_str().expect("upload token");
    assert!(session_id.starts_with("u2-"));
    assert_eq!(created["resume_identity_sha256"], identity);

    let checksum_required = app
        .clone()
        .oneshot(upload_part_request_without_checksum(session_id, 1, 0, data))
        .await
        .expect("checksum-free v2 part");
    let checksum_status = checksum_required.status();
    let checksum_json = response_json(checksum_required).await;
    assert_eq!(checksum_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        checksum_json["detail"],
        "Upload integrity digest is invalid"
    );

    let uploaded = app
        .clone()
        .oneshot(upload_part_token_request(session_id, 1, data, token))
        .await
        .expect("identity-bound token upload");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET user_context = json_set(
            user_context,
            '$._upload_resume_identity_sha256',
            ?
        )
        WHERE id = ?
        ",
    )
    .bind("cd".repeat(32))
    .bind(session_id)
    .execute(&pool)
    .await
    .expect("change bound identity");
    let rejected = app
        .oneshot(upload_part_token_request(session_id, 1, data, token))
        .await
        .expect("mismatched identity token");
    let rejected_status = rejected.status();
    let rejected_json = response_json(rejected).await;
    assert_eq!(rejected_status, StatusCode::UNAUTHORIZED);
    assert_eq!(rejected_json["detail"], "Upload token is invalid");
}

#[tokio::test]
async fn empty_v2_upload_completes_with_canonical_part_manifest() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);
    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "empty.txt",
                "mime_type": "text/plain",
                "size_bytes": 0,
                "resume_identity_sha256": "ab".repeat(32)
            }),
        ))
        .await
        .expect("create empty v2 upload");
    assert_eq!(created.status(), StatusCode::OK);
    let created = response_json(created).await;
    let session_id = created["id"].as_str().expect("session id");
    let chunk_size = usize::try_from(created["chunk_size"].as_i64().expect("chunk size"))
        .expect("positive chunk size");
    assert_eq!(created["part_count"], 0);

    let completed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"part_manifest_sha256": part_manifest_sha256_hex(b"", chunk_size)}),
        ))
        .await
        .expect("complete empty v2 upload");
    assert_eq!(completed.status(), StatusCode::OK);
    let document_id = response_json(completed).await["id"]
        .as_i64()
        .expect("document id");
    let downloaded = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/documents/{document_id}/download"),
            Body::empty(),
        ))
        .await
        .expect("download empty upload");
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert!(response_bytes(downloaded).await.is_empty());
}

#[tokio::test]
async fn create_upload_completion_does_not_recreate_a_deleted_target_folder() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let target = get_or_create_folder_path(&state.db, Some("Transient"))
        .await
        .expect("target folder");
    let transfers_path = state.config.transfers_path();
    let pool = state.db.clone();
    let app = http::router(state);
    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "Transient",
                "filename": "empty.txt",
                "mime_type": "text/plain",
                "size_bytes": 0,
                "resume_identity_sha256": "ab".repeat(32)
            }),
        ))
        .await
        .expect("create upload");
    assert_eq!(created.status(), StatusCode::OK);
    let created = response_json(created).await;
    let session_id = created["id"].as_str().expect("session id").to_string();
    let chunk_size = usize::try_from(created["chunk_size"].as_i64().expect("chunk size"))
        .expect("positive chunk size");
    let session_dir = transfers_path.join("uploads").join(&session_id);
    assert!(session_dir.is_dir(), "upload staging should exist");

    sqlx::query("DELETE FROM folders WHERE id = ?")
        .bind(target.id)
        .execute(&pool)
        .await
        .expect("delete upload target");

    let completed = app
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"part_manifest_sha256": part_manifest_sha256_hex(b"", chunk_size)}),
        ))
        .await
        .expect("complete upload");
    assert_eq!(completed.status(), StatusCode::NOT_FOUND);
    assert_eq!(response_json(completed).await["detail"], "Folder not found");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM folders WHERE root_key = 'vault' AND name = 'Transient'",
        )
        .fetch_one(&pool)
        .await
        .expect("target folder count"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE name = 'empty.txt'")
            .fetch_one(&pool)
            .await
            .expect("document count"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM upload_sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .expect("session status"),
        "failed"
    );
    assert!(
        !session_dir.exists(),
        "failed completion must remove upload staging"
    );
    let orphan_counts = sqlx::query_as::<_, (i64, i64)>(
        r"
        SELECT
            (SELECT COUNT(*) FROM blobs),
            (SELECT COUNT(*) FROM blob_locations)
        ",
    )
    .fetch_one(&pool)
    .await
    .expect("orphan metadata counts");
    assert_eq!(orphan_counts, (0, 0));
}

#[tokio::test]
async fn checkin_session_adds_version_renames_and_releases_lock() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id =
        insert_stored_versioned_document(&state.db, &state.storage, project.id, "plan.txt", b"old")
            .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let locked = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/lock",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("lock");
    assert_eq!(locked.status(), StatusCode::OK);

    let data = b"fresh content";
    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "checkin",
                "document_id": document_id,
                "filename": "plan-v2.txt",
                "mime_type": "text/plain",
                "note": "new version",
                "rename_to_upload": true,
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create checkin upload");
    assert_eq!(created.status(), StatusCode::OK);
    let session_id = response_json(created).await["id"]
        .as_str()
        .expect("session id")
        .to_string();
    let uploaded = app
        .clone()
        .oneshot(upload_part_request(&session_id, 1, data))
        .await
        .expect("upload part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);
    let completed = app
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("complete checkin");
    assert_eq!(completed.status(), StatusCode::OK);
    let completed = response_json(completed).await;
    assert_eq!(completed["path"], "Project/plan-v2.txt");

    let row: (String, i64, i64, String) = sqlx::query_as(
        r"
        SELECT name, latest_version_number, version_count, current_version_id
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document");
    assert_eq!(row.0, "plan-v2.txt");
    assert_eq!(row.1, 2);
    assert_eq!(row.2, 2);
    assert_eq!(row.3, completed["version"].as_str().expect("version"));
    let active_locks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM document_locks WHERE is_active = 1")
            .fetch_one(&pool)
            .await
            .expect("locks");
    assert_eq!(active_locks, 0);
    let state_events: Vec<String> =
        sqlx::query_scalar("SELECT event_type FROM state_events ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("state events");
    let move_index = state_events
        .iter()
        .position(|event| event == "document.move")
        .expect("check-in rename should emit document.move");
    let checkin_index = state_events
        .iter()
        .position(|event| event == "document.checkin")
        .expect("check-in should emit document.checkin");
    let complete_index = state_events
        .iter()
        .position(|event| event == "document.upload.complete")
        .expect("check-in should emit document.upload.complete");
    assert!(move_index < checkin_index);
    assert!(checkin_index < complete_index);
}

async fn complete_checkin_upload_for_document(
    app: axum::Router,
    document_id: i64,
    filename: &str,
    data: &[u8],
) -> Value {
    let locked = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/lock",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("lock");
    assert_eq!(locked.status(), StatusCode::OK);

    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "checkin",
                "document_id": document_id,
                "filename": filename,
                "mime_type": "text/plain",
                "note": "new version",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create checkin upload");
    assert_eq!(created.status(), StatusCode::OK);
    let session_id = response_json(created).await["id"]
        .as_str()
        .expect("session id")
        .to_string();
    let uploaded = app
        .clone()
        .oneshot(upload_part_request(&session_id, 1, data))
        .await
        .expect("upload part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);
    let completed = app
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("complete checkin");
    assert_eq!(completed.status(), StatusCode::OK);
    response_json(completed).await
}

#[tokio::test]
async fn checkin_session_refreshes_delete_ttl_before_sweep() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
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
        insert_stored_versioned_document(&state.db, &state.storage, temp.id, "draft.txt", b"old")
            .await;
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

    let data = b"fresh content";
    let completed = complete_checkin_upload_for_document(app, document_id, "draft.txt", data).await;
    assert_eq!(completed["path"], "Temp/draft.txt");

    let document = sqlx::query_as::<_, (Option<String>, i64)>(
        r"
        SELECT
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
    assert_eq!(document, (Some("delete".to_string()), 1));

    let sweep = sweep_expired_documents(&pool, 250).await.expect("sweep");
    assert!(sweep.deleted.is_empty());
    assert!(sweep.archived.is_empty());
    assert!(sweep.skipped.is_empty());
}

#[tokio::test]
async fn checkin_completion_rechecks_archived_state_after_part_upload() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    let document_id =
        insert_stored_versioned_document(&state.db, &state.storage, project.id, "plan.txt", b"old")
            .await;
    let pool = state.db.clone();
    let app = http::router(state);

    let locked = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/lock",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("lock");
    assert_eq!(locked.status(), StatusCode::OK);
    let data = b"new bytes";
    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "checkin",
                "document_id": document_id,
                "filename": "plan.txt",
                "mime_type": "text/plain",
                "note": "race",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create checkin upload");
    assert_eq!(created.status(), StatusCode::OK);
    let session_id = response_json(created).await["id"]
        .as_str()
        .expect("session id")
        .to_string();
    let uploaded = app
        .clone()
        .oneshot(upload_part_request(&session_id, 1, data))
        .await
        .expect("upload part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);

    mark_document_archived_for_writer(&pool, document_id, archive_root.id).await;
    let completed = app
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("complete checkin");
    let completed_status = completed.status();
    let completed = response_json(completed).await;
    assert_eq!(completed_status, StatusCode::BAD_REQUEST, "{completed}");
    assert_eq!(completed["detail"], "Restore this file before editing");
    let version_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM document_versions WHERE document_id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("versions");
    assert_eq!(version_count, 1);
    let session_status: String =
        sqlx::query_scalar("SELECT status FROM upload_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("session status");
    assert_eq!(session_status, "failed");
}

#[tokio::test]
async fn upload_abort_cleans_parts_blocks_completion_and_preserves_canonical_state() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let transfers_path = state.config.transfers_path();
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"abcdef";
    let create = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "cancelled.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload");
    assert_eq!(create.status(), StatusCode::OK);
    let created = response_json(create).await;
    let session_id = created["id"].as_str().expect("session id");

    let uploaded = app
        .clone()
        .oneshot(upload_part_request(session_id, 1, data))
        .await
        .expect("upload part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);
    let session_dir = transfers_path.join("uploads").join(session_id);
    assert!(
        tokio::fs::metadata(&session_dir).await.is_ok(),
        "part upload should create transfer scratch files"
    );

    let aborted = app
        .clone()
        .oneshot(authed_request(
            Method::DELETE,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("abort upload");
    let aborted_status = aborted.status();
    let aborted_json = response_json(aborted).await;
    assert_eq!(aborted_status, StatusCode::OK);
    assert_eq!(aborted_json["status"], "aborted");
    assert_eq!(aborted_json["uploaded_bytes"], 0);
    assert_eq!(aborted_json["uploaded_parts"], json!([]));
    assert!(tokio::fs::metadata(&session_dir).await.is_err());

    let completed = app
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({}),
        ))
        .await
        .expect("complete aborted upload");
    let completed_status = completed.status();
    let completed_json = response_json(completed).await;
    assert_eq!(completed_status, StatusCode::CONFLICT);
    assert_eq!(completed_json["detail"], "Upload session is aborted");

    let counts = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        r"
        SELECT
            (SELECT COUNT(*) FROM documents),
            (SELECT COUNT(*) FROM blobs),
            (SELECT COUNT(*) FROM blob_locations),
            (SELECT COUNT(*) FROM upload_parts)
        ",
    )
    .fetch_one(&pool)
    .await
    .expect("canonical counts");
    assert_eq!(counts, (0, 0, 0, 0));
}

#[tokio::test]
async fn upload_abort_requires_owner_or_admin() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let app = http::router(state);
    let create = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "owned.txt",
                "mime_type": "text/plain",
                "size_bytes": 4
            }),
        ))
        .await
        .expect("create upload");
    assert_eq!(create.status(), StatusCode::OK);
    let created = response_json(create).await;
    let session_id = created["id"].as_str().expect("session id");

    let blocked = app
        .clone()
        .oneshot(authed_request_for(
            Method::DELETE,
            &format!("/api/uploads/{session_id}"),
            "intruder",
            "Intruder",
            "intruder@example.com",
            "",
            Body::empty(),
        ))
        .await
        .expect("intruder abort");
    assert_eq!(blocked.status(), StatusCode::NOT_FOUND);

    let visible_to_owner = app
        .clone()
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("owner resume");
    let visible_json = response_json(visible_to_owner).await;
    assert_eq!(visible_json["status"], "active");

    let admin_abort = app
        .oneshot(authed_request_for(
            Method::DELETE,
            &format!("/api/uploads/{session_id}"),
            "admin",
            "Admin",
            "admin@example.com",
            "vault-admin",
            Body::empty(),
        ))
        .await
        .expect("admin abort");
    let admin_abort_status = admin_abort.status();
    let admin_abort_json = response_json(admin_abort).await;
    assert_eq!(admin_abort_status, StatusCode::OK);
    assert_eq!(admin_abort_json["status"], "aborted");
}

#[tokio::test]
async fn expired_upload_session_cleans_parts_and_is_not_resumable() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let transfers_path = state.config.transfers_path();
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"abcdef";
    let create = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "expired.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload");
    assert_eq!(create.status(), StatusCode::OK);
    let created = response_json(create).await;
    let session_id = created["id"].as_str().expect("session id");

    let uploaded = app
        .clone()
        .oneshot(upload_part_request(session_id, 1, data))
        .await
        .expect("upload part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);
    let session_dir = transfers_path.join("uploads").join(session_id);
    assert!(tokio::fs::metadata(&session_dir).await.is_ok());

    let expired_at = (OffsetDateTime::now_utc() - Duration::seconds(1))
        .format(&Rfc3339)
        .expect("expired timestamp");
    sqlx::query("UPDATE upload_sessions SET expires_at = ? WHERE id = ?")
        .bind(expired_at)
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("expire session");

    let expired = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("expired upload");
    let expired_status = expired.status();
    let expired_json = response_json(expired).await;
    assert_eq!(expired_status, StatusCode::GONE);
    assert_eq!(expired_json["detail"], "Upload session expired");
    assert!(tokio::fs::metadata(&session_dir).await.is_err());

    let row = sqlx::query_as::<_, (String, i64)>(
        r"
        SELECT
            status,
            (SELECT COUNT(*) FROM upload_parts)
        FROM upload_sessions
        WHERE id = ?
        ",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("expired session row");
    assert_eq!(row, ("expired".to_string(), 0));
}

#[tokio::test]
async fn completed_upload_session_reports_verification_progress() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"abcdefgh";
    let create = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "verification-progress.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload");
    assert_eq!(create.status(), StatusCode::OK);
    let created = response_json(create).await;
    let session_id = created["id"].as_str().expect("session id");

    let uploaded = app
        .clone()
        .oneshot(upload_part_request(session_id, 1, data))
        .await
        .expect("upload part");
    assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);

    let completed = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("complete upload");
    assert_eq!(completed.status(), StatusCode::OK);

    let status = app
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("completed upload status");
    let status_code = status.status();
    let status_json = response_json(status).await;
    assert_eq!(status_code, StatusCode::OK);
    assert_eq!(
        status_json["verification"],
        json!({
            "processed_bytes": data.len(),
            "total_bytes": data.len()
        })
    );

    let verification = sqlx::query_as::<_, (i64, i64)>(
        "SELECT verification_total_bytes, verification_processed_bytes FROM upload_sessions WHERE id = ?",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("verification row");
    assert_eq!(
        verification,
        (
            i64::try_from(data.len()).expect("total bytes"),
            i64::try_from(data.len()).expect("processed bytes")
        )
    );
}

async fn uploaded_session_with_fixed_parts(
    app: axum::Router,
    filename: &str,
    data: &[u8],
    part_size: usize,
) -> String {
    let create = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": filename,
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload");
    assert_eq!(create.status(), StatusCode::OK);
    let created = response_json(create).await;
    let session_id = created["id"].as_str().expect("session id").to_string();

    for (index, chunk) in data.chunks(part_size).enumerate() {
        let part_number = i64::try_from(index + 1).expect("part number");
        let offset = i64::try_from(index * part_size).expect("offset");
        let uploaded = app
            .clone()
            .oneshot(upload_part_request_at_offset(
                &session_id,
                part_number,
                offset,
                chunk,
            ))
            .await
            .expect("upload part");
        assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);
    }
    session_id
}

async fn uploaded_session_without_part_checksums(
    app: axum::Router,
    filename: &str,
    data: &[u8],
    part_size: usize,
) -> String {
    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": filename,
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload");
    assert_eq!(created.status(), StatusCode::OK);
    let session_id = response_json(created).await["id"]
        .as_str()
        .expect("session id")
        .to_string();
    for (index, chunk) in data.chunks(part_size).enumerate() {
        let part_number = i64::try_from(index + 1).expect("part number");
        let offset = i64::try_from(index * part_size).expect("part offset");
        let uploaded = app
            .clone()
            .oneshot(upload_part_request_without_checksum(
                &session_id,
                part_number,
                offset,
                chunk,
            ))
            .await
            .expect("upload part without checksum");
        assert_eq!(uploaded.status(), StatusCode::NO_CONTENT);
    }
    session_id
}

async fn restart_after_interrupted_upload_completion(
    state: AppState,
    app: axum::Router,
    session_id: &str,
    expected_bytes: i64,
) -> AppState {
    let coordinator = state.upload_hash_coordinator.clone();
    for _ in 0..100 {
        if coordinator.preverified_bytes(session_id).await == Some(expected_bytes) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        coordinator.preverified_bytes(session_id).await,
        Some(expected_bytes)
    );
    coordinator.forget(session_id).await;
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'completing',
            verification_total_bytes = ?,
            verification_processed_bytes = ?,
            error = 'stale completion state'
        WHERE id = ?
        ",
    )
    .bind(expected_bytes)
    .bind(expected_bytes)
    .bind(session_id)
    .execute(&state.db)
    .await
    .expect("interrupt upload completion");
    let config = state.config.as_ref().clone();
    let auth = state.auth.as_ref().clone();
    state.db.close().await;
    drop(app);
    drop(state);

    let db = db::connect(&config.db_path())
        .await
        .expect("restart database");
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    let restarted = AppState::new(config, auth, db, Arc::new(storage));
    assert_eq!(
        restarted
            .upload_hash_coordinator
            .preverified_bytes(session_id)
            .await,
        None
    );
    let transfers_path = restarted.config.transfers_path();
    let recovery =
        recover_interrupted_transfers(&restarted.db, &restarted.storage, &transfers_path, false)
            .await
            .expect("recover interrupted upload");
    assert_eq!(recovery.resumed_uploads, vec![session_id]);
    assert!(recovery.failed_uploads.is_empty());
    let state: (String, i64, i64, Option<String>) = sqlx::query_as(
        r"
        SELECT status, verification_total_bytes, verification_processed_bytes, error
        FROM upload_sessions
        WHERE id = ?
        ",
    )
    .bind(session_id)
    .fetch_one(&restarted.db)
    .await
    .expect("recovered upload state");
    assert_eq!(state, ("active".to_string(), 0, 0, None));
    restarted
}

async fn directly_uploaded_session_with_fixed_parts(
    state: &AppState,
    user: &UserContext,
    filename: &str,
    data: &[u8],
    part_size: usize,
) -> String {
    let transfers_path = state.config.transfers_path();
    let session = uploads::create_upload_session(
        &state.db,
        &transfers_path,
        &state.auth.signing_keys,
        UploadRuntimeSettings {
            max_upload_bytes: state.config.max_upload_bytes,
            transfer_chunk_bytes: state.config.transfer_chunk_bytes,
            transfer_session_ttl_seconds: state.config.transfer_session_ttl_seconds,
        },
        CreateUploadRequest {
            mode: "create".to_string(),
            filename: filename.to_string(),
            size_bytes: i64::try_from(data.len()).expect("data size"),
            mime_type: Some("text/plain".to_string()),
            folder: String::new(),
            document_id: None,
            note: None,
            rename_to_upload: false,
            client_upload_parallelism: None,
            resume_identity_sha256: None,
        },
        user,
        &ClientMeta {
            ip: None,
            user_agent: None,
        },
    )
    .await
    .expect("upload session");
    for (index, chunk) in data.chunks(part_size).enumerate() {
        let part_number = i64::try_from(index + 1).expect("part number");
        let offset = i64::try_from(index * part_size).expect("part offset");
        let part_digest = sha256_hex(chunk);
        uploads::ingest_upload_part_for_owner(
            &state.db,
            UploadPartIngest {
                transfers_path: &transfers_path,
                session_id: &session.id,
                part_number,
                headers: UploadPartHeaders {
                    offset,
                    size: i64::try_from(chunk.len()).expect("part size"),
                    sha256: Some(&part_digest),
                },
                body_idle_timeout: uploads::DEFAULT_UPLOAD_PART_BODY_IDLE_TIMEOUT,
            },
            &user.id,
            stream::iter([Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(chunk))]),
        )
        .await
        .expect("upload part");
    }
    session.id
}

fn test_stored_blob(digest: &str, size_bytes: usize) -> StoredBlob {
    StoredBlob {
        backend: "local".to_string(),
        bucket: String::new(),
        digest: digest.to_string(),
        hash_algo: "sha256".to_string(),
        object_key: format!("sha256/{digest}"),
        size_bytes: u64::try_from(size_bytes).expect("size"),
    }
}

async fn wait_for_storage_block(
    waiting: &Notify,
    completion: &mut tokio::task::JoinHandle<
        Result<uploads::UploadResultPayload, uploads::UploadError>,
    >,
) {
    tokio::select! {
        () = waiting.notified() => {}
        result = completion => {
            match result.expect("completion task") {
                Ok(_) => panic!("completion finished before test storage blocked"),
                Err(error) => panic!("completion failed before test storage blocked: {error}"),
            }
        }
        () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
            panic!("completion did not reach test storage");
        }
    }
}

async fn wait_for_upload_status(
    pool: &sqlx::SqlitePool,
    session_id: &str,
    expected_status: &str,
) -> (String, i64, i64, Option<String>) {
    for _ in 0..100 {
        let row = sqlx::query_as::<_, (String, i64, i64, Option<String>)>(
            r"
            SELECT
                status,
                verification_total_bytes,
                verification_processed_bytes,
                error
            FROM upload_sessions
            WHERE id = ?
            ",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .expect("upload status");
        if row.0 == expected_status {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("upload session {session_id} did not become {expected_status}");
}

async fn wait_for_completion_claim_marker(pool: &sqlx::SqlitePool, session_id: &str) {
    for _ in 0..100 {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM completion_claim_markers WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .expect("completion claim marker");
        if count == 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("completion claim for {session_id} was not recorded");
}

#[tokio::test]
async fn incomplete_upload_completion_returns_to_active_and_can_retry() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"abcdefgh";

    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "retry-missing-part.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload");
    assert_eq!(created.status(), StatusCode::OK);
    let session_id = response_json(created).await["id"]
        .as_str()
        .expect("session id")
        .to_string();
    let first_part = app
        .clone()
        .oneshot(upload_part_request_at_offset(&session_id, 1, 0, &data[..4]))
        .await
        .expect("first part");
    assert_eq!(first_part.status(), StatusCode::NO_CONTENT);

    let incomplete = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("incomplete completion");
    let incomplete_status = incomplete.status();
    let incomplete_json = response_json(incomplete).await;
    assert_eq!(incomplete_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        incomplete_json["detail"],
        "Upload session has missing parts"
    );
    assert_eq!(
        wait_for_upload_status(&pool, &session_id, "active").await,
        ("active".to_string(), 0, 0, None)
    );

    let second_part = app
        .clone()
        .oneshot(upload_part_request_at_offset(&session_id, 2, 4, &data[4..]))
        .await
        .expect("second part");
    assert_eq!(second_part.status(), StatusCode::NO_CONTENT);
    let completed = app
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("retried completion");
    assert_eq!(completed.status(), StatusCode::OK);
}

#[tokio::test]
async fn checksum_mismatch_preserves_parts_and_allows_completion_retry() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let app = http::router(state);
    let data = b"abcdefgh";
    let session_id =
        uploaded_session_with_fixed_parts(app.clone(), "retry-checksum.txt", data, 4).await;

    let mismatched = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(b"wrong file")}),
        ))
        .await
        .expect("mismatched completion");
    let mismatch_status = mismatched.status();
    let mismatch_json = response_json(mismatched).await;
    assert_eq!(mismatch_status, StatusCode::BAD_REQUEST);
    assert_eq!(mismatch_json["detail"], "Upload checksum mismatch");
    assert_eq!(
        wait_for_upload_status(&pool, &session_id, "active").await,
        ("active".to_string(), 0, 0, None)
    );
    for part_number in 1..=2 {
        assert!(
            transfers_path
                .join("uploads")
                .join(&session_id)
                .join(format!("{part_number:08}.part"))
                .is_file()
        );
    }

    let completed = app
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("retried completion");
    assert_eq!(completed.status(), StatusCode::OK);
}

#[tokio::test]
async fn part_manifest_mismatch_preserves_parts_and_allows_retry() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let app = http::router(state);
    let data = b"abcdefgh";
    let correct_manifest = part_manifest_sha256_hex(data, 4);
    assert_eq!(
        correct_manifest,
        "7cc9fd2e08c97a13a7d7aa4129de08ae9067cc0b1e93b99680ad1f111d113839"
    );
    let session_id =
        uploaded_session_with_fixed_parts(app.clone(), "retry-manifest.txt", data, 4).await;
    let session_dir = transfers_path.join("uploads").join(&session_id);
    let mismatched = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"part_manifest_sha256": "00".repeat(32)}),
        ))
        .await
        .expect("mismatched manifest completion");
    let mismatch_status = mismatched.status();
    let mismatch_json = response_json(mismatched).await;
    assert_eq!(mismatch_status, StatusCode::BAD_REQUEST);
    assert_eq!(mismatch_json["detail"], "Upload part manifest mismatch");
    assert_eq!(
        wait_for_upload_status(&pool, &session_id, "active").await,
        ("active".to_string(), 0, 0, None)
    );
    for part_number in 1..=2 {
        assert!(
            transfers_path
                .join("uploads")
                .join(&session_id)
                .join(format!("{part_number:08}.part"))
                .is_file()
        );
    }
    let document_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM documents")
        .fetch_one(&pool)
        .await
        .expect("document count");
    assert_eq!(document_count, 0);

    let active = app
        .clone()
        .oneshot(authed_request(
            Method::GET,
            &format!("/api/uploads/{session_id}"),
            Body::empty(),
        ))
        .await
        .expect("active upload status after manifest mismatch");
    let active = response_json(active).await;
    assert_eq!(active["status"], "active");
    assert!(active["part_manifest_sha256"].is_null());

    assert_completed_manifest_is_durable(app, &pool, &session_dir, &session_id, &correct_manifest)
        .await;
}

#[tokio::test]
async fn part_manifest_is_derived_from_staged_bytes_not_sidecars() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let coordinator = state.upload_hash_coordinator.clone();
    let app = http::router(state);
    let data = b"abcdefgh";
    let session_id =
        uploaded_session_with_fixed_parts(app.clone(), "tampered-part.txt", data, 4).await;
    coordinator.forget(&session_id).await;
    let session_dir = transfers_path.join("uploads").join(&session_id);
    tokio::fs::write(session_dir.join("00000001.part"), b"wxyz")
        .await
        .expect("tamper staged bytes");

    let response = app
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"part_manifest_sha256": part_manifest_sha256_hex(data, 4)}),
        ))
        .await
        .expect("tampered completion");
    let status = response.status();
    let body = response_json(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["detail"], "Upload part manifest mismatch");
    assert_eq!(
        wait_for_upload_status(&pool, &session_id, "active").await,
        ("active".to_string(), 0, 0, None)
    );
    let document_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM documents")
        .fetch_one(&pool)
        .await
        .expect("document count");
    assert_eq!(document_count, 0);
    assert!(session_dir.join("00000001.part").is_file());
}

#[tokio::test]
async fn part_read_failure_returns_to_active_and_preserves_retry_data() {
    let (state, temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let app = http::router(state);
    let data = b"abcdefgh";
    let digest = sha256_hex(data);
    let session_id = uploaded_session_with_fixed_parts(app, "retry-part-read.txt", data, 4).await;
    let completion_user_id: String =
        sqlx::query_scalar("SELECT created_by FROM upload_sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .expect("session owner");
    let completion_user = UserContext {
        id: completion_user_id,
        ..writer_context()
    };
    let session_dir = transfers_path.join("uploads").join(&session_id);
    let part_path = session_dir.join("00000001.part");
    let part_backup = session_dir.join("00000001.part.retry-backup");
    assert!(session_dir.join("00000001.json").is_file());
    tokio::fs::rename(&part_path, &part_backup)
        .await
        .expect("temporarily move part");

    let storage = LocalBlobStorage::new(temp_dir.path().join("objects"), "");
    let error = uploads::complete_upload_session(
        &pool,
        &storage,
        &transfers_path,
        &session_id,
        Some(&digest),
        &completion_user,
    )
    .await
    .expect_err("missing part file should fail completion");
    assert!(
        matches!(error, uploads::UploadError::UploadSessionMissingParts),
        "unexpected completion error: {error:?}"
    );
    assert_eq!(
        wait_for_upload_status(&pool, &session_id, "active").await,
        ("active".to_string(), 0, 0, None)
    );
    assert!(part_backup.is_file());
    assert!(!part_path.exists());

    tokio::fs::rename(&part_backup, &part_path)
        .await
        .expect("restore part");
    let completed = uploads::complete_upload_session(
        &pool,
        &storage,
        &transfers_path,
        &session_id,
        Some(&digest),
        &completion_user,
    )
    .await
    .expect("completion after part restore");
    assert_eq!(completed.path, "retry-part-read.txt");
}

#[tokio::test]
async fn completion_reset_database_failure_is_not_silently_discarded() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let app = http::router(state);
    let data = b"abcdefgh";
    let created = app
        .clone()
        .oneshot(authed_json_request(
            Method::POST,
            "/api/uploads",
            &json!({
                "mode": "create",
                "folder": "",
                "filename": "forced-reset-failure.txt",
                "mime_type": "text/plain",
                "size_bytes": data.len()
            }),
        ))
        .await
        .expect("create upload");
    let session_id = response_json(created).await["id"]
        .as_str()
        .expect("session id")
        .to_string();
    let first_part = app
        .clone()
        .oneshot(upload_part_request_at_offset(&session_id, 1, 0, &data[..4]))
        .await
        .expect("first part");
    assert_eq!(first_part.status(), StatusCode::NO_CONTENT);
    sqlx::query(
        r"
        CREATE TRIGGER reject_upload_completion_reset
        BEFORE UPDATE OF status ON upload_sessions
        WHEN OLD.status = 'completing' AND NEW.status = 'active'
        BEGIN
            SELECT RAISE(ABORT, 'forced completion reset failure');
        END
        ",
    )
    .execute(&pool)
    .await
    .expect("reset failure trigger");

    let response = app
        .oneshot(authed_json_request(
            Method::POST,
            &format!("/api/uploads/{session_id}/complete"),
            &json!({"sha256": sha256_hex(data)}),
        ))
        .await
        .expect("completion with reset failure");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let status: String = sqlx::query_scalar("SELECT status FROM upload_sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("status after reset failure");
    assert_eq!(status, "completing");

    sqlx::query("DROP TRIGGER reject_upload_completion_reset")
        .execute(&pool)
        .await
        .expect("remove reset failure trigger");
}

#[tokio::test]
async fn cancelled_upload_completion_returns_to_active_and_can_retry() {
    let (state, temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let coordinator = state.upload_hash_coordinator.clone();
    let data = b"abcdefgh";
    let digest = sha256_hex(data);
    let completion_user = writer_context();
    let session_id = directly_uploaded_session_with_fixed_parts(
        &state,
        &completion_user,
        "retry-cancelled.txt",
        data,
        4,
    )
    .await;
    coordinator.schedule(pool.clone(), transfers_path.clone(), session_id.clone());
    for _ in 0..100 {
        if coordinator.preverified_bytes(&session_id).await
            == Some(i64::try_from(data.len()).expect("data size"))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        coordinator.preverified_bytes(&session_id).await,
        Some(i64::try_from(data.len()).expect("data size"))
    );
    let waiting = Arc::new(Notify::new());
    let storage = BlockingPartStorage {
        release: Arc::new(Notify::new()),
        stored: test_stored_blob(&digest, data.len()),
        waiting: waiting.clone(),
    };
    let complete_pool = pool.clone();
    let complete_transfers_path = transfers_path.clone();
    let complete_session_id = session_id.clone();
    let complete_digest = digest.clone();
    let complete_coordinator = coordinator.clone();
    let retry_user = completion_user.clone();
    let mut completion = tokio::spawn(async move {
        uploads::complete_upload_session_with_hash_coordinator(
            &complete_pool,
            &storage,
            &complete_transfers_path,
            &complete_session_id,
            Some(&complete_digest),
            &completion_user,
            &complete_coordinator,
        )
        .await
    });

    wait_for_storage_block(&waiting, &mut completion).await;
    completion.abort();
    assert!(
        completion
            .await
            .expect_err("completion task should be cancelled")
            .is_cancelled()
    );
    assert_eq!(
        wait_for_upload_status(&pool, &session_id, "active").await,
        ("active".to_string(), 0, 0, None)
    );
    assert_eq!(coordinator.preverified_bytes(&session_id).await, None);
    for part_number in 1..=2 {
        assert!(
            transfers_path
                .join("uploads")
                .join(&session_id)
                .join(format!("{part_number:08}.part"))
                .is_file()
        );
    }

    let retry_storage = LocalBlobStorage::new(temp_dir.path().join("objects"), "");
    let completed = uploads::complete_upload_session_with_hash_coordinator(
        &pool,
        &retry_storage,
        &transfers_path,
        &session_id,
        Some(&digest),
        &retry_user,
        &coordinator,
    )
    .await
    .expect("completion after cancellation");
    assert_eq!(completed.path, "retry-cancelled.txt");
}

#[tokio::test]
async fn abort_wins_against_in_flight_completion_without_committing_metadata() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let signing_keys = state.auth.signing_keys.clone();
    let user = writer_context();
    let data = b"abcdefgh";
    let digest = sha256_hex(data);
    let session_id = directly_uploaded_session_with_fixed_parts(
        &state,
        &user,
        "abort-racing-completion.txt",
        data,
        4,
    )
    .await;
    let waiting = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let storage = BlockingPartStorage {
        release: release.clone(),
        stored: test_stored_blob(&digest, data.len()),
        waiting: waiting.clone(),
    };
    let complete_pool = pool.clone();
    let complete_transfers_path = transfers_path.clone();
    let complete_session_id = session_id.clone();
    let complete_digest = digest.clone();
    let completion_user = user.clone();
    let mut completion = tokio::spawn(async move {
        uploads::complete_upload_session(
            &complete_pool,
            &storage,
            &complete_transfers_path,
            &complete_session_id,
            Some(&complete_digest),
            &completion_user,
        )
        .await
    });

    wait_for_storage_block(&waiting, &mut completion).await;
    let aborted =
        uploads::abort_upload_session(&pool, &transfers_path, &signing_keys, &session_id, &user)
            .await
            .expect("abort should win while storage publication is blocked");
    assert_eq!(aborted.status, "aborted");
    assert!(!transfers_path.join("uploads").join(&session_id).exists());

    release.notify_one();
    let error = completion
        .await
        .expect("completion task")
        .expect_err("aborted completion must not commit");
    assert!(
        matches!(error, uploads::UploadError::UploadSessionStatus(ref status) if status == "aborted"),
        "unexpected completion error: {error:?}"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM upload_sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("aborted status");
    let counts = sqlx::query_as::<_, (i64, i64, i64)>(
        r"
        SELECT
            (SELECT COUNT(*) FROM documents),
            (SELECT COUNT(*) FROM blobs),
            (SELECT COUNT(*) FROM blob_locations)
        ",
    )
    .fetch_one(&pool)
    .await
    .expect("metadata counts");
    assert_eq!(status, "aborted");
    assert_eq!(counts, (0, 0, 0));
}

#[tokio::test]
async fn deletion_tombstone_returns_upload_to_active_without_discarding_parts() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let user = writer_context();
    let data = b"abcdefgh";
    let digest = sha256_hex(data);
    let session_id =
        directly_uploaded_session_with_fixed_parts(&state, &user, "retry-gc.txt", data, 4).await;
    let blob_id =
        sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', ?, ?)")
            .bind(&digest)
            .bind(i64::try_from(data.len()).expect("data size"))
            .execute(&state.db)
            .await
            .expect("deleting blob")
            .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO blob_locations (blob_id, backend, bucket, object_key)
        VALUES (?, '_vault_deleting:test:local', '', ?)
        ",
    )
    .bind(blob_id)
    .bind(multipart_manifest_key_for_hash("", "sha256", &digest))
    .execute(&state.db)
    .await
    .expect("deletion tombstone");
    let transfers_path = state.config.transfers_path();

    let error = uploads::complete_upload_session(
        &state.db,
        state.storage.as_ref(),
        &transfers_path,
        &session_id,
        Some(&digest),
        &user,
    )
    .await
    .expect_err("deletion contention must be retryable");

    assert!(matches!(
        error,
        uploads::UploadError::BlobLifecycle(BlobLifecycleError::DeletionInProgress)
    ));
    assert_eq!(
        wait_for_upload_status(&state.db, &session_id, "active").await,
        ("active".to_string(), 0, 0, None),
    );
    for part_number in 1..=2 {
        assert!(
            transfers_path
                .join("uploads")
                .join(&session_id)
                .join(format!("{part_number:08}.part"))
                .is_file()
        );
    }
}

#[tokio::test]
async fn cancellation_while_completion_claim_is_blocked_cannot_strand_session() {
    let (state, temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let user = writer_context();
    let data = b"abcdefgh";
    let digest = sha256_hex(data);
    let session_id = directly_uploaded_session_with_fixed_parts(
        &state,
        &user,
        "retry-cancelled-claim.txt",
        data,
        4,
    )
    .await;
    sqlx::query("CREATE TABLE completion_claim_markers (session_id TEXT NOT NULL)")
        .execute(&pool)
        .await
        .expect("claim marker table");
    sqlx::query(
        r"
        CREATE TRIGGER record_upload_completion_claim
        AFTER UPDATE OF status ON upload_sessions
        WHEN OLD.status = 'active' AND NEW.status = 'completing'
        BEGIN
            INSERT INTO completion_claim_markers (session_id) VALUES (NEW.id);
        END
        ",
    )
    .execute(&pool)
    .await
    .expect("claim marker trigger");
    let mut write_lock = pool.acquire().await.expect("write lock connection");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *write_lock)
        .await
        .expect("block completion claim");

    let complete_pool = pool.clone();
    let complete_transfers_path = transfers_path.clone();
    let complete_session_id = session_id.clone();
    let complete_digest = digest.clone();
    let completion_user = user.clone();
    let storage = LocalBlobStorage::new(temp_dir.path().join("objects"), "");
    let completion = tokio::spawn(async move {
        uploads::complete_upload_session(
            &complete_pool,
            &storage,
            &complete_transfers_path,
            &complete_session_id,
            Some(&complete_digest),
            &completion_user,
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    completion.abort();
    assert!(
        completion
            .await
            .expect_err("completion should be cancelled while claiming")
            .is_cancelled()
    );
    sqlx::query("COMMIT")
        .execute(&mut *write_lock)
        .await
        .expect("release completion claim");
    drop(write_lock);

    wait_for_completion_claim_marker(&pool, &session_id).await;
    assert_eq!(
        wait_for_upload_status(&pool, &session_id, "active").await,
        ("active".to_string(), 0, 0, None)
    );
    let retry_storage = LocalBlobStorage::new(temp_dir.path().join("objects"), "");
    let completed = uploads::complete_upload_session(
        &pool,
        &retry_storage,
        &transfers_path,
        &session_id,
        Some(&digest),
        &user,
    )
    .await
    .expect("completion after cancelled claim");
    assert_eq!(completed.path, "retry-cancelled-claim.txt");
}

#[tokio::test]
async fn upload_completion_reports_verification_bytes_before_storage_finishes() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let app = http::router(state);
    let data = b"abcdefgh";
    let digest = sha256_hex(data);
    let session_id =
        uploaded_session_with_fixed_parts(app, "verification-live-progress.txt", data, 4).await;

    let waiting = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let storage = BlockingPartStorage {
        release: release.clone(),
        stored: test_stored_blob(&digest, data.len()),
        waiting: waiting.clone(),
    };
    let complete_pool = pool.clone();
    let complete_session_id = session_id.clone();
    let complete_digest = digest.clone();
    let completion_user_id: String =
        sqlx::query_scalar("SELECT created_by FROM upload_sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .expect("session owner");
    let completion_user = UserContext {
        id: completion_user_id,
        ..writer_context()
    };
    let mut completion = tokio::spawn(async move {
        uploads::complete_upload_session(
            &complete_pool,
            &storage,
            &transfers_path,
            &complete_session_id,
            Some(&complete_digest),
            &completion_user,
        )
        .await
    });

    wait_for_storage_block(&waiting, &mut completion).await;
    let progress = sqlx::query_as::<_, (String, i64, i64)>(
        r"
        SELECT status, verification_total_bytes, verification_processed_bytes
        FROM upload_sessions
        WHERE id = ?
        ",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("verification progress");
    assert_eq!(
        progress,
        (
            "completing".to_string(),
            i64::try_from(data.len()).expect("total bytes"),
            i64::try_from(data.len()).expect("processed bytes")
        )
    );

    release.notify_one();
    let completed = completion
        .await
        .expect("completion task")
        .expect("completion");
    assert_eq!(completed.path, "verification-live-progress.txt");
}

#[tokio::test]
async fn upload_completion_reuses_server_preverified_hash_state() {
    let (state, _temp_dir) =
        test_state_with_upload_settings(5 * 1024 * 1024 * 1024, 4, 86_400).await;
    grant_writer_root(&state.db).await;
    let pool = state.db.clone();
    let transfers_path = state.config.transfers_path();
    let coordinator = state.upload_hash_coordinator.clone();
    let app = http::router(state);
    let data = b"abcdefghijklmnop";
    let digest = sha256_hex(data);
    let session_id =
        uploaded_session_with_fixed_parts(app, "preverified-upload.txt", data, 4).await;

    coordinator.schedule(pool.clone(), transfers_path.clone(), session_id.clone());
    for _ in 0..100 {
        if coordinator.preverified_bytes(&session_id).await
            == Some(i64::try_from(data.len()).expect("data size"))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        coordinator.preverified_bytes(&session_id).await,
        Some(i64::try_from(data.len()).expect("data size")),
    );

    let waiting = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let storage = BlockingPartStorage {
        release: release.clone(),
        stored: test_stored_blob(&digest, data.len()),
        waiting: waiting.clone(),
    };
    let completion_user_id: String =
        sqlx::query_scalar("SELECT created_by FROM upload_sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .expect("session owner");
    let completion_user = UserContext {
        id: completion_user_id,
        ..writer_context()
    };
    let complete_pool = pool.clone();
    let complete_transfers_path = transfers_path.clone();
    let complete_session_id = session_id.clone();
    let complete_digest = digest.clone();
    let complete_coordinator = coordinator.clone();
    let mut completion = tokio::spawn(async move {
        uploads::complete_upload_session_with_hash_coordinator(
            &complete_pool,
            &storage,
            &complete_transfers_path,
            &complete_session_id,
            Some(&complete_digest),
            &completion_user,
            &complete_coordinator,
        )
        .await
    });

    wait_for_storage_block(&waiting, &mut completion).await;
    let progress = sqlx::query_as::<_, (String, i64, i64)>(
        r"
        SELECT status, verification_total_bytes, verification_processed_bytes
        FROM upload_sessions
        WHERE id = ?
        ",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("verification progress");
    assert_eq!(
        progress,
        (
            "completing".to_string(),
            i64::try_from(data.len()).expect("total bytes"),
            i64::try_from(data.len()).expect("processed bytes")
        )
    );

    release.notify_one();
    let completed = completion
        .await
        .expect("completion task")
        .expect("completion");
    assert_eq!(completed.path, "preverified-upload.txt");
}
