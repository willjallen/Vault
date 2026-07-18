//! Crash-recoverable publication and garbage collection for content-addressed blobs.
//!
//! A publisher commits an exact, UUID-bearing pending `blob_locations` row before writing
//! backend bytes. It may create a document/export reference only while that exact lease still
//! exists; the reference, canonical location, and lease removal commit atomically. Failed or
//! interrupted publications therefore retain a location-shaped cleanup record instead of
//! creating an unknowable object.
//!
//! Collection first replaces an unreferenced location with an exact deletion tombstone, then
//! performs the idempotent backend delete, and removes metadata only after success. A crash or
//! ambiguous backend response leaves the tombstone for retry. Pending and deleting backends are
//! reserved lifecycle states: download/reconciliation queries must never serve them, and code
//! outside this module must not directly discard unreferenced blob/location rows.

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use serde::Serialize;
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use tokio::time::{Instant, interval_at, timeout};
use uuid::Uuid;

use crate::storage::{BlobStorageBackend, BlobWriteKind, StorageError, StoredBlob};

const PENDING_BACKEND_PREFIX: &str = "_vault_pending:";
const DELETING_BACKEND_PREFIX: &str = "_vault_deleting:";
const UNTRACKED_RESERVATION_HASH_ALGO: &str = "_vault_untracked_reservation";
const PUBLICATION_HEARTBEAT_SECONDS: u64 = 15;
const PUBLICATION_STALE_SECONDS: i64 = 3_600;
const DELETE_TIMEOUT_SECONDS: u64 = 5;
const GC_RUN_BUDGET_SECONDS: u64 = 10;
const MAX_BLOB_GC_LIMIT: i64 = 32;
pub const DEFAULT_BLOB_GC_LIMIT: i64 = 16;

