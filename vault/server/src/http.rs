use std::collections::{HashSet, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Form, Json, Router};
use futures_util::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;
use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
use tower_http::compression::{CompressionLayer, CompressionLevel};
use uuid::Uuid;

use crate::admin::{
    self, AdminDirectoryPayload, AdminError, AdminGroupMemberRequest, AdminGroupRequest,
    AdminUserUpdatePayload,
};
use crate::assets::{self, AssetError};
use crate::auth::{
    AuthError, AuthMode, AuthSettings, UserContext, dev_identity, header_identity,
    oidc_token_urlsafe, session_identity, sign_session_payload,
};
use crate::config::Config;
use crate::db::DbPool;
use crate::documents::{
    ClientMeta, DocumentError, VersionDownload, archive_document, archive_folder,
    checkout_version_download, current_version_download, delete_document_forever,
    document_access_level, document_folder_path, lock_document, move_document,
    record_checkout_event_and_lock, record_document_batch_state, record_document_deleted_state,
    record_download_event, rename_document, restore_document, sweep_expired_documents,
    try_fetch_document_by_id, unlock_document, version_download_by_id,
};
use crate::exports::{
    self, ExportError, ExportExecutionContext, ExportRuntimeSettings, ExportSelectionItem,
    ExportZipOptions,
};
use crate::folders::{
    CreatedFolderPayload, FolderError, FolderPermissionUpdate, FolderRetentionUpdate,
    create_folder_path, folder_path_by_id, get_folder_by_path, get_or_create_folder_path,
    move_folder, rename_folder, update_folder_permissions, update_folder_properties,
    update_folder_retention,
};
use crate::oidc::{self, CallbackRequest, OidcError};
use crate::preferences::{PreferenceError, update_preferences_for_user};
use crate::reconciliation::{self, ReconciliationError};
use crate::shares::{self, CreateShareLinkRequest, CreateShareLinkResponse, ShareError};
use crate::site_settings::{
    SiteSettingsError, archive_permanent_delete_admin_only, site_settings_for_db,
    update_admin_site_settings,
};
use crate::state_events::{
    StateEventError, StateEventRecord, latest_state_event_id, notify_state_event_committed,
    record_state_event, state_events_after, subscribe_state_events,
};
use crate::storage::{
    BlobStorageBackend, LocalBlobStorage, SharedBlobStorage, StorageError, StoredBlob,
};
use crate::transfers::{self, TransferMaintenanceError};
use crate::uploads::{
    self, CompleteUploadRequest, CreateUploadRequest, UploadError, UploadPartHeaders,
    UploadPartIngest, UploadResultPayload, UploadRuntimeSettings, UploadSessionPayload,
};
use crate::views::{
    self, BootstrapPayload, ContentsPayload, DocumentDetailPayload, MyEditsPayload, SidebarPayload,
    ViewError,
};

const HEADER_PERCENT_HEX: &[u8; 16] = b"0123456789ABCDEF";
const DEBUG_TIMEOUT_SECONDS: i64 = 10;
static DEBUG_EVENT_STREAM_GENERATION: AtomicI64 = AtomicI64::new(0);
static DEBUG_EVENT_STREAM_RETRY_MS: AtomicI64 = AtomicI64::new(3000);

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub auth: Arc<AuthSettings>,
    pub db: DbPool,
    pub storage: SharedBlobStorage,
    pub export_execution: Arc<ExportExecutionContext>,
    upload_part_locks: Arc<AsyncMutex<HashSet<String>>>,
}

impl AppState {
    #[must_use]
    pub fn new(config: Config, auth: AuthSettings, db: DbPool, storage: SharedBlobStorage) -> Self {
        let config = config.normalized();
        let export_execution = Arc::new(ExportExecutionContext::new(export_runtime_settings(
            &config,
        )));
        Self {
            config: Arc::new(config),
            auth: Arc::new(auth),
            db,
            storage,
            export_execution,
            upload_part_locks: Arc::new(AsyncMutex::new(HashSet::new())),
        }
    }
}

fn export_runtime_settings(config: &Config) -> ExportRuntimeSettings {
    ExportRuntimeSettings {
        ttl_seconds: config.export_ttl_seconds,
        workers: config.export_workers,
        zip_options: ExportZipOptions {
            compression_threshold_bytes: config.export_zip_compression_threshold_bytes,
            compresslevel: u32::try_from(config.export_zip_compresslevel.clamp(1, 9)).unwrap_or(1),
        },
    }
}

#[derive(Debug, Clone)]
struct CspNonce(String);

pub fn router(state: AppState) -> Router {
    let security_state = state.clone();
    let app = Router::new()
        .route("/", get(index))
        .route("/s/{code}", get(share_entry))
        .route("/login", get(login))
        .route("/auth/callback", get(auth_callback))
        .route("/logout", get(logout))
        .route("/static/{*path}", get(static_asset))
        .route("/health", get(health))
        .route("/api/health", get(api_health))
        .route("/api/bench/sink", put(api_bench_sink))
        .route("/folders", post(create_folder))
        .route("/api/bootstrap", get(api_bootstrap))
        .route("/api/settings", get(api_settings))
        .route("/api/admin/directory", get(api_admin_directory))
        .merge(admin_debug_routes())
        .route("/api/admin/settings", patch(api_admin_update_settings))
        .route("/api/admin/users/{user_id}", patch(api_admin_update_user))
        .route("/api/admin/groups", post(api_admin_create_group))
        .route(
            "/api/admin/groups/{group_id}",
            patch(api_admin_update_group).delete(api_admin_delete_group),
        )
        .route(
            "/api/admin/groups/{group_id}/members",
            post(api_admin_add_group_member),
        )
        .route(
            "/api/admin/groups/{group_id}/members/{user_id}",
            axum::routing::delete(api_admin_remove_group_member),
        )
        .route(
            "/api/preferences",
            get(api_preferences).patch(api_update_preferences),
        )
        .route("/api/folders/sidebar", get(api_sidebar))
        .route("/api/folders/contents", get(api_folder_contents))
        .route("/api/share-links", post(api_create_share_link))
        .route("/api/share-links/{code}", get(api_resolve_share_link))
        .route(
            "/api/folders/properties",
            get(api_folder_properties).patch(api_update_folder_properties),
        )
        .route("/api/folders/retention", put(api_update_folder_retention))
        .route(
            "/api/folders/permissions",
            put(api_update_folder_permissions),
        )
        .route("/api/lock", post(api_lock_items))
        .route("/api/unlock", post(api_unlock_items))
        .route("/api/move", post(api_move_items))
        .route("/api/rename", post(api_rename_item))
        .route("/api/archive", post(api_archive_items))
        .route("/api/restore", post(api_restore_items))
        .route("/api/delete-forever", post(api_delete_forever))
        .route("/api/events/stream", get(api_events_stream))
        .merge(document_transfer_routes())
        .layer(middleware::from_fn_with_state(
            security_state,
            security_headers_middleware,
        ));
    gzip_layer(app, &state.config).with_state(state)
}

fn gzip_layer(app: Router<AppState>, config: &Config) -> Router<AppState> {
    if config.gzip_minimum_size <= 0 {
        return app;
    }
    let minimum_size = u16::try_from(config.gzip_minimum_size).unwrap_or(u16::MAX);
    let level = i32::try_from(config.gzip_compresslevel).unwrap_or(6);
    let predicate = SizeAbove::new(minimum_size)
        .and(NotForContentType::GRPC)
        .and(NotForContentType::IMAGES)
        .and(NotForContentType::SSE);
    app.layer(
        CompressionLayer::new()
            .gzip(true)
            .no_br()
            .no_deflate()
            .no_zstd()
            .quality(CompressionLevel::Precise(level))
            .compress_when(predicate),
    )
}

fn document_transfer_routes() -> Router<AppState> {
    Router::new()
        .route("/api/uploads", post(api_create_upload_session))
        .route(
            "/api/uploads/{session_id}",
            get(api_get_upload_session).delete(api_abort_upload_session),
        )
        .route(
            "/api/uploads/{session_id}/parts/{part_number}",
            put(api_upload_session_part),
        )
        .route(
            "/api/uploads/{session_id}/complete",
            post(api_complete_upload_session),
        )
        .route("/api/my-edits", get(api_my_edits))
        .route("/api/documents/{doc_id}/detail", get(api_document_detail))
        .route("/api/download", post(api_download_items))
        .route("/api/exports", post(api_create_export_job))
        .route(
            "/api/exports/{job_id}",
            get(api_get_export_job).delete(api_cancel_export_job),
        )
        .route(
            "/api/exports/{job_id}/download",
            get(download_export_artifact),
        )
        .route("/documents", post(legacy_create_document))
        .route("/documents/{doc_id}", get(legacy_document_detail_redirect))
        .route(
            "/documents/{doc_id}/download",
            get(download_current_document_version),
        )
        .route(
            "/documents/{doc_id}/checkout",
            get(checkout_document_version),
        )
        .route("/documents/{doc_id}/checkin", post(legacy_checkin_document))
        .route(
            "/documents/{doc_id}/versions/{version_id}/download",
            get(download_document_version),
        )
}

fn admin_debug_routes() -> Router<AppState> {
    Router::new()
        .route("/api/admin/debug/error", post(api_admin_debug_error))
        .route("/api/admin/debug/timeout", post(api_admin_debug_timeout))
        .route(
            "/api/admin/debug/emit-state",
            post(api_admin_debug_emit_state),
        )
        .route(
            "/api/admin/debug/sweep-ttl",
            post(api_admin_debug_sweep_ttl),
        )
        .route(
            "/api/admin/debug/storage-report",
            post(api_admin_debug_storage_report),
        )
        .route("/api/admin/debug/seed", post(api_admin_debug_seed))
        .route(
            "/api/admin/debug/reset-database",
            post(api_admin_debug_reset_database),
        )
}

async fn health() -> &'static str {
    "ok"
}

async fn api_bench_sink(request: Request) -> Result<Json<Value>, ApiError> {
    if !bench_env_flag("VAULT_BENCH_ROUTES") {
        return Err(ApiError::NotFound("Not found".to_string()));
    }
    let hash_body = bench_env_flag("VAULT_BENCH_SINK_HASH");
    let write_body = bench_env_flag("VAULT_BENCH_SINK_WRITE");
    let mut digest = hash_body.then(Sha256::new);
    let mut output = if write_body {
        Some(open_bench_sink_file().await?)
    } else {
        None
    };
    let mut size_bytes = 0_i64;
    let mut body = request.into_body().into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| ApiError::BadRequest("Upload failed".to_string()))?;
        if chunk.is_empty() {
            continue;
        }
        size_bytes += i64::try_from(chunk.len())
            .map_err(|_| ApiError::BadRequest("Upload too large".to_string()))?;
        if let Some(digest) = digest.as_mut() {
            digest.update(&chunk);
        }
        if let Some(output) = output.as_mut() {
            output
                .write_all(&chunk)
                .await
                .map_err(|error| bench_sink_io_error(&error))?;
        }
    }
    if let Some(output) = output.as_mut() {
        output
            .flush()
            .await
            .map_err(|error| bench_sink_io_error(&error))?;
    }
    Ok(Json(json!({
        "bytes": size_bytes,
        "sha256": digest.map(|digest| lower_hex(&digest.finalize())),
    })))
}

async fn open_bench_sink_file() -> Result<tokio::fs::File, ApiError> {
    let sink_dir = std::env::var("VAULT_BENCH_SINK_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map_or_else(
            || std::env::temp_dir().join("vault-bench-sink"),
            PathBuf::from,
        );
    tokio::fs::create_dir_all(&sink_dir)
        .await
        .map_err(|error| bench_sink_io_error(&error))?;
    let path = sink_dir.join(format!("sink-{}.bin", Uuid::new_v4().simple()));
    tokio::fs::File::create(path)
        .await
        .map_err(|error| bench_sink_io_error(&error))
}

