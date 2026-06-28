use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{Stream, StreamExt};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::auth::UserContext;
use crate::documents::{
    ClientMeta, DocumentError, DocumentRecord, document_path, editable_document_for_write,
    normalize_file_name,
};
use crate::folders::{
    FolderError, apply_effective_ttl_to_document_in_tx, get_or_create_folder_path_in_tx, join_path,
    normalize_folder, parse_public_folder_path, require_write_for_folder_path,
};
use crate::state_events::state_event_resources_json;
use crate::storage::{BlobStorageBackend, StorageError, StoredBlob};

type HmacSha256 = Hmac<Sha256>;

const MAX_UPLOAD_BYTES: i64 = 5 * 1024 * 1024 * 1024;
const TRANSFER_CHUNK_BYTES: i64 = 32 * 1024 * 1024;
const TRANSFER_SESSION_TTL_SECONDS: i64 = 86_400;
const UPLOAD_MIN_ADAPTIVE_PARTS: i64 = 4;
const UPLOAD_DEFAULT_ADAPTIVE_PARTS: i64 = 16;
const UPLOAD_MAX_ADAPTIVE_PARTS: i64 = 16;
const UPLOAD_SMALL_ADAPTIVE_MAX_BYTES: i64 = 48 * 1024 * 1024;
const UPLOAD_TARGET_ADAPTIVE_CHUNK_BYTES: i64 = 8 * 1024 * 1024;
const UPLOAD_MIN_ADAPTIVE_CHUNK_BYTES: i64 = 4 * 1024 * 1024;
const UPLOAD_CHUNK_ROUNDING_BYTES: i64 = 1024 * 1024;
const SMALL_PART_MEMORY_BUFFER_BYTES: i64 = 8 * 1024 * 1024;
const VERIFICATION_PROGRESS_UPDATE_BYTES: i64 = 32 * 1024 * 1024;

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
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompleteUploadRequest {
    pub sha256: Option<String>,
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
    pub upload_token: Option<String>,
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
    #[error("upload size does not match session")]
    UploadSizeMismatch,
    #[error("storage location points at another blob")]
    StorageLocationConflict,
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
    size_bytes: i64,
    paths: Vec<PathBuf>,
}

pub async fn create_upload_session(
    pool: &SqlitePool,
    transfers_path: &Path,
    token_secret: &str,
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
    let chunk_size = choose_upload_chunk_size(
        payload.size_bytes,
        payload.client_upload_parallelism,
        settings.transfer_chunk_bytes,
    );
    let part_count = part_count(payload.size_bytes, chunk_size);
    let (folder_path, document_id) =
        prepare_upload_target(pool, &mode, &filename, &payload, user).await?;

    let session_id = Uuid::new_v4().simple().to_string();
    let session_dir = upload_session_dir(transfers_path, &session_id)?;
    fs::create_dir_all(&session_dir).await?;
    let now = now_rfc3339()?;
    let expires_at = (OffsetDateTime::now_utc()
        + Duration::seconds(settings.transfer_session_ttl_seconds))
    .format(&Rfc3339)?;
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
    .bind(serde_json::to_string(user)?)
    .bind(&meta.ip)
    .bind(&meta.user_agent)
    .bind(&now)
    .bind(&now)
    .bind(&expires_at)
    .execute(pool)
    .await?;
    upload_session_payload(pool, transfers_path, token_secret, &session_id).await
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
    token_secret: &str,
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
    upload_session_payload(pool, transfers_path, token_secret, session_id).await
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
    ingest: UploadPartIngest<'_>,
    token_claims: UploadPartTokenClaims,
    stream: S,
) -> Result<(), UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    ingest_upload_part_for_session(ingest, token_claims.into_session_row(), stream).await
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
        if let Some(existing) =
            read_part_metadata(&session_dir, &session, ingest.part_number).await?
            && part_metadata_matches(
                &existing,
                expected_offset,
                expected_size,
                ingest.headers.sha256,
            )
        {
            return Ok(());
        }
        return Err(UploadError::UploadPartConflict);
    }
    if part.sha256.is_some() {
        write_part_metadata(&session_dir, &part).await?;
    }
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
}

impl UploadPartTokenClaims {
    #[must_use]
    pub fn is_expired(&self) -> bool {
        OffsetDateTime::parse(&self.expires_at, &Rfc3339)
            .map_or(true, |expires_at| expires_at < OffsetDateTime::now_utc())
    }

