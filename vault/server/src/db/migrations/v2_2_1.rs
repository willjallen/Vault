use std::collections::BTreeMap;
use std::fmt::Write as _;

use futures_util::future::BoxFuture;
use sqlx::{Sqlite, Transaction};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

use super::super::invariants::{self, RootInvariantDefinition};
use super::MigrationDefinition;

const ROW_BATCH_SIZE: i64 = 1_000;
const CANONICAL_TIMESTAMP_LENGTH: usize = 27;
const SOURCE_NAIVE_TIMESTAMP_FORMATS: [&str; 4] = [
    "[year]-[month]-[day] [hour]:[minute]:[second]",
    "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond]",
    "[year]-[month]-[day]T[hour]:[minute]:[second]",
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]",
];
const TARGET_ROOT_FOLDERS: [RootInvariantDefinition; 2] = [
    RootInvariantDefinition {
        key: "vault",
        stored_name: "",
        public_path_prefix: "",
        allows_folder_descendants: true,
    },
    RootInvariantDefinition {
        key: "archive",
        stored_name: "Archive",
        public_path_prefix: "Archive",
        allows_folder_descendants: false,
    },
];

pub const MIGRATION: MigrationDefinition = MigrationDefinition {
    version: 4,
    target_version: "2.2.1",
    name: "canonicalize UTC timestamps",
    apply: apply_boxed,
    validate_target: validate_target_boxed,
};

#[derive(Debug)]
struct TimestampTable {
    table: String,
    columns: Vec<String>,
    legacy_default_columns: Vec<String>,
}

async fn apply(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    let tables = timestamp_tables(tx).await?;
    for table in &tables {
        for column in &table.columns {
            normalize_column(tx, &table.table, column).await?;
        }
    }
    for table in &tables {
        install_timestamp_default_normalizer(tx, table).await?;
    }
    Ok(())
}

async fn timestamp_tables(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<Vec<TimestampTable>> {
    let columns = sqlx::query_as::<_, (String, String, Option<String>)>(
        r"
        SELECT schema_object.name, table_column.name, table_column.dflt_value
        FROM sqlite_schema schema_object
        JOIN pragma_table_info(schema_object.name) table_column
        WHERE schema_object.type = 'table'
          AND schema_object.name NOT LIKE 'sqlite_%'
          AND table_column.name GLOB '*_at'
        ORDER BY schema_object.name, table_column.cid
        ",
    )
    .fetch_all(&mut **tx)
    .await?;

    let mut by_table = BTreeMap::<String, (Vec<String>, Vec<String>)>::new();
    for (table, column, default_value) in columns {
        let entry = by_table.entry(table).or_default();
        entry.0.push(column.clone());
        if default_value
            .as_deref()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("CURRENT_TIMESTAMP"))
        {
            entry.1.push(column);
        }
    }
    anyhow::ensure!(
        !by_table.is_empty(),
        "current schema does not contain any timestamp columns"
    );
    Ok(by_table
        .into_iter()
        .map(
            |(table, (columns, legacy_default_columns))| TimestampTable {
                table,
                columns,
                legacy_default_columns,
            },
        )
        .collect())
}

async fn normalize_column(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    column: &str,
) -> anyhow::Result<()> {
    let table_identifier = quote_identifier(table);
    let column_identifier = quote_identifier(column);
    let mut after_rowid = None;
    loop {
        let query = after_rowid.map_or_else(
            || {
                format!(
                    "SELECT rowid, CAST({column_identifier} AS TEXT) \
                     FROM {table_identifier} WHERE {column_identifier} IS NOT NULL \
                     ORDER BY rowid LIMIT {ROW_BATCH_SIZE}"
                )
            },
            |_| {
                format!(
                    "SELECT rowid, CAST({column_identifier} AS TEXT) \
                     FROM {table_identifier} WHERE {column_identifier} IS NOT NULL \
                     AND rowid > ? ORDER BY rowid LIMIT {ROW_BATCH_SIZE}"
                )
            },
        );
        let rows = if let Some(rowid) = after_rowid {
            sqlx::query_as::<_, (i64, String)>(&query)
                .bind(rowid)
                .fetch_all(&mut **tx)
                .await?
        } else {
            sqlx::query_as::<_, (i64, String)>(&query)
                .fetch_all(&mut **tx)
                .await?
        };
        if rows.is_empty() {
            break;
        }

        for (rowid, value) in &rows {
            let canonical = canonicalize_source_timestamp(value).map_err(|error| {
                anyhow::anyhow!("cannot canonicalize {table}.{column} at rowid {rowid}: {error}")
            })?;
            if canonical != *value {
                let update = format!(
                    "UPDATE {table_identifier} SET {column_identifier} = ? WHERE rowid = ?"
                );
                sqlx::query(&update)
                    .bind(canonical)
                    .bind(rowid)
                    .execute(&mut **tx)
                    .await?;
            }
        }
        after_rowid = rows.last().map(|(rowid, _)| *rowid);
    }
    Ok(())
}