fn bench_env_flag(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn bench_sink_io_error(error: &std::io::Error) -> ApiError {
    tracing::error!(?error, "benchmark sink I/O failed");
    ApiError::Internal("Benchmark sink I/O failed".to_string())
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

async fn security_headers_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let public_https = request_is_public_https(&state.auth, request.headers(), request.uri());
    let path = request.uri().path();
    let needs_nonce = state.auth.security_headers.enabled || path == "/" || path.starts_with("/s/");
    let nonce = CspNonce(if needs_nonce {
        Uuid::new_v4().simple().to_string()
    } else {
        String::new()
    });
    request.extensions_mut().insert(nonce.clone());
    let mut response = next.run(request).await;
    apply_security_headers(&state.auth, response.headers_mut(), &nonce.0, public_https);
    response
}

async fn index(
    State(state): State<AppState>,
    Extension(csp_nonce): Extension<CspNonce>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Html<String>, ApiError> {
    app_shell_response(state, headers, &uri, None, &csp_nonce.0).await
}

async fn share_entry(
    State(state): State<AppState>,
    Extension(csp_nonce): Extension<CspNonce>,
    headers: HeaderMap,
    uri: Uri,
    Path(code): Path<String>,
) -> Result<Html<String>, ApiError> {
    if !shares::valid_share_code(&code) {
        return Err(ApiError::NotFound("Share link not found".to_string()));
    }
    app_shell_response(state, headers, &uri, Some(code), &csp_nonce.0).await
}

async fn app_shell_response(
    state: AppState,
    headers: HeaderMap,
    uri: &Uri,
    share_code: Option<String>,
    nonce: &str,
) -> Result<Html<String>, ApiError> {
    let user = current_browser_user(&state, &headers, uri).await?;
    let initial_state = views::build_initial_state_payload(
        &state.db,
        &user,
        &state.auth,
        &state.config,
        "",
        share_code,
    )
    .await?;
    let manifest = assets::load_static_asset_manifest(&state.config.static_dir).await?;
    Ok(Html(assets::app_shell_html(
        &initial_state,
        &manifest,
        &headers,
        nonce,
    )?))
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<RedirectQuery>,
) -> Result<Response, ApiError> {
    if state.auth.mode != AuthMode::Oidc {
        return Ok(redirect_response("/", Vec::new()));
    }
    let authorization_endpoint = oidc::authorization_endpoint(&state.auth).await?;
    if state.auth.oidc_client_id.trim().is_empty() {
        return Err(ApiError::Internal("OIDC is not configured".to_string()));
    }
    let rd = safe_redirect(query.rd.as_deref());
    let oidc_state = oidc_token_urlsafe(state.auth.oidc_nonce_bytes)
        .map_err(|_| ApiError::Internal("Could not generate OIDC state".to_string()))?;
    let nonce = oidc_token_urlsafe(state.auth.oidc_nonce_bytes)
        .map_err(|_| ApiError::Internal("Could not generate OIDC nonce".to_string()))?;
    let mut state_payload = Map::new();
    state_payload.insert("state".to_string(), Value::String(oidc_state.clone()));
    state_payload.insert("nonce".to_string(), Value::String(nonce.clone()));
    state_payload.insert("rd".to_string(), Value::String(rd));
    state_payload.insert("exp".to_string(), json!(unix_timestamp_now() + 600.0));
    let state_cookie = sign_session_payload(&state.auth, &state_payload)?;
    let redirect_uri = oidc_redirect_uri(&state.auth, &headers, &uri);
    let location = format!(
        "{}?{}",
        authorization_endpoint,
        form_urlencode(&[
            ("client_id", state.auth.oidc_client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", state.auth.oidc_scopes.as_str()),
            ("state", oidc_state.as_str()),
            ("nonce", nonce.as_str()),
        ])
    );
    Ok(redirect_response(
        &location,
        vec![set_cookie_header(
            &state.auth.oidc_state_cookie_name,
            &state_cookie,
            600,
            cookie_secure(&state.auth, &headers),
        )],
    ))
}

async fn auth_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<OidcCallbackQuery>,
) -> Result<Response, ApiError> {
    if state.auth.mode != AuthMode::Oidc {
        return Ok(redirect_response("/", Vec::new()));
    }
    if let Some(error) = query.error {
        return Err(ApiError::Unauthorized(format!(
            "OIDC login failed: {error}"
        )));
    }
    let redirect_uri = oidc_redirect_uri(&state.auth, &headers, &uri);
    let result = oidc::complete_callback(
        &state.auth,
        &state.db,
        CallbackRequest {
            code: query.code.as_deref().unwrap_or_default(),
            state: query.state.as_deref().unwrap_or_default(),
            cookie_header: header_value(&headers, header::COOKIE),
            redirect_uri: &redirect_uri,
        },
    )
    .await?;
    let mut session_payload = Map::new();
    session_payload.insert("uid".to_string(), json!(result.user.vault_user_id));
    session_payload.insert(
        "exp".to_string(),
        json!(unix_timestamp_now_i64().saturating_add(state.auth.session_max_age_seconds)),
    );
    let session_cookie = sign_session_payload(&state.auth, &session_payload)?;
    Ok(redirect_response(
        &result.redirect_path,
        vec![
            set_cookie_header(
                &state.auth.session_cookie_name,
                &session_cookie,
                state.auth.session_max_age_seconds,
                cookie_secure(&state.auth, &headers),
            ),
            delete_cookie_header(&state.auth.oidc_state_cookie_name),
        ],
    ))
}

async fn logout(
    State(state): State<AppState>,
    Query(query): Query<RedirectQuery>,
) -> Result<Response, ApiError> {
    let rd = safe_redirect(query.rd.as_deref());
    Ok(redirect_response(
        &rd,
        vec![
            delete_cookie_header(&state.auth.session_cookie_name),
            delete_cookie_header(&state.auth.oidc_state_cookie_name),
        ],
    ))
}

async fn static_asset(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Result<Response, ApiError> {
    let asset = assets::read_static_asset(&state.config.static_dir, &path).await?;
    let mut response = Response::new(Body::from(asset.bytes));
    insert_header(
        response.headers_mut(),
        header::CONTENT_TYPE,
        &asset.content_type,
    );
    Ok(response)
}

async fn api_health(State(state): State<AppState>) -> Json<HealthPayload> {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.db)
        .await
        .is_ok();
    Json(HealthPayload {
        ok: db_ok,
        storage_backend: state.config.storage_backend.clone(),
    })
}

#[derive(Debug, Serialize)]
struct HealthPayload {
    ok: bool,
    storage_backend: String,
}

#[derive(Debug, Default, Deserialize)]
struct ContentsQuery {
    #[serde(default)]
    folder: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    recursive: bool,
}

#[derive(Debug, Default, Deserialize)]
struct FolderPropertiesQuery {
    #[serde(default)]
    path: String,
}

#[derive(Debug, Default, Deserialize)]
struct BootstrapQuery {
    #[serde(default)]
    folder: String,
}

#[derive(Debug, Default, Deserialize)]
struct RedirectQuery {
    rd: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OidcCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DebugErrorPayload {
    kind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DebugStateEventPayload {
    #[serde(default = "default_debug_state_resources")]
    resources: Vec<String>,
}

fn default_debug_state_resources() -> Vec<String> {
    vec![
        "contents".to_string(),
        "sidebar".to_string(),
        "my_edits".to_string(),
    ]
}

#[derive(Debug, Deserialize)]
struct PreferencesPatchRequest {
    #[serde(default = "empty_json_object")]
    preferences: Value,
}

#[derive(Debug, Serialize)]
struct PreferencesResponse {
    preferences: Value,
}

#[derive(Debug, Deserialize)]
struct CreateFolderForm {
    folder: String,
}

#[derive(Debug, Deserialize)]
struct FolderPropertiesPatchRequest {
    path: String,
    color: Option<String>,
    icon: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FolderPermissionRowRequest {
    group_id: i64,
    #[serde(default = "default_true")]
    can_view: bool,
    #[serde(default = "default_true")]
    can_read: bool,
    #[serde(default)]
    can_write: bool,
}

#[derive(Debug, Deserialize)]
struct FolderPermissionsPutRequest {
    path: String,
    #[serde(default)]
    permissions: Vec<FolderPermissionRowRequest>,
}

#[derive(Debug, Deserialize)]
struct FolderRetentionPutRequest {
    path: String,
    default_ttl_days: Option<i64>,
    default_ttl_action: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ActionPayloadRequest {
    #[serde(default)]
    items: Vec<ActionItemRequest>,
    destination_folder: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ActionItemRequest {
    #[serde(rename = "type")]
    item_type: String,
    id: Option<i64>,
    path: Option<String>,
}

#[derive(Debug, Clone)]
enum NormalizedActionItem {
    Document {
        id: i64,
    },
    Folder {
        id: i64,
        path: String,
    },
    MissingFolder {
        id: Option<i64>,
        path: Option<String>,
    },
}

#[derive(Debug, Default, Serialize)]
struct BulkActionResponse {
    ok: Vec<ActionResultPayload>,
    failed: Vec<ActionResultPayload>,
    skipped: Vec<ActionResultPayload>,
}

#[derive(Debug, Serialize)]
struct ActionResultPayload {
    item: ActionItemPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
struct ActionItemPayload {
    #[serde(rename = "type")]
    item_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Debug, Serialize)]
struct SettingsResponse {
    settings: Value,
}

#[derive(Debug, Deserialize)]
struct AdminSettingsPatchRequest {
    #[serde(default = "empty_json_object")]
    settings: Value,
}

async fn api_bootstrap(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BootstrapQuery>,
) -> Result<Json<BootstrapPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        views::build_bootstrap_payload(&state.db, &user, &state.auth, &state.config, &query.folder)
            .await?,
    ))
}

async fn create_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateFolderForm>,
) -> Result<Json<CreatedFolderPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    let payload = create_folder_path(&state.db, &form.folder, &user).await?;
    notify_state_event_committed();
    Ok(Json(payload))
}

async fn api_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SettingsResponse>, ApiError> {
    let _user = current_user(&state, &headers).await?;
    Ok(Json(SettingsResponse {
        settings: site_settings_for_db(&state.db).await?,
    }))
}

async fn api_admin_directory(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminDirectoryPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    require_admin(&user)?;
    Ok(Json(
        admin::build_admin_directory_payload(&state.db, &state.auth).await?,
    ))
}

async fn api_admin_debug_error(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<DebugErrorPayload>,
) -> Result<Json<Value>, ApiError> {
    let _user = require_dev_admin(&state, &headers).await?;
    match payload
        .kind
        .as_deref()
        .unwrap_or("server")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "bad-request" => Err(ApiError::BadRequest("Debug bad request error".to_string())),
        "forbidden" => Err(ApiError::Forbidden("Debug forbidden error".to_string())),
        "not-found" => Err(ApiError::NotFound("Debug not found error".to_string())),
        "unavailable" => Err(ApiError::ServiceUnavailable(
            "Debug service unavailable".to_string(),
        )),
        _ => Err(ApiError::Internal("Debug server error".to_string())),
    }
}

async fn api_admin_debug_timeout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let _user = require_dev_admin(&state, &headers).await?;
    let stream_generation = DEBUG_EVENT_STREAM_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let stream_retry_ms = DEBUG_TIMEOUT_SECONDS * 1000;
    DEBUG_EVENT_STREAM_RETRY_MS.store(stream_retry_ms, Ordering::SeqCst);
    notify_state_event_committed();
    Ok(Json(debug_action_result(json!({
        "action": "timeout",
        "seconds": DEBUG_TIMEOUT_SECONDS,
        "stream_generation": stream_generation,
        "stream_retry_ms": stream_retry_ms,
    }))))
}

async fn api_admin_debug_emit_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<DebugStateEventPayload>,
) -> Result<Json<Value>, ApiError> {
    let _user = require_dev_admin(&state, &headers).await?;
    let resources = debug_allowed_resources(&payload.resources);
    let state_resources = if resources.is_empty() {
        vec!["contents", "sidebar", "my_edits"]
    } else {
        resources.iter().map(String::as_str).collect::<Vec<_>>()
    };
    record_state_event(&state.db, "debug.refresh", &state_resources).await?;
    notify_state_event_committed();
    Ok(Json(debug_action_result(json!({
        "action": "emit-state",
        "resources": resources,
    }))))
}

async fn api_admin_debug_sweep_ttl(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let _user = require_dev_admin(&state, &headers).await?;
    let transfers_path = state.config.transfers_path();
    let documents = sweep_expired_documents(&state.db, 250).await?;
    if documents.has_state_changes() {
        notify_state_event_committed();
    }
    Ok(Json(debug_action_result(json!({
        "action": "sweep-ttl",
        "result": {
            "documents": documents,
            "transfers": transfers::sweep_expired_transfers(
                &state.db,
                &state.storage,
                &transfers_path,
            ).await?,
        },
    }))))
}

async fn api_admin_debug_storage_report(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let _user = require_dev_admin(&state, &headers).await?;
    let local_storage =
        LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix);
    Ok(Json(debug_action_result(json!({
        "action": "storage-report",
        "report": reconciliation::storage_reconciliation_report(&state.db, &local_storage, false).await?,
    }))))
}

async fn api_admin_debug_seed(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = require_dev_admin(&state, &headers).await?;
    let folder = get_or_create_folder_path(&state.db, Some("Debug Samples")).await?;
    let name = format!("debug-sample-{}.txt", Uuid::new_v4().simple());
    let content = format!("Debug sample created by Rust Vault as {}\n", user.name);
    let stored = state.storage.put_bytes(content.as_bytes()).await?;
    let document_id = create_debug_document(&state, folder.id, &name, &stored, &user).await?;
    record_state_event(&state.db, "folder.debug.seeded", &["contents", "sidebar"]).await?;
    notify_state_event_committed();
    Ok(Json(debug_action_result(json!({
        "action": "seed",
        "document_id": document_id,
        "folder": "Debug Samples",
        "name": name,
    }))))
}

async fn api_admin_debug_reset_database(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let _user = require_dev_admin(&state, &headers).await?;
    crate::db::reset(&state.db)
        .await
        .map_err(|error| ApiError::Internal(format!("Database reset failed: {error}")))?;
    notify_state_event_committed();
    Ok(Json(debug_action_result(json!({
        "action": "reset-database",
        "reload": true,
    }))))
}

