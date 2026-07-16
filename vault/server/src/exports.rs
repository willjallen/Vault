use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration as StdDuration;

use axum::body::Bytes;
use crc::{CRC_32_ISO_HDLC, Crc};
use flate2::write::ZlibEncoder;
use flate2::{Compress, Compression, FlushCompress, Status};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::auth::UserContext;
use crate::blob_lifecycle::{
    BlobLifecycleError, begin_blob_publication, collect_unreferenced_blobs_with_limit,
};
use crate::documents::{
    DocumentError, VersionDownload, current_version_download, document_for_read,
};
use crate::folders::{
    FolderError, all_folders, folder_path_by_id, require_folder_read_access,
    subtree_folder_ids_from_records,
};
use crate::storage::{
    BlobByteStream, BlobReadRange, BlobStorageBackend, BlobWriteKind, STORAGE_CHUNK_SIZE,
    SharedBlobStorage, StorageError,
};

const EXPORT_TTL_SECONDS: i64 = 86_400;
const EXPORT_WORKERS: i64 = 1;
const ZIP_DOS_DATE_1980_01_01: u16 = 33;
const ZIP_VERSION_DEFLATE: u16 = 20;
const ZIP_VERSION_ZIP64: u16 = 45;
const ZIP64_EXTRA_FIELD_ID: u16 = 0x0001;
const ZIP_GENERAL_PURPOSE_DATA_DESCRIPTOR: u16 = 1 << 3;
const ZIP_FIELD_U16_MAX: usize = u16::MAX as usize;
const ZIP_FIELD_U32_MAX: u64 = u32::MAX as u64;
const EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES: i64 = 3 * 1024 * 1024 * 1024;
const EXPORT_ZIP_COMPRESSLEVEL: u32 = 1;
const EXPORT_CANCEL_CHECK_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const EXPORT_STREAM_STORED_ENTRY_BYTES: i64 = 64 * 1024 * 1024;
const EXPORT_PROGRESS_UPDATE_BYTES: i64 = 32 * 1024 * 1024;
const EXPORT_COMPRESSION_SAMPLE_BYTES: usize = 1024 * 1024;
const EXPORT_COMPRESSION_BATCH_BYTES: usize = 8 * 1024 * 1024;
const EXPORT_COMPRESSION_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
const EXPORT_COMPRESSION_TASKS: usize = 4;
const EXPORT_SOURCE_CANCEL_POLL_INTERVAL: StdDuration = StdDuration::from_millis(250);
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
    #[error("storage location points at another blob")]
    StorageLocationConflict,
    #[error(transparent)]
    BlobLifecycle(#[from] BlobLifecycleError),
    #[error("export is too large for the current ZIP writer")]
    ZipLimitExceeded,
    #[error("export compression worker failed")]
    CompressionTaskFailed,
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
    uses_data_descriptor: bool,
    force_zip64: bool,
}

#[derive(Debug)]
struct ExportZipArtifact {
    path: PathBuf,
    digest: String,
    size_bytes: u64,
}

struct ZipWriteContext<'a> {
    pool: &'a SqlitePool,
    storage: &'a dyn BlobStorageBackend,
    job_id: &'a str,
    file: &'a mut fs::File,
    zip_hasher: &'a mut Sha256,
    offset: &'a mut u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZipCompressionPlan {
    Stored,
    Deflated,
    Sample,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamingZipHeaderProbeInput<'a> {
    pub name: &'a str,
    pub deflated: bool,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingZipHeaderProbe {
    pub local_file_header: Vec<u8>,
    pub data_descriptor: Vec<u8>,
    pub central_directory_header: Vec<u8>,
}

