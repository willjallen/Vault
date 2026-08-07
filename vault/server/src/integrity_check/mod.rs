#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

use sqlx::Connection as _;

use crate::config::Config;
use crate::storage::{configured_blob_storage, normalize_storage_prefix};

pub mod cli;
pub mod database;
pub mod lock;
pub mod report;
pub mod storage_scan;

use database::DatabaseInventory;
use lock::{InstanceLock, LockPurpose};
use report::{IntegrityReport, ReportBuilder, Severity};

const CHECK_LOCK: &str = "execution.lock";
const CHECK_PATHS: &str = "configuration.paths";
const CHECK_DATABASE_FILES: &str = "database.files";

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    exists: bool,
    length: u64,
    modified_nanos: Option<u128>,
    is_file: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Clone, Copy)]
struct PathSafety {
    database: bool,
    durable_storage: bool,
    transfers: bool,
}

/// Latest completed integrity-check progress, used to emit a truthful partial
/// report if the process receives an interrupt during a later phase.
#[derive(Debug, Clone)]
pub struct IntegrityProgress {
    report: Arc<Mutex<ReportBuilder>>,
}

impl IntegrityProgress {
    #[must_use]
    pub fn new(config: &Config, max_findings_per_code: usize) -> Self {
        let backend = config.storage_backend.trim().to_ascii_lowercase();
        Self {
            report: Arc::new(Mutex::new(ReportBuilder::new(
                backend,
                max_findings_per_code,
            ))),
        }
    }