#[derive(Debug, Error)]
pub enum BlobLifecycleError {
    #[error("blob publication lease was lost before metadata commit")]
    PublicationLeaseLost,
    #[error("blob storage location is still being deleted; retry publication")]
    DeletionInProgress,
    #[error("storage returned a different object than the publication lease reserved")]
    PublicationLocationMismatch,
    #[error("storage location points at another blob")]
    StorageLocationConflict,
    #[error("blob size exceeds SQLite's supported integer range")]
    BlobSizeOutOfRange,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlobGcFailure {
    pub blob_id: i64,
    pub backend: String,
    pub bucket: String,
    pub object_key: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BlobGcResult {
    pub deleted_blob_ids: Vec<i64>,
    pub deleted_objects: Vec<String>,
    pub deferred_objects: Vec<String>,
    pub failures: Vec<BlobGcFailure>,
}

#[derive(Debug)]
pub struct PendingBlobPublication {
    pool: SqlitePool,
    lease_id: i64,
    lease_backend: String,
    blob_id: i64,
    planned: StoredBlob,
}

#[derive(Debug, Clone, FromRow)]
struct BlobLocationRow {
    id: i64,
    blob_id: i64,
    backend: String,
    bucket: String,
    object_key: String,
}

impl PendingBlobPublication {
    #[must_use]
    pub const fn blob_id(&self) -> i64 {
        self.blob_id
    }

    #[must_use]
    pub const fn planned(&self) -> &StoredBlob {
        &self.planned
    }

    pub async fn run_storage<F, T>(&self, operation: F) -> Result<T, StorageError>
    where
        F: Future<Output = Result<T, StorageError>>,
    {
        let heartbeat_at = Instant::now() + Duration::from_secs(PUBLICATION_HEARTBEAT_SECONDS);
        let mut heartbeat = interval_at(
            heartbeat_at,
            Duration::from_secs(PUBLICATION_HEARTBEAT_SECONDS),
        );
        tokio::pin!(operation);
        loop {
            tokio::select! {
                result = &mut operation => return result,
                _ = heartbeat.tick() => {
                    match sqlx::query(
                        r"
                        UPDATE blob_locations
                        SET created_at = CURRENT_TIMESTAMP
                        WHERE id = ? AND blob_id = ? AND backend = ? AND bucket = ? AND object_key = ?
                        ",
                    )
                    .bind(self.lease_id)
                    .bind(self.blob_id)
                    .bind(&self.lease_backend)
                    .bind(&self.planned.bucket)
                    .bind(&self.planned.object_key)
                    .execute(&self.pool)
                    .await
                    {
                        Ok(result) if result.rows_affected() == 1 => {}
                        Ok(_) => tracing::warn!(
                            blob_id = self.blob_id,
                            lease_id = self.lease_id,
                            "blob publication lease disappeared while storage write was running"
                        ),
                        Err(error) => tracing::warn!(
                            ?error,
                            blob_id = self.blob_id,
                            lease_id = self.lease_id,
                            "blob publication lease heartbeat failed"
                        ),
                    }
                }
            }
        }
    }

    pub async fn prepare_metadata_in_tx(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        stored: &StoredBlob,
    ) -> Result<i64, BlobLifecycleError> {
        if stored != &self.planned {
            return Err(BlobLifecycleError::PublicationLocationMismatch);
        }
        if target_has_deletion_in_tx(transaction, stored).await? {
            return Err(BlobLifecycleError::DeletionInProgress);
        }
        let lease_exists = sqlx::query_scalar::<_, i64>(
            r"
            SELECT 1
            FROM blob_locations
            WHERE id = ?
              AND blob_id = ?
              AND backend = ?
              AND bucket = ?
              AND object_key = ?
            ",
        )
        .bind(self.lease_id)
        .bind(self.blob_id)
        .bind(&self.lease_backend)
        .bind(&self.planned.bucket)
        .bind(&self.planned.object_key)
        .fetch_optional(&mut **transaction)
        .await?
        .is_some();
        if !lease_exists {
            return Err(BlobLifecycleError::PublicationLeaseLost);
        }
        insert_canonical_location_in_tx(transaction, self.blob_id, stored).await?;
        Ok(self.blob_id)
    }

    pub async fn finish_metadata_in_tx(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
    ) -> Result<(), BlobLifecycleError> {
        let deleted = sqlx::query(
            r"
            DELETE FROM blob_locations
            WHERE id = ?
              AND blob_id = ?
              AND backend = ?
              AND bucket = ?
              AND object_key = ?
            ",
        )
        .bind(self.lease_id)
        .bind(self.blob_id)
        .bind(&self.lease_backend)
        .bind(&self.planned.bucket)
        .bind(&self.planned.object_key)
        .execute(&mut **transaction)
        .await?;
        if deleted.rows_affected() != 1 {
            return Err(BlobLifecycleError::PublicationLeaseLost);
        }
        Ok(())
    }

    pub async fn abandon(self, stored: Option<&StoredBlob>) -> Result<(), BlobLifecycleError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let blob_id = get_or_create_blob_in_tx(&mut transaction, &self.planned).await?;
        insert_canonical_location_in_tx(&mut transaction, blob_id, &self.planned).await?;
        if let Some(stored) = stored.filter(|stored| *stored != &self.planned) {
            let actual_blob_id = get_or_create_blob_in_tx(&mut transaction, stored).await?;
            insert_canonical_location_in_tx(&mut transaction, actual_blob_id, stored).await?;
        }
        sqlx::query(
            r"
            DELETE FROM blob_locations
            WHERE id = ?
              AND blob_id = ?
              AND backend = ?
              AND bucket = ?
              AND object_key = ?
            ",
        )
        .bind(self.lease_id)
        .bind(self.blob_id)
        .bind(&self.lease_backend)
        .bind(&self.planned.bucket)
        .bind(&self.planned.object_key)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

pub async fn begin_blob_publication(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    hash_algo: &str,
    digest: &str,
    size_bytes: u64,
    write_kind: BlobWriteKind,
) -> Result<PendingBlobPublication, BlobLifecycleError> {
    let object_key = storage.planned_object_key(hash_algo, digest, write_kind)?;
    let planned = StoredBlob {
        hash_algo: hash_algo.to_string(),
        digest: digest.to_ascii_lowercase(),
        size_bytes,
        backend: storage.name().to_string(),
        bucket: storage.bucket().to_string(),
        object_key,
    };
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let blob_id = get_or_create_blob_in_tx(&mut transaction, &planned).await?;
    ensure_location_available_in_tx(&mut transaction, blob_id, &planned).await?;
    if target_has_deletion_in_tx(&mut transaction, &planned).await? {
        transaction.rollback().await?;
        return Err(BlobLifecycleError::DeletionInProgress);
    }
    let lease_backend = format!(
        "{PENDING_BACKEND_PREFIX}{}:{}",
        Uuid::new_v4().simple(),
        planned.backend
    );
    let lease_id = sqlx::query(
        r"
        INSERT INTO blob_locations (blob_id, backend, bucket, object_key)
        VALUES (?, ?, ?, ?)
        ",
    )
    .bind(blob_id)
    .bind(&lease_backend)
    .bind(&planned.bucket)
    .bind(&planned.object_key)
    .execute(&mut *transaction)
    .await?
    .last_insert_rowid();
    transaction.commit().await?;
    Ok(PendingBlobPublication {
        pool: pool.clone(),
        lease_id,
        lease_backend,
        blob_id,
        planned,
    })
}

pub async fn collect_unreferenced_blobs(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
) -> Result<BlobGcResult, BlobLifecycleError> {
    collect_unreferenced_blobs_with_limit(pool, storage, DEFAULT_BLOB_GC_LIMIT).await
}

/// Drops derived-cache metadata once no document version references its source.
///
/// A rendition can deduplicate to the exact same blob as its source. Releasing
/// the cache metadata before normal liveness checks prevents that strong output
/// reference from keeping an otherwise orphaned source alive forever.
async fn release_orphaned_preview_jobs(
    pool: &SqlitePool,
    source_blob_ids: Option<&[i64]>,
    limit: i64,
) -> Result<Vec<i64>, BlobLifecycleError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let source_blob_ids = if let Some(source_blob_ids) = source_blob_ids {
        let mut seen = HashSet::new();
        source_blob_ids
            .iter()
            .copied()
            .filter(|blob_id| seen.insert(*blob_id))
            .collect::<Vec<_>>()
    } else {
        sqlx::query_scalar::<_, i64>(
            r"
            SELECT DISTINCT pj.source_blob_id
            FROM preview_jobs pj
            WHERE NOT EXISTS (
                SELECT 1
                FROM document_versions v
                WHERE v.blob_id = pj.source_blob_id
            )
            ORDER BY pj.source_blob_id
            LIMIT ?
            ",
        )
        .bind(limit)
        .fetch_all(&mut *transaction)
        .await?
    };
    let mut released_blob_ids = Vec::new();
    for source_blob_id in source_blob_ids {
        let rendition_blob_ids = sqlx::query_scalar::<_, i64>(
            r"
            SELECT DISTINCT pr.blob_id
            FROM preview_jobs pj
            JOIN preview_renditions pr ON pr.preview_job_id = pj.id
            WHERE pj.source_blob_id = ?
              AND NOT EXISTS (
                  SELECT 1
                  FROM document_versions v
                  WHERE v.blob_id = pj.source_blob_id
              )
            ",
        )
        .bind(source_blob_id)
        .fetch_all(&mut *transaction)
        .await?;
        let deleted = sqlx::query(
            r"
            DELETE FROM preview_jobs
            WHERE source_blob_id = ?
              AND NOT EXISTS (
                  SELECT 1
                  FROM document_versions v
                  WHERE v.blob_id = preview_jobs.source_blob_id
              )
            ",
        )
        .bind(source_blob_id)
        .execute(&mut *transaction)
        .await?;
        if deleted.rows_affected() > 0 {
            released_blob_ids.push(source_blob_id);
            released_blob_ids.extend(rendition_blob_ids);
        }
    }
    transaction.commit().await?;
    released_blob_ids.sort_unstable();
    released_blob_ids.dedup();
    Ok(released_blob_ids)
}

// Keep candidate interleaving, bounded-run accounting, and result ordering in one audit surface.
#[allow(clippy::too_many_lines)]
pub async fn collect_unreferenced_blobs_with_limit(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    limit: i64,
) -> Result<BlobGcResult, BlobLifecycleError> {
    let limit = limit.clamp(1, MAX_BLOB_GC_LIMIT);
    let released_preview_blob_ids = release_orphaned_preview_jobs(pool, None, limit).await?;
    let maintenance_ids = sqlx::query_scalar::<_, i64>(
        r"
        SELECT DISTINCT l.blob_id
        FROM blob_locations l
        WHERE (l.bucket = '' OR l.bucket = ?)
          AND (
              l.backend GLOB ?
              OR (
                  l.backend GLOB ?
                  AND datetime(l.created_at) <= datetime('now', ?)
              )
          )
        ORDER BY l.created_at, l.blob_id
        LIMIT ?
        ",
    )
    .bind(storage.bucket())
    .bind(format!("{DELETING_BACKEND_PREFIX}*:{}", storage.name()))
    .bind(format!("{PENDING_BACKEND_PREFIX}*:{}", storage.name()))
    .bind(format!("-{PUBLICATION_STALE_SECONDS} seconds"))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let orphan_ids = sqlx::query_scalar::<_, i64>(
        r"
        SELECT b.id
        FROM blobs b
        WHERE NOT EXISTS (
                  SELECT 1 FROM document_versions v WHERE v.blob_id = b.id
              )
          AND NOT EXISTS (
                  SELECT 1 FROM export_artifacts a WHERE a.blob_id = b.id
              )
          AND NOT EXISTS (
                  SELECT 1 FROM preview_renditions p WHERE p.blob_id = b.id
              )
          AND NOT EXISTS (
                  SELECT 1
                  FROM blob_locations fresh
                  WHERE fresh.blob_id = b.id
                    AND fresh.backend GLOB ?
                    AND datetime(fresh.created_at) > datetime('now', ?)
              )
          AND (
                  NOT EXISTS (
                      SELECT 1 FROM blob_locations any_location WHERE any_location.blob_id = b.id
                  )
                  OR EXISTS (
                      SELECT 1
                      FROM blob_locations serviceable
                      WHERE serviceable.blob_id = b.id
                        AND (serviceable.bucket = '' OR serviceable.bucket = ?)
                        AND (
                            serviceable.backend = ?
                            OR serviceable.backend GLOB ?
                            OR serviceable.backend GLOB ?
                        )
                  )
              )
        ORDER BY b.id
        LIMIT ?
        ",
    )
    .bind(pending_backend_pattern())
    .bind(format!("-{PUBLICATION_STALE_SECONDS} seconds"))
    .bind(storage.bucket())
    .bind(storage.name())
    .bind(format!("{PENDING_BACKEND_PREFIX}*:{}", storage.name()))
    .bind(format!("{DELETING_BACKEND_PREFIX}*:{}", storage.name()))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut candidate_ids = Vec::with_capacity(
        maintenance_ids.len() + orphan_ids.len() + released_preview_blob_ids.len(),
    );
    for index in 0..maintenance_ids
        .len()
        .max(orphan_ids.len())
        .max(released_preview_blob_ids.len())
    {
        if let Some(blob_id) = maintenance_ids.get(index)
            && !candidate_ids.contains(blob_id)
        {
            candidate_ids.push(*blob_id);
        }
        if let Some(blob_id) = released_preview_blob_ids.get(index)
            && !candidate_ids.contains(blob_id)
        {
            candidate_ids.push(*blob_id);
        }
        if let Some(blob_id) = orphan_ids.get(index)
            && !candidate_ids.contains(blob_id)
        {
            candidate_ids.push(*blob_id);
        }
    }
    collect_blob_candidates(
        pool,
        storage,
        &candidate_ids,
        Some(Duration::from_secs(GC_RUN_BUDGET_SECONDS)),
    )
    .await
}

/// Collects only the supplied blob IDs while revalidating every candidate through the
/// crash-recoverable deletion state machine.
pub(crate) async fn collect_unreferenced_blob_candidates(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    candidate_ids: &[i64],
) -> Result<BlobGcResult, BlobLifecycleError> {
    let released_preview_blob_ids =
        release_orphaned_preview_jobs(pool, Some(candidate_ids), i64::MAX).await?;
    let mut all_candidate_ids =
        Vec::with_capacity(candidate_ids.len() + released_preview_blob_ids.len());
    all_candidate_ids.extend_from_slice(candidate_ids);
    all_candidate_ids.extend(released_preview_blob_ids);
    collect_blob_candidates(pool, storage, &all_candidate_ids, None).await
}

/// Reserves and collects a backend object that currently has no lifecycle metadata.
///
/// The reservation is committed before the backend effect, so a concurrent publisher either
/// wins the initial write transaction or observes the deletion tombstone and retries. A failed
/// or interrupted deletion remains discoverable by normal blob garbage collection.
pub(crate) async fn collect_untracked_blob_object(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    object_key: &str,
) -> Result<BlobGcResult, BlobLifecycleError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let existing_backends = sqlx::query_scalar::<_, String>(
        r"
        SELECT backend
        FROM blob_locations
        WHERE object_key = ?
          AND (bucket = '' OR bucket = ?)
        ",
    )
    .bind(object_key)
    .bind(storage.bucket())
    .fetch_all(&mut *transaction)
    .await?;
    if existing_backends
        .iter()
        .any(|backend| backend == storage.name() || actual_backend(backend) == Some(storage.name()))
    {
        transaction.commit().await?;
        let mut result = BlobGcResult {
            deferred_objects: vec![object_key.to_string()],
            ..BlobGcResult::default()
        };
        normalize_gc_result(&mut result);
        return Ok(result);
    }

    let reservation = Uuid::new_v4().simple().to_string();
    let blob_id = sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES (?, ?, 0)")
        .bind(UNTRACKED_RESERVATION_HASH_ALGO)
        .bind(&reservation)
        .execute(&mut *transaction)
        .await?
        .last_insert_rowid();
    let deleting_backend = format!("{DELETING_BACKEND_PREFIX}{reservation}:{}", storage.name());
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, ?, ?, ?)",
    )
    .bind(blob_id)
    .bind(deleting_backend)
    .bind(storage.bucket())
    .bind(object_key)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    collect_unreferenced_blob_candidates(pool, storage, &[blob_id]).await
}