#[doc(hidden)]
pub fn zip_header_probe(input: ZipHeaderProbeInput<'_>) -> Result<ZipHeaderProbe, ExportError> {
    let local_file_header = local_file_header(
        input.name,
        ZipCompression::Stored,
        0,
        input.compressed_size,
        input.uncompressed_size,
        false,
        false,
    )?;
    let central_directory_header = central_directory_header(&ZipEntryMeta {
        name: input.name.to_string(),
        compression: ZipCompression::Stored,
        crc32: 0,
        compressed_size: input.compressed_size,
        uncompressed_size: input.uncompressed_size,
        local_header_offset: input.local_header_offset,
        uses_data_descriptor: false,
        force_zip64: false,
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

#[doc(hidden)]
pub fn streaming_zip_header_probe(
    input: StreamingZipHeaderProbeInput<'_>,
) -> Result<StreamingZipHeaderProbe, ExportError> {
    let compression = if input.deflated {
        ZipCompression::Deflated
    } else {
        ZipCompression::Stored
    };
    let force_zip64 = streaming_entry_requires_zip64(compression, input.uncompressed_size);
    let local_file_header = local_file_header(
        input.name,
        compression,
        0,
        input.compressed_size,
        input.uncompressed_size,
        true,
        force_zip64,
    )?;
    let data_descriptor = data_descriptor(
        0,
        input.compressed_size,
        input.uncompressed_size,
        force_zip64,
    )?;
    let central_directory_header = central_directory_header(&ZipEntryMeta {
        name: input.name.to_string(),
        compression,
        crc32: 0,
        compressed_size: input.compressed_size,
        uncompressed_size: input.uncompressed_size,
        local_header_offset: input.local_header_offset,
        uses_data_descriptor: true,
        force_zip64,
    })?;
    Ok(StreamingZipHeaderProbe {
        local_file_header,
        data_descriptor,
        central_directory_header,
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
    sqlx::query(
        r"
        UPDATE export_jobs
        SET status = 'cancelled',
            cancelled_at = CURRENT_TIMESTAMP,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
          AND status IN ('queued', 'running', 'finalizing')
        ",
    )
    .bind(job_id)
    .execute(pool)
    .await?;
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
        LEFT JOIN blob_locations l
         ON l.blob_id = a.blob_id
         AND l.backend NOT GLOB '_vault_pending:*'
         AND l.backend NOT GLOB '_vault_deleting:*'
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
    let artifact = match create_export_zip(
        pool,
        storage,
        transfers_path,
        job_id,
        &downloads,
        zip_options,
    )
    .await
    {
        Ok(artifact) => artifact,
        Err(error) => {
            let _ = fs::remove_file(export_temp_path(transfers_path, job_id)).await;
            return Err(error);
        }
    };
    let result = async {
        ensure_export_not_cancelled(pool, job_id).await?;
        let finalizing = sqlx::query(
            r"
            UPDATE export_jobs
            SET status = 'finalizing',
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
              AND status = 'running'
            ",
        )
        .bind(job_id)
        .execute(pool)
        .await?;
        if finalizing.rows_affected() == 0 {
            return Err(ExportError::ExportCancelled);
        }
        ensure_export_not_cancelled(pool, job_id).await?;
        persist_export_artifact(pool, storage, job_id, &artifact).await
    }
    .await;
    let _ = fs::remove_file(&artifact.path).await;
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
    let updated = sqlx::query(
        r"
        UPDATE export_jobs
        SET total_items = ?,
            total_bytes = ?,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
          AND status = 'running'
        ",
    )
    .bind(total_items)
    .bind(total_bytes)
    .bind(job_id)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ExportError::ExportCancelled);
    }
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

async fn record_export_byte_progress(
    pool: &SqlitePool,
    job_id: &str,
    processed_bytes: i64,
) -> Result<(), ExportError> {
    if processed_bytes <= 0 {
        return Ok(());
    }
    ensure_export_not_cancelled(pool, job_id).await?;
    let updated = sqlx::query(
        r"
        UPDATE export_jobs
        SET processed_bytes = CASE
                WHEN processed_bytes + ? > total_bytes THEN total_bytes
                ELSE processed_bytes + ?
            END,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
          AND status = 'running'
        ",
    )
    .bind(processed_bytes)
    .bind(processed_bytes)
    .bind(job_id)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ExportError::ExportCancelled);
    }
    Ok(())
}

async fn record_export_item_complete(pool: &SqlitePool, job_id: &str) -> Result<(), ExportError> {
    ensure_export_not_cancelled(pool, job_id).await?;
    let updated = sqlx::query(
        r"
        UPDATE export_jobs
        SET processed_items = processed_items + 1,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
          AND status = 'running'
        ",
    )
    .bind(job_id)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ExportError::ExportCancelled);
    }
    Ok(())
}

