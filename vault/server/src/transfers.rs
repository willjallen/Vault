use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::fs;
use tokio::sync::Mutex;

use crate::blob_lifecycle::collect_unreferenced_blobs_with_limit;
use crate::exports::{ExportError, ExportExecutionContext};
use crate::storage::{
    S3_UPLOAD_STAGE_FILENAME, SharedBlobStorage, StorageError, remove_s3_upload_stage_file,
};
use crate::uploads::{UploadHashCoordinator, clear_upload_session_files};

const DEFAULT_SWEEP_LIMIT: i64 = 250;
const DEFAULT_RECOVERY_BLOB_GC_LIMIT: i64 = 1000;
const MAX_UPLOAD_PART_METADATA_BYTES: u64 = 4096;
const ORPHAN_UPLOAD_MINIMUM_AGE: Duration = Duration::from_mins(5);

#[derive(Clone, Default)]
pub struct TransferMaintenanceCoordinator {
    orphan_upload_scan: Arc<Mutex<Option<OrphanUploadDirectoryScan>>>,
}

struct OrphanUploadDirectoryScan {
    root: PathBuf,
    entries: fs::ReadDir,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TransferSweepResult {
    pub expired_uploads: Vec<String>,
    pub deleted_uploads: Vec<String>,
    pub deleted_orphan_uploads: Vec<String>,
    pub cancelled_exports: Vec<String>,
    pub deleted_exports: Vec<String>,
    pub deleted_export_objects: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TransferRecoveryResult {
    pub resumed_uploads: Vec<String>,
    pub failed_uploads: Vec<String>,
    pub deleted_upload_temps: Vec<String>,
    pub requeued_exports: Vec<String>,
    pub deleted_export_temps: Vec<String>,
    pub deleted_export_objects: Vec<String>,
}

#[derive(Debug, Error)]
pub enum TransferMaintenanceError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Export(#[from] ExportError),
    #[error(transparent)]
    TimeFormat(#[from] time::error::Format),
    #[error("export startup requires a persistent dispatcher runtime")]
    ExportDispatcherRequired,
}

#[derive(Debug, FromRow)]
struct ExpiredUploadRow {
    id: String,
    status: String,
}

#[derive(Debug, FromRow)]
struct ExpiredExportRow {
    id: String,
    status: String,
}

#[derive(Debug, FromRow)]
struct InterruptedUploadRow {
    id: String,
    total_size: i64,
    chunk_size: i64,
    part_count: i64,
}

#[derive(Debug, FromRow)]
struct InterruptedExportRow {
    id: String,
}

#[derive(Debug, Deserialize)]
struct UploadPartMetadata {
    part_number: i64,
    offset_bytes: i64,
    size_bytes: i64,
}

pub async fn sweep_expired_transfers(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    hash_coordinator: &UploadHashCoordinator,
    maintenance: &TransferMaintenanceCoordinator,
) -> Result<TransferSweepResult, TransferMaintenanceError> {
    sweep_expired_transfers_with_limit(
        pool,
        storage,
        transfers_path,
        hash_coordinator,
        maintenance,
        DEFAULT_SWEEP_LIMIT,
    )
    .await
}

pub async fn sweep_expired_transfers_with_limit(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    hash_coordinator: &UploadHashCoordinator,
    maintenance: &TransferMaintenanceCoordinator,
    limit: i64,
) -> Result<TransferSweepResult, TransferMaintenanceError> {
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let limit = limit.max(1);
    let uploads = expired_uploads(pool, &now, limit).await?;
    let exports = expired_exports(pool, &now, limit).await?;
    let mut result = TransferSweepResult::default();

    let mut transaction = pool.begin().await?;
    sweep_upload_rows(&mut transaction, &uploads, &now, &mut result).await?;
    sweep_export_rows(&mut transaction, &exports, &now, &mut result).await?;
    transaction.commit().await?;

    let terminal_uploads = result
        .expired_uploads
        .iter()
        .chain(result.deleted_uploads.iter())
        .cloned()
        .collect::<Vec<_>>();
    cleanup_upload_session_resources(hash_coordinator, transfers_path, &terminal_uploads).await;
    match sweep_orphaned_upload_directories(
        pool,
        hash_coordinator,
        maintenance,
        transfers_path,
        ORPHAN_UPLOAD_MINIMUM_AGE,
        limit,
    )
    .await
    {
        Ok(removed) => result.deleted_orphan_uploads = removed,
        Err(error) => tracing::warn!(?error, "orphaned upload directory sweep failed"),
    }
    for job_id in result
        .cancelled_exports
        .iter()
        .chain(result.deleted_exports.iter())
    {
        clear_export_temp_file(transfers_path, job_id).await?;
    }
    match collect_unreferenced_blobs_with_limit(pool, storage.as_ref(), limit).await {
        Ok(garbage_collection) => result
            .deleted_export_objects
            .extend(garbage_collection.deleted_objects),
        Err(error) => tracing::warn!(?error, "transfer object garbage collection failed"),
    }
    Ok(result)
}

pub async fn cleanup_upload_session_resources(
    hash_coordinator: &UploadHashCoordinator,
    transfers_path: &Path,
    session_ids: &[String],
) {
    hash_coordinator.forget_many(session_ids).await;
    for session_id in session_ids {
        clear_upload_session_files(transfers_path, session_id).await;
    }
}

pub async fn sweep_orphaned_upload_directories(
    pool: &SqlitePool,
    hash_coordinator: &UploadHashCoordinator,
    maintenance: &TransferMaintenanceCoordinator,
    transfers_path: &Path,
    minimum_age: Duration,
    limit: i64,
) -> Result<Vec<String>, TransferMaintenanceError> {
    let Some(upload_root) = validated_upload_root(transfers_path).await? else {
        *maintenance.orphan_upload_scan.lock().await = None;
        return Ok(Vec::new());
    };
    let now = SystemTime::now();
    let entry_limit = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
    let entries = next_orphan_scan_entries(maintenance, &upload_root, entry_limit).await?;
    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(session_id) = entry.file_name().into_string() else {
            continue;
        };
        if !is_safe_transfer_id(&session_id) {
            continue;
        }
        let expected_path = upload_root.join(&session_id);
        if entry.path() != expected_path
            || !old_enough_directory(&expected_path, now, minimum_age).await
        {
            continue;
        }
        candidates.push((session_id, expected_path));
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let live_session_ids =
        live_upload_session_ids(pool, candidates.iter().map(|(session_id, _)| session_id)).await?;
    let mut removed = Vec::new();
    for (session_id, expected_path) in candidates {
        if live_session_ids.contains(&session_id)
            || validated_upload_root(transfers_path).await?.as_ref() != Some(&upload_root)
            || !old_enough_directory(&expected_path, now, minimum_age).await
        {
            continue;
        }
        match fs::remove_dir_all(&expected_path).await {
            Ok(()) => removed.push(session_id),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                ?error,
                path = %expected_path.display(),
                "could not remove orphaned upload directory"
            ),
        }
    }
    hash_coordinator.forget_many(&removed).await;
    Ok(removed)
}

async fn next_orphan_scan_entries(
    maintenance: &TransferMaintenanceCoordinator,
    upload_root: &Path,
    limit: usize,
) -> Result<Vec<fs::DirEntry>, std::io::Error> {
    let mut cursor = maintenance.orphan_upload_scan.lock().await;
    if cursor.as_ref().is_none_or(|scan| scan.root != upload_root) {
        *cursor = Some(OrphanUploadDirectoryScan {
            root: upload_root.to_path_buf(),
            entries: fs::read_dir(upload_root).await?,
        });
    }
    let mut entries = Vec::with_capacity(limit);
    for _ in 0..limit {
        let next = cursor
            .as_mut()
            .expect("orphan scan cursor initialized")
            .entries
            .next_entry()
            .await;
        match next {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {
                *cursor = None;
                break;
            }
            Err(error) => {
                *cursor = None;
                return Err(error);
            }
        }
    }
    Ok(entries)
}

async fn live_upload_session_ids<'a>(
    pool: &SqlitePool,
    session_ids: impl Iterator<Item = &'a String>,
) -> Result<HashSet<String>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new("SELECT id FROM upload_sessions WHERE id IN (");
    let mut separated = query.separated(", ");
    for session_id in session_ids {
        separated.push_bind(session_id);
    }
    separated.push_unseparated(")");
    Ok(query
        .build_query_scalar::<String>()
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect())
}