async fn collect_blob_candidates(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    candidate_ids: &[i64],
    run_budget: Option<Duration>,
) -> Result<BlobGcResult, BlobLifecycleError> {
    let mut result = BlobGcResult::default();
    let started_at = Instant::now();
    let mut visited = HashSet::new();
    for &blob_id in candidate_ids {
        if !visited.insert(blob_id) {
            continue;
        }
        loop {
            if run_budget.is_some_and(|budget| started_at.elapsed() >= budget) {
                break;
            }
            if !collect_blob_candidate(pool, storage, blob_id, &mut result).await? {
                break;
            }
        }
    }
    normalize_gc_result(&mut result);
    Ok(result)
}

fn normalize_gc_result(result: &mut BlobGcResult) {
    result.deleted_blob_ids.sort_unstable();
    result.deleted_objects.sort();
    result.deleted_objects.dedup();
    result.deferred_objects.sort();
    result.deferred_objects.dedup();
    result.failures.sort_by(|left, right| {
        (&left.blob_id, &left.backend, &left.bucket, &left.object_key).cmp(&(
            &right.blob_id,
            &right.backend,
            &right.bucket,
            &right.object_key,
        ))
    });
}

// This is the deletion state machine: eligibility, claim, backend effect, and finalization order
// are intentionally visible together because reordering any phase can corrupt live references.
#[allow(clippy::too_many_lines)]
async fn collect_blob_candidate(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    blob_id: i64,
    result: &mut BlobGcResult,
) -> Result<bool, BlobLifecycleError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let blob_is_referenced = blob_is_referenced_in_tx(&mut transaction, blob_id).await?;
    let locations = sqlx::query_as::<_, BlobLocationRow>(
        r"
        SELECT id, blob_id, backend, bucket, object_key
        FROM blob_locations
        WHERE blob_id = ?
        ORDER BY
            CASE WHEN backend GLOB '_vault_deleting:*' THEN 1 ELSE 0 END,
            datetime(created_at),
            id
        ",
    )
    .bind(blob_id)
    .fetch_all(&mut *transaction)
    .await?;
    if locations.is_empty() {
        let deleted_blob = delete_blob_if_empty_in_tx(&mut transaction, blob_id).await?;
        transaction.commit().await?;
        if deleted_blob {
            result.deleted_blob_ids.push(blob_id);
        }
        return Ok(false);
    }
    let has_fresh_publication = blob_has_fresh_publication_in_tx(&mut transaction, blob_id).await?;
    let deleting_location = if blob_is_referenced || has_fresh_publication {
        locations
            .iter()
            .find(|location| {
                is_deleting_backend(&location.backend) && location_is_serviceable(storage, location)
            })
            .cloned()
    } else {
        None
    };
    let stale_pending_location = if deleting_location.is_none() && blob_is_referenced {
        let mut selected = None;
        for location in &locations {
            if is_pending_backend(&location.backend)
                && location_is_serviceable(storage, location)
                && !publication_is_fresh_in_tx(&mut transaction, location.id).await?
            {
                selected = Some(location.clone());
                break;
            }
        }
        selected
    } else {
        None
    };
    let location = if let Some(location) = deleting_location.or(stale_pending_location) {
        location
    } else if blob_is_referenced || has_fresh_publication {
        transaction.commit().await?;
        return Ok(false);
    } else {
        let Some(location) = locations
            .iter()
            .find(|location| location_is_serviceable(storage, location))
            .cloned()
        else {
            result
                .deferred_objects
                .push(locations[0].object_key.clone());
            transaction.commit().await?;
            return Ok(false);
        };
        location
    };
    let backend = actual_backend(&location.backend).unwrap_or(&location.backend);
    if target_is_protected_in_tx(
        &mut transaction,
        location.id,
        backend,
        &location.bucket,
        &location.object_key,
    )
    .await?
    {
        let deleted_stale_publication = if is_pending_backend(&location.backend) {
            sqlx::query("DELETE FROM blob_locations WHERE id = ?")
                .bind(location.id)
                .execute(&mut *transaction)
                .await?
                .rows_affected()
                == 1
        } else {
            false
        };
        let deleted_blob = if deleted_stale_publication {
            delete_blob_if_empty_in_tx(&mut transaction, blob_id).await?
        } else {
            false
        };
        transaction.commit().await?;
        if deleted_blob {
            result.deleted_blob_ids.push(blob_id);
        }
        if !deleted_stale_publication {
            result.deferred_objects.push(location.object_key.clone());
        }
        return Ok(deleted_stale_publication && !deleted_blob);
    }
    if blob_is_referenced && is_pending_backend(&location.backend) {
        result.deferred_objects.push(location.object_key.clone());
        transaction.commit().await?;
        return Ok(false);
    }
    let deleting_backend = if is_deleting_backend(&location.backend) {
        location.backend.clone()
    } else {
        format!(
            "{DELETING_BACKEND_PREFIX}{}:{backend}",
            Uuid::new_v4().simple()
        )
    };
    if !is_deleting_backend(&location.backend) {
        let claimed = sqlx::query(
            r"
            UPDATE blob_locations
            SET backend = ?, created_at = CURRENT_TIMESTAMP
            WHERE id = ? AND blob_id = ? AND backend = ?
            ",
        )
        .bind(&deleting_backend)
        .bind(location.id)
        .bind(blob_id)
        .bind(&location.backend)
        .execute(&mut *transaction)
        .await?;
        if claimed.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }
    }
    transaction.commit().await?;

    let deletion = timeout(
        Duration::from_secs(DELETE_TIMEOUT_SECONDS),
        storage.delete_location(backend, &location.bucket, &location.object_key),
    )
    .await;
    let mut finalization = pool.begin_with("BEGIN IMMEDIATE").await?;
    let tombstone_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM blob_locations WHERE id = ? AND blob_id = ? AND backend = ?",
    )
    .bind(location.id)
    .bind(blob_id)
    .bind(&deleting_backend)
    .fetch_optional(&mut *finalization)
    .await?
    .is_some();
    if !tombstone_exists {
        finalization.commit().await?;
        return Ok(true);
    }
    let failure = match deletion {
        Ok(Ok(())) => {
            sqlx::query("DELETE FROM blob_locations WHERE id = ? AND backend = ?")
                .bind(location.id)
                .bind(&deleting_backend)
                .execute(&mut *finalization)
                .await?;
            None
        }
        Ok(Err(error)) => Some(BlobGcFailure {
            blob_id,
            backend: backend.to_string(),
            bucket: location.bucket.clone(),
            object_key: location.object_key.clone(),
            error: error.to_string(),
        }),
        Err(_) => Some(BlobGcFailure {
            blob_id,
            backend: backend.to_string(),
            bucket: location.bucket.clone(),
            object_key: location.object_key.clone(),
            error: format!("storage deletion exceeded {DELETE_TIMEOUT_SECONDS} seconds"),
        }),
    };
    let deleted_blob = if failure.is_none() {
        delete_blob_if_empty_in_tx(&mut finalization, blob_id).await?
    } else {
        sqlx::query(
            "UPDATE blob_locations SET created_at = CURRENT_TIMESTAMP WHERE id = ? AND backend = ?",
        )
        .bind(location.id)
        .bind(&deleting_backend)
        .execute(&mut *finalization)
        .await?;
        false
    };
    finalization.commit().await?;
    if failure.is_none() {
        result.deleted_objects.push(location.object_key.clone());
    }
    let retry_same_blob = failure.is_none() && !deleted_blob;
    if let Some(failure) = failure {
        tracing::warn!(
            blob_id = failure.blob_id,
            backend = %failure.backend,
            bucket = %failure.bucket,
            object_key = %failure.object_key,
            error = %failure.error,
            "unreferenced blob deletion will be retried"
        );
        result.failures.push(failure);
    }
    if deleted_blob {
        result.deleted_blob_ids.push(blob_id);
    }
    Ok(retry_same_blob)
}

