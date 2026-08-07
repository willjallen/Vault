#![allow(clippy::too_many_lines)]
//! Exhaustive, read-only stored-object and transfer-working-tree auditing.
//!
//! This scanner deliberately does not use the runtime reconciliation entry
//! points: those may create storage directories or apply cleanup. Every path
//! reached here is inventoried with `symlink_metadata`, and no symlink is
//! followed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncReadExt;

use super::database::{BlobRecord, DatabaseInventory, LiveReferenceKind, UploadSessionRecord};
use super::report::{ReportBuilder, ScanCounters, Severity};
use crate::config::Config;
use crate::storage::{
    BlobReadRange, BlobStorageBackend, LOCAL_MULTIPART_FORMAT, S3_UPLOAD_STAGE_FILENAME,
    STORAGE_CHUNK_SIZE, STORAGE_MULTIPART_MAX_PARTS, StorageError, StorageObjectInventoryEntry,
    multipart_manifest_key_for_hash, multipart_part_key_for_hash,
    multipart_part_key_for_hash_layout, normalize_storage_prefix, object_key_for_hash,
};

const CHECK_LOCATIONS: &str = "storage.locations";
const CHECK_CONTENT: &str = "storage.content";
const CHECK_INVENTORY: &str = "storage.inventory";
const CHECK_WORKING: &str = "storage.working_data";

const MAX_MULTIPART_MANIFEST_BYTES: u64 = 512 * 1024;
const MAX_UPLOAD_PART_METADATA_BYTES: u64 = 4096;
const MAX_REMOTE_INVENTORY_PAGE_ENTRIES: usize = 1000;
const MAX_LOCAL_INVENTORY_ENTRIES: usize = 1_000_000;
const MAX_PREVIEW_PAYLOAD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PREVIEW_DIMENSION: u32 = 512;
const MAX_PREVIEW_DECODED_BYTES: u64 = 16 * 1024 * 1024;
const LEGACY_S3_STAGE_MINIMUM_AGE: Duration = Duration::from_hours(168);

pub(crate) fn mark_durable_checks_incomplete(report: &mut ReportBuilder) {
    for check in [CHECK_LOCATIONS, CHECK_CONTENT, CHECK_INVENTORY] {
        report.mark_check_incomplete(check);
    }
}

pub(crate) fn mark_transfer_check_incomplete(report: &mut ReportBuilder) {
    report.mark_check_incomplete(CHECK_WORKING);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalEntryKind {
    Directory,
    File,
    Symlink,
    Special,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    length: u64,
    modified_nanos: Option<u128>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalEntry {
    path: PathBuf,
    relative: PathBuf,
    kind: LocalEntryKind,
    identity: Option<FileIdentity>,
}

#[derive(Debug, Default)]
struct LocalTreeInventory {
    entries: Vec<LocalEntry>,
    issues: Vec<(PathBuf, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CopyKey {
    backend: String,
    bucket: String,
    object_key: String,
}

#[derive(Debug, Clone)]
struct CopyVerification {
    key: CopyKey,
    blob_id: i64,
    bytes_read: u64,
    digest: Option<String>,
    error: Option<CopyFailure>,
    indeterminate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyFailureKind {
    Missing,
    Invalid,
    Unavailable,
}

#[derive(Debug, Clone)]
struct CopyFailure {
    kind: CopyFailureKind,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MultipartManifestPayload {
    format: String,
    hash_algo: String,
    digest: String,
    size_bytes: u64,
    parts: Vec<MultipartManifestPart>,
}

#[derive(Debug, Clone, Deserialize)]
struct MultipartManifestPart {
    object_key: String,
    size_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadPartMetadata {
    part_number: i64,
    offset_bytes: i64,
    size_bytes: i64,
    sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManagedKeyKind {
    Direct {
        digest: String,
    },
    Manifest {
        digest: String,
    },
    LegacyPart {
        digest: String,
        part_number: usize,
    },
    LayoutPart {
        digest: String,
        layout_id: String,
        part_number: usize,
    },
    Malformed,
    OutsidePrefix,
}

/// Audits every database-named serviceable copy, the configured backend's
/// complete inventory, local multipart structures, and transfer working data.
pub(crate) async fn run_storage_scan(
    config: &Config,
    storage: &dyn BlobStorageBackend,
    database: &DatabaseInventory,
    report: &mut ReportBuilder,
) {
    for check in [CHECK_LOCATIONS, CHECK_CONTENT, CHECK_INVENTORY] {
        report.ensure_check(check);
    }
    if !database.complete_for_storage {
        incomplete(
            report,
            CHECK_LOCATIONS,
            "storage.database_inventory_incomplete",
            None,
            "database metadata inventory was incomplete, so missing/untracked cross-correlation cannot be authoritative",
        );
    }

    let prefix = normalize_storage_prefix(&config.storage_prefix);
    let mut content_counters = ScanCounters::default();
    let first_inventory = if storage.name() == "local" {
        local_inventory_pass(&config.objects_path(), report)
            .await
            .map(BackendInventory::Local)
    } else {
        remote_inventory_pass(storage, database, &prefix, &mut content_counters, report)
            .await
            .map(BackendInventory::Remote)
    };

    let verifications = verify_database_locations(
        config,
        storage,
        database,
        first_inventory.as_ref(),
        &mut content_counters,
        report,
    )
    .await;
    verify_preview_rendition_payloads(
        config,
        storage,
        database,
        &verifications,
        &mut content_counters,
        report,
    )
    .await;

    if let Some(inventory) = first_inventory.as_ref() {
        match inventory {
            BackendInventory::Local(local) => {
                audit_local_inventory(
                    &config.objects_path(),
                    &prefix,
                    local,
                    database,
                    &verifications,
                    &mut content_counters,
                    report,
                )
                .await;
            }
            BackendInventory::Remote(_) => {}
        }
    }
    report.add_counters(CHECK_CONTENT, &content_counters);

    verify_inventory_stability(config, storage, first_inventory.as_ref(), report).await;
}

/// Audits upload/export scratch data independently from durable object
/// storage, allowing a storage configuration failure to leave transfer
/// coverage intact.
pub(crate) async fn run_transfer_scan(
    config: &Config,
    database: &DatabaseInventory,
    report: &mut ReportBuilder,
) {
    report.ensure_check(CHECK_WORKING);
    audit_transfer_working_tree(config, database, report).await;
}

#[derive(Debug)]
enum BackendInventory {
    Local(LocalTreeInventory),
    Remote(RemoteInventory),
}

#[derive(Debug)]
struct RemoteInventory {
    snapshot: RemoteInventorySnapshot,
    location_sizes: HashMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteInventorySnapshot {
    objects: u64,
    identity_sha256: String,
}

fn finding(
    report: &mut ReportBuilder,
    check: &str,
    code: &str,
    severity: Severity,
    entity: Option<String>,
    evidence: impl Into<String>,
    remediation: impl Into<String>,
) {
    report.finding(
        check,
        code,
        severity,
        entity,
        evidence,
        Some(remediation.into()),
    );
}

fn incomplete(
    report: &mut ReportBuilder,
    check: &str,
    code: &str,
    entity: Option<String>,
    evidence: impl Into<String>,
) {
    report.mark_incomplete(check, code, entity, evidence);
}

fn path_entity(path: &Path) -> String {
    format!("path:{}", bounded(path.to_string_lossy()))
}

fn object_entity(key: &str) -> String {
    format!("object:{}", bounded(key))
}

fn bounded(value: impl AsRef<str>) -> String {
    const MAX_CHARS: usize = 320;
    let value = value.as_ref();
    if value.chars().count() <= MAX_CHARS {
        value.to_string()
    } else {
        let mut result = value.chars().take(MAX_CHARS).collect::<String>();
        result.push('…');
        result
    }
}

async fn local_inventory_pass(
    root: &Path,
    report: &mut ReportBuilder,
) -> Option<LocalTreeInventory> {
    match inventory_local_tree(root).await {
        Ok(inventory) => {
            let files = inventory
                .entries
                .iter()
                .filter(|entry| entry.kind != LocalEntryKind::Directory)
                .count();
            report.record_files(CHECK_INVENTORY, files as u64);
            for (path, error) in &inventory.issues {
                incomplete(
                    report,
                    CHECK_INVENTORY,
                    "storage.local_entry_unreadable",
                    Some(path_entity(path)),
                    format!("could not inventory this local entry: {}", bounded(error)),
                );
            }
            Some(inventory)
        }
        Err(error) => {
            incomplete(
                report,
                CHECK_INVENTORY,
                "storage.local_root_unavailable",
                Some(path_entity(root)),
                format!("local storage root could not be inventoried: {error}"),
            );
            None
        }
    }
}

async fn remote_inventory_pass(
    storage: &dyn BlobStorageBackend,
    database: &DatabaseInventory,
    prefix: &str,
    counters: &mut ScanCounters,
    report: &mut ReportBuilder,
) -> Option<RemoteInventory> {
    let location_keys = database
        .locations
        .iter()
        .filter(|location| {
            location_belongs_to_inventory(storage, &location.backend, &location.bucket)
        })
        .map(|location| location.object_key.clone())
        .collect::<HashSet<_>>();
    let deferred_keys = database
        .locations
        .iter()
        .filter(|location| {
            location_is_serviceable(storage, &location.backend, &location.bucket)
                && !location.object_key.is_empty()
                && database.blobs.get(&location.blob_id).is_some_and(|blob| {
                    blob.size_bytes >= 0
                        && blob.hash_algo == "sha256"
                        && is_canonical_digest(&blob.hash)
                })
        })
        .map(|location| location.object_key.clone())
        .collect::<HashSet<_>>();
    let mut unseen_locations = location_keys.clone();
    let mut location_sizes = HashMap::new();
    let mut continuation_token = None;
    let mut seen_continuation_tokens = HashSet::new();
    let mut previous_key = None;
    let mut snapshot_hasher = Sha256::new();
    let mut object_count = 0_u64;

    loop {
        let page = match storage
            .inventory_object_page(continuation_token.as_deref())
            .await
        {
            Ok(page) => page,
            Err(error) => {
                remote_inventory_unavailable(report, error);
                return None;
            }
        };
        if page.entries.len() > MAX_REMOTE_INVENTORY_PAGE_ENTRIES {
            remote_inventory_unavailable(
                report,
                "remote inventory exceeded the bounded page-size contract",
            );
            return None;
        }
        report.record_objects(CHECK_INVENTORY, page.entries.len() as u64);
        for entry in page.entries {
            if !remote_inventory_entry_has_stable_identity(&entry) {
                remote_inventory_unavailable(
                    report,
                    "remote inventory entry lacked a coherent ETag or modification timestamp",
                );
                return None;
            }
            if previous_key
                .as_ref()
                .is_some_and(|previous: &String| previous >= &entry.object_key)
            {
                remote_inventory_unavailable(
                    report,
                    "remote inventory returned duplicate or out-of-order keys across pages",
                );
                return None;
            }
            previous_key = Some(entry.object_key.clone());
            update_remote_inventory_snapshot(&mut snapshot_hasher, &entry);
            object_count = object_count.saturating_add(1);
            unseen_locations.remove(&entry.object_key);
            if location_keys.contains(&entry.object_key) {
                location_sizes.insert(entry.object_key.clone(), entry.size_bytes);
            }

            let ManagedKeyKind::Direct { digest } = classify_managed_key(prefix, &entry.object_key)
            else {
                finding(
                    report,
                    CHECK_INVENTORY,
                    "storage.remote_key_noncanonical",
                    Severity::Warning,
                    Some(object_entity(&entry.object_key)),
                    "remote managed prefix contains a key outside the canonical SHA-256 object layout",
                    "Preserve the object until its ownership is known, then move or remove it through a reviewed process.",
                );
                continue;
            };
            if database.complete_for_storage && !location_keys.contains(&entry.object_key) {
                finding(
                    report,
                    CHECK_INVENTORY,
                    "storage.untracked_object",
                    Severity::Warning,
                    Some(object_entity(&entry.object_key)),
                    "canonical remote object has no database location row",
                    "Preserve the object until its ownership is established.",
                );
            }
            if deferred_keys.contains(&entry.object_key) {
                continue;
            }
            let verification = verify_streamed_copy(
                storage,
                storage.name(),
                storage.bucket(),
                &entry.object_key,
                entry.size_bytes,
                0,
            )
            .await;
            counters.objects = counters.objects.saturating_add(1);
            counters.bytes_hashed = counters
                .bytes_hashed
                .saturating_add(verification.bytes_read);
            if verification.error.is_some() {
                incomplete(
                    report,
                    CHECK_CONTENT,
                    "storage.remote_object_unreadable",
                    Some(object_entity(&entry.object_key)),
                    format!(
                        "digest-addressed remote object could not be read fully: {}",
                        bounded(
                            verification
                                .error
                                .as_ref()
                                .map_or("unknown error", |error| error.message.as_str())
                        )
                    ),
                );
            } else if verification.digest.as_deref() != Some(digest.as_str()) {
                finding(
                    report,
                    CHECK_CONTENT,
                    "storage.path_digest_mismatch",
                    Severity::Warning,
                    Some(object_entity(&entry.object_key)),
                    format!(
                        "path asserts SHA-256 {digest}, but bytes hash to {}",
                        verification.digest.as_deref().unwrap_or("unavailable")
                    ),
                    "Preserve the object for recovery analysis; do not attach it under the asserted digest.",
                );
            }
        }

        let Some(next) = page.continuation_token else {
            break;
        };
        if !seen_continuation_tokens.insert(next.clone()) {
            remote_inventory_unavailable(report, "remote inventory cycled a continuation token");
            return None;
        }
        continuation_token = Some(next);
    }

    for key in unseen_locations {
        let inside_prefix = prefix.is_empty() || key.starts_with(&format!("{prefix}/"));
        if inside_prefix {
            incomplete(
                report,
                CHECK_INVENTORY,
                "storage.remote_location_not_listed",
                Some(object_entity(&key)),
                "database location was not returned by the configured backend's inventory",
            );
        }
    }

    Some(RemoteInventory {
        snapshot: RemoteInventorySnapshot {
            objects: object_count,
            identity_sha256: lower_hex(&snapshot_hasher.finalize()),
        },
        location_sizes,
    })
}

fn remote_inventory_unavailable(report: &mut ReportBuilder, error: impl std::fmt::Display) {
    incomplete(
        report,
        CHECK_INVENTORY,
        "storage.remote_inventory_unavailable",
        None,
        format!("remote object inventory could not finish: {error}"),
    );
}

fn update_remote_inventory_snapshot(hasher: &mut Sha256, entry: &StorageObjectInventoryEntry) {
    update_snapshot_bytes(hasher, entry.object_key.as_bytes());
    hasher.update(entry.size_bytes.to_be_bytes());
    update_snapshot_option_bytes(hasher, entry.etag.as_deref().map(str::as_bytes));
    match entry.last_modified_secs {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    match entry.last_modified_subsec_nanos {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
        }
        None => hasher.update([0]),
    }
}

fn remote_inventory_entry_has_stable_identity(entry: &StorageObjectInventoryEntry) -> bool {
    let has_etag = entry
        .etag
        .as_deref()
        .is_some_and(|etag| !etag.trim().is_empty());
    let timestamp_is_coherent =
        entry.last_modified_secs.is_some() == entry.last_modified_subsec_nanos.is_some();
    timestamp_is_coherent && (has_etag || entry.last_modified_secs.is_some())
}

fn update_snapshot_bytes(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn update_snapshot_option_bytes(hasher: &mut Sha256, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            hasher.update([1]);
            update_snapshot_bytes(hasher, value);
        }
        None => hasher.update([0]),
    }
}

async fn inventory_local_tree(root: &Path) -> io::Result<LocalTreeInventory> {
    let root_metadata = fs::symlink_metadata(root).await?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage root is not a real directory",
        ));
    }

    let mut result = LocalTreeInventory::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = match fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) => {
                result.issues.push((directory, error.to_string()));
                continue;
            }
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    result.issues.push((directory.clone(), error.to_string()));
                    break;
                }
            };
            let path = entry.path();
            let relative = match path.strip_prefix(root) {
                Ok(relative) => relative.to_path_buf(),
                Err(error) => {
                    result.issues.push((path, error.to_string()));
                    continue;
                }
            };
            let metadata = match fs::symlink_metadata(&path).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    result.issues.push((path, error.to_string()));
                    continue;
                }
            };
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                LocalEntryKind::Symlink
            } else if file_type.is_dir() {
                LocalEntryKind::Directory
            } else if file_type.is_file() {
                LocalEntryKind::File
            } else {
                LocalEntryKind::Special
            };
            if kind == LocalEntryKind::Directory {
                pending.push(path.clone());
            }
            if result.entries.len() == MAX_LOCAL_INVENTORY_ENTRIES {
                return Err(io::Error::other(
                    "local inventory exceeds the 1,000,000-entry safety limit",
                ));
            }
            result.entries.push(LocalEntry {
                path,
                relative,
                kind,
                identity: Some(file_identity(&metadata)),
            });
        }
    }
    result
        .entries
        .sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(result)
}

fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        FileIdentity {
            length: metadata.len(),
            modified_nanos,
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        FileIdentity {
            length: metadata.len(),
            modified_nanos,
        }
    }
}

fn inventory_snapshot(
    inventory: &LocalTreeInventory,
) -> BTreeMap<PathBuf, (LocalEntryKind, Option<FileIdentity>)> {
    inventory
        .entries
        .iter()
        .map(|entry| (entry.relative.clone(), (entry.kind, entry.identity.clone())))
        .collect()
}

async fn verify_database_locations(
    config: &Config,
    storage: &dyn BlobStorageBackend,
    database: &DatabaseInventory,
    backend_inventory: Option<&BackendInventory>,
    counters: &mut ScanCounters,
    report: &mut ReportBuilder,
) -> Vec<CopyVerification> {
    report.record_rows(CHECK_LOCATIONS, database.locations.len() as u64);
    let prefix = normalize_storage_prefix(&config.storage_prefix);
    let remote_sizes = backend_inventory.and_then(|inventory| match inventory {
        BackendInventory::Remote(inventory) => Some(&inventory.location_sizes),
        BackendInventory::Local(_) => None,
    });
    let locations_by_blob = database.locations.iter().fold(
        BTreeMap::<i64, Vec<_>>::new(),
        |mut locations, location| {
            locations
                .entry(location.blob_id)
                .or_default()
                .push(location);
            locations
        },
    );

    let mut verifications = Vec::new();
    let mut verified_physical_copies = HashMap::<CopyKey, CopyVerification>::new();
    for location in &database.locations {
        let Some(blob) = database.blobs.get(&location.blob_id) else {
            finding(
                report,
                CHECK_LOCATIONS,
                "storage.location_blob_missing",
                Severity::Error,
                Some(format!("blob_location:{}", location.id)),
                format!("location refers to missing blob {}", location.blob_id),
                "Restore the blob metadata row or remove the dangling location after taking a backup.",
            );
            continue;
        };
        if location.object_key.is_empty() {
            finding(
                report,
                CHECK_LOCATIONS,
                "storage.location_key_empty",
                Severity::Error,
                Some(format!("blob_location:{}", location.id)),
                "location has an empty object key",
                "Restore a valid content-addressed location or remove this unusable location row.",
            );
            continue;
        }
        if !canonical_location_key(
            location.backend.as_str(),
            &prefix,
            blob,
            &location.object_key,
        ) {
            finding(
                report,
                CHECK_LOCATIONS,
                "storage.location_key_noncanonical",
                Severity::Warning,
                Some(format!("blob_location:{}", location.id)),
                format!(
                    "object key {} does not match the canonical {} layout for blob {}",
                    bounded(&location.object_key),
                    bounded(&location.backend),
                    blob.id
                ),
                "Preserve the bytes, then republish the blob through Vault so its location uses the canonical content-addressed key.",
            );
        }
        if !location_is_serviceable(storage, &location.backend, &location.bucket) {
            finding(
                report,
                CHECK_LOCATIONS,
                "storage.location_not_serviceable",
                Severity::Info,
                Some(format!("blob_location:{}", location.id)),
                format!(
                    "configured backend {}/{} cannot serve location {}/{}",
                    storage.name(),
                    bounded(storage.bucket()),
                    bounded(&location.backend),
                    bounded(&location.bucket)
                ),
                "Retain this replica only if another configured backend is expected to serve it.",
            );
            continue;
        }
        if blob.size_bytes < 0 || blob.hash_algo != "sha256" || !is_canonical_digest(&blob.hash) {
            finding(
                report,
                CHECK_CONTENT,
                "storage.copy_metadata_unusable",
                Severity::Error,
                Some(format!("blob:{}", blob.id)),
                "copy could not be content-verified because its blob size or digest metadata is invalid",
                "Restore trustworthy blob metadata before deciding whether the stored copy can be retained.",
            );
            continue;
        }

        let copy_key = CopyKey {
            backend: location.backend.clone(),
            bucket: location.bucket.clone(),
            object_key: location.object_key.clone(),
        };
        let (verification, newly_verified) =
            if let Some(cached) = verified_physical_copies.get(&copy_key) {
                let mut cached = cached.clone();
                cached.blob_id = blob.id;
                (cached, false)
            } else {
                let database_size = u64::try_from(blob.size_bytes).unwrap_or_default();
                let verification = if storage.name() == "local" {
                    verify_local_copy(
                        &config.objects_path(),
                        &prefix,
                        &location.backend,
                        &location.bucket,
                        &location.object_key,
                        blob.id,
                    )
                    .await
                } else {
                    let stream_size = remote_sizes
                        .and_then(|sizes| sizes.get(location.object_key.as_str()).copied())
                        .unwrap_or(database_size);
                    verify_streamed_copy(
                        storage,
                        &location.backend,
                        &location.bucket,
                        &location.object_key,
                        stream_size,
                        blob.id,
                    )
                    .await
                };
                verified_physical_copies.insert(copy_key, verification.clone());
                (verification, true)
            };
        if newly_verified {
            counters.objects = counters.objects.saturating_add(1);
            counters.bytes_hashed = counters
                .bytes_hashed
                .saturating_add(verification.bytes_read);
            if verification.indeterminate {
                incomplete(
                    report,
                    CHECK_CONTENT,
                    "storage.copy_changed_during_read",
                    Some(object_entity(&verification.key.object_key)),
                    "the physical copy changed while it was being hashed",
                );
            }
        }
        verifications.push(verification);
    }

    for (blob_id, references) in &database.live_references {
        let Some(blob) = database.blobs.get(blob_id) else {
            continue;
        };
        let serviceable_locations = locations_by_blob
            .get(blob_id)
            .into_iter()
            .flatten()
            .filter(|location| {
                location_is_serviceable(storage, &location.backend, &location.bucket)
            })
            .count();
        if serviceable_locations == 0 {
            finding(
                report,
                CHECK_LOCATIONS,
                "storage.live_blob_unserviceable",
                live_blob_failure_severity(references),
                Some(format!("blob:{blob_id}")),
                format!(
                    "live blob has no location serviceable through configured backend {}/{}",
                    storage.name(),
                    bounded(storage.bucket())
                ),
                "Restore or republish a verified copy into the configured backend before serving this content.",
            );
            continue;
        }

        let outcomes = verifications
            .iter()
            .filter(|verification| verification.blob_id == *blob_id)
            .collect::<Vec<_>>();
        let good_copies = outcomes
            .iter()
            .filter(|verification| copy_matches_blob(verification, blob))
            .count();
        for verification in outcomes
            .iter()
            .filter(|verification| !copy_matches_blob(verification, blob))
        {
            let severity = if good_copies > 0 {
                Severity::Warning
            } else {
                live_blob_failure_severity(references)
            };
            report_copy_failure(report, blob, verification, severity);
        }
        if good_copies == 0 && outcomes.is_empty() {
            finding(
                report,
                CHECK_CONTENT,
                "storage.live_blob_unverified",
                live_blob_failure_severity(references),
                Some(format!("blob:{blob_id}")),
                "no serviceable copy could be content-verified",
                "Correct invalid blob metadata or storage access, then run the complete integrity check again.",
            );
        }
    }

    for verification in &verifications {
        if database.live_references.contains_key(&verification.blob_id) {
            continue;
        }
        let Some(blob) = database.blobs.get(&verification.blob_id) else {
            continue;
        };
        if !copy_matches_blob(verification, blob) {
            report_copy_failure(report, blob, verification, Severity::Warning);
        }
    }
    verifications
}

