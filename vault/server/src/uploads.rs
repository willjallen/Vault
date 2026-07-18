use std::collections::HashMap;
use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::auth::{SigningKeyring, UserContext};
use crate::blob_lifecycle::{
    BlobLifecycleError, PendingBlobPublication, begin_blob_publication,
    collect_unreferenced_blobs_with_limit,
};
use crate::documents::{
    ClientMeta, DocumentError, DocumentRecord, document_path, editable_document_for_write,
    normalize_file_name,
};
use crate::folders::{
    FolderError, apply_effective_ttl_to_document_in_tx, get_folder_by_path_in_tx,
    get_or_create_folder_path_in_tx, join_path, normalize_folder, parse_public_folder_path,
    require_write_for_folder_path,
};
use crate::previews::enqueue_preview_job_in_tx;
use crate::state_events::state_event_resources_json;
use crate::storage::{
    BlobStorageBackend, BlobWriteKind, STORAGE_MULTIPART_MAX_PARTS, StorageError, StoredBlob,
};

const MAX_UPLOAD_BYTES: i64 = 5 * 1024 * 1024 * 1024;
const TRANSFER_CHUNK_BYTES: i64 = 32 * 1024 * 1024;
const TRANSFER_SESSION_TTL_SECONDS: i64 = 86_400;
const UPLOAD_MIN_ADAPTIVE_PARTS: i64 = 4;
const UPLOAD_DEFAULT_ADAPTIVE_PARTS: i64 = 16;
const UPLOAD_MAX_ADAPTIVE_PARTS: i64 = 16;
const UPLOAD_MAX_INTEGRITY_CHUNK_BYTES: i64 = 32 * 1024 * 1024;
const UPLOAD_SMALL_ADAPTIVE_MAX_BYTES: i64 = 48 * 1024 * 1024;
const UPLOAD_TARGET_ADAPTIVE_CHUNK_BYTES: i64 = 8 * 1024 * 1024;
const UPLOAD_MIN_ADAPTIVE_CHUNK_BYTES: i64 = 4 * 1024 * 1024;
const UPLOAD_CHUNK_ROUNDING_BYTES: i64 = 1024 * 1024;
const SMALL_PART_MEMORY_BUFFER_BYTES: i64 = 8 * 1024 * 1024;
const VERIFICATION_PROGRESS_UPDATE_BYTES: i64 = 32 * 1024 * 1024;
const MAX_UPLOAD_PART_METADATA_BYTES: u64 = 4096;
const UPLOAD_RESUME_IDENTITY_CONTEXT_KEY: &str = "_upload_resume_identity_sha256";
const UPLOAD_PART_MANIFEST_CONTEXT_KEY: &str = "_upload_part_manifest_sha256";
// Preverification is an optimization. Stop caching new sessions at this bound;
// completion can use a request-owned state and hash the immutable parts without
// evicting an active state or retaining it for the process lifetime.
const MAX_UPLOAD_HASH_STATES: usize = 256;

#[derive(Debug, Clone, Copy)]
pub struct UploadRuntimeSettings {
    pub max_upload_bytes: i64,
    pub transfer_chunk_bytes: i64,
    pub transfer_session_ttl_seconds: i64,
}

impl Default for UploadRuntimeSettings {
    fn default() -> Self {
        Self {
            max_upload_bytes: MAX_UPLOAD_BYTES,
            transfer_chunk_bytes: TRANSFER_CHUNK_BYTES,
            transfer_session_ttl_seconds: TRANSFER_SESSION_TTL_SECONDS,
        }
    }
}