    fn into_session_row(self) -> UploadSessionRow {
        UploadSessionRow {
            id: self.session_id,
            mode: self.mode,
            status: "active".to_string(),
            folder_path: None,
            document_id: None,
            filename: self.filename,
            total_size: self.total_size,
            chunk_size: self.chunk_size,
            part_count: self.part_count,
            verification_total_bytes: 0,
            verification_processed_bytes: 0,
            mime_type: None,
            note: None,
            rename_to_upload: false,
            created_by: self.owner_id,
            created_by_name: None,
            upload_ip: None,
            upload_user_agent: None,
            expires_at: self.expires_at,
            result_document_id: None,
            result_version_id: None,
            result_path: None,
        }
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
    let session = fetch_upload_session(pool, session_id)
        .await?
        .ok_or(UploadError::UploadSessionNotFound)?;
    require_transfer_owner(&session, user)?;
    if session.status == "complete" {
        return completed_result(&session);
    }
    ensure_active_session(pool, transfers_path, &session).await?;
    mark_upload_completing(pool, session_id, session.total_size).await?;
    let parts = completed_parts(pool, transfers_path, &session, expected_sha256).await?;

    let result = complete_upload_session_inner(pool, storage, &session, &parts, user).await;
    match result {
        Ok(payload) => {
            clear_upload_session_files(transfers_path, session_id).await;
            Ok(payload)
        }
        Err(error) => {
            let _ = mark_upload_failed(pool, transfers_path, session_id, &error.to_string()).await;
            Err(error)
        }
    }
}

pub async fn abort_upload_session(
    pool: &SqlitePool,
    transfers_path: &Path,
    token_secret: &str,
    session_id: &str,
    user: &UserContext,
) -> Result<UploadSessionPayload, UploadError> {
    let session = fetch_upload_session(pool, session_id)
        .await?
        .ok_or(UploadError::UploadSessionNotFound)?;
    require_transfer_owner(&session, user)?;
    if session.status != "complete" {
        sqlx::query(
            r"
            UPDATE upload_sessions
            SET status = 'aborted',
                aborted_at = ?,
                updated_at = ?
            WHERE id = ?
            ",
        )
        .bind(now_rfc3339()?)
        .bind(now_rfc3339()?)
        .bind(session_id)
        .execute(pool)
        .await?;
    }
    clear_upload_session_files(transfers_path, session_id).await;
    upload_session_payload(pool, transfers_path, token_secret, session_id).await
}

async fn complete_upload_session_inner(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    session: &UploadSessionRow,
    parts: &CompletedParts,
    user: &UserContext,
) -> Result<UploadResultPayload, UploadError> {
    let stored = storage
        .put_part_files(&parts.paths, Some(&parts.digest))
        .await?;
    let result = complete_upload_session_after_store(pool, session, parts, user, &stored).await;
    match result {
        Ok(payload) => Ok(payload),
        Err(error) => {
            // Upload completion promotes bytes before the canonical SQLite commit so
            // stale-state checks can still reject the upload without leaving a
            // content-addressed object that has no DB location row.
            cleanup_unreferenced_stored_object(pool, storage, &stored).await;
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
    let mut transaction = pool.begin().await?;
    let blob_id = get_or_create_blob_in_tx(&mut transaction, stored).await?;
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
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'complete',
            verification_total_bytes = ?,
            verification_processed_bytes = ?,
            completed_at = ?,
            updated_at = ?,
            result_document_id = ?,
            result_version_id = ?,
            result_path = ?
        WHERE id = ?
        ",
    )
    .bind(session.total_size)
    .bind(session.total_size)
    .bind(now_rfc3339()?)
    .bind(now_rfc3339()?)
    .bind(result.id)
    .bind(&result.version)
    .bind(&result.path)
    .bind(&session.id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(result)
}

async fn cleanup_unreferenced_stored_object(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    stored: &StoredBlob,
) {
    let referenced = sqlx::query_scalar::<_, i64>(
        r"
        SELECT COUNT(*)
        FROM blob_locations
        WHERE backend = ? AND bucket = ? AND object_key = ?
        ",
    )
    .bind(&stored.backend)
    .bind(&stored.bucket)
    .bind(&stored.object_key)
    .fetch_one(pool)
    .await;
    if referenced.ok() == Some(0) {
        let _ = storage.delete_object(&stored.object_key).await;
    }
}

async fn complete_create_upload_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    session: &UploadSessionRow,
    blob_id: i64,
) -> Result<UploadResultPayload, UploadError> {
    let folder_path = session.folder_path.clone().unwrap_or_default();
    let target_folder = get_or_create_folder_path_in_tx(transaction, &folder_path).await?;
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
) -> Result<CompletedParts, UploadError> {
    let parts = transfer_parts(transfers_path, session).await?;
    if i64::try_from(parts.len()).ok() != Some(session.part_count) {
        return Err(UploadError::UploadSessionMissingParts);
    }
    let mut hasher = Sha256::new();
    let mut size_bytes = 0_i64;
    let mut paths = Vec::with_capacity(parts.len());
    let mut progress = VerificationProgress::new(&session.id, session.total_size);
    for (index, part) in parts.iter().enumerate() {
        let part_number = i64::try_from(index + 1).map_err(|_| UploadError::InvalidPartNumber)?;
        if part.part_number != part_number {
            return Err(UploadError::UploadSessionMissingParts);
        }
        let (expected_offset, expected_size) = expected_part_bounds(session, part.part_number)?;
        if part.offset_bytes != expected_offset || part.size_bytes != expected_size {
            return Err(UploadError::UploadSessionMissingParts);
        }
        let path = PathBuf::from(&part.storage_path);
        hash_file(pool, &path, &mut hasher, &mut progress).await?;
        size_bytes += part.size_bytes;
        paths.push(path);
    }
    let digest = lower_hex(&hasher.finalize());
    if expected_sha256.is_some_and(|expected| digest != expected.to_ascii_lowercase()) {
        return Err(UploadError::UploadChecksumMismatch);
    }
    Ok(CompletedParts {
        digest,
        size_bytes,
        paths,
    })
}

async fn hash_file(
    pool: &SqlitePool,
    path: &Path,
    hasher: &mut Sha256,
    progress: &mut VerificationProgress<'_>,
) -> Result<(), UploadError> {
    let mut file = fs::File::open(path).await?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..read]);
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
    file.flush().await?;
    if size_bytes != expected_size {
        return Err(UploadError::UploadPartSizeMismatch);
    }
    if let (Some(expected), Some(hasher)) = (expected_sha256, hasher) {
        let actual_sha256 = lower_hex(&hasher.finalize());
        if actual_sha256 != expected.to_ascii_lowercase() {
            return Err(UploadError::UploadPartChecksumMismatch);
        }
    }
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
    Ok(())
}