async fn verify_preview_rendition_payloads(
    config: &Config,
    storage: &dyn BlobStorageBackend,
    database: &DatabaseInventory,
    verifications: &[CopyVerification],
    counters: &mut ScanCounters,
    report: &mut ReportBuilder,
) {
    let raster_job_ids = database
        .preview_jobs
        .iter()
        .filter(|job| matches!(job.recipe.as_str(), "raster-v1" | "raster-v2"))
        .map(|job| job.id)
        .collect::<HashSet<_>>();
    let mut renditions_by_blob = BTreeMap::<i64, Vec<_>>::new();
    for rendition in database.preview_renditions.iter().filter(|rendition| {
        raster_job_ids.contains(&rendition.preview_job_id) && rendition.mime_type == "image/webp"
    }) {
        renditions_by_blob
            .entry(rendition.blob_id)
            .or_default()
            .push(rendition);
    }
    let mut verified_by_blob = HashMap::new();
    for verification in verifications {
        if let Some(blob) = database.blobs.get(&verification.blob_id)
            && copy_matches_blob(verification, blob)
        {
            verified_by_blob
                .entry(verification.blob_id)
                .or_insert(verification);
        }
    }

    for (blob_id, renditions) in renditions_by_blob {
        let Some(blob) = database.blobs.get(&blob_id) else {
            continue;
        };
        let Ok(expected_size) = u64::try_from(blob.size_bytes) else {
            continue;
        };
        if expected_size > MAX_PREVIEW_PAYLOAD_BYTES {
            finding(
                report,
                CHECK_CONTENT,
                "preview.payload_oversized",
                Severity::Warning,
                Some(format!("blob:{blob_id}")),
                format!(
                    "preview rendition payload is {expected_size} bytes, exceeding the 16 MiB output limit"
                ),
                "Discard and regenerate the derived preview from its verified source blob.",
            );
            continue;
        }
        let Some(verification) = verified_by_blob.get(&blob_id).copied() else {
            // The ordinary stored-copy findings already describe why no bytes
            // are trustworthy enough to decode.
            continue;
        };
        let payload = match read_preview_payload(config, storage, &verification.key, expected_size)
            .await
        {
            Ok(payload) => payload,
            Err(error) => {
                incomplete(
                    report,
                    CHECK_CONTENT,
                    "preview.payload_decode_read_incomplete",
                    Some(object_entity(&verification.key.object_key)),
                    format!(
                        "verified preview payload could not be reread safely for format validation: {}",
                        bounded(&error)
                    ),
                );
                continue;
            }
        };
        counters.objects = counters.objects.saturating_add(1);
        counters.bytes_hashed = counters.bytes_hashed.saturating_add(payload.len() as u64);
        if lower_hex(&Sha256::digest(&payload)) != blob.hash {
            incomplete(
                report,
                CHECK_CONTENT,
                "preview.payload_changed_before_decode",
                Some(object_entity(&verification.key.object_key)),
                "preview bytes changed between content verification and format decoding",
            );
            continue;
        }
        let decoded = tokio::task::spawn_blocking(move || decode_preview_webp(&payload)).await;
        let (actual_width, actual_height) = match decoded {
            Ok(Ok(dimensions)) => dimensions,
            Ok(Err(error)) => {
                finding(
                    report,
                    CHECK_CONTENT,
                    "preview.payload_invalid_webp",
                    Severity::Warning,
                    Some(format!("blob:{blob_id}")),
                    format!("preview payload is not a bounded decodable WebP: {error}"),
                    "Discard and regenerate the derived preview from its verified source blob.",
                );
                continue;
            }
            Err(error) => {
                incomplete(
                    report,
                    CHECK_CONTENT,
                    "preview.payload_decoder_failed",
                    Some(format!("blob:{blob_id}")),
                    format!("preview decoder task did not finish: {error}"),
                );
                continue;
            }
        };
        for rendition in renditions {
            if u32::try_from(rendition.width).ok() != Some(actual_width)
                || u32::try_from(rendition.height).ok() != Some(actual_height)
            {
                finding(
                    report,
                    CHECK_CONTENT,
                    "preview.payload_dimensions_mismatch",
                    Severity::Warning,
                    Some(format!("preview_rendition:{}", rendition.id)),
                    format!(
                        "rendition {} for preview job {} declares {}x{} but its WebP is {actual_width}x{actual_height}",
                        bounded(&rendition.variant),
                        rendition.preview_job_id,
                        rendition.width,
                        rendition.height
                    ),
                    "Discard and regenerate the derived preview from its verified source blob.",
                );
            }
        }
    }
}

async fn read_preview_payload(
    config: &Config,
    storage: &dyn BlobStorageBackend,
    key: &CopyKey,
    expected_size: u64,
) -> Result<Vec<u8>, String> {
    let expected_capacity = usize::try_from(expected_size)
        .map_err(|_| "preview payload size is not addressable".to_string())?;
    let max_payload_bytes = usize::try_from(MAX_PREVIEW_PAYLOAD_BYTES)
        .map_err(|_| "preview payload limit is not addressable".to_string())?;
    if storage.name() == "local" {
        let ManagedKeyKind::Direct { .. } = classify_managed_key(
            &normalize_storage_prefix(&config.storage_prefix),
            &key.object_key,
        ) else {
            return Err(
                "preview rendition uses a multipart or noncanonical local layout".to_string(),
            );
        };
        let path = local_object_path(&config.objects_path(), &key.object_key)?;
        let before = safe_regular_file_metadata(&config.objects_path(), &path)
            .await
            .map_err(|error| error.to_string())?;
        if before.len() != expected_size || before.len() > MAX_PREVIEW_PAYLOAD_BYTES {
            return Err("preview payload size changed before decoding".to_string());
        }
        let source = fs::File::open(&path)
            .await
            .map_err(|error| error.to_string())?;
        let mut payload = Vec::with_capacity(expected_capacity);
        source
            .take(MAX_PREVIEW_PAYLOAD_BYTES + 1)
            .read_to_end(&mut payload)
            .await
            .map_err(|error| error.to_string())?;
        let after = safe_regular_file_metadata(&config.objects_path(), &path)
            .await
            .map_err(|error| error.to_string())?;
        if payload.len() as u64 != expected_size || file_identity(&before) != file_identity(&after)
        {
            return Err("preview payload changed while it was read for decoding".to_string());
        }
        return Ok(payload);
    }

    let range = BlobReadRange {
        expected_size,
        offset: 0,
        length: expected_size,
    };
    let mut stream = storage
        .stream_location_range(&key.backend, &key.bucket, &key.object_key, range)
        .await
        .map_err(|error| error.to_string())?;
    let mut payload = Vec::with_capacity(expected_capacity);
    while let Some(frame) = stream.next().await {
        let frame = frame.map_err(|error| error.to_string())?;
        if frame.is_empty()
            || frame.len() > STORAGE_CHUNK_SIZE
            || payload.len().saturating_add(frame.len()) > max_payload_bytes
        {
            return Err("preview stream violated its bounded frame or size contract".to_string());
        }
        payload.extend_from_slice(&frame);
    }
    if payload.len() as u64 != expected_size {
        return Err("preview stream ended at an unexpected byte count".to_string());
    }
    Ok(payload)
}

fn decode_preview_webp(payload: &[u8]) -> Result<(u32, u32), String> {
    let mut reader = ImageReader::with_format(Cursor::new(payload), ImageFormat::WebP);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_PREVIEW_DIMENSION);
    limits.max_image_height = Some(MAX_PREVIEW_DIMENSION);
    limits.max_alloc = Some(MAX_PREVIEW_DECODED_BYTES);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|error| bounded(error.to_string()))?;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > MAX_PREVIEW_DIMENSION
        || height > MAX_PREVIEW_DIMENSION
        || decoder.total_bytes() > MAX_PREVIEW_DECODED_BYTES
    {
        return Err("decoded dimensions or allocation exceed preview limits".to_string());
    }
    let image = DynamicImage::from_decoder(decoder).map_err(|error| bounded(error.to_string()))?;
    Ok((image.width(), image.height()))
}

fn location_is_serviceable(storage: &dyn BlobStorageBackend, backend: &str, bucket: &str) -> bool {
    backend == storage.name()
        && (bucket == storage.bucket() || (!storage.bucket().is_empty() && bucket.is_empty()))
}

fn location_belongs_to_inventory(
    storage: &dyn BlobStorageBackend,
    backend: &str,
    bucket: &str,
) -> bool {
    let backend = lifecycle_underlying_backend(backend).unwrap_or(backend);
    backend == storage.name()
        && (bucket == storage.bucket() || (!storage.bucket().is_empty() && bucket.is_empty()))
}

