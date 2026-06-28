mod schema;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Executor, Row, Sqlite, SqlitePool, Transaction};

pub use schema::SQLITE_BUSY_TIMEOUT_MS;

const SQLITE_POOL_SIZE: u32 = 10;

pub type DbPool = SqlitePool;

pub async fn connect(db_path: &Path) -> anyhow::Result<DbPool> {
    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS));

    let pool = SqlitePoolOptions::new()
        .max_connections(SQLITE_POOL_SIZE)
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(options)
        .await?;

    init_schema(&pool).await?;
    Ok(pool)
}

pub async fn reset(pool: &DbPool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    for table in [
        "share_links",
        "export_artifacts",
        "export_jobs",
        "upload_parts",
        "upload_sessions",
        "document_events",
        "document_locks",
        "document_versions",
        "documents",
        "blob_locations",
        "blobs",
        "folder_events",
        "folder_permissions",
        "vault_group_memberships",
        "vault_groups",
        "vault_users",
        "vault_settings",
        "state_events",
        "folders",
    ] {
        tx.execute(format!("DELETE FROM {table}").as_str()).await?;
    }
    seed_root_folder(&mut tx, "vault", "", "Vault").await?;
    seed_root_folder(&mut tx, "archive", "Archive", "Archive").await?;
    tx.commit().await?;
    Ok(())
}

async fn init_schema(pool: &DbPool) -> anyhow::Result<()> {
    let existing_tables = user_table_names(pool).await?;
    let mut tx = pool.begin().await?;
    if existing_tables.is_empty() {
        apply_schema_statements(&mut tx).await?;
    } else {
        apply_known_additive_migrations(&mut tx, &existing_tables).await?;
    }
    tx.commit().await?;

    validate_schema(pool).await?;

    let mut tx = pool.begin().await?;
    seed_root_folder(&mut tx, "vault", "", "Vault").await?;
    seed_root_folder(&mut tx, "archive", "Archive", "Archive").await?;
    tx.commit().await?;
    Ok(())
}

async fn apply_schema_statements(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    for statement in schema::STATEMENTS {
        tx.execute(*statement).await?;
    }
    Ok(())
}

async fn apply_known_additive_migrations(
    tx: &mut Transaction<'_, Sqlite>,
    existing_tables: &BTreeSet<String>,
) -> anyhow::Result<()> {
    if existing_tables.contains("vault_users") {
        for table in [
            "vault_settings",
            "upload_sessions",
            "upload_parts",
            "export_jobs",
            "export_artifacts",
        ] {
            if !existing_tables.contains(table) {
                create_schema_for_table(tx, table).await?;
            }
        }
        migrate_vault_users(tx).await?;
    }
    if existing_tables.contains("upload_sessions") {
        migrate_upload_sessions(tx).await?;
    }
    if existing_tables.contains("share_links") {
        migrate_share_links(tx).await?;
    }
    if existing_tables.contains("export_jobs") {
        migrate_export_jobs(tx).await?;
    }
    Ok(())
}

async fn create_schema_for_table(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
) -> anyhow::Result<()> {
    for statement in schema::STATEMENTS {
        if schema_statement_targets_table(statement, table) {
            tx.execute(*statement).await?;
        }
    }
    Ok(())
}

fn schema_statement_targets_table(statement: &str, table: &str) -> bool {
    let normalized = normalize_sql(statement);
    normalized.starts_with(&format!("create table if not exists {table} "))
        || normalized.contains(&format!(" on {table}("))
        || normalized.contains(&format!(" on {table} ("))
}

async fn migrate_vault_users(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    let rows = sqlx::query("PRAGMA table_info(vault_users)")
        .fetch_all(&mut **tx)
        .await?;
    let mut columns = rows
        .iter()
        .filter_map(|row| row.try_get::<String, _>("name").ok())
        .collect::<HashSet<_>>();

    add_column(
        tx,
        &mut columns,
        "vault_users",
        "preferences",
        "preferences TEXT NOT NULL DEFAULT '{}'",
    )
    .await
}

async fn migrate_upload_sessions(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    let rows = sqlx::query("PRAGMA table_info(upload_sessions)")
        .fetch_all(&mut **tx)
        .await?;
    let mut columns = rows
        .iter()
        .filter_map(|row| row.try_get::<String, _>("name").ok())
        .collect::<HashSet<_>>();

    add_column(
        tx,
        &mut columns,
        "upload_sessions",
        "verification_total_bytes",
        "verification_total_bytes INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    add_column(
        tx,
        &mut columns,
        "upload_sessions",
        "verification_processed_bytes",
        "verification_processed_bytes INTEGER NOT NULL DEFAULT 0",
    )
    .await
}

