use std::path::Path;

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::fs;

use crate::exports::{self, ExportError, ExportExecutionContext};
use crate::storage::{SharedBlobStorage, StorageError};

const DEFAULT_SWEEP_LIMIT: i64 = 250;
const DEFAULT_STARTUP_EXPORT_LIMIT: i64 = 1000;

#[derive(Debug, Clone, Default, Serialize)]
pub struct TransferSweepResult {
    pub expired_uploads: Vec<String>,
    pub deleted_uploads: Vec<String>,
    pub cancelled_exports: Vec<String>,
    pub deleted_exports: Vec<String>,
    pub deleted_export_objects: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TransferRecoveryResult {
    pub resumed_uploads: Vec<String>,
    pub failed_uploads: Vec<String>,
    pub requeued_exports: Vec<String>,
    pub queued_exports: Vec<String>,
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
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    TimeFormat(#[from] time::error::Format),
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
) -> Result<TransferSweepResult, TransferMaintenanceError> {
    sweep_expired_transfers_with_limit(pool, storage, transfers_path, DEFAULT_SWEEP_LIMIT).await
}

pub async fn sweep_expired_transfers_with_limit(
    pool: &SqlitePool,
    _storage: &SharedBlobStorage,
    transfers_path: &Path,
    limit: i64,
) -> Result<TransferSweepResult, TransferMaintenanceError> {
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let limit = limit.max(1);
    let uploads = expired_uploads(pool, &now, limit).await?;
    let exports = expired_exports(pool, &now, limit).await?;
    let mut result = TransferSweepResult::default();

    let mut transaction = pool.begin().await?;
    sweep_upload_rows(&mut transaction, &uploads, &mut result).await?;
    sweep_export_rows(&mut transaction, &exports, &mut result).await?;
    transaction.commit().await?;

    for session_id in result
        .expired_uploads
        .iter()
        .chain(result.deleted_uploads.iter())
    {
        clear_upload_session_files(transfers_path, session_id).await;
    }
    for job_id in result
        .cancelled_exports
        .iter()
        .chain(result.deleted_exports.iter())
    {
        clear_export_temp_file(transfers_path, job_id).await?;
    }
    Ok(result)
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
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let uploads = interrupted_uploads(pool, &now).await?;
    let exports = interrupted_exports(pool, &now).await?;
    let mut result = TransferRecoveryResult::default();
    let mut failed_upload_dirs = Vec::new();
    let mut artifact_blob_ids = Vec::new();

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
                    error = 'Upload completion interrupted and part files are missing',
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
                ",
            )
            .bind(&upload.id)
            .execute(&mut *transaction)
            .await?;
            result.failed_uploads.push(upload.id.clone());
            failed_upload_dirs.push(upload.id.clone());
        }
    }

    for export in &exports {
        let blob_ids =
            sqlx::query_scalar::<_, i64>("SELECT blob_id FROM export_artifacts WHERE job_id = ?")
                .bind(&export.id)
                .fetch_all(&mut *transaction)
                .await?;
        artifact_blob_ids.extend(blob_ids);
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
    delete_unreferenced_blobs(&mut transaction, &artifact_blob_ids).await?;
    transaction.commit().await?;

    for session_id in failed_upload_dirs {
        clear_upload_session_files(transfers_path, &session_id).await;
    }
    for job_id in &result.requeued_exports {
        if clear_export_temp_file(transfers_path, job_id).await? {
            result
                .deleted_export_temps
                .push(format!("{job_id}.zip.tmp"));
        }
    }
    if enqueue_exports {
        result.queued_exports =
            enqueue_pending_exports(pool, storage, transfers_path, export_execution).await?;
    }
    Ok(result)
}