async fn upload_session_payload(
    pool: &SqlitePool,
    transfers_path: &Path,
    token_secret: &str,
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
    let upload_token = upload_session_token(token_secret, &session)?;
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
            result_path
        FROM upload_sessions
        WHERE id = ?
        ",
    )
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
    let metadata_bytes = match fs::read(&metadata_path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let metadata = match fs::metadata(&storage_path).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let (offset_bytes, size_bytes) = expected_part_bounds(session, part_number)?;
            if metadata.len() != u64::try_from(size_bytes).unwrap_or(u64::MAX) {
                return Ok(None);
            }
            return Ok(Some(UploadPartRow {
                part_number,
                offset_bytes,
                size_bytes,
                sha256: None,
                storage_path: storage_path.to_string_lossy().to_string(),
            }));
        }
        Err(error) => return Err(error.into()),
    };
    let metadata: UploadPartMetadata = serde_json::from_slice(&metadata_bytes)?;
    Ok(Some(UploadPartRow {
        part_number: metadata.part_number,
        offset_bytes: metadata.offset_bytes,
        size_bytes: metadata.size_bytes,
        sha256: metadata.sha256,
        storage_path: storage_path.to_string_lossy().to_string(),
    }))
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
    fs::write(&temp_path, bytes).await?;
    fs::rename(&temp_path, &metadata_path).await?;
    Ok(())
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

async fn get_or_create_blob_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    stored: &StoredBlob,
) -> Result<i64, UploadError> {
    let size_bytes =
        i64::try_from(stored.size_bytes).map_err(|_| UploadError::UploadSizeMismatch)?;
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
    let existing_location = sqlx::query_scalar::<_, i64>(
        r"
        SELECT blob_id
        FROM blob_locations
        WHERE backend = ? AND bucket = ? AND object_key = ?
        ",
    )
    .bind(&stored.backend)
    .bind(&stored.bucket)
    .bind(&stored.object_key)
    .fetch_optional(&mut **transaction)
    .await?;
    if existing_location.is_some_and(|existing_blob_id| existing_blob_id != blob_id) {
        return Err(UploadError::StorageLocationConflict);
    }
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
) -> Result<(), UploadError> {
    let result = sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'completing',
            verification_total_bytes = ?,
            verification_processed_bytes = 0,
            updated_at = ?
        WHERE id = ? AND status = 'active'
        ",
    )
    .bind(total_bytes)
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