async fn install_timestamp_default_normalizer(
    tx: &mut Transaction<'_, Sqlite>,
    timestamp_table: &TimestampTable,
) -> anyhow::Result<()> {
    if timestamp_table.legacy_default_columns.is_empty() {
        return Ok(());
    }
    let table_identifier = quote_identifier(&timestamp_table.table);
    let legacy_defaults = timestamp_table
        .legacy_default_columns
        .iter()
        .map(|column| {
            let value = format!("NEW.{}", quote_identifier(column));
            format!("({})", legacy_sqlite_default_sql(&value))
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let assignments = timestamp_table
        .legacy_default_columns
        .iter()
        .map(|column| {
            let column_identifier = quote_identifier(column);
            let value = format!("NEW.{column_identifier}");
            format!(
                "{column_identifier} = CASE WHEN {} \
                 THEN substr({value}, 1, 10) || 'T' || substr({value}, 12, 8) || \
                 '.000000Z' ELSE {value} END",
                legacy_sqlite_default_sql(&value)
            )
        })
        .collect::<Vec<_>>()
        .join(",\n                ");
    let table_name = &timestamp_table.table;
    let trigger = format!(
        r"
        CREATE TRIGGER {trigger_name}
        AFTER INSERT ON {table_identifier}
        WHEN {legacy_defaults}
        BEGIN
            UPDATE {table_identifier}
            SET {assignments}
            WHERE rowid = NEW.rowid;
        END
        ",
        trigger_name = quote_identifier(&format!(
            "trg_{table_name}_timestamp_defaults_canonical_insert"
        )),
    );
    sqlx::query(&trigger).execute(&mut **tx).await?;
    Ok(())
}

fn legacy_sqlite_default_sql(value: &str) -> String {
    const LEGACY_GLOB: &str = concat!(
        "[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] ",
        "[0-9][0-9]:[0-9][0-9]:[0-9][0-9]"
    );
    format!(
        "length({value}) = 19 AND {value} GLOB '{LEGACY_GLOB}' \
         AND julianday({value}) IS NOT NULL"
    )
}

async fn validate_target(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    invariants::validate(tx, &TARGET_ROOT_FOLDERS, &[]).await?;
    validate_upload_target_identity(tx).await?;
    validate_archive_lifecycle(tx).await?;
    for table in timestamp_tables(tx).await? {
        for column in &table.columns {
            validate_column(tx, &table.table, column).await?;
        }
    }
    Ok(())
}

async fn validate_upload_target_identity(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    let invalid_active_create_uploads: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM upload_sessions
        WHERE mode = 'create'
          AND status IN ('active', 'completing')
          AND target_folder_id IS NULL
        ",
    )
    .fetch_one(&mut **tx)
    .await?;
    anyhow::ensure!(
        invalid_active_create_uploads == 0,
        "upload_target_invariant_failed reason=missing_target_folder \
         active_create_uploads={invalid_active_create_uploads}"
    );
    Ok(())
}

async fn validate_archive_lifecycle(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    let invalid_documents: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM documents d
        JOIN folders f ON f.id = d.folder_id
        WHERE
            (
                d.archived_at IS NULL
                AND (
                    d.archived_origin_path IS NOT NULL
                    OR d.archived_access IS NOT NULL
                )
            )
            OR (
                d.archived_at IS NOT NULL
                AND (
                    d.archived_origin_path IS NULL
                    OR d.archived_access IS NULL
                    OR f.root_key != 'vault'
                )
            )
        ",
    )
    .fetch_one(&mut **tx)
    .await?;
    anyhow::ensure!(
        invalid_documents == 0,
        "archive_invariant_failed invalid_documents={invalid_documents}"
    );

    let invalid_folders: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM folders
        WHERE
            (
                archived_at IS NULL
                AND (
                    archived_origin_path IS NOT NULL
                    OR archived_access IS NOT NULL
                )
            )
            OR (
                archived_at IS NOT NULL
                AND (
                    archived_origin_path IS NULL
                    OR archived_access IS NULL
                    OR is_root != 0
                    OR root_key != 'vault'
                )
            )
        ",
    )
    .fetch_one(&mut **tx)
    .await?;
    anyhow::ensure!(
        invalid_folders == 0,
        "archive_invariant_failed invalid_folders={invalid_folders}"
    );

    let physical_archive_items: i64 = sqlx::query_scalar(
        r"
        SELECT
            (SELECT COUNT(*) FROM documents d JOIN folders f ON f.id = d.folder_id
             WHERE f.root_key = 'archive')
          + (SELECT COUNT(*) FROM folders WHERE root_key = 'archive' AND is_root = 0)
        ",
    )
    .fetch_one(&mut **tx)
    .await?;
    anyhow::ensure!(
        physical_archive_items == 0,
        "archive_invariant_failed physical_archive_items={physical_archive_items}"
    );
    Ok(())
}