async fn api_admin_update_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AdminSettingsPatchRequest>,
) -> Result<Json<AdminDirectoryPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    require_admin(&user)?;
    update_admin_site_settings(&state.db, &payload.settings).await?;
    notify_state_event_committed();
    Ok(Json(
        admin::build_admin_directory_payload(&state.db, &state.auth).await?,
    ))
}

async fn api_admin_update_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
    Json(payload): Json<AdminUserUpdatePayload>,
) -> Result<Json<AdminDirectoryPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    require_admin(&user)?;
    admin::update_user(&state.db, &state.auth, user_id, &payload).await?;
    notify_state_event_committed();
    Ok(Json(
        admin::build_admin_directory_payload(&state.db, &state.auth).await?,
    ))
}

async fn api_admin_create_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AdminGroupRequest>,
) -> Result<Json<AdminDirectoryPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    require_admin(&user)?;
    admin::create_group(&state.db, &payload).await?;
    notify_state_event_committed();
    Ok(Json(
        admin::build_admin_directory_payload(&state.db, &state.auth).await?,
    ))
}

async fn api_admin_update_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(group_id): Path<i64>,
    Json(payload): Json<AdminGroupRequest>,
) -> Result<Json<AdminDirectoryPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    require_admin(&user)?;
    admin::update_group(&state.db, &state.auth, group_id, &payload).await?;
    notify_state_event_committed();
    Ok(Json(
        admin::build_admin_directory_payload(&state.db, &state.auth).await?,
    ))
}

async fn api_admin_delete_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(group_id): Path<i64>,
) -> Result<Json<AdminDirectoryPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    require_admin(&user)?;
    admin::delete_group(&state.db, &state.auth, group_id).await?;
    notify_state_event_committed();
    Ok(Json(
        admin::build_admin_directory_payload(&state.db, &state.auth).await?,
    ))
}

async fn api_admin_add_group_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(group_id): Path<i64>,
    Json(payload): Json<AdminGroupMemberRequest>,
) -> Result<Json<AdminDirectoryPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    require_admin(&user)?;
    admin::add_group_member(&state.db, group_id, &payload).await?;
    notify_state_event_committed();
    Ok(Json(
        admin::build_admin_directory_payload(&state.db, &state.auth).await?,
    ))
}

async fn api_admin_remove_group_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((group_id, user_id)): Path<(i64, i64)>,
) -> Result<Json<AdminDirectoryPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    require_admin(&user)?;
    admin::remove_group_member(&state.db, &state.auth, group_id, user_id).await?;
    notify_state_event_committed();
    Ok(Json(
        admin::build_admin_directory_payload(&state.db, &state.auth).await?,
    ))
}

async fn api_preferences(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PreferencesResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(PreferencesResponse {
        preferences: views::build_preferences_payload(&state.db, &user).await?,
    }))
}

async fn api_update_preferences(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<PreferencesPatchRequest>,
) -> Result<Json<PreferencesResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    update_preferences_for_user(&state.db, &user, &payload.preferences).await?;
    Ok(Json(PreferencesResponse {
        preferences: views::build_preferences_payload(&state.db, &user).await?,
    }))
}

async fn api_sidebar(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SidebarPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(views::build_sidebar_payload(&state.db, &user).await?))
}

async fn api_folder_contents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ContentsQuery>,
) -> Result<Json<ContentsPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        views::build_contents_payload(&state.db, &query.folder, &user, &query.q, query.recursive)
            .await?,
    ))
}

async fn api_create_share_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateShareLinkRequest>,
) -> Result<Json<CreateShareLinkResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        shares::create_share_link(&state.db, &state.auth.public_url, payload, &user).await?,
    ))
}

async fn api_resolve_share_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        shares::resolve_share_link(&state.db, &code, &user).await?,
    ))
}

async fn api_my_edits(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MyEditsPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(views::build_my_edits_payload(&state.db, &user).await?))
}

async fn api_events_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let _user = current_user(&state, &headers).await?;
    let last_id = event_stream_start_id(&state.db, &headers).await?;
    Ok(Sse::new(state_event_stream(
        state.db.clone(),
        last_id,
        state.auth.dev_mode,
    )))
}

async fn api_folder_properties(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FolderPropertiesQuery>,
) -> Result<Json<Value>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        views::build_folder_properties_payload(&state.db, &query.path, &user).await?,
    ))
}

async fn api_update_folder_properties(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<FolderPropertiesPatchRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = current_user(&state, &headers).await?;
    update_folder_properties(
        &state.db,
        &payload.path,
        payload.color.as_deref(),
        payload.icon.as_deref(),
        &user,
    )
    .await?;
    notify_state_event_committed();
    Ok(Json(
        views::build_folder_properties_payload(&state.db, &payload.path, &user).await?,
    ))
}

async fn api_update_folder_permissions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<FolderPermissionsPutRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = current_user(&state, &headers).await?;
    require_admin(&user)?;
    let permissions = payload
        .permissions
        .iter()
        .map(|permission| FolderPermissionUpdate {
            group_id: permission.group_id,
            can_view: permission.can_view,
            can_read: permission.can_read,
            can_write: permission.can_write,
        })
        .collect::<Vec<_>>();
    update_folder_permissions(&state.db, &payload.path, &permissions, &user).await?;
    notify_state_event_committed();
    Ok(Json(
        views::build_folder_properties_payload(&state.db, &payload.path, &user).await?,
    ))
}

async fn api_update_folder_retention(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<FolderRetentionPutRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = current_user(&state, &headers).await?;
    update_folder_retention(
        &state.db,
        &payload.path,
        &FolderRetentionUpdate {
            default_ttl_days: payload.default_ttl_days,
            default_ttl_action: payload.default_ttl_action,
        },
        &user,
    )
    .await?;
    notify_state_event_committed();
    Ok(Json(
        views::build_folder_properties_payload(&state.db, &payload.path, &user).await?,
    ))
}

async fn api_document_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_id): Path<i64>,
) -> Result<Json<DocumentDetailPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        views::build_document_detail_payload(&state.db, doc_id, &user).await?,
    ))
}

async fn api_download_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ActionPayloadRequest>,
) -> Result<Response, ApiError> {
    let user = current_user(&state, &headers).await?;
    let items = normalize_action_items(&state.db, &payload).await?;
    if let [NormalizedActionItem::Document { id }] = items.as_slice() {
        let download = current_version_download(&state.db, *id, &user).await?;
        let response =
            version_download_response(state.storage.as_ref(), &download, &headers).await?;
        record_download_event(&state.db, &download, &user, &client_meta(&headers), true).await?;
        notify_state_event_committed();
        return Ok(response);
    }
    let export = exports::create_download_job_with_runtime(
        &state.db,
        &state.storage,
        &state.config.transfers_path(),
        &export_selection_items(&items),
        &user,
        &state.export_execution,
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(export)).into_response())
}

async fn api_create_export_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ActionPayloadRequest>,
) -> Result<Json<exports::ExportJobPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    let items = normalize_action_items(&state.db, &payload).await?;
    Ok(Json(
        exports::create_export_job_with_runtime(
            &state.db,
            &state.storage,
            &state.config.transfers_path(),
            &export_selection_items(&items),
            &user,
            &state.export_execution,
        )
        .await?,
    ))
}

async fn api_get_export_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<exports::ExportJobPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        exports::get_export_job(&state.db, &job_id, &user).await?,
    ))
}

async fn api_cancel_export_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<exports::ExportJobPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        exports::cancel_export_job(&state.db, &job_id, &user).await?,
    ))
}

async fn download_export_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Response, ApiError> {
    let user = current_user(&state, &headers).await?;
    let artifact = exports::export_artifact_download(&state.db, &job_id, &user).await?;
    let download = VersionDownload {
        document_id: 0,
        document_path: artifact.filename.clone(),
        version_id: artifact.job_id,
        version_number: 1,
        filename: artifact.filename,
        mime_type: Some(artifact.mime_type),
        hash_algo: artifact.hash_algo,
        hash: artifact.hash,
        size_bytes: artifact.size_bytes,
        backend: artifact.backend,
        bucket: artifact.bucket,
        object_key: artifact.object_key,
    };
    version_download_response(state.storage.as_ref(), &download, &headers).await
}

async fn api_create_upload_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateUploadRequest>,
) -> Result<Json<UploadSessionPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        uploads::create_upload_session(
            &state.db,
            &state.config.transfers_path(),
            &state.auth.session_secret,
            UploadRuntimeSettings {
                max_upload_bytes: state.config.max_upload_bytes,
                transfer_chunk_bytes: state.config.transfer_chunk_bytes,
                transfer_session_ttl_seconds: state.config.transfer_session_ttl_seconds,
            },
            payload,
            &user,
            &client_meta(&headers),
        )
        .await?,
    ))
}

async fn api_get_upload_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<UploadSessionPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        uploads::get_upload_session(
            &state.db,
            &state.config.transfers_path(),
            &state.auth.session_secret,
            &session_id,
            &user,
        )
        .await?,
    ))
}

async fn api_upload_session_part(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((session_id, part_number)): Path<(String, i64)>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let part_headers = UploadPartHeaders {
        offset: required_i64_header(&headers, "x-upload-offset")?,
        size: required_i64_header(&headers, "x-upload-size")?,
        sha256: header_value_by_name(&headers, "x-upload-sha256"),
    };
    let transfers_path = state.config.transfers_path();
    let ingest = UploadPartIngest {
        transfers_path: &transfers_path,
        session_id: &session_id,
        part_number,
        headers: part_headers,
    };
    let stream = request.into_body().into_data_stream();
    let _part_lock = UploadPartLock::acquire(
        state.upload_part_locks.clone(),
        format!("{session_id}:{part_number}"),
    )
    .await;
    if let Some(token) = header_value_by_name(&headers, "x-upload-token") {
        let token_claims =
            uploads::verify_upload_token_claims(&state.auth.session_secret, token, &session_id)?;
        uploads::ingest_upload_part_with_token(ingest, token_claims, stream).await?;
    } else {
        let user = current_user(&state, &headers).await?;
        uploads::ingest_upload_part(&state.db, ingest, &user, stream).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

struct UploadPartLock {
    locks: Arc<AsyncMutex<HashSet<String>>>,
    key: String,
}

impl UploadPartLock {
    async fn acquire(locks: Arc<AsyncMutex<HashSet<String>>>, key: String) -> Self {
        loop {
            let inserted = {
                let mut active = locks.lock().await;
                active.insert(key.clone())
            };
            if inserted {
                return Self { locks, key };
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for UploadPartLock {
    fn drop(&mut self) {
        let locks = self.locks.clone();
        let key = self.key.clone();
        tokio::spawn(async move {
            locks.lock().await.remove(&key);
        });
    }
}

async fn api_complete_upload_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(payload): Json<CompleteUploadRequest>,
) -> Result<Json<UploadResultPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    let payload = uploads::complete_upload_session(
        &state.db,
        state.storage.as_ref(),
        &state.config.transfers_path(),
        &session_id,
        payload.sha256.as_deref(),
        &user,
    )
    .await?;
    notify_state_event_committed();
    Ok(Json(payload))
}

async fn api_abort_upload_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<UploadSessionPayload>, ApiError> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(
        uploads::abort_upload_session(
            &state.db,
            &state.config.transfers_path(),
            &state.auth.session_secret,
            &session_id,
            &user,
        )
        .await?,
    ))
}

async fn legacy_create_document(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let _user = current_user(&state, &headers).await?;
    Ok((
        StatusCode::GONE,
        Json(ErrorPayload {
            detail: "Use resumable upload sessions".to_string(),
        }),
    )
        .into_response())
}

async fn legacy_document_detail_redirect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_id): Path<i64>,
) -> Result<Response, ApiError> {
    let user = current_user(&state, &headers).await?;
    let document = try_fetch_document_by_id(&state.db, doc_id)
        .await?
        .ok_or(DocumentError::DocumentNotFound)?;
    let level = document_access_level(&state.db, &document, &user).await?;
    if level < 1 {
        return Err(ApiError::Document(DocumentError::DocumentNotFound));
    }
    Ok(redirect_response("/", Vec::new()))
}

async fn download_current_document_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_id): Path<i64>,
) -> Result<Response, ApiError> {
    let user = current_user(&state, &headers).await?;
    let download = current_version_download(&state.db, doc_id, &user).await?;
    let response = version_download_response(state.storage.as_ref(), &download, &headers).await?;
    record_download_event(&state.db, &download, &user, &client_meta(&headers), true).await?;
    notify_state_event_committed();
    Ok(response)
}

async fn checkout_document_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_id): Path<i64>,
) -> Result<Response, ApiError> {
    let user = current_user(&state, &headers).await?;
    let download = checkout_version_download(&state.db, doc_id, &user).await?;
    let response = version_download_response(state.storage.as_ref(), &download, &headers).await?;
    record_checkout_event_and_lock(&state.db, &download, &user, &client_meta(&headers)).await?;
    notify_state_event_committed();
    Ok(response)
}