fn lifecycle_underlying_backend(backend: &str) -> Option<&str> {
    let remainder = backend
        .strip_prefix("_vault_pending:")
        .or_else(|| backend.strip_prefix("_vault_deleting:"))?;
    let (token, underlying) = remainder.split_once(':')?;
    (!token.is_empty() && !underlying.is_empty()).then_some(underlying)
}

fn canonical_location_key(backend: &str, prefix: &str, blob: &BlobRecord, key: &str) -> bool {
    if blob.hash_algo != "sha256" || !is_canonical_digest(&blob.hash) {
        return false;
    }
    if key == object_key_for_hash(prefix, &blob.hash_algo, &blob.hash) {
        return true;
    }
    lifecycle_underlying_backend(backend).unwrap_or(backend) == "local"
        && key == multipart_manifest_key_for_hash(prefix, &blob.hash_algo, &blob.hash)
}

fn live_blob_failure_severity(references: &BTreeSet<LiveReferenceKind>) -> Severity {
    if references
        .iter()
        .all(|reference| *reference == LiveReferenceKind::PreviewRendition)
    {
        Severity::Warning
    } else {
        Severity::Error
    }
}

fn copy_matches_blob(verification: &CopyVerification, blob: &BlobRecord) -> bool {
    verification.error.is_none()
        && u64::try_from(blob.size_bytes).ok() == Some(verification.bytes_read)
        && verification.digest.as_deref() == Some(blob.hash.as_str())
}

fn report_copy_failure(
    report: &mut ReportBuilder,
    blob: &BlobRecord,
    verification: &CopyVerification,
    severity: Severity,
) {
    if verification.indeterminate {
        return;
    }
    let (code, evidence) = if let Some(error) = &verification.error {
        if error.kind == CopyFailureKind::Unavailable {
            incomplete(
                report,
                CHECK_CONTENT,
                "storage.copy_read_incomplete",
                Some(object_entity(&verification.key.object_key)),
                format!(
                    "blob {} copy could not be fully inspected: {}",
                    blob.id,
                    bounded(&error.message)
                ),
            );
            return;
        }
        (
            if error.kind == CopyFailureKind::Missing {
                "storage.copy_missing"
            } else {
                "storage.copy_unreadable"
            },
            format!(
                "blob {} copy could not be read completely: {}",
                blob.id,
                bounded(&error.message)
            ),
        )
    } else if u64::try_from(blob.size_bytes).ok() != Some(verification.bytes_read) {
        (
            "storage.copy_size_mismatch",
            format!(
                "blob {} expects {} bytes but copy contains {} bytes",
                blob.id, blob.size_bytes, verification.bytes_read
            ),
        )
    } else {
        (
            "storage.copy_digest_mismatch",
            format!(
                "blob {} expects SHA-256 {} but the copy hashes to {}",
                blob.id,
                blob.hash,
                verification.digest.as_deref().unwrap_or("unavailable")
            ),
        )
    };
    finding(
        report,
        CHECK_CONTENT,
        code,
        severity,
        Some(object_entity(&verification.key.object_key)),
        evidence,
        "Restore this physical copy from a verified replica or backup; do not overwrite it before preserving evidence.",
    );
}

fn copy_failure_from_storage(error: &StorageError) -> CopyFailure {
    let kind = match error {
        StorageError::NotFound => CopyFailureKind::Missing,
        StorageError::InvalidObjectKey
        | StorageError::InvalidRange
        | StorageError::ChecksumMismatch
        | StorageError::ContentMismatch
        | StorageError::ConflictingMultipartPart
        | StorageError::InvalidMultipartManifest
        | StorageError::UnreadableMultipartManifest
        | StorageError::InvalidStoragePath
        | StorageError::Json(_) => CopyFailureKind::Invalid,
        StorageError::SourceSizeChanged
        | StorageError::Configuration(_)
        | StorageError::BackendMismatch
        | StorageError::Busy
        | StorageError::UnsupportedOperation(_)
        | StorageError::Remote(_)
        | StorageError::Io(_) => CopyFailureKind::Unavailable,
    };
    CopyFailure {
        kind,
        message: error.to_string(),
    }
}

fn copy_failure_from_local_message(message: String) -> CopyFailure {
    let normalized = message.to_ascii_lowercase();
    let kind = if normalized.contains("no such file")
        || normalized.contains("not found")
        || normalized.contains("cannot find the file")
    {
        CopyFailureKind::Missing
    } else if normalized.contains("permission denied")
        || normalized.contains("access is denied")
        || normalized.contains("timed out")
        || normalized.contains("i/o error")
        || normalized.contains("input/output error")
        || normalized.contains("changed or disappeared")
    {
        CopyFailureKind::Unavailable
    } else {
        CopyFailureKind::Invalid
    };
    CopyFailure { kind, message }
}

async fn verify_streamed_copy(
    storage: &dyn BlobStorageBackend,
    backend: &str,
    bucket: &str,
    object_key: &str,
    expected_stream_size: u64,
    blob_id: i64,
) -> CopyVerification {
    let key = CopyKey {
        backend: backend.to_string(),
        bucket: bucket.to_string(),
        object_key: object_key.to_string(),
    };
    let range = BlobReadRange {
        expected_size: expected_stream_size,
        offset: 0,
        length: expected_stream_size,
    };
    let mut stream = match storage
        .stream_location_range(backend, bucket, object_key, range)
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            return CopyVerification {
                key,
                blob_id,
                bytes_read: 0,
                digest: None,
                error: Some(copy_failure_from_storage(&error)),
                indeterminate: false,
            };
        }
    };
    let mut hasher = Sha256::new();
    let mut bytes_read = 0_u64;
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(frame) if !frame.is_empty() && frame.len() <= STORAGE_CHUNK_SIZE => {
                hasher.update(&frame);
                bytes_read = bytes_read.saturating_add(frame.len() as u64);
            }
            Ok(_) => {
                return CopyVerification {
                    key,
                    blob_id,
                    bytes_read,
                    digest: None,
                    error: Some(CopyFailure {
                        kind: CopyFailureKind::Unavailable,
                        message: "storage stream returned an invalid frame".to_string(),
                    }),
                    indeterminate: false,
                };
            }
            Err(error) => {
                return CopyVerification {
                    key,
                    blob_id,
                    bytes_read,
                    digest: None,
                    error: Some(copy_failure_from_storage(&error)),
                    indeterminate: false,
                };
            }
        }
    }
    CopyVerification {
        key,
        blob_id,
        bytes_read,
        digest: Some(lower_hex(&hasher.finalize())),
        error: (bytes_read != expected_stream_size).then(|| CopyFailure {
            kind: CopyFailureKind::Invalid,
            message: "storage stream ended at an unexpected byte count".to_string(),
        }),
        indeterminate: false,
    }
}

async fn verify_local_copy(
    root: &Path,
    prefix: &str,
    backend: &str,
    bucket: &str,
    object_key: &str,
    blob_id: i64,
) -> CopyVerification {
    let key = CopyKey {
        backend: backend.to_string(),
        bucket: bucket.to_string(),
        object_key: object_key.to_string(),
    };
    let outcome = if let ManagedKeyKind::Manifest { .. } = classify_managed_key(prefix, object_key)
    {
        hash_local_multipart(root, prefix, object_key).await
    } else {
        let path = local_object_path(root, object_key);
        match path {
            Ok(path) => {
                let mut hasher = Sha256::new();
                hash_local_file_into(root, &path, &mut hasher)
                    .await
                    .map(|(bytes, changed)| (lower_hex(&hasher.finalize()), bytes, changed))
            }
            Err(error) => Err(error),
        }
    };
    match outcome {
        Ok((digest, bytes_read, changed)) => CopyVerification {
            key,
            blob_id,
            bytes_read,
            digest: Some(digest),
            error: changed.then(|| CopyFailure {
                kind: CopyFailureKind::Unavailable,
                message: "local object identity changed during hashing".to_string(),
            }),
            indeterminate: changed,
        },
        Err(error) => CopyVerification {
            key,
            blob_id,
            bytes_read: 0,
            digest: None,
            error: Some(copy_failure_from_local_message(error)),
            indeterminate: false,
        },
    }
}

async fn hash_local_multipart(
    root: &Path,
    prefix: &str,
    manifest_key: &str,
) -> Result<(String, u64, bool), String> {
    let manifest_path = local_object_path(root, manifest_key)?;
    let manifest_before = safe_regular_file_metadata(root, &manifest_path)
        .await
        .map_err(|error| format!("multipart manifest is unavailable: {error}"))?;
    if manifest_before.len() > MAX_MULTIPART_MANIFEST_BYTES {
        return Err("multipart manifest exceeds the 512 KiB format limit".to_string());
    }
    let source = fs::File::open(&manifest_path)
        .await
        .map_err(|error| format!("multipart manifest could not be opened: {error}"))?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(manifest_before.len())
            .map_err(|_| "multipart manifest size is not addressable".to_string())?,
    );
    source
        .take(MAX_MULTIPART_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("multipart manifest could not be read: {error}"))?;
    if bytes.len() as u64 > MAX_MULTIPART_MANIFEST_BYTES {
        return Err("multipart manifest exceeds the 512 KiB format limit".to_string());
    }
    let payload = serde_json::from_slice::<MultipartManifestPayload>(&bytes)
        .map_err(|error| format!("multipart manifest JSON is malformed: {error}"))?;
    validate_manifest_payload(prefix, manifest_key, &payload)?;

    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut changed = false;
    for part in &payload.parts {
        let path = local_object_path(root, &part.object_key)?;
        let (size, part_changed) = hash_local_file_into(root, &path, &mut hasher).await?;
        changed |= part_changed;
        if size != part.size_bytes {
            return Err(format!(
                "multipart part {} declares {} bytes but contains {size}",
                bounded(&part.object_key),
                part.size_bytes
            ));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| "multipart part sizes overflow u64".to_string())?;
    }
    if total != payload.size_bytes {
        return Err(format!(
            "multipart manifest declares {} bytes but its parts contain {total}",
            payload.size_bytes
        ));
    }
    let manifest_after = safe_regular_file_metadata(root, &manifest_path)
        .await
        .map_err(|error| format!("multipart manifest changed or disappeared: {error}"))?;
    changed |= file_identity(&manifest_before) != file_identity(&manifest_after);
    Ok((lower_hex(&hasher.finalize()), total, changed))
}

