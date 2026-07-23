mod invariants;
mod migrations;
mod schema_validation;

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Executor, SqlitePool};

use crate::root_folders::ROOT_FOLDERS;
use crate::state_events::replace_state_events_with_compaction_marker_in_tx;

pub const SQLITE_BUSY_TIMEOUT_MS: u64 = 30_000;
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

    migrations::run(&pool).await?;
    Ok(pool)
}

pub async fn reset(pool: &DbPool) -> anyhow::Result<Vec<String>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let upload_session_ids =
        sqlx::query_scalar::<_, String>("SELECT id FROM upload_sessions ORDER BY id")
            .fetch_all(&mut *tx)
            .await?;
    for table in [
        "share_links",
        "preview_renditions",
        "preview_jobs",
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
        "folders",
    ] {
        tx.execute(format!("DELETE FROM {table}").as_str()).await?;
    }
    insert_current_root_folders(&mut tx).await?;
    replace_state_events_with_compaction_marker_in_tx(&mut tx).await?;
    invariants::validate_current(&mut tx, &ROOT_FOLDERS).await?;
    tx.commit().await?;
    Ok(upload_session_ids)
}

pub async fn readiness_check(pool: &DbPool) -> anyhow::Result<()> {
    let mut connection = pool.acquire().await?;
    let value = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&mut *connection)
        .await?;
    if value != 1 {
        anyhow::bail!("database readiness query returned an unexpected value");
    }
    migrations::validate_readiness(&mut connection).await?;
    invariants::validate_current(&mut connection, &ROOT_FOLDERS).await
}

async fn insert_current_root_folders(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    for root in ROOT_FOLDERS {
        sqlx::query(
            r"
            INSERT INTO folders (root_key, parent_id, name, is_root)
            VALUES (?, NULL, ?, 1)
            ",
        )
        .bind(root.key)
        .bind(root.stored_name)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}
