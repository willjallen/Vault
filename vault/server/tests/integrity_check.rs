use std::collections::BTreeMap;
use std::ffi::OsString;
#[cfg(unix)]
use std::fs::File;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::process::{Command, Output};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Connection, SqliteConnection};
use vault_server::config::Config;
use vault_server::db;
use vault_server::integrity_check;
use vault_server::integrity_check::cli::integrity_subcommand_requested;
use vault_server::integrity_check::lock::{
    InstanceLock, InstanceLockError, LockPurpose, lock_path,
};
use vault_server::integrity_check::report::{CheckState, IntegrityResult, ReportBuilder, Severity};
use vault_server::previews::PREVIEW_RECIPE;
use vault_server::storage::{LocalBlobStorage, StoredBlob, object_key_for_hash, sha256_hex};

fn isolated_config(data_dir: &Path) -> Config {
    let db_path = data_dir.join("vault.db");
    let objects_path = data_dir.join("objects");
    let transfers_path = data_dir.join("transfers");
    Config::try_parse_from([
        OsString::from("vault-server"),
        OsString::from("--data-dir"),
        data_dir.as_os_str().to_owned(),
        OsString::from("--db-path"),
        db_path.into_os_string(),
        OsString::from("--objects-path"),
        objects_path.into_os_string(),
        OsString::from("--transfers-path"),
        transfers_path.into_os_string(),
        OsString::from("--storage-backend"),
        OsString::from("local"),
    ])
    .expect("isolated Vault config")
}

async fn initialize_vault(data_dir: &Path) -> Config {
    let config = isolated_config(data_dir);
    let pool = db::connect(&config.db_path())
        .await
        .expect("initialize Vault database");
    pool.close().await;
    std::fs::create_dir_all(config.objects_path()).expect("create object root");
    std::fs::create_dir_all(config.transfers_path()).expect("create transfer root");
    config
}

async fn insert_integrity_defects(db_path: &Path) {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(false)
        .foreign_keys(false);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("open database for fault injection");
    sqlx::query(
        "INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', 'not-a-digest', -1)",
    )
    .execute(&mut connection)
    .await
    .expect("insert malformed blob");
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) \
         VALUES (999999, 'local', '', 'objects/sha256/missing')",
    )
    .execute(&mut connection)
    .await
    .expect("insert dangling blob location");
    connection.close().await.expect("close fault connection");
}

async fn register_stored_blob(db_path: &Path, stored: &StoredBlob) -> i64 {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(false)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("open database to register stored blob");
    let inserted = sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES (?, ?, ?)")
        .bind(&stored.hash_algo)
        .bind(&stored.digest)
        .bind(i64::try_from(stored.size_bytes).expect("fixture size fits SQLite integer"))
        .execute(&mut connection)
        .await
        .expect("register blob metadata");
    let blob_id = inserted.last_insert_rowid();
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, ?, ?, ?)",
    )
    .bind(blob_id)
    .bind(&stored.backend)
    .bind(&stored.bucket)
    .bind(&stored.object_key)
    .execute(&mut connection)
    .await
    .expect("register blob location");
    connection.close().await.expect("close fixture database");
    blob_id
}

fn finding_codes(report: &vault_server::integrity_check::report::IntegrityReport) -> Vec<&str> {
    report
        .findings
        .iter()
        .map(|finding| finding.code.as_str())
        .collect()
}

fn database_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn snapshot_database_family(db_path: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let base = db_path.as_os_str().to_string_lossy();
    ["", "-wal", "-shm", "-journal"]
        .into_iter()
        .map(|suffix| {
            let path = PathBuf::from(format!("{base}{suffix}"));
            let bytes = path
                .exists()
                .then(|| std::fs::read(&path).expect("read database family"));
            (path, bytes)
        })
        .collect()
}

fn assert_snapshots_equal(
    before: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    after: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) {
    let changed = before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect::<Vec<_>>();
    assert!(changed.is_empty(), "changed paths: {changed:#?}");
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(path: &Path, snapshot: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        let metadata = std::fs::symlink_metadata(path).expect("snapshot metadata");
        if metadata.is_dir() {
            snapshot.insert(path.to_path_buf(), None);
            let mut children = std::fs::read_dir(path)
                .expect("snapshot directory")
                .map(|entry| entry.expect("snapshot entry").path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                visit(&child, snapshot);
            }
        } else {
            snapshot.insert(
                path.to_path_buf(),
                Some(std::fs::read(path).expect("snapshot file")),
            );
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, &mut snapshot);
    snapshot
}

fn run_binary(data_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vault-server"))
        .args([
            "--data-dir",
            data_dir.to_str().expect("UTF-8 temporary path"),
            "integrity-check",
            "--format",
            "json",
        ])
        .env("VAULT_AUTH_MODE", "intentionally-invalid")
        .env("VAULT_STORAGE_BACKEND", "local")
        .env("VAULT_PORT", "intentionally-not-a-port")
        .env("VAULT_EXPORT_WORKERS", "intentionally-not-a-number")
        .env("RUST_LOG", "trace")
        .env_remove("VAULT_DB_PATH")
        .env_remove("VAULT_OBJECTS_PATH")
        .env_remove("VAULT_TRANSFERS_PATH")
        .env_remove("VAULT_LOCAL_OBJECTS_PATH")
        .env_remove("VAULT_FILES_PATH")
        .output()
        .expect("run vault-server integrity-check")
}

fn parse_process_report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "integrity-check stdout was not pure JSON: {error}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    })
}

