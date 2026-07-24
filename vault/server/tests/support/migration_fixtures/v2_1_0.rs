//! Runtime-generated database created from the released Vault 2.1.0 contract.
//!
//! The migration ledger and upload-session schema are pinned from upstream tag
//! `v2.1.0`, commit `372f2f3f6980a5119f55f829de2cd8801d87b42b`.
//! This builder extends the independently pinned 2.0.0 fixture with the exact
//! 2.1.0 transition and deterministic transfer states.

#![allow(dead_code)]

use std::path::Path;
use std::str::FromStr;

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Connection, Executor};

use super::v2_0_0;

const RELEASE_TIMESTAMP: &str = "2026-07-22T22:42:38Z";
pub const ACTIVE_CREATE_UPLOAD_ID: &str = "v2-1-active-create";
pub const COMPLETING_CREATE_UPLOAD_ID: &str = "v2-1-completing-create";
pub const ACTIVE_CHECKIN_UPLOAD_ID: &str = "v2-1-active-checkin";
pub const COMPLETE_CREATE_UPLOAD_ID: &str = "v2-1-complete-create";

#[derive(Debug)]
pub struct Fixture {
    baseline: v2_0_0::Fixture,
}

impl Fixture {
    pub async fn create() -> anyhow::Result<Self> {
        let baseline = v2_0_0::Fixture::create().await?;
        let options =
            SqliteConnectOptions::from_str(&format!("sqlite://{}", baseline.db_path().display()))?
                .create_if_missing(false)
                .foreign_keys(true)
                .journal_mode(SqliteJournalMode::Delete);
        let mut connection = sqlx::SqliteConnection::connect_with(&options)
            .await
            .context("open v2.1.0 fixture database")?;
        let mut transaction = connection
            .begin()
            .await
            .context("begin v2.1.0 fixture transaction")?;

        transaction
            .execute(
                r"
                INSERT INTO schema_migrations (version, name, applied_at)
                VALUES (2, 'normalize root folders', '2026-07-22T22:42:38Z')
                ",
            )
            .await?;
        seed_upload_sessions(&mut transaction).await?;
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

async fn seed_upload_sessions(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO upload_sessions (
            id, mode, status, folder_path, document_id, filename, total_size,
            chunk_size, part_count, verification_total_bytes,
            verification_processed_bytes, created_by, created_by_name,
            user_context, created_at, updated_at, expires_at, completed_at,
            result_document_id, result_version_id, result_path
        )
        VALUES
            (
                ?, 'create', 'active', 'Visual Assets/Migration Previews',
                NULL, 'active-create.txt', 1, 1, 1, 0, 0,
                'fixture:alice', 'Alice Fixture', '{}', ?, ?,
                '2999-01-01T00:00:00Z', NULL, NULL, NULL, NULL
            ),
            (
                ?, 'create', 'completing', 'Visual Assets/Migration Previews',
                NULL, 'completing-create.txt', 1, 1, 1, 1, 1,
                'fixture:alice', 'Alice Fixture', '{}', ?, ?,
                '2999-01-01T00:00:00Z', NULL, NULL, NULL, NULL
            ),
            (
                ?, 'checkin', 'active', NULL, 1000, 'migration-preview.png',
                1, 1, 1, 0, 0, 'fixture:alice', 'Alice Fixture', '{}', ?, ?,
                '2999-01-01T00:00:00Z', NULL, NULL, NULL, NULL
            ),
            (
                ?, 'create', 'complete', 'Visual Assets/Migration Previews',
                NULL, 'migration-preview.png', 1, 1, 1, 1, 1,
                'fixture:alice', 'Alice Fixture', '{}', ?, ?,
                '2999-01-01T00:00:00Z', ?, 1000,
                '00000000-0000-7000-8000-000000000001',
                'Visual Assets/Migration Previews/migration-preview.png'
            )
        ",
    )
    .bind(ACTIVE_CREATE_UPLOAD_ID)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(COMPLETING_CREATE_UPLOAD_ID)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(ACTIVE_CHECKIN_UPLOAD_ID)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(COMPLETE_CREATE_UPLOAD_ID)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn validate_fixture(connection: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut *connection)
        .await?;
    anyhow::ensure!(
        integrity == "ok",
        "v2.1.0 fixture integrity check returned {integrity:?}"
    );
    let foreign_key_violations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut *connection)
            .await?;
    anyhow::ensure!(
        foreign_key_violations == 0,
        "v2.1.0 fixture has {foreign_key_violations} foreign-key violations"
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
                    RELEASE_TIMESTAMP.to_string(),
                ),
            ],
        "v2.1.0 fixture has unexpected migration history: {history:?}"
    );
    let upload_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_sessions")
        .fetch_one(&mut *connection)
        .await?;
    anyhow::ensure!(
        upload_count == 4,
        "v2.1.0 fixture contains {upload_count} upload sessions; expected 4"
    );
    Ok(())
}
