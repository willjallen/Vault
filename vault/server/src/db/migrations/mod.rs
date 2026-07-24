mod v2_0_0;
mod v2_1_0;
mod v2_2_0;

use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use futures_util::future::BoxFuture;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

use super::SQLITE_BUSY_TIMEOUT_MS;
use super::schema_validation::{
    SchemaMetadata, schema_incompatible, schema_metadata, user_schema_objects,
    validate_schema_metadata_exact,
};

pub type MigrationOperation =
    for<'borrow, 'connection> fn(
        &'borrow mut Transaction<'connection, Sqlite>,
    ) -> BoxFuture<'borrow, anyhow::Result<()>>;

#[derive(Clone, Copy)]
pub struct BaselineDefinition {
    pub migration_version: i64,
    pub target_version: &'static str,
    pub migration_name: &'static str,
    pub install: MigrationOperation,
    pub validate: MigrationOperation,
}

#[derive(Clone, Copy)]
pub struct MigrationDefinition {
    pub version: i64,
    pub target_version: &'static str,
    pub name: &'static str,
    pub apply: MigrationOperation,
    pub validate_target: MigrationOperation,
}

const BASELINE: BaselineDefinition = v2_0_0::BASELINE;
pub const MIGRATIONS: [MigrationDefinition; 2] = [v2_1_0::MIGRATION, v2_2_0::MIGRATION];
const CURRENT_MIGRATION_VERSION: i64 = MIGRATIONS[MIGRATIONS.len() - 1].version;
const KNOWN_HISTORY_LENGTH: usize = MIGRATIONS.len() + 1;

pub async fn run(pool: &SqlitePool) -> anyhow::Result<()> {
    validate_registry()?;
    let expected_schemas = expected_schema_metadata_by_version().await?;

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let existing_objects = user_schema_objects(&mut tx).await?;
    let has_migration_ledger = existing_objects
        .iter()
        .any(|object| object.object_type == "table" && object.name == "schema_migrations");
    let current_version = if existing_objects.is_empty() {
        (BASELINE.install)(&mut tx).await.with_context(|| {
            format!(
                "database baseline {} installation failed",
                BASELINE.target_version
            )
        })?;
        validate_live_schema(
            &mut tx,
            expected_schema_for_version(BASELINE.migration_version, &expected_schemas)?,
            format!("fresh {} baseline", BASELINE.target_version),
        )
        .await?;
        validate_invariants_for_version(&mut tx, BASELINE.migration_version).await?;
        BASELINE.migration_version
    } else if has_migration_ledger {
        let version = migration_history_version(&mut tx).await?;
        validate_live_schema(
            &mut tx,
            expected_schema_for_version(version, &expected_schemas)?,
            format!("database migration {version}"),
        )
        .await?;
        validate_invariants_for_version(&mut tx, version).await?;
        version
    } else {
        return Err(schema_incompatible(format!(
            "database contains existing objects but no schema_migrations ledger; \
             the oldest supported source baseline is Vault {}",
            BASELINE.target_version
        )));
    };

    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current_version)
    {
        apply_one(&mut tx, *migration).await?;
        validate_live_schema(
            &mut tx,
            expected_schema_for_version(migration.version, &expected_schemas)?,
            format!(
                "migration {} ({})",
                migration.version, migration.target_version
            ),
        )
        .await?;
        validate_invariants_for_version(&mut tx, migration.version).await?;
    }

    let foreign_key_violations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut *tx)
            .await?;
    if foreign_key_violations != 0 {
        return Err(schema_incompatible(format!(
            "foreign-key validation found {foreign_key_violations} violations"
        )));
    }

    tx.commit().await?;
    Ok(())
}

pub(super) async fn validate_readiness(connection: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows = migration_history_rows(connection).await?;
    let version = validate_migration_history(&rows)?;
    if version != CURRENT_MIGRATION_VERSION {
        return Err(schema_incompatible(format!(
            "database is at migration {version}; expected {CURRENT_MIGRATION_VERSION}"
        )));
    }
    Ok(())
}

async fn apply_one(
    tx: &mut Transaction<'_, Sqlite>,
    migration: MigrationDefinition,
) -> anyhow::Result<()> {
    (migration.apply)(tx).await.with_context(|| {
        format!(
            "database migration {} targeting {} ({}) failed",
            migration.version, migration.target_version, migration.name
        )
    })?;
    record_migration(tx, migration).await.with_context(|| {
        format!(
            "database migration {} targeting {} could not be recorded",
            migration.version, migration.target_version
        )
    })
}