#[test]
fn integrity_subcommand_detection_ignores_option_values() {
    /*
     * The two-stage parser must select the reduced integrity configuration only when
     * `integrity-check` is the positional subcommand. A data-directory value with the same text
     * must retain normal server parsing.
     */
    let arguments = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
    assert!(integrity_subcommand_requested(&arguments(&[
        "vault-server",
        "--data-dir",
        "/vault",
        "integrity-check",
        "--format",
        "json",
    ])));
    assert!(!integrity_subcommand_requested(&arguments(&[
        "vault-server",
        "--data-dir",
        "integrity-check",
    ])));
    assert!(!integrity_subcommand_requested(&arguments(&[
        "vault-server",
        "--data-dir=integrity-check",
    ])));
}

#[tokio::test]
async fn initialized_empty_local_vault_passes() {
    /*
     * Initializes the current schema and empty local working roots, then performs the complete
     * library audit. This is the baseline proving a freshly initialized Vault is not diagnosed
     * as damaged or incomplete.
     */
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;

    let report = integrity_check::run(&config, 100).await;

    assert_eq!(report.result, IntegrityResult::Pass, "{report:#?}");
    assert!(report.complete);
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn malformed_blob_and_foreign_key_are_non_passing_findings() {
    /*
     * Bypasses SQLite foreign-key enforcement to inject both an invalid blob identity and a
     * dangling location. The audit must finish, retain both stable finding codes, and return a
     * non-passing result instead of aborting at the first bad row.
     */
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    insert_integrity_defects(&config.db_path()).await;

    let report = integrity_check::run(&config, 100).await;
    let codes = report
        .findings
        .iter()
        .map(|finding| finding.code.as_str())
        .collect::<Vec<_>>();

    assert_ne!(report.result, IntegrityResult::Pass);
    assert!(report.findings_summary.errors > 0);
    assert!(codes.contains(&"blob.identity_invalid"), "{codes:?}");
    assert!(codes.contains(&"db.foreign_key_violation"), "{codes:?}");
}

#[tokio::test]
async fn malformed_timestamps_json_primary_keys_and_legacy_share_aliases_are_checked() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let options = SqliteConnectOptions::new()
        .filename(config.db_path())
        .create_if_missing(false)
        .foreign_keys(false);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("open fault-injection database");
    sqlx::query("INSERT INTO vault_settings (key, value) VALUES (NULL, '{}')")
        .execute(&mut connection)
        .await
        .expect("insert null text primary key");
    sqlx::query(
        "INSERT INTO state_events (id, event_type, resources, created_at) VALUES (?, 'test', '[\"contents\"]', 'not-a-time')",
    )
    .bind(i64::MIN)
    .execute(&mut connection)
    .await
    .expect("insert minimum-rowid event");
    sqlx::query(
        "INSERT INTO state_events (event_type, resources) VALUES ('test', '[\"contents\", 7]')",
    )
    .execute(&mut connection)
    .await
    .expect("insert mixed resource array");
    sqlx::query(
        "INSERT INTO share_links (code, target_type, folder_id, expires_at, item_type, item_id) \
         SELECT 'badexpiry', 'folder', id, '2026-99-99 88:77:66', 'folder', id \
         FROM folders WHERE root_key = 'vault'",
    )
    .execute(&mut connection)
    .await
    .expect("insert malformed share expiry");
    sqlx::query(
        "INSERT INTO share_links (code, target_type, document_id, item_type, item_id) \
         VALUES ('legacyfile', 'document', 999999, 'file', 999999)",
    )
    .execute(&mut connection)
    .await
    .expect("insert compatible legacy share alias");
    connection.close().await.expect("close fault database");

    let report = integrity_check::run(&config, 100).await;
    assert_ne!(report.result, IntegrityResult::Pass, "{report:#?}");
    assert!(report.findings.iter().any(|finding| {
        finding.code == "db.required_value_null"
            && finding
                .entity
                .as_deref()
                .is_some_and(|entity| entity.starts_with("vault_settings["))
    }));
    assert!(report.findings.iter().any(|finding| {
        finding.code == "db.timestamp_malformed"
            && finding.entity.as_deref() == Some("state_events[-9223372036854775808]")
    }));
    assert!(report.findings.iter().any(|finding| {
        finding.code == "db.timestamp_malformed"
            && finding
                .entity
                .as_deref()
                .is_some_and(|entity| entity.starts_with("share_links["))
    }));
    assert!(finding_codes(&report).contains(&"db.json_shape_invalid"));
    assert!(
        !finding_codes(&report).contains(&"share.shape_invalid"),
        "legacy file/document aliases must normalize to the same target: {report:#?}"
    );
}

#[tokio::test]
async fn invalid_utf8_text_is_reported_instead_of_silently_skipped() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let options = SqliteConnectOptions::new()
        .filename(config.db_path())
        .create_if_missing(false);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("open fault-injection database");
    sqlx::query(
        "INSERT INTO vault_settings (key, value) \
         VALUES ('invalid-utf8-setting', CAST(x'80' AS TEXT))",
    )
    .execute(&mut connection)
    .await
    .expect("insert invalid UTF-8 TEXT");
    connection.close().await.expect("close fixture database");

    let report = integrity_check::run(&config, 100).await;

    assert_ne!(report.result, IntegrityResult::Pass, "{report:#?}");
    assert!(
        finding_codes(&report).contains(&"db.text_value_unreadable"),
        "{report:#?}"
    );
}