impl UploadRuntimeSettings {
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            max_upload_bytes: self.max_upload_bytes.max(1),
            transfer_chunk_bytes: self.transfer_chunk_bytes.max(1),
            transfer_session_ttl_seconds: self.transfer_session_ttl_seconds.max(60),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateUploadRequest {
    #[serde(default = "default_upload_mode")]
    pub mode: String,
    pub filename: String,
    pub size_bytes: i64,
    pub mime_type: Option<String>,
    #[serde(default)]
    pub folder: String,
    pub document_id: Option<i64>,
    pub note: Option<String>,
    #[serde(default)]
    pub rename_to_upload: bool,
    pub client_upload_parallelism: Option<i64>,
    #[serde(default)]
    pub resume_identity_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompleteUploadRequest {
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub part_manifest_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UploadIntegrityExpectations<'a> {
    pub sha256: Option<&'a str>,
    pub part_manifest_sha256: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UploadSessionPayload {
    pub id: String,
    pub mode: String,
    pub status: String,
    pub filename: String,
    pub size_bytes: i64,
    pub chunk_size: i64,
    pub part_count: i64,
    pub uploaded_bytes: i64,
    pub uploaded_parts: Vec<UploadPartPayload>,
    pub verification: Option<UploadVerificationPayload>,
    pub expires_at: Option<String>,
    pub result: Option<UploadResultPayload>,
    pub resume_identity_sha256: Option<String>,
    pub part_manifest_sha256: Option<String>,
    pub upload_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UploadSessionStatusPayload {
    pub status: String,
    pub verification: Option<UploadVerificationPayload>,
    pub result: Option<UploadResultPayload>,
    pub part_manifest_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UploadPartPayload {
    pub part_number: i64,
    pub offset: i64,
    pub size_bytes: i64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UploadVerificationPayload {
    pub processed_bytes: i64,
    pub total_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UploadResultPayload {
    pub id: i64,
    pub version: String,
    pub path: String,
}

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("upload session not found")]
    UploadSessionNotFound,
    #[error("transfer not found")]
    TransferNotFound,
    #[error("upload session is {0}")]
    UploadSessionStatus(String),
    #[error("upload session expired")]
    UploadSessionExpired,
    #[error("completed upload session is missing result")]
    CompletedSessionMissingResult,
    #[error("unsupported upload session mode")]
    UnsupportedUploadSessionMode,
    #[error("upload size must be non-negative")]
    UploadSizeNegative,
    #[error("upload exceeds limit of {0} bytes")]
    UploadTooLarge(i64),
    #[error("upload requires more than {0} parts")]
    UploadTooManyParts(usize),
    #[error("upload new documents to Vault")]
    UploadNewDocumentsToVault,
    #[error("check out the file before uploading a new version")]
    CheckOutBeforeUploading,
    #[error("invalid part number")]
    InvalidPartNumber,
    #[error("upload part range does not match session")]
    UploadPartRangeMismatch,
    #[error("upload part is too large")]
    UploadPartTooLarge,
    #[error("upload part size does not match session")]
    UploadPartSizeMismatch,
    #[error("upload part checksum mismatch")]
    UploadPartChecksumMismatch,
    #[error("upload part already exists with different content")]
    UploadPartConflict,
    #[error("upload session has missing parts")]
    UploadSessionMissingParts,
    #[error("upload failed while reading request body")]
    UploadReadFailed,
    #[error("upload checksum mismatch")]
    UploadChecksumMismatch,
    #[error("upload completion requires an integrity expectation")]
    UploadIntegrityExpectationRequired,
    #[error("upload integrity digest is invalid")]
    UploadIntegrityDigestInvalid,
    #[error("upload part manifest mismatch")]
    UploadPartManifestMismatch,
    #[error("upload size does not match session")]
    UploadSizeMismatch,
    #[error("upload completion state transition failed: {0}")]
    CompletionStateTransition(String),
    #[error("storage location points at another blob")]
    StorageLocationConflict,
    #[error(transparent)]
    BlobLifecycle(#[from] BlobLifecycleError),
    #[error("upload token is required")]
    UploadTokenRequired,
    #[error("upload token is invalid")]
    UploadTokenInvalid,
    #[error("upload token is not valid for this session")]
    UploadTokenWrongSession,
    #[error("upload token expired")]
    UploadTokenExpired,
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Folder(#[from] FolderError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    TimeFormat(#[from] time::error::Format),
    #[error(transparent)]
    TimeParse(#[from] time::error::Parse),
}

#[derive(Debug, Clone, FromRow)]
struct UploadSessionRow {
    id: String,
    mode: String,
    status: String,
    folder_path: Option<String>,
    document_id: Option<i64>,
    filename: String,
    total_size: i64,
    chunk_size: i64,
    part_count: i64,
    verification_total_bytes: i64,
    verification_processed_bytes: i64,
    mime_type: Option<String>,
    note: Option<String>,
    rename_to_upload: bool,
    created_by: String,
    created_by_name: Option<String>,
    upload_ip: Option<String>,
    upload_user_agent: Option<String>,
    expires_at: String,
    result_document_id: Option<i64>,
    result_version_id: Option<String>,
    result_path: Option<String>,
    resume_identity_sha256: Option<String>,
    part_manifest_sha256: Option<String>,
}

#[derive(Debug, FromRow)]
struct UploadSessionStatusRow {
    status: String,
    total_size: i64,
    verification_total_bytes: i64,
    verification_processed_bytes: i64,
    created_by: String,
    result_document_id: Option<i64>,
    result_version_id: Option<String>,
    result_path: Option<String>,
    part_manifest_sha256: Option<String>,
}

#[derive(Debug, Clone)]
struct UploadPartRow {
    part_number: i64,
    offset_bytes: i64,
    size_bytes: i64,
    sha256: Option<String>,
    storage_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct UploadPartMetadata {
    part_number: i64,
    offset_bytes: i64,
    size_bytes: i64,
    sha256: Option<String>,
}

#[derive(Debug, FromRow)]
struct ActiveUploadLockRow {
    id: i64,
    locked_by: String,
}

#[derive(Debug)]
struct CompletedParts {
    digest: String,
    part_manifest_sha256: String,
    size_bytes: i64,
    paths: Vec<PathBuf>,
    staging_dir: PathBuf,
}

#[derive(Debug, Default)]
struct UploadHashCoordinatorInner {
    states: HashMap<String, Arc<UploadHashState>>,
}

#[derive(Debug, Clone, Default)]
pub struct UploadHashCoordinator {
    inner: Arc<Mutex<UploadHashCoordinatorInner>>,
}

// Large uploads should not finish network transfer and then start integrity
// verification from zero. This coordinator hashes server-received part files in
// order while later parts are still uploading; completion reuses that
// server-computed SHA-256 state, with the old full-pass verification as fallback.
#[derive(Debug)]
struct UploadHashState {
    processed_bytes: AtomicI64,
    inner: Mutex<UploadHashStateInner>,
}

#[derive(Debug)]
struct UploadHashStateInner {
    hasher: Sha256,
    next_part: i64,
    reported_bytes: i64,
    digest: Option<String>,
    part_digests: Vec<String>,
}

impl UploadHashState {
    fn new() -> Self {
        Self {
            processed_bytes: AtomicI64::new(0),
            inner: Mutex::new(UploadHashStateInner {
                hasher: Sha256::new(),
                next_part: 1,
                reported_bytes: 0,
                digest: None,
                part_digests: Vec::new(),
            }),
        }
    }
}

impl UploadHashCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn cache_capacity() -> usize {
        MAX_UPLOAD_HASH_STATES
    }

    pub async fn cached_session_count(&self) -> usize {
        self.inner.lock().await.states.len()
    }

    pub fn schedule(&self, pool: SqlitePool, transfers_path: PathBuf, session_id: String) {
        let coordinator = self.clone();
        tokio::spawn(async move {
            if let Err(error) = coordinator
                .advance_session(&pool, &transfers_path, &session_id, false)
                .await
            {
                tracing::debug!(?error, session_id, "upload hash preverification paused");
            }
        });
    }

    pub async fn forget(&self, session_id: &str) {
        let mut inner = self.inner.lock().await;
        inner.states.remove(session_id);
    }

    pub async fn forget_many(&self, session_ids: &[String]) {
        if session_ids.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().await;
        for session_id in session_ids {
            inner.states.remove(session_id);
        }
    }

    pub async fn preverified_bytes(&self, session_id: &str) -> Option<i64> {
        self.processed_bytes(session_id).await
    }

    async fn processed_bytes(&self, session_id: &str) -> Option<i64> {
        let state = {
            let inner = self.inner.lock().await;
            inner.states.get(session_id).cloned()?
        };
        Some(state.processed_bytes.load(Ordering::Acquire))
    }

    async fn completed_parts(
        &self,
        pool: &SqlitePool,
        transfers_path: &Path,
        session: &UploadSessionRow,
        expected_sha256: Option<&str>,
        expected_part_manifest_sha256: Option<&str>,
    ) -> Result<CompletedParts, UploadError> {
        let (size_bytes, paths) = completed_part_paths(transfers_path, session).await?;
        let state = self
            .advance_session(pool, transfers_path, &session.id, true)
            .await?
            .ok_or(UploadError::UploadSessionMissingParts)?;
        let inner = state.inner.lock().await;
        let digest = inner
            .digest
            .clone()
            .ok_or(UploadError::UploadSessionMissingParts)?;
        let part_manifest_sha256 = validate_completed_integrity(
            session,
            &digest,
            &inner.part_digests,
            expected_sha256,
            expected_part_manifest_sha256,
        )?;
        drop(inner);
        Ok(CompletedParts {
            digest,
            part_manifest_sha256,
            size_bytes,
            paths,
            staging_dir: upload_session_dir(transfers_path, &session.id)?,
        })
    }

    async fn advance_session(
        &self,
        pool: &SqlitePool,
        transfers_path: &Path,
        session_id: &str,
        allow_uncached: bool,
    ) -> Result<Option<Arc<UploadHashState>>, UploadError> {
        let Some(session) = fetch_upload_session(pool, session_id).await? else {
            self.forget(session_id).await;
            return Err(UploadError::UploadSessionNotFound);
        };
        if !matches!(session.status.as_str(), "active" | "completing") {
            self.forget(session_id).await;
            return Ok(None);
        }
        let Some(state) = self.state_for(session_id, allow_uncached).await else {
            return Ok(None);
        };
        let result = advance_hash_state(pool, transfers_path, &session, &state).await;
        // A terminal transition or document cascade can race a scheduled hash
        // task after its initial status read. Recheck after every outcome so the
        // task cannot recreate coordinator state after terminal cleanup.
        let still_hashable = fetch_upload_session(pool, session_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|current| matches!(current.status.as_str(), "active" | "completing"));
        if !still_hashable {
            self.forget(session_id).await;
        }
        result.map(|()| Some(state))
    }

    async fn state_for(
        &self,
        session_id: &str,
        allow_uncached: bool,
    ) -> Option<Arc<UploadHashState>> {
        let mut inner = self.inner.lock().await;
        if let Some(state) = inner.states.get(session_id).cloned() {
            return Some(state);
        }
        if inner.states.len() >= MAX_UPLOAD_HASH_STATES {
            return allow_uncached.then(|| Arc::new(UploadHashState::new()));
        }
        let state = Arc::new(UploadHashState::new());
        inner.states.insert(session_id.to_string(), state.clone());
        Some(state)
    }
}

#[derive(Debug)]
struct UploadCompletionAttempt {
    pool: SqlitePool,
    transfers_path: PathBuf,
    session_id: String,
    hash_coordinator: Option<UploadHashCoordinator>,
    drop_action: UploadCompletionDropAction,
    armed: bool,
}

#[derive(Debug, Clone)]
enum UploadCompletionDropAction {
    Retry,
    Fail(String),
}

impl UploadCompletionAttempt {
    fn new(
        pool: &SqlitePool,
        transfers_path: &Path,
        session_id: &str,
        hash_coordinator: Option<&UploadHashCoordinator>,
    ) -> Self {
        Self {
            pool: pool.clone(),
            transfers_path: transfers_path.to_path_buf(),
            session_id: session_id.to_string(),
            hash_coordinator: hash_coordinator.cloned(),
            drop_action: UploadCompletionDropAction::Retry,
            armed: true,
        }
    }

    async fn reset_for_retry(&mut self) -> Result<(), UploadError> {
        let pool = self.pool.clone();
        let session_id = self.session_id.clone();
        let hash_coordinator = self.hash_coordinator.clone();
        // Transfer recovery ownership to one task before awaiting it. If the
        // request is cancelled, that task continues and Drop must not start a
        // second reset that could race a fast retry's new completion claim.
        let recovery = tokio::spawn(async move {
            if let Some(coordinator) = hash_coordinator {
                coordinator.forget(&session_id).await;
            }
            // Publish `active` only after coordinator cleanup. Callers can
            // treat the status transition as retry readiness.
            reset_upload_completion_for_retry(&pool, &session_id).await
        });
        self.armed = false;
        recovery
            .await
            .map_err(|error| UploadError::CompletionStateTransition(error.to_string()))?
    }

    fn fail_on_drop(&mut self, message: String) {
        self.drop_action = UploadCompletionDropAction::Fail(message);
    }

    async fn forget_and_disarm(&mut self) {
        let session_id = self.session_id.clone();
        let hash_coordinator = self.hash_coordinator.clone();
        // As with retry recovery, give cleanup to one owned task before
        // disarming. Cancellation cannot leave coordinator state resident or
        // trigger a second state transition.
        let cleanup = tokio::spawn(async move {
            if let Some(coordinator) = hash_coordinator {
                coordinator.forget(&session_id).await;
            }
        });
        self.armed = false;
        if let Err(error) = cleanup.await {
            tracing::error!(?error, "upload completion coordinator cleanup task failed");
        }
    }
}

impl Drop for UploadCompletionAttempt {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let pool = self.pool.clone();
        let transfers_path = self.transfers_path.clone();
        let session_id = self.session_id.clone();
        let hash_coordinator = self.hash_coordinator.clone();
        let drop_action = self.drop_action.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                session_id,
                "upload completion attempt dropped outside a Tokio runtime; startup recovery must reset it"
            );
            return;
        };
        runtime.spawn(async move {
            if let Some(coordinator) = hash_coordinator {
                coordinator.forget(&session_id).await;
            }
            let recovery = match drop_action {
                UploadCompletionDropAction::Retry => {
                    reset_upload_completion_for_retry(&pool, &session_id).await
                }
                UploadCompletionDropAction::Fail(message) => {
                    mark_upload_failed(&pool, &transfers_path, &session_id, &message).await
                }
            };
            if let Err(error) = recovery {
                tracing::error!(
                    ?error,
                    session_id,
                    "could not settle interrupted upload completion"
                );
            }
        });
    }
}

pub async fn create_upload_session(
    pool: &SqlitePool,
    transfers_path: &Path,
    signing_keys: &SigningKeyring,
    settings: UploadRuntimeSettings,
    payload: CreateUploadRequest,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<UploadSessionPayload, UploadError> {
    let settings = settings.normalized();
    let mode = normalize_upload_mode(&payload.mode)?;
    let filename = normalize_file_name(&payload.filename)?;
    if payload.size_bytes < 0 {
        return Err(UploadError::UploadSizeNegative);
    }
    let max_upload_bytes = settings.max_upload_bytes;
    if payload.size_bytes > max_upload_bytes {
        return Err(UploadError::UploadTooLarge(max_upload_bytes));
    }
    let mime_type = sanitize_mime_type(payload.mime_type.as_deref(), &filename);
    let resume_identity_sha256 =
        normalize_optional_sha256(payload.resume_identity_sha256.as_deref())?;
    let chunk_size = choose_upload_chunk_size(
        payload.size_bytes,
        payload.client_upload_parallelism,
        settings.transfer_chunk_bytes,
    );
    let part_count = part_count(payload.size_bytes, chunk_size);
    if usize::try_from(part_count).map_or(true, |count| count > STORAGE_MULTIPART_MAX_PARTS) {
        return Err(UploadError::UploadTooManyParts(STORAGE_MULTIPART_MAX_PARTS));
    }
    let (folder_path, document_id) =
        prepare_upload_target(pool, &mode, &filename, &payload, user).await?;

    let session_id = if resume_identity_sha256.is_some() {
        format!("u2-{}", Uuid::new_v4().simple())
    } else {
        Uuid::new_v4().simple().to_string()
    };
    let session_dir = upload_session_dir(transfers_path, &session_id)?;
    create_durable_upload_session_dir(transfers_path, &session_dir).await?;
    let now = now_rfc3339()?;
    let expires_at = (OffsetDateTime::now_utc()
        + Duration::seconds(settings.transfer_session_ttl_seconds))
    .format(&Rfc3339)?;
    let mut user_context = serde_json::to_value(user)?;
    if let (Some(identity), Some(context)) = (
        resume_identity_sha256.as_ref(),
        user_context.as_object_mut(),
    ) {
        context.insert(
            UPLOAD_RESUME_IDENTITY_CONTEXT_KEY.to_string(),
            serde_json::Value::String(identity.clone()),
        );
    }
    sqlx::query(
        r"
        INSERT INTO upload_sessions
            (
                id,
                mode,
                status,
                folder_path,
                document_id,
                filename,
                total_size,
                chunk_size,
                part_count,
                mime_type,
                note,
                rename_to_upload,
                created_by,
                created_by_name,
                user_context,
                upload_ip,
                upload_user_agent,
                created_at,
                updated_at,
                expires_at
            )
        VALUES
            (?, ?, 'active', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ",
    )
    .bind(&session_id)
    .bind(&mode)
    .bind(&folder_path)
    .bind(document_id)
    .bind(&filename)
    .bind(payload.size_bytes)
    .bind(chunk_size)
    .bind(part_count)
    .bind(&mime_type)
    .bind(trim_to_option(payload.note.as_deref()))
    .bind(payload.rename_to_upload)
    .bind(&user.id)
    .bind(&user.name)
    .bind(serde_json::to_string(&user_context)?)
    .bind(&meta.ip)
    .bind(&meta.user_agent)
    .bind(&now)
    .bind(&now)
    .bind(&expires_at)
    .execute(pool)
    .await?;
    upload_session_payload(pool, transfers_path, signing_keys, &session_id).await
}

async fn prepare_upload_target(
    pool: &SqlitePool,
    mode: &str,
    filename: &str,
    payload: &CreateUploadRequest,
    user: &UserContext,
) -> Result<(Option<String>, Option<i64>), UploadError> {
    match mode {
        "create" => prepare_create_upload_target(pool, filename, payload, user).await,
        "checkin" => prepare_checkin_upload_target(pool, filename, payload, user).await,
        _ => Err(UploadError::UnsupportedUploadSessionMode),
    }
}

async fn prepare_create_upload_target(
    pool: &SqlitePool,
    filename: &str,
    payload: &CreateUploadRequest,
    user: &UserContext,
) -> Result<(Option<String>, Option<i64>), UploadError> {
    let folder_path = normalize_folder(Some(&payload.folder))?;
    ensure_upload_folder(&folder_path)?;
    require_write_for_folder_path(pool, &folder_path, user).await?;
    let mut transaction = pool.begin().await?;
    let target_folder = get_or_create_folder_path_in_tx(&mut transaction, &folder_path).await?;
    ensure_unique_document_name_in_tx(&mut transaction, target_folder.id, filename, None).await?;
    transaction.commit().await?;
    Ok((Some(folder_path), None))
}

async fn prepare_checkin_upload_target(
    pool: &SqlitePool,
    filename: &str,
    payload: &CreateUploadRequest,
    user: &UserContext,
) -> Result<(Option<String>, Option<i64>), UploadError> {
    let document_id = payload.document_id.unwrap_or_default();
    let document = editable_document_for_upload(pool, document_id, user).await?;
    let lock = active_upload_lock(pool, document.id).await?;
    if lock.as_ref().is_none_or(|lock| lock.locked_by != user.id) {
        return Err(UploadError::CheckOutBeforeUploading);
    }
    if payload.rename_to_upload && filename != document.name {
        let mut transaction = pool.begin().await?;
        ensure_unique_document_name_in_tx(
            &mut transaction,
            document.folder_id,
            filename,
            Some(document.id),
        )
        .await?;
        transaction.commit().await?;
    }
    Ok((None, Some(document.id)))
}

pub async fn get_upload_session(
    pool: &SqlitePool,
    transfers_path: &Path,
    signing_keys: &SigningKeyring,
    session_id: &str,
    user: &UserContext,
) -> Result<UploadSessionPayload, UploadError> {
    let session = fetch_upload_session(pool, session_id)
        .await?
        .ok_or(UploadError::UploadSessionNotFound)?;
    require_transfer_owner(&session, user)?;
    if session.status == "active" {
        ensure_session_not_expired(pool, transfers_path, &session).await?;
    }
    upload_session_payload(pool, transfers_path, signing_keys, session_id).await
}

pub async fn get_upload_session_status(
    pool: &SqlitePool,
    session_id: &str,
    user: &UserContext,
) -> Result<UploadSessionStatusPayload, UploadError> {
    let row = sqlx::query_as::<_, UploadSessionStatusRow>(
        r"
        SELECT
            status,
            total_size,
            verification_total_bytes,
            verification_processed_bytes,
            created_by,
            result_document_id,
            result_version_id,
            result_path,
            json_extract(user_context, '$.' || ?)
                AS part_manifest_sha256
        FROM upload_sessions
        WHERE id = ?
        ",
    )
    .bind(UPLOAD_PART_MANIFEST_CONTEXT_KEY)
    .bind(session_id)
    .fetch_optional(pool)
    .await?
    .ok_or(UploadError::UploadSessionNotFound)?;
    if row.created_by != user.id && !user.is_admin {
        return Err(UploadError::UploadSessionNotFound);
    }

    let verification = match row.status.as_str() {
        "completing" => {
            let total_bytes = row.verification_total_bytes.clamp(0, row.total_size.max(0));
            Some(UploadVerificationPayload {
                processed_bytes: row.verification_processed_bytes.clamp(0, total_bytes),
                total_bytes,
            })
        }
        "complete" => {
            let total_bytes = row.total_size.max(0);
            Some(UploadVerificationPayload {
                processed_bytes: total_bytes,
                total_bytes,
            })
        }
        _ => None,
    };
    let result = if row.status == "complete" {
        Some(completed_status_result(&row)?)
    } else {
        None
    };
    let part_manifest_sha256 = if row.status == "complete" {
        row.part_manifest_sha256
    } else {
        None
    };
    Ok(UploadSessionStatusPayload {
        status: row.status,
        verification,
        result,
        part_manifest_sha256,
    })
}

pub async fn ingest_upload_part<S, E>(
    pool: &SqlitePool,
    ingest: UploadPartIngest<'_>,
    user: &UserContext,
    stream: S,
) -> Result<(), UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    ingest_upload_part_authorized(pool, ingest, PartAuthorization::User(user), stream).await
}

pub async fn ingest_upload_part_for_owner<S, E>(
    pool: &SqlitePool,
    ingest: UploadPartIngest<'_>,
    owner_id: &str,
    stream: S,
) -> Result<(), UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    ingest_upload_part_authorized(pool, ingest, PartAuthorization::OwnerId(owner_id), stream).await
}

pub async fn ingest_upload_part_with_token<S, E>(
    pool: &SqlitePool,
    ingest: UploadPartIngest<'_>,
    token_claims: UploadPartTokenClaims,
    stream: S,
) -> Result<(), UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    let session = fetch_upload_session(pool, ingest.session_id)
        .await?
        .ok_or(UploadError::UploadSessionNotFound)?;
    if !token_claims.matches_session(&session) {
        return Err(UploadError::UploadTokenInvalid);
    }
    require_part_authorization(&session, PartAuthorization::OwnerId(&token_claims.owner_id))?;
    ensure_active_session(pool, ingest.transfers_path, &session).await?;
    ingest_upload_part_for_session(ingest, session, stream).await
}

async fn ingest_upload_part_authorized<S, E>(
    pool: &SqlitePool,
    ingest: UploadPartIngest<'_>,
    authorization: PartAuthorization<'_>,
    stream: S,
) -> Result<(), UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    let session = fetch_upload_session(pool, ingest.session_id)
        .await?
        .ok_or(UploadError::UploadSessionNotFound)?;
    require_part_authorization(&session, authorization)?;
    ensure_active_session(pool, ingest.transfers_path, &session).await?;
    ingest_upload_part_for_session(ingest, session, stream).await
}

async fn ingest_upload_part_for_session<S, E>(
    ingest: UploadPartIngest<'_>,
    session: UploadSessionRow,
    mut stream: S,
) -> Result<(), UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    if (session.resume_identity_sha256.is_some() && ingest.headers.sha256.is_none())
        || ingest
            .headers
            .sha256
            .is_some_and(|value| !is_sha256_hex(value))
    {
        return Err(UploadError::UploadIntegrityDigestInvalid);
    }
    let (expected_offset, expected_size) = expected_part_bounds(&session, ingest.part_number)?;
    if ingest.headers.offset != expected_offset || ingest.headers.size != expected_size {
        return Err(UploadError::UploadPartRangeMismatch);
    }
    let session_dir = upload_session_dir(ingest.transfers_path, ingest.session_id)?;
    let temp_path = session_dir.join(format!(
        "{:08}.part.tmp-{}",
        ingest.part_number,
        Uuid::new_v4().simple()
    ));
    let final_path = part_file_path(&session_dir, ingest.part_number);
    let write_result = write_part_stream(
        &temp_path,
        expected_size,
        ingest.headers.sha256,
        &mut stream,
    )
    .await;
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    write_result?;

    // The normal upload path is first-write-wins. Avoid probing for an existing
    // part before every PUT; that is pure miss-path filesystem work for fresh
    // high-fanout uploads. The atomic hard-link promotion fails if a retry or
    // race already created the final part, and only then do we inspect metadata
    // to preserve resumable/idempotent duplicate semantics.
    let part = UploadPartRow {
        part_number: ingest.part_number,
        offset_bytes: expected_offset,
        size_bytes: expected_size,
        sha256: ingest.headers.sha256.map(str::to_ascii_lowercase),
        storage_path: final_path.to_string_lossy().to_string(),
    };
    if !promote_part_file(&temp_path, &final_path).await? {
        if let Some(mut existing) =
            read_part_metadata(&session_dir, &session, ingest.part_number).await?
        {
            if let Some(incoming_sha256) = ingest.headers.sha256
                && existing.sha256.as_deref() != Some(incoming_sha256)
            {
                let incoming_sha256 = incoming_sha256.to_ascii_lowercase();
                if hash_existing_upload_part(&final_path, expected_size).await? != incoming_sha256 {
                    return Err(UploadError::UploadPartConflict);
                }
                // A first writer can be interrupted after linking its durable
                // part but before publishing the checksum sidecar. Repair that
                // state from the verified immutable final file so both the
                // interrupted writer and an identical concurrent writer remain
                // safely retryable.
                existing.sha256 = Some(incoming_sha256);
                write_part_metadata(&session_dir, &existing).await?;
            }
            if part_metadata_matches(
                &existing,
                expected_offset,
                expected_size,
                ingest.headers.sha256,
            ) {
                // A concurrent first writer may have linked the part immediately
                // before this request observed it. Make the data and its directory
                // entry durable ourselves rather than returning success while that
                // writer is still between promotion and its directory sync.
                sync_file(&final_path).await?;
                if existing.sha256.is_some() {
                    sync_file(&part_metadata_path(&session_dir, ingest.part_number)).await?;
                }
                sync_directory(&session_dir).await?;
                return Ok(());
            }
        }
        return Err(UploadError::UploadPartConflict);
    }
    if part.sha256.is_some() {
        write_part_metadata(&session_dir, &part).await?;
    }
    // The temporary part inode (and optional sidecar inode) have already been
    // synced. Persist their final names before acknowledging the part.
    sync_directory(&session_dir).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct UploadPartHeaders<'a> {
    pub offset: i64,
    pub size: i64,
    pub sha256: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct UploadPartIngest<'a> {
    pub transfers_path: &'a Path,
    pub session_id: &'a str,
    pub part_number: i64,
    pub headers: UploadPartHeaders<'a>,
}

#[derive(Debug, Clone, Copy)]
enum PartAuthorization<'a> {
    User(&'a UserContext),
    OwnerId(&'a str),
}

#[derive(Debug, Clone)]
pub struct UploadPartTokenClaims {
    session_id: String,
    owner_id: String,
    mode: String,
    filename: String,
    total_size: i64,
    chunk_size: i64,
    part_count: i64,
    expires_at: String,
    resume_identity_sha256: Option<String>,
}

impl UploadPartTokenClaims {
    #[must_use]
    pub fn is_expired(&self) -> bool {
        OffsetDateTime::parse(&self.expires_at, &Rfc3339)
            .map_or(true, |expires_at| expires_at < OffsetDateTime::now_utc())
    }

    fn matches_session(&self, session: &UploadSessionRow) -> bool {
        self.session_id == session.id
            && self.owner_id == session.created_by
            && self.mode == session.mode
            && self.filename == session.filename
            && self.total_size == session.total_size
            && self.chunk_size == session.chunk_size
            && self.part_count == session.part_count
            && self.expires_at == session.expires_at
            && self.resume_identity_sha256 == session.resume_identity_sha256
    }
}

pub async fn complete_upload_session(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    transfers_path: &Path,
    session_id: &str,
    expected_sha256: Option<&str>,
    user: &UserContext,
) -> Result<UploadResultPayload, UploadError> {
    complete_upload_session_impl(
        pool,
        storage,
        transfers_path,
        session_id,
        UploadIntegrityExpectations {
            sha256: expected_sha256,
            part_manifest_sha256: None,
        },
        user,
        None,
    )
    .await
}

pub async fn complete_upload_session_with_hash_coordinator(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    transfers_path: &Path,
    session_id: &str,
    expected_sha256: Option<&str>,
    user: &UserContext,
    hash_coordinator: &UploadHashCoordinator,
) -> Result<UploadResultPayload, UploadError> {
    complete_upload_session_with_hash_coordinator_and_manifest(
        pool,
        storage,
        transfers_path,
        session_id,
        UploadIntegrityExpectations {
            sha256: expected_sha256,
            part_manifest_sha256: None,
        },
        user,
        hash_coordinator,
    )
    .await
}

pub async fn complete_upload_session_with_hash_coordinator_and_manifest(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    transfers_path: &Path,
    session_id: &str,
    expectations: UploadIntegrityExpectations<'_>,
    user: &UserContext,
    hash_coordinator: &UploadHashCoordinator,
) -> Result<UploadResultPayload, UploadError> {
    complete_upload_session_impl(
        pool,
        storage,
        transfers_path,
        session_id,
        expectations,
        user,
        Some(hash_coordinator),
    )
    .await
}

// The completion guard must cover every validation, publication, retry, and terminal path.
#[allow(clippy::too_many_lines)]
async fn complete_upload_session_impl(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    transfers_path: &Path,
    session_id: &str,
    expectations: UploadIntegrityExpectations<'_>,
    user: &UserContext,
    hash_coordinator: Option<&UploadHashCoordinator>,
) -> Result<UploadResultPayload, UploadError> {
    let session = fetch_upload_session(pool, session_id)
        .await?
        .ok_or(UploadError::UploadSessionNotFound)?;
    require_transfer_owner(&session, user)?;
    if session.status == "complete" {
        return completed_result(&session);
    }
    ensure_active_session(pool, transfers_path, &session).await?;
    let expected_sha256 = normalize_optional_sha256(expectations.sha256)?;
    let expected_part_manifest_sha256 =
        normalize_optional_sha256(expectations.part_manifest_sha256)?;
    if expected_sha256.is_none() && expected_part_manifest_sha256.is_none() {
        return Err(UploadError::UploadIntegrityExpectationRequired);
    }
    let verified_bytes = if let Some(coordinator) = hash_coordinator {
        coordinator
            .processed_bytes(session_id)
            .await
            .unwrap_or_default()
    } else {
        0
    };
    let mut completion_attempt = claim_upload_completion(
        pool,
        transfers_path,
        session_id,
        session.total_size,
        verified_bytes,
        hash_coordinator,
    )
    .await?;
    let parts_result = if let Some(coordinator) = hash_coordinator {
        coordinator
            .completed_parts(
                pool,
                transfers_path,
                &session,
                expected_sha256.as_deref(),
                expected_part_manifest_sha256.as_deref(),
            )
            .await
    } else {
        completed_parts(
            pool,
            transfers_path,
            &session,
            expected_sha256.as_deref(),
            expected_part_manifest_sha256.as_deref(),
        )
        .await
    };
    let parts = match parts_result {
        Ok(parts) => parts,
        Err(error) => {
            if let Err(recovery_error) = completion_attempt.reset_for_retry().await {
                tracing::error!(
                    ?error,
                    ?recovery_error,
                    session_id,
                    "upload completion validation and state recovery both failed"
                );
                return Err(recovery_error);
            }
            return Err(error);
        }
    };

    let result = complete_upload_session_inner(pool, storage, &session, &parts, user).await;
    match result {
        Ok(payload) => {
            completion_attempt.forget_and_disarm().await;
            clear_upload_session_files(transfers_path, session_id).await;
            Ok(payload)
        }
        Err(error) => {
            if matches!(
                &error,
                UploadError::BlobLifecycle(
                    BlobLifecycleError::DeletionInProgress
                        | BlobLifecycleError::PublicationLeaseLost
                )
            ) {
                if let Err(recovery_error) = completion_attempt.reset_for_retry().await {
                    tracing::error!(
                        ?error,
                        ?recovery_error,
                        session_id,
                        "transient blob lifecycle error and upload state recovery both failed"
                    );
                    return Err(recovery_error);
                }
                return Err(error);
            }
            completion_attempt.fail_on_drop(error.to_string());
            match mark_upload_failed(pool, transfers_path, session_id, &error.to_string()).await {
                Ok(()) => {
                    completion_attempt.forget_and_disarm().await;
                    Err(error)
                }
                Err(failure_error) => {
                    completion_attempt.forget_and_disarm().await;
                    tracing::error!(
                        ?error,
                        ?failure_error,
                        session_id,
                        "upload completion and terminal state recovery both failed"
                    );
                    Err(failure_error)
                }
            }
        }
    }
}

pub async fn abort_upload_session(
    pool: &SqlitePool,
    transfers_path: &Path,
    signing_keys: &SigningKeyring,
    session_id: &str,
    user: &UserContext,
) -> Result<UploadSessionPayload, UploadError> {
    let session = fetch_upload_session(pool, session_id)
        .await?
        .ok_or(UploadError::UploadSessionNotFound)?;
    require_transfer_owner(&session, user)?;
    let now = now_rfc3339()?;
    let aborted = sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'aborted',
            aborted_at = ?,
            updated_at = ?
        WHERE id = ?
          AND status IN ('active', 'completing')
        ",
    )
    .bind(&now)
    .bind(&now)
    .bind(session_id)
    .execute(pool)
    .await?;
    let current_status: String =
        sqlx::query_scalar("SELECT status FROM upload_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(pool)
            .await?
            .ok_or(UploadError::UploadSessionNotFound)?;
    if aborted.rows_affected() > 0
        || matches!(current_status.as_str(), "aborted" | "failed" | "expired")
    {
        clear_upload_session_files(transfers_path, session_id).await;
    }
    upload_session_payload(pool, transfers_path, signing_keys, session_id).await
}

async fn complete_upload_session_inner(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    session: &UploadSessionRow,
    parts: &CompletedParts,
    user: &UserContext,
) -> Result<UploadResultPayload, UploadError> {
    let size_bytes =
        u64::try_from(parts.size_bytes).map_err(|_| UploadError::UploadSizeMismatch)?;
    let publication = begin_blob_publication(
        pool,
        storage,
        "sha256",
        &parts.digest,
        size_bytes,
        BlobWriteKind::PartFiles,
    )
    .await?;
    let stored = match publication
        .run_storage(storage.put_part_files_in_staging(
            &parts.paths,
            Some(&parts.digest),
            &parts.staging_dir,
        ))
        .await
    {
        Ok(stored) => stored,
        Err(error) => {
            if let Err(cleanup_error) = publication.abandon(None).await {
                tracing::error!(
                    ?cleanup_error,
                    "failed to queue an unsuccessful upload publication for cleanup"
                );
            }
            return Err(error.into());
        }
    };
    let result =
        complete_upload_session_after_store(pool, session, parts, user, &stored, &publication)
            .await;
    match result {
        Ok(payload) => Ok(payload),
        Err(error) => {
            tracing::warn!(
                object_key = %stored.object_key,
                "upload object promotion succeeded but the metadata commit failed; leaving object for delayed cleanup"
            );
            if let Err(cleanup_error) = publication.abandon(Some(&stored)).await {
                tracing::error!(
                    ?cleanup_error,
                    object_key = %stored.object_key,
                    "failed to preserve upload object metadata for delayed cleanup"
                );
            } else if let Err(cleanup_error) =
                collect_unreferenced_blobs_with_limit(pool, storage, 1).await
            {
                tracing::warn!(?cleanup_error, "prompt upload-object cleanup failed");
            }
            Err(error)
        }
    }
}

async fn complete_upload_session_after_store(
    pool: &SqlitePool,
    session: &UploadSessionRow,
    parts: &CompletedParts,
    user: &UserContext,
    stored: &StoredBlob,
    publication: &PendingBlobPublication,
) -> Result<UploadResultPayload, UploadError> {
    if i64::try_from(stored.size_bytes).ok() != Some(session.total_size)
        || parts.size_bytes != session.total_size
    {
        return Err(UploadError::UploadSizeMismatch);
    }
    let checkin_document = if session.mode == "checkin" {
        let document_id = session.document_id.unwrap_or_default();
        let document = editable_document_for_upload(pool, document_id, user).await?;
        let mut result_document = document.clone();
        result_document.name = checkin_target_name(session, &document);
        Some((document, document_path(pool, &result_document).await?))
    } else {
        None
    };
    if session.mode == "create" {
        require_write_for_folder_path(
            pool,
            session.folder_path.as_deref().unwrap_or_default(),
            user,
        )
        .await?;
    }
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let blob_id = publication
        .prepare_metadata_in_tx(&mut transaction, stored)
        .await?;
    let result = match session.mode.as_str() {
        "create" => complete_create_upload_in_tx(&mut transaction, session, blob_id).await?,
        "checkin" => {
            let Some((document, result_path)) = checkin_document.as_ref() else {
                return Err(UploadError::UnsupportedUploadSessionMode);
            };
            complete_checkin_upload_in_tx(&mut transaction, session, blob_id, document, result_path)
                .await?
        }
        _ => return Err(UploadError::UnsupportedUploadSessionMode),
    };
    record_state_event_in_tx(
        &mut transaction,
        "document.upload.complete",
        &["contents", "sidebar", "document_detail"],
    )
    .await?;
    let completed = sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'complete',
            verification_total_bytes = ?,
            verification_processed_bytes = ?,
            completed_at = ?,
            updated_at = ?,
            result_document_id = ?,
            result_version_id = ?,
            result_path = ?,
            user_context = json_set(user_context, '$.' || ?, ?)
        WHERE id = ?
          AND status = 'completing'
        ",
    )
    .bind(session.total_size)
    .bind(session.total_size)
    .bind(now_rfc3339()?)
    .bind(now_rfc3339()?)
    .bind(result.id)
    .bind(&result.version)
    .bind(&result.path)
    .bind(UPLOAD_PART_MANIFEST_CONTEXT_KEY)
    .bind(&parts.part_manifest_sha256)
    .bind(&session.id)
    .execute(&mut *transaction)
    .await?;
    if completed.rows_affected() == 0 {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM upload_sessions WHERE id = ?")
                .bind(&session.id)
                .fetch_optional(&mut *transaction)
                .await?;
        transaction.rollback().await?;
        return Err(match status {
            Some(status) => UploadError::UploadSessionStatus(status),
            None => UploadError::UploadSessionNotFound,
        });
    }
    publication.finish_metadata_in_tx(&mut transaction).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn complete_create_upload_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    session: &UploadSessionRow,
    blob_id: i64,
) -> Result<UploadResultPayload, UploadError> {
    let folder_path = session.folder_path.clone().unwrap_or_default();
    let target_folder = get_folder_by_path_in_tx(transaction, &folder_path)
        .await?
        .ok_or(FolderError::FolderNotFound)?;
    ensure_unique_document_name_in_tx(transaction, target_folder.id, &session.filename, None)
        .await?;
    let inserted = sqlx::query(
        r"
        INSERT INTO documents
            (
                folder_id,
                name,
                created_by,
                created_by_name,
                latest_modified_by,
                latest_modified_at
            )
        VALUES
            (?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
        ",
    )
    .bind(target_folder.id)
    .bind(&session.filename)
    .bind(&session.created_by)
    .bind(session.created_by_name.as_deref())
    .bind(&session.created_by)
    .execute(&mut **transaction)
    .await?;
    let document_id = inserted.last_insert_rowid();
    apply_effective_ttl_to_document_in_tx(transaction, document_id, target_folder.id).await?;
    let version_id = create_document_version_in_tx(
        transaction,
        CreateVersion {
            document_id,
            blob_id,
            actor_id: &session.created_by,
            actor_name: session.created_by_name.as_deref(),
            message: &format!("Uploaded {}", session.filename),
            mime_type: session.mime_type.as_deref(),
            original_filename: &session.filename,
            upload_ip: session.upload_ip.as_deref(),
            upload_user_agent: session.upload_user_agent.as_deref(),
            created_via: "upload",
            folder_id: target_folder.id,
        },
    )
    .await?;
    Ok(UploadResultPayload {
        id: document_id,
        version: version_id,
        path: join_path(&[&folder_path, &session.filename]),
    })
}

async fn complete_checkin_upload_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    session: &UploadSessionRow,
    blob_id: i64,
    document: &DocumentRecord,
    result_path: &str,
) -> Result<UploadResultPayload, UploadError> {
    let lock = active_upload_lock_in_tx(transaction, document.id).await?;
    let Some(lock) = lock.filter(|lock| lock.locked_by == session.created_by) else {
        return Err(UploadError::CheckOutBeforeUploading);
    };
    let target_name = checkin_target_name(session, document);
    if target_name != document.name {
        ensure_unique_document_name_in_tx(
            transaction,
            document.folder_id,
            &target_name,
            Some(document.id),
        )
        .await?;
        record_document_event_in_tx(
            transaction,
            document.id,
            session,
            "move",
            &format!("Renamed {} to {target_name}", document.name),
        )
        .await?;
        // Python treats a check-in rename as a real document move/rename event
        // before the new version lands, so subscribers refresh path-sensitive views.
        record_state_event_in_tx(
            transaction,
            "document.move",
            &["contents", "sidebar", "document_detail"],
        )
        .await?;
    }
    let version_message = trim_to_option(session.note.as_deref())
        .unwrap_or_else(|| format!("Uploaded {}", session.filename));
    let version_id =
        create_checkin_version_in_tx(transaction, session, document, blob_id, &version_message)
            .await?;
    release_checkin_lock_in_tx(
        transaction,
        session,
        lock.id,
        document.id,
        document.folder_id,
        &target_name,
        result_path,
    )
    .await?;
    Ok(UploadResultPayload {
        id: document.id,
        version: version_id,
        path: result_path.to_string(),
    })
}

fn checkin_target_name(session: &UploadSessionRow, document: &DocumentRecord) -> String {
    if session.rename_to_upload {
        session.filename.clone()
    } else {
        document.name.clone()
    }
}

async fn create_checkin_version_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    session: &UploadSessionRow,
    document: &DocumentRecord,
    blob_id: i64,
    version_message: &str,
) -> Result<String, UploadError> {
    let version_id = create_document_version_in_tx(
        transaction,
        CreateVersion {
            document_id: document.id,
            blob_id,
            actor_id: &session.created_by,
            actor_name: session.created_by_name.as_deref(),
            message: version_message,
            mime_type: session.mime_type.as_deref(),
            original_filename: &session.filename,
            upload_ip: session.upload_ip.as_deref(),
            upload_user_agent: session.upload_user_agent.as_deref(),
            created_via: "checkin",
            folder_id: document.folder_id,
        },
    )
    .await?;
    Ok(version_id)
}

async fn release_checkin_lock_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    session: &UploadSessionRow,
    lock_id: i64,
    document_id: i64,
    folder_id: i64,
    target_name: &str,
    result_path: &str,
) -> Result<(), UploadError> {
    sqlx::query(
        r"
        UPDATE documents
        SET name = ?,
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?
        WHERE id = ?
        ",
    )
    .bind(target_name)
    .bind(&session.created_by)
    .bind(document_id)
    .execute(&mut **transaction)
    .await?;
    apply_effective_ttl_to_document_in_tx(transaction, document_id, folder_id).await?;
    sqlx::query(
        r"
        UPDATE document_locks
        SET is_active = 0,
            released_at = CURRENT_TIMESTAMP,
            released_by = ?
        WHERE id = ?
        ",
    )
    .bind(&session.created_by)
    .bind(lock_id)
    .execute(&mut **transaction)
    .await?;
    record_document_event_in_tx(
        transaction,
        document_id,
        session,
        "release",
        &format!("Released lock for {result_path}"),
    )
    .await?;
    Ok(())
}