async fn create_export_zip(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    transfers_path: &Path,
    job_id: &str,
    downloads: &[VersionDownload],
    zip_options: ExportZipOptions,
) -> Result<ExportZipArtifact, ExportError> {
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
        let compression_plan =
            export_entry_compression_plan(&archive_name, download, total_export_bytes, zip_options);
        let mut write_context = ZipWriteContext {
            pool,
            storage,
            job_id,
            file: &mut file,
            zip_hasher: &mut zip_hasher,
            offset: &mut offset,
        };
        let entry = write_zip_entry_streaming(
            &mut write_context,
            &archive_name,
            download,
            compression_plan,
            total_export_bytes,
            zip_options,
        )
        .await?;
        entries.push(entry);
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
    Ok(ExportZipArtifact {
        path: temp_path,
        digest: lower_hex(&zip_hasher.finalize()),
        size_bytes: offset,
    })
}

// The entry pipeline intentionally keeps source validation, progress, compression state, and ZIP
// emission in one ordered failure boundary so no partially verified entry can be published.
#[allow(clippy::too_many_lines)]
async fn write_zip_entry_streaming(
    context: &mut ZipWriteContext<'_>,
    archive_name: &str,
    download: &VersionDownload,
    compression_plan: ZipCompressionPlan,
    total_export_bytes: i64,
    zip_options: ExportZipOptions,
) -> Result<ZipEntryMeta, ExportError> {
    if download.hash_algo != "sha256" {
        return Err(ExportError::BlobContentMismatch);
    }
    let expected_size =
        u64::try_from(download.size_bytes).map_err(|_| ExportError::ZipLimitExceeded)?;
    let mut compression_permit = match compression_plan {
        ZipCompressionPlan::Stored => None,
        ZipCompressionPlan::Deflated | ZipCompressionPlan::Sample => {
            Some(acquire_export_compression_permit(context.pool, context.job_id).await?)
        }
    };
    let mut source = open_export_source(
        context,
        download,
        BlobReadRange {
            expected_size,
            offset: 0,
            length: expected_size,
        },
    )
    .await?;

    let mut replay = VecDeque::new();
    let compression = match compression_plan {
        ZipCompressionPlan::Stored => ZipCompression::Stored,
        ZipCompressionPlan::Deflated => ZipCompression::Deflated,
        ZipCompressionPlan::Sample => {
            let (sample, remainder) =
                read_compression_sample(context.pool, context.job_id, &mut source, expected_size)
                    .await?;
            let permit = compression_permit
                .take()
                .ok_or(ExportError::CompressionTaskFailed)?;
            let ((compression, sample), permit) =
                sampled_zip_compression_offloaded(permit, sample, total_export_bytes, zip_options)
                    .await?;
            if compression == ZipCompression::Deflated {
                compression_permit = Some(permit);
            }
            ensure_export_not_cancelled(context.pool, context.job_id).await?;
            if !sample.is_empty() {
                replay.push_back(Bytes::from(sample));
            }
            if let Some(remainder) = remainder {
                replay.push_back(remainder);
            }
            compression
        }
    };

    // A data descriptor cannot change width after the local header has been emitted. Deflated
    // output has no trustworthy size bound at that point, so reserve ZIP64 up front.
    let force_zip64 = streaming_entry_requires_zip64(compression, expected_size);
    let local_header_offset = *context.offset;
    let local_header = local_file_header(
        archive_name,
        compression,
        0,
        expected_size,
        expected_size,
        true,
        force_zip64,
    )?;
    write_counted(
        context.file,
        context.zip_hasher,
        context.offset,
        &local_header,
    )
    .await?;

    let crc = Crc::<u32>::new(&CRC_32_ISO_HDLC);
    let mut crc_digest = crc.digest();
    let mut source_hasher = Sha256::new();
    let mut source_size = 0_u64;
    let mut compressed_size = 0_u64;
    let mut pending_progress = 0_i64;
    let mut bytes_since_cancel_check = 0_usize;
    let mut deflater = (compression == ZipCompression::Deflated).then(|| {
        Compress::new(
            Compression::new(zip_options.compresslevel.clamp(1, 9)),
            false,
        )
    });
    let mut deflate_batch = Vec::new();
    let mut deflate_batch_bytes = 0_usize;

    loop {
        let chunk = if let Some(chunk) = replay.pop_front() {
            Some(chunk)
        } else {
            next_export_source_chunk(context.pool, context.job_id, &mut source).await?
        };
        let Some(chunk) = chunk else {
            break;
        };
        validate_source_frame(&chunk)?;
        let chunk_size = u64::try_from(chunk.len()).map_err(|_| ExportError::ZipLimitExceeded)?;
        source_size = source_size
            .checked_add(chunk_size)
            .ok_or(ExportError::ZipLimitExceeded)?;
        if source_size > expected_size {
            return Err(ExportError::BlobContentMismatch);
        }
        crc_digest.update(&chunk);
        source_hasher.update(&chunk);

        match compression {
            ZipCompression::Stored => {
                write_counted(context.file, context.zip_hasher, context.offset, &chunk).await?;
                compressed_size = compressed_size
                    .checked_add(chunk_size)
                    .ok_or(ExportError::ZipLimitExceeded)?;
                add_export_byte_progress(
                    context.pool,
                    context.job_id,
                    &mut pending_progress,
                    chunk_size,
                )
                .await?;
            }
            ZipCompression::Deflated => {
                deflate_batch_bytes = deflate_batch_bytes
                    .checked_add(chunk.len())
                    .ok_or(ExportError::ZipLimitExceeded)?;
                deflate_batch.push(chunk);
                if deflate_batch_bytes >= EXPORT_COMPRESSION_BATCH_BYTES {
                    let compressor = deflater.take().ok_or(ExportError::CompressionTaskFailed)?;
                    let permit = compression_permit
                        .take()
                        .ok_or(ExportError::CompressionTaskFailed)?;
                    let (result, permit) = deflate_batch_offloaded(
                        permit,
                        compressor,
                        std::mem::take(&mut deflate_batch),
                    )
                    .await?;
                    ensure_export_not_cancelled(context.pool, context.job_id).await?;
                    write_compressed_output(context, &result.output, &mut compressed_size).await?;
                    if result.total_in != source_size || result.total_out != compressed_size {
                        return Err(ExportError::CompressionTaskFailed);
                    }
                    add_export_byte_progress(
                        context.pool,
                        context.job_id,
                        &mut pending_progress,
                        u64::try_from(deflate_batch_bytes)
                            .map_err(|_| ExportError::ZipLimitExceeded)?,
                    )
                    .await?;
                    deflate_batch_bytes = 0;
                    deflater = Some(result.compressor);
                    compression_permit = Some(permit);
                }
            }
        }

        bytes_since_cancel_check = bytes_since_cancel_check
            .checked_add(usize::try_from(chunk_size).map_err(|_| ExportError::ZipLimitExceeded)?)
            .ok_or(ExportError::ZipLimitExceeded)?;
        if bytes_since_cancel_check >= EXPORT_CANCEL_CHECK_CHUNK_BYTES {
            ensure_export_not_cancelled(context.pool, context.job_id).await?;
            bytes_since_cancel_check = 0;
        }
    }

    if source_size != expected_size {
        return Err(ExportError::BlobContentMismatch);
    }
    let digest = lower_hex(&source_hasher.finalize());
    if digest != download.hash {
        return Err(ExportError::BlobContentMismatch);
    }

    if compression == ZipCompression::Deflated {
        if !deflate_batch.is_empty() {
            let compressor = deflater.take().ok_or(ExportError::CompressionTaskFailed)?;
            let permit = compression_permit
                .take()
                .ok_or(ExportError::CompressionTaskFailed)?;
            let (result, permit) =
                deflate_batch_offloaded(permit, compressor, deflate_batch).await?;
            ensure_export_not_cancelled(context.pool, context.job_id).await?;
            write_compressed_output(context, &result.output, &mut compressed_size).await?;
            if result.total_in != source_size || result.total_out != compressed_size {
                return Err(ExportError::CompressionTaskFailed);
            }
            add_export_byte_progress(
                context.pool,
                context.job_id,
                &mut pending_progress,
                u64::try_from(deflate_batch_bytes).map_err(|_| ExportError::ZipLimitExceeded)?,
            )
            .await?;
            deflater = Some(result.compressor);
            compression_permit = Some(permit);
        }
        ensure_export_not_cancelled(context.pool, context.job_id).await?;
        let permit = compression_permit
            .take()
            .ok_or(ExportError::CompressionTaskFailed)?;
        let (result, finish_permit) = finish_deflate_offloaded(
            permit,
            deflater.take().ok_or(ExportError::CompressionTaskFailed)?,
        )
        .await?;
        ensure_export_not_cancelled(context.pool, context.job_id).await?;
        write_compressed_output(context, &result.output, &mut compressed_size).await?;
        if result.total_in != source_size || result.total_out != compressed_size {
            return Err(ExportError::CompressionTaskFailed);
        }
        drop(finish_permit);
    }
    if !force_zip64 && compressed_size >= ZIP_FIELD_U32_MAX {
        return Err(ExportError::ZipLimitExceeded);
    }
    if pending_progress > 0 {
        record_export_byte_progress(context.pool, context.job_id, pending_progress).await?;
    }

    let crc32 = crc_digest.finalize();
    ensure_export_not_cancelled(context.pool, context.job_id).await?;
    let descriptor = data_descriptor(crc32, compressed_size, expected_size, force_zip64)?;
    write_counted(
        context.file,
        context.zip_hasher,
        context.offset,
        &descriptor,
    )
    .await?;
    record_export_item_complete(context.pool, context.job_id).await?;
    Ok(ZipEntryMeta {
        name: archive_name.to_string(),
        compression,
        crc32,
        compressed_size,
        uncompressed_size: expected_size,
        local_header_offset,
        uses_data_descriptor: true,
        force_zip64,
    })
}