async fn delete_blob_if_empty_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    blob_id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        r"
        DELETE FROM blobs
        WHERE id = ?
          AND NOT EXISTS (
                  SELECT 1 FROM document_versions v WHERE v.blob_id = blobs.id
              )
          AND NOT EXISTS (
                  SELECT 1 FROM export_artifacts a WHERE a.blob_id = blobs.id
              )
          AND NOT EXISTS (
                  SELECT 1 FROM preview_renditions p WHERE p.blob_id = blobs.id
              )
          AND NOT EXISTS (
                  SELECT 1 FROM blob_locations l WHERE l.blob_id = blobs.id
              )
        ",
    )
    .bind(blob_id)
    .execute(&mut **transaction)
    .await?
    .rows_affected()
        == 1)
}

async fn get_or_create_blob_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    stored: &StoredBlob,
) -> Result<i64, BlobLifecycleError> {
    let size_bytes =
        i64::try_from(stored.size_bytes).map_err(|_| BlobLifecycleError::BlobSizeOutOfRange)?;
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
    Ok(sqlx::query_scalar::<_, i64>(
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
    .await?)
}

async fn ensure_location_available_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    blob_id: i64,
    stored: &StoredBlob,
) -> Result<(), BlobLifecycleError> {
    let existing_blob_id = sqlx::query_scalar::<_, i64>(
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
    if existing_blob_id.is_some_and(|existing_blob_id| existing_blob_id != blob_id) {
        return Err(BlobLifecycleError::StorageLocationConflict);
    }
    Ok(())
}

async fn insert_canonical_location_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    blob_id: i64,
    stored: &StoredBlob,
) -> Result<(), BlobLifecycleError> {
    ensure_location_available_in_tx(transaction, blob_id, stored).await?;
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
    Ok(())
}