struct CreateVersion<'a> {
    document_id: i64,
    blob_id: i64,
    actor_id: &'a str,
    actor_name: Option<&'a str>,
    message: &'a str,
    mime_type: Option<&'a str>,
    original_filename: &'a str,
    upload_ip: Option<&'a str>,
    upload_user_agent: Option<&'a str>,
    created_via: &'a str,
    folder_id: i64,
}

async fn create_document_version_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    version: CreateVersion<'_>,
) -> Result<String, UploadError> {
    let version_number = next_version_number_in_tx(transaction, version.document_id).await?;
    let version_id = new_version_id();
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
                upload_ip,
                upload_user_agent,
                created_via
            )
        VALUES
            (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ",
    )
    .bind(&version_id)
    .bind(version.document_id)
    .bind(version.blob_id)
    .bind(version_number)
    .bind(version.actor_id)
    .bind(version.actor_name)
    .bind(version.message)
    .bind(version.mime_type)
    .bind(version.original_filename)
    .bind(version.upload_ip)
    .bind(version.upload_user_agent)
    .bind(version.created_via)
    .execute(&mut **transaction)
    .await?;
    enqueue_preview_job_in_tx(transaction, version.blob_id).await?;
    sqlx::query(
        r"
        UPDATE documents
        SET current_version_id = ?,
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?,
            latest_version_number = ?,
            version_count = CASE
                WHEN version_count > ? THEN version_count
                ELSE ?
            END
        WHERE id = ?
        ",
    )
    .bind(&version_id)
    .bind(version.actor_id)
    .bind(version_number)
    .bind(version_number)
    .bind(version_number)
    .bind(version.document_id)
    .execute(&mut **transaction)
    .await?;
    apply_effective_ttl_to_document_in_tx(transaction, version.document_id, version.folder_id)
        .await?;
    let resources = if version.created_via == "checkin" {
        &["contents", "document_detail", "my_edits"][..]
    } else {
        &["contents", "sidebar", "document_detail"][..]
    };
    record_state_event_in_tx(
        transaction,
        &format!("document.{}", version.created_via),
        resources,
    )
    .await?;
    Ok(version_id)
}