struct DeflateBatchResult {
    compressor: Compress,
    output: Vec<u8>,
    total_in: u64,
    total_out: u64,
}

struct DeflateFinishResult {
    output: Vec<u8>,
    total_in: u64,
    total_out: u64,
}

async fn open_export_source(
    context: &ZipWriteContext<'_>,
    download: &VersionDownload,
    range: BlobReadRange,
) -> Result<BlobByteStream, ExportError> {
    let open = context.storage.stream_location_range(
        &download.backend,
        &download.bucket,
        &download.object_key,
        range,
    );
    tokio::pin!(open);
    loop {
        tokio::select! {
            result = &mut open => return result.map_err(ExportError::from),
            () = tokio::time::sleep(EXPORT_SOURCE_CANCEL_POLL_INTERVAL) => {
                ensure_export_not_cancelled(context.pool, context.job_id).await?;
            }
        }
    }
}

async fn read_compression_sample(
    pool: &SqlitePool,
    job_id: &str,
    source: &mut BlobByteStream,
    expected_size: u64,
) -> Result<(Vec<u8>, Option<Bytes>), ExportError> {
    let sample_target = usize::try_from(
        expected_size.min(
            u64::try_from(EXPORT_COMPRESSION_SAMPLE_BYTES)
                .map_err(|_| ExportError::ZipLimitExceeded)?,
        ),
    )
    .map_err(|_| ExportError::ZipLimitExceeded)?;
    let mut sample = Vec::with_capacity(sample_target);
    let mut remainder = None;
    while sample.len() < sample_target {
        let mut chunk = next_export_source_chunk(pool, job_id, source)
            .await?
            .ok_or(ExportError::BlobContentMismatch)?;
        validate_source_frame(&chunk)?;
        let observed = sample
            .len()
            .checked_add(chunk.len())
            .ok_or(ExportError::ZipLimitExceeded)?;
        if u64::try_from(observed).map_err(|_| ExportError::ZipLimitExceeded)? > expected_size {
            return Err(ExportError::BlobContentMismatch);
        }
        let needed = sample_target - sample.len();
        if chunk.len() > needed {
            let prefix = chunk.split_to(needed);
            sample.extend_from_slice(&prefix);
            remainder = Some(chunk);
        } else {
            sample.extend_from_slice(&chunk);
        }
    }
    Ok((sample, remainder))
}