    fn builder(&self) -> ReportBuilder {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn publish(&self, report: &ReportBuilder) {
        *self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = report.clone();
    }

    #[must_use]
    pub fn interrupted_report(&self) -> IntegrityReport {
        let mut report = self.builder();
        report.set_scope("partial");
        report.mark_incomplete(
            "execution.interruption",
            "integrity.interrupted",
            None,
            "integrity check was interrupted before all mandatory checks completed",
        );
        report.finish()
    }
}

/// Runs one complete, offline, non-repairing integrity check.
#[must_use]
pub async fn run(config: &Config, max_findings_per_code: usize) -> IntegrityReport {
    run_internal(config, max_findings_per_code, None).await
}

/// Runs an integrity check while publishing snapshots after each completed
/// phase for signal-safe partial reporting.
pub async fn run_with_progress(
    config: &Config,
    max_findings_per_code: usize,
    progress: &IntegrityProgress,
) -> IntegrityReport {
    run_internal(config, max_findings_per_code, Some(progress)).await
}

async fn run_internal(
    config: &Config,
    max_findings_per_code: usize,
    progress: Option<&IntegrityProgress>,
) -> IntegrityReport {
    let backend = config.storage_backend.trim().to_ascii_lowercase();
    let mut report = progress.map_or_else(
        || ReportBuilder::new(&backend, max_findings_per_code),
        IntegrityProgress::builder,
    );
    report.ensure_check(CHECK_LOCK);
    publish_progress(progress, &report);

    let db_path = config.db_path();
    let _instance_lock = match InstanceLock::acquire(&db_path, LockPurpose::IntegrityCheck) {
        Ok(lock) => lock,
        Err(error) => {
            report.set_scope("not_started");
            report.mark_incomplete(
                CHECK_LOCK,
                "integrity.instance_lock_unavailable",
                Some(format!("path:{}", lock::lock_path(&db_path).display())),
                error.to_string(),
            );
            publish_progress(progress, &report);
            return report.finish();
        }
    };
    publish_progress(progress, &report);

    for check in [CHECK_PATHS, CHECK_DATABASE_FILES] {
        report.ensure_check(check);
    }
    let path_safety = check_configured_paths(config, &db_path, &mut report);
    publish_progress(progress, &report);

    let mut database_inventory = DatabaseInventory::default();
    if path_safety.database {
        let before_database_files = snapshot_database_files(&db_path, &mut report);
        let pending_journals = pending_transaction_sidecars(&db_path, &mut report);
        match pending_journals {
            Some(sidecars) if sidecars.is_empty() => match database::open_read_only(&db_path).await
            {
                Ok(mut connection) => {
                    database_inventory =
                        database::audit_database(&mut connection, &mut report).await;
                    if let Err(error) = connection.close().await {
                        report.mark_incomplete(
                            CHECK_DATABASE_FILES,
                            "db.read_only_close_failed",
                            Some(format!("path:{}", db_path.display())),
                            format!("read-only SQLite connection did not close cleanly: {error}"),
                        );
                    }
                }
                Err(error) => {
                    report.mark_incomplete(
                        CHECK_DATABASE_FILES,
                        "db.open_read_only_failed",
                        Some(format!("path:{}", db_path.display())),
                        format!("database could not be opened read-only: {error}"),
                    );
                    database::mark_checks_incomplete(&mut report);
                }
            },
            Some(sidecars) => {
                for (path, kind, length) in sidecars {
                    report.mark_incomplete(
                        CHECK_DATABASE_FILES,
                        kind,
                        Some(format!("path:{}", path.display())),
                        format!(
                            "the non-empty SQLite sidecar ({length} bytes) may contain transaction data; inspecting it would require SQLite to create or update shared state"
                        ),
                    );
                }
                database::mark_checks_incomplete(&mut report);
            }
            None => database::mark_checks_incomplete(&mut report),
        }

        compare_database_files(
            &before_database_files,
            &snapshot_database_files(&db_path, &mut report),
            &mut report,
        );
    } else {
        database::mark_checks_incomplete(&mut report);
    }
    publish_progress(progress, &report);

    if path_safety.durable_storage {
        match configured_blob_storage(config).await {
            Ok(storage) => {
                storage_scan::run_storage_scan(
                    config,
                    storage.as_ref(),
                    &database_inventory,
                    &mut report,
                )
                .await;
            }
            Err(error) => {
                report.mark_incomplete(
                    "storage.inventory",
                    "storage.configuration_invalid",
                    None,
                    format!("configured storage backend could not be inspected: {error}"),
                );
                storage_scan::mark_durable_checks_incomplete(&mut report);
            }
        }
    } else {
        storage_scan::mark_durable_checks_incomplete(&mut report);
    }
    publish_progress(progress, &report);

    if path_safety.transfers {
        storage_scan::run_transfer_scan(config, &database_inventory, &mut report).await;
    } else {
        storage_scan::mark_transfer_check_incomplete(&mut report);
    }
    publish_progress(progress, &report);

    report.finish()
}

fn publish_progress(progress: Option<&IntegrityProgress>, report: &ReportBuilder) {
    if let Some(progress) = progress {
        progress.publish(report);
    }
}

fn pending_transaction_sidecars(
    db_path: &Path,
    report: &mut ReportBuilder,
) -> Option<Vec<(PathBuf, &'static str, u64)>> {
    let candidates = [
        (
            path_with_suffix(db_path, "-wal"),
            Some("db.wal_snapshot_requires_shared_state"),
        ),
        (path_with_suffix(db_path, "-shm"), None),
        (
            path_with_suffix(db_path, "-journal"),
            Some("db.rollback_journal_recovery_required"),
        ),
    ];
    let mut pending = Vec::new();
    let mut safe = true;
    for (path, code) in candidates {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                report.mark_incomplete(
                    CHECK_DATABASE_FILES,
                    "db.transaction_sidecar_unsafe",
                    Some(format!("path:{}", path.display())),
                    "SQLite transaction sidecar is not a regular file",
                );
                safe = false;
            }
            Ok(metadata) if metadata.len() > 0 => {
                if let Some(code) = code {
                    pending.push((path, code, metadata.len()));
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                report.mark_incomplete(
                    CHECK_DATABASE_FILES,
                    "db.transaction_sidecar_unavailable",
                    Some(format!("path:{}", path.display())),
                    format!("SQLite transaction sidecar metadata could not be read: {error}"),
                );
                safe = false;
            }
        }
    }
    safe.then_some(pending)
}

fn check_configured_paths(
    config: &Config,
    db_path: &Path,
    report: &mut ReportBuilder,
) -> PathSafety {
    let mut safety = PathSafety {
        database: check_existing_file(db_path, "database", report),
        durable_storage: true,
        transfers: check_existing_directory(&config.transfers_path(), "transfer root", report),
    };
    if check_overlap(db_path, &config.transfers_path(), report) {
        safety.database = false;
        safety.transfers = false;
    }
    if config.storage_backend.trim().eq_ignore_ascii_case("local") {
        safety.durable_storage =
            check_existing_directory(&config.objects_path(), "local storage root", report);
        if !local_storage_prefix_is_safe(&config.storage_prefix) {
            report.mark_incomplete(
                CHECK_PATHS,
                "storage.prefix_unsafe",
                Some(format!("prefix:{}", config.storage_prefix)),
                "configured local storage prefix contains an empty, current-directory, parent-directory, root, or platform-prefix component",
            );
            safety.durable_storage = false;
        }
        if check_overlap(db_path, &config.objects_path(), report) {
            safety.database = false;
            safety.durable_storage = false;
        }
        if check_overlap(&config.objects_path(), &config.transfers_path(), report) {
            safety.durable_storage = false;
            safety.transfers = false;
        }
    }
    safety
}