fn validate_manifest_payload(
    prefix: &str,
    manifest_key: &str,
    payload: &MultipartManifestPayload,
) -> Result<(), String> {
    if payload.format != LOCAL_MULTIPART_FORMAT {
        return Err(format!(
            "unsupported multipart manifest format {}",
            bounded(&payload.format)
        ));
    }
    if payload.hash_algo != "sha256" || !is_canonical_digest(&payload.digest) {
        return Err("multipart manifest digest metadata is invalid".to_string());
    }
    if multipart_manifest_key_for_hash(prefix, &payload.hash_algo, &payload.digest) != manifest_key
    {
        return Err("multipart manifest path does not match its digest".to_string());
    }
    if payload.parts.len() > STORAGE_MULTIPART_MAX_PARTS {
        return Err(format!(
            "multipart manifest contains more than {STORAGE_MULTIPART_MAX_PARTS} parts"
        ));
    }
    if (payload.size_bytes == 0 && !payload.parts.is_empty())
        || (payload.size_bytes > 0 && payload.parts.is_empty())
    {
        return Err("multipart manifest part count does not match its content size".to_string());
    }
    if payload.parts.iter().any(|part| part.size_bytes == 0) {
        return Err("multipart manifest contains a zero-sized part".to_string());
    }
    let part_sizes = payload
        .parts
        .iter()
        .map(|part| part.size_bytes)
        .collect::<Vec<_>>();
    let layout_id = multipart_layout_id(&part_sizes);
    let current_layout = payload.parts.first().is_some_and(|part| {
        part.object_key
            == multipart_part_key_for_hash_layout(
                prefix,
                &payload.hash_algo,
                &payload.digest,
                &layout_id,
                1,
            )
    });
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    for (index, part) in payload.parts.iter().enumerate() {
        let part_number = index + 1;
        let expected = if current_layout {
            multipart_part_key_for_hash_layout(
                prefix,
                &payload.hash_algo,
                &payload.digest,
                &layout_id,
                part_number,
            )
        } else {
            multipart_part_key_for_hash(prefix, &payload.hash_algo, &payload.digest, part_number)
        };
        if part.object_key != expected {
            return Err(format!(
                "multipart part {part_number} uses noncanonical or mixed-layout key {}",
                bounded(&part.object_key)
            ));
        }
        if !seen.insert(part.object_key.as_str()) {
            return Err("multipart manifest references the same part more than once".to_string());
        }
        total = total
            .checked_add(part.size_bytes)
            .ok_or_else(|| "multipart part sizes overflow u64".to_string())?;
    }
    if total != payload.size_bytes {
        return Err("multipart manifest size does not equal the sum of part sizes".to_string());
    }
    Ok(())
}

fn local_object_path(root: &Path, object_key: &str) -> Result<PathBuf, String> {
    let cleaned = object_key.trim().trim_start_matches('/').replace('\\', "/");
    if cleaned.is_empty() {
        return Err("object key is empty".to_string());
    }
    let mut path = root.to_path_buf();
    for component in Path::new(&cleaned).components() {
        match component {
            Component::Normal(component) => path.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("object key escapes the configured storage root".to_string());
            }
        }
    }
    if path == root {
        return Err("object key does not identify a file".to_string());
    }
    Ok(path)
}

async fn safe_regular_file_metadata(root: &Path, path: &Path) -> io::Result<std::fs::Metadata> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path escapes storage root"))?;
    let root_metadata = fs::symlink_metadata(root).await?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage root is not a real directory",
        ));
    }
    let components = relative.components().collect::<Vec<_>>();
    let mut cursor = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains an unsafe component",
            ));
        };
        cursor.push(component);
        let metadata = fs::symlink_metadata(&cursor).await?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path traverses a symlink",
            ));
        }
        if index + 1 == components.len() {
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "object path is not a regular file",
                ));
            }
            return Ok(metadata);
        }
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "object path traverses a non-directory",
            ));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "object path is empty",
    ))
}

async fn hash_local_file_into(
    root: &Path,
    path: &Path,
    hasher: &mut Sha256,
) -> Result<(u64, bool), String> {
    let before = safe_regular_file_metadata(root, path)
        .await
        .map_err(|error| error.to_string())?;
    let mut source = fs::File::open(path)
        .await
        .map_err(|error| format!("file could not be opened: {error}"))?;
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; STORAGE_CHUNK_SIZE];
    loop {
        let read = source
            .read(&mut buffer)
            .await
            .map_err(|error| format!("file could not be read: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| "file size overflowed u64".to_string())?;
    }
    let after = safe_regular_file_metadata(root, path)
        .await
        .map_err(|error| format!("file changed or disappeared: {error}"))?;
    Ok((size, file_identity(&before) != file_identity(&after)))
}

fn multipart_layout_id(part_sizes: &[u64]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((part_sizes.len() as u64).to_be_bytes());
    for size in part_sizes {
        hasher.update(size.to_be_bytes());
    }
    lower_hex(&hasher.finalize())
}

fn is_canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

fn classify_managed_key(prefix: &str, key: &str) -> ManagedKeyKind {
    if key.trim() != key || key.starts_with('/') || key.contains('\\') || key.contains("//") {
        return ManagedKeyKind::Malformed;
    }
    let relative = if prefix.is_empty() {
        key
    } else if let Some(relative) = key.strip_prefix(&format!("{prefix}/")) {
        relative
    } else {
        return ManagedKeyKind::OutsidePrefix;
    };
    let parts = relative.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["sha256", digest] if is_canonical_digest(digest) => ManagedKeyKind::Direct {
            digest: (*digest).to_string(),
        },
        ["multipart", "sha256", digest, "manifest.json"] if is_canonical_digest(digest) => {
            ManagedKeyKind::Manifest {
                digest: (*digest).to_string(),
            }
        }
        ["multipart", "sha256", digest, "parts", part] if is_canonical_digest(digest) => {
            parse_part_number(part).map_or(ManagedKeyKind::Malformed, |part_number| {
                ManagedKeyKind::LegacyPart {
                    digest: (*digest).to_string(),
                    part_number,
                }
            })
        }
        ["multipart", "sha256", digest, "parts", layout_id, part]
            if is_canonical_digest(digest) && is_canonical_digest(layout_id) =>
        {
            parse_part_number(part).map_or(ManagedKeyKind::Malformed, |part_number| {
                ManagedKeyKind::LayoutPart {
                    digest: (*digest).to_string(),
                    layout_id: (*layout_id).to_string(),
                    part_number,
                }
            })
        }
        _ => ManagedKeyKind::Malformed,
    }
}

fn parse_part_number(value: &str) -> Option<usize> {
    let stem = value.strip_suffix(".part")?;
    (stem.len() == 8 && stem.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| stem.parse::<usize>().ok())
        .flatten()
        .filter(|part| (1..=STORAGE_MULTIPART_MAX_PARTS).contains(part))
}

fn recognized_managed_directory(prefix: &str, key: &str) -> bool {
    if key == ".vault-staging" || key.starts_with(".vault-staging/") {
        return true;
    }
    let key_parts = key.split('/').collect::<Vec<_>>();
    let prefix_parts = prefix
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    if key_parts.len() <= prefix_parts.len() {
        return prefix_parts.starts_with(&key_parts);
    }
    if !key_parts.starts_with(&prefix_parts) {
        return false;
    }
    let relative = &key_parts[prefix_parts.len()..];
    matches!(relative, ["sha256" | "multipart"] | ["multipart", "sha256"])
        || matches!(relative, ["multipart", "sha256", digest] if is_canonical_digest(digest))
        || matches!(relative, ["multipart", "sha256", digest, "parts"] if is_canonical_digest(digest))
        || matches!(relative, ["multipart", "sha256", digest, "parts", layout]
            if is_canonical_digest(digest) && is_canonical_digest(layout))
}

async fn audit_local_inventory(
    root: &Path,
    prefix: &str,
    inventory: &LocalTreeInventory,
    database: &DatabaseInventory,
    verifications: &[CopyVerification],
    counters: &mut ScanCounters,
    report: &mut ReportBuilder,
) {
    let location_keys = database
        .locations
        .iter()
        .filter(|location| {
            lifecycle_underlying_backend(&location.backend).unwrap_or(&location.backend) == "local"
                && location.bucket.is_empty()
        })
        .map(|location| location.object_key.as_str())
        .collect::<HashSet<_>>();
    let attempted_keys = verifications
        .iter()
        .filter(|verification| verification.key.backend == "local")
        .map(|verification| verification.key.object_key.as_str())
        .collect::<HashSet<_>>();
    let mut manifest_part_keys = HashSet::new();
    let mut manifest_digests = HashSet::new();
    let mut part_groups = BTreeMap::<(String, Option<String>), Vec<(usize, &LocalEntry)>>::new();
    let mut object_count = 0_u64;

    for entry in &inventory.entries {
        if matches!(
            entry.kind,
            LocalEntryKind::Symlink | LocalEntryKind::Special
        ) {
            finding(
                report,
                CHECK_INVENTORY,
                if entry.kind == LocalEntryKind::Symlink {
                    "storage.local_symlink"
                } else {
                    "storage.local_special_file"
                },
                Severity::Error,
                Some(path_entity(&entry.path)),
                "local storage contains an entry type Vault must not traverse or serve",
                "Move the unexpected entry out of the Vault tree after preserving it for investigation.",
            );
            continue;
        }
        let Some(key) = path_to_key(&entry.relative) else {
            finding(
                report,
                CHECK_INVENTORY,
                "storage.local_name_not_utf8",
                Severity::Warning,
                Some(path_entity(&entry.path)),
                "local storage contains a filename that cannot be represented as an object key",
                "Preserve and remove the unexpected entry after confirming it is not application data.",
            );
            continue;
        };
        if entry.kind == LocalEntryKind::Directory {
            if !recognized_managed_directory(prefix, &key) {
                finding(
                    report,
                    CHECK_INVENTORY,
                    "storage.local_directory_unrecognized",
                    Severity::Warning,
                    Some(path_entity(&entry.path)),
                    "local storage contains an unexpected directory layout",
                    "Preserve the directory until its ownership is known, then move it outside the Vault tree.",
                );
            }
            continue;
        }
        match classify_managed_key(prefix, &key) {
            ManagedKeyKind::Direct { digest } => {
                object_count = object_count.saturating_add(1);
                if database.complete_for_storage && !location_keys.contains(key.as_str()) {
                    finding(
                        report,
                        CHECK_INVENTORY,
                        "storage.untracked_object",
                        Severity::Warning,
                        Some(object_entity(&key)),
                        "canonical local object has no database location row",
                        "Preserve the object until its ownership is established; it may be recoverable content or garbage-collection residue.",
                    );
                }
                if !attempted_keys.contains(key.as_str()) {
                    hash_untracked_local_direct(root, &entry.path, &key, &digest, counters, report)
                        .await;
                }
            }
            ManagedKeyKind::Manifest { digest } => {
                object_count = object_count.saturating_add(1);
                manifest_digests.insert(digest.clone());
                if database.complete_for_storage && !location_keys.contains(key.as_str()) {
                    finding(
                        report,
                        CHECK_INVENTORY,
                        "storage.untracked_object",
                        Severity::Warning,
                        Some(object_entity(&key)),
                        "canonical multipart object has no database location row",
                        "Preserve the manifest and parts until their ownership is established.",
                    );
                }
                match read_local_manifest_payload(root, &key).await {
                    Ok(payload) => match validate_manifest_payload(prefix, &key, &payload) {
                        Ok(()) => {
                            manifest_part_keys
                                .extend(payload.parts.iter().map(|part| part.object_key.clone()));
                        }
                        Err(error) => report_manifest_failure(
                            report,
                            &key,
                            location_keys.contains(key.as_str()),
                            error,
                        ),
                    },
                    Err(error) => report_manifest_failure(
                        report,
                        &key,
                        location_keys.contains(key.as_str()),
                        error,
                    ),
                }
                if !attempted_keys.contains(key.as_str()) {
                    hash_untracked_local_multipart(root, prefix, &key, &digest, counters, report)
                        .await;
                }
            }
            ManagedKeyKind::LegacyPart {
                digest,
                part_number,
            } => {
                part_groups
                    .entry((digest, None))
                    .or_default()
                    .push((part_number, entry));
            }
            ManagedKeyKind::LayoutPart {
                digest,
                layout_id,
                part_number,
            } => {
                part_groups
                    .entry((digest, Some(layout_id)))
                    .or_default()
                    .push((part_number, entry));
            }
            ManagedKeyKind::Malformed => {
                finding(
                    report,
                    CHECK_INVENTORY,
                    "storage.managed_entry_malformed",
                    Severity::Warning,
                    Some(path_entity(&entry.path)),
                    "file inside the managed prefix does not use a recognized object, manifest, or part layout",
                    "Preserve the file until its origin is known, then move it outside the managed prefix.",
                );
            }
            ManagedKeyKind::OutsidePrefix => {
                classify_local_operational_debris(entry, &key, report);
            }
        }
    }
    report.record_objects(CHECK_INVENTORY, object_count);

    for ((digest, layout_id), mut parts) in part_groups {
        parts.sort_by_key(|(number, _)| *number);
        for (_, entry) in &parts {
            let Some(key) = path_to_key(&entry.relative) else {
                continue;
            };
            if !manifest_part_keys.contains(&key) {
                finding(
                    report,
                    CHECK_INVENTORY,
                    "storage.multipart_unreferenced_part",
                    Severity::Warning,
                    Some(object_entity(&key)),
                    "multipart part is not referenced by a valid manifest",
                    "Preserve the part until its digest group is assessed; it may be recoverable interrupted publication residue.",
                );
            }
        }
        if !manifest_digests.contains(&digest) {
            audit_manifestless_part_group(
                root,
                &digest,
                layout_id.as_deref(),
                &parts,
                counters,
                report,
            )
            .await;
        }
    }
}