async fn enqueue_pending_exports(
    pool: &SqlitePool,
    storage: &SharedBlobStorage,
    transfers_path: &Path,
    export_execution: Option<&ExportExecutionContext>,
) -> Result<Vec<String>, TransferMaintenanceError> {
    if let Some(execution) = export_execution {
        Ok(exports::start_pending_export_jobs_with_runtime(
            pool,
            storage,
            transfers_path,
            DEFAULT_STARTUP_EXPORT_LIMIT,
            execution,
        )
        .await?)
    } else {
        Ok(exports::start_pending_export_jobs(
            pool,
            storage,
            transfers_path,
            DEFAULT_STARTUP_EXPORT_LIMIT,
        )
        .await?)
    }
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
    if upload.part_count < 0 || upload.chunk_size <= 0 || upload.total_size < 0 {
        return Ok(false);
    }
    let session_dir = transfers_path.join("uploads").join(&upload.id);
    let mut recovered_size = 0_i64;
    for part_number in 1..=upload.part_count {
        let metadata_path = session_dir.join(format!("{part_number:08}.json"));
        let part_path = session_dir.join(format!("{part_number:08}.part"));
        let metadata_bytes = match fs::read(&metadata_path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let metadata: UploadPartMetadata = serde_json::from_slice(&metadata_bytes)?;
        if metadata.part_number != part_number {
            return Ok(false);
        }
        let expected_offset = (part_number - 1) * upload.chunk_size;
        let expected_size = if part_number == upload.part_count {
            upload.total_size - expected_offset
        } else {
            upload.chunk_size
        };
        if expected_size < 0
            || metadata.offset_bytes != expected_offset
            || metadata.size_bytes != expected_size
        {
            return Ok(false);
        }
        let part_size = match fs::metadata(&part_path).await {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if i64::try_from(part_size).ok() != Some(expected_size) {
            return Ok(false);
        }
        recovered_size += expected_size;
    }
    Ok(recovered_size == upload.total_size)
}

async fn sweep_upload_rows(
    transaction: &mut Transaction<'_, Sqlite>,
    uploads: &[ExpiredUploadRow],
    result: &mut TransferSweepResult,
) -> Result<(), sqlx::Error> {
    for upload in uploads {
        if matches!(upload.status.as_str(), "active" | "completing") {
            sqlx::query(
                r"
                UPDATE upload_sessions
                SET status = 'expired',
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
                ",
            )
            .bind(&upload.id)
            .execute(&mut **transaction)
            .await?;
            result.expired_uploads.push(upload.id.clone());
        } else {
            sqlx::query("DELETE FROM upload_sessions WHERE id = ?")
                .bind(&upload.id)
                .execute(&mut **transaction)
                .await?;
            result.deleted_uploads.push(upload.id.clone());
        }
    }
    Ok(())
}

async fn sweep_export_rows(
    transaction: &mut Transaction<'_, Sqlite>,
    exports: &[ExpiredExportRow],
    result: &mut TransferSweepResult,
) -> Result<(), sqlx::Error> {
    let mut deleted_blob_ids = Vec::new();
    for export in exports {
        if matches!(export.status.as_str(), "queued" | "running" | "finalizing") {
            sqlx::query(
                r"
                UPDATE export_jobs
                SET status = 'cancelled',
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?
                ",
            )
            .bind(&export.id)
            .execute(&mut **transaction)
            .await?;
            result.cancelled_exports.push(export.id.clone());
        } else {
            let artifact_blob_ids = sqlx::query_scalar::<_, i64>(
                "SELECT blob_id FROM export_artifacts WHERE job_id = ?",
            )
            .bind(&export.id)
            .fetch_all(&mut **transaction)
            .await?;
            deleted_blob_ids.extend(artifact_blob_ids);
            sqlx::query("DELETE FROM export_jobs WHERE id = ?")
                .bind(&export.id)
                .execute(&mut **transaction)
                .await?;
            result.deleted_exports.push(export.id.clone());
        }
    }
    delete_unreferenced_blobs(transaction, &deleted_blob_ids).await
}

async fn delete_unreferenced_blobs(
    transaction: &mut Transaction<'_, Sqlite>,
    blob_ids: &[i64],
) -> Result<(), sqlx::Error> {
    for blob_id in blob_ids {
        if blob_is_referenced(transaction, *blob_id).await? {
            continue;
        }
        sqlx::query("DELETE FROM blobs WHERE id = ?")
            .bind(blob_id)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
}

async fn blob_is_referenced(
    transaction: &mut Transaction<'_, Sqlite>,
    blob_id: i64,
) -> Result<bool, sqlx::Error> {
    let document_reference =
        sqlx::query_scalar::<_, i64>("SELECT 1 FROM document_versions WHERE blob_id = ? LIMIT 1")
            .bind(blob_id)
            .fetch_optional(&mut **transaction)
            .await?
            .is_some();
    if document_reference {
        return Ok(true);
    }
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT 1 FROM export_artifacts WHERE blob_id = ? LIMIT 1")
            .bind(blob_id)
            .fetch_optional(&mut **transaction)
            .await?
            .is_some(),
    )
}

async fn clear_upload_session_files(transfers_path: &Path, session_id: &str) {
    if is_safe_transfer_id(session_id) {
        let _ = fs::remove_dir_all(transfers_path.join("uploads").join(session_id)).await;
    }
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