#[tokio::test]
async fn unreadable_rowids_end_a_table_scan_as_incomplete() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let options = SqliteConnectOptions::new()
        .filename(config.db_path())
        .create_if_missing(false)
        .foreign_keys(false);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("open rowid fixture database");
    sqlx::query("DROP TABLE vault_settings")
        .execute(&mut connection)
        .await
        .expect("replace settings table");
    sqlx::query(
        r"CREATE VIEW vault_settings AS
          WITH RECURSIVE sequence(item) AS
            (SELECT 1 UNION ALL SELECT item + 1 FROM sequence WHERE item < 500)
          SELECT printf('key-%03d', item) AS key, '{}' AS value,
                 '2099-01-01T00:00:00Z' AS updated_at, NULL AS rowid
          FROM sequence",
    )
    .execute(&mut connection)
    .await
    .expect("create a full view page with null shadow rowids");
    connection.close().await.expect("close rowid fixture");

    let report = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        integrity_check::run(&config, 100),
    )
    .await
    .expect("row scan must not repeat an unreadable page forever");

    assert_eq!(report.result, IntegrityResult::Incomplete, "{report:#?}");
    assert!(
        finding_codes(&report).contains(&"db.table_scan_no_progress"),
        "{report:#?}"
    );
}

#[tokio::test]
async fn current_raster_preview_metadata_and_payload_are_validated() {
    /*
     * Gives a current-recipe job an incomplete rendition set backed by invalid WebP bytes, while
     * an unknown future recipe shares that blob. Both current metadata and payload checks must
     * still run without treating the unknown recipe's non-WebP rendition as raster output.
     */
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    let stored = storage
        .put_bytes(b"coherent digest metadata but not a WebP image")
        .await
        .expect("store malformed preview payload");
    let blob_id = register_stored_blob(&config.db_path(), &stored).await;
    let options = SqliteConnectOptions::new()
        .filename(config.db_path())
        .create_if_missing(false)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("open preview fixture database");
    let job_id = sqlx::query(
        "INSERT INTO preview_jobs (source_blob_id, recipe, status, completed_at) \
         VALUES (?, ?, 'ready', CURRENT_TIMESTAMP)",
    )
    .bind(blob_id)
    .bind(PREVIEW_RECIPE)
    .execute(&mut connection)
    .await
    .expect("insert ready preview job")
    .last_insert_rowid();
    for variant in ["small", "medium"] {
        sqlx::query(
            "INSERT INTO preview_renditions \
             (preview_job_id, variant, blob_id, mime_type, width, height) \
             VALUES (?, ?, ?, 'image/webp', 1, 1)",
        )
        .bind(job_id)
        .bind(variant)
        .bind(blob_id)
        .execute(&mut connection)
        .await
        .expect("insert preview rendition");
    }
    let unknown_job_id = sqlx::query(
        "INSERT INTO preview_jobs (source_blob_id, recipe, status, completed_at) \
         VALUES (?, 'future-recipe-v1', 'ready', CURRENT_TIMESTAMP)",
    )
    .bind(blob_id)
    .execute(&mut connection)
    .await
    .expect("insert unknown-recipe preview job")
    .last_insert_rowid();
    sqlx::query(
        "INSERT INTO preview_renditions \
         (preview_job_id, variant, blob_id, mime_type, width, height) \
         VALUES (?, 'original', ?, 'application/octet-stream', 1, 1)",
    )
    .bind(unknown_job_id)
    .bind(blob_id)
    .execute(&mut connection)
    .await
    .expect("insert unknown-recipe non-WebP rendition sharing the raster blob");
    connection.close().await.expect("close preview fixture");

    let report = integrity_check::run(&config, 100).await;

    assert_eq!(report.result, IntegrityResult::Warnings, "{report:#?}");
    assert!(
        finding_codes(&report).contains(&"preview.raster_renditions_invalid"),
        "{report:#?}"
    );
    assert!(
        finding_codes(&report).contains(&"preview.payload_invalid_webp"),
        "{report:#?}"
    );
}

#[tokio::test]
async fn unavailable_object_root_does_not_skip_database_or_transfer_scans() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    std::fs::remove_dir(config.objects_path()).expect("remove empty object root");
    std::fs::write(config.transfers_path().join("orphan.tmp"), b"residue")
        .expect("write transfer fixture");

    let report = integrity_check::run(&config, 100).await;

    assert_eq!(report.result, IntegrityResult::Incomplete, "{report:#?}");
    let database_check = report
        .checks
        .iter()
        .find(|check| check.name == "database.sqlite")
        .expect("database check");
    assert_ne!(database_check.state, CheckState::Incomplete);
    let transfer_check = report
        .checks
        .iter()
        .find(|check| check.name == "storage.working_data")
        .expect("transfer check");
    assert_ne!(transfer_check.state, CheckState::Incomplete);
    assert!(transfer_check.counters.files >= 1);
}

