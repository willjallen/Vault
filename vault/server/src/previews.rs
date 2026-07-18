//! Durable, content-addressed preview orchestration.
//!
//! Preview jobs are keyed by an immutable source blob and a versioned recipe.
//! Object paths and document names are deliberately absent from that identity,
//! so rename/move operations never invalidate derived bytes. Providers render
//! into a small, fixed rendition set; each output is published through the same
//! crash-recoverable blob lifecycle as uploads and exports.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::auth::UserContext;
use crate::blob_lifecycle::{
    BlobLifecycleError, PendingBlobPublication, begin_blob_publication,
    collect_unreferenced_blob_candidates,
};
use crate::documents::parse_archived_access;
use crate::folders::{ARCHIVE_ROOT_KEY, FolderError, folder_access_levels};
use crate::state_events::{notify_state_event_committed, record_state_event};
use crate::storage::{
    BlobLocation, BlobReadRange, BlobWriteKind, SharedBlobStorage, StorageError, StoredBlob,
    open_ranked_location_stream, sha256_hex,
};

mod raster;

pub use raster::RasterPreviewProvider;

pub const PREVIEW_RECIPE: &str = "raster-v1";
pub const PREVIEW_VARIANT_SMALL: &str = "small";
pub const PREVIEW_VARIANT_MEDIUM: &str = "medium";
pub const PREVIEW_VARIANT_LARGE: &str = "large";
const PREVIEW_VARIANTS: [(&str, i64); 3] = [
    (PREVIEW_VARIANT_SMALL, 128),
    (PREVIEW_VARIANT_MEDIUM, 256),
    (PREVIEW_VARIANT_LARGE, 512),
];

#[must_use]
pub fn is_supported_preview_rendition(recipe: &str, variant: &str) -> bool {
    recipe == PREVIEW_RECIPE && PREVIEW_VARIANTS.iter().any(|(name, _)| *name == variant)
}

const PREVIEW_LEASE_SECONDS: i64 = 300;
const PREVIEW_RETRY_LIMIT: i64 = 3;
const PREVIEW_TERMINAL_RETRY_COOLDOWN: &str = "-15 minutes";
const PREVIEW_SNIFF_BYTES: u64 = 32;
const PREVIEW_SNIFF_TIMEOUT: Duration = Duration::from_secs(10);
const PREVIEW_SOURCE_MAX_BYTES: u64 = 128 * 1024 * 1024;
const PREVIEW_OUTPUT_MAX_BYTES: usize = 16 * 1024 * 1024;
const PREVIEW_OUTPUT_MAX_DIMENSION: i64 = 8_192;
const PREVIEW_RENDER_TIMEOUT: Duration = Duration::from_secs(30);
const PREVIEW_SOURCE_READ_TIMEOUT: Duration = Duration::from_mins(1);
const PREVIEW_RESOLVE_MAX_DOCUMENTS: usize = 200;
const PREVIEW_EVENT_COALESCE: Duration = Duration::from_millis(100);
const PREVIEW_EVENT_RETRY_DELAY: Duration = Duration::from_secs(1);
const PREVIEW_IDLE_POLL: Duration = Duration::from_secs(5);
const PREVIEW_MAINTENANCE_INTERVAL: Duration = Duration::from_hours(1);
const PREVIEW_ACCESS_TOUCH_AGE: &str = "-1 day";
const PREVIEW_CACHE_MAX_BYTES: i64 = 2 * 1024 * 1024 * 1024;
const PREVIEW_HISTORICAL_MAX_AGE: Duration = Duration::from_hours(2_160);

