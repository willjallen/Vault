use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use futures_util::future::join_all;
use futures_util::stream;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tempfile::TempDir;
use tower::ServiceExt;
use vault_server::auth::{AuthSettings, UserContext, header_identity};
use vault_server::config::Config;
use vault_server::db;
use vault_server::exports::{self, ExportSelectionItem};
use vault_server::folders::{
    VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path, get_root_folder,
};
use vault_server::http::{self, AppState};
use vault_server::storage::LocalBlobStorage;

pub const CONCURRENT_TRANSFER_BYTES: usize = 16 * 1024 * 1024;
pub const CONCURRENT_USERS: usize = 12;
pub const DOWNLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const EXPORT_BYTES: usize = 32 * 1024 * 1024;
pub const LARGE_DOWNLOAD_BYTES: usize = 256 * 1024 * 1024;
pub const LARGE_UPLOAD_BYTES: i64 = 256 * 1024 * 1024;
const UPLOAD_BODY_CHUNK_BYTES: usize = 256 * 1024;
static UPLOAD_BODY_CHUNK: [u8; UPLOAD_BODY_CHUNK_BYTES] = [0x5a; UPLOAD_BODY_CHUNK_BYTES];

pub struct PerformanceFixture {
    pub auth: AuthSettings,
    pub auth_headers: HeaderMap,
    pub direct_object_key: String,
    pub local_storage: LocalBlobStorage,
    pub state: AppState,
    pub state_event_cursor: i64,
    pub target_folder: String,
    pub user: UserContext,
    _temp_dir: TempDir,
}

pub struct LargeDownloadFixture {
    pub app: Router,
    pub auth_headers: Vec<HeaderMap>,
    pub document_id: i64,
    _temp_dir: TempDir,
}

pub struct ExportScenario {
    document_id: i64,
    state: AppState,
    user: UserContext,
    _temp_dir: TempDir,
}

#[derive(Clone)]
pub struct UploadSessionInput {
    pub chunk_size: usize,
    pub headers: HeaderMap,
    pub id: String,
    pub part_sha256: Vec<String>,
    pub part_count: usize,
    pub sha256: String,
    pub total_size: usize,
    pub upload_token: String,
}

pub struct UploadScenario {
    pub app: Router,
    pub sessions: Vec<UploadSessionInput>,
    _temp_dir: TempDir,
}

impl PerformanceFixture {
    pub async fn build() -> Self {
        let temp_dir = performance_temp_dir();
        let config = test_config(&temp_dir);
        let db = db::connect(&config.db_path())
            .await
            .expect("performance fixture database");
        let local_storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
        local_storage
            .ensure()
            .await
            .expect("performance fixture storage");
        let auth = AuthSettings::default();
        let state = AppState::new(config, auth.clone(), db, Arc::new(local_storage.clone()));

        let reader_group_id =
            sqlx::query("INSERT INTO vault_groups (name) VALUES ('perf-readers')")
                .execute(&state.db)
                .await
                .expect("performance reader group")
                .last_insert_rowid();
        let vault_root = get_root_folder(&state.db, VAULT_ROOT_KEY)
            .await
            .expect("vault root");
        add_folder_permission(&state.db, vault_root.id, reader_group_id, true, true, true)
            .await
            .expect("performance reader permission");

        let target_folder = "Bench Target".to_string();
        let target = get_or_create_folder_path(&state.db, Some(&target_folder))
            .await
            .expect("target folder");
        let unrelated_folder_ids = seed_folder_shape(&state.db, vault_root.id).await;
        seed_view_documents(&state.db, target.id, &unrelated_folder_ids).await;

        let download_data = deterministic_bytes(DOWNLOAD_BYTES, 0x51);
        let direct_object_key =
            insert_stored_document(&state, target.id, "large-download.bin", &download_data)
                .await
                .1;
        seed_reconciliation_objects(&state, target.id).await;
        seed_state_events(&state.db, 10_000).await;
        let state_event_cursor =
            sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(id), 0) - 100 FROM state_events")
                .fetch_one(&state.db)
                .await
                .expect("state event cursor");