async fn migrate_export_jobs(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    let rows = sqlx::query("PRAGMA table_info(export_jobs)")
        .fetch_all(&mut **tx)
        .await?;
    let mut columns = rows
        .iter()
        .filter_map(|row| row.try_get::<String, _>("name").ok())
        .collect::<HashSet<_>>();

    add_column(
        tx,
        &mut columns,
        "export_jobs",
        "request_payload",
        "request_payload TEXT NOT NULL DEFAULT '{}'",
    )
    .await?;
    add_column(
        tx,
        &mut columns,
        "export_jobs",
        "completed_at",
        "completed_at TEXT",
    )
    .await?;
    add_column(
        tx,
        &mut columns,
        "export_jobs",
        "cancelled_at",
        "cancelled_at TEXT",
    )
    .await?;
    Ok(())
}

async fn migrate_share_links(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    let rows = sqlx::query("PRAGMA table_info(share_links)")
        .fetch_all(&mut **tx)
        .await?;
    let mut columns = rows
        .iter()
        .filter_map(|row| row.try_get::<String, _>("name").ok())
        .collect::<HashSet<_>>();

    add_share_link_column(tx, &mut columns, "target_type", "target_type TEXT").await?;
    add_share_link_column(tx, &mut columns, "document_id", "document_id INTEGER").await?;
    add_share_link_column(tx, &mut columns, "folder_id", "folder_id INTEGER").await?;
    add_share_link_column(
        tx,
        &mut columns,
        "access_mode",
        "access_mode TEXT NOT NULL DEFAULT 'internal'",
    )
    .await?;
    add_share_link_column(
        tx,
        &mut columns,
        "created_by_user_id",
        "created_by_user_id INTEGER",
    )
    .await?;
    add_share_link_column(tx, &mut columns, "disabled_at", "disabled_at TEXT").await?;
    add_share_link_column(tx, &mut columns, "item_type", "item_type TEXT").await?;
    add_share_link_column(tx, &mut columns, "item_id", "item_id INTEGER").await?;

    sqlx::query(
        r"
        UPDATE share_links
        SET target_type =
            CASE
                WHEN item_type IN ('document', 'file') THEN 'document'
                WHEN item_type = 'folder' THEN 'folder'
                ELSE item_type
            END
        WHERE target_type IS NULL
          AND item_type IS NOT NULL
        ",
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r"
        UPDATE share_links
        SET document_id = item_id
        WHERE document_id IS NULL
          AND item_type IN ('document', 'file')
        ",
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r"
        UPDATE share_links
        SET folder_id = item_id
        WHERE folder_id IS NULL
          AND item_type = 'folder'
        ",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn add_share_link_column(
    tx: &mut Transaction<'_, Sqlite>,
    columns: &mut HashSet<String>,
    name: &str,
    definition: &str,
) -> anyhow::Result<()> {
    add_column(tx, columns, "share_links", name, definition).await
}

async fn add_column(
    tx: &mut Transaction<'_, Sqlite>,
    columns: &mut HashSet<String>,
    table: &str,
    name: &str,
    definition: &str,
) -> anyhow::Result<()> {
    if columns.contains(name) {
        return Ok(());
    }
    sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {definition}"))
        .execute(&mut **tx)
        .await?;
    columns.insert(name.to_string());
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaMetadata {
    tables: BTreeMap<String, TableMetadata>,
    triggers: BTreeSet<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableMetadata {
    columns: BTreeMap<String, ColumnMetadata>,
    named_indexes: BTreeMap<String, IndexMetadata>,
    unique_constraints: BTreeSet<Vec<String>>,
    foreign_keys: BTreeSet<ForeignKeyMetadata>,
    has_check_constraints: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnMetadata {
    type_family: String,
    not_null: bool,
    primary_key_position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexMetadata {
    columns: Vec<String>,
    unique: bool,
    where_clause: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ForeignKeyMetadata {
    from_column: String,
    foreign_table: String,
    foreign_column: String,
    on_delete: String,
}

async fn validate_schema(pool: &DbPool) -> anyhow::Result<()> {
    let expected_pool = expected_schema_pool().await?;
    let expected = schema_metadata(&expected_pool).await?;
    let live = schema_metadata(pool).await?;

    for (table_name, expected_table) in &expected.tables {
        let Some(live_table) = live.tables.get(table_name) else {
            return Err(schema_incompatible(format!("missing table {table_name}")));
        };
        if live_table.columns != expected_table.columns {
            return Err(schema_incompatible(format!(
                "column definition mismatch for table {table_name}",
            )));
        }
        for (index_name, expected_index) in &expected_table.named_indexes {
            let Some(live_index) = live_table.named_indexes.get(index_name) else {
                return Err(schema_incompatible(format!(
                    "missing index {index_name} on table {table_name}",
                )));
            };
            if live_index != expected_index {
                return Err(schema_incompatible(format!(
                    "index definition mismatch for {index_name}",
                )));
            }
        }
        let expected_named_indexes = expected_table
            .named_indexes
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        for (index_name, live_index) in &live_table.named_indexes {
            if live_index.unique && !expected_named_indexes.contains(index_name) {
                return Err(schema_incompatible(format!(
                    "unexpected unique index {index_name} on table {table_name}",
                )));
            }
        }
        if live_table.unique_constraints != expected_table.unique_constraints {
            return Err(schema_incompatible(format!(
                "unique constraint mismatch for table {table_name}",
            )));
        }
        if live_table.foreign_keys != expected_table.foreign_keys {
            return Err(schema_incompatible(format!(
                "foreign key mismatch for table {table_name}",
            )));
        }
        if live_table.has_check_constraints != expected_table.has_check_constraints {
            return Err(schema_incompatible(format!(
                "check constraint mismatch for table {table_name}",
            )));
        }
    }

    let expected_tables = expected.tables.keys().cloned().collect::<BTreeSet<_>>();
    for (trigger_name, table_name) in &live.triggers {
        if expected_tables.contains(table_name)
            && !expected
                .triggers
                .contains(&(trigger_name.clone(), table_name.clone()))
        {
            return Err(schema_incompatible(format!(
                "unexpected trigger {trigger_name} on table {table_name}",
            )));
        }
    }
    Ok(())
}

fn schema_incompatible(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "Database schema is incompatible with this app version. \
         Startup refused to alter or drop existing metadata automatically. {reason}"
    )
}

async fn expected_schema_pool() -> anyhow::Result<DbPool> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let mut tx = pool.begin().await?;
    apply_schema_statements(&mut tx).await?;
    tx.commit().await?;
    Ok(pool)
}

async fn schema_metadata(pool: &DbPool) -> anyhow::Result<SchemaMetadata> {
    let mut tables = BTreeMap::new();
    for table_name in user_table_names(pool).await? {
        tables.insert(table_name.clone(), table_metadata(pool, &table_name).await?);
    }
    let trigger_rows = sqlx::query(
        r"
        SELECT name, tbl_name
        FROM sqlite_master
        WHERE type = 'trigger'
        ",
    )
    .fetch_all(pool)
    .await?;
    let triggers = trigger_rows
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get::<String, _>("name")?,
                row.try_get::<String, _>("tbl_name")?,
            ))
        })
        .collect::<Result<BTreeSet<_>, sqlx::Error>>()?;
    Ok(SchemaMetadata { tables, triggers })
}