fn local_storage_prefix_is_safe(prefix: &str) -> bool {
    let normalized = normalize_storage_prefix(prefix);
    if normalized.is_empty() {
        return true;
    }
    normalized
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
        && Path::new(&normalized)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn check_existing_file(path: &Path, label: &str, report: &mut ReportBuilder) -> bool {
    if reject_symlink_component(path, label, report) {
        return false;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => true,
        Ok(_) => {
            report.mark_incomplete(
                CHECK_PATHS,
                "integrity.path_not_file",
                Some(format!("path:{}", path.display())),
                format!("configured {label} is not a regular file"),
            );
            false
        }
        Err(error) => {
            report.mark_incomplete(
                CHECK_PATHS,
                "integrity.path_unavailable",
                Some(format!("path:{}", path.display())),
                format!("configured {label} is unavailable: {error}"),
            );
            false
        }
    }
}

fn check_existing_directory(path: &Path, label: &str, report: &mut ReportBuilder) -> bool {
    if reject_symlink_component(path, label, report) {
        return false;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => true,
        Ok(_) => {
            report.mark_incomplete(
                CHECK_PATHS,
                "integrity.path_not_directory",
                Some(format!("path:{}", path.display())),
                format!("configured {label} is not a real directory"),
            );
            false
        }
        Err(error) => {
            report.mark_incomplete(
                CHECK_PATHS,
                "integrity.path_unavailable",
                Some(format!("path:{}", path.display())),
                format!("configured {label} is unavailable: {error}"),
            );
            false
        }
    }
}

fn reject_symlink_component(path: &Path, label: &str, report: &mut ReportBuilder) -> bool {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    for ancestor in absolute.ancestors().collect::<Vec<_>>().into_iter().rev() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                report.mark_incomplete(
                    CHECK_PATHS,
                    "integrity.path_traverses_symlink",
                    Some(format!("path:{}", ancestor.display())),
                    format!("configured {label} traverses a symbolic link"),
                );
                return true;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    false
}

fn check_overlap(left: &Path, right: &Path, report: &mut ReportBuilder) -> bool {
    let Some(left) = resolved_path(left) else {
        return false;
    };
    let Some(right) = resolved_path(right) else {
        return false;
    };
    if left.starts_with(&right) || right.starts_with(&left) {
        report.finding(
            CHECK_PATHS,
            "integrity.path_overlap",
            Severity::Error,
            Some(format!("path:{}", left.display())),
            format!(
                "configured data paths overlap: {} and {}",
                left.display(),
                right.display()
            ),
            Some(
                "Move the database, object storage, and transfer roots to disjoint paths."
                    .to_string(),
            ),
        );
        true
    } else {
        false
    }
}

fn resolved_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .ok()
}

fn snapshot_database_files(
    db_path: &Path,
    report: &mut ReportBuilder,
) -> BTreeMap<PathBuf, FileIdentity> {
    database_related_paths(db_path)
        .into_iter()
        .filter_map(|path| match std::fs::symlink_metadata(&path) {
            Ok(metadata) => Some((path, identity_from_metadata(&metadata))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some((
                path,
                FileIdentity {
                    exists: false,
                    length: 0,
                    modified_nanos: None,
                    is_file: false,
                    #[cfg(unix)]
                    device: 0,
                    #[cfg(unix)]
                    inode: 0,
                },
            )),
            Err(error) => {
                report.mark_incomplete(
                    CHECK_DATABASE_FILES,
                    "db.file_identity_unavailable",
                    Some(format!("path:{}", path.display())),
                    format!("database file identity could not be read: {error}"),
                );
                None
            }
        })
        .collect()
}

fn compare_database_files(
    before: &BTreeMap<PathBuf, FileIdentity>,
    after: &BTreeMap<PathBuf, FileIdentity>,
    report: &mut ReportBuilder,
) {
    for path in before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        if before.get(path) != after.get(path) {
            report.mark_incomplete(
                CHECK_DATABASE_FILES,
                "db.file_changed_during_check",
                Some(format!("path:{}", path.display())),
                "a SQLite database, WAL, shared-memory, or journal file changed during the read-only database phase",
            );
        }
    }
}

fn database_related_paths(db_path: &Path) -> Vec<PathBuf> {
    ["", "-wal", "-shm", "-journal"]
        .into_iter()
        .map(|suffix| path_with_suffix(db_path, suffix))
        .collect()
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(OsStr::new(suffix));
    PathBuf::from(value)
}

fn identity_from_metadata(metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        exists: true,
        length: metadata.len(),
        modified_nanos: metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos()),
        is_file: metadata.is_file(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    }
}