        let auth_headers = auth_headers();
        header_identity(&auth, &state.db, &auth_headers)
            .await
            .expect("warm header identity");
        let user = UserContext {
            id: "perf-reader".to_string(),
            vault_user_id: 0,
            issuer: "benchmark".to_string(),
            subject: "perf-reader".to_string(),
            name: "Performance Reader".to_string(),
            email: "perf-reader@example.com".to_string(),
            groups: vec!["perf-readers".to_string()],
            is_admin: false,
        };
        Self {
            auth,
            auth_headers,
            direct_object_key,
            local_storage,
            state,
            state_event_cursor,
            target_folder,
            user,
            _temp_dir: temp_dir,
        }
    }
}

impl LargeDownloadFixture {
    pub async fn build() -> Self {
        let temp_dir = performance_temp_dir();
        let config = test_config(&temp_dir);
        let db = db::connect(&config.db_path())
            .await
            .expect("large download fixture database");
        let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
        storage.ensure().await.expect("large download storage");
        let auth = AuthSettings::default();
        let state = AppState::new(config, auth.clone(), db, Arc::new(storage));
        let group_id = sqlx::query("INSERT INTO vault_groups (name) VALUES ('perf-readers')")
            .execute(&state.db)
            .await
            .expect("large download reader group")
            .last_insert_rowid();
        let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
            .await
            .expect("large download root");
        add_folder_permission(&state.db, root.id, group_id, true, true, false)
            .await
            .expect("large download reader permission");
        let content = deterministic_bytes(LARGE_DOWNLOAD_BYTES, 0x6d);
        let (document_id, _) =
            insert_stored_document(&state, root.id, "large-256-mib.bin", &content).await;
        drop(content);
        let auth_headers = (0..CONCURRENT_USERS)
            .map(auth_headers_for_user)
            .collect::<Vec<_>>();
        for headers in &auth_headers {
            header_identity(&auth, &state.db, headers)
                .await
                .expect("warm large download identity");
        }
        let app = http::router(state);
        Self {
            app,
            auth_headers,
            document_id,
            _temp_dir: temp_dir,
        }
    }
}

impl ExportScenario {
    pub async fn build() -> Self {
        let temp_dir = performance_temp_dir();
        let mut config = test_config(&temp_dir);
        config.export_zip_compression_threshold_bytes = 1;
        config.export_zip_compresslevel = 1;
        let db = db::connect(&config.db_path())
            .await
            .expect("export scenario database");
        let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
        storage.ensure().await.expect("export scenario storage");
        let state = AppState::new(config, AuthSettings::default(), db, Arc::new(storage));
        let group_id = sqlx::query("INSERT INTO vault_groups (name) VALUES ('perf-readers')")
            .execute(&state.db)
            .await
            .expect("export scenario reader group")
            .last_insert_rowid();
        let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
            .await
            .expect("export scenario root");
        add_folder_permission(&state.db, root.id, group_id, true, true, false)
            .await
            .expect("export scenario reader permission");
        let content = vec![b'x'; EXPORT_BYTES];
        let (document_id, _) =
            insert_stored_document(&state, root.id, "compressible-export.txt", &content).await;
        sqlx::query("UPDATE document_versions SET mime_type = 'text/plain' WHERE document_id = ?")
            .bind(document_id)
            .execute(&state.db)
            .await
            .expect("mark export fixture as compressible text");
        let user = UserContext {
            id: "perf-exporter".to_string(),
            vault_user_id: 0,
            issuer: "benchmark".to_string(),
            subject: "perf-exporter".to_string(),
            name: "Performance Exporter".to_string(),
            email: "perf-exporter@example.com".to_string(),
            groups: vec!["perf-readers".to_string()],
            is_admin: false,
        };
        Self {
            document_id,
            state,
            user,
            _temp_dir: temp_dir,
        }
    }