#[tokio::test]
async fn remote_storage_configuration_still_rejects_database_transfer_overlap() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let output = Command::new(env!("CARGO_BIN_EXE_vault-server"))
        .args([
            "--data-dir",
            data_dir.path().to_str().expect("UTF-8 temporary path"),
            "--db-path",
            config
                .db_path()
                .to_str()
                .expect("UTF-8 temporary database path"),
            "--transfers-path",
            data_dir.path().to_str().expect("UTF-8 temporary path"),
            "--storage-backend",
            "s3",
            "integrity-check",
            "--format",
            "json",
        ])
        .env_remove("VAULT_S3_BUCKET")
        .output()
        .expect("run remote-backend overlap integrity check");
    let report = parse_process_report(&output);

    assert_eq!(output.status.code(), Some(2), "{report:#}");
    assert_eq!(report["result"], "incomplete");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .any(|finding| finding["code"] == "integrity.path_overlap"),
        "{report:#}"
    );
    for check_name in ["database.sqlite", "storage.working_data"] {
        let state = report["checks"]
            .as_array()
            .expect("checks array")
            .iter()
            .find(|check| check["name"] == check_name)
            .unwrap_or_else(|| panic!("missing {check_name} check: {report:#}"))["state"]
            .as_str();
        assert_eq!(state, Some("incomplete"), "{check_name}: {report:#}");
    }
}

#[tokio::test]
async fn unsafe_local_storage_prefix_is_rejected_as_incomplete() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let mut config = initialize_vault(data_dir.path()).await;
    config.storage_prefix = "../outside".to_string();

    let report = integrity_check::run(&config, 100).await;

    assert_eq!(report.result, IntegrityResult::Incomplete, "{report:#?}");
    assert!(
        finding_codes(&report).contains(&"storage.prefix_unsafe"),
        "{report:#?}"
    );
    let storage_check = report
        .checks
        .iter()
        .find(|check| check.name == "storage.inventory")
        .expect("storage inventory check");
    assert_eq!(storage_check.state, CheckState::Incomplete, "{report:#?}");
}

#[tokio::test]
async fn retention_upload_and_blob_lifecycle_shapes_are_validated() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let options = SqliteConnectOptions::new()
        .filename(config.db_path())
        .create_if_missing(false)
        .foreign_keys(false);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("open semantic fixture database");
    let vault_root = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1",
    )
    .fetch_one(&mut connection)
    .await
    .expect("Vault root folder");
    sqlx::query("UPDATE folders SET default_ttl_days = 30, default_ttl_action = '' WHERE id = ?")
        .bind(vault_root)
        .execute(&mut connection)
        .await
        .expect("inject blank TTL action");
    sqlx::query(
        "INSERT INTO upload_sessions \
         (id, mode, status, target_folder_id, filename, total_size, chunk_size, part_count, \
          created_by, user_context, expires_at) \
         VALUES ('0123456789abcdef0123456789abcdef', 'create', 'active', ?, '../unsafe', \
                 0, 1024, 0, 'tester', '{}', '2099-01-01T00:00:00Z'), \
                ('00112233445566778899aabbccddeeff', 'create', 'active', ?, ?, \
                 0, 1024, 0, 'tester', '{}', '2099-01-01T00:00:00Z')",
    )
    .bind(vault_root)
    .bind(vault_root)
    .bind("control\ncharacter\t.bin")
    .execute(&mut connection)
    .await
    .expect("inject unsafe upload filenames");
    sqlx::query(
        "INSERT INTO upload_sessions \
         (id, mode, status, filename, total_size, chunk_size, part_count, created_by, \
          user_context, expires_at) \
         VALUES ('fedcba9876543210fedcba9876543210', 'create', 'complete', 'finished.bin', \
                 0, 1024, 0, 'tester', '{}', '2099-01-01T00:00:00Z')",
    )
    .execute(&mut connection)
    .await
    .expect("inject completed upload without manifest digest");
    let reservation_id = sqlx::query(
        "INSERT INTO blobs (hash_algo, hash, size_bytes) \
         VALUES ('_vault_untracked_reservation', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 0)",
    )
    .execute(&mut connection)
    .await
    .expect("insert reservation")
    .last_insert_rowid();
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) \
         VALUES (?, '_vault_deleting:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:local', '', \
                 'objects/sha256/reserved')",
    )
    .bind(reservation_id)
    .execute(&mut connection)
    .await
    .expect("insert mismatched reservation location");
    let unsupported_blob = sqlx::query(
        "INSERT INTO blobs (hash_algo, hash, size_bytes) \
         VALUES ('sha256', 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', 0)",
    )
    .execute(&mut connection)
    .await
    .expect("insert unsupported-backend blob")
    .last_insert_rowid();
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) \
         VALUES (?, 'unsupported', '', \
                 'objects/sha256/cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc')",
    )
    .bind(unsupported_blob)
    .execute(&mut connection)
    .await
    .expect("insert unsupported backend location");
    connection.close().await.expect("close semantic fixture");

    let report = integrity_check::run(&config, 100).await;
    let codes = finding_codes(&report);

    assert!(codes.contains(&"folder.ttl_policy_invalid"), "{report:#?}");
    assert!(codes.contains(&"upload.filename_unsafe"), "{report:#?}");
    assert_eq!(
        report.finding_totals_by_code["upload.filename_unsafe"], 2,
        "both traversal and control-character filenames must be reported: {report:#?}"
    );
    assert!(
        codes.contains(&"upload.complete_manifest_digest_invalid"),
        "{report:#?}"
    );
    assert!(
        codes.contains(&"blob.untracked_reservation_shape"),
        "{report:#?}"
    );
    assert!(
        codes.contains(&"blob.location_backend_unsupported"),
        "{report:#?}"
    );
}