fn path_to_key(relative: &Path) -> Option<String> {
    relative.to_str().map(|value| value.replace('\\', "/"))
}

fn report_manifest_failure(
    report: &mut ReportBuilder,
    key: &str,
    has_location: bool,
    error: impl AsRef<str>,
) {
    finding(
        report,
        CHECK_INVENTORY,
        "storage.multipart_manifest_invalid",
        if has_location {
            Severity::Error
        } else {
            Severity::Warning
        },
        Some(object_entity(key)),
        format!("multipart manifest is invalid: {}", bounded(error)),
        "Preserve the manifest and parts, then recover from a verified copy or reviewed reconstruction plan.",
    );
}

async fn read_local_manifest_payload(
    root: &Path,
    key: &str,
) -> Result<MultipartManifestPayload, String> {
    let path = local_object_path(root, key)?;
    let metadata = safe_regular_file_metadata(root, &path)
        .await
        .map_err(|error| error.to_string())?;
    if metadata.len() > MAX_MULTIPART_MANIFEST_BYTES {
        return Err("manifest exceeds 512 KiB".to_string());
    }
    let file = fs::File::open(path)
        .await
        .map_err(|error| error.to_string())?;
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| "manifest size is not addressable".to_string())?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_MULTIPART_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_MULTIPART_MANIFEST_BYTES {
        return Err("manifest exceeds 512 KiB".to_string());
    }
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

async fn hash_untracked_local_direct(
    root: &Path,
    path: &Path,
    key: &str,
    expected_digest: &str,
    counters: &mut ScanCounters,
    report: &mut ReportBuilder,
) {
    counters.objects = counters.objects.saturating_add(1);
    let mut hasher = Sha256::new();
    match hash_local_file_into(root, path, &mut hasher).await {
        Ok((bytes, changed)) => {
            counters.bytes_hashed = counters.bytes_hashed.saturating_add(bytes);
            if changed {
                incomplete(
                    report,
                    CHECK_CONTENT,
                    "storage.copy_changed_during_read",
                    Some(object_entity(key)),
                    "untracked local object changed while it was being hashed",
                );
            } else {
                let actual = lower_hex(&hasher.finalize());
                if actual != expected_digest {
                    finding(
                        report,
                        CHECK_CONTENT,
                        "storage.path_digest_mismatch",
                        Severity::Warning,
                        Some(object_entity(key)),
                        format!(
                            "path asserts SHA-256 {expected_digest}, but bytes hash to {actual}"
                        ),
                        "Preserve the file for recovery analysis; do not attach it to blob metadata under the asserted digest.",
                    );
                }
            }
        }
        Err(error) => incomplete(
            report,
            CHECK_CONTENT,
            "storage.untracked_object_unreadable",
            Some(object_entity(key)),
            format!(
                "canonical untracked object could not be hashed: {}",
                bounded(error)
            ),
        ),
    }
}

async fn hash_untracked_local_multipart(
    root: &Path,
    prefix: &str,
    key: &str,
    expected_digest: &str,
    counters: &mut ScanCounters,
    report: &mut ReportBuilder,
) {
    counters.objects = counters.objects.saturating_add(1);
    match hash_local_multipart(root, prefix, key).await {
        Ok((actual, bytes, changed)) => {
            counters.bytes_hashed = counters.bytes_hashed.saturating_add(bytes);
            if changed {
                incomplete(
                    report,
                    CHECK_CONTENT,
                    "storage.copy_changed_during_read",
                    Some(object_entity(key)),
                    "untracked multipart object changed while it was being hashed",
                );
            } else if actual != expected_digest {
                finding(
                    report,
                    CHECK_CONTENT,
                    "storage.path_digest_mismatch",
                    Severity::Warning,
                    Some(object_entity(key)),
                    format!(
                        "manifest path asserts SHA-256 {expected_digest}, but parts hash to {actual}"
                    ),
                    "Preserve the manifest and parts for recovery analysis.",
                );
            }
        }
        Err(error) => {
            let failure = copy_failure_from_local_message(error);
            if failure.kind == CopyFailureKind::Unavailable {
                incomplete(
                    report,
                    CHECK_CONTENT,
                    "storage.untracked_multipart_read_incomplete",
                    Some(object_entity(key)),
                    format!(
                        "untracked multipart object could not be fully inspected: {}",
                        bounded(&failure.message)
                    ),
                );
            } else {
                finding(
                    report,
                    CHECK_CONTENT,
                    "storage.untracked_multipart_unreadable",
                    Severity::Warning,
                    Some(object_entity(key)),
                    format!(
                        "untracked multipart object could not be hashed: {}",
                        bounded(&failure.message)
                    ),
                    "Preserve all parts until recovery value is assessed.",
                );
            }
        }
    }
}

fn classify_local_operational_debris(entry: &LocalEntry, key: &str, report: &mut ReportBuilder) {
    let file_name = entry
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let recognized = key.starts_with(".vault-staging/")
        || file_name.starts_with(".vault-storage.lock.readiness-")
        || interrupted_temp_name(file_name);
    finding(
        report,
        CHECK_INVENTORY,
        if recognized {
            "storage.operational_residue"
        } else {
            "storage.entry_outside_managed_prefix"
        },
        Severity::Warning,
        Some(path_entity(&entry.path)),
        if recognized {
            "recognized interrupted-write or readiness residue remains in local storage"
        } else {
            "unexpected file exists outside the configured managed prefix"
        },
        "Preserve the file until no active operation owns it, then review it for safe cleanup.",
    );
}