async fn completed_parts(
    pool: &SqlitePool,
    transfers_path: &Path,
    session: &UploadSessionRow,
    expected_sha256: Option<&str>,
    expected_part_manifest_sha256: Option<&str>,
) -> Result<CompletedParts, UploadError> {
    let (size_bytes, paths) = completed_part_paths(transfers_path, session).await?;
    let (digest, part_digests) = hash_completed_part_paths(pool, session, &paths).await?;
    let part_manifest_sha256 = validate_completed_integrity(
        session,
        &digest,
        &part_digests,
        expected_sha256,
        expected_part_manifest_sha256,
    )?;
    Ok(CompletedParts {
        digest,
        part_manifest_sha256,
        size_bytes,
        paths,
        staging_dir: upload_session_dir(transfers_path, &session.id)?,
    })
}

async fn completed_part_paths(
    transfers_path: &Path,
    session: &UploadSessionRow,
) -> Result<(i64, Vec<PathBuf>), UploadError> {
    let parts = transfer_parts(transfers_path, session).await?;
    if i64::try_from(parts.len()).ok() != Some(session.part_count) {
        return Err(UploadError::UploadSessionMissingParts);
    }
    let mut size_bytes = 0_i64;
    let mut paths = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        let part_number = i64::try_from(index + 1).map_err(|_| UploadError::InvalidPartNumber)?;
        if part.part_number != part_number {
            return Err(UploadError::UploadSessionMissingParts);
        }
        let (expected_offset, expected_size) = expected_part_bounds(session, part.part_number)?;
        if part.offset_bytes != expected_offset || part.size_bytes != expected_size {
            return Err(UploadError::UploadSessionMissingParts);
        }
        size_bytes += part.size_bytes;
        paths.push(PathBuf::from(&part.storage_path));
    }
    Ok((size_bytes, paths))
}