#[tokio::test]
async fn missing_database_is_incomplete_and_is_not_created() {
    /*
     * Supplies readable object and transfer roots but no database. Integrity mode may create its
     * coordination sidecar, but it must report incomplete coverage and leave the absent database
     * absent rather than entering normal create-and-migrate startup.
     */
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = isolated_config(data_dir.path());
    std::fs::create_dir_all(config.objects_path()).expect("create object root");
    std::fs::create_dir_all(config.transfers_path()).expect("create transfer root");

    let report = integrity_check::run(&config, 100).await;

    assert_eq!(report.result, IntegrityResult::Incomplete);
    assert_eq!(report.exit_code(), 2);
    assert!(!config.db_path().exists());
    assert!(lock_path(&config.db_path()).is_file());
}

#[tokio::test]
async fn library_run_preserves_database_objects_and_transfer_data() {
    /*
     * Records every directory and byte in the configured data tree around an audit containing
     * recognizable object and transfer residue. The only permitted difference is the empty
     * advisory-lock sidecar used for offline coordination.
     */
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    std::fs::write(
        config.objects_path().join("sentinel.object"),
        b"object bytes",
    )
    .expect("write object sentinel");
    std::fs::write(
        config.transfers_path().join("sentinel.part"),
        b"transfer bytes",
    )
    .expect("write transfer sentinel");
    let before = snapshot_tree(data_dir.path());

    let _report = integrity_check::run(&config, 100).await;

    let mut after = snapshot_tree(data_dir.path());
    let lock_contents = after
        .remove(&lock_path(&config.db_path()))
        .expect("lock sidecar created");
    assert_eq!(lock_contents, Some(Vec::new()));
    assert_snapshots_equal(&before, &after);
}

#[tokio::test]
async fn direct_objects_are_hashed_and_missing_or_corrupt_copies_are_reported() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    let stored = storage
        .put_bytes(b"content verified by the integrity checker")
        .await
        .expect("store fixture blob");
    register_stored_blob(&config.db_path(), &stored).await;

    let clean = integrity_check::run(&config, 100).await;
    assert_eq!(clean.result, IntegrityResult::Pass, "{clean:#?}");
    assert!(clean.counters.bytes_hashed >= stored.size_bytes);

    let object_path = config.objects_path().join(&stored.object_key);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let original_permissions = std::fs::metadata(&object_path)
            .expect("read object permissions")
            .permissions();
        let mut denied = original_permissions.clone();
        denied.set_mode(0o000);
        std::fs::set_permissions(&object_path, denied).expect("deny fixture object reads");
        let unavailable = integrity_check::run(&config, 100).await;
        std::fs::set_permissions(&object_path, original_permissions)
            .expect("restore fixture object permissions");
        assert_eq!(
            unavailable.result,
            IntegrityResult::Incomplete,
            "{unavailable:#?}"
        );
        assert!(
            finding_codes(&unavailable).contains(&"storage.copy_read_incomplete"),
            "{unavailable:#?}"
        );
    }
    std::fs::write(
        &object_path,
        vec![b'x'; usize::try_from(stored.size_bytes).expect("fixture size is addressable")],
    )
    .expect("corrupt fixture object");
    let corrupt = integrity_check::run(&config, 100).await;
    assert_eq!(corrupt.result, IntegrityResult::Warnings, "{corrupt:#?}");
    assert!(
        finding_codes(&corrupt).contains(&"storage.copy_digest_mismatch"),
        "{corrupt:#?}"
    );

    std::fs::remove_file(&object_path).expect("remove fixture object");
    let missing = integrity_check::run(&config, 100).await;
    assert_eq!(missing.result, IntegrityResult::Warnings, "{missing:#?}");
    assert!(
        finding_codes(&missing).contains(&"storage.copy_missing"),
        "{missing:#?}"
    );
}

#[tokio::test]
async fn untracked_and_multipart_objects_are_exhaustively_checked() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);

    storage
        .put_bytes(b"untracked but content-addressed")
        .await
        .expect("store untracked object");
    let untracked = integrity_check::run(&config, 100).await;
    assert_eq!(
        untracked.result,
        IntegrityResult::Warnings,
        "{untracked:#?}"
    );
    assert!(
        finding_codes(&untracked).contains(&"storage.untracked_object"),
        "{untracked:#?}"
    );

    let parts = tempfile::tempdir().expect("multipart source directory");
    let part_paths = [parts.path().join("one.part"), parts.path().join("two.part")];
    std::fs::write(&part_paths[0], b"multipart first half").expect("write first part");
    std::fs::write(&part_paths[1], b" and second half").expect("write second part");
    let expected_digest = sha256_hex(b"multipart first half and second half");
    let stored = storage
        .put_part_files(&part_paths, Some(&expected_digest))
        .await
        .expect("publish multipart object");
    register_stored_blob(&config.db_path(), &stored).await;
    let manifest = storage
        .read_multipart_manifest(&stored.object_key)
        .await
        .expect("read multipart fixture manifest");
    std::fs::write(&manifest.parts[1].path, b"Xand second half")
        .expect("corrupt multipart part without changing its size");

    let report = integrity_check::run(&config, 100).await;
    assert_ne!(report.result, IntegrityResult::Pass, "{report:#?}");
    assert!(
        finding_codes(&report).contains(&"storage.copy_digest_mismatch"),
        "{report:#?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn local_storage_symlinks_are_reported_without_being_followed() {
    use std::os::unix::fs::symlink;

    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let outside = data_dir.path().join("outside-secret");
    std::fs::write(&outside, b"must not be read").expect("write symlink target");
    symlink(&outside, config.objects_path().join("unexpected-link"))
        .expect("create fixture symlink");

    let report = integrity_check::run(&config, 100).await;
    assert_eq!(report.result, IntegrityResult::Fail, "{report:#?}");
    assert!(
        finding_codes(&report).contains(&"storage.local_symlink"),
        "{report:#?}"
    );
    assert_eq!(
        std::fs::read(&outside).expect("read target"),
        b"must not be read"
    );
}