    pub async fn export_and_wait(&self) {
        let job = exports::create_export_job_with_runtime(
            &self.state.db,
            &self.state.storage,
            &self.state.config.transfers_path(),
            &[ExportSelectionItem::Document {
                id: self.document_id,
            }],
            &self.user,
            &self.state.export_execution,
        )
        .await
        .expect("create forced-compression export benchmark job");
        let completed = tokio::time::timeout(Duration::from_mins(1), async {
            loop {
                let current = exports::get_export_job(&self.state.db, &job.id, &self.user)
                    .await
                    .expect("read export benchmark job");
                match current.status.as_str() {
                    "complete" => break current,
                    "failed" | "cancelled" => {
                        panic!("export benchmark ended with status {}", current.status)
                    }
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
        })
        .await
        .expect("forced-compression export benchmark timeout");
        let artifact = exports::export_artifact_download(&self.state.db, &job.id, &self.user)
            .await
            .expect("forced-compression export benchmark artifact");
        assert_eq!(completed.size_bytes, Some(artifact.size_bytes));
        assert!(artifact.size_bytes > 0);
        assert!(
            artifact.size_bytes < i64::try_from(EXPORT_BYTES).expect("export benchmark size"),
            "compressible export should be smaller than its input"
        );
    }
}

impl UploadScenario {
    pub async fn build(user_count: usize, total_size: i64) -> Self {
        let temp_dir = performance_temp_dir();
        let config = test_config(&temp_dir);
        let db = db::connect(&config.db_path())
            .await
            .expect("upload scenario database");
        let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
        storage.ensure().await.expect("upload scenario storage");
        let state = AppState::new(config, AuthSettings::default(), db, Arc::new(storage));
        let group_id = sqlx::query("INSERT INTO vault_groups (name) VALUES ('perf-readers')")
            .execute(&state.db)
            .await
            .expect("upload scenario writer group")
            .last_insert_rowid();
        let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
            .await
            .expect("upload scenario root");
        add_folder_permission(&state.db, root.id, group_id, true, true, true)
            .await
            .expect("upload scenario writer permission");
        let app = http::router(state);
        let mut sessions = Vec::with_capacity(user_count);
        let sha256 = sha256_repeated_upload_byte(
            usize::try_from(total_size).expect("positive upload benchmark size"),
        );
        let mut part_sha256_by_size = HashMap::new();
        for user_index in 0..user_count {
            sessions.push(
                create_upload_session(
                    app.clone(),
                    user_index,
                    total_size,
                    &sha256,
                    &mut part_sha256_by_size,
                )
                .await,
            );
        }
        Self {
            app,
            sessions,
            _temp_dir: temp_dir,
        }
    }

    pub async fn upload_and_complete(&self) {
        let part_uploads = self.sessions.iter().flat_map(|session| {
            (1..=session.part_count).map(|part_number| {
                let app = self.app.clone();
                let request = upload_part_request(session, part_number);
                async move {
                    app.oneshot(request)
                        .await
                        .expect("upload part benchmark response")
                }
            })
        });
        let part_responses = join_all(part_uploads).await;
        for response in part_responses {
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let _ = to_bytes(response.into_body(), 1024)
                .await
                .expect("drain upload part response");
        }

        let completions = self.sessions.iter().map(|session| {
            let app = self.app.clone();
            let request = json_request(
                Method::POST,
                &format!("/api/uploads/{}/complete", session.id),
                &session.headers,
                &json!({"sha256": session.sha256}),
            );
            async move {
                app.oneshot(request)
                    .await
                    .expect("upload completion benchmark response")
            }
        });
        let completion_responses = join_all(completions).await;
        for response in completion_responses {
            assert_eq!(response.status(), StatusCode::OK);
            let _ = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("drain upload completion response");
        }
    }
}

async fn create_upload_session(
    app: Router,
    user_index: usize,
    total_size: i64,
    sha256: &str,
    part_sha256_by_size: &mut HashMap<usize, String>,
) -> UploadSessionInput {
    let headers = auth_headers_for_user(user_index);
    let resume_identity_sha256 =
        sha256_bytes(format!("vault-performance-upload:{user_index}:{total_size}").as_bytes());
    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/uploads",
            &headers,
            &json!({
                "mode": "create",
                "folder": "",
                "filename": format!("benchmark-upload-{user_index:02}.bin"),
                "mime_type": "application/octet-stream",
                "size_bytes": total_size,
                "client_upload_parallelism": 16,
                "resume_identity_sha256": resume_identity_sha256
            }),
        ))
        .await
        .expect("create upload benchmark session response");
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    let id = payload["id"]
        .as_str()
        .expect("upload benchmark session id")
        .to_string();
    let chunk_size = usize::try_from(
        payload["chunk_size"]
            .as_i64()
            .expect("upload benchmark chunk size"),
    )
    .expect("positive upload benchmark chunk size");
    let part_count = usize::try_from(
        payload["part_count"]
            .as_i64()
            .expect("upload benchmark part count"),
    )
    .expect("positive upload benchmark part count");
    let total_size = usize::try_from(total_size).expect("positive upload benchmark size");
    let part_sha256 = (1..=part_count)
        .map(|part_number| {
            let offset = (part_number - 1) * chunk_size;
            let size = chunk_size.min(total_size - offset);
            part_sha256_by_size
                .entry(size)
                .or_insert_with(|| sha256_repeated_upload_byte(size))
                .clone()
        })
        .collect();
    UploadSessionInput {
        id,
        chunk_size,
        part_count,
        part_sha256,
        sha256: sha256.to_string(),
        total_size,
        upload_token: payload["upload_token"]
            .as_str()
            .expect("upload benchmark token")
            .to_string(),
        headers,
    }
}