async fn validated_upload_root(transfers_path: &Path) -> Result<Option<PathBuf>, std::io::Error> {
    let upload_root = transfers_path.join("uploads");
    match fs::symlink_metadata(&upload_root).await {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(upload_root))
        }
        Ok(_) => {
            tracing::warn!(
                path = %upload_root.display(),
                "refusing upload cleanup because the upload root is not a real directory"
            );
            Ok(None)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub async fn recover_interrupted_transfers(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    enqueue_exports: bool,
) -> Result<TransferRecoveryResult, TransferMaintenanceError> {
    recover_interrupted_transfers_inner(pool, storage, transfers_path, enqueue_exports, None).await
}

pub async fn recover_interrupted_transfers_with_export_runtime(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    enqueue_exports: bool,
    export_execution: &ExportExecutionContext,
) -> Result<TransferRecoveryResult, TransferMaintenanceError> {
    recover_interrupted_transfers_inner(
        pool,
        storage,
        transfers_path,
        enqueue_exports,
        Some(export_execution),
    )
    .await
}

async fn recover_interrupted_transfers_inner(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    enqueue_exports: bool,
    export_execution: Option<&ExportExecutionContext>,
) -> Result<TransferRecoveryResult, TransferMaintenanceError> {
    if enqueue_exports && export_execution.is_none() {
        return Err(TransferMaintenanceError::ExportDispatcherRequired);
    }
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let uploads = interrupted_uploads(pool, &now).await?;
    let exports = interrupted_exports(pool, &now).await?;
    let mut result = TransferRecoveryResult::default();
    result.deleted_upload_temps = clear_interrupted_upload_stages(&uploads, transfers_path).await;

    let mut transaction = pool.begin().await?;
    for upload in &uploads {
        if upload_has_recoverable_parts(transfers_path, upload).await? {
            sqlx::query(
                r"
                UPDATE upload_sessions
                SET status = 'active',
                    verification_total_bytes = 0,
                    verification_processed_bytes = 0,
                    error = NULL,
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
                ",
            )
            .bind(&upload.id)
            .execute(&mut *transaction)
            .await?;
            result.resumed_uploads.push(upload.id.clone());
        } else {
            sqlx::query(
                r"
                UPDATE upload_sessions
                SET status = 'failed',
                    verification_total_bytes = 0,
                    verification_processed_bytes = 0,
                    error = 'Upload completion interrupted and staged parts are missing or invalid',
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
                ",
            )
            .bind(&upload.id)
            .execute(&mut *transaction)
            .await?;
            result.failed_uploads.push(upload.id.clone());
        }
    }

    for export in &exports {
        sqlx::query("DELETE FROM export_artifacts WHERE job_id = ?")
            .bind(&export.id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            r"
            UPDATE export_jobs
            SET status = 'queued',
                processed_items = 0,
                processed_bytes = 0,
                error = NULL,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            ",
        )
        .bind(&export.id)
        .execute(&mut *transaction)
        .await?;
        result.requeued_exports.push(export.id.clone());
    }
    transaction.commit().await?;

    for job_id in &result.requeued_exports {
        if clear_export_temp_file(transfers_path, job_id).await? {
            result
                .deleted_export_temps
                .push(format!("{job_id}.zip.tmp"));
        }
    }
    match collect_unreferenced_blobs_with_limit(
        pool,
        storage.as_ref(),
        DEFAULT_RECOVERY_BLOB_GC_LIMIT,
    )
    .await
    {
        Ok(garbage_collection) => result
            .deleted_export_objects
            .extend(garbage_collection.deleted_objects),
        Err(error) => tracing::warn!(?error, "recovery object garbage collection failed"),
    }
    if enqueue_exports {
        start_export_dispatcher(pool, storage, transfers_path, export_execution)?;
    }
    Ok(result)
}

async fn clear_interrupted_upload_stages(
    uploads: &[InterruptedUploadRow],
    transfers_path: &Path,
) -> Vec<String> {
    let mut deleted = Vec::new();
    let upload_root = match validated_upload_root(transfers_path).await {
        Ok(Some(upload_root)) => upload_root,
        Ok(None) => return deleted,
        Err(error) => {
            tracing::warn!(?error, "could not validate interrupted upload staging root");
            return deleted;
        }
    };
    for upload in uploads {
        if !is_safe_transfer_id(&upload.id) {
            continue;
        }
        if validated_upload_root(transfers_path)
            .await
            .ok()
            .flatten()
            .as_ref()
            != Some(&upload_root)
        {
            break;
        }
        let session_dir = upload_root.join(&upload.id);
        match remove_s3_upload_stage_file(&session_dir).await {
            Ok(true) => deleted.push(format!("{}/{}", upload.id, S3_UPLOAD_STAGE_FILENAME)),
            Ok(false) => {}
            Err(StorageError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                ?error,
                session_id = %upload.id,
                "could not remove interrupted S3 upload stage file"
            ),
        }
    }
    deleted
}

fn start_export_dispatcher(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    export_execution: Option<&ExportExecutionContext>,
) -> Result<(), TransferMaintenanceError> {
    let execution = export_execution.ok_or(TransferMaintenanceError::ExportDispatcherRequired)?;
    execution.start_dispatcher(pool, storage, transfers_path);
    execution.notify_dispatcher();
    Ok(())
}

async fn expired_uploads(
    pool: &SqlitePool,
    now: &str,
    limit: i64,
) -> Result<Vec<ExpiredUploadRow>, sqlx::Error> {
    sqlx::query_as::<_, ExpiredUploadRow>(
        r"
        SELECT id, status
        FROM upload_sessions
        WHERE datetime(expires_at) <= datetime(?)
        ORDER BY expires_at
        LIMIT ?
        ",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await
}

async fn expired_exports(
    pool: &SqlitePool,
    now: &str,
    limit: i64,
) -> Result<Vec<ExpiredExportRow>, sqlx::Error> {
    sqlx::query_as::<_, ExpiredExportRow>(
        r"
        SELECT id, status
        FROM export_jobs
        WHERE datetime(expires_at) <= datetime(?)
        ORDER BY expires_at
        LIMIT ?
        ",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await
}

async fn interrupted_uploads(
    pool: &SqlitePool,
    now: &str,
) -> Result<Vec<InterruptedUploadRow>, sqlx::Error> {
    sqlx::query_as::<_, InterruptedUploadRow>(
        r"
        SELECT id, total_size, chunk_size, part_count
        FROM upload_sessions
        WHERE status = 'completing'
          AND datetime(expires_at) > datetime(?)
        ORDER BY updated_at
        ",
    )
    .bind(now)
    .fetch_all(pool)
    .await
}

async fn interrupted_exports(
    pool: &SqlitePool,
    now: &str,
) -> Result<Vec<InterruptedExportRow>, sqlx::Error> {
    sqlx::query_as::<_, InterruptedExportRow>(
        r"
        SELECT id
        FROM export_jobs
        WHERE status IN ('running', 'finalizing')
          AND datetime(expires_at) > datetime(?)
        ORDER BY updated_at
        ",
    )
    .bind(now)
    .fetch_all(pool)
    .await
}

async fn upload_has_recoverable_parts(
    transfers_path: &Path,
    upload: &InterruptedUploadRow,
) -> Result<bool, TransferMaintenanceError> {
    if !is_safe_transfer_id(&upload.id)
        || canonical_part_count(upload.total_size, upload.chunk_size) != Some(upload.part_count)
    {
        return Ok(false);
    }
    let session_dir = transfers_path.join("uploads").join(&upload.id);
    if upload.part_count > 0 {
        let session_metadata = match fs::symlink_metadata(&session_dir).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !session_metadata.file_type().is_dir() {
            return Ok(false);
        }
    }
    let mut recovered_size = 0_i64;
    for part_number in 1..=upload.part_count {
        let metadata_path = session_dir.join(format!("{part_number:08}.json"));
        let part_path = session_dir.join(format!("{part_number:08}.part"));
        let Some(expected_offset) = part_number
            .checked_sub(1)
            .and_then(|index| index.checked_mul(upload.chunk_size))
        else {
            return Ok(false);
        };
        let Some(expected_size) = upload
            .total_size
            .checked_sub(expected_offset)
            .map(|remaining| remaining.min(upload.chunk_size))
            .filter(|size| *size > 0)
        else {
            return Ok(false);
        };
        let part_metadata = match fs::symlink_metadata(&part_path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !part_metadata.file_type().is_file()
            || i64::try_from(part_metadata.len()).ok() != Some(expected_size)
        {
            return Ok(false);
        }
        match fs::symlink_metadata(&metadata_path).await {
            Ok(metadata)
                if !metadata.file_type().is_file()
                    || metadata.len() > MAX_UPLOAD_PART_METADATA_BYTES =>
            {
                return Ok(false);
            }
            Ok(_) => {
                let metadata_bytes = fs::read(&metadata_path).await?;
                let Ok(metadata) = serde_json::from_slice::<UploadPartMetadata>(&metadata_bytes)
                else {
                    return Ok(false);
                };
                if metadata.part_number != part_number
                    || metadata.offset_bytes != expected_offset
                    || metadata.size_bytes != expected_size
                {
                    return Ok(false);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let Some(next_recovered_size) = recovered_size.checked_add(expected_size) else {
            return Ok(false);
        };
        recovered_size = next_recovered_size;
    }
    Ok(recovered_size == upload.total_size)
}

fn canonical_part_count(total_size: i64, chunk_size: i64) -> Option<i64> {
    if total_size < 0 || chunk_size <= 0 {
        return None;
    }
    if total_size == 0 {
        return Some(0);
    }
    total_size
        .checked_sub(1)?
        .checked_div(chunk_size)?
        .checked_add(1)
}

async fn sweep_upload_rows(
    transaction: &mut Transaction<'_, Sqlite>,
    uploads: &[ExpiredUploadRow],
    now: &str,
    result: &mut TransferSweepResult,
) -> Result<(), sqlx::Error> {
    for upload in uploads {
        if matches!(upload.status.as_str(), "active" | "completing") {
            let expired = sqlx::query(
                r"
                UPDATE upload_sessions
                SET status = 'expired',
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
                  AND status = ?
                  AND datetime(expires_at) <= datetime(?)
                ",
            )
            .bind(&upload.id)
            .bind(&upload.status)
            .bind(now)
            .execute(&mut **transaction)
            .await?;
            if expired.rows_affected() > 0 {
                result.expired_uploads.push(upload.id.clone());
            }
        } else {
            let deleted = sqlx::query(
                r"
                DELETE FROM upload_sessions
                WHERE id = ?
                  AND status = ?
                  AND datetime(expires_at) <= datetime(?)
                ",
            )
            .bind(&upload.id)
            .bind(&upload.status)
            .bind(now)
            .execute(&mut **transaction)
            .await?;
            if deleted.rows_affected() > 0 {
                result.deleted_uploads.push(upload.id.clone());
            }
        }
    }
    Ok(())
}

async fn sweep_export_rows(
    transaction: &mut Transaction<'_, Sqlite>,
    exports: &[ExpiredExportRow],
    now: &str,
    result: &mut TransferSweepResult,
) -> Result<(), sqlx::Error> {
    for export in exports {
        if matches!(export.status.as_str(), "queued" | "running" | "finalizing") {
            let cancelled = sqlx::query(
                r"
                UPDATE export_jobs
                SET status = 'cancelled',
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
                  AND status = ?
                  AND datetime(expires_at) <= datetime(?)
                ",
            )
            .bind(&export.id)
            .bind(&export.status)
            .bind(now)
            .execute(&mut **transaction)
            .await?;
            if cancelled.rows_affected() > 0 {
                result.cancelled_exports.push(export.id.clone());
            }
        } else {
            let deleted = sqlx::query(
                r"
                DELETE FROM export_jobs
                WHERE id = ?
                  AND status = ?
                  AND datetime(expires_at) <= datetime(?)
                ",
            )
            .bind(&export.id)
            .bind(&export.status)
            .bind(now)
            .execute(&mut **transaction)
            .await?;
            if deleted.rows_affected() > 0 {
                result.deleted_exports.push(export.id.clone());
            }
        }
    }
    Ok(())
}

async fn old_enough_directory(path: &Path, now: SystemTime, minimum_age: Duration) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path).await else {
        return false;
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return false;
    }
    metadata
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= minimum_age)
}

async fn clear_export_temp_file(
    transfers_path: &Path,
    job_id: &str,
) -> Result<bool, std::io::Error> {
    if is_safe_transfer_id(job_id) {
        match fs::remove_file(
            transfers_path
                .join("exports")
                .join(format!("{job_id}.zip.tmp")),
        )
        .await
        {
            Ok(()) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn is_safe_transfer_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}