async fn record_migration(
    tx: &mut Transaction<'_, Sqlite>,
    migration: MigrationDefinition,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO schema_migrations (version, name) VALUES (?, ?)")
        .bind(migration.version)
        .bind(migration.name)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn migration_history_version(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<i64> {
    let rows = migration_history_rows(tx).await?;
    validate_migration_history(&rows)
}

async fn migration_history_rows(
    connection: &mut SqliteConnection,
) -> anyhow::Result<Vec<(i64, String)>> {
    sqlx::query_as::<_, (i64, String)>(
        "SELECT version, name FROM schema_migrations ORDER BY version",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| schema_incompatible(format!("schema_migrations cannot be read: {error}")))
}

fn validate_migration_history(rows: &[(i64, String)]) -> anyhow::Result<i64> {
    if rows.is_empty() {
        return Err(schema_incompatible(
            "schema_migrations exists but does not contain the required Vault 2.0.0 baseline entry",
        ));
    }
    if rows.len() > KNOWN_HISTORY_LENGTH {
        return Err(schema_incompatible(format!(
            "database migration history has {} entries but this app knows only {}",
            rows.len(),
            KNOWN_HISTORY_LENGTH
        )));
    }

    for (index, (version, name)) in rows.iter().enumerate() {
        let (expected_version, expected_name) = expected_history_entry(index).ok_or_else(|| {
            schema_incompatible(format!(
                "database migration history contains unsupported entry index {index}"
            ))
        })?;
        if *version != expected_version || name != expected_name {
            return Err(schema_incompatible(format!(
                "unsupported migration history entry ({version}, {name:?}); \
                 expected ({expected_version}, {expected_name:?})"
            )));
        }
    }

    Ok(rows
        .last()
        .map_or(BASELINE.migration_version, |(version, _)| *version))
}

fn expected_history_entry(index: usize) -> Option<(i64, &'static str)> {
    if index == 0 {
        return Some((BASELINE.migration_version, BASELINE.migration_name));
    }
    MIGRATIONS
        .get(index.checked_sub(1)?)
        .map(|migration| (migration.version, migration.name))
}

fn validate_registry() -> anyhow::Result<()> {
    if BASELINE.migration_version < 1
        || BASELINE.migration_name.trim().is_empty()
        || BASELINE.target_version.trim().is_empty()
    {
        anyhow::bail!("database baseline has invalid migration metadata");
    }

    for (index, migration) in MIGRATIONS.iter().enumerate() {
        let expected_version = BASELINE.migration_version + i64::try_from(index + 1)?;
        if migration.version != expected_version {
            anyhow::bail!(
                "migration registry entry {} has version {}; expected {expected_version}",
                migration.target_version,
                migration.version
            );
        }
        if migration.name.trim().is_empty() || migration.target_version.trim().is_empty() {
            anyhow::bail!("migration registry entry {expected_version} has empty metadata");
        }
    }
    Ok(())
}

async fn validate_live_schema(
    tx: &mut Transaction<'_, Sqlite>,
    expected: &SchemaMetadata,
    state: impl std::fmt::Display,
) -> anyhow::Result<()> {
    let live = schema_metadata(tx).await?;
    validate_schema_metadata_exact(
        expected,
        &live,
        format!("{state} schema does not exactly match its migration contract"),
    )
}

async fn validate_invariants_for_version(
    tx: &mut Transaction<'_, Sqlite>,
    version: i64,
) -> anyhow::Result<()> {
    let result = if version == BASELINE.migration_version {
        (BASELINE.validate)(tx).await
    } else {
        let version_index =
            usize::try_from(version - BASELINE.migration_version - 1).map_err(|_| {
                schema_incompatible(format!(
                    "database declares unknown migration version {version}"
                ))
            })?;
        let migration = MIGRATIONS.get(version_index).ok_or_else(|| {
            schema_incompatible(format!(
                "database declares unknown migration version {version}"
            ))
        })?;
        (migration.validate_target)(tx).await
    };
    result.map_err(|error| {
        schema_incompatible(format!(
            "persisted folder state failed validation at migration {version}: {error}"
        ))
    })
}

fn expected_schema_for_version(
    version: i64,
    expected_schemas: &[SchemaMetadata],
) -> anyhow::Result<&SchemaMetadata> {
    let version_index = usize::try_from(version - BASELINE.migration_version).map_err(|_| {
        schema_incompatible(format!(
            "database declares unknown migration version {version}"
        ))
    })?;
    expected_schemas.get(version_index).ok_or_else(|| {
        schema_incompatible(format!(
            "database declares unknown migration version {version}"
        ))
    })
}

async fn expected_schema_metadata_by_version() -> anyhow::Result<Vec<SchemaMetadata>> {
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
    (BASELINE.install)(&mut tx).await?;
    (BASELINE.validate)(&mut tx).await?;
    let mut schemas = vec![schema_metadata(&mut tx).await?];
    for migration in MIGRATIONS {
        apply_one(&mut tx, migration).await?;
        (migration.validate_target)(&mut tx).await?;
        schemas.push(schema_metadata(&mut tx).await?);
    }
    Ok(schemas)
}