fn upload_part_request(session: &UploadSessionInput, part_number: usize) -> Request<Body> {
    let offset = (part_number - 1) * session.chunk_size;
    let size = session.chunk_size.min(session.total_size - offset);
    let chunks = stream::unfold(size, |remaining| async move {
        if remaining == 0 {
            return None;
        }
        let chunk_size = remaining.min(UPLOAD_BODY_CHUNK_BYTES);
        let chunk = Bytes::from_static(&UPLOAD_BODY_CHUNK[..chunk_size]);
        Some((Ok::<_, Infallible>(chunk), remaining - chunk_size))
    });
    Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/uploads/{}/parts/{part_number}", session.id))
        .header("content-type", "application/octet-stream")
        .header("content-length", size.to_string())
        .header("x-upload-offset", offset.to_string())
        .header("x-upload-size", size.to_string())
        .header("x-upload-sha256", &session.part_sha256[part_number - 1])
        .header("x-upload-token", &session.upload_token)
        .body(Body::from_stream(chunks))
        .expect("upload part benchmark request")
}

fn sha256_repeated_upload_byte(size: usize) -> String {
    let mut hasher = Sha256::new();
    let mut remaining = size;
    while remaining > 0 {
        let chunk_size = remaining.min(UPLOAD_BODY_CHUNK_BYTES);
        hasher.update(&UPLOAD_BODY_CHUNK[..chunk_size]);
        remaining -= chunk_size;
    }
    lower_hex(&hasher.finalize())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    lower_hex(&Sha256::digest(bytes))
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

fn json_request(method: Method, uri: &str, headers: &HeaderMap, payload: &Value) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("benchmark JSON request");
    extend_headers(request.headers_mut(), headers);
    request
}

fn extend_headers(target: &mut HeaderMap, source: &HeaderMap) {
    for (name, value) in source {
        target.insert(name, value.clone());
    }
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("benchmark JSON response body");
    serde_json::from_slice(&bytes).expect("benchmark JSON response")
}

fn performance_temp_dir() -> TempDir {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("vault-server must be below the workspace root");
    let target_dir = workspace_root.join("target");
    let canonical_workspace = workspace_root
        .canonicalize()
        .expect("canonical performance workspace");
    let canonical_target = target_dir
        .canonicalize()
        .expect("canonical workspace target directory");
    assert!(
        canonical_target.starts_with(&canonical_workspace)
            && canonical_target != canonical_workspace,
        "performance target directory must remain inside the workspace"
    );
    let base_dir = canonical_target.join("perf-tmp");
    std::fs::create_dir_all(&base_dir).expect("create target/perf-tmp");
    let canonical_base = base_dir
        .canonicalize()
        .expect("canonical performance temp directory");
    assert!(
        canonical_base.starts_with(&canonical_target) && canonical_base != canonical_target,
        "performance temp directory must be a child of the workspace target directory"
    );
    let temp_dir = tempfile::Builder::new()
        .prefix("vault-performance-")
        .tempdir_in(&canonical_base)
        .expect("performance fixture tempdir below target/perf-tmp");
    let canonical_temp = temp_dir
        .path()
        .canonicalize()
        .expect("canonical unique performance temp directory");
    assert_eq!(
        canonical_temp.parent(),
        Some(canonical_base.as_path()),
        "performance fixture must be a direct child of target/perf-tmp"
    );
    temp_dir
}