async fn hash_completed_part_paths(
    pool: &SqlitePool,
    session: &UploadSessionRow,
    paths: &[PathBuf],
) -> Result<(String, Vec<String>), UploadError> {
    let mut hasher = Sha256::new();
    let mut part_digests = Vec::with_capacity(paths.len());
    let mut progress = VerificationProgress::new(&session.id, session.total_size);
    for path in paths {
        part_digests.push(hash_file(pool, path, &mut hasher, &mut progress).await?);
    }
    Ok((lower_hex(&hasher.finalize()), part_digests))
}

fn validate_completed_integrity(
    session: &UploadSessionRow,
    digest: &str,
    part_digests: &[String],
    expected_sha256: Option<&str>,
    expected_part_manifest_sha256: Option<&str>,
) -> Result<String, UploadError> {
    if expected_sha256.is_some_and(|expected| digest != expected) {
        return Err(UploadError::UploadChecksumMismatch);
    }
    let actual_part_manifest_sha256 = part_manifest_sha256(session, part_digests)?;
    if let Some(expected) = expected_part_manifest_sha256
        && actual_part_manifest_sha256 != expected
    {
        return Err(UploadError::UploadPartManifestMismatch);
    }
    Ok(actual_part_manifest_sha256)
}

fn part_manifest_sha256(
    session: &UploadSessionRow,
    part_digests: &[String],
) -> Result<String, UploadError> {
    if i64::try_from(part_digests.len()).ok() != Some(session.part_count) {
        return Err(UploadError::UploadSessionMissingParts);
    }
    let mut hasher = Sha256::new();
    hasher.update(
        format!(
            "vault-upload-part-manifest-v1\nsize={}\nchunk={}\nparts={}\n",
            session.total_size, session.chunk_size, session.part_count
        )
        .as_bytes(),
    );
    for (index, digest) in part_digests.iter().enumerate() {
        if !is_sha256_hex(digest) {
            return Err(UploadError::UploadIntegrityDigestInvalid);
        }
        let part_number = i64::try_from(index + 1).map_err(|_| UploadError::InvalidPartNumber)?;
        let (offset, size) = expected_part_bounds(session, part_number)?;
        hasher.update(format!("part={part_number}:{offset}:{size}:{digest}\n").as_bytes());
    }
    Ok(lower_hex(&hasher.finalize()))
}

async fn advance_hash_state(
    pool: &SqlitePool,
    transfers_path: &Path,
    session: &UploadSessionRow,
    state: &Arc<UploadHashState>,
) -> Result<(), UploadError> {
    let session_dir = upload_session_dir(transfers_path, &session.id)?;
    let mut inner = state.inner.lock().await;
    loop {
        let processed_bytes = state.processed_bytes.load(Ordering::Acquire);
        if inner.digest.is_some() {
            return Ok(());
        }
        if inner.next_part > session.part_count {
            if processed_bytes == session.total_size {
                inner.digest = Some(lower_hex(&inner.hasher.clone().finalize()));
                record_upload_verification_progress(pool, &session.id, processed_bytes).await?;
                inner.reported_bytes = processed_bytes;
                return Ok(());
            }
            return Err(UploadError::UploadSessionMissingParts);
        }
        let Some(part) = read_part_metadata(&session_dir, session, inner.next_part).await? else {
            return Ok(());
        };
        let (expected_offset, expected_size) = expected_part_bounds(session, part.part_number)?;
        if part.part_number != inner.next_part
            || part.offset_bytes != expected_offset
            || part.size_bytes != expected_size
        {
            return Err(UploadError::UploadSessionMissingParts);
        }
        hash_part_file_into_state(
            pool,
            session,
            &PathBuf::from(&part.storage_path),
            expected_size,
            state,
            &mut inner,
        )
        .await?;
        inner.next_part += 1;
    }
}

async fn hash_part_file_into_state(
    pool: &SqlitePool,
    session: &UploadSessionRow,
    path: &Path,
    expected_size: i64,
    state: &UploadHashState,
    inner: &mut UploadHashStateInner,
) -> Result<(), UploadError> {
    let mut file = fs::File::open(path).await?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut part_bytes = 0_i64;
    let mut part_hasher = Sha256::new();
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            if part_bytes != expected_size {
                return Err(UploadError::UploadPartSizeMismatch);
            }
            inner.part_digests.push(lower_hex(&part_hasher.finalize()));
            return Ok(());
        }
        part_bytes = part_bytes
            .checked_add(i64::try_from(read).map_err(|_| UploadError::UploadPartTooLarge)?)
            .ok_or(UploadError::UploadPartTooLarge)?;
        if part_bytes > expected_size {
            return Err(UploadError::UploadPartSizeMismatch);
        }
        inner.hasher.update(&buffer[..read]);
        part_hasher.update(&buffer[..read]);
        let processed_bytes = state
            .processed_bytes
            .load(Ordering::Acquire)
            .checked_add(i64::try_from(read).map_err(|_| UploadError::UploadPartTooLarge)?)
            .ok_or(UploadError::UploadSizeMismatch)?
            .min(session.total_size);
        state
            .processed_bytes
            .store(processed_bytes, Ordering::Release);
        if processed_bytes - inner.reported_bytes >= VERIFICATION_PROGRESS_UPDATE_BYTES
            || processed_bytes >= session.total_size
        {
            record_upload_verification_progress(pool, &session.id, processed_bytes).await?;
            inner.reported_bytes = processed_bytes;
        }
    }
}

async fn hash_file(
    pool: &SqlitePool,
    path: &Path,
    hasher: &mut Sha256,
    progress: &mut VerificationProgress<'_>,
) -> Result<String, UploadError> {
    let mut file = fs::File::open(path).await?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut part_hasher = Sha256::new();
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok(lower_hex(&part_hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
        part_hasher.update(&buffer[..read]);
        progress
            .add_bytes(
                pool,
                i64::try_from(read).map_err(|_| UploadError::UploadPartTooLarge)?,
            )
            .await?;
    }
}

struct VerificationProgress<'a> {
    session_id: &'a str,
    total_bytes: i64,
    processed_bytes: i64,
    reported_bytes: i64,
}

impl<'a> VerificationProgress<'a> {
    fn new(session_id: &'a str, total_bytes: i64) -> Self {
        Self {
            session_id,
            total_bytes,
            processed_bytes: 0,
            reported_bytes: 0,
        }
    }

