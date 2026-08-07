#![allow(clippy::too_many_lines)]
//! Read-only `SQLite` and application-metadata integrity auditing.
//!
//! This module deliberately owns the database-specific audit rules. Runtime
//! startup stays fail-fast; the integrity checker records as many independent
//! defects as it can and never invokes migration or recovery code.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use futures_util::TryStreamExt;
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqliteRow};
use sqlx::{ConnectOptions, Connection, Row, SqliteConnection};

use super::report::{ReportBuilder, Severity};
use crate::db;
use crate::root_folders::ROOT_FOLDERS;

const CHECK_SQLITE: &str = "database.sqlite";
const CHECK_SCHEMA: &str = "database.schema";
const CHECK_ROWS: &str = "database.rows";
const CHECK_FOLDERS: &str = "database.folders";
const CHECK_ACCESS: &str = "database.access";
const CHECK_DOCUMENTS: &str = "database.documents";
const CHECK_BLOBS: &str = "database.blobs";
const CHECK_TRANSFERS: &str = "database.transfers";
const CHECK_PREVIEWS: &str = "database.previews";
const CHECK_SHARES_STATE: &str = "database.shares_state";
const MAX_BUFFERED_DOMAIN_ROWS: u64 = 1_000_000;

const APPLICATION_TABLES: &[&str] = &[
    "schema_migrations",
    "folders",
    "folder_events",
    "vault_users",
    "vault_groups",
    "vault_group_memberships",
    "folder_permissions",
    "vault_settings",
    "blobs",
    "blob_locations",
    "documents",
    "document_locks",
    "document_versions",
    "document_events",
    "upload_sessions",
    "upload_parts",
    "export_jobs",
    "export_artifacts",
    "state_events",
    "share_links",
    "preview_jobs",
    "preview_renditions",
];