async fn user_table_names(pool: &DbPool) -> anyhow::Result<BTreeSet<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        r"
        SELECT name
        FROM sqlite_master
        WHERE type = 'table'
          AND name NOT LIKE 'sqlite_%'
        ORDER BY name
        ",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect())
}

async fn table_metadata(pool: &DbPool, table_name: &str) -> anyhow::Result<TableMetadata> {
    let columns = column_metadata(pool, table_name).await?;
    let (named_indexes, unique_constraints) = index_metadata(pool, table_name).await?;
    let foreign_keys = foreign_key_metadata(pool, table_name).await?;
    let has_check_constraints = table_has_check_constraints(pool, table_name).await?;
    Ok(TableMetadata {
        columns,
        named_indexes,
        unique_constraints,
        foreign_keys,
        has_check_constraints,
    })
}

async fn column_metadata(
    pool: &DbPool,
    table_name: &str,
) -> anyhow::Result<BTreeMap<String, ColumnMetadata>> {
    let rows = sqlx::query(&format!("PRAGMA table_info({})", quote_ident(table_name)))
        .fetch_all(pool)
        .await?;
    let mut columns = BTreeMap::new();
    for row in rows {
        let name = row.try_get::<String, _>("name")?;
        let declared_type = row.try_get::<String, _>("type")?;
        columns.insert(
            name,
            ColumnMetadata {
                type_family: sqlite_type_family(&declared_type),
                not_null: row.try_get::<i64, _>("notnull")? != 0,
                primary_key_position: row.try_get::<i64, _>("pk")?,
            },
        );
    }
    Ok(columns)
}

