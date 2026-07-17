use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqlitePool};
use thiserror::Error;

use crate::storage::{LocalBlobStorage, LocalMultipartPartObject, StorageError};

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
}

#[derive(Debug, Clone, FromRow)]
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
    let state = load_reconciliation_state(pool, storage, minimum_part_age).await?;
    let report = reconciliation_report_from_state(&state, false);
    if apply {
        let deleted_multipart_part_keys =
            apply_storage_reconciliation(pool, storage, &state, &report).await?;
        let mut applied = reconciliation_report_from_state(&state, true);
        applied.deleted_multipart_part_keys = deleted_multipart_part_keys;
        return Ok(applied);
    }
    Ok(report)
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
    let referenced_blob_ids = document_blob_ids
        .union(&export_blob_ids)
        .copied()
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
    let referenced_blobs_by_id = referenced_blobs
        .iter()
        .map(|blob| (blob.id, blob))
        .collect::<HashMap<_, _>>();
    for blob in referenced_blobs {
        let object_key = storage.object_key_for_hash(&blob.hash_algo, &blob.hash);
        if !local_keys.contains(&object_key) {
            continue;
        }
        match storage.read_bytes(&object_key).await {
            Ok(data) if blob_bytes_match(blob, &data) => {
                recoverable.insert((blob.id, object_key));
            }
            Ok(_) | Err(_) => {
                corrupt.insert(object_key);
            }
        }
    }
    for location in local_locations {
        let Some(blob) = referenced_blobs_by_id.get(&location.blob_id) else {
            continue;
        };
        if !local_keys.contains(&location.object_key) {
            continue;
        }
        match storage.read_bytes(&location.object_key).await {
            Ok(data) if blob_bytes_match(blob, &data) => {}
            Ok(_) | Err(_) => {
                corrupt.insert(location.object_key.clone());
            }
        }
    }
    Ok((recoverable, corrupt))
}

fn reconciliation_report_from_state(
    state: &StorageReconciliationState,
    applied: bool,
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
    let referenced_protected_local_keys = referenced_local_keys
        .union(&recoverable_keys)
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(&state.corrupt_local_keys)
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(&state.pending_local_keys)
        .cloned()
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
    let orphan_local_keys_to_delete = orphan_local_keys_to_delete(
        &state.orphan_blobs,
        &state.local_locations,
        &referenced_protected_local_keys,
    );
    let deleted_local_keys = if applied {
        unreferenced_local_keys
            .iter()
            .cloned()
            .chain(orphan_local_keys_to_delete)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    StorageReconciliationReport {
        orphan_blob_ids: state.orphan_blobs.iter().map(|blob| blob.id).collect(),
        unreferenced_local_keys,
        missing_local_keys,
        missing_local_location_keys,
        corrupt_local_keys: state.corrupt_local_keys.iter().cloned().collect(),
        unreferenced_multipart_part_keys: state.unreferenced_multipart_part_keys.clone(),
        multipart_part_scan_complete: state.multipart_part_scan_complete,
        deleted_local_keys,
        deleted_multipart_part_keys: Vec::new(),
    }
}

async fn apply_storage_reconciliation(
    pool: &SqlitePool,
    storage: &LocalBlobStorage,
    state: &StorageReconciliationState,
    report: &StorageReconciliationReport,
) -> Result<Vec<String>, ReconciliationError> {
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
    let recoverable_keys = state
        .recoverable_referenced_local_locations
        .iter()
        .map(|(_, object_key)| object_key.clone())
        .collect::<BTreeSet<_>>();
    let referenced_protected_local_keys = referenced_local_keys
        .union(&recoverable_keys)
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(&state.corrupt_local_keys)
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(&state.pending_local_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    let orphan_local_keys_to_delete = orphan_local_keys_to_delete(
        &state.orphan_blobs,
        &state.local_locations,
        &referenced_protected_local_keys,
    );
    for object_key in &orphan_local_keys_to_delete {
        storage.delete_object(object_key).await?;
    }
    for object_key in &report.unreferenced_local_keys {
        if !known_local_keys.contains(object_key) {
            storage.delete_object(object_key).await?;
        }
    }
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
    let orphan_blob_ids = state
        .orphan_blobs
        .iter()
        .map(|blob| blob.id)
        .collect::<Vec<_>>();
    if !orphan_blob_ids.is_empty() {
        delete_orphan_local_locations(pool, &orphan_blob_ids).await?;
        delete_local_only_orphan_blobs(pool, &orphan_blob_ids).await?;
    }
    restore_missing_local_locations(pool, &state.recoverable_referenced_local_locations).await?;
    Ok(deleted_multipart_part_keys)
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

async fn delete_orphan_local_locations(
    pool: &SqlitePool,
    orphan_blob_ids: &[i64],
) -> Result<(), ReconciliationError> {
    for blob_id in orphan_blob_ids {
        sqlx::query("DELETE FROM blob_locations WHERE blob_id = ? AND backend = 'local'")
            .bind(blob_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn delete_local_only_orphan_blobs(
    pool: &SqlitePool,
    orphan_blob_ids: &[i64],
) -> Result<(), ReconciliationError> {
    for blob_id in orphan_blob_ids {
        let has_remote_location = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM blob_locations WHERE blob_id = ? AND backend != 'local' LIMIT 1",
        )
        .bind(blob_id)
        .fetch_optional(pool)
        .await?
        .is_some();
        if !has_remote_location {
            sqlx::query("DELETE FROM blobs WHERE id = ?")
                .bind(blob_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

async fn restore_missing_local_locations(
    pool: &SqlitePool,
    recoverable_locations: &BTreeSet<(i64, String)>,
) -> Result<(), ReconciliationError> {
    for (blob_id, object_key) in recoverable_locations {
        let exact_pair_exists = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM blob_locations WHERE blob_id = ? AND backend = 'local' AND object_key = ? LIMIT 1",
        )
        .bind(blob_id)
        .bind(object_key)
        .fetch_optional(pool)
        .await?
        .is_some();
        if exact_pair_exists {
            continue;
        }
        let object_key_claimed = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM blob_locations WHERE backend = 'local' AND bucket = '' AND object_key = ? LIMIT 1",
        )
        .bind(object_key)
        .fetch_optional(pool)
        .await?
        .is_some();
        if object_key_claimed {
            continue;
        }
        sqlx::query(
            "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 'local', '', ?)",
        )
        .bind(blob_id)
        .bind(object_key)
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn orphan_local_keys_to_delete(
    orphan_blobs: &[BlobRecord],
    local_locations: &[LocalLocationRecord],
    protected_keys: &BTreeSet<String>,
) -> BTreeSet<String> {
    let orphan_blob_ids = orphan_blobs
        .iter()
        .map(|blob| blob.id)
        .collect::<HashSet<_>>();
    local_locations
        .iter()
        .filter(|location| orphan_blob_ids.contains(&location.blob_id))
        .filter(|location| !protected_keys.contains(&location.object_key))
        .map(|location| location.object_key.clone())
        .collect()
}

fn id_set(ids: Vec<i64>) -> HashSet<i64> {
    ids.into_iter().collect()
}

fn blob_bytes_match(blob: &BlobRecord, data: &[u8]) -> bool {
    blob.hash_algo == "sha256"
        && blob.size_bytes >= 0
        && usize::try_from(blob.size_bytes).is_ok_and(|size| size == data.len())
        && lower_hex(&Sha256::digest(data)) == blob.hash
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