    async fn add_bytes(&mut self, pool: &SqlitePool, bytes: i64) -> Result<(), UploadError> {
        self.processed_bytes = self
            .processed_bytes
            .checked_add(bytes)
            .ok_or(UploadError::UploadSizeMismatch)?
            .min(self.total_bytes);
        if self.processed_bytes - self.reported_bytes >= VERIFICATION_PROGRESS_UPDATE_BYTES
            || self.processed_bytes >= self.total_bytes
        {
            record_upload_verification_progress(pool, self.session_id, self.processed_bytes)
                .await?;
            self.reported_bytes = self.processed_bytes;
        }
        Ok(())
    }
}

fn part_checksum_headers_match(existing: &UploadPartRow, incoming_sha256: Option<&str>) -> bool {
    match (&existing.sha256, incoming_sha256) {
        (Some(existing), Some(incoming)) => existing == &incoming.to_ascii_lowercase(),
        (None, None) => true,
        _ => false,
    }
}

fn part_metadata_matches(
    existing: &UploadPartRow,
    expected_offset: i64,
    expected_size: i64,
    incoming_sha256: Option<&str>,
) -> bool {
    existing.offset_bytes == expected_offset
        && existing.size_bytes == expected_size
        && part_checksum_headers_match(existing, incoming_sha256)
}

async fn promote_part_file(temp_path: &Path, final_path: &Path) -> Result<bool, UploadError> {
    match fs::hard_link(temp_path, final_path).await {
        Ok(()) => {
            let _ = fs::remove_file(temp_path).await;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(temp_path).await;
            Ok(false)
        }
        Err(error) => {
            let _ = fs::remove_file(temp_path).await;
            Err(error.into())
        }
    }
}

async fn write_part_stream<S, E>(
    temp_path: &Path,
    expected_size: i64,
    expected_sha256: Option<&str>,
    stream: &mut S,
) -> Result<(), UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    if expected_size <= SMALL_PART_MEMORY_BUFFER_BYTES {
        return write_small_part_chunked(temp_path, expected_size, expected_sha256, stream).await;
    }

    let mut file = fs::File::create(temp_path).await?;
    let mut hasher = expected_sha256.map(|_| Sha256::new());
    let mut size_bytes = 0_i64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| UploadError::UploadReadFailed)?;
        if chunk.is_empty() {
            continue;
        }
        size_bytes += i64::try_from(chunk.len()).map_err(|_| UploadError::UploadPartTooLarge)?;
        if size_bytes > expected_size {
            return Err(UploadError::UploadPartTooLarge);
        }
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&chunk);
        }
        file.write_all(&chunk).await?;
    }
    if size_bytes != expected_size {
        return Err(UploadError::UploadPartSizeMismatch);
    }
    if let (Some(expected), Some(hasher)) = (expected_sha256, hasher) {
        let actual_sha256 = lower_hex(&hasher.finalize());
        if actual_sha256 != expected.to_ascii_lowercase() {
            return Err(UploadError::UploadPartChecksumMismatch);
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

async fn write_small_part_chunked<S, E>(
    temp_path: &Path,
    expected_size: i64,
    expected_sha256: Option<&str>,
    stream: &mut S,
) -> Result<(), UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    // High-fanout medium uploads showed better local throughput when each 4 MiB
    // request is collected once and written by a blocking file writer. Larger
    // parts stay on the streaming async path to avoid unbounded per-request RAM.
    let chunks = read_small_part_chunks(expected_size, stream).await?;
    let writer_path = temp_path.to_path_buf();
    let expected_sha256 = expected_sha256.map(str::to_ascii_lowercase);
    tokio::task::spawn_blocking(move || {
        write_chunked_part_blocking(writer_path, expected_size, expected_sha256, &chunks)
    })
    .await
    .map_err(|_| UploadError::UploadReadFailed)??;
    Ok(())
}

async fn read_small_part_chunks<S, E>(
    expected_size: i64,
    stream: &mut S,
) -> Result<Vec<Bytes>, UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    let mut chunks = Vec::new();
    let mut size_bytes = 0_i64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| UploadError::UploadReadFailed)?;
        if chunk.is_empty() {
            continue;
        }
        size_bytes += i64::try_from(chunk.len()).map_err(|_| UploadError::UploadPartTooLarge)?;
        if size_bytes > expected_size {
            return Err(UploadError::UploadPartTooLarge);
        }
        chunks.push(chunk);
    }
    if size_bytes != expected_size {
        return Err(UploadError::UploadPartSizeMismatch);
    }
    Ok(chunks)
}

fn write_chunked_part_blocking(
    path: PathBuf,
    expected_size: i64,
    expected_sha256: Option<String>,
    chunks: &[Bytes],
) -> Result<(), UploadError> {
    let size_bytes = chunks.iter().try_fold(0_i64, |total, chunk| {
        let chunk_len = i64::try_from(chunk.len()).map_err(|_| UploadError::UploadPartTooLarge)?;
        total
            .checked_add(chunk_len)
            .ok_or(UploadError::UploadPartTooLarge)
    })?;
    if size_bytes != expected_size {
        return Err(UploadError::UploadPartSizeMismatch);
    }
    if let Some(expected) = expected_sha256 {
        let mut hasher = Sha256::new();
        for chunk in chunks {
            hasher.update(chunk);
        }
        if lower_hex(&hasher.finalize()) != expected {
            return Err(UploadError::UploadPartChecksumMismatch);
        }
    }
    let mut file = std::fs::File::create(path)?;
    for chunk in chunks {
        file.write_all(chunk)?;
    }
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

async fn upload_session_payload(
    pool: &SqlitePool,
    transfers_path: &Path,
    signing_keys: &SigningKeyring,
    session_id: &str,
) -> Result<UploadSessionPayload, UploadError> {
    let session = fetch_upload_session(pool, session_id)
        .await?
        .ok_or(UploadError::UploadSessionNotFound)?;
    let parts = transfer_parts(transfers_path, &session).await?;
    let uploaded_parts = parts
        .iter()
        .map(|part| UploadPartPayload {
            part_number: part.part_number,
            offset: part.offset_bytes,
            size_bytes: part.size_bytes,
            sha256: part.sha256.clone(),
        })
        .collect::<Vec<_>>();
    let uploaded_bytes = uploaded_parts.iter().map(|part| part.size_bytes).sum();
    let verification = if session.status == "complete" {
        Some(UploadVerificationPayload {
            processed_bytes: session.total_size,
            total_bytes: session.total_size,
        })
    } else if session.status == "completing" && session.verification_total_bytes > 0 {
        Some(UploadVerificationPayload {
            processed_bytes: session
                .verification_processed_bytes
                .min(session.verification_total_bytes),
            total_bytes: session.verification_total_bytes,
        })
    } else {
        None
    };
    let result = if session.result_document_id.is_some()
        || session.result_version_id.is_some()
        || session.result_path.is_some()
    {
        Some(completed_result(&session)?)
    } else {
        None
    };
    let part_manifest_sha256 = if session.status == "complete" {
        session.part_manifest_sha256.clone()
    } else {
        None
    };
    let upload_token = upload_session_token(signing_keys, &session)?;
    Ok(UploadSessionPayload {
        id: session.id,
        mode: session.mode,
        status: session.status,
        filename: session.filename,
        size_bytes: session.total_size,
        chunk_size: session.chunk_size,
        part_count: session.part_count,
        uploaded_bytes,
        uploaded_parts,
        verification,
        expires_at: Some(session.expires_at.clone()),
        result,
        resume_identity_sha256: session.resume_identity_sha256.clone(),
        part_manifest_sha256,
        upload_token,
    })
}

async fn fetch_upload_session(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<UploadSessionRow>, UploadError> {
    Ok(sqlx::query_as::<_, UploadSessionRow>(
        r"
        SELECT
            id,
            mode,
            status,
            folder_path,
            document_id,
            filename,
            total_size,
            chunk_size,
            part_count,
            verification_total_bytes,
            verification_processed_bytes,
            mime_type,
            note,
            rename_to_upload,
            created_by,
            created_by_name,
            upload_ip,
            upload_user_agent,
            expires_at,
            result_document_id,
            result_version_id,
            result_path,
            json_extract(user_context, '$._upload_resume_identity_sha256')
                AS resume_identity_sha256,
            json_extract(user_context, '$.' || ?)
                AS part_manifest_sha256
        FROM upload_sessions
        WHERE id = ?
        ",
    )
    .bind(UPLOAD_PART_MANIFEST_CONTEXT_KEY)
    .bind(session_id)
    .fetch_optional(pool)
    .await?)
}

async fn transfer_parts(
    transfers_path: &Path,
    session: &UploadSessionRow,
) -> Result<Vec<UploadPartRow>, UploadError> {
    let session_dir = upload_session_dir(transfers_path, &session.id)?;
    let mut parts = Vec::new();
    for part_number in 1..=session.part_count {
        if let Some(part) = read_part_metadata(&session_dir, session, part_number).await? {
            parts.push(part);
        }
    }
    Ok(parts)
}

async fn read_part_metadata(
    session_dir: &Path,
    session: &UploadSessionRow,
    part_number: i64,
) -> Result<Option<UploadPartRow>, UploadError> {
    let metadata_path = part_metadata_path(session_dir, part_number);
    let storage_path = part_file_path(session_dir, part_number);
    let (offset_bytes, size_bytes) = expected_part_bounds(session, part_number)?;
    if !valid_upload_part_file(&storage_path, size_bytes).await? {
        return Ok(None);
    }
    let inferred = UploadPartRow {
        part_number,
        offset_bytes,
        size_bytes,
        sha256: None,
        storage_path: storage_path.to_string_lossy().to_string(),
    };
    let sidecar_metadata = match fs::symlink_metadata(&metadata_path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(inferred)),
        Err(error) => return Err(error.into()),
    };
    if sidecar_metadata.file_type().is_symlink()
        || !sidecar_metadata.is_file()
        || sidecar_metadata.len() > MAX_UPLOAD_PART_METADATA_BYTES
    {
        return Ok(Some(inferred));
    }
    let metadata_bytes = match fs::read(&metadata_path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(inferred)),
        Err(error) => return Err(error.into()),
    };
    let Ok(mut metadata) = serde_json::from_slice::<UploadPartMetadata>(&metadata_bytes) else {
        return Ok(Some(inferred));
    };
    if metadata.part_number != part_number
        || metadata.offset_bytes != offset_bytes
        || metadata.size_bytes != size_bytes
        || metadata
            .sha256
            .as_deref()
            .is_some_and(|sha256| !is_sha256_hex(sha256))
    {
        return Ok(Some(inferred));
    }
    metadata.sha256 = metadata.sha256.map(|sha256| sha256.to_ascii_lowercase());
    Ok(Some(UploadPartRow {
        part_number: metadata.part_number,
        offset_bytes: metadata.offset_bytes,
        size_bytes: metadata.size_bytes,
        sha256: metadata.sha256,
        storage_path: storage_path.to_string_lossy().to_string(),
    }))
}

async fn valid_upload_part_file(path: &Path, expected_size: i64) -> Result<bool, UploadError> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(!metadata.file_type().is_symlink()
        && metadata.is_file()
        && u64::try_from(expected_size).ok() == Some(metadata.len()))
}

async fn hash_existing_upload_part(path: &Path, expected_size: i64) -> Result<String, UploadError> {
    if !valid_upload_part_file(path, expected_size).await? {
        return Err(UploadError::UploadPartConflict);
    }
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut size_bytes = 0_i64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        size_bytes = size_bytes
            .checked_add(i64::try_from(read).map_err(|_| UploadError::UploadPartTooLarge)?)
            .ok_or(UploadError::UploadPartTooLarge)?;
        if size_bytes > expected_size {
            return Err(UploadError::UploadPartConflict);
        }
        hasher.update(&buffer[..read]);
    }
    if size_bytes != expected_size {
        return Err(UploadError::UploadPartConflict);
    }
    Ok(lower_hex(&hasher.finalize()))
}

