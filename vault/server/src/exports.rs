use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use crc::{CRC_32_ISO_HDLC, Crc};
use flate2::Compression;
use flate2::write::{DeflateEncoder, ZlibEncoder};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::auth::UserContext;
use crate::documents::{
    DocumentError, VersionDownload, current_version_download, document_for_read,
};
use crate::folders::{
    FolderError, all_folders, folder_path_by_id, require_folder_read_access,
    subtree_folder_ids_from_records,
};
use crate::storage::{BlobStorageBackend, SharedBlobStorage, StorageError, StoredBlob};

const EXPORT_TTL_SECONDS: i64 = 86_400;
const EXPORT_WORKERS: i64 = 1;
const ZIP_DOS_DATE_1980_01_01: u16 = 33;
const ZIP_VERSION_DEFLATE: u16 = 20;
const ZIP_VERSION_ZIP64: u16 = 45;
const ZIP64_EXTRA_FIELD_ID: u16 = 0x0001;
const ZIP_FIELD_U16_MAX: usize = u16::MAX as usize;
const ZIP_FIELD_U32_MAX: u64 = u32::MAX as u64;
const EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES: i64 = 3 * 1024 * 1024 * 1024;
const EXPORT_ZIP_COMPRESSLEVEL: u32 = 1;
const EXPORT_CANCEL_CHECK_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const EXPORT_COMPRESSION_SAMPLE_BYTES: usize = 1024 * 1024;
const EXPORT_COMPRESSION_MIN_RATIO_NUMERATOR: usize = 98;
const EXPORT_COMPRESSION_MIN_RATIO_DENOMINATOR: usize = 100;
const EXPORT_COMPRESSIBLE_MIME_PREFIXES: &[&str] = &["text/"];
const EXPORT_COMPRESSIBLE_MIME_TYPES: &[&str] = &[
    "application/csv",
    "application/javascript",
    "application/json",
    "application/sql",
    "application/toml",
    "application/xml",
    "application/x-yaml",
    "image/svg+xml",
];
const EXPORT_STORED_MIME_PREFIXES: &[&str] = &["audio/", "video/"];
const EXPORT_STORED_MIME_TYPES: &[&str] = &[
    "application/gzip",
    "application/pdf",
    "application/vnd.rar",
    "application/x-7z-compressed",
    "application/x-bzip2",
    "application/x-gzip",
    "application/x-rar-compressed",
    "application/x-tar",
    "application/x-xz",
    "application/zip",
    "image/avif",
    "image/gif",
    "image/heic",
    "image/heif",
    "image/jpeg",
    "image/jpg",
    "image/png",
    "image/webp",
];
const EXPORT_STORED_EXTENSIONS: &[&str] = &[
    ".7z", ".avi", ".avif", ".bz2", ".gz", ".heic", ".heif", ".jpg", ".jpeg", ".m4v", ".mkv",
    ".mov", ".mp3", ".mp4", ".pdf", ".png", ".rar", ".webm", ".webp", ".xz", ".zip", ".zst",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportZipOptions {
    pub compression_threshold_bytes: i64,
    pub compresslevel: u32,
}

impl Default for ExportZipOptions {
    fn default() -> Self {
        Self {
            compression_threshold_bytes: EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES,
            compresslevel: EXPORT_ZIP_COMPRESSLEVEL,
        }
    }
}

impl ExportZipOptions {
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            compression_threshold_bytes: self.compression_threshold_bytes.max(0),
            compresslevel: self.compresslevel.clamp(1, 9),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportRuntimeSettings {
    pub ttl_seconds: i64,
    pub workers: i64,
    pub zip_options: ExportZipOptions,
}

impl Default for ExportRuntimeSettings {
    fn default() -> Self {
        Self {
            ttl_seconds: EXPORT_TTL_SECONDS,
            workers: EXPORT_WORKERS,
            zip_options: ExportZipOptions::default(),
        }
    }
}

impl ExportRuntimeSettings {
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            ttl_seconds: self.ttl_seconds.max(60),
            workers: self.workers.max(1),
            zip_options: self.zip_options.normalized(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportExecutionContext {
    settings: ExportRuntimeSettings,
    worker_slots: std::sync::Arc<Semaphore>,
}

impl ExportExecutionContext {
    #[must_use]
    pub fn new(settings: ExportRuntimeSettings) -> Self {
        let settings = settings.normalized();
        let workers = usize::try_from(settings.workers).unwrap_or(usize::MAX);
        Self {
            settings,
            worker_slots: std::sync::Arc::new(Semaphore::new(workers)),
        }
    }

    #[must_use]
    pub const fn settings(&self) -> &ExportRuntimeSettings {
        &self.settings
    }

    #[must_use]
    fn job_runner(&self) -> ExportJobRunner {
        ExportJobRunner {
            zip_options: self.settings.zip_options,
            worker_slots: Some(self.worker_slots.clone()),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ExportJobRunner {
    zip_options: ExportZipOptions,
    worker_slots: Option<std::sync::Arc<Semaphore>>,
}

#[derive(Debug, Clone)]
struct ExportJobCreateOptions {
    settings: ExportRuntimeSettings,
    runner: ExportJobRunner,
    mode: ExportJobCreateMode,
}

#[derive(Debug)]
struct ResolvedDownloads {
    selected_documents: i64,
    downloads: Vec<VersionDownload>,
}

#[derive(Debug, Clone, Copy)]
enum ExportJobCreateMode {
    Export,
    Download,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ExportSelectionItem {
    Document { id: i64 },
    Folder { id: i64, path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportRequestPayload {
    #[serde(default)]
    items: Vec<ExportSelectionItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportJobPayload {
    pub id: String,
    pub status: String,
    pub filename: String,
    pub total_items: i64,
    pub processed_items: i64,
    pub total_bytes: i64,
    pub processed_bytes: i64,
    pub error: Option<String>,
    pub expires_at: String,
    pub download_url: Option<String>,
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ExportArtifactDownload {
    pub job_id: String,
    pub filename: String,
    pub mime_type: String,
    pub hash_algo: String,
    pub hash: String,
    pub size_bytes: i64,
    pub backend: String,
    pub bucket: String,
    pub object_key: String,
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("export not found")]
    ExportNotFound,
    #[error("transfer not found")]
    TransferNotFound,
    #[error("export has no downloadable files")]
    ExportHasNoDownloadableFiles,
    #[error("insufficient folder access")]
    InsufficientFolderAccess,
    #[error("export expired")]
    ExportExpired,
    #[error("export is not complete")]
    ExportNotComplete,
    #[error("export was cancelled")]
    ExportCancelled,
    #[error("export artifact has no storage location")]
    ArtifactMissingStorageLocation,
    #[error("blob content does not match metadata")]
    BlobContentMismatch,
    #[error("export is too large for the current ZIP writer")]
    ZipLimitExceeded,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Folder(#[from] FolderError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    TimeFormat(#[from] time::error::Format),
    #[error(transparent)]
    TimeParse(#[from] time::error::Parse),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, FromRow)]
struct ExportJobRow {
    id: String,
    status: String,
    filename: String,
    total_items: i64,
    processed_items: i64,
    total_bytes: i64,
    processed_bytes: i64,
    created_by: String,
    error: Option<String>,
    expires_at: String,
    artifact_size_bytes: Option<i64>,
}

#[derive(Debug, FromRow)]
struct ExportArtifactRow {
    job_id: String,
    status: String,
    created_by: String,
    expires_at: String,
    artifact_filename: Option<String>,
    mime_type: Option<String>,
    size_bytes: Option<i64>,
    hash_algo: Option<String>,
    hash: Option<String>,
    backend: Option<String>,
    bucket: Option<String>,
    object_key: Option<String>,
}

#[derive(Debug, FromRow)]
struct ExportWorkRow {
    request_payload: String,
    user_context: String,
}

#[derive(Debug)]
struct ExportWork {
    items: Vec<ExportSelectionItem>,
    user: UserContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZipCompression {
    Stored,
    Deflated,
}

impl ZipCompression {
    const fn method_code(self) -> u16 {
        match self {
            Self::Stored => 0,
            Self::Deflated => 8,
        }
    }
}

#[derive(Debug)]
struct ZipEntryMeta {
    name: String,
    compression: ZipCompression,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_header_offset: u64,
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZipHeaderProbeInput<'a> {
    pub name: &'a str,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
    pub entry_count: usize,
    pub central_directory_size: u64,
    pub central_directory_offset: u64,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipHeaderProbe {
    pub local_file_header: Vec<u8>,
    pub central_directory_header: Vec<u8>,
    pub end_of_central_directory: Vec<u8>,
}

#[doc(hidden)]
pub fn zip_header_probe(input: ZipHeaderProbeInput<'_>) -> Result<ZipHeaderProbe, ExportError> {
    let local_file_header = local_file_header(
        input.name,
        ZipCompression::Stored,
        0,
        input.compressed_size,
        input.uncompressed_size,
    )?;
    let central_directory_header = central_directory_header(&ZipEntryMeta {
        name: input.name.to_string(),
        compression: ZipCompression::Stored,
        crc32: 0,
        compressed_size: input.compressed_size,
        uncompressed_size: input.uncompressed_size,
        local_header_offset: input.local_header_offset,
    })?;
    let end_of_central_directory = end_of_central_directory(
        input.entry_count,
        input.central_directory_size,
        input.central_directory_offset,
    )?;
    Ok(ZipHeaderProbe {
        local_file_header,
        central_directory_header,
        end_of_central_directory,
    })
}

pub async fn create_export_job(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    items: &[ExportSelectionItem],
    user: &UserContext,
) -> Result<ExportJobPayload, ExportError> {
    create_export_job_with_options(
        pool,
        storage,
        transfers_path,
        items,
        user,
        ExportZipOptions::default(),
    )
    .await
}

pub async fn create_export_job_with_runtime(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    items: &[ExportSelectionItem],
    user: &UserContext,
    execution: &ExportExecutionContext,
) -> Result<ExportJobPayload, ExportError> {
    create_export_job_inner(
        pool,
        storage,
        transfers_path,
        items,
        user,
        ExportJobCreateOptions {
            settings: execution.settings().clone(),
            runner: execution.job_runner(),
            mode: ExportJobCreateMode::Export,
        },
    )
    .await
}

pub async fn create_download_job_with_runtime(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    items: &[ExportSelectionItem],
    user: &UserContext,
    execution: &ExportExecutionContext,
) -> Result<ExportJobPayload, ExportError> {
    create_export_job_inner(
        pool,
        storage,
        transfers_path,
        items,
        user,
        ExportJobCreateOptions {
            settings: execution.settings().clone(),
            runner: execution.job_runner(),
            mode: ExportJobCreateMode::Download,
        },
    )
    .await
}

pub async fn create_export_job_with_options(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    items: &[ExportSelectionItem],
    user: &UserContext,
    zip_options: ExportZipOptions,
) -> Result<ExportJobPayload, ExportError> {
    create_export_job_inner(
        pool,
        storage,
        transfers_path,
        items,
        user,
        ExportJobCreateOptions {
            settings: ExportRuntimeSettings {
                zip_options,
                ..ExportRuntimeSettings::default()
            },
            runner: ExportJobRunner {
                zip_options: zip_options.normalized(),
                worker_slots: None,
            },
            mode: ExportJobCreateMode::Export,
        },
    )
    .await
}

async fn create_export_job_inner(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    items: &[ExportSelectionItem],
    user: &UserContext,
    options: ExportJobCreateOptions,
) -> Result<ExportJobPayload, ExportError> {
    let settings = options.settings.normalized();
    let job_id = Uuid::new_v4().simple().to_string();
    let filename = export_filename_for_items(pool, items).await?;
    let (total_items, total_bytes) = match options.mode {
        ExportJobCreateMode::Export => {
            let resolved = resolve_downloads(pool, items, user).await?;
            if resolved.selected_documents == 0 {
                return Err(ExportError::ExportHasNoDownloadableFiles);
            }
            (
                resolved.selected_documents,
                export_total_bytes(&resolved.downloads)?,
            )
        }
        ExportJobCreateMode::Download => {
            validate_download_queue_selection(pool, items, user).await?;
            (0, 0)
        }
    };
    let expires_at = expires_at_rfc3339(settings.ttl_seconds)?;
    let request_payload = serde_json::to_string(&ExportRequestPayload {
        items: items.to_vec(),
    })?;
    sqlx::query(
        r"
        INSERT INTO export_jobs
            (
                id,
                status,
                filename,
                total_items,
                total_bytes,
                created_by,
                created_by_name,
                user_context,
                request_payload,
                expires_at
            )
        VALUES
            (?, 'queued', ?, ?, ?, ?, ?, ?, ?, ?)
        ",
    )
    .bind(&job_id)
    .bind(&filename)
    .bind(total_items)
    .bind(total_bytes)
    .bind(&user.id)
    .bind(&user.name)
    .bind(serde_json::to_string(&transfer_user_payload(user))?)
    .bind(request_payload)
    .bind(&expires_at)
    .execute(pool)
    .await?;

    let payload = get_export_job(pool, &job_id, user).await?;
    start_export_job(
        pool.clone(),
        storage.clone(),
        transfers_path.to_path_buf(),
        job_id,
        options.runner,
    );
    Ok(payload)
}

pub async fn get_export_job(
    pool: &SqlitePool,
    job_id: &str,
    user: &UserContext,
) -> Result<ExportJobPayload, ExportError> {
    let row = export_job_row(pool, job_id)
        .await?
        .ok_or(ExportError::ExportNotFound)?;
    require_transfer_owner(&row.created_by, user)?;
    Ok(export_job_payload(row))
}

pub async fn cancel_export_job(
    pool: &SqlitePool,
    job_id: &str,
    user: &UserContext,
) -> Result<ExportJobPayload, ExportError> {
    let row = export_job_row(pool, job_id)
        .await?
        .ok_or(ExportError::ExportNotFound)?;
    require_transfer_owner(&row.created_by, user)?;
    if matches!(row.status.as_str(), "queued" | "running" | "finalizing") {
        sqlx::query(
            r"
            UPDATE export_jobs
            SET status = 'cancelled',
                cancelled_at = CURRENT_TIMESTAMP,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            ",
        )
        .bind(job_id)
        .execute(pool)
        .await?;
    }
    get_export_job(pool, job_id, user).await
}

fn start_export_job(
    pool: SqlitePool,
    storage: SharedBlobStorage,
    transfers_path: PathBuf,
    job_id: String,
    runner: ExportJobRunner,
) {
    tokio::spawn(async move {
        // Export jobs are queued in SQLite, but ZIP generation is in-process. This semaphore
        // preserves the deploy-time VAULT_EXPORT_WORKERS limit without adding an external queue.
        let _worker_permit = match runner.worker_slots.clone() {
            Some(slots) => Some(
                slots
                    .acquire_owned()
                    .await
                    .expect("export worker semaphore should not close"),
            ),
            None => None,
        };
        if let Err(error) = run_export_job(
            &pool,
            storage.as_ref(),
            &transfers_path,
            &job_id,
            runner.zip_options,
        )
        .await
        {
            if matches!(error, ExportError::ExportCancelled) {
                return;
            }
            let _ = mark_export_failed(&pool, &job_id, &error.to_string()).await;
        }
    });
}

pub async fn start_pending_export_jobs(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    limit: i64,
) -> Result<Vec<String>, ExportError> {
    start_pending_export_jobs_with_options(
        pool,
        storage,
        transfers_path,
        limit,
        ExportZipOptions::default(),
    )
    .await
}

pub async fn start_pending_export_jobs_with_runtime(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    limit: i64,
    execution: &ExportExecutionContext,
) -> Result<Vec<String>, ExportError> {
    start_pending_export_jobs_inner(pool, storage, transfers_path, limit, execution.job_runner())
        .await
}

pub async fn start_pending_export_jobs_with_options(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    limit: i64,
    zip_options: ExportZipOptions,
) -> Result<Vec<String>, ExportError> {
    start_pending_export_jobs_inner(
        pool,
        storage,
        transfers_path,
        limit,
        ExportJobRunner {
            zip_options: zip_options.normalized(),
            worker_slots: None,
        },
    )
    .await
}

async fn start_pending_export_jobs_inner(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    limit: i64,
    runner: ExportJobRunner,
) -> Result<Vec<String>, ExportError> {
    let job_ids = sqlx::query_scalar::<_, String>(
        r"
        SELECT id
        FROM export_jobs
        WHERE status = 'queued'
        ORDER BY created_at
        LIMIT ?
        ",
    )
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;
    for job_id in &job_ids {
        start_export_job(
            pool.clone(),
            storage.clone(),
            transfers_path.to_path_buf(),
            job_id.clone(),
            runner.clone(),
        );
    }
    Ok(job_ids)
}

pub async fn export_artifact_download(
    pool: &SqlitePool,
    job_id: &str,
    user: &UserContext,
) -> Result<ExportArtifactDownload, ExportError> {
    let row = sqlx::query_as::<_, ExportArtifactRow>(
        r"
        SELECT
            j.id AS job_id,
            j.status,
            j.created_by,
            j.expires_at,
            a.filename AS artifact_filename,
            a.mime_type,
            a.size_bytes,
            a.hash_algo,
            a.hash,
            l.backend,
            l.bucket,
            l.object_key
        FROM export_jobs j
        LEFT JOIN export_artifacts a ON a.job_id = j.id
        LEFT JOIN blob_locations l ON l.blob_id = a.blob_id
        WHERE j.id = ?
        ORDER BY a.id, l.id
        LIMIT 1
        ",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ExportError::ExportNotFound)?;
    require_transfer_owner(&row.created_by, user)?;
    if OffsetDateTime::parse(&row.expires_at, &Rfc3339)? <= OffsetDateTime::now_utc() {
        return Err(ExportError::ExportExpired);
    }
    if row.status != "complete" {
        return Err(ExportError::ExportNotComplete);
    }
    Ok(ExportArtifactDownload {
        job_id: row.job_id,
        filename: row
            .artifact_filename
            .ok_or(ExportError::ExportNotComplete)?,
        mime_type: row.mime_type.ok_or(ExportError::ExportNotComplete)?,
        hash_algo: row.hash_algo.ok_or(ExportError::ExportNotComplete)?,
        hash: row.hash.ok_or(ExportError::ExportNotComplete)?,
        size_bytes: row.size_bytes.ok_or(ExportError::ExportNotComplete)?,
        backend: row
            .backend
            .ok_or(ExportError::ArtifactMissingStorageLocation)?,
        bucket: row
            .bucket
            .ok_or(ExportError::ArtifactMissingStorageLocation)?,
        object_key: row
            .object_key
            .ok_or(ExportError::ArtifactMissingStorageLocation)?,
    })
}

async fn complete_export_job(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    transfers_path: &Path,
    job_id: &str,
    zip_options: ExportZipOptions,
) -> Result<(), ExportError> {
    let Some(work) = claim_export_job(pool, job_id).await? else {
        return Ok(());
    };
    let resolved = resolve_downloads(pool, &work.items, &work.user).await?;
    let downloads = resolved.downloads;
    // Queued jobs can legitimately resolve to zero files: `/api/download` keeps
    // Python's empty-folder behavior, and normal export jobs can lose readable
    // descendants before the worker rechecks state. Finish those as empty ZIPs.
    update_export_totals(pool, job_id, &downloads).await?;
    let temp_path = match create_export_zip(
        pool,
        storage,
        transfers_path,
        job_id,
        &downloads,
        zip_options,
    )
    .await
    {
        Ok(temp_path) => temp_path,
        Err(error) => {
            let _ = fs::remove_file(export_temp_path(transfers_path, job_id)).await;
            return Err(error);
        }
    };
    ensure_export_not_cancelled(pool, job_id).await?;
    sqlx::query(
        "UPDATE export_jobs SET status = 'finalizing', updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(job_id)
    .execute(pool)
    .await?;
    ensure_export_not_cancelled(pool, job_id).await?;
    let result = persist_export_artifact(pool, storage, job_id, &temp_path).await;
    let _ = fs::remove_file(&temp_path).await;
    if result.is_ok() {
        record_export_events(pool, &downloads, &work.user).await?;
    }
    result
}

async fn run_export_job(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    transfers_path: &Path,
    job_id: &str,
    zip_options: ExportZipOptions,
) -> Result<(), ExportError> {
    complete_export_job(pool, storage, transfers_path, job_id, zip_options).await
}

async fn validate_download_queue_selection(
    pool: &SqlitePool,
    items: &[ExportSelectionItem],
    user: &UserContext,
) -> Result<(), ExportError> {
    let mut seen_documents = HashSet::new();
    for item in items {
        match item {
            ExportSelectionItem::Document { id } => {
                if seen_documents.insert(*id) {
                    require_document_read_access(pool, *id, user).await?;
                }
            }
            ExportSelectionItem::Folder { id, .. } => {
                require_folder_read_access(pool, *id, user).await?;
            }
        }
    }
    Ok(())
}

async fn resolve_downloads(
    pool: &SqlitePool,
    items: &[ExportSelectionItem],
    user: &UserContext,
) -> Result<ResolvedDownloads, ExportError> {
    let mut downloads = Vec::new();
    let mut selected_documents = 0_i64;
    let mut seen_documents = HashSet::new();
    for item in items {
        match item {
            ExportSelectionItem::Document { id } => {
                if seen_documents.insert(*id) {
                    match current_version_download(pool, *id, user).await {
                        Ok(download) => {
                            selected_documents = selected_documents
                                .checked_add(1)
                                .ok_or(ExportError::ZipLimitExceeded)?;
                            downloads.push(download);
                        }
                        Err(DocumentError::DocumentHasNoVersions) => {
                            selected_documents = selected_documents
                                .checked_add(1)
                                .ok_or(ExportError::ZipLimitExceeded)?;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            ExportSelectionItem::Folder { id, .. } => {
                require_folder_read_access(pool, *id, user).await?;
                for document_id in document_ids_in_folder_subtree(pool, *id).await? {
                    if !seen_documents.insert(document_id) {
                        continue;
                    }
                    match current_version_download(pool, document_id, user).await {
                        Ok(download) => {
                            selected_documents = selected_documents
                                .checked_add(1)
                                .ok_or(ExportError::ZipLimitExceeded)?;
                            downloads.push(download);
                        }
                        Err(DocumentError::DocumentHasNoVersions) => {
                            selected_documents = selected_documents
                                .checked_add(1)
                                .ok_or(ExportError::ZipLimitExceeded)?;
                        }
                        // Folder exports are scoped to the readable subset of the selected
                        // subtree. Missing-version descendants still count as selected readable
                        // documents for creation, but hidden or disappeared descendants are
                        // omitted to match the Python folder download/export contract.
                        Err(
                            DocumentError::DocumentNotFound
                            | DocumentError::InsufficientDocumentAccess
                            | DocumentError::Folder(
                                FolderError::FolderNotFound | FolderError::InsufficientFolderAccess,
                            ),
                        ) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
    }
    downloads.sort_by(|left, right| left.document_path.cmp(&right.document_path));
    Ok(ResolvedDownloads {
        selected_documents,
        downloads,
    })
}

async fn require_document_read_access(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<(), ExportError> {
    document_for_read(pool, document_id, user).await?;
    Ok(())
}

async fn document_ids_in_folder_subtree(
    pool: &SqlitePool,
    folder_id: i64,
) -> Result<Vec<i64>, ExportError> {
    let folders = all_folders(pool).await?;
    let folder_ids = subtree_folder_ids_from_records(folder_id, &folders);
    if folder_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT id FROM documents WHERE folder_id IN (");
    {
        let mut separated = builder.separated(", ");
        for id in &folder_ids {
            separated.push_bind(*id);
        }
    }
    builder.push(") ORDER BY folder_id, name, id");
    Ok(builder.build_query_scalar::<i64>().fetch_all(pool).await?)
}

async fn claim_export_job(
    pool: &SqlitePool,
    job_id: &str,
) -> Result<Option<ExportWork>, ExportError> {
    let result = sqlx::query(
        r"
        UPDATE export_jobs
        SET status = 'running',
            processed_items = 0,
            processed_bytes = 0,
            error = NULL,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
          AND status = 'queued'
        ",
    )
    .bind(job_id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    let row = sqlx::query_as::<_, ExportWorkRow>(
        r"
        SELECT request_payload, user_context
        FROM export_jobs
        WHERE id = ?
        ",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await?;
    let request: ExportRequestPayload = serde_json::from_str(&row.request_payload)?;
    let user: UserContext = serde_json::from_str(&row.user_context)?;
    Ok(Some(ExportWork {
        items: request.items,
        user,
    }))
}

async fn update_export_totals(
    pool: &SqlitePool,
    job_id: &str,
    downloads: &[VersionDownload],
) -> Result<(), ExportError> {
    let (total_items, total_bytes) = export_totals(downloads)?;
    sqlx::query(
        r"
        UPDATE export_jobs
        SET total_items = ?,
            total_bytes = ?,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
        ",
    )
    .bind(total_items)
    .bind(total_bytes)
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn export_totals(downloads: &[VersionDownload]) -> Result<(i64, i64), ExportError> {
    let total_items = i64::try_from(downloads.len()).map_err(|_| ExportError::ZipLimitExceeded)?;
    Ok((total_items, export_total_bytes(downloads)?))
}

fn export_total_bytes(downloads: &[VersionDownload]) -> Result<i64, ExportError> {
    let total_bytes = downloads
        .iter()
        .try_fold(0_i64, |total, download| {
            total.checked_add(download.size_bytes)
        })
        .ok_or(ExportError::ZipLimitExceeded)?;
    Ok(total_bytes)
}

async fn ensure_export_not_cancelled(pool: &SqlitePool, job_id: &str) -> Result<(), ExportError> {
    let status = sqlx::query_scalar::<_, String>("SELECT status FROM export_jobs WHERE id = ?")
        .bind(job_id)
        .fetch_optional(pool)
        .await?
        .ok_or(ExportError::ExportNotFound)?;
    if status == "cancelled" {
        Err(ExportError::ExportCancelled)
    } else {
        Ok(())
    }
}

async fn record_export_progress(
    pool: &SqlitePool,
    job_id: &str,
    processed_bytes: i64,
) -> Result<(), ExportError> {
    ensure_export_not_cancelled(pool, job_id).await?;
    sqlx::query(
        r"
        UPDATE export_jobs
        SET processed_items = processed_items + 1,
            processed_bytes = processed_bytes + ?,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
          AND status = 'running'
        ",
    )
    .bind(processed_bytes)
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn create_export_zip(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    transfers_path: &Path,
    job_id: &str,
    downloads: &[VersionDownload],
    zip_options: ExportZipOptions,
) -> Result<PathBuf, ExportError> {
    // Export ZIPs are derived transfer artifacts, not canonical document state. Build them in
    // transfer scratch space first, then promote only the completed archive into blob storage.
    fs::create_dir_all(transfers_path.join("exports")).await?;
    let temp_path = export_temp_path(transfers_path, job_id);
    let mut file = fs::File::create(&temp_path).await?;
    let mut zip_hasher = Sha256::new();
    let mut offset = 0_u64;
    let mut entries = Vec::with_capacity(downloads.len());
    let mut written_names = HashSet::new();
    let total_export_bytes = downloads
        .iter()
        .try_fold(0_i64, |total, download| {
            total.checked_add(download.size_bytes)
        })
        .ok_or(ExportError::ZipLimitExceeded)?;

    for download in downloads {
        ensure_export_not_cancelled(pool, job_id).await?;
        let mut archive_name = safe_zip_entry_name(&download.document_path);
        if !written_names.insert(archive_name.clone()) {
            archive_name = safe_zip_entry_name(&format!("{}-{archive_name}", download.document_id));
            written_names.insert(archive_name.clone());
        }
        let data = storage
            .read_location_bytes(&download.backend, &download.bucket, &download.object_key)
            .await?;
        verify_download_bytes(download, &data)?;
        let crc32 = Crc::<u32>::new(&CRC_32_ISO_HDLC).checksum(&data);
        let size_bytes = u64::try_from(data.len()).map_err(|_| ExportError::ZipLimitExceeded)?;
        let compression = export_entry_compression(
            &archive_name,
            download,
            &data,
            total_export_bytes,
            zip_options,
        )?;
        let entry_data = match compression {
            ZipCompression::Stored => data,
            ZipCompression::Deflated => deflate_bytes(&data, zip_options.compresslevel)?,
        };
        let compressed_size =
            u64::try_from(entry_data.len()).map_err(|_| ExportError::ZipLimitExceeded)?;
        let local_header_offset = offset;
        let local_header = local_file_header(
            &archive_name,
            compression,
            crc32,
            compressed_size,
            size_bytes,
        )?;
        write_counted(&mut file, &mut zip_hasher, &mut offset, &local_header).await?;
        // A single large ZIP entry can dominate an export. Check cancellation between
        // payload chunks so cancelling a job does not wait for the whole file to finish.
        write_counted_checked(
            pool,
            job_id,
            &mut file,
            &mut zip_hasher,
            &mut offset,
            &entry_data,
        )
        .await?;
        entries.push(ZipEntryMeta {
            name: archive_name,
            compression,
            crc32,
            compressed_size,
            uncompressed_size: size_bytes,
            local_header_offset,
        });
        record_export_progress(pool, job_id, download.size_bytes).await?;
    }

    ensure_export_not_cancelled(pool, job_id).await?;
    let central_directory_offset = offset;
    for entry in &entries {
        let central_header = central_directory_header(entry)?;
        write_counted(&mut file, &mut zip_hasher, &mut offset, &central_header).await?;
    }
    let central_directory_size = offset
        .checked_sub(central_directory_offset)
        .ok_or(ExportError::ZipLimitExceeded)?;
    let end_record = end_of_central_directory(
        entries.len(),
        central_directory_size,
        central_directory_offset,
    )?;
    write_counted(&mut file, &mut zip_hasher, &mut offset, &end_record).await?;
    file.flush().await?;
    Ok(temp_path)
}

fn export_temp_path(transfers_path: &Path, job_id: &str) -> PathBuf {
    transfers_path
        .join("exports")
        .join(format!("{job_id}.zip.tmp"))
}

async fn persist_export_artifact(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    job_id: &str,
    temp_path: &Path,
) -> Result<(), ExportError> {
    ensure_export_not_cancelled(pool, job_id).await?;
    let (digest, size_bytes) = hash_file(temp_path).await?;
    ensure_export_not_cancelled(pool, job_id).await?;
    let stored = storage.put_file(temp_path, &digest, size_bytes).await?;
    if let Err(error) = ensure_export_not_cancelled(pool, job_id).await {
        cleanup_unreferenced_export_object(pool, storage, &stored).await;
        return Err(error);
    }
    let blob_id = get_or_create_blob(pool, &stored).await?;
    if let Err(error) = ensure_export_not_cancelled(pool, job_id).await {
        cleanup_export_artifact_commit(pool, storage, job_id, &stored).await;
        return Err(error);
    }
    let job = export_job_row(pool, job_id)
        .await?
        .ok_or(ExportError::ExportNotFound)?;
    sqlx::query(
        r"
        INSERT INTO export_artifacts
            (
                job_id,
                blob_id,
                filename,
                mime_type,
                size_bytes,
                hash_algo,
                hash,
                expires_at
            )
        VALUES
            (?, ?, ?, 'application/zip', ?, 'sha256', ?, ?)
        ",
    )
    .bind(job_id)
    .bind(blob_id)
    .bind(&job.filename)
    .bind(i64::try_from(stored.size_bytes).map_err(|_| ExportError::ZipLimitExceeded)?)
    .bind(&stored.digest)
    .bind(&job.expires_at)
    .execute(pool)
    .await?;
    if let Err(error) = ensure_export_not_cancelled(pool, job_id).await {
        cleanup_export_artifact_commit(pool, storage, job_id, &stored).await;
        return Err(error);
    }
    let completed = sqlx::query(
        r"
        UPDATE export_jobs
        SET status = 'complete',
            processed_items = total_items,
            processed_bytes = total_bytes,
            completed_at = CURRENT_TIMESTAMP,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
          AND status = 'finalizing'
        ",
    )
    .bind(job_id)
    .execute(pool)
    .await?;
    if completed.rows_affected() == 0 {
        cleanup_export_artifact_commit(pool, storage, job_id, &stored).await;
        return Err(ExportError::ExportCancelled);
    }
    Ok(())
}

async fn cleanup_export_artifact_commit(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    job_id: &str,
    stored: &StoredBlob,
) {
    let _ = sqlx::query("DELETE FROM export_artifacts WHERE job_id = ?")
        .bind(job_id)
        .execute(pool)
        .await;
    cleanup_unreferenced_export_blob(pool, storage, stored).await;
}

async fn cleanup_unreferenced_export_object(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    stored: &StoredBlob,
) {
    let references = sqlx::query_scalar::<_, i64>(
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
    if matches!(references, Ok(0)) {
        let _ = storage
            .delete_location(&stored.backend, &stored.bucket, &stored.object_key)
            .await;
    }
}

async fn cleanup_unreferenced_export_blob(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    stored: &StoredBlob,
) {
    let Ok(size_bytes) = i64::try_from(stored.size_bytes) else {
        return;
    };
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
    .fetch_optional(pool)
    .await;
    let Some(blob_id) = blob_id.ok().flatten() else {
        cleanup_unreferenced_export_object(pool, storage, stored).await;
        return;
    };
    let references = sqlx::query_scalar::<_, i64>(
        r"
        SELECT
            (SELECT COUNT(*) FROM document_versions WHERE blob_id = ?)
            + (SELECT COUNT(*) FROM export_artifacts WHERE blob_id = ?)
        ",
    )
    .bind(blob_id)
    .bind(blob_id)
    .fetch_one(pool)
    .await;
    if !matches!(references, Ok(0)) {
        return;
    }
    let _ = sqlx::query("DELETE FROM blob_locations WHERE blob_id = ?")
        .bind(blob_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM blobs WHERE id = ?")
        .bind(blob_id)
        .execute(pool)
        .await;
    cleanup_unreferenced_export_object(pool, storage, stored).await;
}

async fn record_export_events(
    pool: &SqlitePool,
    downloads: &[VersionDownload],
    user: &UserContext,
) -> Result<(), ExportError> {
    for download in downloads {
        sqlx::query(
            r"
            INSERT INTO document_events
                (document_id, event_type, actor, actor_name, message, result)
            VALUES
                (?, 'download', ?, ?, ?, 'ok')
            ",
        )
        .bind(download.document_id)
        .bind(&user.id)
        .bind(&user.name)
        .bind(format!("Exported {}", download.document_path))
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn get_or_create_blob(pool: &SqlitePool, stored: &StoredBlob) -> Result<i64, ExportError> {
    sqlx::query(
        r"
        INSERT OR IGNORE INTO blobs (hash_algo, hash, size_bytes)
        VALUES (?, ?, ?)
        ",
    )
    .bind(&stored.hash_algo)
    .bind(&stored.digest)
    .bind(i64::try_from(stored.size_bytes).map_err(|_| ExportError::ZipLimitExceeded)?)
    .execute(pool)
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
    .bind(i64::try_from(stored.size_bytes).map_err(|_| ExportError::ZipLimitExceeded)?)
    .fetch_one(pool)
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
    .execute(pool)
    .await?;
    Ok(blob_id)
}

async fn hash_file(path: &Path) -> Result<(String, u64), ExportError> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut size_bytes = 0_u64;
    let mut buffer = vec![0_u8; crate::storage::STORAGE_CHUNK_SIZE];
    loop {
        let read = tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size_bytes = size_bytes
            .checked_add(u64::try_from(read).map_err(|_| ExportError::ZipLimitExceeded)?)
            .ok_or(ExportError::ZipLimitExceeded)?;
    }
    Ok((lower_hex(&hasher.finalize()), size_bytes))
}

async fn mark_export_failed(
    pool: &SqlitePool,
    job_id: &str,
    error: &str,
) -> Result<(), ExportError> {
    sqlx::query(
        r"
        UPDATE export_jobs
        SET status = 'failed',
            error = ?,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
          AND status != 'cancelled'
        ",
    )
    .bind(error)
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn export_job_row(
    pool: &SqlitePool,
    job_id: &str,
) -> Result<Option<ExportJobRow>, ExportError> {
    Ok(sqlx::query_as::<_, ExportJobRow>(
        r"
        SELECT
            j.id,
            j.status,
            j.filename,
            j.total_items,
            j.processed_items,
            j.total_bytes,
            j.processed_bytes,
            j.created_by,
            j.error,
            j.expires_at,
            a.size_bytes AS artifact_size_bytes
        FROM export_jobs j
        LEFT JOIN export_artifacts a ON a.job_id = j.id
        WHERE j.id = ?
        ORDER BY a.id
        LIMIT 1
        ",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?)
}

fn export_job_payload(row: ExportJobRow) -> ExportJobPayload {
    let has_artifact = row.artifact_size_bytes.is_some();
    ExportJobPayload {
        id: row.id.clone(),
        status: row.status,
        filename: row.filename,
        total_items: row.total_items,
        processed_items: row.processed_items,
        total_bytes: row.total_bytes,
        processed_bytes: row.processed_bytes,
        error: row.error,
        expires_at: row.expires_at,
        download_url: has_artifact.then(|| format!("/api/exports/{}/download", row.id)),
        size_bytes: row.artifact_size_bytes,
    }
}

async fn export_filename_for_items(
    pool: &SqlitePool,
    items: &[ExportSelectionItem],
) -> Result<String, ExportError> {
    if let [ExportSelectionItem::Folder { id, .. }] = items {
        let folder_path = folder_path_by_id(pool, *id).await?;
        let folder_name = folder_path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("folder");
        return Ok(zip_download_name(folder_name));
    }
    Ok("vault-download.zip".to_string())
}

fn zip_download_name(name: &str) -> String {
    let mut base = safe_download_name(name).replace(['/', '\\'], "_");
    if base.is_empty() {
        base = "folder".to_string();
    }
    if base.to_ascii_lowercase().ends_with(".zip") {
        base
    } else {
        format!("{base}.zip")
    }
}

fn require_transfer_owner(owner_id: &str, user: &UserContext) -> Result<(), ExportError> {
    if owner_id == user.id || user.is_admin {
        Ok(())
    } else {
        Err(ExportError::TransferNotFound)
    }
}

fn transfer_user_payload(user: &UserContext) -> serde_json::Value {
    json!({
        "id": user.id,
        "vault_user_id": user.vault_user_id,
        "issuer": user.issuer,
        "subject": user.subject,
        "name": user.name,
        "email": user.email,
        "groups": user.groups,
        "is_admin": user.is_admin,
    })
}

fn verify_download_bytes(download: &VersionDownload, data: &[u8]) -> Result<(), ExportError> {
    let size_bytes = u64::try_from(data.len()).map_err(|_| ExportError::ZipLimitExceeded)?;
    if size_bytes
        != u64::try_from(download.size_bytes).map_err(|_| ExportError::ZipLimitExceeded)?
    {
        return Err(ExportError::BlobContentMismatch);
    }
    let digest = lower_hex(&Sha256::digest(data));
    if download.hash_algo != "sha256" || digest != download.hash {
        return Err(ExportError::BlobContentMismatch);
    }
    Ok(())
}

fn export_entry_compression(
    archive_name: &str,
    download: &VersionDownload,
    data: &[u8],
    total_export_bytes: i64,
    options: ExportZipOptions,
) -> Result<ZipCompression, ExportError> {
    if !export_zip_compression_enabled(total_export_bytes, options) {
        return Ok(ZipCompression::Stored);
    }
    if export_entry_is_known_stored(archive_name, download.mime_type.as_deref()) {
        return Ok(ZipCompression::Stored);
    }
    if export_entry_is_known_compressible(archive_name, download.mime_type.as_deref()) {
        return Ok(ZipCompression::Deflated);
    }
    sampled_zip_compression(data, options.compresslevel)
}

fn export_zip_compression_enabled(total_bytes: i64, options: ExportZipOptions) -> bool {
    options.compression_threshold_bytes > 0 && total_bytes >= options.compression_threshold_bytes
}

fn export_entry_is_known_compressible(archive_name: &str, mime_type: Option<&str>) -> bool {
    let mime_type = normalized_export_mime_type(archive_name, mime_type);
    EXPORT_COMPRESSIBLE_MIME_TYPES.contains(&mime_type.as_str())
        || EXPORT_COMPRESSIBLE_MIME_PREFIXES
            .iter()
            .any(|prefix| mime_type.starts_with(prefix))
}

fn export_entry_is_known_stored(archive_name: &str, mime_type: Option<&str>) -> bool {
    let mime_type = normalized_export_mime_type(archive_name, mime_type);
    let extension = file_extension(archive_name);
    EXPORT_STORED_MIME_TYPES.contains(&mime_type.as_str())
        || EXPORT_STORED_MIME_PREFIXES
            .iter()
            .any(|prefix| mime_type.starts_with(prefix))
        || EXPORT_STORED_EXTENSIONS.contains(&extension.as_str())
}

fn sampled_zip_compression(data: &[u8], compresslevel: u32) -> Result<ZipCompression, ExportError> {
    if data.is_empty() {
        return Ok(ZipCompression::Stored);
    }
    let sample_len = data.len().min(EXPORT_COMPRESSION_SAMPLE_BYTES);
    let sample = &data[..sample_len];
    let compressed = zlib_bytes(sample, compresslevel)?;
    if compressed.len()
        <= sample.len() * EXPORT_COMPRESSION_MIN_RATIO_NUMERATOR
            / EXPORT_COMPRESSION_MIN_RATIO_DENOMINATOR
    {
        Ok(ZipCompression::Deflated)
    } else {
        Ok(ZipCompression::Stored)
    }
}

fn deflate_bytes(data: &[u8], compresslevel: u32) -> Result<Vec<u8>, ExportError> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(compresslevel.clamp(1, 9)));
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn zlib_bytes(data: &[u8], compresslevel: u32) -> Result<Vec<u8>, ExportError> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(compresslevel.clamp(1, 9)));
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn normalized_export_mime_type(archive_name: &str, mime_type: Option<&str>) -> String {
    let fallback = mime_from_filename(archive_name);
    let raw_type = mime_type.unwrap_or(&fallback).trim().to_ascii_lowercase();
    raw_type
        .split_once(';')
        .map_or(raw_type.as_str(), |(base, _)| base)
        .trim()
        .to_string()
}

fn mime_from_filename(filename: &str) -> String {
    match file_extension(filename).as_str() {
        ".csv" => "text/csv",
        ".htm" | ".html" => "text/html",
        ".js" | ".mjs" => "application/javascript",
        ".json" => "application/json",
        ".md" => "text/markdown",
        ".txt" => "text/plain",
        ".pdf" => "application/pdf",
        ".png" => "image/png",
        ".svg" => "image/svg+xml",
        ".xml" => "application/xml",
        ".yaml" | ".yml" => "application/x-yaml",
        ".zip" => "application/zip",
        _ => "",
    }
    .to_string()
}

fn file_extension(filename: &str) -> String {
    let Some(index) = filename.rfind('.') else {
        return String::new();
    };
    filename[index..].to_ascii_lowercase()
}

fn local_file_header(
    name: &str,
    compression: ZipCompression,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
) -> Result<Vec<u8>, ExportError> {
    let name_bytes = name.as_bytes();
    let needs_zip64 = zip64_sizes_needed(compressed_size, uncompressed_size);
    let extra = if needs_zip64 {
        zip64_extra_field(&[uncompressed_size, compressed_size])?
    } else {
        Vec::new()
    };
    let mut header = Vec::with_capacity(30 + name_bytes.len() + extra.len());
    push_u32(&mut header, 0x0403_4b50);
    push_u16(
        &mut header,
        if needs_zip64 {
            ZIP_VERSION_ZIP64
        } else {
            ZIP_VERSION_DEFLATE
        },
    );
    push_u16(&mut header, 0);
    push_u16(&mut header, compression.method_code());
    push_u16(&mut header, 0);
    push_u16(&mut header, ZIP_DOS_DATE_1980_01_01);
    push_u32(&mut header, crc32);
    push_zip_u32_or_zip64(&mut header, compressed_size, needs_zip64)?;
    push_zip_u32_or_zip64(&mut header, uncompressed_size, needs_zip64)?;
    push_u16(&mut header, checked_zip_u16(name_bytes.len())?);
    push_u16(&mut header, checked_zip_u16(extra.len())?);
    header.extend_from_slice(name_bytes);
    header.extend_from_slice(&extra);
    Ok(header)
}

fn central_directory_header(entry: &ZipEntryMeta) -> Result<Vec<u8>, ExportError> {
    let name_bytes = entry.name.as_bytes();
    let needs_zip64 = zip64_central_header_needed(entry);
    let extra = if needs_zip64 {
        zip64_extra_field(&[
            entry.uncompressed_size,
            entry.compressed_size,
            entry.local_header_offset,
        ])?
    } else {
        Vec::new()
    };
    let mut header = Vec::with_capacity(46 + name_bytes.len() + extra.len());
    push_u32(&mut header, 0x0201_4b50);
    push_u16(
        &mut header,
        if needs_zip64 {
            ZIP_VERSION_ZIP64
        } else {
            ZIP_VERSION_DEFLATE
        },
    );
    push_u16(
        &mut header,
        if needs_zip64 {
            ZIP_VERSION_ZIP64
        } else {
            ZIP_VERSION_DEFLATE
        },
    );
    push_u16(&mut header, 0);
    push_u16(&mut header, entry.compression.method_code());
    push_u16(&mut header, 0);
    push_u16(&mut header, ZIP_DOS_DATE_1980_01_01);
    push_u32(&mut header, entry.crc32);
    push_zip_u32_or_zip64(&mut header, entry.compressed_size, needs_zip64)?;
    push_zip_u32_or_zip64(&mut header, entry.uncompressed_size, needs_zip64)?;
    push_u16(&mut header, checked_zip_u16(name_bytes.len())?);
    push_u16(&mut header, checked_zip_u16(extra.len())?);
    push_u16(&mut header, 0);
    push_u16(&mut header, 0);
    push_u16(&mut header, 0);
    push_u32(&mut header, 0);
    push_zip_u32_or_zip64(&mut header, entry.local_header_offset, needs_zip64)?;
    header.extend_from_slice(name_bytes);
    header.extend_from_slice(&extra);
    Ok(header)
}

fn end_of_central_directory(
    entry_count: usize,
    central_directory_size: u64,
    central_directory_offset: u64,
) -> Result<Vec<u8>, ExportError> {
    let needs_zip64 = zip64_end_record_needed(
        entry_count,
        central_directory_size,
        central_directory_offset,
    );
    let mut record = Vec::with_capacity(if needs_zip64 { 98 } else { 22 });
    if needs_zip64 {
        let zip64_end_offset = central_directory_offset
            .checked_add(central_directory_size)
            .ok_or(ExportError::ZipLimitExceeded)?;
        push_u32(&mut record, 0x0606_4b50);
        push_u64(&mut record, 44);
        push_u16(&mut record, ZIP_VERSION_ZIP64);
        push_u16(&mut record, ZIP_VERSION_ZIP64);
        push_u32(&mut record, 0);
        push_u32(&mut record, 0);
        push_u64(
            &mut record,
            u64::try_from(entry_count).map_err(|_| ExportError::ZipLimitExceeded)?,
        );
        push_u64(
            &mut record,
            u64::try_from(entry_count).map_err(|_| ExportError::ZipLimitExceeded)?,
        );
        push_u64(&mut record, central_directory_size);
        push_u64(&mut record, central_directory_offset);

        push_u32(&mut record, 0x0706_4b50);
        push_u32(&mut record, 0);
        push_u64(&mut record, zip64_end_offset);
        push_u32(&mut record, 1);
    }

    push_u32(&mut record, 0x0605_4b50);
    push_u16(&mut record, 0);
    push_u16(&mut record, 0);
    push_zip_u16_or_zip64(&mut record, entry_count);
    push_zip_u16_or_zip64(&mut record, entry_count);
    push_zip_u32_or_zip64(&mut record, central_directory_size, needs_zip64)?;
    push_zip_u32_or_zip64(&mut record, central_directory_offset, needs_zip64)?;
    push_u16(&mut record, 0);
    Ok(record)
}

async fn write_counted(
    file: &mut fs::File,
    hasher: &mut Sha256,
    offset: &mut u64,
    bytes: &[u8],
) -> Result<(), ExportError> {
    file.write_all(bytes).await?;
    hasher.update(bytes);
    *offset = offset
        .checked_add(u64::try_from(bytes.len()).map_err(|_| ExportError::ZipLimitExceeded)?)
        .ok_or(ExportError::ZipLimitExceeded)?;
    Ok(())
}

async fn write_counted_checked(
    pool: &SqlitePool,
    job_id: &str,
    file: &mut fs::File,
    hasher: &mut Sha256,
    offset: &mut u64,
    bytes: &[u8],
) -> Result<(), ExportError> {
    for chunk in bytes.chunks(EXPORT_CANCEL_CHECK_CHUNK_BYTES) {
        ensure_export_not_cancelled(pool, job_id).await?;
        write_counted(file, hasher, offset, chunk).await?;
        tokio::task::yield_now().await;
    }
    Ok(())
}

fn zip64_sizes_needed(compressed_size: u64, uncompressed_size: u64) -> bool {
    compressed_size > ZIP_FIELD_U32_MAX || uncompressed_size > ZIP_FIELD_U32_MAX
}

fn zip64_central_header_needed(entry: &ZipEntryMeta) -> bool {
    zip64_sizes_needed(entry.compressed_size, entry.uncompressed_size)
        || entry.local_header_offset > ZIP_FIELD_U32_MAX
}

fn zip64_end_record_needed(
    entry_count: usize,
    central_directory_size: u64,
    central_directory_offset: u64,
) -> bool {
    entry_count > ZIP_FIELD_U16_MAX
        || central_directory_size > ZIP_FIELD_U32_MAX
        || central_directory_offset > ZIP_FIELD_U32_MAX
}

fn zip64_extra_field(values: &[u64]) -> Result<Vec<u8>, ExportError> {
    let payload_len = values
        .len()
        .checked_mul(8)
        .ok_or(ExportError::ZipLimitExceeded)?;
    let mut extra = Vec::with_capacity(4 + payload_len);
    push_u16(&mut extra, ZIP64_EXTRA_FIELD_ID);
    push_u16(&mut extra, checked_zip_u16(payload_len)?);
    for value in values {
        push_u64(&mut extra, *value);
    }
    Ok(extra)
}

fn push_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(buffer: &mut Vec<u8>, value: u64) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn push_zip_u16_or_zip64(buffer: &mut Vec<u8>, value: usize) {
    if value > ZIP_FIELD_U16_MAX {
        push_u16(buffer, u16::MAX);
    } else {
        push_u16(buffer, u16::try_from(value).unwrap_or(u16::MAX));
    }
}

fn push_zip_u32_or_zip64(
    buffer: &mut Vec<u8>,
    value: u64,
    force_zip64: bool,
) -> Result<(), ExportError> {
    if force_zip64 || value > ZIP_FIELD_U32_MAX {
        push_u32(buffer, u32::MAX);
    } else {
        push_u32(buffer, checked_zip_u32(value)?);
    }
    Ok(())
}

fn checked_zip_u16(value: usize) -> Result<u16, ExportError> {
    u16::try_from(value).map_err(|_| ExportError::ZipLimitExceeded)
}

fn checked_zip_u32(value: u64) -> Result<u32, ExportError> {
    u32::try_from(value).map_err(|_| ExportError::ZipLimitExceeded)
}

fn safe_zip_entry_name(name: &str) -> String {
    let normalized = name.replace('\\', "/");
    let parts = normalized
        .split('/')
        .filter_map(|part| {
            let trimmed = part.trim();
            if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
                None
            } else {
                Some(safe_download_name(trimmed))
            }
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "download".to_string()
    } else {
        parts.join("/")
    }
}

fn safe_download_name(name: &str) -> String {
    name.chars()
        .filter(|character| {
            !matches!(
                character,
                '\0' | '\n' | '\r' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string()
}

fn expires_at_rfc3339(ttl_seconds: i64) -> Result<String, ExportError> {
    Ok((OffsetDateTime::now_utc() + Duration::seconds(ttl_seconds.max(60))).format(&Rfc3339)?)
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
