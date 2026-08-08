//! Runtime-generated database created from the released Vault 2.2.0 contract.
//!
//! The schema, migration ledger, and persisted-state transition are pinned from
//! upstream tag `v2.2.0`, commit `662d2528518e5da2cb92dea45825e1ef47c7eaf3`.
//! This builder advances the independently pinned 2.1.0 fixture without calling
//! the current migration registry or application bootstrap.

#![allow(dead_code)]

use std::path::Path;
use std::str::FromStr;

use anyhow::Context;
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

use super::v2_1_0::{
    self, ACTIVE_CREATE_UPLOAD_ID, ARCHIVED_DOCUMENT_ID, COMPLETING_CREATE_UPLOAD_ID,
    EXISTING_PATH_ARCHIVED_DOCUMENT_ID,
};

const ROOT_MIGRATION_APPLIED_AT: &str = "2026-07-31 18:00:00";
const IDENTITY_MIGRATION_APPLIED_AT: &str = "2026-07-31 18:00:01";
const PROJECTS_FOLDER_ID: i64 = 13;
const INCOMING_FOLDER_ID: i64 = 14;

#[derive(Debug)]
pub struct Fixture {
    baseline: v2_1_0::Fixture,
}

impl Fixture {
    pub async fn create() -> anyhow::Result<Self> {
        let baseline = v2_1_0::Fixture::create().await?;
        let options =
            SqliteConnectOptions::from_str(&format!("sqlite://{}", baseline.db_path().display()))?
                .create_if_missing(false)
                .foreign_keys(true)
                .journal_mode(SqliteJournalMode::Delete);
        let mut connection = sqlx::SqliteConnection::connect_with(&options)
            .await
            .context("open v2.2.0 fixture database")?;
        let mut transaction = connection
            .begin()
            .await
            .context("begin v2.2.0 fixture transaction")?;

        record_root_normalization(&mut transaction).await?;
        add_upload_target_identity(&mut transaction).await?;
        add_archive_lifecycle(&mut transaction).await?;
        sqlx::query(
            r"
            INSERT INTO schema_migrations (version, name, applied_at)
            VALUES (3, 'preserve stored item identities', ?)
            ",
        )
        .bind(IDENTITY_MIGRATION_APPLIED_AT)
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;
        validate_fixture(&mut connection).await?;
        connection.close().await?;
        Ok(Self { baseline })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.baseline.root()
    }

    #[must_use]
    pub fn db_path(&self) -> &Path {
        self.baseline.db_path()
    }
}

async fn record_root_normalization(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    let canonical_roots: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM folders
        WHERE root_key = 'vault'
          AND parent_id IS NULL
          AND name = ''
          AND is_root = 1
        ",
    )
    .fetch_one(&mut **transaction)
    .await?;
    anyhow::ensure!(
        canonical_roots == 1,
        "pinned v2.1.0 source does not contain its canonical Vault root"
    );
    sqlx::query(
        r"
        INSERT INTO schema_migrations (version, name, applied_at)
        VALUES (2, 'normalize root folders', ?)
        ",
    )
    .bind(ROOT_MIGRATION_APPLIED_AT)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn add_upload_target_identity(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        ALTER TABLE upload_sessions
        ADD COLUMN target_folder_id INTEGER
            REFERENCES folders(id) ON DELETE SET NULL
        ",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'failed',
            verification_total_bytes = 0,
            verification_processed_bytes = 0,
            error = 'Upload target identity is unavailable after upgrade; restart the upload',
            updated_at = ?
        WHERE mode = 'create'
          AND status IN ('active', 'completing')
        ",
    )
    .bind(IDENTITY_MIGRATION_APPLIED_AT)
    .execute(&mut **transaction)
    .await?;

    for statement in [
        r"
        CREATE INDEX ix_upload_sessions_target_folder_status
        ON upload_sessions(target_folder_id, status)
        WHERE mode = 'create'
        ",
        r"
        CREATE TRIGGER trg_upload_sessions_require_create_target_insert
        BEFORE INSERT ON upload_sessions
        WHEN NEW.mode = 'create'
         AND NEW.status IN ('active', 'completing')
         AND NEW.target_folder_id IS NULL
        BEGIN
            SELECT RAISE(ABORT, 'active create upload requires a target folder identity');
        END
        ",
        r"
        CREATE TRIGGER trg_upload_sessions_require_create_target_update
        BEFORE UPDATE OF mode, status, target_folder_id ON upload_sessions
        WHEN NEW.mode = 'create'
         AND NEW.status IN ('active', 'completing')
         AND NEW.target_folder_id IS NULL
        BEGIN
            SELECT RAISE(ABORT, 'active create upload requires a target folder identity');
        END
        ",
    ] {
        sqlx::query(statement).execute(&mut **transaction).await?;
    }
    Ok(())
}

async fn add_archive_lifecycle(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    add_archive_columns(transaction).await?;
    migrate_archived_documents(transaction).await?;
    replace_archive_indexes_and_columns(transaction).await?;
    install_archive_invariant_triggers(transaction).await
}