async fn legacy_checkin_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_doc_id): Path<i64>,
) -> Result<Response, ApiError> {
    let _user = current_user(&state, &headers).await?;
    Ok((
        StatusCode::GONE,
        Json(ErrorPayload {
            detail: "Use resumable upload sessions".to_string(),
        }),
    )
        .into_response())
}

async fn download_document_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((doc_id, version_id)): Path<(i64, String)>,
) -> Result<Response, ApiError> {
    let user = current_user(&state, &headers).await?;
    let download = version_download_by_id(&state.db, doc_id, &version_id, &user).await?;
    let response = version_download_response(state.storage.as_ref(), &download, &headers).await?;
    record_download_event(&state.db, &download, &user, &client_meta(&headers), false).await?;
    notify_state_event_committed();
    Ok(response)
}

async fn api_lock_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ActionPayloadRequest>,
) -> Result<Json<BulkActionResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    let items = normalize_bulk_action_items(&state.db, &payload).await?;
    let meta = client_meta(&headers);
    let mut response = BulkActionResponse::default();
    let mut changed = false;
    for item in items {
        match &item {
            NormalizedActionItem::Document { id } => {
                match lock_document(&state.db, *id, &user, &meta).await {
                    Ok(result) => {
                        response.ok.push(action_result(&item, Some(result.detail)));
                        changed = true;
                    }
                    Err(error) => {
                        let detail = document_action_error_detail(error)?;
                        response.failed.push(action_result(&item, Some(detail)));
                    }
                }
            }
            NormalizedActionItem::Folder { .. } => {
                response.failed.push(action_result(
                    &item,
                    Some("Only files can be locked".to_string()),
                ));
            }
            NormalizedActionItem::MissingFolder { .. } => {
                response.failed.push(missing_folder_action_result(&item));
            }
        }
    }
    if changed {
        record_document_batch_state(&state.db, "lock").await?;
        notify_state_event_committed();
    }
    Ok(Json(response))
}

async fn api_unlock_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ActionPayloadRequest>,
) -> Result<Json<BulkActionResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    let items = normalize_bulk_action_items(&state.db, &payload).await?;
    let meta = client_meta(&headers);
    let mut response = BulkActionResponse::default();
    let mut changed = false;
    for item in items {
        match &item {
            NormalizedActionItem::Document { id } => {
                match unlock_document(&state.db, *id, &user, &meta).await {
                    Ok(result) => {
                        response.ok.push(action_result(&item, Some(result.detail)));
                        changed = true;
                    }
                    Err(error) => {
                        let detail = document_action_error_detail(error)?;
                        response.failed.push(action_result(&item, Some(detail)));
                    }
                }
            }
            NormalizedActionItem::Folder { .. } => {
                response.failed.push(action_result(
                    &item,
                    Some("Only files can be unlocked".to_string()),
                ));
            }
            NormalizedActionItem::MissingFolder { .. } => {
                response.failed.push(missing_folder_action_result(&item));
            }
        }
    }
    if changed {
        record_document_batch_state(&state.db, "unlock").await?;
        notify_state_event_committed();
    }
    Ok(Json(response))
}

async fn api_delete_forever(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ActionPayloadRequest>,
) -> Result<Json<BulkActionResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    if archive_permanent_delete_admin_only(&state.db).await? && !user.is_admin {
        return Err(ApiError::AdminRequired);
    }
    let items = normalize_bulk_action_items(&state.db, &payload).await?;
    let mut response = BulkActionResponse::default();
    let mut changed = false;
    for item in items {
        match &item {
            NormalizedActionItem::Document { id } => {
                match delete_document_forever(&state.db, *id, &user).await {
                    Ok(detail) => {
                        response.ok.push(action_result(&item, Some(detail)));
                        changed = true;
                    }
                    Err(error) => {
                        let detail = document_action_error_detail(error)?;
                        response.failed.push(action_result(&item, Some(detail)));
                    }
                }
            }
            NormalizedActionItem::Folder { .. } => {
                response.failed.push(action_result(
                    &item,
                    Some("Delete forever is only available for archived files".to_string()),
                ));
            }
            NormalizedActionItem::MissingFolder { .. } => {
                response.failed.push(missing_folder_action_result(&item));
            }
        }
    }
    if changed {
        record_document_deleted_state(&state.db).await?;
        notify_state_event_committed();
    }
    Ok(Json(response))
}

async fn api_move_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ActionPayloadRequest>,
) -> Result<Json<BulkActionResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    let destination = crate::folders::normalize_folder(payload.destination_folder.as_deref())?;
    let items = normalize_bulk_action_items(&state.db, &payload).await?;
    let meta = client_meta(&headers);
    let mut response = BulkActionResponse::default();
    let mut changed = false;
    for item in items {
        match &item {
            NormalizedActionItem::Document { id } => {
                match move_document(&state.db, *id, &destination, &user, &meta).await {
                    Ok(detail) => {
                        response.ok.push(action_result(&item, Some(detail)));
                        changed = true;
                    }
                    Err(error) => {
                        let detail = document_action_error_detail(error)?;
                        response.failed.push(action_result(&item, Some(detail)));
                    }
                }
            }
            NormalizedActionItem::Folder { id, .. } => {
                match move_folder(&state.db, *id, &destination, &user).await {
                    Ok(result) => {
                        response.ok.push(action_result(&item, Some(result.path)));
                        changed = true;
                    }
                    Err(error) => {
                        let detail = folder_action_error_detail(error)?;
                        response.failed.push(action_result(&item, Some(detail)));
                    }
                }
            }
            NormalizedActionItem::MissingFolder { .. } => {
                response.failed.push(missing_folder_action_result(&item));
            }
        }
    }
    if changed {
        record_document_batch_state(&state.db, "move").await?;
        notify_state_event_committed();
    }
    Ok(Json(response))
}

async fn api_archive_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ActionPayloadRequest>,
) -> Result<Json<BulkActionResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    let items = normalize_bulk_action_items(&state.db, &payload).await?;
    let meta = client_meta(&headers);
    let mut response = BulkActionResponse::default();
    let mut changed = false;
    for item in items {
        match &item {
            NormalizedActionItem::Document { id } => {
                match archive_document(&state.db, *id, &user, &meta).await {
                    Ok(detail) => {
                        response.ok.push(action_result(&item, Some(detail)));
                        changed = true;
                    }
                    Err(error) => {
                        let detail = document_action_error_detail(error)?;
                        response.failed.push(action_result(&item, Some(detail)));
                    }
                }
            }
            NormalizedActionItem::Folder { id, .. } => {
                match archive_folder(&state.db, *id, &user, &meta).await {
                    Ok(detail) => {
                        response.ok.push(action_result(&item, Some(detail)));
                        changed = true;
                    }
                    Err(error) => {
                        let detail = document_action_error_detail(error)?;
                        response.failed.push(action_result(&item, Some(detail)));
                    }
                }
            }
            NormalizedActionItem::MissingFolder { .. } => {
                response.failed.push(missing_folder_action_result(&item));
            }
        }
    }
    if changed {
        record_document_batch_state(&state.db, "archive").await?;
        notify_state_event_committed();
    }
    Ok(Json(response))
}

async fn api_restore_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ActionPayloadRequest>,
) -> Result<Json<BulkActionResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    let items = normalize_bulk_action_items(&state.db, &payload).await?;
    let meta = client_meta(&headers);
    let mut response = BulkActionResponse::default();
    let mut changed = false;
    for item in items {
        match &item {
            NormalizedActionItem::Document { id } => {
                match restore_document(&state.db, *id, &user, &meta).await {
                    Ok(detail) => {
                        response.ok.push(action_result(&item, Some(detail)));
                        changed = true;
                    }
                    Err(error) => {
                        let detail = document_action_error_detail(error)?;
                        response.failed.push(action_result(&item, Some(detail)));
                    }
                }
            }
            NormalizedActionItem::Folder { .. } => {
                response.failed.push(action_result(
                    &item,
                    Some("Restore archived files, not folders".to_string()),
                ));
            }
            NormalizedActionItem::MissingFolder { .. } => {
                response.failed.push(missing_folder_action_result(&item));
            }
        }
    }
    if changed {
        record_document_batch_state(&state.db, "restore").await?;
        notify_state_event_committed();
    }
    Ok(Json(response))
}

async fn api_rename_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ActionPayloadRequest>,
) -> Result<Json<BulkActionResponse>, ApiError> {
    let user = current_user(&state, &headers).await?;
    let items = normalize_bulk_action_items(&state.db, &payload).await?;
    if items.len() != 1 {
        return Err(ApiError::BadRequest("Rename exactly one item".to_string()));
    }
    let Some(name) = payload.name.as_deref() else {
        return Err(ApiError::BadRequest("Name is required".to_string()));
    };
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required".to_string()));
    }
    let meta = client_meta(&headers);
    let item = items.into_iter().next().expect("single item");
    let mut response = BulkActionResponse::default();
    match &item {
        NormalizedActionItem::Document { id } => {
            match rename_document(
                &state.db,
                *id,
                payload.destination_folder.as_deref(),
                name,
                &user,
                &meta,
            )
            .await
            {
                Ok(detail) => {
                    response.ok.push(action_result(&item, Some(detail)));
                    record_document_batch_state(&state.db, "rename").await?;
                    notify_state_event_committed();
                }
                Err(error) => {
                    let detail = document_action_error_detail(error)?;
                    response.failed.push(action_result(&item, Some(detail)));
                }
            }
        }
        NormalizedActionItem::Folder { id, .. } => {
            match rename_folder(
                &state.db,
                *id,
                payload.destination_folder.as_deref(),
                name,
                &user,
            )
            .await
            {
                Ok(result) => {
                    response.ok.push(action_result(&item, Some(result.path)));
                    record_document_batch_state(&state.db, "rename").await?;
                    notify_state_event_committed();
                }
                Err(error) => {
                    let detail = folder_action_error_detail(error)?;
                    response.failed.push(action_result(&item, Some(detail)));
                }
            }
        }
        NormalizedActionItem::MissingFolder { .. } => {
            response.failed.push(missing_folder_action_result(&item));
        }
    }
    Ok(Json(response))
}

async fn normalize_action_items(
    pool: &DbPool,
    payload: &ActionPayloadRequest,
) -> Result<Vec<NormalizedActionItem>, ApiError> {
    normalize_action_items_with_options(pool, payload, false).await
}

async fn normalize_bulk_action_items(
    pool: &DbPool,
    payload: &ActionPayloadRequest,
) -> Result<Vec<NormalizedActionItem>, ApiError> {
    // Mutating bulk endpoints mirror the Python action contract: a stale folder
    // path is an item-level failure, not a top-level 404 that aborts the whole batch.
    normalize_action_items_with_options(pool, payload, true).await
}

async fn normalize_action_items_with_options(
    pool: &DbPool,
    payload: &ActionPayloadRequest,
    allow_missing_folders: bool,
) -> Result<Vec<NormalizedActionItem>, ApiError> {
    if payload.items.is_empty() {
        return Err(ApiError::BadRequest("Select at least one item".to_string()));
    }
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for item in &payload.items {
        let item_type = item.item_type.trim().to_ascii_lowercase();
        match item_type.as_str() {
            "document" => {
                let id = item
                    .id
                    .ok_or_else(|| ApiError::BadRequest("Document id is required".to_string()))?;
                if try_fetch_document_by_id(pool, id).await?.is_none() {
                    return Err(ApiError::Document(DocumentError::DocumentNotFound));
                }
                if seen.insert(format!("document:{id}")) {
                    normalized.push(NormalizedActionItem::Document { id });
                }
            }
            "folder" => {
                let normalized_item =
                    normalize_folder_action_item(pool, item, allow_missing_folders).await?;
                if seen.insert(action_item_dedupe_key(&normalized_item)) {
                    normalized.push(normalized_item);
                }
            }
            _ => return Err(ApiError::BadRequest("Invalid item type".to_string())),
        }
    }
    // The Python service prunes explicit descendants when a parent folder is
    // selected. Keep that contract in the shared normalizer so every bulk
    // action, download, and export path sees the same deduplicated selection.
    prune_nested_action_items(pool, normalized).await
}

async fn prune_nested_action_items(
    pool: &DbPool,
    items: Vec<NormalizedActionItem>,
) -> Result<Vec<NormalizedActionItem>, ApiError> {
    let folder_paths = items
        .iter()
        .filter_map(|item| match item {
            NormalizedActionItem::Folder { path, .. } => Some(path.clone()),
            NormalizedActionItem::Document { .. } | NormalizedActionItem::MissingFolder { .. } => {
                None
            }
        })
        .collect::<Vec<_>>();
    let mut pruned = Vec::new();
    for item in items {
        match &item {
            NormalizedActionItem::Folder { path, .. } => {
                if folder_paths
                    .iter()
                    .any(|parent| path != parent && path.starts_with(&format!("{parent}/")))
                {
                    continue;
                }
            }
            NormalizedActionItem::Document { id } => {
                let Some(document) = try_fetch_document_by_id(pool, *id).await? else {
                    pruned.push(item);
                    continue;
                };
                let folder_path = document_folder_path(pool, &document).await?;
                if folder_paths.iter().any(|parent| {
                    folder_path == *parent || folder_path.starts_with(&format!("{parent}/"))
                }) {
                    continue;
                }
            }
            NormalizedActionItem::MissingFolder { .. } => {}
        }
        pruned.push(item);
    }
    Ok(pruned)
}