async fn validate_column(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    column: &str,
) -> anyhow::Result<()> {
    let table_identifier = quote_identifier(table);
    let column_identifier = quote_identifier(column);
    let mut after_rowid = None;
    loop {
        let query = after_rowid.map_or_else(
            || {
                format!(
                    "SELECT rowid, CAST({column_identifier} AS TEXT) \
                     FROM {table_identifier} WHERE {column_identifier} IS NOT NULL \
                     ORDER BY rowid LIMIT {ROW_BATCH_SIZE}"
                )
            },
            |_| {
                format!(
                    "SELECT rowid, CAST({column_identifier} AS TEXT) \
                     FROM {table_identifier} WHERE {column_identifier} IS NOT NULL \
                     AND rowid > ? ORDER BY rowid LIMIT {ROW_BATCH_SIZE}"
                )
            },
        );
        let rows = if let Some(rowid) = after_rowid {
            sqlx::query_as::<_, (i64, String)>(&query)
                .bind(rowid)
                .fetch_all(&mut **tx)
                .await?
        } else {
            sqlx::query_as::<_, (i64, String)>(&query)
                .fetch_all(&mut **tx)
                .await?
        };
        if rows.is_empty() {
            break;
        }
        for (rowid, value) in &rows {
            anyhow::ensure!(
                is_canonical_target_timestamp(value),
                "noncanonical timestamp remains in {table}.{column} at rowid {rowid}"
            );
        }
        after_rowid = rows.last().map(|(rowid, _)| *rowid);
    }
    Ok(())
}

// These conversion rules are part of migration 4's immutable data contract.
// Keep them local so later changes to the application's timestamp policy cannot
// alter a replay of the released v2.2.0-to-v2.2.1 transition.
fn canonicalize_source_timestamp(value: &str) -> anyhow::Result<String> {
    let timestamp = parse_source_timestamp(value)?;
    format_target_timestamp(timestamp)
}

fn parse_source_timestamp(value: &str) -> anyhow::Result<OffsetDateTime> {
    let value = value.trim();
    anyhow::ensure!(!value.is_empty(), "timestamp is empty");

    if let Ok(timestamp) = OffsetDateTime::parse(value, &Rfc3339) {
        return Ok(timestamp.to_offset(UtcOffset::UTC));
    }
    for format in SOURCE_NAIVE_TIMESTAMP_FORMATS {
        let description = time::format_description::parse_borrowed::<1>(format)
            .expect("migration 4 contains a valid pinned timestamp format");
        if let Ok(timestamp) = PrimitiveDateTime::parse(value, &description) {
            return Ok(timestamp.assume_utc());
        }
    }
    anyhow::bail!("timestamp is not RFC 3339 or a recognized legacy UTC timestamp")
}

fn format_target_timestamp(timestamp: OffsetDateTime) -> anyhow::Result<String> {
    let timestamp = timestamp.to_offset(UtcOffset::UTC);
    let year = timestamp.year();
    anyhow::ensure!(
        (0..=9999).contains(&year),
        "timestamp year {year} cannot be represented by migration 4"
    );

    let mut canonical = String::with_capacity(CANONICAL_TIMESTAMP_LENGTH);
    write!(
        canonical,
        "{year:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second(),
        timestamp.nanosecond() / 1_000,
    )
    .expect("writing a timestamp to a String cannot fail");
    Ok(canonical)
}

fn is_canonical_target_timestamp(value: &str) -> bool {
    value.len() == CANONICAL_TIMESTAMP_LENGTH
        && canonicalize_source_timestamp(value).is_ok_and(|canonical| canonical == value)
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn apply_boxed<'borrow>(
    tx: &'borrow mut Transaction<'_, Sqlite>,
) -> BoxFuture<'borrow, anyhow::Result<()>> {
    Box::pin(apply(tx))
}

fn validate_target_boxed<'borrow>(
    tx: &'borrow mut Transaction<'_, Sqlite>,
) -> BoxFuture<'borrow, anyhow::Result<()>> {
    Box::pin(validate_target(tx))
}