async fn blob_is_referenced_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    blob_id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, i64>(
        r"
        SELECT 1
        WHERE EXISTS (SELECT 1 FROM document_versions WHERE blob_id = ?)
           OR EXISTS (SELECT 1 FROM export_artifacts WHERE blob_id = ?)
           OR EXISTS (SELECT 1 FROM preview_renditions WHERE blob_id = ?)
        ",
    )
    .bind(blob_id)
    .bind(blob_id)
    .bind(blob_id)
    .fetch_optional(&mut **transaction)
    .await?
    .is_some())
}

async fn blob_has_fresh_publication_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    blob_id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, i64>(
        r"
        SELECT 1
        FROM blob_locations
        WHERE blob_id = ?
          AND backend GLOB ?
          AND datetime(created_at) > datetime('now', ?)
        LIMIT 1
        ",
    )
    .bind(blob_id)
    .bind(pending_backend_pattern())
    .bind(format!("-{PUBLICATION_STALE_SECONDS} seconds"))
    .fetch_optional(&mut **transaction)
    .await?
    .is_some())
}

async fn target_is_protected_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    location_id: i64,
    backend: &str,
    bucket: &str,
    object_key: &str,
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query_as::<_, BlobLocationRow>(
        r"
        SELECT id, blob_id, backend, bucket, object_key
        FROM blob_locations
        WHERE id != ? AND bucket = ? AND object_key = ?
        ",
    )
    .bind(location_id)
    .bind(bucket)
    .bind(object_key)
    .fetch_all(&mut **transaction)
    .await?;
    for row in rows {
        if is_pending_backend(&row.backend)
            && actual_backend(&row.backend) == Some(backend)
            && publication_is_fresh_in_tx(transaction, row.id).await?
        {
            return Ok(true);
        }
        let deletion_protects_target =
            is_deleting_backend(&row.backend) && actual_backend(&row.backend) == Some(backend);
        let reference_protects_target =
            row.backend == backend && blob_is_referenced_in_tx(transaction, row.blob_id).await?;
        if deletion_protects_target || reference_protects_target {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn publication_is_fresh_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    location_id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, i64>(
        r"
        SELECT 1
        FROM blob_locations
        WHERE id = ?
          AND datetime(created_at) > datetime('now', ?)
        ",
    )
    .bind(location_id)
    .bind(format!("-{PUBLICATION_STALE_SECONDS} seconds"))
    .fetch_optional(&mut **transaction)
    .await?
    .is_some())
}