async fn add_archive_columns(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    for statement in [
        "ALTER TABLE documents ADD COLUMN archived_at TEXT",
        "ALTER TABLE documents ADD COLUMN archived_origin_path TEXT",
        "ALTER TABLE folders ADD COLUMN archived_at TEXT",
        "ALTER TABLE folders ADD COLUMN archived_origin_path TEXT",
        "ALTER TABLE folders ADD COLUMN archived_access TEXT",
    ] {
        sqlx::query(statement).execute(&mut **transaction).await?;
    }
    Ok(())
}

async fn migrate_archived_documents(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO folders (
            id, root_key, parent_id, name, is_root, created_at
        )
        VALUES
            (?, 'vault', 1, 'Projects', 0, ?),
            (?, 'vault', ?, 'Incoming', 0, ?)
        ",
    )
    .bind(PROJECTS_FOLDER_ID)
    .bind(IDENTITY_MIGRATION_APPLIED_AT)
    .bind(INCOMING_FOLDER_ID)
    .bind(PROJECTS_FOLDER_ID)
    .bind(IDENTITY_MIGRATION_APPLIED_AT)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        UPDATE documents
        SET folder_id = ?,
            name = 'payload.bin',
            archived_at = '2026-07-22T22:35:00Z',
            archived_origin_path = 'Projects/Incoming/payload.bin',
            archived_access = COALESCE(archived_access, '{}')
        WHERE id = ?
          AND archived_from_folder = 'Projects/Incoming'
        ",
    )
    .bind(INCOMING_FOLDER_ID)
    .bind(ARCHIVED_DOCUMENT_ID)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        UPDATE documents
        SET folder_id = 11,
            name = 'existing-path.bin',
            archived_at = '2026-07-22T22:36:00Z',
            archived_origin_path = 'Visual Assets/Migration Previews/existing-path.bin',
            archived_access = COALESCE(archived_access, '{}')
        WHERE id = ?
          AND archived_from_folder = 'Visual Assets/Migration Previews'
        ",
    )
    .bind(EXISTING_PATH_ARCHIVED_DOCUMENT_ID)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn replace_archive_indexes_and_columns(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    for statement in [
        "DROP INDEX uq_documents_active_folder_name",
        "DROP INDEX uq_folders_parent_name",
        r"
        CREATE UNIQUE INDEX uq_documents_active_folder_name
        ON documents(folder_id, name)
        WHERE archived_at IS NULL
        ",
        r"
        CREATE UNIQUE INDEX uq_folders_parent_name
        ON folders(parent_id, name)
        WHERE is_root = 0 AND archived_at IS NULL
        ",
        r"
        CREATE INDEX ix_documents_archived_at
        ON documents(archived_at, id)
        WHERE archived_at IS NOT NULL
        ",
        r"
        CREATE INDEX ix_folders_archived_at
        ON folders(archived_at, id)
        WHERE archived_at IS NOT NULL
        ",
        "ALTER TABLE documents DROP COLUMN archived_from_folder",
        "ALTER TABLE documents DROP COLUMN archived_original_name",
    ] {
        sqlx::query(statement).execute(&mut **transaction).await?;
    }
    Ok(())
}

async fn install_archive_invariant_triggers(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    for statement in [
        r"
        CREATE TRIGGER trg_documents_archive_metadata_insert
        BEFORE INSERT ON documents
        WHEN
            (
                NEW.archived_at IS NULL
                AND (
                    NEW.archived_origin_path IS NOT NULL
                    OR NEW.archived_access IS NOT NULL
                )
            )
            OR (
                NEW.archived_at IS NOT NULL
                AND (
                    NEW.archived_origin_path IS NULL
                    OR NEW.archived_access IS NULL
                )
            )
        BEGIN
            SELECT RAISE(ABORT, 'document archive metadata must be complete');
        END
        ",
        r"
        CREATE TRIGGER trg_documents_archive_metadata_update
        BEFORE UPDATE OF archived_at, archived_origin_path, archived_access ON documents
        WHEN
            (
                NEW.archived_at IS NULL
                AND (
                    NEW.archived_origin_path IS NOT NULL
                    OR NEW.archived_access IS NOT NULL
                )
            )
            OR (
                NEW.archived_at IS NOT NULL
                AND (
                    NEW.archived_origin_path IS NULL
                    OR NEW.archived_access IS NULL
                )
            )
        BEGIN
            SELECT RAISE(ABORT, 'document archive metadata must be complete');
        END
        ",
        r"
        CREATE TRIGGER trg_folders_archive_metadata_insert
        BEFORE INSERT ON folders
        WHEN
            (
                NEW.archived_at IS NULL
                AND (
                    NEW.archived_origin_path IS NOT NULL
                    OR NEW.archived_access IS NOT NULL
                )
            )
            OR (
                NEW.archived_at IS NOT NULL
                AND (
                    NEW.archived_origin_path IS NULL
                    OR NEW.archived_access IS NULL
                    OR NEW.is_root != 0
                )
            )
        BEGIN
            SELECT RAISE(ABORT, 'folder archive metadata must be complete and non-root');
        END
        ",
        r"
        CREATE TRIGGER trg_folders_archive_metadata_update
        BEFORE UPDATE OF archived_at, archived_origin_path, archived_access, is_root ON folders
        WHEN
            (
                NEW.archived_at IS NULL
                AND (
                    NEW.archived_origin_path IS NOT NULL
                    OR NEW.archived_access IS NOT NULL
                )
            )
            OR (
                NEW.archived_at IS NOT NULL
                AND (
                    NEW.archived_origin_path IS NULL
                    OR NEW.archived_access IS NULL
                    OR NEW.is_root != 0
                )
            )
        BEGIN
            SELECT RAISE(ABORT, 'folder archive metadata must be complete and non-root');
        END
        ",
    ] {
        sqlx::query(statement).execute(&mut **transaction).await?;
    }
    Ok(())
}

