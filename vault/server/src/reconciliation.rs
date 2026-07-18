use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqlitePool};
use thiserror::Error;

use crate::blob_lifecycle::{
    BlobLifecycleError, collect_unreferenced_blob_candidates, collect_untracked_blob_object,
};
use crate::storage::{
    BlobReadRange, LocalBlobStorage, LocalMultipartPartObject, LocalObjectReadGuard,
    STORAGE_CHUNK_SIZE, StorageError,
};

const UNREFERENCED_MULTIPART_PART_MINIMUM_AGE: Duration = Duration::from_hours(1);
const MULTIPART_PART_SCAN_WORK_LIMIT: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageReconciliationReport {
    pub orphan_blob_ids: Vec<i64>,
    pub unreferenced_local_keys: Vec<String>,
    pub missing_local_keys: Vec<String>,
    pub missing_local_location_keys: Vec<String>,
    pub corrupt_local_keys: Vec<String>,
    pub unreferenced_multipart_part_keys: Vec<String>,
    pub multipart_part_scan_complete: bool,
    pub deleted_local_keys: Vec<String>,
    pub deleted_multipart_part_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MultipartPartSweepResult {
    pub examined: usize,
    pub scan_complete: bool,
    pub deleted_multipart_part_keys: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ReconciliationError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    BlobLifecycle(#[from] BlobLifecycleError),
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
struct BlobRecord {
    id: i64,
    hash_algo: String,
    hash: String,
    size_bytes: i64,
}

#[derive(Debug, Clone, FromRow)]
struct LocalLocationRecord {
    blob_id: i64,
    object_key: String,
}

#[derive(Debug)]
struct StorageReconciliationState {
    referenced_blob_ids: HashSet<i64>,
    referenced_blobs: Vec<BlobRecord>,
    orphan_blobs: Vec<BlobRecord>,
    local_locations: Vec<LocalLocationRecord>,
    pending_local_keys: BTreeSet<String>,
    local_keys: BTreeSet<String>,
    recoverable_referenced_local_locations: BTreeSet<(i64, String)>,
    corrupt_local_keys: BTreeSet<String>,
    unreferenced_multipart_part_keys: Vec<String>,
    aged_unreferenced_multipart_part_keys: Vec<String>,
    multipart_part_minimum_age: Duration,
    multipart_part_scan_complete: bool,
}

#[derive(Debug)]
pub struct StorageReconciliationPlan {
    state: StorageReconciliationState,
    report: StorageReconciliationReport,
}

impl StorageReconciliationPlan {
    #[must_use]
    pub const fn report(&self) -> &StorageReconciliationReport {
        &self.report
    }
}

pub async fn storage_reconciliation_report(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    apply: bool,
) -> Result<StorageReconciliationReport, ReconciliationError> {
    storage_reconciliation_report_with_multipart_part_age(
        pool,
        storage,
        apply,
        UNREFERENCED_MULTIPART_PART_MINIMUM_AGE,
    )
    .await
}

pub async fn plan_storage_reconciliation(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
) -> Result<StorageReconciliationPlan, ReconciliationError> {
    plan_storage_reconciliation_with_multipart_part_age(
        pool,
        storage,
        UNREFERENCED_MULTIPART_PART_MINIMUM_AGE,
    )
    .await
}

pub async fn apply_storage_reconciliation_plan(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    plan: StorageReconciliationPlan,
) -> Result<StorageReconciliationReport, ReconciliationError> {
    let StorageReconciliationPlan { state, mut report } = plan;
    let (deleted_local_keys, deleted_multipart_part_keys) =
        apply_storage_reconciliation(pool, storage, &state, &report).await?;
    report.deleted_local_keys = deleted_local_keys;
    report.deleted_multipart_part_keys = deleted_multipart_part_keys;
    Ok(report)
}

pub async fn sweep_unreferenced_multipart_parts(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
) -> Result<MultipartPartSweepResult, ReconciliationError> {
    sweep_unreferenced_multipart_parts_with_options(
        pool,
        storage,
        UNREFERENCED_MULTIPART_PART_MINIMUM_AGE,
        MULTIPART_PART_SCAN_WORK_LIMIT,
    )
    .await
}

pub async fn sweep_unreferenced_multipart_parts_with_options(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    minimum_age: Duration,
    work_limit: usize,
) -> Result<MultipartPartSweepResult, ReconciliationError> {
    let (parts, scan_complete) = storage.scan_multipart_part_objects(work_limit).await?;
    let examined = parts.len();
    let (reachable, _) = multipart_part_protection(storage, &parts, &BTreeSet::new()).await?;
    let now = SystemTime::now();
    let mut deleted_multipart_part_keys = Vec::new();
    for part in parts {
        if reachable.contains(&part.object_key)
            || part
                .modified_at
                .and_then(|modified| now.duration_since(modified).ok())
                .is_none_or(|age| age < minimum_age)
        {
            continue;
        }
        if delete_unreferenced_multipart_part(pool, storage, &part.object_key, minimum_age).await? {
            deleted_multipart_part_keys.push(part.object_key);
        }
    }
    Ok(MultipartPartSweepResult {
        examined,
        scan_complete,
        deleted_multipart_part_keys,
    })
}

pub async fn storage_reconciliation_report_with_multipart_part_age(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    apply: bool,
    minimum_part_age: Duration,
) -> Result<StorageReconciliationReport, ReconciliationError> {
    let plan = plan_storage_reconciliation_with_multipart_part_age(pool, storage, minimum_part_age)
        .await?;
    if apply {
        return apply_storage_reconciliation_plan(pool, storage, plan).await;
    }
    Ok(plan.report)
}

async fn plan_storage_reconciliation_with_multipart_part_age(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    minimum_part_age: Duration,
) -> Result<StorageReconciliationPlan, ReconciliationError> {
    let state = load_reconciliation_state(pool, storage, minimum_part_age).await?;
    let report = reconciliation_report_from_state(&state);
    Ok(StorageReconciliationPlan { state, report })
}

async fn load_reconciliation_state(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    minimum_part_age: Duration,
) -> Result<StorageReconciliationState, ReconciliationError> {
    let document_blob_ids = id_set(
        sqlx::query_scalar::<_, i64>("SELECT blob_id FROM document_versions")
            .fetch_all(pool)
            .await?,
    );
    let export_blob_ids = id_set(
        sqlx::query_scalar::<_, i64>("SELECT blob_id FROM export_artifacts")
            .fetch_all(pool)
            .await?,
    );
    let preview_blob_ids = id_set(
        sqlx::query_scalar::<_, i64>("SELECT blob_id FROM preview_renditions")
            .fetch_all(pool)
            .await?,
    );
    let referenced_blob_ids = document_blob_ids
        .union(&export_blob_ids)
        .copied()
        .chain(preview_blob_ids)
        .collect::<HashSet<_>>();
    let all_blobs = sqlx::query_as::<_, BlobRecord>(
        "SELECT id, hash_algo, hash, size_bytes FROM blobs ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let referenced_blobs = all_blobs
        .iter()
        .filter(|blob| referenced_blob_ids.contains(&blob.id))
        .cloned()
        .collect::<Vec<_>>();
    let orphan_blobs = all_blobs
        .into_iter()
        .filter(|blob| !referenced_blob_ids.contains(&blob.id))
        .collect::<Vec<_>>();
    let local_locations = sqlx::query_as::<_, LocalLocationRecord>(
        "SELECT blob_id, object_key FROM blob_locations WHERE backend = 'local'",
    )
    .fetch_all(pool)
    .await?;
    let pending_local_keys = sqlx::query_scalar::<_, String>(
        r"
        SELECT object_key
        FROM blob_locations
        WHERE backend GLOB '_vault_pending:*:local'
           OR backend GLOB '_vault_deleting:*:local'
        ",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect::<BTreeSet<_>>();
    let local_keys = storage
        .list_object_keys()
        .await?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let (recoverable_referenced_local_locations, corrupt_local_keys) =
        local_recoverability(storage, &referenced_blobs, &local_locations, &local_keys).await?;
    let (
        unreferenced_multipart_part_keys,
        aged_unreferenced_multipart_part_keys,
        multipart_part_scan_complete,
    ) = multipart_part_snapshot(
        storage,
        &local_locations,
        &referenced_blob_ids,
        &pending_local_keys,
        minimum_part_age,
    )
    .await?;
    Ok(StorageReconciliationState {
        referenced_blob_ids,
        referenced_blobs,
        orphan_blobs,
        local_locations,
        pending_local_keys,
        local_keys,
        recoverable_referenced_local_locations,
        corrupt_local_keys,
        unreferenced_multipart_part_keys,
        aged_unreferenced_multipart_part_keys,
        multipart_part_minimum_age: minimum_part_age,
        multipart_part_scan_complete,
    })
}

async fn multipart_part_snapshot(
    storage: &LocalBlobStorage,
    local_locations: &[LocalLocationRecord],
    referenced_blob_ids: &HashSet<i64>,
    pending_local_keys: &BTreeSet<String>,
    minimum_part_age: Duration,
) -> Result<(Vec<String>, Vec<String>, bool), ReconciliationError> {
    let protected_indeterminate_manifests = local_locations
        .iter()
        .filter(|location| referenced_blob_ids.contains(&location.blob_id))
        .map(|location| location.object_key.clone())
        .collect::<BTreeSet<_>>()
        .union(pending_local_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    let (multipart_parts, scan_complete) = storage
        .scan_multipart_part_objects(MULTIPART_PART_SCAN_WORK_LIMIT)
        .await?;
    let (reachable, protected_prefixes) = multipart_part_protection(
        storage,
        &multipart_parts,
        &protected_indeterminate_manifests,
    )
    .await?;
    let now = SystemTime::now();
    let mut unreferenced = Vec::new();
    let mut aged = Vec::new();
    for part in multipart_parts {
        if reachable.contains(&part.object_key)
            || protected_prefixes
                .iter()
                .any(|prefix| part.object_key.starts_with(prefix))
        {
            continue;
        }
        if part
            .modified_at
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= minimum_part_age)
        {
            aged.push(part.object_key.clone());
        }
        unreferenced.push(part.object_key);
    }
    Ok((unreferenced, aged, scan_complete))
}

async fn multipart_part_protection(
    storage: &LocalBlobStorage,
    multipart_parts: &[LocalMultipartPartObject],
    protected_indeterminate_manifests: &BTreeSet<String>,
) -> Result<(BTreeSet<String>, BTreeSet<String>), ReconciliationError> {
    let mut reachable_parts = BTreeSet::new();
    let mut protected_prefixes = BTreeSet::new();
    let manifest_keys = multipart_parts
        .iter()
        .filter_map(|part| storage.multipart_manifest_key_for_part_object(&part.object_key))
        .collect::<BTreeSet<_>>();
    for manifest_key in manifest_keys {
        match storage.multipart_manifest_part_keys(&manifest_key).await {
            Ok(parts) => reachable_parts.extend(parts),
            Err(
                StorageError::NotFound
                | StorageError::InvalidMultipartManifest
                | StorageError::UnreadableMultipartManifest,
            ) if protected_indeterminate_manifests.contains(&manifest_key) => {
                if let Some(prefix) = multipart_part_prefix(&manifest_key) {
                    protected_prefixes.insert(prefix);
                }
            }
            Err(
                StorageError::NotFound
                | StorageError::InvalidMultipartManifest
                | StorageError::UnreadableMultipartManifest,
            ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok((reachable_parts, protected_prefixes))
}

fn multipart_part_prefix(manifest_key: &str) -> Option<String> {
    manifest_key
        .strip_suffix("manifest.json")
        .map(|prefix| format!("{prefix}parts/"))
}

async fn local_recoverability(
    storage: &LocalBlobStorage,
    referenced_blobs: &[BlobRecord],
    local_locations: &[LocalLocationRecord],
    local_keys: &BTreeSet<String>,
) -> Result<(BTreeSet<(i64, String)>, BTreeSet<String>), ReconciliationError> {
    let mut recoverable = BTreeSet::new();
    let mut corrupt = BTreeSet::new();
    let mut verification_cache = HashMap::<(i64, String), bool>::new();
    let referenced_blobs_by_id = referenced_blobs
        .iter()
        .map(|blob| (blob.id, blob))
        .collect::<HashMap<_, _>>();
    for blob in referenced_blobs {
        let object_key = storage.object_key_for_hash(&blob.hash_algo, &blob.hash);
        if !local_keys.contains(&object_key) {
            continue;
        }
        let cache_key = (blob.id, object_key.clone());
        let matches = verified_local_blob_guard(storage, blob, &object_key)
            .await
            .is_some();
        verification_cache.insert(cache_key, matches);
        if matches {
            recoverable.insert((blob.id, object_key));
        } else {
            corrupt.insert(object_key);
        }
    }
    for location in local_locations {
        let Some(blob) = referenced_blobs_by_id.get(&location.blob_id) else {
            continue;
        };
        if !local_keys.contains(&location.object_key) {
            continue;
        }
        let cache_key = (blob.id, location.object_key.clone());
        let matches = if let Some(matches) = verification_cache.get(&cache_key) {
            *matches
        } else {
            let matches = verified_local_blob_guard(storage, blob, &location.object_key)
                .await
                .is_some();
            verification_cache.insert(cache_key, matches);
            matches
        };
        if !matches {
            corrupt.insert(location.object_key.clone());
        }
    }
    Ok((recoverable, corrupt))
}

async fn verified_local_blob_guard(
    storage: &LocalBlobStorage,
    blob: &BlobRecord,
    object_key: &str,
) -> Option<LocalObjectReadGuard> {
    if blob.hash_algo != "sha256" {
        return None;
    }
    let expected_size = u64::try_from(blob.size_bytes).ok()?;
    let read_guard = storage.try_object_read_guard(object_key).ok()?;
    let mut stream = storage
        .stream_range(
            object_key,
            BlobReadRange {
                expected_size,
                offset: 0,
                length: expected_size,
            },
        )
        .await
        .ok()?;
    let mut remaining = expected_size;
    let mut hasher = Sha256::new();
    while remaining != 0 {
        let frame = stream.next().await?.ok()?;
        if frame.is_empty() || frame.len() > STORAGE_CHUNK_SIZE {
            return None;
        }
        let frame_len = u64::try_from(frame.len()).ok()?;
        remaining = remaining.checked_sub(frame_len)?;
        hasher.update(&frame);
    }
    (lower_hex(&hasher.finalize()) == blob.hash).then_some(read_guard)
}

fn reconciliation_report_from_state(
    state: &StorageReconciliationState,
) -> StorageReconciliationReport {
    let known_local_keys = state
        .local_locations
        .iter()
        .map(|location| location.object_key.clone())
        .collect::<BTreeSet<_>>()
        .union(&state.pending_local_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    let referenced_local_keys = state
        .local_locations
        .iter()
        .filter(|location| state.referenced_blob_ids.contains(&location.blob_id))
        .map(|location| location.object_key.clone())
        .collect::<BTreeSet<_>>();
    let local_location_pairs = state
        .local_locations
        .iter()
        .map(|location| (location.blob_id, location.object_key.clone()))
        .collect::<HashSet<_>>();
    let recoverable_keys = state
        .recoverable_referenced_local_locations
        .iter()
        .map(|(_, object_key)| object_key.clone())
        .collect::<BTreeSet<_>>();
    let unreferenced_local_keys = state
        .local_keys
        .difference(&known_local_keys)
        .filter(|key| !recoverable_keys.contains(*key))
        .filter(|key| !state.corrupt_local_keys.contains(*key))
        .cloned()
        .collect::<Vec<_>>();
    let missing_local_keys = referenced_local_keys
        .difference(&state.local_keys)
        .cloned()
        .collect::<Vec<_>>();
    let missing_local_location_keys = state
        .recoverable_referenced_local_locations
        .iter()
        .filter(|pair| !local_location_pairs.contains(pair))
        .map(|(_, object_key)| object_key.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    StorageReconciliationReport {
        orphan_blob_ids: state.orphan_blobs.iter().map(|blob| blob.id).collect(),
        unreferenced_local_keys,
        missing_local_keys,
        missing_local_location_keys,
        corrupt_local_keys: state.corrupt_local_keys.iter().cloned().collect(),
        unreferenced_multipart_part_keys: state.unreferenced_multipart_part_keys.clone(),
        multipart_part_scan_complete: state.multipart_part_scan_complete,
        deleted_local_keys: Vec::new(),
        deleted_multipart_part_keys: Vec::new(),
    }
}

async fn apply_storage_reconciliation(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    state: &StorageReconciliationState,
    report: &StorageReconciliationReport,
) -> Result<(Vec<String>, Vec<String>), ReconciliationError> {
    restore_missing_local_locations(pool, storage, state).await?;

    let protected_keys = referenced_protected_local_keys(state);
    // A referenced blob can be recoverable from an object key currently claimed by an orphan's
    // arbitrary location row. Until restore can transfer that ownership atomically, the generic
    // collector cannot discover the reference through its own location-based protection query.
    let protected_orphan_blob_ids = state
        .local_locations
        .iter()
        .filter(|location| protected_keys.contains(&location.object_key))
        .map(|location| location.blob_id)
        .collect::<HashSet<_>>();
    let orphan_blob_ids = state
        .orphan_blobs
        .iter()
        .map(|blob| blob.id)
        .filter(|blob_id| !protected_orphan_blob_ids.contains(blob_id))
        .collect::<Vec<_>>();
    let collected = collect_unreferenced_blob_candidates(pool, storage, &orphan_blob_ids).await?;
    let mut deleted_local_keys = collected.deleted_objects;

    for object_key in &report.unreferenced_local_keys {
        let collected = collect_untracked_blob_object(pool, storage, object_key).await?;
        deleted_local_keys.extend(collected.deleted_objects);
    }
    deleted_local_keys.sort();
    deleted_local_keys.dedup();

    let mut deleted_multipart_part_keys = Vec::new();
    for object_key in &state.aged_unreferenced_multipart_part_keys {
        if delete_unreferenced_multipart_part(
            pool,
            storage,
            object_key,
            state.multipart_part_minimum_age,
        )
        .await?
        {
            deleted_multipart_part_keys.push(object_key.clone());
        }
    }
    deleted_multipart_part_keys.sort();
    deleted_multipart_part_keys.dedup();
    Ok((deleted_local_keys, deleted_multipart_part_keys))
}

fn referenced_protected_local_keys(state: &StorageReconciliationState) -> BTreeSet<String> {
    let mut protected = state.pending_local_keys.clone();
    protected.extend(state.corrupt_local_keys.iter().cloned());
    protected.extend(
        state
            .recoverable_referenced_local_locations
            .iter()
            .map(|(_, object_key)| object_key.clone()),
    );
    protected.extend(
        state
            .local_locations
            .iter()
            .filter(|location| state.referenced_blob_ids.contains(&location.blob_id))
            .map(|location| location.object_key.clone()),
    );
    protected
}

async fn delete_unreferenced_multipart_part(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    object_key: &str,
    minimum_age: Duration,
) -> Result<bool, ReconciliationError> {
    let Some(manifest_key) = storage.multipart_manifest_key_for_part_object(object_key) else {
        return Ok(false);
    };
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (has_lifecycle_lease, has_referenced_local_location) = sqlx::query_as::<_, (i64, i64)>(
        r"
            SELECT
                EXISTS(
                    SELECT 1
                    FROM blob_locations
                    WHERE bucket = ''
                      AND object_key = ?
                      AND (
                          backend GLOB '_vault_pending:*:local'
                          OR backend GLOB '_vault_deleting:*:local'
                      )
                ),
                EXISTS(
                    SELECT 1
                    FROM blob_locations l
                    WHERE l.backend = 'local'
                      AND l.bucket = ''
                      AND l.object_key = ?
                      AND (
                          EXISTS(
                              SELECT 1 FROM document_versions v WHERE v.blob_id = l.blob_id
                          )
                          OR EXISTS(
                              SELECT 1 FROM export_artifacts a WHERE a.blob_id = l.blob_id
                          )
                          OR EXISTS(
                              SELECT 1 FROM preview_renditions p WHERE p.blob_id = l.blob_id
                          )
                      )
                )
            ",
    )
    .bind(&manifest_key)
    .bind(&manifest_key)
    .fetch_one(&mut *transaction)
    .await?;
    let deleted = if has_lifecycle_lease != 0 {
        false
    } else {
        storage
            .delete_unreferenced_multipart_part(
                object_key,
                minimum_age,
                has_referenced_local_location != 0,
            )
            .await?
    };
    transaction.commit().await?;
    Ok(deleted)
}

async fn restore_missing_local_locations(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    state: &StorageReconciliationState,
) -> Result<(), ReconciliationError> {
    let referenced_blobs = state
        .referenced_blobs
        .iter()
        .map(|blob| (blob.id, blob))
        .collect::<HashMap<_, _>>();
    for (blob_id, object_key) in &state.recoverable_referenced_local_locations {
        let exact_pair_exists = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM blob_locations WHERE blob_id = ? AND backend = 'local' AND bucket = '' AND object_key = ? LIMIT 1",
        )
        .bind(blob_id)
        .bind(object_key)
        .fetch_optional(pool)
        .await?
        .is_some();
        if exact_pair_exists {
            continue;
        }
        let Some(snapshot_blob) = referenced_blobs.get(blob_id) else {
            continue;
        };

        // Do not acquire SQLite's writer while hashing a multi-gigabyte object. The returned
        // explicit read guard remains held until the short recheck/insert transaction commits.
        let Some(verification_lease) =
            verified_local_blob_guard(storage, snapshot_blob, object_key).await
        else {
            continue;
        };
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let current_blob = sqlx::query_as::<_, BlobRecord>(
            r"
            SELECT b.id, b.hash_algo, b.hash, b.size_bytes
            FROM blobs b
            WHERE b.id = ?
              AND (
                    EXISTS (SELECT 1 FROM document_versions v WHERE v.blob_id = b.id)
                    OR EXISTS (SELECT 1 FROM export_artifacts a WHERE a.blob_id = b.id)
                    OR EXISTS (SELECT 1 FROM preview_renditions p WHERE p.blob_id = b.id)
                  )
            ",
        )
        .bind(blob_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if current_blob.as_ref() == Some(*snapshot_blob) {
            let (exact_pair_exists, object_key_claimed) = sqlx::query_as::<_, (i64, i64)>(
                r"
                    SELECT
                        EXISTS(
                            SELECT 1
                            FROM blob_locations
                            WHERE blob_id = ?
                              AND backend = 'local'
                              AND bucket = ''
                              AND object_key = ?
                        ),
                        EXISTS(
                            SELECT 1
                            FROM blob_locations
                            WHERE object_key = ?
                              AND (
                                    backend = 'local'
                                    OR backend GLOB '_vault_pending:*:local'
                                    OR backend GLOB '_vault_deleting:*:local'
                                  )
                        )
                    ",
            )
            .bind(blob_id)
            .bind(object_key)
            .bind(object_key)
            .fetch_one(&mut *transaction)
            .await?;
            if exact_pair_exists == 0 && object_key_claimed == 0 {
                sqlx::query(
                    "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 'local', '', ?)",
                )
                .bind(blob_id)
                .bind(object_key)
                .execute(&mut *transaction)
                .await?;
            }
        }
        transaction.commit().await?;
        drop(verification_lease);
    }
    Ok(())
}

fn id_set(ids: Vec<i64>) -> HashSet<i64> {
    ids.into_iter().collect()
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