#[tokio::test]
async fn nonempty_wal_is_incomplete_and_database_family_is_untouched() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let options = SqliteConnectOptions::new()
        .filename(config.db_path())
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(false);
    let mut writer = SqliteConnection::connect_with(&options)
        .await
        .expect("open WAL fixture writer");
    sqlx::query("PRAGMA wal_autocheckpoint = 0")
        .execute(&mut writer)
        .await
        .expect("disable fixture auto-checkpoint");
    sqlx::query(
        "INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', 'wal-only-invalid', -1)",
    )
    .execute(&mut writer)
    .await
    .expect("commit malformed WAL-only row");
    let before = snapshot_database_family(&config.db_path());
    assert!(
        before
            .iter()
            .any(|(path, bytes)| path.to_string_lossy().ends_with("-wal")
                && bytes.as_ref().is_some_and(|bytes| !bytes.is_empty())),
        "fixture did not retain a WAL"
    );

    let report = integrity_check::run(&config, 100).await;
    let after = snapshot_database_family(&config.db_path());
    assert_eq!(report.result, IntegrityResult::Incomplete, "{report:#?}");
    assert!(
        finding_codes(&report).contains(&"db.wal_snapshot_requires_shared_state"),
        "{report:#?}"
    );
    assert_snapshots_equal(&before, &after);
    writer.close().await.expect("close WAL fixture writer");
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_sidecars_must_be_regular_files() {
    use std::os::unix::fs::symlink;

    let shm_data_dir = tempfile::tempdir().expect("temporary Vault");
    let shm_config = initialize_vault(shm_data_dir.path()).await;
    let shm_path = database_sidecar_path(&shm_config.db_path(), "-shm");
    let shm_entity = format!("path:{}", shm_path.display());
    let shm_target = shm_data_dir.path().join("outside-shared-memory");
    std::fs::write(&shm_target, b"must not be inspected").expect("write SHM symlink target");
    symlink(&shm_target, &shm_path).expect("create SHM sidecar symlink");

    let shm_report = integrity_check::run(&shm_config, 100).await;

    assert_eq!(
        shm_report.result,
        IntegrityResult::Incomplete,
        "{shm_report:#?}"
    );
    assert!(
        shm_report.findings.iter().any(|finding| {
            finding.code == "db.transaction_sidecar_unsafe"
                && finding.entity.as_deref() == Some(shm_entity.as_str())
        }),
        "{shm_report:#?}"
    );
    assert_eq!(
        std::fs::read(&shm_target).expect("read SHM symlink target"),
        b"must not be inspected"
    );

    let wal_data_dir = tempfile::tempdir().expect("temporary Vault");
    let wal_config = initialize_vault(wal_data_dir.path()).await;
    let wal_path = database_sidecar_path(&wal_config.db_path(), "-wal");
    let wal_entity = format!("path:{}", wal_path.display());
    std::fs::create_dir(&wal_path).expect("create non-file WAL sidecar");

    let wal_report = integrity_check::run(&wal_config, 100).await;

    assert_eq!(
        wal_report.result,
        IntegrityResult::Incomplete,
        "{wal_report:#?}"
    );
    assert!(
        wal_report.findings.iter().any(|finding| {
            finding.code == "db.transaction_sidecar_unsafe"
                && finding.entity.as_deref() == Some(wal_entity.as_str())
        }),
        "{wal_report:#?}"
    );
}