async fn next_export_source_chunk(
    pool: &SqlitePool,
    job_id: &str,
    source: &mut BlobByteStream,
) -> Result<Option<Bytes>, ExportError> {
    loop {
        tokio::select! {
            item = source.next() => {
                return item.transpose().map_err(ExportError::from);
            }
            () = tokio::time::sleep(EXPORT_SOURCE_CANCEL_POLL_INTERVAL) => {
                ensure_export_not_cancelled(pool, job_id).await?;
            }
        }
    }
}

fn validate_source_frame(chunk: &Bytes) -> Result<(), ExportError> {
    if chunk.is_empty() || chunk.len() > STORAGE_CHUNK_SIZE {
        return Err(ExportError::BlobContentMismatch);
    }
    Ok(())
}

async fn sampled_zip_compression_offloaded(
    permit: OwnedSemaphorePermit,
    sample: Vec<u8>,
    total_export_bytes: i64,
    options: ExportZipOptions,
) -> Result<((ZipCompression, Vec<u8>), OwnedSemaphorePermit), ExportError> {
    run_compression_task(permit, move || {
        let compression = sampled_zip_compression(&sample, total_export_bytes, options)?;
        Ok((compression, sample))
    })
    .await
}

async fn deflate_batch_offloaded(
    permit: OwnedSemaphorePermit,
    mut compressor: Compress,
    chunks: Vec<Bytes>,
) -> Result<(DeflateBatchResult, OwnedSemaphorePermit), ExportError> {
    run_compression_task(permit, move || {
        let mut output = Vec::new();
        for chunk in chunks {
            compress_deflate_input(&mut compressor, &chunk, &mut output)?;
        }
        Ok(DeflateBatchResult {
            total_in: compressor.total_in(),
            total_out: compressor.total_out(),
            compressor,
            output,
        })
    })
    .await
}