fn test_config(temp_dir: &TempDir) -> Config {
    Config {
        host: "127.0.0.1".parse().expect("benchmark host"),
        port: 0,
        data_dir: temp_dir.path().to_path_buf(),
        db_path: Some(temp_dir.path().join("vault.db")),
        objects_path: None,
        transfers_path: None,
        static_dir: "vault/client".into(),
        storage_backend: "local".to_string(),
        storage_prefix: "objects".to_string(),
        site_name: "Vault performance".to_string(),
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

fn auth_headers() -> HeaderMap {
    auth_headers_for_user(usize::MAX)
}

fn auth_headers_for_user(index: usize) -> HeaderMap {
    let (subject, name, email) = if index == usize::MAX {
        (
            "perf-reader".to_string(),
            "Performance Reader".to_string(),
            "perf-reader@example.com".to_string(),
        )
    } else {
        (
            format!("perf-reader-{index:02}"),
            format!("Performance Reader {index:02}"),
            format!("perf-reader-{index:02}@example.com"),
        )
    };
    let mut headers = HeaderMap::new();
    for (header_name, value) in [
        ("remote-user", subject.as_str()),
        ("remote-name", name.as_str()),
        ("remote-email", email.as_str()),
        ("remote-groups", "perf-readers"),
    ] {
        headers.insert(
            HeaderName::from_bytes(header_name.as_bytes()).expect("benchmark header name"),
            HeaderValue::from_str(value).expect("benchmark header value"),
        );
    }
    headers
}

async fn seed_folder_shape(pool: &SqlitePool, vault_root_id: i64) -> Vec<i64> {
    let mut unrelated_ids = Vec::with_capacity(256);
    for index in 0..256 {
        let folder_id = sqlx::query(
            "INSERT INTO folders (root_key, parent_id, name, is_root) VALUES ('vault', ?, ?, 0)",
        )
        .bind(vault_root_id)
        .bind(format!("Unrelated {index:03}"))
        .execute(pool)
        .await
        .expect("wide benchmark folder")
        .last_insert_rowid();
        unrelated_ids.push(folder_id);
    }

    let mut parent_id = vault_root_id;
    for depth in 0..32 {
        parent_id = sqlx::query(
            "INSERT INTO folders (root_key, parent_id, name, is_root) VALUES ('vault', ?, ?, 0)",
        )
        .bind(parent_id)
        .bind(format!("Deep {depth:02}"))
        .execute(pool)
        .await
        .expect("deep benchmark folder")
        .last_insert_rowid();
    }
    unrelated_ids.push(parent_id);
    unrelated_ids
}

async fn seed_view_documents(pool: &SqlitePool, target_folder_id: i64, unrelated_ids: &[i64]) {
    let blob_id =
        sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', ?, 0)")
            .bind("0".repeat(64))
            .execute(pool)
            .await
            .expect("view benchmark blob")
            .last_insert_rowid();
    let mut transaction = pool.begin().await.expect("view fixture transaction");
    for index in 0..2_000 {
        let folder_id = if index < 48 {
            target_folder_id
        } else {
            unrelated_ids[(index - 48) % unrelated_ids.len()]
        };
        let name = format!("document-{index:04}.bin");
        let document_id = sqlx::query(
            r"
            INSERT INTO documents
                (folder_id, name, created_by, created_by_name, latest_modified_by)
            VALUES (?, ?, 'seed', 'Fixture', 'seed')
            ",
        )
        .bind(folder_id)
        .bind(&name)
        .execute(&mut *transaction)
        .await
        .expect("view benchmark document")
        .last_insert_rowid();
        let version_id = format!("view-version-{document_id}");
        sqlx::query(
            r"
            INSERT INTO document_versions
                (id, document_id, blob_id, version_number, committed_by,
                 committed_by_name, message, mime_type, original_filename, created_via)
            VALUES (?, ?, ?, 1, 'seed', 'Fixture', 'Seed',
                    'application/octet-stream', ?, 'upload')
            ",
        )
        .bind(&version_id)
        .bind(document_id)
        .bind(blob_id)
        .bind(&name)
        .execute(&mut *transaction)
        .await
        .expect("view benchmark version");
        sqlx::query(
            "UPDATE documents SET current_version_id = ?, latest_version_number = 1, version_count = 1 WHERE id = ?",
        )
        .bind(version_id)
        .bind(document_id)
        .execute(&mut *transaction)
        .await
        .expect("view benchmark current version");
    }
    transaction
        .commit()
        .await
        .expect("commit view benchmark fixture");
}

async fn insert_stored_document(
    state: &AppState,
    folder_id: i64,
    name: &str,
    content: &[u8],
) -> (i64, String) {
    let stored = state
        .storage
        .put_bytes(content)
        .await
        .expect("stored benchmark blob");
    let blob_id = sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES (?, ?, ?)")
        .bind(&stored.hash_algo)
        .bind(&stored.digest)
        .bind(i64::try_from(stored.size_bytes).expect("benchmark blob size"))
        .execute(&state.db)
        .await
        .expect("benchmark blob row")
        .last_insert_rowid();
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, ?, ?, ?)",
    )
    .bind(blob_id)
    .bind(&stored.backend)
    .bind(&stored.bucket)
    .bind(&stored.object_key)
    .execute(&state.db)
    .await
    .expect("benchmark blob location");
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES (?, ?, 'seed', 'Fixture', 'seed')
        ",
    )
    .bind(folder_id)
    .bind(name)
    .execute(&state.db)
    .await
    .expect("stored benchmark document")
    .last_insert_rowid();
    let version_id = format!("stored-version-{document_id}");
    sqlx::query(
        r"
        INSERT INTO document_versions
            (id, document_id, blob_id, version_number, committed_by,
             committed_by_name, message, mime_type, original_filename, created_via)
        VALUES (?, ?, ?, 1, 'seed', 'Fixture', 'Seed',
                'application/octet-stream', ?, 'upload')
        ",
    )
    .bind(&version_id)
    .bind(document_id)
    .bind(blob_id)
    .bind(name)
    .execute(&state.db)
    .await
    .expect("stored benchmark version");
    sqlx::query(
        "UPDATE documents SET current_version_id = ?, latest_version_number = 1, version_count = 1 WHERE id = ?",
    )
    .bind(version_id)
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("stored benchmark current version");
    (document_id, stored.object_key)
}