async fn normalize_folder_action_item(
    pool: &DbPool,
    item: &ActionItemRequest,
    allow_missing: bool,
) -> Result<NormalizedActionItem, ApiError> {
    if let Some(id) = item.id {
        if id < 1 {
            return Err(ApiError::BadRequest(
                "Folder id must be positive".to_string(),
            ));
        }
        let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM folders WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?
            .is_some();
        if !exists {
            if allow_missing {
                return Ok(NormalizedActionItem::MissingFolder {
                    id: Some(id),
                    path: None,
                });
            }
            return Err(ApiError::NotFound("Folder not found".to_string()));
        }
        return Ok(NormalizedActionItem::Folder {
            id,
            path: folder_path_by_id(pool, id).await?,
        });
    }

    let path = crate::folders::normalize_folder(item.path.as_deref())?;
    if path.is_empty() {
        return Err(ApiError::BadRequest("Folder path is required".to_string()));
    }
    let Some(folder) = get_folder_by_path(pool, Some(&path)).await? else {
        if allow_missing {
            return Ok(NormalizedActionItem::MissingFolder {
                id: None,
                path: Some(path),
            });
        }
        return Err(ApiError::NotFound(format!("Folder not found: {path}")));
    };
    Ok(NormalizedActionItem::Folder {
        id: folder.id,
        path: folder_path_by_id(pool, folder.id).await?,
    })
}

fn action_item_dedupe_key(item: &NormalizedActionItem) -> String {
    match item {
        NormalizedActionItem::Document { id } => format!("document:{id}"),
        NormalizedActionItem::Folder { id, .. } => format!("folder:{id}"),
        NormalizedActionItem::MissingFolder { id: Some(id), .. } => {
            format!("missing-folder-id:{id}")
        }
        NormalizedActionItem::MissingFolder {
            id: None,
            path: Some(path),
        } => format!("missing-folder-path:{}", path.trim()),
        NormalizedActionItem::MissingFolder {
            id: None,
            path: None,
        } => "missing-folder".to_string(),
    }
}

fn action_result(item: &NormalizedActionItem, detail: Option<String>) -> ActionResultPayload {
    ActionResultPayload {
        item: action_item_payload(item),
        detail,
    }
}

fn action_item_payload(item: &NormalizedActionItem) -> ActionItemPayload {
    match item {
        NormalizedActionItem::Document { id } => ActionItemPayload {
            item_type: "document".to_string(),
            id: Some(*id),
            path: None,
        },
        NormalizedActionItem::Folder { id, path } => ActionItemPayload {
            item_type: "folder".to_string(),
            id: Some(*id),
            path: Some(path.clone()),
        },
        NormalizedActionItem::MissingFolder { id, path } => ActionItemPayload {
            item_type: "folder".to_string(),
            id: *id,
            path: path.clone(),
        },
    }
}

fn export_selection_items(items: &[NormalizedActionItem]) -> Vec<ExportSelectionItem> {
    items
        .iter()
        .filter_map(|item| match item {
            NormalizedActionItem::Document { id } => {
                Some(ExportSelectionItem::Document { id: *id })
            }
            NormalizedActionItem::Folder { id, path } => Some(ExportSelectionItem::Folder {
                id: *id,
                path: path.clone(),
            }),
            NormalizedActionItem::MissingFolder { .. } => None,
        })
        .collect()
}

fn missing_folder_action_result(item: &NormalizedActionItem) -> ActionResultPayload {
    action_result(item, Some("Folder not found".to_string()))
}

fn document_action_error_detail(error: DocumentError) -> Result<String, ApiError> {
    match error {
        DocumentError::DocumentNotFound => Ok("Document not found".to_string()),
        DocumentError::InsufficientDocumentAccess => Ok("Insufficient document access".to_string()),
        DocumentError::RestoreBeforeEditing => Ok("Restore this file before editing".to_string()),
        DocumentError::DocumentLockedByOtherUser => {
            Ok("Document is locked by another user".to_string())
        }
        DocumentError::DocumentNotLocked => Ok("Document is not locked".to_string()),
        DocumentError::MoveDocumentToArchiveBeforeDeleting => {
            Ok("Move the document to Archive before deleting".to_string())
        }
        DocumentError::FileNameRequired => Ok("File name is required".to_string()),
        DocumentError::InvalidFileName => Ok("Invalid file name".to_string()),
        DocumentError::DocumentPathAlreadyExists => {
            Ok("A document already exists at that path".to_string())
        }
        DocumentError::RestoreArchivedBeforeRenaming => {
            Ok("Restore archived files before renaming".to_string())
        }
        DocumentError::UseArchiveOrRestoreForArchiveMoves => {
            Ok("Use archive or restore for Archive moves".to_string())
        }
        DocumentError::DocumentAlreadyArchived => Ok("Document is already archived".to_string()),
        DocumentError::DocumentNotArchived => Ok("Document is not archived".to_string()),
        DocumentError::ArchivedDocumentMissingRestoreMetadata => {
            Ok("Archived document is missing restore metadata".to_string())
        }
        DocumentError::CannotArchiveRootFolder => Ok("Cannot archive a root folder".to_string()),
        DocumentError::FolderAlreadyArchived => Ok("Folder is already archived".to_string()),
        DocumentError::FolderHasNoFilesToArchive => {
            Ok("Folder has no files to archive".to_string())
        }
        DocumentError::DocumentHasNoVersions => Ok("Document has no versions".to_string()),
        DocumentError::InconsistentCurrentVersion => {
            Ok("Current document version metadata is inconsistent".to_string())
        }
        DocumentError::VersionNotFound => Ok("Version not found".to_string()),
        DocumentError::BlobHasNoStorageLocation => Ok("Blob has no storage location".to_string()),
        DocumentError::Folder(FolderError::InsufficientFolderAccess) => {
            Ok("Insufficient folder access".to_string())
        }
        DocumentError::Folder(FolderError::FolderNotFound) => Ok("Folder not found".to_string()),
        DocumentError::Folder(FolderError::InvalidPath) => Ok("Invalid folder path".to_string()),
        DocumentError::Folder(FolderError::ArchiveDoesNotContainFolders) => {
            Ok("Archive does not contain folders".to_string())
        }
        error => Err(ApiError::Document(error)),
    }
}

fn folder_action_error_detail(error: FolderError) -> Result<String, ApiError> {
    match error {
        FolderError::CannotMoveRootFolder => Ok("Cannot move a root folder".to_string()),
        FolderError::CannotMoveFolderIntoItself => {
            Ok("Cannot move a folder into itself".to_string())
        }
        FolderError::DocumentLockedByOtherUser => {
            Ok("Document is locked by another user".to_string())
        }
        FolderError::UseArchiveOrRestoreForArchiveMoves => {
            Ok("Use archive or restore for Archive moves".to_string())
        }
        FolderError::FolderNameRequired => Ok("Folder name is required".to_string()),
        FolderError::InvalidFolderName => Ok("Invalid folder name".to_string()),
        FolderError::TargetFolderAlreadyExists => {
            Ok("A folder already exists at that path".to_string())
        }
        FolderError::InvalidPath => Ok("Invalid folder path".to_string()),
        FolderError::ArchiveDoesNotContainFolders => {
            Ok("Archive does not contain folders".to_string())
        }
        FolderError::InsufficientFolderAccess => Ok("Insufficient folder access".to_string()),
        FolderError::FolderNotFound => Ok("Folder not found".to_string()),
        error => Err(ApiError::Folder(error)),
    }
}

fn client_meta(headers: &HeaderMap) -> ClientMeta {
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let user_agent = headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string);
    ClientMeta { ip, user_agent }
}

const STATE_EVENT_HEARTBEAT: Duration = Duration::from_secs(25);

struct EventStreamState {
    db: DbPool,
    last_id: i64,
    pending: VecDeque<StateEventRecord>,
    receiver: broadcast::Receiver<()>,
    debug_generation: i64,
    dev_mode: bool,
    close_after_retry: bool,
}

async fn event_stream_start_id(pool: &DbPool, headers: &HeaderMap) -> Result<i64, ApiError> {
    let Some(value) = header_value_by_name(headers, "last-event-id") else {
        return Ok(latest_state_event_id(pool).await?);
    };
    match value.trim().parse::<i64>() {
        Ok(last_id) if last_id >= 0 => Ok(last_id),
        _ => Ok(latest_state_event_id(pool).await?),
    }
}

fn state_event_stream(
    db: DbPool,
    last_id: i64,
    dev_mode: bool,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(
        EventStreamState {
            db,
            last_id,
            pending: VecDeque::new(),
            receiver: subscribe_state_events(),
            debug_generation: if dev_mode {
                DEBUG_EVENT_STREAM_GENERATION.load(Ordering::SeqCst)
            } else {
                0
            },
            dev_mode,
            close_after_retry: false,
        },
        |mut state| async move {
            if state.close_after_retry {
                return None;
            }
            loop {
                if state.dev_mode {
                    let current_generation = DEBUG_EVENT_STREAM_GENERATION.load(Ordering::SeqCst);
                    if current_generation != state.debug_generation {
                        state.debug_generation = current_generation;
                        let retry_ms = DEBUG_EVENT_STREAM_RETRY_MS.load(Ordering::SeqCst);
                        let retry_ms = u64::try_from(retry_ms).unwrap_or_default();
                        // The dev timeout tool tells existing browsers to reconnect with
                        // the new retry delay, matching the Python stream contract.
                        state.close_after_retry = true;
                        return Some((
                            Ok(Event::default().retry(Duration::from_millis(retry_ms))),
                            state,
                        ));
                    }
                }
                if let Some(event) = state.pending.pop_front() {
                    state.last_id = event.id;
                    return Some((Ok(state_sse_event(&event)), state));
                }
                match state_events_after(&state.db, state.last_id).await {
                    Ok(events) if !events.is_empty() => {
                        state.pending = events.into();
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => tracing::error!(?error, "state event stream query failed"),
                }
                // State streams are intentionally notification-driven. Without this
                // wait, ten idle browsers would create continuous SQLite polling load.
                match tokio::time::timeout(STATE_EVENT_HEARTBEAT, state.receiver.recv()).await {
                    Ok(Ok(()) | Err(broadcast::error::RecvError::Lagged(_))) => {}
                    Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                    Err(_) => {
                        return Some((Ok(Event::default().comment("heartbeat")), state));
                    }
                }
            }
        },
    )
}

fn state_sse_event(event: &StateEventRecord) -> Event {
    let data = serde_json::to_string(&event.payload)
        .unwrap_or_else(|_| "{\"type\":\"state.error\",\"resources\":[]}".to_string());
    Event::default()
        .id(event.id.to_string())
        .event("state")
        .data(data)
}

#[derive(Debug, Clone, Copy)]
struct DownloadRange {
    start: u64,
    end: u64,
    status: StatusCode,
    len: u64,
}

async fn version_download_response(
    storage: &dyn BlobStorageBackend,
    download: &VersionDownload,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let size = u64::try_from(download.size_bytes)
        .map_err(|_| ApiError::Storage(StorageError::ContentMismatch))?;
    let etag = blob_etag(download);
    let range = parse_range_header(
        header_value(headers, header::RANGE),
        header_value(headers, header::IF_RANGE),
        size,
        &etag,
    )?;
    // Download routes fail closed on corrupt canonical blobs. Validate the full object
    // before slicing ranges so a partial request cannot leak bytes that no longer match
    // the database metadata.
    let bytes = validated_download_bytes(storage, download, size).await?;
    let body = if range.len == 0 {
        Vec::new()
    } else {
        download_range_body(&bytes, &range, size)?
    };
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = range.status;
    let safe_name = safe_download_name(&download.filename);
    insert_header(
        response.headers_mut(),
        header::CONTENT_TYPE,
        &sanitize_mime_type(download.mime_type.as_deref(), &safe_name),
    );
    insert_header(
        response.headers_mut(),
        header::CONTENT_DISPOSITION,
        &download_content_disposition(&safe_name),
    );
    insert_header(
        response.headers_mut(),
        header::CONTENT_LENGTH,
        &range.len.to_string(),
    );
    insert_header(response.headers_mut(), header::ACCEPT_RANGES, "bytes");
    insert_header(response.headers_mut(), header::CONTENT_ENCODING, "identity");
    insert_header(response.headers_mut(), header::ETAG, &etag);
    if range.status == StatusCode::PARTIAL_CONTENT {
        insert_header(
            response.headers_mut(),
            header::CONTENT_RANGE,
            &format!("bytes {}-{}/{}", range.start, range.end, size),
        );
    }
    Ok(response)
}

async fn validated_download_bytes(
    storage: &dyn BlobStorageBackend,
    download: &VersionDownload,
    expected_size: u64,
) -> Result<Vec<u8>, ApiError> {
    let bytes = storage
        .read_location_bytes(&download.backend, &download.bucket, &download.object_key)
        .await
        .map_err(storage_download_error)?;
    let actual_size =
        u64::try_from(bytes.len()).map_err(|_| ApiError::Storage(StorageError::ContentMismatch))?;
    if actual_size != expected_size || !download.hash_algo.eq_ignore_ascii_case("sha256") {
        return Err(ApiError::Storage(StorageError::ContentMismatch));
    }
    let actual_hash = lower_hex(&Sha256::digest(&bytes));
    if actual_hash != download.hash.to_ascii_lowercase() {
        return Err(ApiError::Storage(StorageError::ContentMismatch));
    }
    Ok(bytes)
}

fn download_range_body(
    bytes: &[u8],
    range: &DownloadRange,
    size: u64,
) -> Result<Vec<u8>, ApiError> {
    let start = usize::try_from(range.start).map_err(|_| byte_range_error(size))?;
    let end_exclusive = range
        .end
        .checked_add(1)
        .ok_or_else(|| byte_range_error(size))?;
    let end = usize::try_from(end_exclusive).map_err(|_| byte_range_error(size))?;
    bytes
        .get(start..end)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| byte_range_error(size))
}