#[derive(Debug, Error)]
pub enum PreviewError {
    #[error("preview request contains too many documents")]
    TooManyDocuments,
    #[error("preview resolve request is invalid")]
    InvalidResolveRequest,
    #[error("document not found")]
    DocumentNotFound,
    #[error("insufficient document access")]
    InsufficientDocumentAccess,
    #[error("preview recipe or variant was not found")]
    RenditionNotFound,
    #[error("preview provider does not support this content")]
    Unsupported,
    #[error("preview provider returned an invalid rendition set")]
    InvalidProviderOutput,
    #[error("preview job lease was lost")]
    LeaseLost,
    #[error("preview source is too large")]
    SourceTooLarge,
    #[error("preview source metadata or bytes are invalid")]
    InvalidSource,
    #[error("preview source read timed out")]
    SourceReadTimeout,
    #[error("preview rendering timed out")]
    RenderTimeout,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    BlobLifecycle(#[from] BlobLifecycleError),
    #[error(transparent)]
    Folder(#[from] FolderError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VisualPayload {
    pub icon_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<PreviewDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewDescriptor {
    pub version_id: String,
    pub recipe: String,
    pub status: String,
    pub variants: Vec<PreviewVariantPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewVariantPayload {
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub mime_type: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct VisualSource<'a> {
    pub document_id: i64,
    pub name: &'a str,
    pub version_id: Option<&'a str>,
    pub blob_id: Option<i64>,
    pub mime_type: Option<&'a str>,
    pub can_read: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolvePreviewRequest {
    pub documents: Vec<ResolvePreviewDocumentRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvePreviewDocumentRequest {
    pub document_id: i64,
    pub version_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvePreviewResponse {
    pub documents: Vec<ResolvedPreviewDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedPreviewDocument {
    pub document_id: i64,
    pub visual: VisualPayload,
}

#[derive(Debug, Clone)]
pub struct AuthorizedPreviewSource {
    pub document_id: i64,
    pub version_id: String,
    pub name: String,
    pub blob_id: i64,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPreview {
    pub variant: String,
    pub mime_type: String,
    pub width: i64,
    pub height: i64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PreviewRenderRequest {
    pub source_bytes: Vec<u8>,
    pub source_mime_type: Option<String>,
    pub source_filename: Option<String>,
}

#[async_trait]
pub trait PreviewProvider: std::fmt::Debug + Send + Sync {
    /// A cheap hint check performed before the source object is downloaded.
    fn supports(&self, mime_type: Option<&str>, filename: Option<&str>) -> bool;

    /// A bounded magic-byte check used when metadata hints are missing or
    /// misleading. Providers should never trust a filename or MIME alone.
    fn supports_bytes(&self, _prefix: &[u8]) -> bool {
        false
    }

    async fn render(
        &self,
        request: PreviewRenderRequest,
    ) -> Result<Vec<RenderedPreview>, PreviewProviderFailure>;
}

#[derive(Debug, Error)]
pub enum PreviewProviderFailure {
    #[error("unsupported content")]
    Unsupported,
    #[error("invalid or corrupt content")]
    InvalidContent,
    #[error("preview rendering failed")]
    Failed,
}

/// A deterministic provider useful for installations and tests that explicitly
/// disable content preview generation.
#[derive(Debug, Default)]
pub struct UnsupportedPreviewProvider;

#[async_trait]
impl PreviewProvider for UnsupportedPreviewProvider {
    fn supports(&self, _mime_type: Option<&str>, _filename: Option<&str>) -> bool {
        false
    }

    async fn render(
        &self,
        _request: PreviewRenderRequest,
    ) -> Result<Vec<RenderedPreview>, PreviewProviderFailure> {
        Err(PreviewProviderFailure::Unsupported)
    }
}

#[derive(Debug)]
pub struct PreviewExecutionContext {
    provider: Arc<dyn PreviewProvider>,
    wake: Notify,
    changes: Notify,
    dirty: AtomicBool,
    started: AtomicBool,
    shutdown: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl PreviewExecutionContext {
    #[must_use]
    pub fn new() -> Self {
        Self::with_provider(Arc::new(RasterPreviewProvider::default()))
    }

    #[must_use]
    pub fn with_provider(provider: Arc<dyn PreviewProvider>) -> Self {
        Self {
            provider,
            wake: Notify::new(),
            changes: Notify::new(),
            dirty: AtomicBool::new(false),
            started: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
        }
    }

    pub async fn start(
        self: &Arc<Self>,
        pool: SqlitePool,
        storage: SharedBlobStorage,
        workers: usize,
    ) -> Result<(), PreviewError> {
        if self.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        recover_interrupted_jobs(&pool).await?;

        let mut tasks = self.tasks.lock().await;
        for worker_index in 0..workers.clamp(1, 8) {
            let context = Arc::clone(self);
            let worker_pool = pool.clone();
            let worker_storage = Arc::clone(&storage);
            tasks.push(tokio::spawn(async move {
                context
                    .worker_loop(worker_index, worker_pool, worker_storage)
                    .await;
            }));
        }
        let context = Arc::clone(self);
        let event_pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            context.event_loop(event_pool).await;
        }));
        let context = Arc::clone(self);
        let maintenance_storage = Arc::clone(&storage);
        tasks.push(tokio::spawn(async move {
            context.maintenance_loop(pool, maintenance_storage).await;
        }));
        drop(tasks);
        self.notify_jobs();
        Ok(())
    }

    pub fn notify_jobs(&self) {
        self.wake.notify_waiters();
    }

    pub fn notify_changed(&self) {
        self.mark_changed();
    }

    pub async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.wake.notify_waiters();
        self.changes.notify_waiters();
        let tasks = std::mem::take(&mut *self.tasks.lock().await);
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    }

    async fn worker_loop(&self, worker_index: usize, pool: SqlitePool, storage: SharedBlobStorage) {
        while !self.shutdown.load(Ordering::Acquire) {
            match claim_preview_job(&pool).await {
                Ok(Some(job)) => {
                    let result = self.process_job(&pool, storage.as_ref(), &job).await;
                    if let Err(error) = finish_job_after_error(&pool, &job, &result).await {
                        tracing::error!(
                            ?error,
                            job_id = job.id,
                            worker_index,
                            "failed to record preview job failure"
                        );
                    }
                    self.mark_changed();
                }
                Ok(None) => {
                    tokio::select! {
                        () = self.wake.notified() => {}
                        () = tokio::time::sleep(PREVIEW_IDLE_POLL) => {}
                    }
                }
                Err(error) => {
                    tracing::error!(?error, worker_index, "preview worker dispatch failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn process_job(
        &self,
        pool: &SqlitePool,
        storage: &dyn crate::storage::BlobStorageBackend,
        job: &ClaimedPreviewJob,
    ) -> Result<(), PreviewError> {
        if !self
            .provider
            .supports(job.mime_type.as_deref(), job.filename.as_deref())
        {
            let prefix =
                tokio::time::timeout(PREVIEW_SNIFF_TIMEOUT, read_preview_prefix(storage, job))
                    .await
                    .map_err(|_| PreviewError::SourceReadTimeout)??;
            if !self.provider.supports_bytes(&prefix) {
                mark_job_unsupported(pool, job, "provider_unavailable").await?;
                return Ok(());
            }
        }
        let source_bytes = tokio::time::timeout(
            PREVIEW_SOURCE_READ_TIMEOUT,
            read_preview_source(storage, job),
        )
        .await
        .map_err(|_| PreviewError::SourceReadTimeout)??;
        renew_job_lease(pool, job).await?;
        let rendered = tokio::time::timeout(
            PREVIEW_RENDER_TIMEOUT,
            self.provider.render(PreviewRenderRequest {
                source_bytes,
                source_mime_type: job.mime_type.clone(),
                source_filename: job.filename.clone(),
            }),
        )
        .await
        .map_err(|_| PreviewError::RenderTimeout)?;
        let outputs = match rendered {
            Ok(outputs) => outputs,
            Err(PreviewProviderFailure::Unsupported) => {
                mark_job_unsupported(pool, job, "unsupported_content").await?;
                return Ok(());
            }
            Err(PreviewProviderFailure::InvalidContent) => {
                return Err(PreviewError::InvalidSource);
            }
            Err(PreviewProviderFailure::Failed) => return Err(PreviewError::InvalidProviderOutput),
        };
        validate_rendered_previews(&outputs)?;
        publish_renditions(pool, storage, job, &outputs).await
    }

    fn mark_changed(&self) {
        self.dirty.store(true, Ordering::Release);
        self.changes.notify_one();
    }

    async fn event_loop(&self, pool: SqlitePool) {
        while !self.shutdown.load(Ordering::Acquire) {
            self.changes.notified().await;
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(PREVIEW_EVENT_COALESCE).await;
            if !self.dirty.swap(false, Ordering::AcqRel) {
                continue;
            }
            match record_state_event(&pool, "preview.changed", &["previews"]).await {
                Ok(()) => notify_state_event_committed(),
                Err(error) => {
                    // `dirty` is the durable-in-process edge for this coalescer. A
                    // failed insert must restore it before retrying or the only
                    // notification for a completed/pruned job can be lost.
                    self.dirty.store(true, Ordering::Release);
                    tracing::error!(?error, "failed to publish preview state event");
                    tokio::time::sleep(PREVIEW_EVENT_RETRY_DELAY).await;
                    self.changes.notify_one();
                }
            }
        }
    }

    async fn maintenance_loop(&self, pool: SqlitePool, storage: SharedBlobStorage) {
        while !self.shutdown.load(Ordering::Acquire) {
            tokio::time::sleep(PREVIEW_MAINTENANCE_INTERVAL).await;
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            match prune_preview_cache(
                &pool,
                PREVIEW_CACHE_MAX_BYTES,
                PREVIEW_HISTORICAL_MAX_AGE,
                256,
            )
            .await
            {
                Ok(pruned) => {
                    self.apply_prune_result(&pool, storage.as_ref(), pruned)
                        .await;
                }
                Err(error) => tracing::warn!(?error, "preview cache pruning failed"),
            }
        }
    }

    #[doc(hidden)]
    pub async fn apply_prune_result(
        &self,
        pool: &SqlitePool,
        storage: &dyn crate::storage::BlobStorageBackend,
        pruned: PreviewPruneResult,
    ) {
        if !pruned.deleted_job_ids.is_empty() {
            self.mark_changed();
        }
        if !pruned.released_blob_ids.is_empty()
            && let Err(error) =
                collect_unreferenced_blob_candidates(pool, storage, &pruned.released_blob_ids).await
        {
            tracing::warn!(?error, "pruned preview blob collection failed");
        }
    }
}

impl Default for PreviewExecutionContext {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, FromRow)]
struct PreviewStateRow {
    source_blob_id: i64,
    job_id: i64,
    recipe: String,
    status: String,
    variant: Option<String>,
    mime_type: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
struct ClaimedPreviewJobRow {
    id: i64,
    source_blob_id: i64,
    attempt_count: i64,
    lease_token: String,
    hash_algo: String,
    hash: String,
    size_bytes: i64,
    mime_type: Option<String>,
    filename: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct ResolveSourceRow {
    ordinal: i64,
    document_id: i64,
    version_id: String,
    name: String,
    folder_id: i64,
    root_key: String,
    archived_access: Option<String>,
    blob_id: i64,
    mime_type: Option<String>,
}

#[derive(Debug, Clone)]
struct ClaimedPreviewJob {
    id: i64,
    attempt_count: i64,
    lease_token: String,
    hash_algo: String,
    hash: String,
    size_bytes: i64,
    mime_type: Option<String>,
    filename: Option<String>,
    locations: Vec<BlobLocation>,
}

#[derive(Debug, Clone, FromRow)]
pub struct PreviewRenditionDownload {
    pub hash_algo: String,
    pub hash: String,
    pub size_bytes: i64,
    pub mime_type: String,
    pub width: i64,
    pub height: i64,
    blob_id: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewPruneResult {
    pub deleted_job_ids: Vec<i64>,
    pub released_blob_ids: Vec<i64>,
}

impl PreviewRenditionDownload {
    pub async fn locations(&self, pool: &SqlitePool) -> Result<Vec<BlobLocation>, PreviewError> {
        Ok(sqlx::query_as::<_, (String, String, String)>(
            r"
            SELECT backend, bucket, object_key
            FROM blob_locations
            WHERE blob_id = ?
              AND backend NOT GLOB '_vault_pending:*'
              AND backend NOT GLOB '_vault_deleting:*'
              AND TRIM(backend) != ''
              AND TRIM(object_key) != ''
            ORDER BY id
            ",
        )
        .bind(self.blob_id)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(backend, bucket, object_key)| BlobLocation {
            backend,
            bucket,
            object_key,
        })
        .collect())
    }
}

#[allow(clippy::too_many_lines)]
pub async fn visual_payloads(
    pool: &SqlitePool,
    sources: &[VisualSource<'_>],
) -> Result<HashMap<i64, VisualPayload>, PreviewError> {
    let blob_ids = sources
        .iter()
        .filter(|source| source.can_read)
        .filter_map(|source| source.blob_id)
        .collect::<Vec<_>>();
    let rows = if blob_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, PreviewStateRow>(
            r"
            SELECT
                pj.source_blob_id,
                pj.id AS job_id,
                pj.recipe,
                pj.status,
                pr.variant,
                pr.mime_type,
                pr.width,
                pr.height
            FROM preview_jobs pj
            LEFT JOIN preview_renditions pr ON pr.preview_job_id = pj.id
            WHERE pj.recipe = ?
              AND pj.source_blob_id IN (
                  SELECT CAST(value AS INTEGER) FROM json_each(?)
              )
            ORDER BY pj.source_blob_id, pr.width, pr.variant
            ",
        )
        .bind(PREVIEW_RECIPE)
        .bind(serde_json::to_string(&blob_ids).map_err(|_| PreviewError::InvalidSource)?)
        .fetch_all(pool)
        .await?
    };
    let mut by_blob: HashMap<i64, (String, String, Vec<&PreviewStateRow>)> = HashMap::new();
    for row in &rows {
        let entry = by_blob.entry(row.source_blob_id).or_insert_with(|| {
            (
                row.recipe.clone(),
                normalized_preview_status(&row.status),
                Vec::new(),
            )
        });
        if row.variant.is_some() {
            entry.2.push(row);
        }
        let _ = row.job_id;
    }
    let mut payloads = HashMap::new();
    for source in sources {
        let preview = match (source.can_read, source.version_id, source.blob_id) {
            (true, Some(version_id), Some(blob_id)) => {
                let (recipe, mut status, variant_rows) = by_blob.get(&blob_id).map_or_else(
                    || {
                        (
                            PREVIEW_RECIPE.to_string(),
                            "pending".to_string(),
                            Vec::new(),
                        )
                    },
                    |(recipe, status, rows)| (recipe.clone(), status.clone(), rows.clone()),
                );
                let mut variants = if status == "ready" {
                    variant_rows
                        .into_iter()
                        .filter_map(|row| {
                            let name = row.variant.clone()?;
                            let expected_dimension =
                                PREVIEW_VARIANTS.iter().find_map(|(variant, dimension)| {
                                    (*variant == name).then_some(*dimension)
                                })?;
                            let width = row.width?;
                            let height = row.height?;
                            let mime_type = row.mime_type.clone()?;
                            if width <= 0
                                || height <= 0
                                || width > expected_dimension
                                || height > expected_dimension
                                || mime_type != "image/webp"
                            {
                                return None;
                            }
                            Some(PreviewVariantPayload {
                                name,
                                width,
                                height,
                                mime_type,
                                url: format!(
                                    "/api/documents/{}/versions/{}/previews/{}/{}",
                                    source.document_id,
                                    percent_encode_segment(version_id),
                                    percent_encode_segment(&recipe),
                                    percent_encode_segment(row.variant.as_deref()?),
                                ),
                            })
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let complete = variants.len() == PREVIEW_VARIANTS.len()
                    && PREVIEW_VARIANTS
                        .iter()
                        .all(|(name, _)| variants.iter().any(|variant| variant.name == *name));
                if status == "ready" && !complete {
                    status = "failed".to_string();
                    variants.clear();
                }
                Some(PreviewDescriptor {
                    version_id: version_id.to_string(),
                    recipe,
                    status,
                    variants,
                })
            }
            _ => None,
        };
        payloads.insert(
            source.document_id,
            VisualPayload {
                icon_key: semantic_icon_key(source.name, source.mime_type),
                preview,
            },
        );
    }
    Ok(payloads)
}

pub async fn enqueue_preview_job_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    source_blob_id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        r"
        INSERT OR IGNORE INTO preview_jobs (source_blob_id, recipe, status)
        VALUES (?, ?, 'queued')
        ",
    )
    .bind(source_blob_id)
    .bind(PREVIEW_RECIPE)
    .execute(&mut **transaction)
    .await?
    .rows_affected()
        == 1)
}

pub async fn enqueue_preview_job(
    pool: &SqlitePool,
    source_blob_id: i64,
) -> Result<bool, PreviewError> {
    Ok(enqueue_preview_jobs(pool, &[source_blob_id]).await? > 0)
}

pub async fn enqueue_preview_jobs(
    pool: &SqlitePool,
    source_blob_ids: &[i64],
) -> Result<u64, PreviewError> {
    if source_blob_ids.is_empty() {
        return Ok(0);
    }
    let source_blob_ids =
        serde_json::to_string(source_blob_ids).map_err(|_| PreviewError::InvalidSource)?;
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let inserted = sqlx::query(
        r"
        INSERT OR IGNORE INTO preview_jobs (source_blob_id, recipe, status)
        SELECT DISTINCT CAST(value AS INTEGER), ?, 'queued'
        FROM json_each(?)
        ",
    )
    .bind(PREVIEW_RECIPE)
    .bind(&source_blob_ids)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    let revived = sqlx::query(
        r"
        UPDATE preview_jobs
        SET status = 'queued',
            attempt_count = 0,
            lease_token = NULL,
            lease_expires_at = NULL,
            next_attempt_at = NULL,
            completed_at = NULL,
            updated_at = CURRENT_TIMESTAMP,
            last_error_code = NULL,
            last_error_detail = NULL
        WHERE recipe = ?
          AND source_blob_id IN (
              SELECT DISTINCT CAST(value AS INTEGER) FROM json_each(?)
          )
          AND status = 'failed'
          AND next_attempt_at IS NULL
          AND last_error_code IN (
              'database',
              'storage',
              'blob_lifecycle',
              'folder',
              'io',
              'lease_lost',
              'source_read_timeout',
              'render_timeout'
          )
          AND datetime(COALESCE(completed_at, updated_at))
              <= datetime('now', ?)
        ",
    )
    .bind(PREVIEW_RECIPE)
    .bind(&source_blob_ids)
    .bind(PREVIEW_TERMINAL_RETRY_COOLDOWN)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    transaction.commit().await?;
    Ok(inserted.saturating_add(revived))
}

/// Authorizes a resolve batch with a constant number of database queries:
/// one requested document/version lookup, one inherited-folder access lookup,
/// and (only for archived rows) one group-id lookup. The query count is
/// independent of the bounded request length.
#[allow(clippy::too_many_lines)]
pub async fn authorize_resolve_sources(
    pool: &SqlitePool,
    user: &UserContext,
    request: &ResolvePreviewRequest,
) -> Result<Vec<AuthorizedPreviewSource>, PreviewError> {
    validate_resolve_request(request)?;
    if request.documents.is_empty() {
        return Ok(Vec::new());
    }
    let requested_json =
        serde_json::to_string(&request.documents).map_err(|_| PreviewError::InvalidSource)?;
    let rows = sqlx::query_as::<_, ResolveSourceRow>(
        r"
        WITH requested AS (
            SELECT
                CAST(key AS INTEGER) AS ordinal,
                CAST(json_extract(value, '$.document_id') AS INTEGER) AS document_id,
                CAST(json_extract(value, '$.version_id') AS TEXT) AS version_id
            FROM json_each(?)
        )
        SELECT
            requested.ordinal,
            d.id AS document_id,
            v.id AS version_id,
            d.name,
            d.folder_id,
            f.root_key,
            d.archived_access,
            v.blob_id,
            v.mime_type
        FROM requested
        JOIN documents d ON d.id = requested.document_id
        JOIN document_versions v
          ON v.document_id = d.id AND v.id = requested.version_id
        JOIN folders f ON f.id = d.folder_id
        ORDER BY requested.ordinal
        ",
    )
    .bind(requested_json)
    .fetch_all(pool)
    .await?;
    if rows.len() != request.documents.len()
        || rows
            .iter()
            .enumerate()
            .any(|(index, row)| row.ordinal != i64::try_from(index).unwrap_or(-1))
    {
        return Err(PreviewError::DocumentNotFound);
    }
    let mut folder_ids = rows.iter().map(|row| row.folder_id).collect::<Vec<_>>();
    folder_ids.sort_unstable();
    folder_ids.dedup();
    let levels = folder_access_levels(pool, &folder_ids, user).await?;
    let archived_group_ids = if user.is_admin
        || !rows.iter().any(|row| row.root_key == ARCHIVE_ROOT_KEY)
        || user.groups.is_empty()
    {
        Vec::new()
    } else {
        let names = user
            .groups
            .iter()
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        sqlx::query_scalar::<_, String>(
            r"
            SELECT CAST(id AS TEXT)
            FROM vault_groups
            WHERE lower(trim(name)) IN (SELECT value FROM json_each(?))
            ",
        )
        .bind(serde_json::to_string(&names).map_err(|_| PreviewError::InvalidSource)?)
        .fetch_all(pool)
        .await?
    };
    let mut authorized = Vec::with_capacity(rows.len());
    for row in rows {
        let folder_level = levels.get(&row.folder_id).copied().unwrap_or(0);
        let level = if user.is_admin {
            3
        } else if row.root_key != ARCHIVE_ROOT_KEY || folder_level <= 0 {
            folder_level
        } else {
            let snapshot = parse_archived_access(row.archived_access.as_deref())
                .map_err(|_| PreviewError::DocumentNotFound)?;
            let source_level = archived_group_ids
                .iter()
                .filter_map(|group_id| snapshot.get(group_id).copied())
                .max()
                .unwrap_or(0);
            folder_level.min(source_level)
        };
        if level < 2 {
            return Err(if level > 0 {
                PreviewError::InsufficientDocumentAccess
            } else {
                PreviewError::DocumentNotFound
            });
        }
        authorized.push(AuthorizedPreviewSource {
            document_id: row.document_id,
            version_id: row.version_id,
            name: row.name,
            blob_id: row.blob_id,
            mime_type: row.mime_type,
        });
    }
    Ok(authorized)
}

pub async fn backfill_current_version_jobs(
    pool: &SqlitePool,
    limit: i64,
) -> Result<u64, PreviewError> {
    Ok(sqlx::query(
        r"
        INSERT OR IGNORE INTO preview_jobs (source_blob_id, recipe, status)
        SELECT DISTINCT v.blob_id, ?, 'queued'
        FROM documents d
        JOIN document_versions v ON v.document_id = d.id AND v.id = d.current_version_id
        ORDER BY v.blob_id
        LIMIT ?
        ",
    )
    .bind(PREVIEW_RECIPE)
    .bind(limit.clamp(1, 100_000))
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn recover_interrupted_jobs(pool: &SqlitePool) -> Result<u64, PreviewError> {
    Ok(sqlx::query(
        r"
        UPDATE preview_jobs
        SET status = 'queued',
            lease_token = NULL,
            lease_expires_at = NULL,
            next_attempt_at = NULL,
            updated_at = CURRENT_TIMESTAMP,
            last_error_code = 'worker_interrupted'
        WHERE status = 'running'
          AND (lease_expires_at IS NULL OR datetime(lease_expires_at) <= datetime('now'))
        ",
    )
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn rendition_for_source(
    pool: &SqlitePool,
    source_blob_id: i64,
    recipe: &str,
    variant: &str,
) -> Result<PreviewRenditionDownload, PreviewError> {
    if recipe != PREVIEW_RECIPE || !PREVIEW_VARIANTS.iter().any(|(name, _)| *name == variant) {
        return Err(PreviewError::RenditionNotFound);
    }
    let row = sqlx::query_as::<_, PreviewRenditionDownload>(
        r"
        SELECT
            b.hash_algo,
            b.hash,
            b.size_bytes,
            pr.mime_type,
            pr.width,
            pr.height,
            b.id AS blob_id
        FROM preview_jobs pj
        JOIN preview_renditions pr ON pr.preview_job_id = pj.id
        JOIN blobs b ON b.id = pr.blob_id
        WHERE pj.source_blob_id = ?
          AND pj.recipe = ?
          AND pj.status = 'ready'
          AND pr.variant = ?
        ",
    )
    .bind(source_blob_id)
    .bind(recipe)
    .bind(variant)
    .fetch_optional(pool)
    .await?
    .ok_or(PreviewError::RenditionNotFound)?;
    sqlx::query(
        r"
        UPDATE preview_jobs
        SET last_accessed_at = CURRENT_TIMESTAMP
        WHERE source_blob_id = ? AND recipe = ?
          AND (
              last_accessed_at IS NULL
              OR datetime(last_accessed_at) < datetime('now', ?)
          )
        ",
    )
    .bind(source_blob_id)
    .bind(recipe)
    .bind(PREVIEW_ACCESS_TOUCH_AGE)
    .execute(pool)
    .await?;
    Ok(row)
}

/// Invalidates every rendition backed by a missing/corrupt derived blob and
/// requeues the affected source jobs. The blob ID is returned so the caller can
/// run targeted crash-safe GC before waking workers; otherwise publication
/// could incorrectly reuse stale location metadata for the same digest.
pub async fn requeue_rendition_blob(
    pool: &SqlitePool,
    source_blob_id: i64,
    recipe: &str,
    variant: &str,
) -> Result<Option<i64>, PreviewError> {
    if !is_supported_preview_rendition(recipe, variant) {
        return Ok(None);
    }
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let rendition_blob_id = sqlx::query_scalar::<_, i64>(
        r"
        SELECT pr.blob_id
        FROM preview_jobs pj
        JOIN preview_renditions pr ON pr.preview_job_id = pj.id
        WHERE pj.source_blob_id = ?
          AND pj.recipe = ?
          AND pr.variant = ?
        ",
    )
    .bind(source_blob_id)
    .bind(recipe)
    .bind(variant)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(rendition_blob_id) = rendition_blob_id else {
        transaction.commit().await?;
        return Ok(None);
    };
    let affected_job_ids = sqlx::query_scalar::<_, i64>(
        "SELECT DISTINCT preview_job_id FROM preview_renditions WHERE blob_id = ?",
    )
    .bind(rendition_blob_id)
    .fetch_all(&mut *transaction)
    .await?;
    sqlx::query("DELETE FROM preview_renditions WHERE blob_id = ?")
        .bind(rendition_blob_id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        r"
        UPDATE preview_jobs
        SET status = 'queued',
            attempt_count = 0,
            lease_token = NULL,
            lease_expires_at = NULL,
            next_attempt_at = NULL,
            completed_at = NULL,
            updated_at = CURRENT_TIMESTAMP,
            last_error_code = 'rendition_unavailable',
            last_error_detail = NULL
        WHERE id IN (SELECT CAST(value AS INTEGER) FROM json_each(?))
        ",
    )
    .bind(serde_json::to_string(&affected_job_ids).map_err(|_| PreviewError::InvalidSource)?)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(Some(rendition_blob_id))
}

async fn delete_preview_jobs_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    job_ids: &[i64],
    released_blob_ids: &mut Vec<i64>,
) -> Result<(), PreviewError> {
    if job_ids.is_empty() {
        return Ok(());
    }
    let job_ids_json = serde_json::to_string(job_ids).map_err(|_| PreviewError::InvalidSource)?;
    released_blob_ids.extend(
        sqlx::query_scalar::<_, i64>(
            r"
            SELECT DISTINCT blob_id
            FROM preview_renditions
            WHERE preview_job_id IN (
                SELECT CAST(value AS INTEGER) FROM json_each(?)
            )
            ",
        )
        .bind(&job_ids_json)
        .fetch_all(&mut **transaction)
        .await?,
    );
    sqlx::query(
        "DELETE FROM preview_jobs WHERE id IN (SELECT CAST(value AS INTEGER) FROM json_each(?))",
    )
    .bind(job_ids_json)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

type PreviewPruneRenditionRow = (i64, i64, i64, i64, i64);

async fn quota_prune_candidates_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    limit: i64,
) -> Result<(Vec<i64>, Vec<PreviewPruneRenditionRow>), PreviewError> {
    let candidates = sqlx::query_scalar::<_, i64>(
        r"
        SELECT pj.id
        FROM preview_jobs pj
        WHERE pj.status != 'running'
          AND EXISTS (
              SELECT 1
              FROM preview_renditions pr
              WHERE pr.preview_job_id = pj.id
                AND NOT EXISTS (
                    SELECT 1 FROM document_versions v WHERE v.blob_id = pr.blob_id
                )
                AND NOT EXISTS (
                    SELECT 1 FROM export_artifacts a WHERE a.blob_id = pr.blob_id
                )
          )
        ORDER BY datetime(COALESCE(pj.last_accessed_at, pj.updated_at)), pj.id
        LIMIT ?
        ",
    )
    .bind(limit)
    .fetch_all(&mut **transaction)
    .await?;
    if candidates.is_empty() {
        return Ok((candidates, Vec::new()));
    }
    let candidate_json =
        serde_json::to_string(&candidates).map_err(|_| PreviewError::InvalidSource)?;
    let rendition_rows = sqlx::query_as::<_, PreviewPruneRenditionRow>(
        r"
        SELECT
            pr.preview_job_id,
            pr.blob_id,
            b.size_bytes,
            COUNT(*) AS job_reference_count,
            (
                SELECT COUNT(*)
                FROM preview_renditions all_pr
                WHERE all_pr.blob_id = pr.blob_id
            ) AS total_reference_count
        FROM preview_renditions pr
        JOIN blobs b ON b.id = pr.blob_id
        WHERE pr.preview_job_id IN (
                  SELECT CAST(value AS INTEGER) FROM json_each(?)
              )
          AND NOT EXISTS (
                  SELECT 1 FROM document_versions v WHERE v.blob_id = pr.blob_id
              )
          AND NOT EXISTS (
                  SELECT 1 FROM export_artifacts a WHERE a.blob_id = pr.blob_id
              )
        GROUP BY pr.preview_job_id, pr.blob_id, b.size_bytes
        ",
    )
    .bind(candidate_json)
    .fetch_all(&mut **transaction)
    .await?;
    Ok((candidates, rendition_rows))
}

fn select_quota_prune_jobs(
    candidates: Vec<i64>,
    rendition_rows: Vec<PreviewPruneRenditionRow>,
    mut total_bytes: i64,
    max_bytes: i64,
) -> Vec<i64> {
    let mut renditions_by_job: HashMap<i64, Vec<(i64, i64, i64, i64)>> = HashMap::new();
    for (preview_job_id, blob_id, size_bytes, job_reference_count, total_reference_count) in
        rendition_rows
    {
        renditions_by_job.entry(preview_job_id).or_default().push((
            blob_id,
            size_bytes,
            job_reference_count,
            total_reference_count,
        ));
    }
    let mut selected_job_ids = Vec::new();
    let mut selected_reference_counts = HashMap::<i64, i64>::new();
    let mut reclaimable_blob_ids = HashSet::new();
    for candidate in candidates {
        selected_job_ids.push(candidate);
        if let Some(renditions) = renditions_by_job.get(&candidate) {
            for &(blob_id, size_bytes, job_references, total_references) in renditions {
                let selected_references = selected_reference_counts.entry(blob_id).or_default();
                *selected_references += job_references;
                if *selected_references >= total_references && reclaimable_blob_ids.insert(blob_id)
                {
                    total_bytes = total_bytes.saturating_sub(size_bytes.max(0));
                }
            }
        }
        if total_bytes <= max_bytes {
            break;
        }
    }
    selected_job_ids.retain(|job_id| {
        renditions_by_job.get(job_id).is_some_and(|renditions| {
            renditions
                .iter()
                .any(|(blob_id, _, _, _)| reclaimable_blob_ids.contains(blob_id))
        })
    });
    selected_job_ids
}

/// Removes old historical preview sets and then trims the uniquely reclaimable
/// preview footprint to a byte budget. Blobs rooted by documents or exports do
/// not count as cache bytes, and a shared output is reclaimed only after every
/// preview job referencing it has been selected. Deleting metadata only
/// releases references; normal blob GC performs the crash-safe backend deletion.
pub async fn prune_preview_cache(
    pool: &SqlitePool,
    max_bytes: i64,
    historical_max_age: Duration,
    limit: i64,
) -> Result<PreviewPruneResult, PreviewError> {
    let max_age_seconds = i64::try_from(historical_max_age.as_secs()).unwrap_or(i64::MAX);
    let age_modifier = format!("-{max_age_seconds} seconds");
    let limit = limit.clamp(1, 1_000);
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut job_ids = sqlx::query_scalar::<_, i64>(
        r"
        SELECT pj.id
        FROM preview_jobs pj
        WHERE pj.status != 'running'
          AND (
              pj.recipe != ?
              OR NOT EXISTS (
                  SELECT 1
                  FROM documents d
                  JOIN document_versions v
                    ON v.document_id = d.id AND v.id = d.current_version_id
                  WHERE v.blob_id = pj.source_blob_id
              )
          )
          AND datetime(COALESCE(pj.last_accessed_at, pj.updated_at)) < datetime('now', ?)
        ORDER BY datetime(COALESCE(pj.last_accessed_at, pj.updated_at)), pj.id
        LIMIT ?
        ",
    )
    .bind(PREVIEW_RECIPE)
    .bind(age_modifier)
    .bind(limit)
    .fetch_all(&mut *transaction)
    .await?;
    let mut released_blob_ids = Vec::new();
    delete_preview_jobs_in_tx(&mut transaction, &job_ids, &mut released_blob_ids).await?;

    let total_bytes = sqlx::query_scalar::<_, i64>(
        r"
        SELECT COALESCE(SUM(b.size_bytes), 0)
        FROM blobs b
        WHERE EXISTS (
                  SELECT 1 FROM preview_renditions pr WHERE pr.blob_id = b.id
              )
          AND NOT EXISTS (
                  SELECT 1 FROM document_versions v WHERE v.blob_id = b.id
              )
          AND NOT EXISTS (
                  SELECT 1 FROM export_artifacts a WHERE a.blob_id = b.id
              )
        ",
    )
    .fetch_one(&mut *transaction)
    .await?;
    let max_bytes = max_bytes.max(0);
    let remaining_limit = limit - i64::try_from(job_ids.len()).unwrap_or(limit);
    if total_bytes > max_bytes && remaining_limit > 0 {
        let (candidates, rendition_rows) =
            quota_prune_candidates_in_tx(&mut transaction, remaining_limit).await?;
        let selected_job_ids =
            select_quota_prune_jobs(candidates, rendition_rows, total_bytes, max_bytes);
        delete_preview_jobs_in_tx(&mut transaction, &selected_job_ids, &mut released_blob_ids)
            .await?;
        job_ids.extend(selected_job_ids);
    }
    transaction.commit().await?;
    job_ids.sort_unstable();
    job_ids.dedup();
    released_blob_ids.sort_unstable();
    released_blob_ids.dedup();
    Ok(PreviewPruneResult {
        deleted_job_ids: job_ids,
        released_blob_ids,
    })
}

pub fn validate_resolve_request(request: &ResolvePreviewRequest) -> Result<(), PreviewError> {
    if request.documents.len() > PREVIEW_RESOLVE_MAX_DOCUMENTS {
        return Err(PreviewError::TooManyDocuments);
    }
    let mut document_ids = HashSet::with_capacity(request.documents.len());
    if request.documents.iter().any(|document| {
        document.document_id <= 0
            || document.version_id.trim().is_empty()
            || !document_ids.insert(document.document_id)
    }) {
        return Err(PreviewError::InvalidResolveRequest);
    }
    Ok(())
}

#[must_use]
pub fn semantic_icon_key(name: &str, mime_type: Option<&str>) -> String {
    let extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime_type = mime_type
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match extension.as_str() {
        "blend" => "app-blender",
        "fbx" | "obj" | "step" | "stp" => "cube",
        "plasticity" => "app-plasticity",
        "avif" | "bmp" | "exr" | "gif" | "hdr" | "heic" | "heif" | "jpeg" | "jpg" | "png"
        | "svg" | "tga" | "tif" | "tiff" | "webp" => "file-image",
        "pdf" => "file-pdf",
        "7z" | "bz2" | "gz" | "rar" | "tar" | "xz" | "zip" | "zst" => "file-zipper",
        "aac" | "flac" | "m4a" | "mp3" | "oga" | "ogg" | "wav" => "file-audio",
        "avi" | "m4v" | "mkv" | "mov" | "mp4" | "webm" => "file-video",
        "c" | "cc" | "cpp" | "cs" | "css" | "go" | "h" | "hpp" | "html" | "java" | "js"
        | "json" | "jsx" | "kt" | "lua" | "py" | "rb" | "rs" | "sh" | "sql" | "ts" | "tsx"
        | "xml" | "yaml" | "yml" => "file-code",
        "csv" => "file-csv",
        "log" | "md" | "rtf" | "txt" => "file-lines",
        _ if mime_type.starts_with("image/") => "file-image",
        _ if mime_type == "application/pdf" => "file-pdf",
        _ if mime_type.starts_with("audio/") => "file-audio",
        _ if mime_type.starts_with("video/") => "file-video",
        _ if mime_type.starts_with("text/") => "file-lines",
        _ => "file",
    }
    .to_string()
}

#[allow(clippy::too_many_lines)]
async fn claim_preview_job(pool: &SqlitePool) -> Result<Option<ClaimedPreviewJob>, PreviewError> {
    let lease_token = Uuid::new_v4().simple().to_string();
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let job_id = sqlx::query_scalar::<_, i64>(
        r"
        SELECT id
        FROM preview_jobs
        WHERE recipe = ?
          AND (
              status = 'queued'
              OR (
                  status = 'failed'
                  AND next_attempt_at IS NOT NULL
                  AND datetime(next_attempt_at) <= datetime('now')
              )
              OR (
                  status = 'running'
                  AND (lease_expires_at IS NULL OR datetime(lease_expires_at) <= datetime('now'))
              )
          )
        ORDER BY
            CASE WHEN status = 'running' THEN 0 ELSE 1 END,
            datetime(COALESCE(next_attempt_at, created_at)),
            id
        LIMIT 1
        ",
    )
    .bind(PREVIEW_RECIPE)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(job_id) = job_id else {
        transaction.commit().await?;
        return Ok(None);
    };
    let claimed = sqlx::query(
        r"
        UPDATE preview_jobs
        SET status = 'running',
            attempt_count = attempt_count + 1,
            lease_token = ?,
            lease_expires_at = datetime('now', ?),
            next_attempt_at = NULL,
            updated_at = CURRENT_TIMESTAMP,
            last_error_code = NULL,
            last_error_detail = NULL
        WHERE id = ?
        ",
    )
    .bind(&lease_token)
    .bind(format!("+{PREVIEW_LEASE_SECONDS} seconds"))
    .bind(job_id)
    .execute(&mut *transaction)
    .await?;
    if claimed.rows_affected() != 1 {
        transaction.rollback().await?;
        return Ok(None);
    }
    let row = sqlx::query_as::<_, ClaimedPreviewJobRow>(
        r"
        SELECT
            pj.id,
            pj.source_blob_id,
            pj.attempt_count,
            pj.lease_token,
            b.hash_algo,
            b.hash,
            b.size_bytes,
            (
                SELECT v.mime_type
                FROM document_versions v
                WHERE v.blob_id = pj.source_blob_id
                ORDER BY v.committed_at DESC, v.id DESC
                LIMIT 1
            ) AS mime_type,
            (
                SELECT COALESCE(v.original_filename, d.name)
                FROM document_versions v
                JOIN documents d ON d.id = v.document_id
                WHERE v.blob_id = pj.source_blob_id
                ORDER BY v.committed_at DESC, v.id DESC
                LIMIT 1
            ) AS filename
        FROM preview_jobs pj
        JOIN blobs b ON b.id = pj.source_blob_id
        WHERE pj.id = ? AND pj.lease_token = ?
        ",
    )
    .bind(job_id)
    .bind(&lease_token)
    .fetch_one(&mut *transaction)
    .await?;
    transaction.commit().await?;
    let locations = blob_locations(pool, row.source_blob_id).await?;
    Ok(Some(ClaimedPreviewJob {
        id: row.id,
        attempt_count: row.attempt_count,
        lease_token: row.lease_token,
        hash_algo: row.hash_algo,
        hash: row.hash,
        size_bytes: row.size_bytes,
        mime_type: row.mime_type,
        filename: row.filename,
        locations,
    }))
}

async fn blob_locations(
    pool: &SqlitePool,
    blob_id: i64,
) -> Result<Vec<BlobLocation>, PreviewError> {
    Ok(sqlx::query_as::<_, (String, String, String)>(
        r"
        SELECT backend, bucket, object_key
        FROM blob_locations
        WHERE blob_id = ?
          AND backend NOT GLOB '_vault_pending:*'
          AND backend NOT GLOB '_vault_deleting:*'
          AND TRIM(backend) != ''
          AND TRIM(object_key) != ''
        ORDER BY id
        ",
    )
    .bind(blob_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(backend, bucket, object_key)| BlobLocation {
        backend,
        bucket,
        object_key,
    })
    .collect())
}

async fn read_preview_source(
    storage: &dyn crate::storage::BlobStorageBackend,
    job: &ClaimedPreviewJob,
) -> Result<Vec<u8>, PreviewError> {
    let size = u64::try_from(job.size_bytes).map_err(|_| PreviewError::InvalidSource)?;
    if size == 0 {
        return Err(PreviewError::InvalidSource);
    }
    if size > PREVIEW_SOURCE_MAX_BYTES {
        return Err(PreviewError::SourceTooLarge);
    }
    if !job.hash_algo.eq_ignore_ascii_case("sha256") || job.locations.is_empty() {
        return Err(PreviewError::InvalidSource);
    }
    let mut stream = open_ranked_location_stream(
        storage,
        &job.locations,
        BlobReadRange {
            expected_size: size,
            offset: 0,
            length: size,
        },
    )
    .await?;
    let capacity = usize::try_from(size).map_err(|_| PreviewError::SourceTooLarge)?;
    let mut source_bytes = Vec::with_capacity(capacity);
    let mut hasher = Sha256::new();
    while let Some(frame) = stream.next().await {
        let bytes = frame?;
        let next_size = source_bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(PreviewError::InvalidSource)?;
        if next_size > capacity || next_size as u64 > PREVIEW_SOURCE_MAX_BYTES {
            return Err(PreviewError::InvalidSource);
        }
        hasher.update(&bytes);
        source_bytes.extend_from_slice(&bytes);
    }
    if source_bytes.len() != capacity || format!("{:x}", hasher.finalize()) != job.hash {
        return Err(PreviewError::InvalidSource);
    }
    Ok(source_bytes)
}

async fn read_preview_prefix(
    storage: &dyn crate::storage::BlobStorageBackend,
    job: &ClaimedPreviewJob,
) -> Result<Vec<u8>, PreviewError> {
    let size = u64::try_from(job.size_bytes).map_err(|_| PreviewError::InvalidSource)?;
    if job.locations.is_empty() || size == 0 {
        return Ok(Vec::new());
    }
    let length = size.min(PREVIEW_SNIFF_BYTES);
    let mut stream = open_ranked_location_stream(
        storage,
        &job.locations,
        BlobReadRange {
            expected_size: size,
            offset: 0,
            length,
        },
    )
    .await?;
    let capacity = usize::try_from(length).map_err(|_| PreviewError::InvalidSource)?;
    let mut prefix = Vec::with_capacity(capacity);
    while let Some(frame) = stream.next().await {
        let bytes = frame?;
        if prefix.len().saturating_add(bytes.len()) > capacity {
            return Err(PreviewError::InvalidSource);
        }
        prefix.extend_from_slice(&bytes);
    }
    if prefix.len() != capacity {
        return Err(PreviewError::InvalidSource);
    }
    Ok(prefix)
}

fn validate_rendered_previews(outputs: &[RenderedPreview]) -> Result<(), PreviewError> {
    if outputs.len() != PREVIEW_VARIANTS.len() {
        return Err(PreviewError::InvalidProviderOutput);
    }
    let mut seen = std::collections::HashSet::new();
    for output in outputs {
        let expected_dimension = PREVIEW_VARIANTS
            .iter()
            .find_map(|(name, dimension)| (*name == output.variant).then_some(*dimension))
            .ok_or(PreviewError::InvalidProviderOutput)?;
        if !seen.insert(output.variant.as_str())
            || output.bytes.is_empty()
            || output.bytes.len() > PREVIEW_OUTPUT_MAX_BYTES
            || output.width <= 0
            || output.height <= 0
            || output.width > PREVIEW_OUTPUT_MAX_DIMENSION
            || output.height > PREVIEW_OUTPUT_MAX_DIMENSION
            || output.width > expected_dimension
            || output.height > expected_dimension
            || !matches!(output.mime_type.as_str(), "image/webp")
        {
            return Err(PreviewError::InvalidProviderOutput);
        }
    }
    Ok(())
}

struct StagedRendition<'a> {
    publication: PendingBlobPublication,
    stored: StoredBlob,
    output: &'a RenderedPreview,
}

async fn abandon_staged_renditions(staged: Vec<StagedRendition<'_>>) {
    for rendition in staged {
        if let Err(error) = rendition.publication.abandon(Some(&rendition.stored)).await {
            tracing::warn!(?error, "failed to abandon staged preview rendition");
        }
    }
}

// Staging, the single metadata commit, and abandonment intentionally stay in
// one audit surface because splitting their ordering can expose partial sets.
#[allow(clippy::too_many_lines)]
async fn publish_renditions(
    pool: &SqlitePool,
    storage: &dyn crate::storage::BlobStorageBackend,
    job: &ClaimedPreviewJob,
    outputs: &[RenderedPreview],
) -> Result<(), PreviewError> {
    let mut staged = Vec::with_capacity(outputs.len());
    for output in outputs {
        if let Err(error) = renew_job_lease(pool, job).await {
            abandon_staged_renditions(staged).await;
            return Err(error);
        }
        let digest = sha256_hex(&output.bytes);
        let publication = match begin_blob_publication(
            pool,
            storage,
            "sha256",
            &digest,
            output.bytes.len() as u64,
            BlobWriteKind::Bytes,
        )
        .await
        {
            Ok(publication) => publication,
            Err(error) => {
                abandon_staged_renditions(staged).await;
                return Err(error.into());
            }
        };
        let stored = match publication
            .run_storage(storage.put_bytes(&output.bytes))
            .await
        {
            Ok(stored) => stored,
            Err(error) => {
                if let Err(cleanup_error) = publication.abandon(None).await {
                    tracing::warn!(
                        ?cleanup_error,
                        "failed to abandon interrupted preview rendition"
                    );
                }
                abandon_staged_renditions(staged).await;
                return Err(error.into());
            }
        };
        staged.push(StagedRendition {
            publication,
            stored,
            output,
        });
    }
    let commit_result = async {
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let lease_exists = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM preview_jobs WHERE id = ? AND status = 'running' AND lease_token = ?",
        )
        .bind(job.id)
        .bind(&job.lease_token)
        .fetch_optional(&mut *transaction)
        .await?
        .is_some();
        if !lease_exists {
            transaction.rollback().await?;
            return Err(PreviewError::LeaseLost);
        }
        sqlx::query("DELETE FROM preview_renditions WHERE preview_job_id = ?")
            .bind(job.id)
            .execute(&mut *transaction)
            .await?;
        for rendition in &staged {
            let blob_id = rendition
                .publication
                .prepare_metadata_in_tx(&mut transaction, &rendition.stored)
                .await?;
            sqlx::query(
                r"
                INSERT INTO preview_renditions
                    (preview_job_id, variant, blob_id, mime_type, width, height)
                VALUES (?, ?, ?, ?, ?, ?)
                ",
            )
            .bind(job.id)
            .bind(&rendition.output.variant)
            .bind(blob_id)
            .bind(&rendition.output.mime_type)
            .bind(rendition.output.width)
            .bind(rendition.output.height)
            .execute(&mut *transaction)
            .await?;
        }
        for rendition in &staged {
            rendition
                .publication
                .finish_metadata_in_tx(&mut transaction)
                .await?;
        }
        let updated = sqlx::query(
            r"
            UPDATE preview_jobs
            SET status = 'ready',
                lease_token = NULL,
                lease_expires_at = NULL,
                next_attempt_at = NULL,
                completed_at = CURRENT_TIMESTAMP,
                updated_at = CURRENT_TIMESTAMP,
                last_error_code = NULL,
                last_error_detail = NULL
            WHERE id = ? AND status = 'running' AND lease_token = ?
            ",
        )
        .bind(job.id)
        .bind(&job.lease_token)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(PreviewError::LeaseLost);
        }
        transaction.commit().await?;
        Ok::<(), PreviewError>(())
    }
    .await;
    if commit_result.is_err() {
        abandon_staged_renditions(staged).await;
    }
    commit_result
}

async fn renew_job_lease(pool: &SqlitePool, job: &ClaimedPreviewJob) -> Result<(), PreviewError> {
    let updated = sqlx::query(
        r"
        UPDATE preview_jobs
        SET lease_expires_at = datetime('now', ?), updated_at = CURRENT_TIMESTAMP
        WHERE id = ? AND status = 'running' AND lease_token = ?
        ",
    )
    .bind(format!("+{PREVIEW_LEASE_SECONDS} seconds"))
    .bind(job.id)
    .bind(&job.lease_token)
    .execute(pool)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(PreviewError::LeaseLost);
    }
    Ok(())
}

async fn mark_job_unsupported(
    pool: &SqlitePool,
    job: &ClaimedPreviewJob,
    code: &str,
) -> Result<(), PreviewError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM preview_renditions WHERE preview_job_id = ?")
        .bind(job.id)
        .execute(&mut *transaction)
        .await?;
    let updated = sqlx::query(
        r"
        UPDATE preview_jobs
        SET status = 'unsupported',
            lease_token = NULL,
            lease_expires_at = NULL,
            next_attempt_at = NULL,
            completed_at = CURRENT_TIMESTAMP,
            updated_at = CURRENT_TIMESTAMP,
            last_error_code = ?,
            last_error_detail = NULL
        WHERE id = ? AND status = 'running' AND lease_token = ?
        ",
    )
    .bind(code)
    .bind(job.id)
    .bind(&job.lease_token)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() != 1 {
        transaction.rollback().await?;
        return Err(PreviewError::LeaseLost);
    }
    transaction.commit().await?;
    Ok(())
}

async fn finish_job_after_error(
    pool: &SqlitePool,
    job: &ClaimedPreviewJob,
    result: &Result<(), PreviewError>,
) -> Result<(), PreviewError> {
    let Err(error) = result else {
        return Ok(());
    };
    let retryable = !matches!(
        error,
        PreviewError::Unsupported
            | PreviewError::InvalidProviderOutput
            | PreviewError::SourceTooLarge
            | PreviewError::InvalidSource
    );
    let retry = retryable && job.attempt_count < PREVIEW_RETRY_LIMIT;
    let retry_seconds = 5_i64.saturating_mul(1_i64 << job.attempt_count.clamp(0, 8));
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(
        r"
        DELETE FROM preview_renditions
        WHERE preview_job_id IN (
            SELECT id
            FROM preview_jobs
            WHERE id = ? AND status = 'running' AND lease_token = ?
        )
        ",
    )
    .bind(job.id)
    .bind(&job.lease_token)
    .execute(&mut *transaction)
    .await?;
    let updated = sqlx::query(
        r"
        UPDATE preview_jobs
        SET status = 'failed',
            lease_token = NULL,
            lease_expires_at = NULL,
            next_attempt_at = CASE WHEN ? THEN datetime('now', ?) ELSE NULL END,
            updated_at = CURRENT_TIMESTAMP,
            completed_at = CASE WHEN ? THEN NULL ELSE CURRENT_TIMESTAMP END,
            last_error_code = ?,
            last_error_detail = ?
        WHERE id = ? AND status = 'running' AND lease_token = ?
        ",
    )
    .bind(retry)
    .bind(format!("+{retry_seconds} seconds"))
    .bind(retry)
    .bind(preview_error_code(error))
    .bind(error.to_string())
    .bind(job.id)
    .bind(&job.lease_token)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() == 0 && !matches!(error, PreviewError::LeaseLost) {
        transaction.rollback().await?;
        return Err(PreviewError::LeaseLost);
    }
    transaction.commit().await?;
    Ok(())
}

fn preview_error_code(error: &PreviewError) -> &'static str {
    match error {
        PreviewError::TooManyDocuments => "too_many_documents",
        PreviewError::InvalidResolveRequest => "invalid_resolve_request",
        PreviewError::DocumentNotFound => "document_not_found",
        PreviewError::InsufficientDocumentAccess => "insufficient_document_access",
        PreviewError::RenditionNotFound => "rendition_not_found",
        PreviewError::Unsupported => "unsupported",
        PreviewError::InvalidProviderOutput => "invalid_provider_output",
        PreviewError::LeaseLost => "lease_lost",
        PreviewError::SourceTooLarge => "source_too_large",
        PreviewError::InvalidSource => "invalid_source",
        PreviewError::SourceReadTimeout => "source_read_timeout",
        PreviewError::RenderTimeout => "render_timeout",
        PreviewError::Database(_) => "database",
        PreviewError::Storage(_) => "storage",
        PreviewError::BlobLifecycle(_) => "blob_lifecycle",
        PreviewError::Folder(_) => "folder",
        PreviewError::Io(_) => "io",
    }
}

fn normalized_preview_status(status: &str) -> String {
    match status {
        "queued" | "running" => "pending",
        "ready" => "ready",
        "unsupported" => "unsupported",
        _ => "failed",
    }
    .to_string()
}

fn percent_encode_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}