fn interrupted_temp_name(name: &str) -> bool {
    let Some((_, suffix)) = name.rsplit_once(".tmp-") else {
        return false;
    };
    suffix.len() == 32
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

async fn audit_manifestless_part_group(
    root: &Path,
    digest: &str,
    layout_id: Option<&str>,
    parts: &[(usize, &LocalEntry)],
    counters: &mut ScanCounters,
    report: &mut ReportBuilder,
) {
    if parts
        .iter()
        .enumerate()
        .any(|(index, (number, _))| *number != index + 1)
    {
        return;
    }
    let sizes = parts
        .iter()
        .filter_map(|(_, entry)| entry.identity.as_ref().map(|identity| identity.length))
        .collect::<Vec<_>>();
    if sizes.len() != parts.len()
        || sizes.contains(&0)
        || layout_id.is_some_and(|layout| multipart_layout_id(&sizes) != layout)
    {
        return;
    }
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    for (_, entry) in parts {
        match hash_local_file_into(root, &entry.path, &mut hasher).await {
            Ok((size, changed)) if !changed => {
                total = total.saturating_add(size);
            }
            Ok((size, _)) => {
                counters.bytes_hashed = counters.bytes_hashed.saturating_add(size);
                incomplete(
                    report,
                    CHECK_CONTENT,
                    "storage.copy_changed_during_read",
                    Some(path_entity(&entry.path)),
                    "manifestless multipart part changed while being hashed",
                );
                return;
            }
            Err(error) => {
                incomplete(
                    report,
                    CHECK_CONTENT,
                    "storage.multipart_part_unreadable",
                    Some(path_entity(&entry.path)),
                    format!(
                        "manifestless multipart part could not be hashed: {}",
                        bounded(error)
                    ),
                );
                return;
            }
        }
    }
    counters.bytes_hashed = counters.bytes_hashed.saturating_add(total);
    counters.objects = counters.objects.saturating_add(1);
    let actual_digest = lower_hex(&hasher.finalize());
    if actual_digest == digest {
        finding(
            report,
            CHECK_INVENTORY,
            "storage.multipart_manifest_missing_recoverable",
            Severity::Warning,
            parts.first().map(|(_, entry)| path_entity(&entry.path)),
            format!(
                "complete canonical part set hashes to its path digest and may be recoverable ({total} bytes)"
            ),
            "Preserve the complete part set and use a reviewed recovery process; the integrity checker will not create a manifest.",
        );
    } else {
        finding(
            report,
            CHECK_CONTENT,
            "storage.multipart_manifestless_digest_mismatch",
            Severity::Warning,
            parts.first().map(|(_, entry)| path_entity(&entry.path)),
            format!(
                "complete manifestless part set asserts SHA-256 {digest} but hashes to {actual_digest}"
            ),
            "Preserve every part for recovery analysis; do not create a manifest under the asserted digest.",
        );
    }
}

async fn verify_inventory_stability(
    config: &Config,
    storage: &dyn BlobStorageBackend,
    first: Option<&BackendInventory>,
    report: &mut ReportBuilder,
) {
    let Some(first) = first else {
        return;
    };
    match first {
        BackendInventory::Local(first) => {
            match inventory_local_tree(&config.objects_path()).await {
                Ok(second)
                    if first.issues.is_empty()
                        && second.issues.is_empty()
                        && inventory_snapshot(first) == inventory_snapshot(&second) => {}
                Ok(_) => incomplete(
                    report,
                    CHECK_INVENTORY,
                    "storage.local_inventory_changed",
                    Some(path_entity(&config.objects_path())),
                    "local storage inventory or file identity changed between passes",
                ),
                Err(error) => incomplete(
                    report,
                    CHECK_INVENTORY,
                    "storage.local_second_inventory_failed",
                    Some(path_entity(&config.objects_path())),
                    format!("second local inventory pass failed: {error}"),
                ),
            }
        }
        BackendInventory::Remote(first) => match remote_inventory_snapshot(storage).await {
            Ok(second) if second == first.snapshot => {}
            Ok(_) => incomplete(
                report,
                CHECK_INVENTORY,
                "storage.remote_inventory_changed",
                None,
                "remote key, size, ETag, or modification identity changed between listing passes",
            ),
            Err(error) => incomplete(
                report,
                CHECK_INVENTORY,
                "storage.remote_second_inventory_failed",
                None,
                format!("second remote inventory pass failed: {error}"),
            ),
        },
    }
}

async fn remote_inventory_snapshot(
    storage: &dyn BlobStorageBackend,
) -> Result<RemoteInventorySnapshot, StorageError> {
    let mut continuation_token = None;
    let mut seen_continuation_tokens = HashSet::new();
    let mut previous_key = None;
    let mut snapshot_hasher = Sha256::new();
    let mut object_count = 0_u64;
    loop {
        let page = storage
            .inventory_object_page(continuation_token.as_deref())
            .await?;
        if page.entries.len() > MAX_REMOTE_INVENTORY_PAGE_ENTRIES {
            return Err(StorageError::Remote(
                "remote inventory exceeded the bounded page-size contract".to_string(),
            ));
        }
        for entry in page.entries {
            if !remote_inventory_entry_has_stable_identity(&entry) {
                return Err(StorageError::Remote(
                    "remote inventory entry lacked a coherent ETag or modification timestamp"
                        .to_string(),
                ));
            }
            if previous_key
                .as_ref()
                .is_some_and(|previous: &String| previous >= &entry.object_key)
            {
                return Err(StorageError::Remote(
                    "remote inventory returned duplicate or out-of-order keys across pages"
                        .to_string(),
                ));
            }
            previous_key = Some(entry.object_key.clone());
            update_remote_inventory_snapshot(&mut snapshot_hasher, &entry);
            object_count = object_count.saturating_add(1);
        }
        let Some(next) = page.continuation_token else {
            break;
        };
        if !seen_continuation_tokens.insert(next.clone()) {
            return Err(StorageError::Remote(
                "remote inventory cycled a continuation token".to_string(),
            ));
        }
        continuation_token = Some(next);
    }
    Ok(RemoteInventorySnapshot {
        objects: object_count,
        identity_sha256: lower_hex(&snapshot_hasher.finalize()),
    })
}

async fn audit_transfer_working_tree(
    config: &Config,
    database: &DatabaseInventory,
    report: &mut ReportBuilder,
) {
    let root = config.transfers_path();
    if !database.complete_for_transfers {
        incomplete(
            report,
            CHECK_WORKING,
            "transfer.database_inventory_incomplete",
            None,
            "upload or export metadata inventory was incomplete, so transfer-tree cross-correlation cannot be authoritative",
        );
    }
    let first = match inventory_local_tree(&root).await {
        Ok(inventory) => inventory,
        Err(error) => {
            incomplete(
                report,
                CHECK_WORKING,
                "transfer.root_unavailable",
                Some(path_entity(&root)),
                format!("transfer root could not be inventoried: {error}"),
            );
            return;
        }
    };
    report.record_files(
        CHECK_WORKING,
        first
            .entries
            .iter()
            .filter(|entry| entry.kind != LocalEntryKind::Directory)
            .count() as u64,
    );
    for (path, error) in &first.issues {
        incomplete(
            report,
            CHECK_WORKING,
            "transfer.entry_unreadable",
            Some(path_entity(path)),
            format!(
                "transfer entry could not be inventoried: {}",
                bounded(error)
            ),
        );
    }

    let upload_sessions = database
        .upload_sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect::<HashMap<_, _>>();
    let export_jobs = database
        .export_jobs
        .iter()
        .map(|job| (job.id.as_str(), job))
        .collect::<HashMap<_, _>>();
    let mut upload_entries = HashMap::<String, Vec<&LocalEntry>>::new();
    let mut upload_directories = HashSet::new();

    for entry in &first.entries {
        if matches!(
            entry.kind,
            LocalEntryKind::Symlink | LocalEntryKind::Special
        ) {
            finding(
                report,
                CHECK_WORKING,
                "transfer.unsafe_entry_type",
                Severity::Error,
                Some(path_entity(&entry.path)),
                "transfer tree contains a symlink or special file that Vault must not traverse",
                "Move the unexpected entry outside the transfer tree after preserving it for investigation.",
            );
            continue;
        }
        let components = entry.relative.components().collect::<Vec<_>>();
        match components.as_slice() {
            [Component::Normal(area)]
                if entry.kind == LocalEntryKind::Directory
                    && (*area == OsStr::new("uploads") || *area == OsStr::new("exports")) => {}
            [Component::Normal(area), Component::Normal(session_id), ..]
                if *area == OsStr::new("uploads") =>
            {
                let session_id = session_id.to_string_lossy().into_owned();
                if components.len() == 2 && entry.kind == LocalEntryKind::Directory {
                    upload_directories.insert(session_id.clone());
                }
                upload_entries.entry(session_id).or_default().push(entry);
            }
            [Component::Normal(area), Component::Normal(file_name)]
                if *area == OsStr::new("exports") && entry.kind == LocalEntryKind::File =>
            {
                audit_export_scratch_entry(
                    entry,
                    &file_name.to_string_lossy(),
                    &export_jobs,
                    database.complete_for_transfers,
                    report,
                );
            }
            _ => finding(
                report,
                CHECK_WORKING,
                "transfer.unexpected_entry",
                Severity::Warning,
                Some(path_entity(&entry.path)),
                "transfer tree contains an unrecognized file or directory layout",
                "Preserve the entry until its ownership is established, then remove it through a reviewed cleanup.",
            ),
        }
    }

    for (session_id, entries) in &upload_entries {
        let Some(session) = upload_sessions.get(session_id.as_str()) else {
            if database.complete_for_transfers {
                finding(
                    report,
                    CHECK_WORKING,
                    "transfer.orphan_upload_directory",
                    Severity::Warning,
                    entries.first().map(|entry| path_entity(&entry.path)),
                    "upload scratch directory has no database session",
                    "Preserve it until its contents are assessed, then remove it through a reviewed cleanup.",
                );
            }
            continue;
        };
        if !matches!(session.status.as_str(), "active" | "completing") {
            finding(
                report,
                CHECK_WORKING,
                "transfer.terminal_upload_residue",
                Severity::Warning,
                entries.first().map(|entry| path_entity(&entry.path)),
                format!(
                    "upload session {} is {}, but scratch data remains",
                    session.id, session.status
                ),
                "Confirm the terminal result is durable before cleaning this scratch directory.",
            );
        }
        audit_upload_scratch(&root, session, entries, report).await;
    }
    if database.complete_for_transfers {
        for session in &database.upload_sessions {
            if matches!(session.status.as_str(), "active" | "completing")
                && safe_transfer_id(&session.id)
                && !upload_directories.contains(&session.id)
            {
                finding(
                    report,
                    CHECK_WORKING,
                    "transfer.upload_directory_missing",
                    Severity::Error,
                    Some(format!("upload_session:{}", session.id)),
                    "nonterminal upload session has no scratch directory",
                    "Ask the uploader to restart unless the missing parts can be restored from backup.",
                );
            }
        }
    }
    scan_legacy_s3_stage_candidates(report).await;

    match inventory_local_tree(&root).await {
        Ok(second)
            if first.issues.is_empty()
                && second.issues.is_empty()
                && inventory_snapshot(&first) == inventory_snapshot(&second) => {}
        Ok(_) => incomplete(
            report,
            CHECK_WORKING,
            "transfer.inventory_changed",
            Some(path_entity(&root)),
            "transfer inventory or file identity changed between passes",
        ),
        Err(error) => incomplete(
            report,
            CHECK_WORKING,
            "transfer.second_inventory_failed",
            Some(path_entity(&root)),
            format!("second transfer inventory pass failed: {error}"),
        ),
    }
}

fn audit_export_scratch_entry(
    entry: &LocalEntry,
    file_name: &str,
    jobs: &HashMap<&str, &super::database::ExportJobRecord>,
    database_complete: bool,
    report: &mut ReportBuilder,
) {
    let Some(job_id) = file_name.strip_suffix(".zip.tmp") else {
        finding(
            report,
            CHECK_WORKING,
            "transfer.export_file_unrecognized",
            Severity::Warning,
            Some(path_entity(&entry.path)),
            "export scratch directory contains an unrecognized filename",
            "Preserve the file until its ownership is established.",
        );
        return;
    };
    match jobs.get(job_id) {
        Some(job) if matches!(job.status.as_str(), "running" | "finalizing") => finding(
            report,
            CHECK_WORKING,
            "transfer.interrupted_export",
            Severity::Warning,
            Some(path_entity(&entry.path)),
            format!(
                "temporary ZIP remains for interrupted {} export",
                job.status
            ),
            "Normal recovery can requeue this export; the integrity checker will not delete its temporary file.",
        ),
        Some(job) => finding(
            report,
            CHECK_WORKING,
            "transfer.terminal_export_residue",
            Severity::Warning,
            Some(path_entity(&entry.path)),
            format!("temporary ZIP remains for {} export", job.status),
            "Confirm the export result is durable before cleaning this file.",
        ),
        None if database_complete => finding(
            report,
            CHECK_WORKING,
            "transfer.orphan_export_file",
            Severity::Warning,
            Some(path_entity(&entry.path)),
            "temporary ZIP has no database export job",
            "Preserve the file until its ownership is established.",
        ),
        None => {}
    }
}

async fn audit_upload_scratch(
    transfer_root: &Path,
    session: &UploadSessionRecord,
    entries: &[&LocalEntry],
    report: &mut ReportBuilder,
) {
    if !safe_transfer_id(&session.id) {
        return;
    }
    let Some(expected_count) = canonical_part_count(session.total_size, session.chunk_size) else {
        return;
    };
    let Ok(max_parts) = i64::try_from(STORAGE_MULTIPART_MAX_PARTS) else {
        return;
    };
    if expected_count != session.part_count || !(0..=max_parts).contains(&session.part_count) {
        return;
    }
    let mut parts = BTreeMap::<i64, &LocalEntry>::new();
    let mut sidecars = BTreeMap::<i64, &LocalEntry>::new();
    let session_root = transfer_root.join("uploads").join(&session.id);
    for entry in entries {
        if entry.kind == LocalEntryKind::Directory {
            if entry.path != session_root {
                finding(
                    report,
                    CHECK_WORKING,
                    "transfer.upload_directory_nested",
                    Severity::Warning,
                    Some(path_entity(&entry.path)),
                    "upload session contains an unexpected nested directory",
                    "Preserve the directory until its ownership is established.",
                );
            }
            continue;
        }
        if entry.kind != LocalEntryKind::File {
            continue;
        }
        if entry.path.parent() != Some(session_root.as_path()) {
            finding(
                report,
                CHECK_WORKING,
                "transfer.upload_file_unrecognized",
                Severity::Warning,
                Some(path_entity(&entry.path)),
                "upload scratch files must be direct children of their session directory",
                "Preserve the file until its ownership is established.",
            );
            continue;
        }
        let name = entry
            .path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        if name == S3_UPLOAD_STAGE_FILENAME || interrupted_temp_name(name) {
            finding(
                report,
                CHECK_WORKING,
                "transfer.upload_staging_residue",
                Severity::Warning,
                Some(path_entity(&entry.path)),
                "interrupted upload staging file remains",
                "Preserve it until the session state is understood; normal recovery may clean it.",
            );
        } else if let Some(number) = upload_part_number(name, ".part") {
            parts.insert(number, entry);
        } else if let Some(number) = upload_part_number(name, ".json") {
            sidecars.insert(number, entry);
        } else {
            finding(
                report,
                CHECK_WORKING,
                "transfer.upload_file_unrecognized",
                Severity::Warning,
                Some(path_entity(&entry.path)),
                "upload scratch directory contains an unrecognized filename",
                "Preserve the file until its ownership is established.",
            );
        }
    }

    for (number, entry) in &parts {
        if !(1..=session.part_count).contains(number) {
            finding(
                report,
                CHECK_WORKING,
                "transfer.upload_part_number_invalid",
                Severity::Error,
                Some(path_entity(&entry.path)),
                format!(
                    "part number {number} is outside the session's 1..={} geometry",
                    session.part_count
                ),
                "Preserve the bytes, then restart or repair the upload through reviewed tooling.",
            );
        }
    }
    for (number, entry) in &sidecars {
        if !(1..=session.part_count).contains(number) || !parts.contains_key(number) {
            finding(
                report,
                CHECK_WORKING,
                "transfer.upload_sidecar_orphaned",
                Severity::Warning,
                Some(path_entity(&entry.path)),
                format!("part metadata {number} has no corresponding expected upload part"),
                "Preserve it until the upload state is understood, then remove it through reviewed cleanup.",
            );
        }
    }

    let mut whole_hasher = Sha256::new();
    let mut part_digests = Vec::new();
    let mut complete = true;
    let mut bytes_hashed = 0_u64;
    for part_number in 1..=session.part_count {
        let Some(entry) = parts.get(&part_number) else {
            complete = false;
            if session.status == "completing" {
                finding(
                    report,
                    CHECK_WORKING,
                    "transfer.completing_part_missing",
                    Severity::Error,
                    Some(format!("upload_session:{}", session.id)),
                    format!("completing upload is missing part {part_number}"),
                    "Restore the part or fail and restart the upload.",
                );
            }
            continue;
        };
        let Some((offset, expected_size)) = expected_part_bounds(session, part_number) else {
            complete = false;
            continue;
        };
        let mut part_hasher = Sha256::new();
        match hash_upload_part(
            &session_root,
            &entry.path,
            &mut whole_hasher,
            &mut part_hasher,
        )
        .await
        {
            Ok((size, false)) => {
                bytes_hashed = bytes_hashed.saturating_add(size);
                if u64::try_from(expected_size).ok() != Some(size) {
                    complete = false;
                    finding(
                        report,
                        CHECK_WORKING,
                        "transfer.upload_part_size_mismatch",
                        Severity::Error,
                        Some(path_entity(&entry.path)),
                        format!(
                            "part {part_number} expects {expected_size} bytes but contains {size}"
                        ),
                        "Restore or re-upload this part.",
                    );
                }
                let digest = lower_hex(&part_hasher.finalize());
                validate_upload_sidecar(
                    &session_root,
                    session,
                    part_number,
                    offset,
                    expected_size,
                    &digest,
                    sidecars.get(&part_number).copied(),
                    report,
                )
                .await;
                part_digests.push(digest);
            }
            Ok((size, true)) => {
                bytes_hashed = bytes_hashed.saturating_add(size);
                complete = false;
                incomplete(
                    report,
                    CHECK_WORKING,
                    "transfer.upload_part_changed",
                    Some(path_entity(&entry.path)),
                    "upload part changed while being hashed",
                );
            }
            Err(error) => {
                complete = false;
                incomplete(
                    report,
                    CHECK_WORKING,
                    "transfer.upload_part_unreadable",
                    Some(path_entity(&entry.path)),
                    format!("upload part could not be hashed: {}", bounded(error)),
                );
            }
        }
    }
    report.record_bytes_hashed(CHECK_WORKING, bytes_hashed);
    if complete && i64::try_from(part_digests.len()).ok() == Some(session.part_count) {
        let whole_digest = lower_hex(&whole_hasher.finalize());
        let manifest_digest = upload_part_manifest_digest(session, &part_digests);
        if session.status == "completing" {
            finding(
                report,
                CHECK_WORKING,
                "transfer.upload_candidate_hashes",
                Severity::Info,
                Some(format!("upload_session:{}", session.id)),
                format!(
                    "ordered parts hash to SHA-256 {whole_digest}; part-manifest SHA-256 is {manifest_digest}"
                ),
                "These are diagnostic candidate hashes; no authoritative whole-file digest exists before completion.",
            );
        }
    }
}

fn safe_transfer_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
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

fn expected_part_bounds(session: &UploadSessionRecord, part_number: i64) -> Option<(i64, i64)> {
    if !(1..=session.part_count).contains(&part_number) {
        return None;
    }
    let offset = part_number
        .checked_sub(1)?
        .checked_mul(session.chunk_size)?;
    let size = session
        .total_size
        .checked_sub(offset)?
        .min(session.chunk_size);
    (size > 0).then_some((offset, size))
}

fn upload_part_number(name: &str, suffix: &str) -> Option<i64> {
    let stem = name.strip_suffix(suffix)?;
    (stem.len() == 8 && stem.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| stem.parse::<i64>().ok())
        .flatten()
}

async fn hash_upload_part(
    session_root: &Path,
    path: &Path,
    whole_hasher: &mut Sha256,
    part_hasher: &mut Sha256,
) -> Result<(u64, bool), String> {
    let before = safe_regular_file_metadata(session_root, path)
        .await
        .map_err(|error| error.to_string())?;
    let mut source = fs::File::open(path)
        .await
        .map_err(|error| error.to_string())?;
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; STORAGE_CHUNK_SIZE];
    loop {
        let read = source
            .read(&mut buffer)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        whole_hasher.update(&buffer[..read]);
        part_hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    let after = safe_regular_file_metadata(session_root, path)
        .await
        .map_err(|error| error.to_string())?;
    Ok((size, file_identity(&before) != file_identity(&after)))
}

#[allow(clippy::too_many_arguments)]
async fn validate_upload_sidecar(
    session_root: &Path,
    session: &UploadSessionRecord,
    part_number: i64,
    expected_offset: i64,
    expected_size: i64,
    actual_digest: &str,
    entry: Option<&LocalEntry>,
    report: &mut ReportBuilder,
) {
    let Some(entry) = entry else {
        finding(
            report,
            CHECK_WORKING,
            "transfer.upload_sidecar_missing",
            Severity::Warning,
            Some(format!("upload_session:{}:part:{part_number}", session.id)),
            "upload part has no checksum metadata sidecar",
            "The runtime can reconstruct this metadata from the correctly sized part.",
        );
        return;
    };
    let metadata = match safe_regular_file_metadata(session_root, &entry.path).await {
        Ok(metadata) if metadata.len() <= MAX_UPLOAD_PART_METADATA_BYTES => metadata,
        Ok(_) => {
            finding(
                report,
                CHECK_WORKING,
                "transfer.upload_sidecar_oversized",
                Severity::Warning,
                Some(path_entity(&entry.path)),
                "upload sidecar exceeds the 4096-byte format limit",
                "Rebuild checksum metadata from the part bytes.",
            );
            return;
        }
        Err(error) => {
            incomplete(
                report,
                CHECK_WORKING,
                "transfer.upload_sidecar_unreadable",
                Some(path_entity(&entry.path)),
                format!("upload sidecar could not be inspected: {error}"),
            );
            return;
        }
    };
    let file = match fs::File::open(&entry.path).await {
        Ok(file) => file,
        Err(error) => {
            incomplete(
                report,
                CHECK_WORKING,
                "transfer.upload_sidecar_unreadable",
                Some(path_entity(&entry.path)),
                format!("upload sidecar could not be opened: {error}"),
            );
            return;
        }
    };
    let Ok(capacity) = usize::try_from(metadata.len()) else {
        incomplete(
            report,
            CHECK_WORKING,
            "transfer.upload_sidecar_unreadable",
            Some(path_entity(&entry.path)),
            "upload sidecar size is not addressable",
        );
        return;
    };
    let mut bytes = Vec::with_capacity(capacity);
    if let Err(error) = file
        .take(MAX_UPLOAD_PART_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
    {
        incomplete(
            report,
            CHECK_WORKING,
            "transfer.upload_sidecar_unreadable",
            Some(path_entity(&entry.path)),
            format!("upload sidecar could not be read: {error}"),
        );
        return;
    }
    let parsed = serde_json::from_slice::<UploadPartMetadata>(&bytes);
    let valid = parsed.as_ref().is_ok_and(|metadata| {
        metadata.part_number == part_number
            && metadata.offset_bytes == expected_offset
            && metadata.size_bytes == expected_size
            && metadata.sha256.as_deref().is_none_or(|digest| {
                digest.len() == 64
                    && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && digest.eq_ignore_ascii_case(actual_digest)
            })
    });
    if !valid {
        finding(
            report,
            CHECK_WORKING,
            "transfer.upload_sidecar_invalid",
            Severity::Warning,
            Some(path_entity(&entry.path)),
            "upload sidecar is malformed or disagrees with part geometry/checksum",
            "Rebuild checksum metadata from the part bytes.",
        );
    }
}

fn upload_part_manifest_digest(session: &UploadSessionRecord, part_digests: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(
        format!(
            "vault-upload-part-manifest-v1\nsize={}\nchunk={}\nparts={}\n",
            session.total_size, session.chunk_size, session.part_count
        )
        .as_bytes(),
    );
    for (index, digest) in part_digests.iter().enumerate() {
        let Some(number) = index
            .checked_add(1)
            .and_then(|number| i64::try_from(number).ok())
        else {
            continue;
        };
        if let Some((offset, size)) = expected_part_bounds(session, number) {
            hasher.update(format!("part={number}:{offset}:{size}:{digest}\n").as_bytes());
        }
    }
    lower_hex(&hasher.finalize())
}

async fn scan_legacy_s3_stage_candidates(report: &mut ReportBuilder) {
    let temp_root = std::env::temp_dir();
    let metadata = match fs::symlink_metadata(&temp_root).await {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => metadata,
        _ => return,
    };
    let _ = metadata;
    let Ok(mut entries) = fs::read_dir(&temp_root).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let Some(uuid) = name
            .strip_prefix("vault-s3-upload-")
            .and_then(|name| name.strip_suffix(".tmp"))
        else {
            continue;
        };
        if uuid.len() != 32
            || !uuid
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path).await else {
            continue;
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_none_or(|age| age < LEGACY_S3_STAGE_MINIMUM_AGE)
        {
            continue;
        }
        finding(
            report,
            CHECK_WORKING,
            "transfer.legacy_s3_stage_candidate",
            Severity::Info,
            Some(path_entity(&path)),
            "system temporary directory contains an aged legacy S3 staging candidate that cannot be attributed to this Vault instance",
            "Investigate ownership before cleanup; the integrity checker will not remove it.",
        );
    }
}