fn parse_range_header(
    range_header: Option<&str>,
    if_range: Option<&str>,
    size: u64,
    etag: &str,
) -> Result<DownloadRange, ApiError> {
    if size == 0 {
        return Ok(DownloadRange {
            start: 0,
            end: 0,
            status: StatusCode::OK,
            len: 0,
        });
    }
    let range_header = if if_range.is_some_and(|value| value.trim() != etag) {
        None
    } else {
        range_header
    };
    let Some(value) = range_header
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(full_download_range(size));
    };
    if !value.starts_with("bytes=") || value.contains(',') {
        return Err(byte_range_error(size));
    }
    let spec = value.trim_start_matches("bytes=").trim();
    let Some((raw_start, raw_end)) = spec.split_once('-') else {
        return Err(byte_range_error(size));
    };
    let (start, end) = if raw_start.is_empty() {
        let suffix = raw_end
            .parse::<u64>()
            .ok()
            .filter(|suffix| *suffix > 0)
            .ok_or_else(|| byte_range_error(size))?;
        (size.saturating_sub(suffix), size - 1)
    } else {
        let start = raw_start
            .parse::<u64>()
            .map_err(|_| byte_range_error(size))?;
        let end = if raw_end.is_empty() {
            size - 1
        } else {
            raw_end.parse::<u64>().map_err(|_| byte_range_error(size))?
        };
        (start, end.min(size - 1))
    };
    if end < start || start >= size {
        return Err(byte_range_error(size));
    }
    Ok(DownloadRange {
        start,
        end,
        status: StatusCode::PARTIAL_CONTENT,
        len: end - start + 1,
    })
}

fn full_download_range(size: u64) -> DownloadRange {
    DownloadRange {
        start: 0,
        end: size - 1,
        status: StatusCode::OK,
        len: size,
    }
}

fn byte_range_error(size: u64) -> ApiError {
    ApiError::RangeNotSatisfiable {
        content_range: format!("bytes */{size}"),
    }
}

fn storage_download_error(error: StorageError) -> ApiError {
    match error {
        StorageError::NotFound => ApiError::NotFound("Blob missing from storage".to_string()),
        StorageError::InvalidRange => byte_range_error(0),
        error => ApiError::Storage(error),
    }
}

fn header_value(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn header_value_by_name<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn required_i64_header(headers: &HeaderMap, name: &str) -> Result<i64, ApiError> {
    header_value_by_name(headers, name)
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| ApiError::BadRequest("Upload part range does not match session".to_string()))
}