const FULL_STATE_EVENT_RESOURCES: &[&str] = &[
    "admin",
    "contents",
    "document_detail",
    "my_edits",
    "preferences",
    "previews",
    "settings",
    "sidebar",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlobRecord {
    pub(crate) id: i64,
    pub(crate) hash_algo: String,
    pub(crate) hash: String,
    pub(crate) size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlobLocationRecord {
    pub(crate) id: i64,
    pub(crate) blob_id: i64,
    pub(crate) backend: String,
    pub(crate) bucket: String,
    pub(crate) object_key: String,
    pub(crate) created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LiveReferenceKind {
    DocumentVersion,
    ExportArtifact,
    PreviewRendition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UploadSessionRecord {
    pub(crate) id: String,
    pub(crate) mode: String,
    pub(crate) status: String,
    pub(crate) total_size: i64,
    pub(crate) chunk_size: i64,
    pub(crate) part_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExportJobRecord {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) filename: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviewJobRecord {
    pub(crate) id: i64,
    pub(crate) source_blob_id: i64,
    pub(crate) recipe: String,
    pub(crate) status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviewRenditionRecord {
    pub(crate) id: i64,
    pub(crate) preview_job_id: i64,
    pub(crate) blob_id: i64,
    pub(crate) variant: String,
    pub(crate) mime_type: String,
    pub(crate) width: i64,
    pub(crate) height: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DatabaseInventory {
    pub(crate) complete_for_storage: bool,
    pub(crate) complete_for_transfers: bool,
    pub(crate) row_counts: BTreeMap<String, u64>,
    pub(crate) blobs: BTreeMap<i64, BlobRecord>,
    pub(crate) locations: Vec<BlobLocationRecord>,
    pub(crate) live_references: BTreeMap<i64, BTreeSet<LiveReferenceKind>>,
    pub(crate) upload_sessions: Vec<UploadSessionRecord>,
    pub(crate) export_jobs: Vec<ExportJobRecord>,
    pub(crate) preview_jobs: Vec<PreviewJobRecord>,
    pub(crate) preview_renditions: Vec<PreviewRenditionRecord>,
}

#[derive(Debug, Clone)]
struct ColumnDefinition {
    name: String,
    declared_type: String,
    not_null: bool,
}

#[derive(Debug, Clone)]
struct FolderRow {
    id: i64,
    root_key: String,
    parent_id: Option<i64>,
    name: String,
    is_root: i64,
    archived_at: Option<String>,
}

/// Opens the target database without creating it, changing its journal mode,
/// or running migrations. `query_only` is also enabled as a defense in depth
/// against accidental writes by future audit rules.
pub(crate) async fn open_read_only(path: &Path) -> anyhow::Result<SqliteConnection> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        // The orchestrator permits this immutable opener only after proving that
        // there is no non-empty WAL or rollback journal whose records SQLite
        // would otherwise need to apply. `SQLITE_OPEN_READONLY` alone can create
        // or update WAL shared-memory sidecars even when no writer is present.
        .immutable(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(db::SQLITE_BUSY_TIMEOUT_MS))
        .pragma("query_only", "ON")
        .disable_statement_logging();
    SqliteConnection::connect_with(&options)
        .await
        .map_err(Into::into)
}

/// Ensures every database area appears as incomplete when `SQLite` cannot safely
/// establish the read snapshot. The root cause is reported separately by the
/// orchestrator so dependent checks do not each duplicate it.
pub(crate) fn mark_checks_incomplete(report: &mut ReportBuilder) {
    for check in [
        CHECK_SQLITE,
        CHECK_SCHEMA,
        CHECK_ROWS,
        CHECK_FOLDERS,
        CHECK_ACCESS,
        CHECK_DOCUMENTS,
        CHECK_BLOBS,
        CHECK_TRANSFERS,
        CHECK_PREVIEWS,
        CHECK_SHARES_STATE,
    ] {
        report.mark_check_incomplete(check);
    }
}

/// Audits one consistent read snapshot and returns the database-backed object
/// inventory needed by the storage phase.
pub(crate) async fn audit_database(
    connection: &mut SqliteConnection,
    report: &mut ReportBuilder,
) -> DatabaseInventory {
    for check in [
        CHECK_SQLITE,
        CHECK_SCHEMA,
        CHECK_ROWS,
        CHECK_FOLDERS,
        CHECK_ACCESS,
        CHECK_DOCUMENTS,
        CHECK_BLOBS,
        CHECK_TRANSFERS,
        CHECK_PREVIEWS,
        CHECK_SHARES_STATE,
    ] {
        report.ensure_check(check);
    }

    if let Err(error) = sqlx::query("BEGIN").execute(&mut *connection).await {
        mark_checks_incomplete(report);
        incomplete(
            report,
            CHECK_SQLITE,
            "db.snapshot_unavailable",
            format!("could not establish a consistent read snapshot: {error}"),
        );
        return DatabaseInventory::default();
    }

    check_sqlite(connection, report).await;
    check_schema_and_migrations(connection, report).await;
    let mut inventory = scan_all_rows(connection, report).await;
    mark_oversized_domain_tables(&inventory, report);
    check_folders(connection, report).await;
    check_access_and_settings(connection, report).await;
    check_documents(connection, report).await;
    check_blob_graph(connection, report, &mut inventory).await;
    check_transfers(connection, report, &mut inventory).await;
    check_previews(connection, report, &mut inventory).await;
    check_shares_and_state(connection, report).await;
    inventory.complete_for_storage &= [
        "blobs",
        "blob_locations",
        "document_versions",
        "export_artifacts",
        "preview_renditions",
        "upload_sessions",
        "export_jobs",
    ]
    .into_iter()
    .all(|table| {
        inventory.row_counts.get(table).copied().unwrap_or_default() <= MAX_BUFFERED_DOMAIN_ROWS
    });

    if let Err(error) = sqlx::query("ROLLBACK").execute(&mut *connection).await {
        incomplete(
            report,
            CHECK_SQLITE,
            "db.snapshot_release_failed",
            format!("could not release the read snapshot cleanly: {error}"),
        );
    }
    inventory
}

fn mark_oversized_domain_tables(inventory: &DatabaseInventory, report: &mut ReportBuilder) {
    for (table, check) in [
        ("folders", CHECK_FOLDERS),
        ("schema_migrations", CHECK_SCHEMA),
        ("vault_users", CHECK_ACCESS),
        ("vault_settings", CHECK_ACCESS),
        ("documents", CHECK_DOCUMENTS),
        ("blobs", CHECK_BLOBS),
        ("blob_locations", CHECK_BLOBS),
        ("document_versions", CHECK_BLOBS),
        ("export_artifacts", CHECK_BLOBS),
        ("upload_sessions", CHECK_TRANSFERS),
        ("export_jobs", CHECK_TRANSFERS),
        ("preview_jobs", CHECK_PREVIEWS),
        ("preview_renditions", CHECK_PREVIEWS),
        ("state_events", CHECK_SHARES_STATE),
    ] {
        let rows = inventory.row_counts.get(table).copied().unwrap_or_default();
        if rows > MAX_BUFFERED_DOMAIN_ROWS {
            report.mark_incomplete(
                check,
                "db.domain_scan_safety_limit",
                Some(format!("table:{table}")),
                format!(
                    "table has {rows} rows, exceeding the 1,000,000-row bounded domain-analysis limit; generic cell validation still completed"
                ),
            );
        }
    }
}

async fn check_sqlite(connection: &mut SqliteConnection, report: &mut ReportBuilder) {
    {
        // SQLite otherwise stops after its default ceiling of 100 errors. Use
        // the largest supported practical ceiling and stream rows so retained
        // report detail remains bounded while per-code totals stay exact.
        let mut rows = sqlx::query_scalar::<_, String>("PRAGMA integrity_check(2147483647)")
            .fetch(&mut *connection);
        loop {
            match rows.try_next().await {
                Ok(Some(message)) if message != "ok" => {
                    finding(
                        report,
                        CHECK_SQLITE,
                        "db.sqlite_integrity",
                        Severity::Error,
                        None,
                        bounded(&message),
                        "Restore the database from a verified backup or follow SQLite recovery procedures.",
                    );
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    incomplete(
                        report,
                        CHECK_SQLITE,
                        "db.sqlite_integrity_unavailable",
                        format!("PRAGMA integrity_check could not finish: {error}"),
                    );
                    break;
                }
            }
        }
    }

    {
        let mut rows = sqlx::query("PRAGMA foreign_key_check").fetch(&mut *connection);
        loop {
            match rows.try_next().await {
                Ok(Some(row)) => {
                    let decoded = (
                        row.try_get::<String, _>("table"),
                        row.try_get::<Option<i64>, _>("rowid"),
                        row.try_get::<String, _>("parent"),
                        row.try_get::<i64, _>("fkid"),
                    );
                    let (Ok(table), Ok(row_id), Ok(parent), Ok(constraint)) = decoded else {
                        incomplete(
                            report,
                            CHECK_SQLITE,
                            "db.foreign_key_row_unreadable",
                            "a foreign-key violation row could not be decoded",
                        );
                        continue;
                    };
                    let row_id =
                        row_id.map_or_else(|| "unknown".to_string(), |value| value.to_string());
                    finding(
                        report,
                        CHECK_SQLITE,
                        "db.foreign_key_violation",
                        Severity::Error,
                        Some(row_entity(&table, row_id)),
                        format!(
                            "row violates foreign key {constraint} referencing table {}",
                            bounded(&parent)
                        ),
                        "Restore the missing parent or remove the invalid child row after taking a backup.",
                    );
                }
                Ok(None) => break,
                Err(error) => {
                    incomplete(
                        report,
                        CHECK_SQLITE,
                        "db.foreign_key_check_unavailable",
                        format!("PRAGMA foreign_key_check could not finish: {error}"),
                    );
                    break;
                }
            }
        }
    }

    match sqlx::query_scalar::<_, i64>("PRAGMA query_only")
        .fetch_one(&mut *connection)
        .await
    {
        Ok(1) => {}
        Ok(value) => incomplete(
            report,
            CHECK_SQLITE,
            "db.query_only_disabled",
            format!("audit connection reported PRAGMA query_only={value}"),
        ),
        Err(error) => incomplete(
            report,
            CHECK_SQLITE,
            "db.query_only_unknown",
            format!("could not verify PRAGMA query_only: {error}"),
        ),
    }
}

async fn check_schema_and_migrations(
    connection: &mut SqliteConnection,
    report: &mut ReportBuilder,
) {
    match sqlx::query(
        "SELECT version, name FROM schema_migrations \
         ORDER BY version, rowid LIMIT 1000000",
    )
    .fetch_all(&mut *connection)
    .await
    {
        Ok(rows) => {
            let expected = db::expected_current_history();
            let actual = rows
                .iter()
                .map(|row| {
                    (
                        row.try_get::<i64, _>("version"),
                        row.try_get::<String, _>("name"),
                    )
                })
                .collect::<Vec<_>>();
            let max_len = expected.len().max(actual.len());
            for index in 0..max_len {
                match (expected.get(index), actual.get(index)) {
                    (Some((expected_version, expected_name)), Some((Ok(version), Ok(name))))
                        if version == expected_version && name == expected_name => {}
                    (Some((expected_version, expected_name)), Some((Ok(version), Ok(name)))) => {
                        finding(
                            report,
                            CHECK_SCHEMA,
                            "db.migration_history_mismatch",
                            Severity::Error,
                            Some(row_entity("schema_migrations", version.to_string())),
                            format!(
                                "history entry {index} is ({version}, {:?}); expected ({expected_version}, {:?})",
                                bounded(name),
                                expected_name
                            ),
                            "Run the matching Vault version or restore an unmodified migration ledger.",
                        );
                    }
                    (Some((expected_version, expected_name)), None) => finding(
                        report,
                        CHECK_SCHEMA,
                        "db.migration_history_missing",
                        Severity::Error,
                        None,
                        format!(
                            "missing migration entry ({expected_version}, {expected_name:?}) at history index {index}"
                        ),
                        "Start the matching Vault normally to migrate, or restore a compatible database backup.",
                    ),
                    (None, Some((Ok(version), Ok(name)))) => finding(
                        report,
                        CHECK_SCHEMA,
                        "db.migration_history_future",
                        Severity::Error,
                        Some(row_entity("schema_migrations", version.to_string())),
                        format!("unknown migration ({version}, {:?})", bounded(name)),
                        "Use a Vault binary that recognizes this database version.",
                    ),
                    (_, Some(_)) => finding(
                        report,
                        CHECK_SCHEMA,
                        "db.migration_history_malformed",
                        Severity::Error,
                        Some(row_entity("schema_migrations", index.to_string())),
                        "migration row could not be decoded using its declared types",
                        "Restore the migration ledger from a compatible backup.",
                    ),
                    (None, None) => {}
                }
            }
        }
        Err(error) => incomplete(
            report,
            CHECK_SCHEMA,
            "db.migration_history_unavailable",
            format!("schema_migrations could not be read: {error}"),
        ),
    }

    let expected = db::expected_current_schema().await;
    let live = db::schema_metadata(connection).await;
    match (expected, live) {
        (Ok(expected), Ok(live)) => {
            for difference in db::schema_differences(&expected, &live) {
                finding(
                    report,
                    CHECK_SCHEMA,
                    "db.schema_mismatch",
                    Severity::Error,
                    None,
                    difference,
                    "Use a compatible Vault binary or restore the expected schema from backup; do not edit it in place without a reviewed recovery plan.",
                );
            }
        }
        (Err(error), _) => incomplete(
            report,
            CHECK_SCHEMA,
            "db.expected_schema_unavailable",
            format!("could not construct the application schema contract: {error}"),
        ),
        (_, Err(error)) => incomplete(
            report,
            CHECK_SCHEMA,
            "db.live_schema_unavailable",
            format!("could not inspect the persisted schema: {error}"),
        ),
    }
}

async fn scan_all_rows(
    connection: &mut SqliteConnection,
    report: &mut ReportBuilder,
) -> DatabaseInventory {
    let mut inventory = DatabaseInventory::default();
    let mut total_rows = 0_u64;
    for table in APPLICATION_TABLES {
        let columns = match table_columns(connection, table).await {
            Ok(columns) if !columns.is_empty() => columns,
            Ok(_) => {
                incomplete(
                    report,
                    CHECK_ROWS,
                    "db.table_missing",
                    format!("required application table {table} is missing"),
                );
                continue;
            }
            Err(error) => {
                incomplete(
                    report,
                    CHECK_ROWS,
                    "db.table_metadata_unavailable",
                    format!("could not inspect columns for table {table}: {error}"),
                );
                continue;
            }
        };
        let mut last_row_id = i64::MIN;
        let mut table_rows = 0_u64;
        loop {
            let previous_last_row_id = last_row_id;
            let first_page = table_rows == 0;
            let sql = storage_scan_sql(table, &columns, first_page);
            let mut query = sqlx::query(&sql);
            if !first_page {
                query = query.bind(last_row_id);
            }
            let rows = match query.fetch_all(&mut *connection).await {
                Ok(rows) => rows,
                Err(error) => {
                    incomplete(
                        report,
                        CHECK_ROWS,
                        "db.table_scan_failed",
                        format!("could not scan every value in table {table}: {error}"),
                    );
                    break;
                }
            };
            if rows.is_empty() {
                break;
            }
            let mut decoded_row_ids = 0_usize;
            for row in &rows {
                let row_id_type = row
                    .try_get::<String, _>("_integrity_rowid_type")
                    .unwrap_or_else(|_| "unreadable".to_string());
                let row_id = match row.try_get::<i64, _>("_integrity_rowid") {
                    Ok(row_id) if row_id_type == "integer" => row_id,
                    Ok(_) => {
                        incomplete(
                            report,
                            CHECK_ROWS,
                            "db.rowid_unreadable",
                            format!(
                                "could not identify a row in table {table}: rowid has SQLite storage class {row_id_type}"
                            ),
                        );
                        continue;
                    }
                    Err(error) => {
                        incomplete(
                            report,
                            CHECK_ROWS,
                            "db.rowid_unreadable",
                            format!("could not identify a row in table {table}: {error}"),
                        );
                        continue;
                    }
                };
                last_row_id = last_row_id.max(row_id);
                decoded_row_ids += 1;
                table_rows += 1;
                inspect_generic_row(report, table, row_id, &columns, row);
            }
            if decoded_row_ids == 0 || (!first_page && last_row_id <= previous_last_row_id) {
                incomplete(
                    report,
                    CHECK_ROWS,
                    "db.table_scan_no_progress",
                    format!(
                        "row pagination made no progress while scanning table {table}; complete row coverage cannot be proven"
                    ),
                );
                break;
            }
            if rows.len() < 500 {
                break;
            }
        }
        inventory
            .row_counts
            .insert((*table).to_string(), table_rows);
        total_rows = total_rows.saturating_add(table_rows);
    }
    report.record_rows(CHECK_ROWS, total_rows);
    inventory
}

async fn table_columns(
    connection: &mut SqliteConnection,
    table: &str,
) -> anyhow::Result<Vec<ColumnDefinition>> {
    let sql = format!("PRAGMA table_info({})", quote_identifier(table));
    let rows = sqlx::query(&sql).fetch_all(&mut *connection).await?;
    rows.into_iter()
        .map(|row| {
            Ok(ColumnDefinition {
                name: row.try_get("name")?,
                declared_type: row.try_get("type")?,
                not_null: row.try_get::<i64, _>("notnull")? != 0
                    || row.try_get::<i64, _>("pk")? > 0,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
}

fn storage_scan_sql(table: &str, columns: &[ColumnDefinition], first_page: bool) -> String {
    let mut projections = vec![
        "rowid AS _integrity_rowid".to_string(),
        "typeof(rowid) AS _integrity_rowid_type".to_string(),
    ];
    for (index, column) in columns.iter().enumerate() {
        let quoted = quote_identifier(&column.name);
        projections.push(format!("typeof({quoted}) AS _integrity_type_{index}"));
        projections.push(format!(
            "CAST({quoted} AS TEXT) AS _integrity_value_{index}"
        ));
    }
    format!(
        "SELECT {} FROM {} {} ORDER BY rowid LIMIT 500",
        projections.join(", "),
        quote_identifier(table),
        if first_page { "" } else { "WHERE rowid > ?" }
    )
}

fn inspect_generic_row(
    report: &mut ReportBuilder,
    table: &str,
    row_id: i64,
    columns: &[ColumnDefinition],
    row: &SqliteRow,
) {
    for (index, column) in columns.iter().enumerate() {
        let storage_class = row
            .try_get::<String, _>(format!("_integrity_type_{index}").as_str())
            .unwrap_or_else(|_| "unreadable".to_string());
        let value =
            match row.try_get::<Option<String>, _>(format!("_integrity_value_{index}").as_str()) {
                Ok(value) => value,
                Err(error) => {
                    if storage_class == "text" {
                        finding(
                            report,
                            CHECK_ROWS,
                            "db.text_value_unreadable",
                            Severity::Error,
                            Some(row_entity(table, row_id.to_string())),
                            format!(
                                "column {} is stored as TEXT but cannot be decoded: {}",
                                column.name,
                                bounded(&error.to_string())
                            ),
                            "Restore a valid text value from backup or reviewed recovery tooling.",
                        );
                    }
                    None
                }
            };
        let expected = expected_storage_classes(&column.declared_type);
        let type_is_valid = storage_class == "null" || expected.contains(&storage_class.as_str());
        if !type_is_valid {
            finding(
                report,
                CHECK_ROWS,
                "db.value_storage_class",
                Severity::Error,
                Some(row_entity(table, row_id.to_string())),
                format!(
                    "column {} has SQLite storage class {storage_class}; declared type {} expects {}",
                    column.name,
                    column.declared_type,
                    expected.join(" or ")
                ),
                "Restore the row from a valid backup or rewrite it through reviewed recovery tooling.",
            );
        }
        if storage_class == "null" && column.not_null {
            finding(
                report,
                CHECK_ROWS,
                "db.required_value_null",
                Severity::Error,
                Some(row_entity(table, row_id.to_string())),
                format!("required column {} is null", column.name),
                "Restore the required value from a valid backup.",
            );
            continue;
        }
        let Some(value) = value else {
            continue;
        };

        if column.name.ends_with("_at") && !valid_timestamp(&value) {
            finding(
                report,
                CHECK_ROWS,
                "db.timestamp_malformed",
                Severity::Error,
                Some(row_entity(table, row_id.to_string())),
                format!("column {} is not a recognized timestamp", column.name),
                "Restore a valid persisted timestamp from backup.",
            );
        }
        if column.name == "expires_at"
            && matches!(table, "upload_sessions" | "export_jobs")
            && time::OffsetDateTime::parse(&value, &time::format_description::well_known::Rfc3339)
                .is_err()
        {
            finding(
                report,
                CHECK_ROWS,
                "db.transfer_expiry_not_rfc3339",
                Severity::Error,
                Some(row_entity(table, row_id.to_string())),
                "expires_at is not strict RFC 3339",
                "Restore the transfer expiration timestamp using RFC 3339.",
            );
        }
        if is_boolean_column(table, &column.name)
            && storage_class == "integer"
            && !matches!(value.as_str(), "0" | "1")
        {
            finding(
                report,
                CHECK_ROWS,
                "db.boolean_out_of_range",
                Severity::Error,
                Some(row_entity(table, row_id.to_string())),
                format!("column {} must be 0 or 1", column.name),
                "Restore the boolean field to its supported binary value.",
            );
        }
        if let Some(expected_json) = json_column_shape(table, &column.name) {
            validate_json_value(report, table, row_id, &column.name, &value, expected_json);
        }
    }
}

fn expected_storage_classes(declared_type: &str) -> &'static [&'static str] {
    let declared_type = declared_type.to_ascii_uppercase();
    if declared_type.contains("INT") {
        &["integer"]
    } else if declared_type.contains("CHAR")
        || declared_type.contains("CLOB")
        || declared_type.contains("TEXT")
    {
        &["text"]
    } else if declared_type.contains("BLOB") || declared_type.is_empty() {
        &["blob"]
    } else if declared_type.contains("REAL")
        || declared_type.contains("FLOA")
        || declared_type.contains("DOUB")
    {
        &["real", "integer"]
    } else {
        &["integer", "real", "text", "blob"]
    }
}

fn is_boolean_column(table: &str, column: &str) -> bool {
    matches!(
        (table, column),
        ("folders", "is_root")
            | ("vault_users", "is_admin" | "is_active")
            | ("folder_permissions", "can_view" | "can_read" | "can_write")
            | ("document_locks", "is_active" | "force_acquired")
            | ("upload_sessions", "rename_to_upload")
    )
}

#[derive(Debug, Clone, Copy)]
enum JsonShape {
    Any,
    Object,
    StringArray,
    ArchivedAccess,
}

fn json_column_shape(table: &str, column: &str) -> Option<JsonShape> {
    match (table, column) {
        ("vault_users", "preferences")
        | ("upload_sessions" | "export_jobs", "user_context")
        | ("export_jobs", "request_payload") => Some(JsonShape::Object),
        ("folders" | "documents", "archived_access") => Some(JsonShape::ArchivedAccess),
        ("state_events", "resources") => Some(JsonShape::StringArray),
        ("vault_settings", "value") => Some(JsonShape::Any),
        _ => None,
    }
}

fn validate_json_value(
    report: &mut ReportBuilder,
    table: &str,
    row_id: i64,
    column: &str,
    raw: &str,
    expected: JsonShape,
) {
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        finding(
            report,
            CHECK_ROWS,
            "db.json_malformed",
            Severity::Error,
            Some(row_entity(table, row_id.to_string())),
            format!("column {column} does not contain valid JSON"),
            "Restore valid JSON from backup or reviewed application metadata.",
        );
        return;
    };
    let valid_shape = match expected {
        JsonShape::Any => true,
        JsonShape::Object => parsed.is_object(),
        JsonShape::StringArray => parsed.as_array().is_some_and(|values| {
            values
                .iter()
                .all(|value| value.as_str().is_some_and(|value| !value.trim().is_empty()))
        }),
        JsonShape::ArchivedAccess => parsed.as_object().is_some_and(|values| {
            values.iter().all(|(key, value)| {
                key.parse::<i64>().is_ok_and(|id| id > 0)
                    && value.as_i64().is_some_and(|level| (1..=3).contains(&level))
            })
        }),
    };
    if !valid_shape {
        finding(
            report,
            CHECK_ROWS,
            "db.json_shape_invalid",
            Severity::Error,
            Some(row_entity(table, row_id.to_string())),
            format!("column {column} has an unsupported JSON shape"),
            "Restore metadata using the shape accepted by this Vault version.",
        );
    }
}

async fn check_folders(connection: &mut SqliteConnection, report: &mut ReportBuilder) {
    let rows = match sqlx::query(
        r"
        SELECT id, root_key, parent_id, name, is_root, archived_at
        FROM folders
        ORDER BY id
        LIMIT 1000000
        ",
    )
    .fetch_all(&mut *connection)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            incomplete(
                report,
                CHECK_FOLDERS,
                "folder.graph_unavailable",
                format!("folder graph could not be decoded: {error}"),
            );
            return;
        }
    };
    let mut folders = Vec::with_capacity(rows.len());
    for row in rows {
        match folder_from_row(&row) {
            Ok(folder) => folders.push(folder),
            Err(error) => finding(
                report,
                CHECK_FOLDERS,
                "folder.row_malformed",
                Severity::Error,
                None,
                format!("a folder row could not be decoded: {error}"),
                "Restore the malformed folder row from a valid backup.",
            ),
        }
    }
    if folders.is_empty() {
        finding(
            report,
            CHECK_FOLDERS,
            "folder.roots_missing",
            Severity::Error,
            None,
            "the folder table contains no root folders",
            "Restore the required Vault and Archive roots from a compatible backup.",
        );
    }

    let by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let roots = folders
        .iter()
        .filter(|folder| folder.is_root == 1)
        .collect::<Vec<_>>();
    if roots.len() != ROOT_FOLDERS.len() {
        finding(
            report,
            CHECK_FOLDERS,
            "folder.root_count",
            Severity::Error,
            None,
            format!(
                "found {} root rows; expected {}",
                roots.len(),
                ROOT_FOLDERS.len()
            ),
            "Restore exactly one Vault root and one Archive root.",
        );
    }
    for definition in ROOT_FOLDERS {
        let matching = roots
            .iter()
            .filter(|folder| folder.root_key == definition.key)
            .copied()
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            finding(
                report,
                CHECK_FOLDERS,
                "folder.root_identity",
                Severity::Error,
                None,
                format!(
                    "root key {} occurs {} times; expected once",
                    definition.key,
                    matching.len()
                ),
                "Restore the canonical root rows from a compatible backup.",
            );
        }
        for root in matching {
            if root.name != definition.stored_name || root.parent_id.is_some() {
                finding(
                    report,
                    CHECK_FOLDERS,
                    "folder.root_shape",
                    Severity::Error,
                    Some(row_entity("folders", root.id.to_string())),
                    format!(
                        "root {} does not have its canonical name and null parent",
                        definition.key
                    ),
                    "Restore the canonical root name and parent relationship.",
                );
            }
        }
    }

    let mut children = HashMap::<i64, Vec<i64>>::new();
    for folder in &folders {
        if !matches!(folder.is_root, 0 | 1) {
            finding(
                report,
                CHECK_FOLDERS,
                "folder.root_flag",
                Severity::Error,
                Some(row_entity("folders", folder.id.to_string())),
                "is_root is not 0 or 1",
                "Restore the binary root flag.",
            );
        }
        if folder.is_root == 1 {
            if !ROOT_FOLDERS
                .iter()
                .any(|definition| definition.key == folder.root_key)
            {
                finding(
                    report,
                    CHECK_FOLDERS,
                    "folder.root_key_unknown",
                    Severity::Error,
                    Some(row_entity("folders", folder.id.to_string())),
                    "root row has an unsupported root key",
                    "Restore the canonical root identity.",
                );
            }
            continue;
        }
        if !canonical_name(&folder.name) {
            finding(
                report,
                CHECK_FOLDERS,
                "folder.name_noncanonical",
                Severity::Error,
                Some(row_entity("folders", folder.id.to_string())),
                "folder name is empty, untrimmed, reserved, or contains a separator/control character",
                "Rename the folder through reviewed recovery tooling.",
            );
        }
        let Some(parent_id) = folder.parent_id else {
            finding(
                report,
                CHECK_FOLDERS,
                "folder.parent_missing",
                Severity::Error,
                Some(row_entity("folders", folder.id.to_string())),
                "non-root folder has no parent",
                "Restore its original parent relationship.",
            );
            continue;
        };
        let Some(parent) = by_id.get(&parent_id) else {
            finding(
                report,
                CHECK_FOLDERS,
                "folder.parent_dangling",
                Severity::Error,
                Some(row_entity("folders", folder.id.to_string())),
                format!("parent folder {parent_id} does not exist"),
                "Restore the missing parent or a valid parent relationship.",
            );
            continue;
        };
        if parent.root_key != folder.root_key {
            finding(
                report,
                CHECK_FOLDERS,
                "folder.parent_cross_root",
                Severity::Error,
                Some(row_entity("folders", folder.id.to_string())),
                "folder and parent have different root keys",
                "Restore a parent within the same root.",
            );
        }
        if parent.is_root == 1
            && ROOT_FOLDERS.iter().any(|definition| {
                definition.key == parent.root_key && !definition.allows_folder_descendants
            })
        {
            finding(
                report,
                CHECK_FOLDERS,
                "folder.root_forbids_descendant",
                Severity::Error,
                Some(row_entity("folders", folder.id.to_string())),
                format!("root {} cannot contain physical folders", parent.root_key),
                "Restore the item to its lifecycle-based archive representation.",
            );
        }
        if parent.is_root == 1
            && ROOT_FOLDERS.iter().any(|definition| {
                definition.key != parent.root_key
                    && !definition.public_path_prefix.is_empty()
                    && definition.public_path_prefix == folder.name
            })
        {
            finding(
                report,
                CHECK_FOLDERS,
                "folder.reserved_namespace",
                Severity::Error,
                Some(row_entity("folders", folder.id.to_string())),
                "folder collides with a reserved root namespace",
                "Rename the folder outside the reserved root namespace.",
            );
        }
        children.entry(parent_id).or_default().push(folder.id);
    }

    let mut reachable = HashSet::with_capacity(folders.len());
    let mut pending = roots.iter().map(|root| root.id).collect::<VecDeque<_>>();
    while let Some(id) = pending.pop_front() {
        if reachable.insert(id)
            && let Some(child_ids) = children.get(&id)
        {
            pending.extend(child_ids);
        }
    }
    for folder in &folders {
        if !reachable.contains(&folder.id) {
            finding(
                report,
                CHECK_FOLDERS,
                "folder.cyclic_or_detached",
                Severity::Error,
                Some(row_entity("folders", folder.id.to_string())),
                "folder is not reachable from a canonical root",
                "Reconstruct the intended ancestry from a backup before editing the graph.",
            );
        }
    }

    report_row_query(
        connection,
        report,
        CHECK_FOLDERS,
        "folders",
        "folder.archive_metadata_incomplete",
        Severity::Error,
        r"
        SELECT CAST(id AS TEXT) AS entity
        FROM folders
        WHERE (archived_at IS NULL AND (archived_origin_path IS NOT NULL OR archived_access IS NOT NULL))
           OR (archived_at IS NOT NULL AND
               (archived_origin_path IS NULL OR trim(archived_origin_path) = ''
                OR archived_access IS NULL OR is_root != 0 OR root_key != 'vault'))
        ORDER BY id
        ",
        "archive metadata is incomplete or attached to an unsupported folder shape",
        "Restore a complete archive timestamp, origin path, and access snapshot, or clear the entire lifecycle tuple.",
    )
    .await;
    report_row_query(
        connection,
        report,
        CHECK_FOLDERS,
        "folders",
        "folder.ttl_policy_invalid",
        Severity::Error,
        r"
        SELECT CAST(id AS TEXT) AS entity
        FROM folders
        WHERE (default_ttl_days IS NULL) != (default_ttl_action IS NULL)
           OR default_ttl_days NOT BETWEEN 1 AND 3650
           OR (default_ttl_action IS NOT NULL
               AND lower(trim(default_ttl_action)) NOT IN ('archive', 'delete'))
        ORDER BY id
        ",
        "default retention fields do not form a supported policy",
        "Restore both retention fields together using 1–3650 days and archive or delete.",
    )
    .await;
    report_row_query(
        connection,
        report,
        CHECK_FOLDERS,
        "folders",
        "folder.color_noncanonical",
        Severity::Warning,
        r"
        SELECT CAST(id AS TEXT) AS entity
        FROM folders
        WHERE color IS NOT NULL
          AND color NOT IN ('blue', 'teal', 'green', 'amber', 'rose', 'violet', 'slate')
        ORDER BY id
        ",
        "folder color is not one of the canonical display values",
        "Resave or clear the folder color.",
    )
    .await;
    for folder in folders.iter().filter(|folder| folder.archived_at.is_some()) {
        if folder.root_key != "vault" || folder.is_root != 0 {
            // Already reported by the archive tuple query; retain the read here
            // so graph and lifecycle validation share one decoded snapshot.
        }
    }
}

fn folder_from_row(row: &SqliteRow) -> Result<FolderRow, sqlx::Error> {
    Ok(FolderRow {
        id: row.try_get("id")?,
        root_key: row.try_get("root_key")?,
        parent_id: row.try_get("parent_id")?,
        name: row.try_get("name")?,
        is_root: row.try_get("is_root")?,
        archived_at: row.try_get("archived_at")?,
    })
}

fn canonical_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\'])
        && !name
            .chars()
            .any(|character| character < ' ' || character == '\u{7f}')
}

async fn check_access_and_settings(connection: &mut SqliteConnection, report: &mut ReportBuilder) {
    for (code, table, query, evidence, remediation) in [
        (
            "identity.user_required_value",
            "vault_users",
            r"SELECT CAST(id AS TEXT) AS entity FROM vault_users
              WHERE trim(issuer) = '' OR trim(subject) = '' OR trim(name) = '' ORDER BY id",
            "user issuer, subject, and name must be nonblank",
            "Restore a complete identity record from the identity provider or backup.",
        ),
        (
            "identity.group_name_blank",
            "vault_groups",
            r"SELECT CAST(id AS TEXT) AS entity FROM vault_groups WHERE trim(name) = '' ORDER BY id",
            "group name is blank",
            "Restore a nonblank group identity.",
        ),
        (
            "identity.group_name_case_collision",
            "vault_groups",
            r"SELECT CAST(g.id AS TEXT) AS entity FROM vault_groups g
              WHERE EXISTS (SELECT 1 FROM vault_groups other
                            WHERE other.id != g.id AND lower(other.name) = lower(g.name))
              ORDER BY g.id",
            "group name collides under case-insensitive administrative lookup",
            "Rename or consolidate colliding groups after reviewing memberships and permissions.",
        ),
        (
            "permission.implication_invalid",
            "folder_permissions",
            r"SELECT CAST(id AS TEXT) AS entity FROM folder_permissions
              WHERE can_write > can_read OR can_read > can_view ORDER BY id",
            "permission does not satisfy write implies read implies view",
            "Restore a monotonic view/read/write permission tuple.",
        ),
    ] {
        report_row_query(
            connection,
            report,
            CHECK_ACCESS,
            table,
            code,
            Severity::Error,
            query,
            evidence,
            remediation,
        )
        .await;
    }

    match sqlx::query(
        "SELECT id, icon FROM folders WHERE icon IS NOT NULL ORDER BY id LIMIT 1000000",
    )
    .fetch_all(&mut *connection)
    .await
    {
        Ok(rows) => {
            for row in rows {
                let id = row.try_get::<i64, _>("id").unwrap_or_default();
                let icon = row.try_get::<String, _>("icon").unwrap_or_default();
                let first_valid = icon
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
                let remaining_valid = icon
                    .bytes()
                    .skip(1)
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
                if icon.is_empty()
                    || icon.len() > 64
                    || icon.trim() != icon
                    || !first_valid
                    || !remaining_valid
                {
                    finding(
                        report,
                        CHECK_ACCESS,
                        "folder.icon_noncanonical",
                        Severity::Warning,
                        Some(row_entity("folders", id.to_string())),
                        "folder icon is not a canonical lowercase icon identifier",
                        "Resave or clear the folder icon.",
                    );
                }
            }
        }
        Err(error) => incomplete(
            report,
            CHECK_ACCESS,
            "folder.icon_scan_failed",
            format!("could not inspect folder icons: {error}"),
        ),
    }

    let folder_ids = id_set(connection, "folders").await;
    let document_ids = id_set(connection, "documents").await;
    match sqlx::query("SELECT id, preferences FROM vault_users ORDER BY id LIMIT 1000000")
        .fetch_all(&mut *connection)
        .await
    {
        Ok(rows) => {
            for row in rows {
                let id = row.try_get::<i64, _>("id").unwrap_or_default();
                let Ok(raw) = row.try_get::<String, _>("preferences") else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                let normalized = crate::preferences::normalize_user_preferences(&value);
                if normalized != value {
                    finding(
                        report,
                        CHECK_ACCESS,
                        "identity.preferences_noncanonical",
                        Severity::Warning,
                        Some(row_entity("vault_users", id.to_string())),
                        "stored preferences contain values the runtime ignores, clamps, or normalizes",
                        "Resave preferences through the application to store their canonical form.",
                    );
                }
                if let Some(favorites) = value.get("favoriteItems").and_then(Value::as_array) {
                    for favorite in favorites {
                        let Some(kind) = favorite.get("type").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(target_id) = favorite.get("id").and_then(Value::as_i64) else {
                            continue;
                        };
                        let exists = match kind {
                            "folder" => folder_ids
                                .as_ref()
                                .is_some_and(|ids| ids.contains(&target_id)),
                            "document" => document_ids
                                .as_ref()
                                .is_some_and(|ids| ids.contains(&target_id)),
                            _ => true,
                        };
                        if !exists {
                            finding(
                                report,
                                CHECK_ACCESS,
                                "identity.favorite_dangling",
                                Severity::Warning,
                                Some(row_entity("vault_users", id.to_string())),
                                format!("favorite references missing {kind} {target_id}"),
                                "Remove the dangling favorite by resaving user preferences.",
                            );
                        }
                    }
                }
            }
        }
        Err(error) => incomplete(
            report,
            CHECK_ACCESS,
            "identity.preferences_scan_failed",
            format!("could not inspect user preferences: {error}"),
        ),
    }

    match sqlx::query("SELECT key, value FROM vault_settings ORDER BY key LIMIT 1000000")
        .fetch_all(&mut *connection)
        .await
    {
        Ok(rows) => {
            for row in rows {
                let key = row.try_get::<String, _>("key").unwrap_or_default();
                let raw = row.try_get::<String, _>("value").unwrap_or_default();
                let value = serde_json::from_str::<Value>(&raw);
                match key.as_str() {
                    "archivePermanentDeleteAdminOnly" | "customDownloadStreamingEnabled" => {
                        if !value.as_ref().is_ok_and(Value::is_boolean) {
                            finding(
                                report,
                                CHECK_ACCESS,
                                "settings.known_value_type",
                                Severity::Error,
                                Some(row_entity("vault_settings", bounded(&key))),
                                "known setting value is not a JSON boolean",
                                "Resave the setting through the administrative API.",
                            );
                        }
                    }
                    _ => finding(
                        report,
                        CHECK_ACCESS,
                        "settings.unknown_key",
                        Severity::Info,
                        Some(row_entity("vault_settings", bounded(&key))),
                        "setting key is not recognized by this Vault version",
                        "Confirm the setting belongs to a compatible newer version before removing it.",
                    ),
                }
            }
        }
        Err(error) => incomplete(
            report,
            CHECK_ACCESS,
            "settings.scan_failed",
            format!("could not inspect site settings: {error}"),
        ),
    }
}

async fn check_documents(connection: &mut SqliteConnection, report: &mut ReportBuilder) {
    match sqlx::query("SELECT id, name FROM documents ORDER BY id LIMIT 1000000")
        .fetch_all(&mut *connection)
        .await
    {
        Ok(rows) => {
            for row in rows {
                let id = row.try_get::<i64, _>("id").unwrap_or_default();
                let name = row.try_get::<String, _>("name").unwrap_or_default();
                if !canonical_name(&name) {
                    finding(
                        report,
                        CHECK_DOCUMENTS,
                        "document.name_noncanonical",
                        Severity::Error,
                        Some(row_entity("documents", id.to_string())),
                        "document name is empty, untrimmed, reserved, or contains a separator/control character",
                        "Rename the document through reviewed recovery tooling.",
                    );
                }
            }
        }
        Err(error) => incomplete(
            report,
            CHECK_DOCUMENTS,
            "document.name_scan_failed",
            format!("could not inspect document names: {error}"),
        ),
    }

    let document_rules = [
        (
            "document.folder_shape",
            r"SELECT CAST(d.id AS TEXT) AS entity FROM documents d
              LEFT JOIN folders f ON f.id = d.folder_id
              WHERE f.id IS NULL OR f.root_key NOT IN ('vault', 'archive') ORDER BY d.id",
            "document has no supported folder placement",
            "Restore the document's original Vault folder relationship.",
        ),
        (
            "document.physical_archive_root",
            r"SELECT CAST(d.id AS TEXT) AS entity FROM documents d
              JOIN folders f ON f.id = d.folder_id WHERE f.root_key = 'archive' ORDER BY d.id",
            "document is physically stored under the reserved Archive root",
            "Restore lifecycle archive metadata under the Vault root instead of physical Archive placement.",
        ),
        (
            "document.archive_metadata_incomplete",
            r"SELECT CAST(d.id AS TEXT) AS entity FROM documents d
              LEFT JOIN folders f ON f.id = d.folder_id
              WHERE (d.archived_at IS NULL AND
                     (d.archived_origin_path IS NOT NULL OR d.archived_access IS NOT NULL))
                 OR (d.archived_at IS NOT NULL AND
                     (d.archived_origin_path IS NULL OR trim(d.archived_origin_path) = ''
                      OR d.archived_access IS NULL OR f.root_key != 'vault'))
              ORDER BY d.id",
            "document archive metadata is incomplete or attached to an unsupported placement",
            "Restore a complete archive timestamp, origin path, and access snapshot, or clear the entire lifecycle tuple.",
        ),
        (
            "document.retention_invalid",
            r"SELECT CAST(id AS TEXT) AS entity FROM documents
              WHERE (expires_at IS NULL) != (expiry_action IS NULL)
                 OR (expiry_action IS NOT NULL AND lower(trim(expiry_action)) NOT IN ('archive', 'delete'))
              ORDER BY id",
            "document expiration fields do not form a supported retention pair",
            "Restore both expiration fields together with archive or delete as the action.",
        ),
        (
            "document.version_empty_state",
            r"SELECT CAST(d.id AS TEXT) AS entity FROM documents d
              WHERE NOT EXISTS (SELECT 1 FROM document_versions v WHERE v.document_id = d.id)
                AND (d.current_version_id IS NOT NULL OR d.latest_version_number IS NOT NULL OR d.version_count != 0)
              ORDER BY d.id",
            "document has no versions but retains current/latest/count metadata",
            "Restore all empty-version cached fields to their null/zero state.",
        ),
        (
            "document.version_sequence",
            r"SELECT CAST(document_id AS TEXT) AS entity FROM document_versions
              GROUP BY document_id
              HAVING MIN(version_number) != 1
                  OR MAX(version_number) != COUNT(*)
                  OR COUNT(DISTINCT version_number) != COUNT(*)
                  OR MIN(version_number) < 1
              ORDER BY document_id",
            "document version numbers are not a contiguous 1..N sequence",
            "Reconstruct version ordering from backup before changing current-version metadata.",
        ),
        (
            "document.current_version_mismatch",
            r"SELECT CAST(d.id AS TEXT) AS entity FROM documents d
              LEFT JOIN document_versions current
                ON current.id = d.current_version_id AND current.document_id = d.id
              WHERE EXISTS (SELECT 1 FROM document_versions v WHERE v.document_id = d.id)
                AND (current.id IS NULL OR current.version_number !=
                     (SELECT MAX(v.version_number) FROM document_versions v WHERE v.document_id = d.id))
              ORDER BY d.id",
            "current version is missing, belongs to another document, or is not the latest version",
            "Restore the current version pointer from a verified version history.",
        ),
        (
            "document.version_cache_mismatch",
            r"SELECT CAST(d.id AS TEXT) AS entity FROM documents d
              WHERE EXISTS (SELECT 1 FROM document_versions v WHERE v.document_id = d.id)
                AND (d.version_count != (SELECT COUNT(*) FROM document_versions v WHERE v.document_id = d.id)
                  OR d.latest_version_number IS NULL
                  OR d.latest_version_number !=
                     (SELECT MAX(v.version_number) FROM document_versions v WHERE v.document_id = d.id))
              ORDER BY d.id",
            "cached version count or latest version number disagrees with version rows",
            "Recompute cached document version metadata from verified version rows.",
        ),
        (
            "document.version_required_value",
            r"SELECT id AS entity FROM document_versions
              WHERE id IS NULL OR trim(id) = '' OR committed_by IS NULL
                 OR trim(committed_by) = '' OR version_number < 1 ORDER BY id",
            "version has a blank ID/actor or a nonpositive version number",
            "Restore the required immutable version metadata from backup.",
        ),
    ];
    for (code, query, evidence, remediation) in document_rules {
        let table = if code == "document.version_required_value" {
            "document_versions"
        } else {
            "documents"
        };
        report_row_query(
            connection,
            report,
            CHECK_DOCUMENTS,
            table,
            code,
            Severity::Error,
            query,
            evidence,
            remediation,
        )
        .await;
    }

    for (code, severity, table, query, evidence, remediation) in [
        (
            "document.lock_state_invalid",
            Severity::Error,
            "document_locks",
            r"SELECT CAST(id AS TEXT) AS entity FROM document_locks
              WHERE (is_active = 1 AND (released_at IS NOT NULL OR released_by IS NOT NULL))
                 OR (is_active = 0 AND released_at IS NULL)
              ORDER BY id",
            "lock active/released fields form an invalid state",
            "Restore a coherent active or released lock state.",
        ),
        (
            "document.lock_actor_blank",
            Severity::Error,
            "document_locks",
            r"SELECT CAST(id AS TEXT) AS entity FROM document_locks WHERE trim(locked_by) = '' ORDER BY id",
            "lock owner is blank",
            "Restore the lock owner identity or release the invalid lock through reviewed recovery tooling.",
        ),
        (
            "document.event_required_value",
            Severity::Warning,
            "document_events",
            r"SELECT CAST(id AS TEXT) AS entity FROM document_events
              WHERE trim(event_type) = '' OR trim(actor) = '' ORDER BY id",
            "historical document event has a blank type or actor",
            "Restore audit metadata from backup if historical attribution is required.",
        ),
        (
            "folder.event_type_blank",
            Severity::Warning,
            "folder_events",
            r"SELECT CAST(id AS TEXT) AS entity FROM folder_events WHERE trim(event_type) = '' ORDER BY id",
            "historical folder event has a blank type",
            "Restore audit metadata from backup if historical attribution is required.",
        ),
    ] {
        report_row_query(
            connection,
            report,
            CHECK_DOCUMENTS,
            table,
            code,
            severity,
            query,
            evidence,
            remediation,
        )
        .await;
    }
}

async fn check_blob_graph(
    connection: &mut SqliteConnection,
    report: &mut ReportBuilder,
    inventory: &mut DatabaseInventory,
) {
    let blobs_result =
        sqlx::query("SELECT id, hash_algo, hash, size_bytes FROM blobs ORDER BY id LIMIT 1000000")
            .fetch_all(&mut *connection)
            .await;
    let blobs_complete = match blobs_result {
        Ok(rows) => {
            let mut complete = true;
            for row in rows {
                if let (Ok(id), Ok(hash_algo), Ok(hash), Ok(size_bytes)) = (
                    row.try_get::<i64, _>("id"),
                    row.try_get::<String, _>("hash_algo"),
                    row.try_get::<String, _>("hash"),
                    row.try_get::<i64, _>("size_bytes"),
                ) {
                    inventory.blobs.insert(
                        id,
                        BlobRecord {
                            id,
                            hash_algo,
                            hash,
                            size_bytes,
                        },
                    );
                } else {
                    complete = false;
                    finding(
                        report,
                        CHECK_BLOBS,
                        "blob.row_malformed",
                        Severity::Error,
                        None,
                        "a blob row could not be decoded",
                        "Restore the blob metadata from a verified backup.",
                    );
                }
            }
            complete
        }
        Err(error) => {
            incomplete(
                report,
                CHECK_BLOBS,
                "blob.inventory_unavailable",
                format!("blob metadata could not be inventoried: {error}"),
            );
            false
        }
    };

    let locations_complete = match sqlx::query(
        "SELECT id, blob_id, backend, bucket, object_key, created_at \
         FROM blob_locations ORDER BY id LIMIT 1000000",
    )
    .fetch_all(&mut *connection)
    .await
    {
        Ok(rows) => {
            let mut complete = true;
            for row in rows {
                let decoded = (
                    row.try_get::<i64, _>("id"),
                    row.try_get::<i64, _>("blob_id"),
                    row.try_get::<String, _>("backend"),
                    row.try_get::<String, _>("bucket"),
                    row.try_get::<String, _>("object_key"),
                    row.try_get::<String, _>("created_at"),
                );
                if let (
                    Ok(id),
                    Ok(blob_id),
                    Ok(backend),
                    Ok(bucket),
                    Ok(object_key),
                    Ok(created_at),
                ) = decoded
                {
                    inventory.locations.push(BlobLocationRecord {
                        id,
                        blob_id,
                        backend,
                        bucket,
                        object_key,
                        created_at,
                    });
                } else {
                    complete = false;
                    finding(
                        report,
                        CHECK_BLOBS,
                        "blob.location_row_malformed",
                        Severity::Error,
                        None,
                        "a blob location row could not be decoded",
                        "Restore location metadata from a verified backup.",
                    );
                }
            }
            complete
        }
        Err(error) => {
            incomplete(
                report,
                CHECK_BLOBS,
                "blob.location_inventory_unavailable",
                format!("blob locations could not be inventoried: {error}"),
            );
            false
        }
    };

    let mut references_complete = true;
    for (kind, query) in [
        (
            LiveReferenceKind::DocumentVersion,
            "SELECT blob_id FROM document_versions ORDER BY id LIMIT 1000000",
        ),
        (
            LiveReferenceKind::ExportArtifact,
            "SELECT blob_id FROM export_artifacts ORDER BY id LIMIT 1000000",
        ),
        (
            LiveReferenceKind::PreviewRendition,
            "SELECT blob_id FROM preview_renditions ORDER BY id LIMIT 1000000",
        ),
    ] {
        match sqlx::query_scalar::<_, i64>(query)
            .fetch_all(&mut *connection)
            .await
        {
            Ok(blob_ids) => {
                for blob_id in blob_ids {
                    inventory
                        .live_references
                        .entry(blob_id)
                        .or_default()
                        .insert(kind);
                }
            }
            Err(error) => {
                references_complete = false;
                incomplete(
                    report,
                    CHECK_BLOBS,
                    "blob.reference_inventory_unavailable",
                    format!("a durable blob reference table could not be read: {error}"),
                );
            }
        }
    }
    inventory.complete_for_storage = blobs_complete && locations_complete && references_complete;

    for blob in inventory.blobs.values() {
        let reservation = blob.hash_algo == "_vault_untracked_reservation";
        let valid = if reservation {
            blob.size_bytes == 0 && valid_simple_uuid(&blob.hash)
        } else {
            blob.hash_algo == "sha256" && blob.size_bytes >= 0 && canonical_sha256(&blob.hash)
        };
        if !valid {
            finding(
                report,
                CHECK_BLOBS,
                "blob.identity_invalid",
                Severity::Error,
                Some(row_entity("blobs", blob.id.to_string())),
                "blob algorithm, digest, or size is outside the supported persisted shape",
                "Restore the canonical SHA-256 metadata or a valid lifecycle reservation.",
            );
        }
    }

    for location in &inventory.locations {
        let lifecycle = parse_lifecycle_backend(&location.backend);
        if location.backend.trim().is_empty() || location.object_key.trim().is_empty() {
            finding(
                report,
                CHECK_BLOBS,
                "blob.location_required_value",
                Severity::Error,
                Some(row_entity("blob_locations", location.id.to_string())),
                "location backend or object key is blank",
                "Restore a complete durable object location.",
            );
        }
        if location.backend.starts_with("_vault_") && lifecycle.is_none() {
            finding(
                report,
                CHECK_BLOBS,
                "blob.lifecycle_backend_malformed",
                Severity::Error,
                Some(row_entity("blob_locations", location.id.to_string())),
                "reserved lifecycle backend is malformed",
                "Restore the exact pending/deleting lifecycle token and underlying backend.",
            );
        }
        if !location.backend.starts_with("_vault_") || lifecycle.is_some() {
            let underlying = lifecycle.map_or(location.backend.as_str(), |(_, backend)| backend);
            if !matches!(underlying, "local" | "s3" | "r2") {
                finding(
                    report,
                    CHECK_BLOBS,
                    "blob.location_backend_unsupported",
                    Severity::Error,
                    Some(row_entity("blob_locations", location.id.to_string())),
                    format!("location uses unsupported backend {}", bounded(underlying)),
                    "Restore a backend supported by this Vault version before trusting the location.",
                );
            }
        }
        if location
            .object_key
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
            || location.object_key.starts_with('/')
            || location.object_key.contains('\\')
        {
            finding(
                report,
                CHECK_BLOBS,
                "blob.location_key_unsafe",
                Severity::Error,
                Some(row_entity("blob_locations", location.id.to_string())),
                "location key contains an empty, relative, absolute, or backslash path component",
                "Restore the exact safe backend key from storage inventory or backup.",
            );
        }
    }

    let ordinary_locations = inventory
        .locations
        .iter()
        .filter(|location| parse_lifecycle_backend(&location.backend).is_none())
        .map(|location| location.blob_id)
        .collect::<HashSet<_>>();
    for (&blob_id, kinds) in &inventory.live_references {
        if !inventory.blobs.contains_key(&blob_id) {
            finding(
                report,
                CHECK_BLOBS,
                "blob.reference_missing_metadata",
                Severity::Error,
                Some(row_entity("blobs", blob_id.to_string())),
                format!("durable references {kinds:?} point to missing blob metadata"),
                "Restore the missing blob row and its verified locations from backup.",
            );
        } else if !ordinary_locations.contains(&blob_id) {
            finding(
                report,
                CHECK_BLOBS,
                "blob.live_without_location",
                Severity::Error,
                Some(row_entity("blobs", blob_id.to_string())),
                "live blob has no non-lifecycle durable location",
                "Restore a verified serviceable location before attempting lifecycle cleanup.",
            );
        }
    }
    for blob in inventory.blobs.values() {
        if !inventory.live_references.contains_key(&blob.id)
            && blob.hash_algo != "_vault_untracked_reservation"
        {
            finding(
                report,
                CHECK_BLOBS,
                "blob.unreferenced_metadata",
                Severity::Info,
                Some(row_entity("blobs", blob.id.to_string())),
                "blob has no durable document, export, or preview reference",
                "Allow normal garbage collection to assess it; the integrity check will not delete it.",
            );
        }
    }

    for (code, severity, table, query, evidence, remediation) in [
        (
            "blob.conflicting_size",
            Severity::Error,
            "blobs",
            r"SELECT CAST(b.id AS TEXT) AS entity FROM blobs b
              WHERE EXISTS (SELECT 1 FROM blobs other
                            WHERE other.id != b.id AND other.hash_algo = b.hash_algo
                              AND other.hash = b.hash AND other.size_bytes != b.size_bytes)
              ORDER BY b.id",
            "rows with the same algorithm and digest disagree about content size",
            "Verify the physical content and restore one canonical size from backup.",
        ),
        (
            "blob.export_metadata_mismatch",
            Severity::Error,
            "export_artifacts",
            r"SELECT CAST(a.id AS TEXT) AS entity FROM export_artifacts a
              LEFT JOIN blobs b ON b.id = a.blob_id
              LEFT JOIN export_jobs j ON j.id = a.job_id
              WHERE b.id IS NULL OR j.id IS NULL
                 OR a.hash_algo != b.hash_algo OR a.hash != b.hash
                 OR a.size_bytes != b.size_bytes OR a.mime_type != 'application/zip'
                 OR a.filename != j.filename OR a.expires_at != j.expires_at
              ORDER BY a.id",
            "export artifact metadata disagrees with its job, blob, or ZIP MIME type",
            "Restore artifact metadata from the verified blob and export result.",
        ),
        (
            "blob.pending_stale",
            Severity::Warning,
            "blob_locations",
            r"SELECT CAST(id AS TEXT) AS entity FROM blob_locations
              WHERE backend GLOB '_vault_pending:*'
                AND datetime(created_at) < datetime('now', '-1 hour') ORDER BY id",
            "pending publication has not been refreshed for at least one hour",
            "Stop Vault and use reviewed recovery tooling after verifying the referenced object.",
        ),
    ] {
        report_row_query(
            connection,
            report,
            CHECK_BLOBS,
            table,
            code,
            severity,
            query,
            evidence,
            remediation,
        )
        .await;
    }

    for blob in inventory
        .blobs
        .values()
        .filter(|blob| blob.hash_algo == "_vault_untracked_reservation")
    {
        let locations = inventory
            .locations
            .iter()
            .filter(|location| location.blob_id == blob.id)
            .collect::<Vec<_>>();
        let valid_location = locations.first().is_some_and(|location| {
            location.backend.starts_with("_vault_deleting:")
                && parse_lifecycle_backend(&location.backend).is_some_and(|(token, backend)| {
                    token == blob.hash && matches!(backend, "local" | "s3" | "r2")
                })
        });
        let valid = !inventory.live_references.contains_key(&blob.id)
            && locations.len() == 1
            && valid_location;
        if !valid {
            finding(
                report,
                CHECK_BLOBS,
                "blob.untracked_reservation_shape",
                Severity::Error,
                Some(row_entity("blobs", blob.id.to_string())),
                "untracked-object reservation is referenced or lacks exactly one deleting location",
                "Reconstruct the lifecycle transition from storage and database backups.",
            );
        }
    }
}

async fn check_transfers(
    connection: &mut SqliteConnection,
    report: &mut ReportBuilder,
    inventory: &mut DatabaseInventory,
) {
    let uploads_complete = match sqlx::query(
        "SELECT id, mode, status, filename, total_size, chunk_size, part_count \
         FROM upload_sessions ORDER BY id LIMIT 1000000",
    )
    .fetch_all(&mut *connection)
    .await
    {
        Ok(rows) => {
            let mut complete = true;
            for row in rows {
                let decoded = (
                    row.try_get::<String, _>("id"),
                    row.try_get::<String, _>("mode"),
                    row.try_get::<String, _>("status"),
                    row.try_get::<String, _>("filename"),
                    row.try_get::<i64, _>("total_size"),
                    row.try_get::<i64, _>("chunk_size"),
                    row.try_get::<i64, _>("part_count"),
                );
                if let (
                    Ok(id),
                    Ok(mode),
                    Ok(status),
                    Ok(filename),
                    Ok(total_size),
                    Ok(chunk_size),
                    Ok(part_count),
                ) = decoded
                {
                    if !safe_identifier(&id) {
                        finding(
                            report,
                            CHECK_TRANSFERS,
                            "upload.id_unsafe",
                            Severity::Error,
                            Some(row_entity("upload_sessions", bounded(&id))),
                            "upload session ID cannot be mapped safely to its scratch directory",
                            "Quarantine the database row and matching scratch data after backup.",
                        );
                    }
                    if !crate::documents::normalize_file_name(&filename)
                        .is_ok_and(|normalized| normalized == filename)
                    {
                        finding(
                            report,
                            CHECK_TRANSFERS,
                            "upload.filename_unsafe",
                            Severity::Error,
                            Some(row_entity("upload_sessions", bounded(&id))),
                            "upload filename is unsafe or is not in its canonical normalized form",
                            "Restore a single safe filename without path separators, surrounding whitespace, or control characters.",
                        );
                    }
                    inventory.upload_sessions.push(UploadSessionRecord {
                        id,
                        mode,
                        status,
                        total_size,
                        chunk_size,
                        part_count,
                    });
                } else {
                    complete = false;
                    finding(
                        report,
                        CHECK_TRANSFERS,
                        "upload.row_malformed",
                        Severity::Error,
                        None,
                        "an upload session row could not be decoded",
                        "Restore transfer metadata from backup.",
                    );
                }
            }
            complete
        }
        Err(error) => {
            incomplete(
                report,
                CHECK_TRANSFERS,
                "upload.inventory_unavailable",
                format!("upload sessions could not be inventoried: {error}"),
            );
            false
        }
    };

    let exports_complete = match sqlx::query(
        "SELECT id, status, filename FROM export_jobs ORDER BY id LIMIT 1000000",
    )
    .fetch_all(&mut *connection)
    .await
    {
        Ok(rows) => {
            let mut complete = true;
            for row in rows {
                if let (Ok(id), Ok(status), Ok(filename)) = (
                    row.try_get::<String, _>("id"),
                    row.try_get::<String, _>("status"),
                    row.try_get::<String, _>("filename"),
                ) {
                    if !safe_identifier(&id) {
                        finding(
                            report,
                            CHECK_TRANSFERS,
                            "export.id_unsafe",
                            Severity::Error,
                            Some(row_entity("export_jobs", bounded(&id))),
                            "export job ID cannot be mapped safely to its temporary file",
                            "Quarantine the database row and matching scratch data after backup.",
                        );
                    }
                    inventory.export_jobs.push(ExportJobRecord {
                        id,
                        status,
                        filename,
                    });
                } else {
                    complete = false;
                    finding(
                        report,
                        CHECK_TRANSFERS,
                        "export.row_malformed",
                        Severity::Error,
                        None,
                        "an export job row could not be decoded",
                        "Restore transfer metadata from backup.",
                    );
                }
            }
            complete
        }
        Err(error) => {
            incomplete(
                report,
                CHECK_TRANSFERS,
                "export.inventory_unavailable",
                format!("export jobs could not be inventoried: {error}"),
            );
            false
        }
    };
    inventory.complete_for_transfers = uploads_complete && exports_complete;
    inventory.complete_for_storage &= inventory.complete_for_transfers;

    let upload_rules = [
        (
            "upload.mode_status_invalid",
            r"SELECT id AS entity FROM upload_sessions
              WHERE mode NOT IN ('create', 'checkin')
                 OR status NOT IN ('active', 'completing', 'complete', 'failed', 'aborted', 'expired')
              ORDER BY id",
            "upload mode or status is unsupported",
            "Restore a state supported by this Vault version.",
        ),
        (
            "upload.geometry_invalid",
            r"SELECT id AS entity FROM upload_sessions
              WHERE total_size < 0 OR chunk_size <= 0 OR part_count < 0 OR part_count > 1024
                 OR part_count != CASE WHEN total_size = 0 THEN 0
                                      ELSE ((total_size - 1) / chunk_size) + 1 END
              ORDER BY id",
            "upload size, chunk size, or part count is not canonical",
            "Recreate the transfer using canonical chunk geometry.",
        ),
        (
            "upload.create_target_invalid",
            r"SELECT u.id AS entity FROM upload_sessions u
              LEFT JOIN folders f ON f.id = u.target_folder_id
              WHERE u.mode = 'create' AND u.status IN ('active', 'completing')
                AND (f.id IS NULL OR f.root_key != 'vault' OR f.archived_at IS NOT NULL)
              ORDER BY u.id",
            "active create upload lacks a live Vault target folder",
            "Abort and restart the upload against the intended current folder identity.",
        ),
        (
            "upload.checkin_target_invalid",
            r"SELECT u.id AS entity FROM upload_sessions u
              LEFT JOIN documents d ON d.id = u.document_id
              WHERE u.mode = 'checkin' AND u.status IN ('active', 'completing')
                AND (d.id IS NULL OR u.target_folder_id IS NOT NULL)
              ORDER BY u.id",
            "active check-in upload has an invalid document/target-folder shape",
            "Abort and restart the check-in against the intended document.",
        ),
        (
            "upload.checkin_lock_missing",
            r"SELECT u.id AS entity FROM upload_sessions u
              WHERE u.mode = 'checkin' AND u.status IN ('active', 'completing')
                AND NOT EXISTS (SELECT 1 FROM document_locks l
                                WHERE l.document_id = u.document_id AND l.is_active = 1
                                  AND l.locked_by = u.created_by)
              ORDER BY u.id",
            "active check-in upload has no matching active lock owned by its creator",
            "Re-establish ownership only after confirming no competing editor exists, or abort the session.",
        ),
        (
            "upload.progress_invalid",
            r"SELECT id AS entity FROM upload_sessions
              WHERE verification_total_bytes < 0 OR verification_processed_bytes < 0
                 OR verification_processed_bytes > verification_total_bytes
                 OR verification_total_bytes > total_size
              ORDER BY id",
            "upload verification progress is outside its byte bounds",
            "Restore progress from verified scratch parts or restart the upload.",
        ),
        (
            "upload.active_state_invalid",
            r"SELECT id AS entity FROM upload_sessions
              WHERE status = 'active' AND
                (verification_total_bytes != 0 OR verification_processed_bytes != 0
                 OR completed_at IS NOT NULL OR aborted_at IS NOT NULL OR error IS NOT NULL
                 OR result_document_id IS NOT NULL OR result_version_id IS NOT NULL OR result_path IS NOT NULL)
              ORDER BY id",
            "active upload claims verification, terminal, error, or result metadata",
            "Restart or reconstruct the session state from its verified scratch files.",
        ),
        (
            "upload.terminal_fields_invalid",
            r"SELECT id AS entity FROM upload_sessions
              WHERE status IN ('completing', 'complete', 'failed', 'aborted', 'expired') AND
                ((status != 'complete' AND
                    (completed_at IS NOT NULL OR result_document_id IS NOT NULL
                     OR result_version_id IS NOT NULL OR result_path IS NOT NULL))
                 OR (status != 'aborted' AND aborted_at IS NOT NULL)
                 OR (status != 'failed' AND error IS NOT NULL))
              ORDER BY id",
            "upload status contradicts its completion, abort, error, or result fields",
            "Restore a single coherent terminal state from logs, committed document metadata, and verified scratch data.",
        ),
        (
            "upload.complete_state_invalid",
            r"SELECT u.id AS entity FROM upload_sessions u
              LEFT JOIN documents d ON d.id = u.result_document_id
              LEFT JOIN document_versions v
                ON v.id = u.result_version_id AND v.document_id = u.result_document_id
              WHERE u.status = 'complete' AND
                (u.completed_at IS NULL OR u.result_document_id IS NULL
                 OR u.result_version_id IS NULL OR u.result_path IS NULL
                 OR u.verification_total_bytes != u.total_size
                 OR u.verification_processed_bytes != u.total_size
                 OR d.id IS NULL OR v.id IS NULL)
              ORDER BY u.id",
            "completed upload lacks full verification or a coherent document/version result",
            "Restore result metadata from the committed document version and verified blob.",
        ),
        (
            "upload.complete_manifest_digest_invalid",
            r"SELECT id AS entity FROM upload_sessions
              WHERE status = 'complete' AND CASE
                WHEN json_valid(user_context) = 0 THEN 1
                ELSE COALESCE(json_type(user_context, '$._upload_part_manifest_sha256'), '') != 'text'
                  OR length(json_extract(user_context, '$._upload_part_manifest_sha256')) != 64
                  OR lower(json_extract(user_context, '$._upload_part_manifest_sha256'))
                     != json_extract(user_context, '$._upload_part_manifest_sha256')
                  OR json_extract(user_context, '$._upload_part_manifest_sha256')
                     GLOB '*[^0-9a-f]*'
                END
              ORDER BY id",
            "completed upload lacks its canonical persisted part-manifest digest",
            "Restore the digest from verified upload-part metadata and committed content.",
        ),
        (
            "upload.failed_state_invalid",
            r"SELECT id AS entity FROM upload_sessions
              WHERE status = 'failed' AND (error IS NULL OR trim(error) = '') ORDER BY id",
            "failed upload has no error detail",
            "Restore the recorded failure reason if available.",
        ),
        (
            "upload.aborted_state_invalid",
            r"SELECT id AS entity FROM upload_sessions
              WHERE status = 'aborted' AND aborted_at IS NULL ORDER BY id",
            "aborted upload has no aborted timestamp",
            "Restore the abort timestamp from logs or backup.",
        ),
        (
            "upload.legacy_part_invalid",
            r"SELECT CAST(p.id AS TEXT) AS entity FROM upload_parts p
              LEFT JOIN upload_sessions u ON u.id = p.session_id
              WHERE u.id IS NULL OR p.part_number < 1 OR p.part_number > u.part_count
                 OR p.offset_bytes != (p.part_number - 1) * u.chunk_size
                 OR p.size_bytes < 0 OR p.size_bytes > u.chunk_size
                 OR length(p.sha256) != 64 OR lower(p.sha256) != p.sha256
                 OR p.sha256 GLOB '*[^0-9a-f]*'
                 OR trim(p.storage_path) = '' OR p.storage_path LIKE '/%'
                 OR p.storage_path LIKE '%..%' OR p.storage_path LIKE '%\\%'
              ORDER BY p.id",
            "legacy upload part metadata is outside session geometry or uses an unsafe path/digest",
            "Do not open the stored path; reconstruct the part row only from verified scratch data.",
        ),
    ];
    for (code, query, evidence, remediation) in upload_rules {
        let table = if code == "upload.legacy_part_invalid" {
            "upload_parts"
        } else {
            "upload_sessions"
        };
        report_row_query(
            connection,
            report,
            CHECK_TRANSFERS,
            table,
            code,
            Severity::Error,
            query,
            evidence,
            remediation,
        )
        .await;
    }

    let export_rules = [
        (
            "export.status_invalid",
            r"SELECT id AS entity FROM export_jobs
              WHERE status NOT IN ('queued', 'running', 'finalizing', 'complete', 'failed', 'cancelled')
              ORDER BY id",
            "export status is unsupported",
            "Restore a state supported by this Vault version.",
        ),
        (
            "export.progress_invalid",
            r"SELECT id AS entity FROM export_jobs
              WHERE total_items < 0 OR processed_items < 0 OR processed_items > total_items
                 OR total_bytes < 0 OR processed_bytes < 0 OR processed_bytes > total_bytes
              ORDER BY id",
            "export totals or progress are negative or out of bounds",
            "Restore progress from the verified job inputs or restart the export.",
        ),
        (
            "export.artifact_state_invalid",
            r"SELECT j.id AS entity FROM export_jobs j
              WHERE (j.status = 'complete' AND
                     ((SELECT COUNT(*) FROM export_artifacts a WHERE a.job_id = j.id) != 1
                      OR j.completed_at IS NULL
                      OR j.processed_items != j.total_items OR j.processed_bytes != j.total_bytes))
                 OR (j.status != 'complete' AND
                     EXISTS (SELECT 1 FROM export_artifacts a WHERE a.job_id = j.id))
              ORDER BY j.id",
            "export status, completion fields, and artifact count disagree",
            "Restore the job state from its one verified artifact or restart the export.",
        ),
        (
            "export.terminal_fields_invalid",
            r"SELECT id AS entity FROM export_jobs
              WHERE (status != 'complete' AND completed_at IS NOT NULL)
                 OR (status != 'cancelled' AND cancelled_at IS NOT NULL)
                 OR (status != 'failed' AND error IS NOT NULL)
              ORDER BY id",
            "export status contradicts its completion, cancellation, or error fields",
            "Restore a single coherent terminal state from logs and verified artifact metadata.",
        ),
        (
            "export.failed_state_invalid",
            r"SELECT id AS entity FROM export_jobs
              WHERE status = 'failed' AND (error IS NULL OR trim(error) = '') ORDER BY id",
            "failed export has no error detail",
            "Restore the recorded failure reason if available.",
        ),
        (
            "export.cancelled_state_invalid",
            r"SELECT id AS entity FROM export_jobs
              WHERE status = 'cancelled' AND cancelled_at IS NULL ORDER BY id",
            "cancelled export has no cancellation timestamp",
            "Restore the cancellation timestamp from logs or backup.",
        ),
    ];
    for (code, query, evidence, remediation) in export_rules {
        report_row_query(
            connection,
            report,
            CHECK_TRANSFERS,
            "export_jobs",
            code,
            Severity::Error,
            query,
            evidence,
            remediation,
        )
        .await;
    }

    report_row_query(
        connection,
        report,
        CHECK_TRANSFERS,
        "export_jobs",
        "export.interrupted_state",
        Severity::Warning,
        "SELECT id AS entity FROM export_jobs WHERE status IN ('running', 'finalizing') ORDER BY id",
        "export was interrupted in a worker-owned state",
        "Normal startup can requeue this job; the integrity check will not alter it.",
    )
    .await;
}

async fn check_previews(
    connection: &mut SqliteConnection,
    report: &mut ReportBuilder,
    inventory: &mut DatabaseInventory,
) {
    match sqlx::query(
        "SELECT id, source_blob_id, recipe, status FROM preview_jobs ORDER BY id LIMIT 1000000",
    )
    .fetch_all(&mut *connection)
    .await
    {
        Ok(rows) => {
            for row in rows {
                match (
                    row.try_get::<i64, _>("id"),
                    row.try_get::<i64, _>("source_blob_id"),
                    row.try_get::<String, _>("recipe"),
                    row.try_get::<String, _>("status"),
                ) {
                    (Ok(id), Ok(source_blob_id), Ok(recipe), Ok(status)) => {
                        inventory.preview_jobs.push(PreviewJobRecord {
                            id,
                            source_blob_id,
                            recipe,
                            status,
                        });
                    }
                    _ => finding(
                        report,
                        CHECK_PREVIEWS,
                        "preview.row_malformed",
                        Severity::Warning,
                        None,
                        "a preview job row could not be decoded",
                        "Discard and regenerate the derived preview after verifying its source blob.",
                    ),
                }
            }
        }
        Err(error) => incomplete(
            report,
            CHECK_PREVIEWS,
            "preview.inventory_unavailable",
            format!("preview jobs could not be inventoried: {error}"),
        ),
    }
    match sqlx::query(
        "SELECT id, preview_job_id, blob_id, variant, mime_type, width, height \
         FROM preview_renditions ORDER BY id LIMIT 1000000",
    )
    .fetch_all(&mut *connection)
    .await
    {
        Ok(rows) => {
            for row in rows {
                match (
                    row.try_get::<i64, _>("id"),
                    row.try_get::<i64, _>("preview_job_id"),
                    row.try_get::<i64, _>("blob_id"),
                    row.try_get::<String, _>("variant"),
                    row.try_get::<String, _>("mime_type"),
                    row.try_get::<i64, _>("width"),
                    row.try_get::<i64, _>("height"),
                ) {
                    (
                        Ok(id),
                        Ok(preview_job_id),
                        Ok(blob_id),
                        Ok(variant),
                        Ok(mime_type),
                        Ok(width),
                        Ok(height),
                    ) => inventory.preview_renditions.push(PreviewRenditionRecord {
                        id,
                        preview_job_id,
                        blob_id,
                        variant,
                        mime_type,
                        width,
                        height,
                    }),
                    _ => finding(
                        report,
                        CHECK_PREVIEWS,
                        "preview.rendition_row_malformed",
                        Severity::Warning,
                        None,
                        "a preview rendition row could not be decoded",
                        "Discard and regenerate the derived preview after verifying its source blob.",
                    ),
                }
            }
        }
        Err(error) => incomplete(
            report,
            CHECK_PREVIEWS,
            "preview.rendition_inventory_unavailable",
            format!("preview renditions could not be inventoried: {error}"),
        ),
    }
    for (code, query, evidence) in [
        (
            "preview.job_state_invalid",
            r"SELECT CAST(id AS TEXT) AS entity FROM preview_jobs
              WHERE status NOT IN ('queued', 'running', 'ready', 'unsupported', 'failed')
                 OR attempt_count < 0
                 OR (status = 'running' AND
                     (lease_token IS NULL OR trim(lease_token) = '' OR lease_expires_at IS NULL))
                 OR (status != 'running' AND
                     (lease_token IS NOT NULL OR lease_expires_at IS NOT NULL))
                 OR (status IN ('ready', 'unsupported') AND completed_at IS NULL)
                 OR (status IN ('queued', 'running') AND completed_at IS NOT NULL)
                 OR (status = 'ready' AND
                     (last_error_code IS NOT NULL OR last_error_detail IS NOT NULL
                      OR next_attempt_at IS NOT NULL))
                 OR (status = 'unsupported' AND
                     (last_error_code IS NULL OR trim(last_error_code) = ''
                      OR next_attempt_at IS NOT NULL))
                 OR (status = 'failed' AND
                     (last_error_code IS NULL OR trim(last_error_code) = ''))
              ORDER BY id",
            "preview status, attempts, or running lease are invalid",
        ),
        (
            "preview.raster_renditions_invalid",
            r"SELECT CAST(j.id AS TEXT) AS entity FROM preview_jobs j
              WHERE (j.status != 'ready' AND
                     EXISTS (SELECT 1 FROM preview_renditions r WHERE r.preview_job_id = j.id))
                 OR (
                    j.recipe IN ('raster-v1', 'raster-v2') AND j.status = 'ready' AND (
                       (SELECT COUNT(*) FROM preview_renditions r
                        WHERE r.preview_job_id = j.id) != 3
                       OR (SELECT COUNT(DISTINCT r.variant) FROM preview_renditions r
                           WHERE r.preview_job_id = j.id
                             AND r.variant IN ('small','medium','large')) != 3
                       OR EXISTS (
                           SELECT 1 FROM preview_renditions r
                           WHERE r.preview_job_id = j.id
                             AND (r.mime_type != 'image/webp' OR r.width <= 0 OR r.height <= 0
                               OR (r.variant = 'small' AND (r.width > 128 OR r.height > 128))
                               OR (r.variant = 'medium' AND (r.width > 256 OR r.height > 256))
                               OR (r.variant = 'large' AND (r.width > 512 OR r.height > 512)))
                       )
                    )
                 )
              ORDER BY j.id",
            "raster preview state and rendition set disagree",
        ),
    ] {
        report_row_query(
            connection,
            report,
            CHECK_PREVIEWS,
            "preview_jobs",
            code,
            Severity::Warning,
            query,
            evidence,
            "Discard and regenerate the derived preview after verifying its source blob.",
        )
        .await;
    }
}

async fn check_shares_and_state(connection: &mut SqliteConnection, report: &mut ReportBuilder) {
    for (code, severity, table, query, evidence, remediation) in [
        (
            "share.shape_invalid",
            Severity::Error,
            "share_links",
            r"SELECT CAST(id AS TEXT) AS entity FROM share_links
              WHERE length(code) NOT BETWEEN 8 AND 64 OR code GLOB '*[^A-Za-z0-9_-]*'
                 OR access_mode != 'internal'
                 OR lower(trim(target_type)) NOT IN ('document', 'file', 'folder')
                 OR (lower(trim(target_type)) IN ('document','file') AND
                     (document_id IS NULL OR folder_id IS NOT NULL))
                 OR (lower(trim(target_type)) = 'folder' AND
                     (folder_id IS NULL OR document_id IS NOT NULL))
                 OR ((item_type IS NULL) != (item_id IS NULL))
                 OR (item_type IS NOT NULL AND
                     (CASE lower(trim(item_type)) WHEN 'file' THEN 'document'
                           ELSE lower(trim(item_type)) END
                      != CASE lower(trim(target_type)) WHEN 'file' THEN 'document'
                           ELSE lower(trim(target_type)) END
                      OR item_id IS NULL
                      OR item_id != COALESCE(document_id, folder_id)))
              ORDER BY id",
            "share code, access mode, typed target, or legacy target fields are inconsistent",
            "Disable the share until its exact target can be restored safely.",
        ),
        (
            "state.resources_noncanonical",
            Severity::Warning,
            "state_events",
            "SELECT CAST(id AS TEXT) AS entity FROM state_events WHERE trim(event_type) = '' ORDER BY id",
            "state event type is blank",
            "Restore or compact state events through reviewed recovery tooling.",
        ),
    ] {
        report_row_query(
            connection,
            report,
            CHECK_SHARES_STATE,
            table,
            code,
            severity,
            query,
            evidence,
            remediation,
        )
        .await;
    }

    match sqlx::query(
        "SELECT id, event_type, resources FROM state_events ORDER BY id LIMIT 1000000",
    )
    .fetch_all(&mut *connection)
    .await
    {
        Ok(rows) => {
            for row in rows {
                let id = row.try_get::<i64, _>("id").unwrap_or_default();
                let event_type = row.try_get::<String, _>("event_type").unwrap_or_default();
                let resources = row.try_get::<String, _>("resources").unwrap_or_default();
                let Ok(values) = serde_json::from_str::<Vec<String>>(&resources) else {
                    continue;
                };
                let canonical = values
                    .iter()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let full = FULL_STATE_EVENT_RESOURCES
                    .iter()
                    .map(|value| (*value).to_string())
                    .collect::<Vec<_>>();
                if canonical != values || (event_type == "state.compacted" && values != full) {
                    finding(
                        report,
                        CHECK_SHARES_STATE,
                        "state.resources_noncanonical",
                        Severity::Warning,
                        Some(row_entity("state_events", id.to_string())),
                        "state resources are not nonblank, sorted, unique, and canonical for the event",
                        "Allow normal state compaction to replace the event after reviewing replay requirements.",
                    );
                }
            }
        }
        Err(error) => incomplete(
            report,
            CHECK_SHARES_STATE,
            "state.resources_scan_failed",
            format!("state-event resources could not be inspected: {error}"),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
async fn report_row_query(
    connection: &mut SqliteConnection,
    report: &mut ReportBuilder,
    check: &str,
    table: &str,
    code: &str,
    severity: Severity,
    query: &str,
    evidence: &str,
    remediation: &str,
) {
    let mut rows = sqlx::query(query).fetch(&mut *connection);
    loop {
        match rows.try_next().await {
            Ok(Some(row)) => {
                let entity = row
                    .try_get::<String, _>("entity")
                    .unwrap_or_else(|_| "unknown".to_string());
                finding(
                    report,
                    check,
                    code,
                    severity,
                    Some(row_entity(table, bounded(&entity))),
                    evidence,
                    remediation,
                );
            }
            Ok(None) => break,
            Err(error) => {
                incomplete(
                    report,
                    check,
                    format!("{code}.unavailable"),
                    format!("could not evaluate {code}: {error}"),
                );
                break;
            }
        }
    }
}

fn finding(
    report: &mut ReportBuilder,
    check: &str,
    code: impl Into<String>,
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
    code: impl Into<String>,
    evidence: impl Into<String>,
) {
    report.mark_incomplete(check, code, None, evidence);
}

fn row_entity(table: &str, id: impl std::fmt::Display) -> String {
    format!("{table}[{id}]")
}

fn bounded(value: &str) -> String {
    const MAX_CHARS: usize = 200;
    let mut result = value.chars().take(MAX_CHARS).collect::<String>();
    if value.chars().count() > MAX_CHARS {
        result.push('…');
    }
    result
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_simple_uuid(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn parse_lifecycle_backend(value: &str) -> Option<(&str, &str)> {
    let rest = value
        .strip_prefix("_vault_pending:")
        .or_else(|| value.strip_prefix("_vault_deleting:"))?;
    let (token, backend) = rest.split_once(':')?;
    (valid_simple_uuid(token) && safe_identifier(backend)).then_some((token, backend))
}

fn valid_timestamp(value: &str) -> bool {
    if time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).is_ok() {
        return true;
    }
    let Ok(description) = time::format_description::parse_borrowed::<1>(
        "[year]-[month]-[day] [hour]:[minute]:[second]",
    ) else {
        return false;
    };
    time::PrimitiveDateTime::parse(value, &description).is_ok()
}

async fn id_set(connection: &mut SqliteConnection, table: &str) -> Option<HashSet<i64>> {
    let query = format!(
        "SELECT id FROM {} ORDER BY id LIMIT 1000000",
        quote_identifier(table)
    );
    sqlx::query_scalar::<_, i64>(&query)
        .fetch_all(&mut *connection)
        .await
        .ok()
        .map(|values| values.into_iter().collect())
}