fn location_is_serviceable(storage: &dyn BlobStorageBackend, location: &BlobLocationRow) -> bool {
    let backend = actual_backend(&location.backend).unwrap_or(&location.backend);
    storage.require_location(backend, &location.bucket).is_ok()
}

#[must_use]
pub fn is_pending_backend(backend: &str) -> bool {
    backend.starts_with(PENDING_BACKEND_PREFIX)
}

fn is_deleting_backend(backend: &str) -> bool {
    backend.starts_with(DELETING_BACKEND_PREFIX)
}

fn actual_backend(backend: &str) -> Option<&str> {
    backend
        .strip_prefix(PENDING_BACKEND_PREFIX)
        .or_else(|| backend.strip_prefix(DELETING_BACKEND_PREFIX))?
        .split_once(':')
        .map(|(_, actual_backend)| actual_backend)
}

async fn target_has_deletion_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    stored: &StoredBlob,
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, String>(
        r"
        SELECT backend
        FROM blob_locations
        WHERE bucket = ? AND object_key = ? AND backend GLOB ?
        ",
    )
    .bind(&stored.bucket)
    .bind(&stored.object_key)
    .bind(deleting_backend_pattern())
    .fetch_all(&mut **transaction)
    .await?;
    Ok(rows
        .iter()
        .any(|backend| actual_backend(backend) == Some(stored.backend.as_str())))
}

fn pending_backend_pattern() -> &'static str {
    "_vault_pending:*"
}

fn deleting_backend_pattern() -> &'static str {
    "_vault_deleting:*"
}