fn insert_header(headers: &mut HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn insert_default_header(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if !headers.contains_key(&name)
        && let Ok(value) = HeaderValue::from_str(value)
    {
        headers.insert(name, value);
    }
}

fn apply_security_headers(
    auth: &AuthSettings,
    headers: &mut HeaderMap,
    nonce: &str,
    public_https: bool,
) {
    if !auth.security_headers.enabled {
        return;
    }
    insert_default_header(
        headers,
        HeaderName::from_static("x-content-type-options"),
        "nosniff",
    );
    insert_default_header(headers, HeaderName::from_static("x-frame-options"), "DENY");
    insert_default_header(
        headers,
        HeaderName::from_static("referrer-policy"),
        "no-referrer",
    );
    insert_default_header(
        headers,
        HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
    );
    insert_default_header(
        headers,
        HeaderName::from_static("content-security-policy"),
        &content_security_policy(auth, nonce),
    );
    if !auth.dev_mode && auth.security_headers.hsts_max_age_seconds > 0 && public_https {
        insert_default_header(
            headers,
            header::STRICT_TRANSPORT_SECURITY,
            &hsts_header_value(auth),
        );
    }
}

fn content_security_policy(auth: &AuthSettings, nonce: &str) -> String {
    let configured = auth.security_headers.content_security_policy.trim();
    if !configured.is_empty() {
        return configured.replace("{nonce}", nonce);
    }
    format!(
        "default-src 'self'; \
         base-uri 'self'; \
         object-src 'none'; \
         frame-ancestors 'none'; \
         form-action 'self'; \
         img-src 'self' data: blob:; \
         style-src 'self' 'unsafe-inline'; \
         script-src 'self' 'nonce-{nonce}'; \
         connect-src 'self'; \
         font-src 'self' data:"
    )
}

fn hsts_header_value(auth: &AuthSettings) -> String {
    let mut value = format!("max-age={}", auth.security_headers.hsts_max_age_seconds);
    if auth.security_headers.hsts_include_subdomains {
        value.push_str("; includeSubDomains");
    }
    if auth.security_headers.hsts_preload {
        value.push_str("; preload");
    }
    value
}

fn request_is_public_https(auth: &AuthSettings, headers: &HeaderMap, uri: &Uri) -> bool {
    uri.scheme_str() == Some("https")
        || auth.public_url.starts_with("https://")
        || forwarded_proto_is_https(headers)
}

fn blob_etag(download: &VersionDownload) -> String {
    format!(
        "\"{}-{}-{}\"",
        download.hash_algo, download.hash, download.size_bytes
    )
}

fn safe_download_name(filename: &str) -> String {
    let cleaned = filename
        .replace('"', "")
        .chars()
        .map(|character| {
            if character < ' ' || character == '\u{7f}' {
                '_'
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned
    }
}

fn sanitize_mime_type(mime_type: Option<&str>, filename: &str) -> String {
    let fallback = mime_from_filename(filename);
    let candidate = mime_type.unwrap_or(&fallback).trim();
    let sanitized = if candidate.is_empty()
        || candidate
            .chars()
            .any(|character| character < ' ' || character == '\u{7f}' || !character.is_ascii())
    {
        fallback
    } else {
        candidate.to_string()
    };
    let lower = sanitized.to_ascii_lowercase();
    if lower.starts_with("text/") && !lower.contains("charset=") {
        format!("{sanitized}; charset=utf-8")
    } else {
        sanitized
    }
}

fn mime_from_filename(filename: &str) -> String {
    match filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn download_content_disposition(filename: &str) -> String {
    let ascii_name = filename
        .chars()
        .map(|character| {
            if (' '..='\u{7e}').contains(&character) {
                character
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string();
    let ascii_name = if ascii_name.is_empty() {
        "download".to_string()
    } else {
        ascii_name
    };
    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_name,
        percent_encode_header_value(filename)
    )
}

fn percent_encode_header_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEADER_PERCENT_HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEADER_PERCENT_HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

async fn current_user(state: &AppState, headers: &HeaderMap) -> Result<UserContext, ApiError> {
    match state.auth.mode {
        AuthMode::Headers => Ok(header_identity(&state.auth, &state.db, headers).await?),
        AuthMode::Dev => dev_identity(&state.auth, &state.db)
            .await?
            .ok_or_else(|| ApiError::Unauthorized("Development auth is disabled".to_string())),
        AuthMode::Oidc => session_identity(
            &state.auth,
            &state.db,
            header_value(headers, header::COOKIE),
        )
        .await?
        .ok_or(AuthError::AuthenticationRequired.into()),
    }
}

async fn current_browser_user(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<UserContext, ApiError> {
    match current_user(state, headers).await {
        Ok(user) => Ok(user),
        Err(ApiError::Auth(AuthError::AuthenticationRequired))
            if state.auth.mode == AuthMode::Oidc =>
        {
            Err(ApiError::LoginRedirect(login_location_for_uri(uri)))
        }
        Err(error) => Err(error),
    }
}

fn login_location_for_uri(uri: &Uri) -> String {
    let rd = uri.path_and_query().map_or("/", |value| value.as_str());
    format!("/login?rd={}", percent_encode_return_path(rd))
}

fn oidc_redirect_uri(auth: &AuthSettings, headers: &HeaderMap, uri: &Uri) -> String {
    let configured = auth.oidc_redirect_uri.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    let public_url = auth.public_url.trim().trim_end_matches('/');
    if !public_url.is_empty() {
        return format!("{public_url}/auth/callback");
    }
    let host = external_request_host(headers);
    let scheme = if forwarded_proto_is_https(headers) {
        "https"
    } else {
        uri.scheme_str().unwrap_or("http")
    };
    format!("{scheme}://{host}/auth/callback")
}

fn external_request_host(headers: &HeaderMap) -> &str {
    header_value_by_name(headers, "x-forwarded-host")
        .and_then(first_forwarded_value)
        .or_else(|| header_value(headers, header::HOST).and_then(first_forwarded_value))
        .unwrap_or("localhost")
}

fn first_forwarded_value(value: &str) -> Option<&str> {
    value
        .split(',')
        .map(str::trim)
        .find(|item| !item.is_empty())
}

fn forwarded_proto_is_https(headers: &HeaderMap) -> bool {
    header_value_by_name(headers, "x-forwarded-proto")
        .and_then(first_forwarded_value)
        .is_some_and(|value| value.eq_ignore_ascii_case("https"))
}

fn cookie_secure(auth: &AuthSettings, headers: &HeaderMap) -> bool {
    match auth
        .session_cookie_secure
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => auth.public_url.starts_with("https://") || forwarded_proto_is_https(headers),
    }
}

fn safe_redirect(value: Option<&str>) -> String {
    value
        .filter(|item| item.starts_with('/') && !item.starts_with("//"))
        .unwrap_or("/")
        .to_string()
}

fn redirect_response(location: &str, set_cookies: Vec<String>) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    insert_header(response.headers_mut(), header::LOCATION, location);
    for cookie in set_cookies {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

fn set_cookie_header(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    let mut cookie = format!(
        "{}={}; Max-Age={}; Path=/; HttpOnly; SameSite=Lax",
        safe_cookie_name(name),
        value,
        max_age
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn delete_cookie_header(name: &str) -> String {
    format!(
        "{}=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax",
        safe_cookie_name(name)
    )
}

fn safe_cookie_name(name: &str) -> String {
    name.chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
        .collect::<String>()
}

fn form_urlencode(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                percent_encode_query_value(key),
                percent_encode_query_value(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEADER_PERCENT_HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEADER_PERCENT_HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn percent_encode_return_path(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEADER_PERCENT_HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEADER_PERCENT_HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn unix_timestamp_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn unix_timestamp_now_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

fn require_admin(user: &UserContext) -> Result<(), ApiError> {
    if user.is_admin {
        Ok(())
    } else {
        Err(ApiError::AdminRequired)
    }
}

async fn require_dev_admin(state: &AppState, headers: &HeaderMap) -> Result<UserContext, ApiError> {
    let user = current_user(state, headers).await?;
    require_admin(&user)?;
    if state.auth.dev_mode {
        Ok(user)
    } else {
        Err(ApiError::NotFound(
            "Debug tools are not available".to_string(),
        ))
    }
}

fn debug_action_result(mut payload: Value) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.insert("dev_mode".to_string(), json!(true));
        object.insert("ok".to_string(), json!(true));
    }
    payload
}

fn debug_allowed_resources(resources: &[String]) -> Vec<String> {
    let allowed = [
        "admin",
        "contents",
        "document_detail",
        "my_edits",
        "preferences",
        "settings",
        "sidebar",
    ];
    resources
        .iter()
        .map(|resource| resource.trim())
        .filter(|resource| allowed.contains(resource))
        .map(ToString::to_string)
        .collect()
}

async fn create_debug_document(
    state: &AppState,
    folder_id: i64,
    name: &str,
    stored: &StoredBlob,
    user: &UserContext,
) -> Result<i64, ApiError> {
    let mut transaction = state.db.begin().await?;
    let blob_id = get_or_create_debug_blob(&mut transaction, stored).await?;
    let document_id = insert_debug_document_row(&mut transaction, folder_id, name, user).await?;
    insert_debug_document_version_and_event(&mut transaction, document_id, blob_id, name, user)
        .await?;
    transaction.commit().await?;
    Ok(document_id)
}

async fn get_or_create_debug_blob(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    stored: &StoredBlob,
) -> Result<i64, ApiError> {
    let size_bytes = i64::try_from(stored.size_bytes)
        .map_err(|_| ApiError::Internal("Debug sample is too large".to_string()))?;
    sqlx::query(
        r"
        INSERT OR IGNORE INTO blobs (hash_algo, hash, size_bytes)
        VALUES (?, ?, ?)
        ",
    )
    .bind(&stored.hash_algo)
    .bind(&stored.digest)
    .bind(size_bytes)
    .execute(&mut **transaction)
    .await?;
    let blob_id = sqlx::query_scalar::<_, i64>(
        r"
        SELECT id
        FROM blobs
        WHERE hash_algo = ? AND hash = ? AND size_bytes = ?
        ",
    )
    .bind(&stored.hash_algo)
    .bind(&stored.digest)
    .bind(size_bytes)
    .fetch_one(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        INSERT OR IGNORE INTO blob_locations (blob_id, backend, bucket, object_key)
        VALUES (?, ?, ?, ?)
        ",
    )
    .bind(blob_id)
    .bind(&stored.backend)
    .bind(&stored.bucket)
    .bind(&stored.object_key)
    .execute(&mut **transaction)
    .await?;
    Ok(blob_id)
}

async fn insert_debug_document_row(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    folder_id: i64,
    name: &str,
    user: &UserContext,
) -> Result<i64, ApiError> {
    Ok(sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, ?, ?, ?, ?)
        ",
    )
    .bind(folder_id)
    .bind(name)
    .bind(&user.id)
    .bind(&user.name)
    .bind(&user.id)
    .execute(&mut **transaction)
    .await?
    .last_insert_rowid())
}

async fn insert_debug_document_version_and_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    document_id: i64,
    blob_id: i64,
    name: &str,
    user: &UserContext,
) -> Result<(), ApiError> {
    let version_id = Uuid::new_v4().to_string();
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
            (?, ?, ?, 1, ?, ?, 'Debug seed', 'text/plain', ?, 'upload')
        ",
    )
    .bind(&version_id)
    .bind(document_id)
    .bind(blob_id)
    .bind(&user.id)
    .bind(&user.name)
    .bind(name)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        UPDATE documents
        SET current_version_id = ?,
            latest_version_number = 1,
            version_count = 1
        WHERE id = ?
        ",
    )
    .bind(&version_id)
    .bind(document_id)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        INSERT INTO document_events
            (document_id, event_type, actor, actor_name, message, result)
        VALUES
            (?, 'upload', ?, ?, 'Debug seed', 'ok')
        ",
    )
    .bind(document_id)
    .bind(&user.id)
    .bind(&user.name)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

#[derive(Debug)]
enum ApiError {
    Admin(AdminError),
    AdminRequired,
    Asset(AssetError),
    Auth(AuthError),
    BadRequest(String),
    Database(sqlx::Error),
    Document(DocumentError),
    Export(ExportError),
    Forbidden(String),
    Folder(FolderError),
    Internal(String),
    LoginRedirect(String),
    NotFound(String),
    Oidc(OidcError),
    Preference(PreferenceError),
    Reconciliation(ReconciliationError),
    RangeNotSatisfiable { content_range: String },
    Settings(SiteSettingsError),
    ServiceUnavailable(String),
    Share(ShareError),
    Storage(StorageError),
    StateEvent(StateEventError),
    TransferMaintenance(TransferMaintenanceError),
    Unauthorized(String),
    Upload(UploadError),
    View(ViewError),
}

#[derive(Debug, Serialize)]
struct ErrorPayload {
    detail: String,
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        Self::Auth(error)
    }
}

impl From<AssetError> for ApiError {
    fn from(error: AssetError) -> Self {
        Self::Asset(error)
    }
}

impl From<AdminError> for ApiError {
    fn from(error: AdminError) -> Self {
        Self::Admin(error)
    }
}

impl From<PreferenceError> for ApiError {
    fn from(error: PreferenceError) -> Self {
        Self::Preference(error)
    }
}

impl From<FolderError> for ApiError {
    fn from(error: FolderError) -> Self {
        Self::Folder(error)
    }
}

impl From<DocumentError> for ApiError {
    fn from(error: DocumentError) -> Self {
        Self::Document(error)
    }
}

impl From<ExportError> for ApiError {
    fn from(error: ExportError) -> Self {
        Self::Export(error)
    }
}

impl From<ReconciliationError> for ApiError {
    fn from(error: ReconciliationError) -> Self {
        Self::Reconciliation(error)
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SiteSettingsError> for ApiError {
    fn from(error: SiteSettingsError) -> Self {
        Self::Settings(error)
    }
}

impl From<ShareError> for ApiError {
    fn from(error: ShareError) -> Self {
        Self::Share(error)
    }
}

impl From<StorageError> for ApiError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<StateEventError> for ApiError {
    fn from(error: StateEventError) -> Self {
        Self::StateEvent(error)
    }
}

impl From<TransferMaintenanceError> for ApiError {
    fn from(error: TransferMaintenanceError) -> Self {
        Self::TransferMaintenance(error)
    }
}

impl From<ViewError> for ApiError {
    fn from(error: ViewError) -> Self {
        Self::View(error)
    }
}

impl From<UploadError> for ApiError {
    fn from(error: UploadError) -> Self {
        Self::Upload(error)
    }
}

impl From<OidcError> for ApiError {
    fn from(error: OidcError) -> Self {
        Self::Oidc(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::LoginRedirect(location) => redirect_response(&location, Vec::new()),
            Self::RangeNotSatisfiable { content_range } => {
                let mut response = (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    Json(ErrorPayload {
                        detail: "Invalid byte range".to_string(),
                    }),
                )
                    .into_response();
                insert_header(
                    response.headers_mut(),
                    header::CONTENT_RANGE,
                    &content_range,
                );
                response
            }
            error => {
                let (status, detail) = api_error_status_detail(error);
                (status, Json(ErrorPayload { detail })).into_response()
            }
        }
    }
}

fn api_error_status_detail(error: ApiError) -> (StatusCode, String) {
    match error {
        ApiError::Admin(error) => admin_error_response(error),
        ApiError::AdminRequired => (StatusCode::FORBIDDEN, "Admin access required".to_string()),
        ApiError::Asset(error) => asset_error_response(error),
        ApiError::Auth(error) => auth_error_response(error),
        ApiError::BadRequest(detail) => (StatusCode::BAD_REQUEST, detail),
        ApiError::Forbidden(detail) => (StatusCode::FORBIDDEN, detail),
        ApiError::Internal(detail) => (StatusCode::INTERNAL_SERVER_ERROR, detail),
        ApiError::NotFound(detail) => (StatusCode::NOT_FOUND, detail),
        ApiError::Oidc(error) => oidc_error_response(error),
        ApiError::Unauthorized(detail) => (StatusCode::UNAUTHORIZED, detail),
        ApiError::Document(error) => document_error_response(error),
        ApiError::Export(error) => export_error_response(error),
        ApiError::Folder(error) | ApiError::View(ViewError::Folder(error)) => {
            folder_error_response(error)
        }
        ApiError::Preference(error) | ApiError::View(ViewError::Preferences(error)) => {
            preference_error_response(error)
        }
        ApiError::Reconciliation(error) => reconciliation_error_response(error),
        ApiError::Settings(error) => site_settings_error_response(error),
        ApiError::ServiceUnavailable(detail) => (StatusCode::SERVICE_UNAVAILABLE, detail),
        ApiError::Share(error) => share_error_response(error),
        ApiError::StateEvent(error) => state_event_error_response(&error),
        ApiError::TransferMaintenance(error) => transfer_maintenance_error_response(error),
        ApiError::Database(error) => {
            tracing::error!(?error, "database request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
        ApiError::Storage(error) => storage_error_response(error),
        ApiError::Upload(error) => upload_error_response(error),
        ApiError::View(error) => view_error_response(error),
        ApiError::LoginRedirect(_) | ApiError::RangeNotSatisfiable { .. } => {
            unreachable!("handled before status mapping")
        }
    }
}

fn oidc_error_response(error: OidcError) -> (StatusCode, String) {
    match error {
        OidcError::StateValidationFailed
        | OidcError::MissingIdToken
        | OidcError::InvalidIdToken
        | OidcError::UserinfoSubjectMismatch => (StatusCode::UNAUTHORIZED, error.to_string()),
        OidcError::NotConfigured
        | OidcError::MissingAuthorizationEndpoint
        | OidcError::MissingTokenEndpoint
        | OidcError::MissingJwksEndpoint => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        OidcError::InsecureEndpoint { .. }
        | OidcError::ProviderUrlInvalid
        | OidcError::ProviderRequest { .. }
        | OidcError::ProviderJson => (StatusCode::BAD_GATEWAY, error.to_string()),
        OidcError::Auth(error) => auth_error_response(error),
    }
}

fn state_event_error_response(error: &StateEventError) -> (StatusCode, String) {
    tracing::error!(?error, "state event request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal server error".to_string(),
    )
}

fn asset_error_response(error: AssetError) -> (StatusCode, String) {
    match error {
        AssetError::InvalidStaticPath | AssetError::StaticAssetNotFound => {
            (StatusCode::NOT_FOUND, "Static asset not found".to_string())
        }
        error => {
            tracing::error!(?error, "static asset request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Static assets are unavailable".to_string(),
            )
        }
    }
}

fn share_error_response(error: ShareError) -> (StatusCode, String) {
    match error {
        ShareError::InvalidShareTarget => {
            (StatusCode::BAD_REQUEST, "Invalid share target".to_string())
        }
        ShareError::DocumentIdRequired => (
            StatusCode::BAD_REQUEST,
            "Document id is required".to_string(),
        ),
        ShareError::ShareLinkNotFound => {
            (StatusCode::NOT_FOUND, "Share link not found".to_string())
        }
        ShareError::ShareLinkExpired => (StatusCode::NOT_FOUND, "Share link expired".to_string()),
        ShareError::CouldNotCreateShareLink => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not create share link".to_string(),
        ),
        ShareError::Document(error) => document_error_response(error),
        ShareError::Folder(error) => folder_error_response(error),
        ShareError::View(error) => view_error_response(error),
        error @ ShareError::Database(_) => {
            tracing::error!(?error, "share link request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn upload_error_response(error: UploadError) -> (StatusCode, String) {
    match error {
        UploadError::UploadSessionNotFound => (
            StatusCode::NOT_FOUND,
            "Upload session not found".to_string(),
        ),
        UploadError::TransferNotFound => (StatusCode::NOT_FOUND, "Transfer not found".to_string()),
        UploadError::UploadSessionStatus(status) => {
            (StatusCode::CONFLICT, format!("Upload session is {status}"))
        }
        UploadError::UploadSessionExpired => {
            (StatusCode::GONE, "Upload session expired".to_string())
        }
        UploadError::CompletedSessionMissingResult => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Completed upload session is missing result".to_string(),
        ),
        UploadError::UnsupportedUploadSessionMode => (
            StatusCode::BAD_REQUEST,
            "Unsupported upload session mode".to_string(),
        ),
        UploadError::UploadSizeNegative => (
            StatusCode::BAD_REQUEST,
            "Upload size must be non-negative".to_string(),
        ),
        UploadError::UploadTooLarge(limit) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("Upload exceeds limit of {limit} bytes"),
        ),
        UploadError::UploadNewDocumentsToVault => (
            StatusCode::BAD_REQUEST,
            "Upload new documents to Vault".to_string(),
        ),
        UploadError::CheckOutBeforeUploading => (
            StatusCode::FORBIDDEN,
            "Check out the file before uploading a new version".to_string(),
        ),
        UploadError::InvalidPartNumber => {
            (StatusCode::BAD_REQUEST, "Invalid part number".to_string())
        }
        UploadError::UploadPartRangeMismatch => (
            StatusCode::BAD_REQUEST,
            "Upload part range does not match session".to_string(),
        ),
        UploadError::UploadPartTooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Upload part is too large".to_string(),
        ),
        UploadError::UploadPartSizeMismatch => (
            StatusCode::BAD_REQUEST,
            "Upload part size does not match session".to_string(),
        ),
        UploadError::UploadPartChecksumMismatch => (
            StatusCode::BAD_REQUEST,
            "Upload part checksum does not match".to_string(),
        ),
        UploadError::UploadPartConflict => (
            StatusCode::CONFLICT,
            "Upload part already exists with different content".to_string(),
        ),
        UploadError::UploadSessionMissingParts => (
            StatusCode::BAD_REQUEST,
            "Upload session has missing parts".to_string(),
        ),
        UploadError::UploadReadFailed => (
            StatusCode::BAD_REQUEST,
            "Upload failed while reading request body".to_string(),
        ),
        UploadError::UploadChecksumMismatch => (
            StatusCode::BAD_REQUEST,
            "Upload checksum mismatch".to_string(),
        ),
        UploadError::UploadSizeMismatch => (
            StatusCode::BAD_REQUEST,
            "Upload size does not match session".to_string(),
        ),
        UploadError::StorageLocationConflict => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Storage location points at another blob".to_string(),
        ),
        error @ (UploadError::UploadTokenRequired
        | UploadError::UploadTokenInvalid
        | UploadError::UploadTokenWrongSession
        | UploadError::UploadTokenExpired) => upload_token_error_response(&error),
        UploadError::Document(error) => document_error_response(error),
        UploadError::Folder(error) => folder_error_response(error),
        UploadError::Storage(error) => storage_error_response(error),
        error @ (UploadError::Database(_)
        | UploadError::Io(_)
        | UploadError::Json(_)
        | UploadError::TimeFormat(_)
        | UploadError::TimeParse(_)) => {
            tracing::error!(?error, "upload request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn upload_token_error_response(error: &UploadError) -> (StatusCode, String) {
    let detail = match error {
        UploadError::UploadTokenRequired => "Upload token is required",
        UploadError::UploadTokenInvalid => "Upload token is invalid",
        UploadError::UploadTokenWrongSession => "Upload token is not valid for this session",
        UploadError::UploadTokenExpired => "Upload token expired",
        _ => unreachable!("only upload token errors are mapped here"),
    };
    (StatusCode::UNAUTHORIZED, detail.to_string())
}

fn admin_error_response(error: AdminError) -> (StatusCode, String) {
    match error {
        AdminError::Settings(error) => site_settings_error_response(error),
        AdminError::UserNotFound => (StatusCode::NOT_FOUND, "User not found".to_string()),
        AdminError::GroupNotFound => (StatusCode::NOT_FOUND, "Group not found".to_string()),
        AdminError::GroupOrUserNotFound => {
            (StatusCode::NOT_FOUND, "Group or user not found".to_string())
        }
        AdminError::MembershipNotFound => {
            (StatusCode::NOT_FOUND, "Membership not found".to_string())
        }
        AdminError::GroupAlreadyExists => {
            (StatusCode::CONFLICT, "Group already exists".to_string())
        }
        AdminError::GroupNameRequired => (
            StatusCode::BAD_REQUEST,
            "Group name is required".to_string(),
        ),
        AdminError::InvalidGroupName => (StatusCode::BAD_REQUEST, "Invalid group name".to_string()),
        AdminError::GroupUsedByFolderPermissions => (
            StatusCode::BAD_REQUEST,
            "Group is used by folder permissions".to_string(),
        ),
        AdminError::LastActiveAdminRequired => (
            StatusCode::BAD_REQUEST,
            "At least one active admin is required".to_string(),
        ),
        error @ AdminError::Database(_) => {
            tracing::error!(?error, "admin request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn auth_error_response(error: AuthError) -> (StatusCode, String) {
    match error {
        AuthError::AuthenticationRequired => (
            StatusCode::UNAUTHORIZED,
            "Authentication required".to_string(),
        ),
        AuthError::UserDisabled => (StatusCode::FORBIDDEN, "User is disabled".to_string()),
        error => {
            tracing::warn!(?error, "authentication failed");
            (
                StatusCode::UNAUTHORIZED,
                "Authentication required".to_string(),
            )
        }
    }
}

fn storage_error_response(error: StorageError) -> (StatusCode, String) {
    match error {
        StorageError::NotFound => (
            StatusCode::NOT_FOUND,
            "Blob missing from storage".to_string(),
        ),
        StorageError::ContentMismatch => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Blob content does not match metadata".to_string(),
        ),
        error => {
            tracing::error!(?error, "storage request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Storage read failed".to_string(),
            )
        }
    }
}

fn view_error_response(error: ViewError) -> (StatusCode, String) {
    match error {
        ViewError::FolderNotFound => (StatusCode::NOT_FOUND, "Folder not found".to_string()),
        ViewError::DocumentNotFound => (StatusCode::NOT_FOUND, "Document not found".to_string()),
        ViewError::InsufficientDocumentAccess => (
            StatusCode::FORBIDDEN,
            "Insufficient document access".to_string(),
        ),
        ViewError::InconsistentDocumentVersion => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Current document version metadata is inconsistent".to_string(),
        ),
        error => {
            tracing::error!(?error, "request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn site_settings_error_response(error: SiteSettingsError) -> (StatusCode, String) {
    match error {
        SiteSettingsError::InvalidPatch(detail) => (StatusCode::BAD_REQUEST, detail),
        error => {
            tracing::error!(?error, "settings request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn reconciliation_error_response(error: ReconciliationError) -> (StatusCode, String) {
    match error {
        ReconciliationError::Storage(error) => storage_error_response(error),
        ReconciliationError::Database(error) => {
            tracing::error!(?error, "storage reconciliation request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn transfer_maintenance_error_response(error: TransferMaintenanceError) -> (StatusCode, String) {
    match error {
        TransferMaintenanceError::Storage(error) => storage_error_response(error),
        error => {
            tracing::error!(?error, "transfer maintenance request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn folder_error_response(error: FolderError) -> (StatusCode, String) {
    match error {
        FolderError::FolderPathRequired => (
            StatusCode::BAD_REQUEST,
            "Folder path is required".to_string(),
        ),
        FolderError::CreateFoldersInVault => (
            StatusCode::BAD_REQUEST,
            "Create folders in Vault".to_string(),
        ),
        FolderError::FolderAlreadyExists => {
            (StatusCode::BAD_REQUEST, "Folder already exists".to_string())
        }
        FolderError::TargetFolderAlreadyExists => (
            StatusCode::BAD_REQUEST,
            "A folder already exists at that path".to_string(),
        ),
        FolderError::FolderNameRequired => (
            StatusCode::BAD_REQUEST,
            "Folder name is required".to_string(),
        ),
        FolderError::InvalidFolderName => {
            (StatusCode::BAD_REQUEST, "Invalid folder name".to_string())
        }
        FolderError::CannotMoveRootFolder => (
            StatusCode::BAD_REQUEST,
            "Cannot move a root folder".to_string(),
        ),
        FolderError::CannotMoveFolderIntoItself => (
            StatusCode::BAD_REQUEST,
            "Cannot move a folder into itself".to_string(),
        ),
        FolderError::DocumentLockedByOtherUser => (
            StatusCode::FORBIDDEN,
            "Document is locked by another user".to_string(),
        ),
        FolderError::UseArchiveOrRestoreForArchiveMoves => (
            StatusCode::BAD_REQUEST,
            "Use archive or restore for Archive moves".to_string(),
        ),
        FolderError::InvalidPath => (StatusCode::BAD_REQUEST, "Invalid folder path".to_string()),
        FolderError::InvalidFolderColor => {
            (StatusCode::BAD_REQUEST, "Invalid folder color".to_string())
        }
        FolderError::InvalidFolderIcon => {
            (StatusCode::BAD_REQUEST, "Invalid folder icon".to_string())
        }
        FolderError::DuplicateGroupPermission => (
            StatusCode::BAD_REQUEST,
            "Duplicate group permission".to_string(),
        ),
        FolderError::GroupNotFound => (StatusCode::NOT_FOUND, "Group not found".to_string()),
        FolderError::InvalidTtlAction => {
            (StatusCode::BAD_REQUEST, "Invalid TTL action".to_string())
        }
        FolderError::TtlDaysRequired => {
            (StatusCode::BAD_REQUEST, "TTL days are required".to_string())
        }
        FolderError::TtlDaysOutOfRange => (
            StatusCode::BAD_REQUEST,
            "TTL days must be between 1 and 3650".to_string(),
        ),
        FolderError::DeleteTtlAdminRequired => (
            StatusCode::FORBIDDEN,
            "Admin access required for delete TTL".to_string(),
        ),
        FolderError::WriteRequiresReadAndView => (
            StatusCode::BAD_REQUEST,
            "Write permission requires read and view permission".to_string(),
        ),
        FolderError::ReadRequiresView => (
            StatusCode::BAD_REQUEST,
            "Read permission requires view permission".to_string(),
        ),
        FolderError::ArchiveDoesNotContainFolders => (
            StatusCode::BAD_REQUEST,
            "Archive does not contain folders".to_string(),
        ),
        FolderError::InsufficientFolderAccess => (
            StatusCode::FORBIDDEN,
            "Insufficient folder access".to_string(),
        ),
        FolderError::FolderNotFound => (StatusCode::NOT_FOUND, "Folder not found".to_string()),
        error => {
            tracing::error!(?error, "folder request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn document_error_response(error: DocumentError) -> (StatusCode, String) {
    match error {
        DocumentError::DocumentNotFound => {
            (StatusCode::NOT_FOUND, "Document not found".to_string())
        }
        DocumentError::InsufficientDocumentAccess => (
            StatusCode::FORBIDDEN,
            "Insufficient document access".to_string(),
        ),
        DocumentError::RestoreBeforeEditing => (
            StatusCode::BAD_REQUEST,
            "Restore this file before editing".to_string(),
        ),
        DocumentError::DocumentLockedByOtherUser => (
            StatusCode::FORBIDDEN,
            "Document is locked by another user".to_string(),
        ),
        DocumentError::DocumentNotLocked => (
            StatusCode::BAD_REQUEST,
            "Document is not locked".to_string(),
        ),
        DocumentError::MoveDocumentToArchiveBeforeDeleting => (
            StatusCode::BAD_REQUEST,
            "Move the document to Archive before deleting".to_string(),
        ),
        DocumentError::FileNameRequired => {
            (StatusCode::BAD_REQUEST, "File name is required".to_string())
        }
        DocumentError::InvalidFileName => {
            (StatusCode::BAD_REQUEST, "Invalid file name".to_string())
        }
        DocumentError::DocumentPathAlreadyExists => (
            StatusCode::BAD_REQUEST,
            "A document already exists at that path".to_string(),
        ),
        DocumentError::RestoreArchivedBeforeRenaming => (
            StatusCode::BAD_REQUEST,
            "Restore archived files before renaming".to_string(),
        ),
        DocumentError::UseArchiveOrRestoreForArchiveMoves => (
            StatusCode::BAD_REQUEST,
            "Use archive or restore for Archive moves".to_string(),
        ),
        DocumentError::DocumentAlreadyArchived => (
            StatusCode::BAD_REQUEST,
            "Document is already archived".to_string(),
        ),
        DocumentError::DocumentNotArchived => (
            StatusCode::BAD_REQUEST,
            "Document is not archived".to_string(),
        ),
        DocumentError::ArchivedDocumentMissingRestoreMetadata => (
            StatusCode::BAD_REQUEST,
            "Archived document is missing restore metadata".to_string(),
        ),
        DocumentError::CannotArchiveRootFolder => (
            StatusCode::BAD_REQUEST,
            "Cannot archive a root folder".to_string(),
        ),
        DocumentError::FolderAlreadyArchived => (
            StatusCode::BAD_REQUEST,
            "Folder is already archived".to_string(),
        ),
        DocumentError::FolderHasNoFilesToArchive => (
            StatusCode::BAD_REQUEST,
            "Folder has no files to archive".to_string(),
        ),
        DocumentError::DocumentHasNoVersions => (
            StatusCode::NOT_FOUND,
            "Document has no versions".to_string(),
        ),
        DocumentError::InconsistentCurrentVersion => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Current document version metadata is inconsistent".to_string(),
        ),
        DocumentError::VersionNotFound => (StatusCode::NOT_FOUND, "Version not found".to_string()),
        DocumentError::BlobHasNoStorageLocation => (
            StatusCode::NOT_FOUND,
            "Blob has no storage location".to_string(),
        ),
        DocumentError::Folder(error) => folder_error_response(error),
        error => {
            tracing::error!(?error, "document request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn export_error_response(error: ExportError) -> (StatusCode, String) {
    match error {
        ExportError::ExportNotFound | ExportError::TransferNotFound => {
            (StatusCode::NOT_FOUND, error.to_string())
        }
        ExportError::ExportHasNoDownloadableFiles => (StatusCode::BAD_REQUEST, error.to_string()),
        ExportError::InsufficientFolderAccess => (
            StatusCode::FORBIDDEN,
            "Insufficient folder access".to_string(),
        ),
        ExportError::ExportExpired => (StatusCode::GONE, "Export expired".to_string()),
        ExportError::ExportNotComplete => {
            (StatusCode::CONFLICT, "Export is not complete".to_string())
        }
        ExportError::ArtifactMissingStorageLocation => (
            StatusCode::NOT_FOUND,
            "Blob has no storage location".to_string(),
        ),
        ExportError::BlobContentMismatch => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Blob content does not match metadata".to_string(),
        ),
        ExportError::ZipLimitExceeded => (
            StatusCode::BAD_REQUEST,
            "Export is too large for the current ZIP writer".to_string(),
        ),
        ExportError::Document(error) => document_error_response(error),
        ExportError::Folder(error) => folder_error_response(error),
        ExportError::Storage(error) => storage_error_response(error),
        error => {
            tracing::error!(?error, "export request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn preference_error_response(error: PreferenceError) -> (StatusCode, String) {
    match error {
        PreferenceError::InvalidPatch(detail) => (StatusCode::BAD_REQUEST, detail),
        PreferenceError::UserPreferencesRequireVaultUser => (
            StatusCode::BAD_REQUEST,
            "User preferences require a vault user".to_string(),
        ),
        PreferenceError::VaultUserNotFound => {
            (StatusCode::NOT_FOUND, "Vault user not found".to_string())
        }
        error => {
            tracing::error!(?error, "preference request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

fn empty_json_object() -> Value {
    Value::Object(Map::new())
}

fn default_true() -> bool {
    true
}