async fn seed_reconciliation_objects(state: &AppState, folder_id: i64) {
    for index in 0..4_u8 {
        let content = deterministic_bytes(256 * 1024, index.wrapping_add(1));
        insert_stored_document(
            state,
            folder_id,
            &format!("reconciliation-{index}.bin"),
            &content,
        )
        .await;
    }
    for index in 0..8_u8 {
        let content = deterministic_bytes(64 * 1024, index.wrapping_add(101));
        state
            .storage
            .put_bytes(&content)
            .await
            .expect("unreferenced reconciliation object");
    }
}

async fn seed_state_events(pool: &SqlitePool, count: usize) {
    let mut transaction = pool.begin().await.expect("state event transaction");
    for index in 0..count {
        sqlx::query("INSERT INTO state_events (event_type, resources) VALUES (?, ?)")
            .bind(format!("perf.event.{}", index % 8))
            .bind(r#"["contents","sidebar"]"#)
            .execute(&mut *transaction)
            .await
            .expect("state event fixture");
    }
    transaction
        .commit()
        .await
        .expect("commit state event fixture");
}

fn deterministic_bytes(size: usize, salt: u8) -> Vec<u8> {
    (0..size)
        .map(|index| index.to_le_bytes()[0].wrapping_mul(31).wrapping_add(salt))
        .collect()
}