async fn validate_fixture(connection: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    validate_integrity_and_history(connection).await?;
    validate_schema_contract(connection).await?;
    validate_transition_data(connection).await
}

async fn validate_integrity_and_history(
    connection: &mut sqlx::SqliteConnection,
) -> anyhow::Result<()> {
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut *connection)
        .await?;
    anyhow::ensure!(
        integrity == "ok",
        "v2.2.0 fixture integrity check returned {integrity:?}"
    );
    let foreign_key_violations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut *connection)
            .await?;
    anyhow::ensure!(
        foreign_key_violations == 0,
        "v2.2.0 fixture has {foreign_key_violations} foreign-key violations"
    );

    let history = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT version, name, applied_at FROM schema_migrations ORDER BY version",
    )
    .fetch_all(&mut *connection)
    .await?;
    anyhow::ensure!(
        history
            == vec![
                (
                    1,
                    "content previews".to_string(),
                    "2026-07-22T14:31:29Z".to_string(),
                ),
                (
                    2,
                    "normalize root folders".to_string(),
                    ROOT_MIGRATION_APPLIED_AT.to_string(),
                ),
                (
                    3,
                    "preserve stored item identities".to_string(),
                    IDENTITY_MIGRATION_APPLIED_AT.to_string(),
                ),
            ],
        "v2.2.0 fixture has unexpected migration history: {history:?}"
    );
    Ok(())
}

async fn validate_schema_contract(connection: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    let required_columns: i64 = sqlx::query_scalar(
        r"
        SELECT
            (SELECT COUNT(*) FROM pragma_table_info('upload_sessions')
             WHERE name = 'target_folder_id')
          + (SELECT COUNT(*) FROM pragma_table_info('documents')
             WHERE name IN ('archived_at', 'archived_origin_path'))
          + (SELECT COUNT(*) FROM pragma_table_info('folders')
             WHERE name IN ('archived_at', 'archived_origin_path', 'archived_access'))
        ",
    )
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        required_columns == 6,
        "v2.2.0 fixture columns are incomplete"
    );
    let legacy_columns: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM pragma_table_info('documents')
        WHERE name IN ('archived_from_folder', 'archived_original_name')
        ",
    )
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        legacy_columns == 0,
        "v2.2.0 fixture retained legacy columns"
    );
    let timestamp_normalizers: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM sqlite_schema
        WHERE type = 'trigger'
          AND name GLOB 'trg_*_timestamp_defaults_canonical_insert'
        ",
    )
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        timestamp_normalizers == 0,
        "v2.2.0 fixture unexpectedly contains a later timestamp trigger"
    );
    Ok(())
}

async fn validate_transition_data(connection: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    let invalid_active_create_uploads: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM upload_sessions
        WHERE mode = 'create'
          AND status IN ('active', 'completing')
          AND target_folder_id IS NULL
        ",
    )
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        invalid_active_create_uploads == 0,
        "v2.2.0 fixture retained ambiguous create uploads"
    );
    let failed_create_uploads: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upload_sessions WHERE id IN (?, ?) AND status = 'failed'",
    )
    .bind(ACTIVE_CREATE_UPLOAD_ID)
    .bind(COMPLETING_CREATE_UPLOAD_ID)
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        failed_create_uploads == 2,
        "v2.2.0 fixture did not preserve the released create-upload transition"
    );

    let archived_documents: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM documents d
        JOIN folders f ON f.id = d.folder_id
        WHERE d.id IN (?, ?)
          AND d.archived_at IS NOT NULL
          AND d.archived_origin_path IS NOT NULL
          AND d.archived_access IS NOT NULL
          AND f.root_key = 'vault'
        ",
    )
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(EXISTING_PATH_ARCHIVED_DOCUMENT_ID)
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        archived_documents == 2,
        "v2.2.0 fixture archive transition is incomplete"
    );
    Ok(())
}