async fn record_upload_verification_progress(
    pool: &SqlitePool,
    session_id: &str,
    processed_bytes: i64,
) -> Result<(), UploadError> {
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET verification_processed_bytes = ?,
            updated_at = ?
        WHERE id = ? AND status = 'completing'
        ",
    )
    .bind(processed_bytes)
    .bind(now_rfc3339()?)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
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
    if OffsetDateTime::parse(&session.expires_at, &Rfc3339)? > OffsetDateTime::now_utc() {
        return Ok(());
    }
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'expired',
            updated_at = ?
        WHERE id = ? AND status = 'active'
        ",
    )
    .bind(now_rfc3339()?)
    .bind(&session.id)
    .execute(pool)
    .await?;
    clear_upload_session_files(transfers_path, &session.id).await;
    Err(UploadError::UploadSessionExpired)
}

async fn mark_upload_failed(
    pool: &SqlitePool,
    transfers_path: &Path,
    session_id: &str,
    message: &str,
) -> Result<(), UploadError> {
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'failed',
            error = ?,
            updated_at = ?
        WHERE id = ? AND status != 'complete' AND status != 'aborted'
        ",
    )
    .bind(message)
    .bind(now_rfc3339()?)
    .bind(session_id)
    .execute(pool)
    .await?;
    clear_upload_session_files(transfers_path, session_id).await;
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
    token_secret: &str,
    token: &str,
    session_id: &str,
) -> Result<String, UploadError> {
    Ok(verify_upload_token_claims(token_secret, token, session_id)?.owner_id)
}

pub fn verify_upload_token_claims(
    token_secret: &str,
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
    let mut mac = HmacSha256::new_from_slice(token_secret.as_bytes())
        .map_err(|_| UploadError::UploadTokenInvalid)?;
    mac.update(body.as_bytes());
    mac.verify_slice(&signature_bytes)
        .map_err(|_| UploadError::UploadTokenInvalid)?;
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
    Ok(UploadPartTokenClaims {
        session_id: session_id.to_string(),
        owner_id: owner.to_string(),
        mode: mode.to_string(),
        filename: filename.to_string(),
        total_size,
        chunk_size,
        part_count,
        expires_at: expires_at.to_string(),
    })
}

fn upload_session_token(
    token_secret: &str,
    session: &UploadSessionRow,
) -> Result<Option<String>, UploadError> {
    if !matches!(session.status.as_str(), "active" | "completing") {
        return Ok(None);
    }
    Ok(Some(sign_upload_token(token_secret, session)?))
}

fn sign_upload_token(
    token_secret: &str,
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
        "typ": "upload-part",
    });
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload)?);
    let mut mac = HmacSha256::new_from_slice(token_secret.as_bytes())
        .map_err(|_| UploadError::UploadTokenInvalid)?;
    mac.update(body.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{body}.{signature}"))
}

fn string_value<'a>(payload: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn integer_value(payload: &Map<String, Value>, key: &str) -> Option<i64> {
    payload.get(key).and_then(Value::as_i64)
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
    let max_chunk = transfer_chunk_bytes.max(1);
    let target_parallelism = upload_parallelism_target(client_upload_parallelism);
    if size_bytes <= 0 {
        return max_chunk;
    }
    if size_bytes <= max_chunk {
        return size_bytes.max(1);
    }
    let full_size_parts = (size_bytes + max_chunk - 1) / max_chunk;
    if full_size_parts >= target_parallelism {
        return max_chunk;
    }
    let min_chunk = max_chunk.min(UPLOAD_MIN_ADAPTIVE_CHUNK_BYTES);
    let round_to = max_chunk.min(UPLOAD_CHUNK_ROUNDING_BYTES);
    let target_parts = if size_bytes <= UPLOAD_SMALL_ADAPTIVE_MAX_BYTES {
        target_parallelism
    } else {
        let target_chunk = target_upload_chunk_bytes(target_parallelism);
        let target_parts = (size_bytes + target_chunk - 1) / target_chunk;
        target_parallelism.min(UPLOAD_MIN_ADAPTIVE_PARTS.max(target_parts))
    };
    let target_chunk = (size_bytes + target_parts - 1) / target_parts;
    let rounded = ((target_chunk + round_to - 1) / round_to) * round_to;
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
        (size_bytes + chunk_size - 1) / chunk_size
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

async fn clear_upload_session_files(transfers_path: &Path, session_id: &str) {
    if let Ok(path) = upload_session_dir(transfers_path, session_id) {
        let _ = fs::remove_dir_all(path).await;
    }
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