#[tokio::test]
async fn nested_and_out_of_range_upload_parts_cannot_satisfy_session_geometry() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let options = SqliteConnectOptions::new()
        .filename(config.db_path())
        .create_if_missing(false)
        .foreign_keys(true);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .expect("open upload fixture database");
    sqlx::query(
        "INSERT INTO upload_sessions (id, mode, status, target_folder_id, filename, total_size, \
         chunk_size, part_count, created_by, user_context, expires_at) \
         SELECT 'nested-upload', 'create', 'completing', id, 'fixture.bin', 4, 4, 1, \
                'test-user', '{}', '2099-01-01T00:00:00Z' \
         FROM folders WHERE root_key = 'vault'",
    )
    .execute(&mut connection)
    .await
    .expect("insert upload session");
    connection
        .close()
        .await
        .expect("close upload fixture database");
    let session_root = config
        .transfers_path()
        .join("uploads")
        .join("nested-upload");
    std::fs::create_dir_all(session_root.join("nested")).expect("create nested upload path");
    std::fs::write(session_root.join("nested/00000001.part"), b"data").expect("write nested part");
    std::fs::write(session_root.join("00000002.part"), b"data").expect("write extra part");

    let report = integrity_check::run(&config, 100).await;
    let codes = finding_codes(&report);
    assert!(
        codes.contains(&"transfer.upload_file_unrecognized"),
        "{report:#?}"
    );
    assert!(
        codes.contains(&"transfer.upload_part_number_invalid"),
        "{report:#?}"
    );
    assert!(
        codes.contains(&"transfer.completing_part_missing"),
        "{report:#?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn binary_emits_incomplete_json_and_exits_130_when_interrupted() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let digest = "0".repeat(64);
    let object_key = object_key_for_hash(&config.storage_prefix, "sha256", &digest);
    let object_path = config.objects_path().join(&object_key);
    std::fs::create_dir_all(object_path.parent().expect("object parent"))
        .expect("create object parent");
    let object = File::create(&object_path).expect("create sparse fixture object");
    object
        .set_len(1024 * 1024 * 1024)
        .expect("size sparse object");
    register_stored_blob(
        &config.db_path(),
        &StoredBlob {
            hash_algo: "sha256".to_string(),
            digest,
            size_bytes: 1024 * 1024 * 1024,
            backend: "local".to_string(),
            bucket: String::new(),
            object_key,
        },
    )
    .await;

    let mut child = Command::new(env!("CARGO_BIN_EXE_vault-server"))
        .args([
            "--data-dir",
            data_dir.path().to_str().expect("UTF-8 temporary path"),
            "integrity-check",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn integrity checker");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match InstanceLock::acquire(&config.db_path(), LockPurpose::IntegrityCheck) {
            Err(InstanceLockError::Busy { .. }) => break,
            Ok(lock) => drop(lock),
            Err(error) => panic!("could not probe child lock: {error}"),
        }
        assert!(
            child.try_wait().expect("poll child").is_none(),
            "integrity checker exited before interruption"
        );
        assert!(
            Instant::now() < deadline,
            "child never acquired instance lock"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // Let the bounded database phase complete, then interrupt while the large
    // sparse object's bytes are being hashed.
    thread::sleep(Duration::from_millis(300));
    let signal_status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("signal integrity checker");
    assert!(signal_status.success());
    let output = child
        .wait_with_output()
        .expect("wait for interrupted checker");
    let report = parse_process_report(&output);
    assert_eq!(output.status.code(), Some(130), "{report:#}");
    assert_eq!(report["result"], "incomplete");
    assert_eq!(report["scope"], "partial");
    assert!(report["counters"]["rows"].as_u64().unwrap_or_default() > 0);
    assert!(
        report["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .any(|finding| finding["code"] == "integrity.interrupted")
    );
}

#[test]
fn instance_lock_rejects_contention_and_releases_on_drop() {
    /*
     * Acquires two independent handles for the same database lock path. The second acquisition
     * must receive the dedicated busy error, and dropping the owner must make the lock available
     * without deleting or rewriting the sidecar.
     */
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let db_path = data_dir.path().join("vault.db");
    let first = InstanceLock::acquire(&db_path, LockPurpose::IntegrityCheck)
        .expect("first lock acquisition");

    let second = InstanceLock::acquire(&db_path, LockPurpose::IntegrityCheck);
    assert!(matches!(second, Err(InstanceLockError::Busy { .. })));

    drop(first);
    let reacquired = InstanceLock::acquire(&db_path, LockPurpose::IntegrityCheck)
        .expect("lock released on drop");
    assert_eq!(reacquired.path(), lock_path(&db_path));
}

#[cfg(unix)]
#[test]
fn instance_lock_canonicalizes_database_symlinks() {
    use std::os::unix::fs::symlink;

    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let real_db = data_dir.path().join("vault.db");
    File::create(&real_db).expect("create database target");
    let alias_db = data_dir.path().join("vault-alias.db");
    symlink(&real_db, &alias_db).expect("create database symlink");

    let owner = InstanceLock::acquire(&real_db, LockPurpose::Server).expect("acquire real lock");
    let alias = InstanceLock::acquire(&alias_db, LockPurpose::IntegrityCheck);
    assert!(matches!(alias, Err(InstanceLockError::Busy { .. })));
    drop(owner);
}

#[cfg(unix)]
#[test]
fn instance_lock_rejects_a_dangling_lock_symlink_without_creating_its_target() {
    use std::os::unix::fs::symlink;

    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let db_path = data_dir.path().join("vault.db");
    std::fs::write(&db_path, b"database placeholder").expect("create database path");
    let target = data_dir.path().join("must-not-be-created");
    let sidecar = lock_path(&db_path);
    symlink(&target, &sidecar).expect("create dangling lock symlink");

    let error = InstanceLock::acquire(&db_path, LockPurpose::IntegrityCheck)
        .expect_err("symlink lock path must be rejected");

    assert!(matches!(error, InstanceLockError::Io { .. }));
    assert!(!target.exists(), "dangling symlink target was created");
}

#[cfg(unix)]
#[tokio::test]
async fn non_utf8_database_names_still_detect_transaction_sidecars() {
    use std::os::unix::ffi::OsStringExt;

    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let mut config = initialize_vault(data_dir.path()).await;
    let renamed = data_dir
        .path()
        .join(OsString::from_vec(b"vault-\xff.db".to_vec()));
    std::fs::rename(config.db_path(), &renamed).expect("rename database to non-UTF-8 path");
    config.db_path = Some(renamed.clone());
    let mut wal_name = renamed.as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal_path = PathBuf::from(wal_name);
    std::fs::write(&wal_path, b"pending transaction bytes").expect("write WAL sidecar");

    let report = integrity_check::run(&config, 100).await;

    assert_eq!(report.result, IntegrityResult::Incomplete, "{report:#?}");
    assert!(
        finding_codes(&report).contains(&"db.wal_snapshot_requires_shared_state"),
        "{report:#?}"
    );
}

#[tokio::test]
async fn held_lock_reports_only_incomplete_not_started_scope() {
    let data_dir = tempfile::tempdir().expect("temporary Vault");
    let config = initialize_vault(data_dir.path()).await;
    let owner = InstanceLock::acquire(&config.db_path(), LockPurpose::Server)
        .expect("acquire server instance lock");

    let report = integrity_check::run(&config, 100).await;

    assert_eq!(report.result, IntegrityResult::Incomplete);
    assert_eq!(report.scope, "not_started");
    assert_eq!(report.checks.len(), 1, "{report:#?}");
    assert_eq!(report.checks[0].name, "execution.lock");
    assert_eq!(
        report.checks[0].state,
        vault_server::integrity_check::report::CheckState::Incomplete
    );
    drop(owner);
}

#[test]
fn report_renderers_preserve_exact_counts_with_bounded_detail() {
    /*
     * Adds three occurrences of one stable finding code through a builder capped at two details.
     * Both renderers must expose a failing report while the serialized totals retain the omitted
     * third occurrence.
     */
    let mut builder = ReportBuilder::new("local", 2);
    builder.ensure_check("database.blobs");
    for entity in ["blob:3", "blob:1", "blob:2"] {
        builder.finding(
            "database.blobs",
            "blob.identity_invalid",
            Severity::Error,
            Some(entity.to_string()),
            "invalid blob digest",
            Some("restore valid metadata".to_string()),
        );
    }
    let report = builder.finish();

    assert_eq!(report.result, IntegrityResult::Fail);
    assert_eq!(report.findings_summary.total, 3);
    assert_eq!(report.findings.len(), 2);
    assert_eq!(report.omitted_by_code["blob.identity_invalid"], 1);
    assert_eq!(
        report
            .findings
            .iter()
            .map(|finding| finding.entity.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("blob:1"), Some("blob:2")]
    );
    assert!(
        report
            .render_human()
            .contains("Vault integrity check: FAIL")
    );
    let json = report.render_json().expect("render report JSON");
    let parsed: Value = serde_json::from_str(&json).expect("parse rendered report JSON");
    assert_eq!(parsed["report_version"], 1);
    assert_eq!(parsed["findings_summary"]["total"], 3);
    assert_eq!(parsed["finding_totals_by_code"]["blob.identity_invalid"], 3);
    assert_eq!(parsed["omitted_by_code"]["blob.identity_invalid"], 1);
}

#[tokio::test]
async fn binary_emits_pure_json_and_uses_documented_exit_codes() {
    /*
     * Invokes the compiled server exactly as an operator would. A valid Vault exits zero despite
     * deliberately invalid authentication configuration, corrupted metadata exits one, and a
     * missing database exits two; every stdout stream must remain independently parseable JSON.
     */
    let passing_dir = tempfile::tempdir().expect("passing Vault");
    let passing_config = initialize_vault(passing_dir.path()).await;
    let passing = run_binary(passing_dir.path());
    let passing_json = parse_process_report(&passing);
    assert_eq!(passing.status.code(), Some(0), "{passing_json:#}");
    assert_eq!(passing_json["result"], "pass");

    let human = Command::new(env!("CARGO_BIN_EXE_vault-server"))
        .args([
            "--data-dir",
            passing_dir.path().to_str().expect("UTF-8 temporary path"),
            "integrity-check",
        ])
        .env("VAULT_PORT", "intentionally-not-a-port")
        .env("VAULT_STORAGE_BACKEND", "local")
        .output()
        .expect("run human integrity check");
    assert_eq!(human.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&human.stdout).starts_with("Vault integrity check: PASS"),
        "stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&human.stdout),
        String::from_utf8_lossy(&human.stderr)
    );

    LocalBlobStorage::new(
        passing_config.objects_path(),
        &passing_config.storage_prefix,
    )
    .put_bytes(b"untracked warning-only object")
    .await
    .expect("store warning-only fixture");
    let warning = run_binary(passing_dir.path());
    let warning_json = parse_process_report(&warning);
    assert_eq!(warning.status.code(), Some(1), "{warning_json:#}");
    assert_eq!(warning_json["result"], "warnings");

    let failing_dir = tempfile::tempdir().expect("failing Vault");
    let failing_config = initialize_vault(failing_dir.path()).await;
    insert_integrity_defects(&failing_config.db_path()).await;
    let failing = run_binary(failing_dir.path());
    let failing_json = parse_process_report(&failing);
    assert_eq!(failing.status.code(), Some(1), "{failing_json:#}");
    assert_eq!(failing_json["result"], "fail");

    let incomplete_dir = tempfile::tempdir().expect("incomplete Vault");
    std::fs::create_dir_all(incomplete_dir.path().join("objects")).expect("object root");
    std::fs::create_dir_all(incomplete_dir.path().join("transfers")).expect("transfer root");
    let incomplete = run_binary(incomplete_dir.path());
    let incomplete_json = parse_process_report(&incomplete);
    assert_eq!(incomplete.status.code(), Some(2), "{incomplete_json:#}");
    assert_eq!(incomplete_json["result"], "incomplete");
    assert!(!incomplete_dir.path().join("vault.db").exists());
}