async fn index_metadata(
    pool: &DbPool,
    table_name: &str,
) -> anyhow::Result<(BTreeMap<String, IndexMetadata>, BTreeSet<Vec<String>>)> {
    let rows = sqlx::query(&format!("PRAGMA index_list({})", quote_ident(table_name)))
        .fetch_all(pool)
        .await?;
    let mut named_indexes = BTreeMap::new();
    let mut unique_constraints = BTreeSet::new();
    for row in rows {
        let name = row.try_get::<String, _>("name")?;
        let unique = row.try_get::<i64, _>("unique")? != 0;
        let origin = row.try_get::<String, _>("origin")?;
        let columns = index_columns(pool, &name).await?;
        match origin.as_str() {
            "u" => {
                unique_constraints.insert(columns);
            }
            "c" => {
                named_indexes.insert(
                    name.clone(),
                    IndexMetadata {
                        columns,
                        unique,
                        where_clause: index_where_clause(pool, &name).await?,
                    },
                );
            }
            _ => {}
        }
    }
    Ok((named_indexes, unique_constraints))
}

async fn index_columns(pool: &DbPool, index_name: &str) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(&format!("PRAGMA index_info({})", quote_ident(index_name)))
        .fetch_all(pool)
        .await?;
    let mut columns = rows
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get::<i64, _>("seqno")?,
                row.try_get::<String, _>("name")?,
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    columns.sort_by_key(|(sequence, _)| *sequence);
    Ok(columns.into_iter().map(|(_, name)| name).collect())
}

async fn index_where_clause(pool: &DbPool, index_name: &str) -> anyhow::Result<String> {
    let sql: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?")
            .bind(index_name)
            .fetch_optional(pool)
            .await?
            .flatten();
    let normalized = sql.as_deref().map(normalize_sql).unwrap_or_default();
    Ok(normalized
        .split_once(" where ")
        .map_or(String::new(), |(_, where_clause)| where_clause.to_string()))
}

async fn foreign_key_metadata(
    pool: &DbPool,
    table_name: &str,
) -> anyhow::Result<BTreeSet<ForeignKeyMetadata>> {
    let rows = sqlx::query(&format!(
        "PRAGMA foreign_key_list({})",
        quote_ident(table_name)
    ))
    .fetch_all(pool)
    .await?;
    let mut foreign_keys = BTreeSet::new();
    for row in rows {
        foreign_keys.insert(ForeignKeyMetadata {
            from_column: row.try_get::<String, _>("from")?,
            foreign_table: row.try_get::<String, _>("table")?,
            foreign_column: row.try_get::<String, _>("to")?,
            on_delete: row.try_get::<String, _>("on_delete")?.to_ascii_uppercase(),
        });
    }
    Ok(foreign_keys)
}

async fn table_has_check_constraints(pool: &DbPool, table_name: &str) -> anyhow::Result<bool> {
    let create_sql: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table_name)
            .fetch_optional(pool)
            .await?
            .flatten();
    let normalized = create_sql.as_deref().map(normalize_sql).unwrap_or_default();
    Ok(normalized.contains(" check ") || normalized.contains(" check("))
}

fn sqlite_type_family(declared_type: &str) -> String {
    let upper = declared_type.trim().to_ascii_uppercase();
    if upper.contains("INT") || upper.contains("BOOL") {
        "integer".to_string()
    } else if upper.contains("CHAR")
        || upper.contains("CLOB")
        || upper.contains("TEXT")
        || upper.contains("JSON")
        || upper.contains("DATE")
        || upper.contains("TIME")
    {
        "text".to_string()
    } else if upper.contains("BLOB") {
        "blob".to_string()
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        "real".to_string()
    } else {
        "numeric".to_string()
    }
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn seed_root_folder(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    root_key: &str,
    name: &str,
    label: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO folders (root_key, parent_id, name, is_root)
        SELECT ?, NULL, ?, 1
        WHERE NOT EXISTS (
            SELECT 1 FROM folders WHERE root_key = ? AND is_root = 1
        )
        ",
    )
    .bind(root_key)
    .bind(name)
    .bind(root_key)
    .execute(&mut **tx)
    .await?;

    tracing::debug!(root_key, label, "ensured root folder");
    Ok(())
}