async fn finish_deflate_offloaded(
    permit: OwnedSemaphorePermit,
    mut compressor: Compress,
) -> Result<(DeflateFinishResult, OwnedSemaphorePermit), ExportError> {
    run_compression_task(permit, move || {
        let mut output = Vec::new();
        loop {
            output.reserve(EXPORT_COMPRESSION_OUTPUT_CHUNK_BYTES);
            let previous_out = compressor.total_out();
            let status = compressor
                .compress_vec(&[], &mut output, FlushCompress::Finish)
                .map_err(|_| ExportError::CompressionTaskFailed)?;
            let produced = compressor.total_out() - previous_out;
            if status == Status::StreamEnd {
                break;
            }
            if produced == 0 {
                return Err(ExportError::CompressionTaskFailed);
            }
        }
        Ok(DeflateFinishResult {
            total_in: compressor.total_in(),
            total_out: compressor.total_out(),
            output,
        })
    })
    .await
}

fn compress_deflate_input(
    compressor: &mut Compress,
    input: &[u8],
    output: &mut Vec<u8>,
) -> Result<(), ExportError> {
    let mut consumed = 0_usize;
    while consumed < input.len() {
        output.reserve(EXPORT_COMPRESSION_OUTPUT_CHUNK_BYTES);
        let previous_in = compressor.total_in();
        let previous_out = compressor.total_out();
        let status = compressor
            .compress_vec(&input[consumed..], output, FlushCompress::None)
            .map_err(|_| ExportError::CompressionTaskFailed)?;
        if status == Status::StreamEnd {
            return Err(ExportError::CompressionTaskFailed);
        }
        let consumed_now = usize::try_from(compressor.total_in() - previous_in)
            .map_err(|_| ExportError::ZipLimitExceeded)?;
        let produced_now = compressor.total_out() - previous_out;
        if consumed_now == 0 && produced_now == 0 {
            return Err(ExportError::CompressionTaskFailed);
        }
        consumed = consumed
            .checked_add(consumed_now)
            .ok_or(ExportError::ZipLimitExceeded)?;
        if consumed > input.len() {
            return Err(ExportError::CompressionTaskFailed);
        }
    }
    Ok(())
}

fn export_compression_slots() -> Arc<Semaphore> {
    static COMPRESSION_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    // Tokio's blocking pool is much larger than this workload should consume. One permit covers
    // the complete entry, including bounded input/output buffers and asynchronous file writes.
    COMPRESSION_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(EXPORT_COMPRESSION_TASKS)))
        .clone()
}

async fn acquire_export_compression_permit(
    pool: &SqlitePool,
    job_id: &str,
) -> Result<OwnedSemaphorePermit, ExportError> {
    let slots = export_compression_slots();
    let acquire = slots.acquire_owned();
    tokio::pin!(acquire);
    loop {
        tokio::select! {
            permit = &mut acquire => {
                return permit.map_err(|_| ExportError::CompressionTaskFailed);
            }
            () = tokio::time::sleep(EXPORT_SOURCE_CANCEL_POLL_INTERVAL) => {
                ensure_export_not_cancelled(pool, job_id).await?;
            }
        }
    }
}

async fn run_compression_task<T, F>(
    permit: OwnedSemaphorePermit,
    task: F,
) -> Result<(T, OwnedSemaphorePermit), ExportError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ExportError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let result = task()?;
        Ok((result, permit))
    })
    .await
    .map_err(|_| ExportError::CompressionTaskFailed)?
}

async fn write_compressed_output(
    context: &mut ZipWriteContext<'_>,
    output: &[u8],
    compressed_size: &mut u64,
) -> Result<(), ExportError> {
    write_counted_checked(
        context.pool,
        context.job_id,
        context.file,
        context.zip_hasher,
        context.offset,
        output,
    )
    .await?;
    *compressed_size = compressed_size
        .checked_add(u64::try_from(output.len()).map_err(|_| ExportError::ZipLimitExceeded)?)
        .ok_or(ExportError::ZipLimitExceeded)?;
    Ok(())
}