async fn write_part_metadata(session_dir: &Path, part: &UploadPartRow) -> Result<(), UploadError> {
    let metadata_path = part_metadata_path(session_dir, part.part_number);
    let temp_path = metadata_path.with_extension(format!("json.tmp-{}", Uuid::new_v4().simple()));
    let metadata = UploadPartMetadata {
        part_number: part.part_number,
        offset_bytes: part.offset_bytes,
        size_bytes: part.size_bytes,
        sha256: part.sha256.clone(),
    };
    let mut bytes = serde_json::to_vec(&metadata)?;
    bytes.push(b'\n');
    let write_result: Result<(), UploadError> = async {
        let mut file = fs::File::create(&temp_path).await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        rename_or_replace_part_metadata(&temp_path, &metadata_path).await?;
        Ok(())
    }
    .await;
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    write_result
}

#[cfg(unix)]
async fn rename_or_replace_part_metadata(source: &Path, target: &Path) -> Result<(), UploadError> {
    fs::rename(source, target).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn rename_or_replace_part_metadata(source: &Path, target: &Path) -> Result<(), UploadError> {
    match fs::rename(source, target).await {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            let metadata = match fs::symlink_metadata(target).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(rename_error.into());
                }
                Err(error) => return Err(error.into()),
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(rename_error.into());
            }
            // This is only checksum metadata; the already-synced final part is
            // authoritative. If power is lost in this replace gap, the missing
            // sidecar is detected and reconstructed from that part on retry.
            fs::remove_file(target).await?;
            fs::rename(source, target).await?;
            Ok(())
        }
    }
}

#[cfg(not(windows))]
async fn sync_file(path: &Path) -> Result<(), UploadError> {
    fs::File::open(path).await?.sync_all().await?;
    Ok(())
}

#[cfg(windows)]
async fn sync_file(path: &Path) -> Result<(), UploadError> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await?
        .sync_all()
        .await?;
    Ok(())
}

#[cfg(unix)]
async fn sync_directory(path: &Path) -> Result<(), UploadError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
        .await
        .map_err(|error| UploadError::Io(std::io::Error::other(error)))??;
    Ok(())
}

#[cfg(windows)]
async fn sync_directory(path: &Path) -> Result<(), UploadError> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?
            .sync_all()
    })
    .await
    .map_err(|error| UploadError::Io(std::io::Error::other(error)))??;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
async fn sync_directory(_path: &Path) -> Result<(), UploadError> {
    Err(UploadError::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "durable directory synchronization is unsupported on this platform",
    )))
}

fn part_file_path(session_dir: &Path, part_number: i64) -> PathBuf {
    session_dir.join(format!("{part_number:08}.part"))
}

fn part_metadata_path(session_dir: &Path, part_number: i64) -> PathBuf {
    session_dir.join(format!("{part_number:08}.json"))
}

async fn editable_document_for_upload(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<DocumentRecord, UploadError> {
    Ok(editable_document_for_write(pool, document_id, user).await?)
}

async fn active_upload_lock(
    pool: &SqlitePool,
    document_id: i64,
) -> Result<Option<ActiveUploadLockRow>, UploadError> {
    Ok(sqlx::query_as::<_, ActiveUploadLockRow>(
        r"
        SELECT id, locked_by
        FROM document_locks
        WHERE document_id = ? AND is_active = 1
        ",
    )
    .bind(document_id)
    .fetch_optional(pool)
    .await?)
}

async fn active_upload_lock_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
) -> Result<Option<ActiveUploadLockRow>, UploadError> {
    Ok(sqlx::query_as::<_, ActiveUploadLockRow>(
        r"
        SELECT id, locked_by
        FROM document_locks
        WHERE document_id = ? AND is_active = 1
        ",
    )
    .bind(document_id)
    .fetch_optional(&mut **transaction)
    .await?)
}

async fn ensure_unique_document_name_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    folder_id: i64,
    filename: &str,
    except_document_id: Option<i64>,
) -> Result<(), UploadError> {
    let duplicate = sqlx::query_scalar::<_, i64>(
        r"
        SELECT id
        FROM documents
        WHERE folder_id = ?
          AND name = ?
          AND archived_from_folder IS NULL
          AND (? IS NULL OR id != ?)
        LIMIT 1
        ",
    )
    .bind(folder_id)
    .bind(filename)
    .bind(except_document_id)
    .bind(except_document_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if duplicate.is_some() {
        return Err(DocumentError::DocumentPathAlreadyExists.into());
    }
    Ok(())
}

async fn next_version_number_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
) -> Result<i64, UploadError> {
    Ok(sqlx::query_scalar::<_, i64>(
        r"
        SELECT COALESCE(MAX(version_number), 0) + 1
        FROM document_versions
        WHERE document_id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&mut **transaction)
    .await?)
}

async fn record_document_event_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
    session: &UploadSessionRow,
    event_type: &str,
    message: &str,
) -> Result<(), UploadError> {
    sqlx::query(
        r"
        INSERT INTO document_events
            (document_id, event_type, actor, actor_name, message, result, ip, user_agent)
        VALUES
            (?, ?, ?, ?, ?, 'ok', ?, ?)
        ",
    )
    .bind(document_id)
    .bind(event_type)
    .bind(&session.created_by)
    .bind(session.created_by_name.as_deref())
    .bind(message)
    .bind(session.upload_ip.as_deref())
    .bind(session.upload_user_agent.as_deref())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn record_state_event_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    event_type: &str,
    resources: &[&str],
) -> Result<(), UploadError> {
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES (?, ?)
        ",
    )
    .bind(event_type)
    .bind(state_event_resources_json(resources))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn mark_upload_completing(
    pool: &SqlitePool,
    session_id: &str,
    total_bytes: i64,
    processed_bytes: i64,
) -> Result<(), UploadError> {
    let result = sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'completing',
            verification_total_bytes = ?,
            verification_processed_bytes = ?,
            updated_at = ?
        WHERE id = ? AND status = 'active'
        ",
    )
    .bind(total_bytes)
    .bind(processed_bytes.clamp(0, total_bytes))
    .bind(now_rfc3339()?)
    .bind(session_id)
    .execute(pool)
    .await?;
    if result.rows_affected() > 0 {
        return Ok(());
    }
    let current = fetch_upload_session(pool, session_id)
        .await?
        .ok_or(UploadError::UploadSessionNotFound)?;
    Err(UploadError::UploadSessionStatus(current.status))
}

async fn claim_upload_completion(
    pool: &SqlitePool,
    transfers_path: &Path,
    session_id: &str,
    total_bytes: i64,
    processed_bytes: i64,
    hash_coordinator: Option<&UploadHashCoordinator>,
) -> Result<UploadCompletionAttempt, UploadError> {
    let pool = pool.clone();
    let transfers_path = transfers_path.to_path_buf();
    let session_id = session_id.to_string();
    let hash_coordinator = hash_coordinator.cloned();
    // SQLx work can finish after the request future is cancelled. Keep the
    // claim in an owned task so a successful state transition always produces
    // a guard. If the waiting request disappears, Tokio drops the task output
    // and the guard schedules its status-predicated recovery.
    tokio::spawn(async move {
        mark_upload_completing(&pool, &session_id, total_bytes, processed_bytes).await?;
        Ok::<_, UploadError>(UploadCompletionAttempt::new(
            &pool,
            &transfers_path,
            &session_id,
            hash_coordinator.as_ref(),
        ))
    })
    .await
    .map_err(|error| UploadError::CompletionStateTransition(error.to_string()))?
}

async fn record_upload_verification_progress(
    pool: &SqlitePool,
    session_id: &str,
    processed_bytes: i64,
) -> Result<bool, UploadError> {
    // Background upload hashing can begin while the session is still active. The
    // guarded update lets that same verifier start publishing progress as soon
    // as completion flips the session to `completing`, without writing noisy
    // upload-time progress rows or regressing a newer byte count.
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET verification_processed_bytes = ?,
            updated_at = ?
        WHERE id = ?
            AND status = 'completing'
            AND verification_processed_bytes < ?
        ",
    )
    .bind(processed_bytes)
    .bind(now_rfc3339()?)
    .bind(session_id)
    .bind(processed_bytes)
    .execute(pool)
    .await
    .map(|result| result.rows_affected() > 0)
    .map_err(UploadError::Database)
}

async fn reset_upload_completion_for_retry(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<(), UploadError> {
    let result = sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'active',
            verification_total_bytes = 0,
            verification_processed_bytes = 0,
            error = NULL,
            updated_at = ?
        WHERE id = ? AND status = 'completing'
        ",
    )
    .bind(now_rfc3339()?)
    .bind(session_id)
    .execute(pool)
    .await?;
    if result.rows_affected() > 0 {
        return Ok(());
    }
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM upload_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(pool)
            .await?;
    match status.as_deref() {
        Some("active" | "complete" | "failed" | "aborted" | "expired") => Ok(()),
        Some(status) => Err(UploadError::CompletionStateTransition(format!(
            "session {session_id} remained {status} after retry recovery"
        ))),
        None => Err(UploadError::UploadSessionNotFound),
    }
}

async fn ensure_active_session(
    pool: &SqlitePool,
    transfers_path: &Path,
    session: &UploadSessionRow,
) -> Result<(), UploadError> {
    if session.status != "active" {
        return Err(UploadError::UploadSessionStatus(session.status.clone()));
    }
    ensure_session_not_expired(pool, transfers_path, session).await
}

async fn ensure_session_not_expired(
    pool: &SqlitePool,
    transfers_path: &Path,
    session: &UploadSessionRow,
) -> Result<(), UploadError> {
    let now = OffsetDateTime::now_utc();
    if OffsetDateTime::parse(&session.expires_at, &Rfc3339)? > now {
        return Ok(());
    }
    let now = now.format(&Rfc3339)?;
    let expired = sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'expired',
            updated_at = ?
        WHERE id = ?
          AND status = 'active'
          AND datetime(expires_at) <= datetime(?)
        ",
    )
    .bind(&now)
    .bind(&session.id)
    .bind(&now)
    .execute(pool)
    .await?;
    if expired.rows_affected() > 0 {
        clear_upload_session_files(transfers_path, &session.id).await;
        return Err(UploadError::UploadSessionExpired);
    }
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM upload_sessions WHERE id = ?")
            .bind(&session.id)
            .fetch_optional(pool)
            .await?;
    match status.as_deref() {
        Some("expired") => {
            clear_upload_session_files(transfers_path, &session.id).await;
            Err(UploadError::UploadSessionExpired)
        }
        Some("active") => Ok(()),
        Some(status) => Err(UploadError::UploadSessionStatus(status.to_string())),
        None => Err(UploadError::UploadSessionNotFound),
    }
}

async fn mark_upload_failed(
    pool: &SqlitePool,
    transfers_path: &Path,
    session_id: &str,
    message: &str,
) -> Result<(), UploadError> {
    let failed = sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'failed',
            error = ?,
            updated_at = ?
        WHERE id = ? AND status = 'completing'
        ",
    )
    .bind(message)
    .bind(now_rfc3339()?)
    .bind(session_id)
    .execute(pool)
    .await?;
    if failed.rows_affected() > 0 {
        clear_upload_session_files(transfers_path, session_id).await;
    }
    Ok(())
}

fn require_transfer_owner(
    session: &UploadSessionRow,
    user: &UserContext,
) -> Result<(), UploadError> {
    if session.created_by == user.id || user.is_admin {
        Ok(())
    } else {
        Err(UploadError::TransferNotFound)
    }
}

fn require_part_authorization(
    session: &UploadSessionRow,
    authorization: PartAuthorization<'_>,
) -> Result<(), UploadError> {
    match authorization {
        PartAuthorization::User(user) => require_transfer_owner(session, user),
        PartAuthorization::OwnerId(owner_id) => {
            if session.created_by == owner_id {
                Ok(())
            } else {
                Err(UploadError::TransferNotFound)
            }
        }
    }
}

pub fn verify_upload_token(
    signing_keys: &SigningKeyring,
    token: &str,
    session_id: &str,
) -> Result<String, UploadError> {
    Ok(verify_upload_token_claims(signing_keys, token, session_id)?.owner_id)
}

pub fn verify_upload_token_claims(
    signing_keys: &SigningKeyring,
    token: &str,
    session_id: &str,
) -> Result<UploadPartTokenClaims, UploadError> {
    if token.is_empty() || !token.contains('.') {
        return Err(UploadError::UploadTokenRequired);
    }
    let (body, signature) = token
        .rsplit_once('.')
        .ok_or(UploadError::UploadTokenRequired)?;
    if !body.is_ascii() || !signature.is_ascii() {
        return Err(UploadError::UploadTokenInvalid);
    }
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(signature.as_bytes())
        .map_err(|_| UploadError::UploadTokenInvalid)?;
    if !signing_keys.verify_upload(body.as_bytes(), &signature_bytes) {
        return Err(UploadError::UploadTokenInvalid);
    }
    let body_bytes = URL_SAFE_NO_PAD
        .decode(body.as_bytes())
        .map_err(|_| UploadError::UploadTokenInvalid)?;
    let Value::Object(payload) = serde_json::from_slice::<Value>(&body_bytes)
        .map_err(|_| UploadError::UploadTokenInvalid)?
    else {
        return Err(UploadError::UploadTokenInvalid);
    };
    if string_value(&payload, "typ") != Some("upload-part")
        || string_value(&payload, "sid") != Some(session_id)
    {
        return Err(UploadError::UploadTokenWrongSession);
    }
    let expires_at = payload
        .get("exp")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or(UploadError::UploadTokenExpired)?;
    if expires_at < unix_timestamp_now() {
        return Err(UploadError::UploadTokenExpired);
    }
    let owner = string_value(&payload, "owner")
        .filter(|owner| !owner.is_empty())
        .ok_or(UploadError::UploadTokenInvalid)?;
    let mode = string_value(&payload, "mode")
        .filter(|mode| matches!(*mode, "create" | "checkin"))
        .ok_or(UploadError::UploadTokenInvalid)?;
    let filename = string_value(&payload, "name")
        .filter(|filename| !filename.is_empty())
        .ok_or(UploadError::UploadTokenInvalid)?;
    let total_size = integer_value(&payload, "size").ok_or(UploadError::UploadTokenInvalid)?;
    let chunk_size = integer_value(&payload, "chunk").ok_or(UploadError::UploadTokenInvalid)?;
    let part_count = integer_value(&payload, "parts").ok_or(UploadError::UploadTokenInvalid)?;
    let expires_at = string_value(&payload, "expires_at")
        .filter(|expires_at| !expires_at.is_empty())
        .ok_or(UploadError::UploadTokenInvalid)?;
    let resume_identity_sha256 = match payload.get("resume") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if is_sha256_hex(value) => Some(value.to_ascii_lowercase()),
        Some(_) => return Err(UploadError::UploadTokenInvalid),
    };
    Ok(UploadPartTokenClaims {
        session_id: session_id.to_string(),
        owner_id: owner.to_string(),
        mode: mode.to_string(),
        filename: filename.to_string(),
        total_size,
        chunk_size,
        part_count,
        expires_at: expires_at.to_string(),
        resume_identity_sha256,
    })
}

fn upload_session_token(
    signing_keys: &SigningKeyring,
    session: &UploadSessionRow,
) -> Result<Option<String>, UploadError> {
    if !matches!(session.status.as_str(), "active" | "completing") {
        return Ok(None);
    }
    Ok(Some(sign_upload_token(signing_keys, session)?))
}

fn sign_upload_token(
    signing_keys: &SigningKeyring,
    session: &UploadSessionRow,
) -> Result<String, UploadError> {
    let expires_timestamp = OffsetDateTime::parse(&session.expires_at, &Rfc3339)?.unix_timestamp();
    let payload = json!({
        "exp": expires_timestamp,
        "expires_at": session.expires_at,
        "owner": session.created_by,
        "sid": session.id,
        "mode": session.mode,
        "name": session.filename,
        "size": session.total_size,
        "chunk": session.chunk_size,
        "parts": session.part_count,
        "resume": session.resume_identity_sha256,
        "typ": "upload-part",
    });
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload)?);
    let signature = signing_keys
        .sign_upload(body.as_bytes())
        .map(|signature| URL_SAFE_NO_PAD.encode(signature))
        .ok_or(UploadError::UploadTokenInvalid)?;
    Ok(format!("{body}.{signature}"))
}

fn string_value<'a>(payload: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn integer_value(payload: &Map<String, Value>, key: &str) -> Option<i64> {
    payload.get(key).and_then(Value::as_i64)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalize_optional_sha256(value: Option<&str>) -> Result<Option<String>, UploadError> {
    value
        .map(|value| {
            if is_sha256_hex(value) {
                Ok(value.to_ascii_lowercase())
            } else {
                Err(UploadError::UploadIntegrityDigestInvalid)
            }
        })
        .transpose()
}

fn unix_timestamp_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn expected_part_bounds(
    session: &UploadSessionRow,
    part_number: i64,
) -> Result<(i64, i64), UploadError> {
    if part_number < 1 || part_number > session.part_count {
        return Err(UploadError::InvalidPartNumber);
    }
    let offset = (part_number - 1) * session.chunk_size;
    let size = session
        .total_size
        .saturating_sub(offset)
        .min(session.chunk_size);
    Ok((offset, size))
}

fn completed_result(session: &UploadSessionRow) -> Result<UploadResultPayload, UploadError> {
    let id = session
        .result_document_id
        .ok_or(UploadError::CompletedSessionMissingResult)?;
    let version = session
        .result_version_id
        .clone()
        .ok_or(UploadError::CompletedSessionMissingResult)?;
    let path = session
        .result_path
        .clone()
        .ok_or(UploadError::CompletedSessionMissingResult)?;
    Ok(UploadResultPayload { id, version, path })
}

fn completed_status_result(
    session: &UploadSessionStatusRow,
) -> Result<UploadResultPayload, UploadError> {
    let id = session
        .result_document_id
        .ok_or(UploadError::CompletedSessionMissingResult)?;
    let version = session
        .result_version_id
        .clone()
        .ok_or(UploadError::CompletedSessionMissingResult)?;
    let path = session
        .result_path
        .clone()
        .ok_or(UploadError::CompletedSessionMissingResult)?;
    Ok(UploadResultPayload { id, version, path })
}

fn normalize_upload_mode(mode: &str) -> Result<String, UploadError> {
    match mode {
        "create" => Ok("create".to_string()),
        "checkin" => Ok("checkin".to_string()),
        _ => Err(UploadError::UnsupportedUploadSessionMode),
    }
}

fn ensure_upload_folder(folder: &str) -> Result<(), UploadError> {
    if parse_public_folder_path(Some(folder))?.root_key == "archive" {
        return Err(UploadError::UploadNewDocumentsToVault);
    }
    Ok(())
}

fn choose_upload_chunk_size(
    size_bytes: i64,
    client_upload_parallelism: Option<i64>,
    transfer_chunk_bytes: i64,
) -> i64 {
    let max_chunk = transfer_chunk_bytes.clamp(1, UPLOAD_MAX_INTEGRITY_CHUNK_BYTES);
    let target_parallelism = upload_parallelism_target(client_upload_parallelism);
    if size_bytes <= 0 {
        return max_chunk;
    }
    if size_bytes <= max_chunk {
        return size_bytes.max(1);
    }
    let full_size_parts = positive_div_ceil(size_bytes, max_chunk);
    if full_size_parts >= target_parallelism {
        return max_chunk;
    }
    let min_chunk = max_chunk.min(UPLOAD_MIN_ADAPTIVE_CHUNK_BYTES);
    let round_to = max_chunk.min(UPLOAD_CHUNK_ROUNDING_BYTES);
    let target_parts = if size_bytes <= UPLOAD_SMALL_ADAPTIVE_MAX_BYTES {
        target_parallelism
    } else {
        let target_chunk = target_upload_chunk_bytes(target_parallelism);
        let target_parts = positive_div_ceil(size_bytes, target_chunk);
        target_parallelism.min(UPLOAD_MIN_ADAPTIVE_PARTS.max(target_parts))
    };
    let target_chunk = positive_div_ceil(size_bytes, target_parts);
    let rounded = positive_div_ceil(target_chunk, round_to).saturating_mul(round_to);
    max_chunk.min(min_chunk.max(rounded))
}

fn upload_parallelism_target(client_upload_parallelism: Option<i64>) -> i64 {
    client_upload_parallelism.map_or(UPLOAD_DEFAULT_ADAPTIVE_PARTS, |parallelism| {
        UPLOAD_MAX_ADAPTIVE_PARTS.min(UPLOAD_MIN_ADAPTIVE_PARTS.max(parallelism))
    })
}

fn target_upload_chunk_bytes(parallelism: i64) -> i64 {
    if parallelism >= UPLOAD_MAX_ADAPTIVE_PARTS {
        return UPLOAD_TARGET_ADAPTIVE_CHUNK_BYTES;
    }
    UPLOAD_TARGET_ADAPTIVE_CHUNK_BYTES.max(
        (UPLOAD_TARGET_ADAPTIVE_CHUNK_BYTES * UPLOAD_MAX_ADAPTIVE_PARTS + parallelism - 1)
            / parallelism,
    )
}

fn part_count(size_bytes: i64, chunk_size: i64) -> i64 {
    if size_bytes <= 0 {
        0
    } else {
        positive_div_ceil(size_bytes, chunk_size)
    }
}

fn positive_div_ceil(value: i64, divisor: i64) -> i64 {
    if value <= 0 {
        0
    } else {
        value / divisor + i64::from(value % divisor != 0)
    }
}

fn upload_session_dir(transfers_path: &Path, session_id: &str) -> Result<PathBuf, UploadError> {
    if session_id.is_empty()
        || !session_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return Err(UploadError::UploadSessionNotFound);
    }
    Ok(transfers_path.join("uploads").join(session_id))
}

async fn create_durable_upload_session_dir(
    transfers_path: &Path,
    session_dir: &Path,
) -> Result<(), UploadError> {
    fs::create_dir_all(session_dir).await?;
    // The database row makes this directory part of the durable upload state.
    // Persist the new directory and each containing entry before publishing an
    // active session that can accept parts.
    sync_directory(session_dir).await?;
    sync_directory(&transfers_path.join("uploads")).await?;
    sync_directory(transfers_path).await?;
    if let Some(parent) = transfers_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        sync_directory(parent).await?;
    }
    Ok(())
}

pub async fn clear_upload_session_files(transfers_path: &Path, session_id: &str) {
    let Ok(path) = upload_session_dir(transfers_path, session_id) else {
        return;
    };
    let upload_root = transfers_path.join("uploads");
    if !real_directory(&upload_root).await {
        tracing::warn!(
            path = %upload_root.display(),
            "refusing upload cleanup because the upload root is not a real directory"
        );
        return;
    }
    let metadata = match fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(?error, path = %path.display(), "could not inspect upload directory");
            return;
        }
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        tracing::warn!(
            path = %path.display(),
            "refusing to remove upload path that is not a real directory"
        );
        return;
    }
    if !real_directory(&upload_root).await {
        return;
    }
    match fs::remove_dir_all(&path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            ?error,
            path = %path.display(),
            "could not remove terminal upload directory"
        ),
    }
}

async fn real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .await
        .is_ok_and(|metadata| metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
}

fn sanitize_mime_type(mime_type: Option<&str>, filename: &str) -> String {
    let fallback = mime_from_filename(filename);
    let candidate = mime_type.unwrap_or(&fallback).trim();
    if candidate.is_empty()
        || candidate
            .chars()
            .any(|character| character < ' ' || character == '\u{7f}' || !character.is_ascii())
    {
        fallback
    } else {
        candidate.to_string()
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

fn trim_to_option(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn default_upload_mode() -> String {
    "create".to_string()
}

fn now_rfc3339() -> Result<String, UploadError> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

fn new_version_id() -> String {
    let now = OffsetDateTime::now_utc();
    let uuid = Uuid::new_v4().simple().to_string();
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}{:06}-{}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.microsecond(),
        &uuid[..8],
    )
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