async fn add_export_byte_progress(
    pool: &SqlitePool,
    job_id: &str,
    pending_progress: &mut i64,
    bytes: u64,
) -> Result<(), ExportError> {
    *pending_progress = pending_progress
        .checked_add(i64::try_from(bytes).map_err(|_| ExportError::ZipLimitExceeded)?)
        .ok_or(ExportError::ZipLimitExceeded)?;
    if *pending_progress >= EXPORT_PROGRESS_UPDATE_BYTES {
        record_export_byte_progress(pool, job_id, *pending_progress).await?;
        *pending_progress = 0;
    }
    Ok(())
}

fn export_temp_path(transfers_path: &Path, job_id: &str) -> PathBuf {
    transfers_path
        .join("exports")
        .join(format!("{job_id}.zip.tmp"))
}

// Publication and artifact metadata must remain a single visibly ordered failure boundary.
#[allow(clippy::too_many_lines)]
async fn persist_export_artifact(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    job_id: &str,
    artifact: &ExportZipArtifact,
) -> Result<(), ExportError> {
    ensure_export_not_cancelled(pool, job_id).await?;
    let publication = begin_blob_publication(
        pool,
        storage,
        "sha256",
        &artifact.digest,
        artifact.size_bytes,
        BlobWriteKind::File,
    )
    .await?;
    let stored = match publication
        .run_storage(storage.put_file(&artifact.path, &artifact.digest, artifact.size_bytes))
        .await
    {
        Ok(stored) => stored,
        Err(error) => {
            if let Err(cleanup_error) = publication.abandon(None).await {
                tracing::error!(
                    ?cleanup_error,
                    "failed to queue an unsuccessful export publication for cleanup"
                );
            }
            return Err(error.into());
        }
    };
    let metadata_result = async {
        ensure_export_not_cancelled(pool, job_id).await?;
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let blob_id = publication
            .prepare_metadata_in_tx(&mut transaction, &stored)
            .await?;
        let job = sqlx::query_as::<_, (String, String)>(
            r"
            UPDATE export_jobs
            SET status = 'complete',
                processed_items = total_items,
                processed_bytes = total_bytes,
                completed_at = CURRENT_TIMESTAMP,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
              AND status = 'finalizing'
            RETURNING filename, expires_at
            ",
        )
        .bind(job_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some((filename, expires_at)) = job else {
            transaction.rollback().await?;
            return Err(ExportError::ExportCancelled);
        };
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
        .bind(&filename)
        .bind(i64::try_from(stored.size_bytes).map_err(|_| ExportError::ZipLimitExceeded)?)
        .bind(&stored.digest)
        .bind(&expires_at)
        .execute(&mut *transaction)
        .await?;
        publication.finish_metadata_in_tx(&mut transaction).await?;
        transaction.commit().await?;
        Ok(())
    }
    .await;
    if let Err(error) = metadata_result {
        tracing::warn!(
            object_key = %stored.object_key,
            "export artifact promotion was not committed; queueing the object for delayed cleanup"
        );
        if let Err(cleanup_error) = publication.abandon(Some(&stored)).await {
            tracing::error!(
                ?cleanup_error,
                object_key = %stored.object_key,
                "failed to preserve export object metadata for delayed cleanup"
            );
        } else if let Err(cleanup_error) =
            collect_unreferenced_blobs_with_limit(pool, storage, 1).await
        {
            tracing::warn!(?cleanup_error, "prompt export-object cleanup failed");
        }
        return Err(error);
    }
    Ok(())
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

fn export_entry_compression_plan(
    archive_name: &str,
    download: &VersionDownload,
    total_export_bytes: i64,
    options: ExportZipOptions,
) -> ZipCompressionPlan {
    if !export_zip_compression_enabled(total_export_bytes, options) {
        return ZipCompressionPlan::Stored;
    }
    if export_entry_is_known_stored(archive_name, download.mime_type.as_deref()) {
        return ZipCompressionPlan::Stored;
    }
    if export_entry_is_known_compressible(archive_name, download.mime_type.as_deref()) {
        return ZipCompressionPlan::Deflated;
    }
    if download.size_bytes >= EXPORT_STREAM_STORED_ENTRY_BYTES {
        return ZipCompressionPlan::Stored;
    }
    ZipCompressionPlan::Sample
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

fn sampled_zip_compression(
    data: &[u8],
    total_export_bytes: i64,
    options: ExportZipOptions,
) -> Result<ZipCompression, ExportError> {
    if !export_zip_compression_enabled(total_export_bytes, options) {
        return Ok(ZipCompression::Stored);
    }
    if data.is_empty() {
        return Ok(ZipCompression::Stored);
    }
    let sample_len = data.len().min(EXPORT_COMPRESSION_SAMPLE_BYTES);
    let sample = &data[..sample_len];
    let compressed = zlib_bytes(sample, options.compresslevel)?;
    if compressed.len()
        <= sample.len() * EXPORT_COMPRESSION_MIN_RATIO_NUMERATOR
            / EXPORT_COMPRESSION_MIN_RATIO_DENOMINATOR
    {
        Ok(ZipCompression::Deflated)
    } else {
        Ok(ZipCompression::Stored)
    }
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
    uses_data_descriptor: bool,
    force_zip64: bool,
) -> Result<Vec<u8>, ExportError> {
    let name_bytes = name.as_bytes();
    let needs_zip64 = force_zip64 || zip64_sizes_needed(compressed_size, uncompressed_size);
    let extra = if needs_zip64 {
        if uses_data_descriptor {
            zip64_extra_field(&[0, 0])?
        } else {
            zip64_extra_field(&[uncompressed_size, compressed_size])?
        }
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
    push_u16(
        &mut header,
        if uses_data_descriptor {
            ZIP_GENERAL_PURPOSE_DATA_DESCRIPTOR
        } else {
            0
        },
    );
    push_u16(&mut header, compression.method_code());
    push_u16(&mut header, 0);
    push_u16(&mut header, ZIP_DOS_DATE_1980_01_01);
    push_u32(&mut header, if uses_data_descriptor { 0 } else { crc32 });
    if uses_data_descriptor {
        // APPNOTE requires all three local value fields to be zero when bit 3 is set. A ZIP64
        // descriptor is advertised by version 4.5 plus the zero-placeholder ZIP64 extra field.
        push_u32(&mut header, 0);
        push_u32(&mut header, 0);
    } else {
        push_zip_u32_or_zip64(&mut header, compressed_size, needs_zip64)?;
        push_zip_u32_or_zip64(&mut header, uncompressed_size, needs_zip64)?;
    }
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
    push_u16(
        &mut header,
        if entry.uses_data_descriptor {
            ZIP_GENERAL_PURPOSE_DATA_DESCRIPTOR
        } else {
            0
        },
    );
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

fn data_descriptor(
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    force_zip64: bool,
) -> Result<Vec<u8>, ExportError> {
    let uses_zip64 = force_zip64 || zip64_sizes_needed(compressed_size, uncompressed_size);
    let mut descriptor = Vec::with_capacity(if uses_zip64 { 24 } else { 16 });
    push_u32(&mut descriptor, 0x0807_4b50);
    push_u32(&mut descriptor, crc32);
    if uses_zip64 {
        push_u64(&mut descriptor, compressed_size);
        push_u64(&mut descriptor, uncompressed_size);
    } else {
        push_u32(&mut descriptor, checked_zip_u32(compressed_size)?);
        push_u32(&mut descriptor, checked_zip_u32(uncompressed_size)?);
    }
    Ok(descriptor)
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
    compressed_size >= ZIP_FIELD_U32_MAX || uncompressed_size >= ZIP_FIELD_U32_MAX
}

fn streaming_entry_requires_zip64(compression: ZipCompression, uncompressed_size: u64) -> bool {
    compression == ZipCompression::Deflated || uncompressed_size >= ZIP_FIELD_U32_MAX
}

fn zip64_central_header_needed(entry: &ZipEntryMeta) -> bool {
    entry.force_zip64
        || zip64_sizes_needed(entry.compressed_size, entry.uncompressed_size)
        || entry.local_header_offset >= ZIP_FIELD_U32_MAX
}

fn zip64_end_record_needed(
    entry_count: usize,
    central_directory_size: u64,
    central_directory_offset: u64,
) -> bool {
    entry_count >= ZIP_FIELD_U16_MAX
        || central_directory_size >= ZIP_FIELD_U32_MAX
        || central_directory_offset >= ZIP_FIELD_U32_MAX
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
    if value >= ZIP_FIELD_U16_MAX {
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
    if force_zip64 || value >= ZIP_FIELD_U32_MAX {
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
