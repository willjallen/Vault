//! Runtime-generated database created from the released Vault 2.0.0 contract.
//!
//! The schema and baseline ledger are pinned from upstream tag `v2.0.0`,
//! commit `073b809ff3e37f785b24539635a4bffa441a9088`. The representative
//! application data is deterministic synthetic test data.
//!
//! This module deliberately does not call the current database bootstrap
//! code: doing so would make an upgrade test prove only that today's producer
//! agrees with today's consumer.

#![allow(clippy::needless_raw_string_hashes)]
// Cargo compiles this shared support module into each integration-test crate,
// and each consumer intentionally uses a different subset of its fixture API.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Connection, Executor};
use tempfile::TempDir;

const RELEASE_TIMESTAMP: &str = "2026-07-22T14:31:29Z";
const ALICE_ID: &str = "fixture:alice";
const ALICE_NAME: &str = "Alice Fixture";

pub const VAULT_ROOT_ID: i64 = 1;
pub const ARCHIVE_ROOT_ID: i64 = 2;
pub const VISUAL_ASSETS_FOLDER_ID: i64 = 10;
pub const MIGRATION_PREVIEWS_FOLDER_ID: i64 = 11;
pub const EMPTY_FOLDER_ID: i64 = 12;
pub const FIXTURE_WRITER_USER_ID: i64 = 100;
pub const FIXTURE_WRITERS_GROUP_ID: i64 = 200;
pub const DOCUMENT_ID: i64 = 1000;

/// A complete Vault 2.0.0 installation generated in an isolated temporary
/// directory.
///
/// Keeping the directory owner in this value ensures the database lives for
/// the duration of the test and disappears automatically afterwards.
#[derive(Debug)]
pub struct Fixture {
    temp_dir: TempDir,
    db_path: PathBuf,
}

impl Fixture {
    /// Builds the frozen 2.0.0 schema and a production-shaped data graph.
    pub async fn create() -> anyhow::Result<Self> {
        let temp_dir = tempfile::tempdir().context("create v2.0.0 fixture directory")?;
        let db_path = temp_dir.path().join("vault.db");

        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Delete);
        let mut connection = sqlx::SqliteConnection::connect_with(&options)
            .await
            .context("open v2.0.0 fixture database")?;
        let mut transaction = connection
            .begin()
            .await
            .context("begin v2.0.0 fixture transaction")?;

        for statement in V2_0_0_SCHEMA {
            transaction
                .execute(*statement)
                .await
                .with_context(|| format!("install v2.0.0 schema statement: {statement}"))?;
        }
        seed_roots_and_ledger(&mut transaction).await?;
        seed_identity_graph(&mut transaction).await?;
        seed_folder_graph(&mut transaction).await?;
        seed_blob_graph(&mut transaction).await?;
        seed_document_and_preview_graph(&mut transaction).await?;
        seed_event_graph(&mut transaction).await?;
        transaction
            .commit()
            .await
            .context("commit v2.0.0 fixture")?;

        validate_fixture(&mut connection).await?;
        connection.close().await?;

        Ok(Self { temp_dir, db_path })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.temp_dir.path()
    }

    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

/// Exact schema emitted by the released Vault 2.0.0 binary.
///
/// This copy is intentionally independent of `vault_server::db` and should
/// change only if the historical release contract was recorded incorrectly.
const V2_0_0_SCHEMA: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS folders (
        id INTEGER PRIMARY KEY,
        root_key TEXT NOT NULL,
        parent_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        is_root INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        created_by TEXT,
        created_by_name TEXT,
        color TEXT,
        icon TEXT,
        default_ttl_days INTEGER,
        default_ttl_action TEXT
    )
    "#,
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_folders_root_key ON folders(root_key) WHERE is_root = 1",
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_folders_parent_name ON folders(parent_id, name) WHERE is_root = 0",
    r#"
    CREATE TABLE IF NOT EXISTS folder_events (
        id INTEGER PRIMARY KEY,
        folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
        event_type TEXT NOT NULL,
        actor TEXT,
        actor_name TEXT,
        message TEXT,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_folder_events_folder_id ON folder_events(folder_id)",
    r#"
    CREATE TABLE IF NOT EXISTS vault_users (
        id INTEGER PRIMARY KEY,
        issuer TEXT NOT NULL,
        subject TEXT NOT NULL,
        email TEXT,
        name TEXT NOT NULL,
        is_admin INTEGER NOT NULL DEFAULT 0,
        is_active INTEGER NOT NULL DEFAULT 1,
        preferences TEXT NOT NULL DEFAULT '{}',
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        last_login_at TEXT,
        last_seen_at TEXT,
        CONSTRAINT uq_vault_users_identity UNIQUE (issuer, subject)
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_vault_users_email ON vault_users(email)",
    r#"
    CREATE TABLE IF NOT EXISTS vault_groups (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        description TEXT,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        CONSTRAINT uq_vault_groups_name UNIQUE (name)
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS vault_group_memberships (
        id INTEGER PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES vault_users(id) ON DELETE CASCADE,
        group_id INTEGER NOT NULL REFERENCES vault_groups(id) ON DELETE CASCADE,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        CONSTRAINT uq_vault_group_membership UNIQUE (user_id, group_id)
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_vault_group_memberships_user_id ON vault_group_memberships(user_id)",
    "CREATE INDEX IF NOT EXISTS ix_vault_group_memberships_group_id ON vault_group_memberships(group_id)",
    r#"
    CREATE TABLE IF NOT EXISTS folder_permissions (
        id INTEGER PRIMARY KEY,
        folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
        group_id INTEGER NOT NULL REFERENCES vault_groups(id) ON DELETE CASCADE,
        can_view INTEGER NOT NULL DEFAULT 1,
        can_read INTEGER NOT NULL DEFAULT 1,
        can_write INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        CONSTRAINT uq_folder_permission_group UNIQUE (folder_id, group_id)
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_folder_permissions_folder_id ON folder_permissions(folder_id)",
    r#"
    CREATE TABLE IF NOT EXISTS vault_settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS blobs (
        id INTEGER PRIMARY KEY,
        hash_algo TEXT NOT NULL DEFAULT 'sha256',
        hash TEXT NOT NULL,
        size_bytes INTEGER NOT NULL,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        CONSTRAINT uq_blob_identity UNIQUE (hash_algo, hash, size_bytes)
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_blobs_hash_algo ON blobs(hash_algo)",
    "CREATE INDEX IF NOT EXISTS ix_blobs_hash ON blobs(hash)",
    r#"
    CREATE TABLE IF NOT EXISTS blob_locations (
        id INTEGER PRIMARY KEY,
        blob_id INTEGER NOT NULL REFERENCES blobs(id) ON DELETE CASCADE,
        backend TEXT NOT NULL,
        bucket TEXT NOT NULL DEFAULT '',
        object_key TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        CONSTRAINT uq_blob_location UNIQUE (backend, bucket, object_key)
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_blob_locations_blob_id ON blob_locations(blob_id)",
    r#"
    CREATE TABLE IF NOT EXISTS documents (
        id INTEGER PRIMARY KEY,
        folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        description TEXT,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        created_by TEXT,
        created_by_name TEXT,
        latest_modified_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        latest_modified_by TEXT,
        latest_version_number INTEGER,
        version_count INTEGER NOT NULL DEFAULT 0,
        current_version_id TEXT,
        expires_at TEXT,
        expiry_action TEXT,
        archived_from_folder TEXT,
        archived_original_name TEXT,
        archived_access TEXT
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_documents_folder_id ON documents(folder_id)",
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_documents_active_folder_name ON documents(folder_id, name) WHERE archived_from_folder IS NULL",
    r#"
    CREATE TABLE IF NOT EXISTS document_locks (
        id INTEGER PRIMARY KEY,
        document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
        locked_by TEXT NOT NULL,
        locked_by_name TEXT,
        locked_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        is_active INTEGER NOT NULL DEFAULT 1,
        locked_ip TEXT,
        locked_user_agent TEXT,
        force_acquired INTEGER NOT NULL DEFAULT 0,
        released_at TEXT,
        released_by TEXT
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_document_locks_document_id ON document_locks(document_id)",
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_document_locks_active_document ON document_locks(document_id) WHERE is_active = 1",
    r#"
    CREATE TABLE IF NOT EXISTS document_versions (
        id TEXT PRIMARY KEY,
        document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
        blob_id INTEGER NOT NULL REFERENCES blobs(id),
        version_number INTEGER NOT NULL,
        committed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        committed_by TEXT NOT NULL,
        committed_by_name TEXT,
        message TEXT,
        mime_type TEXT,
        original_filename TEXT,
        upload_ip TEXT,
        upload_user_agent TEXT,
        created_via TEXT,
        CONSTRAINT uq_versions_document_number UNIQUE (document_id, version_number)
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_document_versions_document_id ON document_versions(document_id)",
    "CREATE INDEX IF NOT EXISTS ix_document_versions_blob_id ON document_versions(blob_id)",
    r#"
    CREATE TABLE IF NOT EXISTS document_events (
        id INTEGER PRIMARY KEY,
        document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
        event_type TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        actor TEXT NOT NULL,
        actor_name TEXT,
        message TEXT,
        result TEXT,
        ip TEXT,
        user_agent TEXT
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_document_events_document_id ON document_events(document_id)",
    r#"
    CREATE TABLE IF NOT EXISTS upload_sessions (
        id TEXT PRIMARY KEY,
        mode TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active',
        folder_path TEXT,
        document_id INTEGER REFERENCES documents(id) ON DELETE CASCADE,
        filename TEXT NOT NULL,
        total_size INTEGER NOT NULL,
        chunk_size INTEGER NOT NULL,
        part_count INTEGER NOT NULL,
        verification_total_bytes INTEGER NOT NULL DEFAULT 0,
        verification_processed_bytes INTEGER NOT NULL DEFAULT 0,
        mime_type TEXT,
        note TEXT,
        rename_to_upload INTEGER NOT NULL DEFAULT 0,
        created_by TEXT NOT NULL,
        created_by_name TEXT,
        user_context TEXT NOT NULL,
        upload_ip TEXT,
        upload_user_agent TEXT,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        expires_at TEXT NOT NULL,
        completed_at TEXT,
        aborted_at TEXT,
        error TEXT,
        result_document_id INTEGER,
        result_version_id TEXT,
        result_path TEXT
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_upload_sessions_owner_status ON upload_sessions(created_by, status)",
    "CREATE INDEX IF NOT EXISTS ix_upload_sessions_expires_at ON upload_sessions(expires_at)",
    r#"
    CREATE TABLE IF NOT EXISTS upload_parts (
        id INTEGER PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES upload_sessions(id) ON DELETE CASCADE,
        part_number INTEGER NOT NULL,
        offset_bytes INTEGER NOT NULL,
        size_bytes INTEGER NOT NULL,
        sha256 TEXT NOT NULL,
        storage_path TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        CONSTRAINT uq_upload_part_number UNIQUE (session_id, part_number)
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_upload_parts_session_id ON upload_parts(session_id)",
    "CREATE INDEX IF NOT EXISTS ix_upload_parts_session_offset ON upload_parts(session_id, offset_bytes)",
    r#"
    CREATE TABLE IF NOT EXISTS export_jobs (
        id TEXT PRIMARY KEY,
        status TEXT NOT NULL DEFAULT 'queued',
        filename TEXT NOT NULL,
        total_items INTEGER NOT NULL,
        processed_items INTEGER NOT NULL DEFAULT 0,
        total_bytes INTEGER NOT NULL DEFAULT 0,
        processed_bytes INTEGER NOT NULL DEFAULT 0,
        created_by TEXT NOT NULL,
        created_by_name TEXT,
        user_context TEXT NOT NULL,
        request_payload TEXT NOT NULL DEFAULT '{}',
        error TEXT,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        expires_at TEXT NOT NULL,
        completed_at TEXT,
        cancelled_at TEXT
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_export_jobs_created_by_status ON export_jobs(created_by, status)",
    "CREATE INDEX IF NOT EXISTS ix_export_jobs_expires_at ON export_jobs(expires_at)",
    r#"
    CREATE TABLE IF NOT EXISTS export_artifacts (
        id INTEGER PRIMARY KEY,
        job_id TEXT NOT NULL REFERENCES export_jobs(id) ON DELETE CASCADE,
        blob_id INTEGER NOT NULL REFERENCES blobs(id) ON DELETE CASCADE,
        filename TEXT NOT NULL,
        mime_type TEXT NOT NULL,
        size_bytes INTEGER NOT NULL,
        hash_algo TEXT NOT NULL DEFAULT 'sha256',
        hash TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        expires_at TEXT NOT NULL
    )
    "#,
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_export_artifact_job ON export_artifacts(job_id)",
    "CREATE INDEX IF NOT EXISTS ix_export_artifacts_job_id ON export_artifacts(job_id)",
    "CREATE INDEX IF NOT EXISTS ix_export_artifacts_blob_id ON export_artifacts(blob_id)",
    "CREATE INDEX IF NOT EXISTS ix_export_artifacts_expires_at ON export_artifacts(expires_at)",
    r#"
    CREATE TABLE IF NOT EXISTS state_events (
        id INTEGER PRIMARY KEY,
        event_type TEXT NOT NULL,
        resources TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_state_events_created_at ON state_events(created_at)",
    r#"
    CREATE TABLE IF NOT EXISTS share_links (
        id INTEGER PRIMARY KEY,
        code TEXT NOT NULL UNIQUE,
        target_type TEXT NOT NULL,
        document_id INTEGER REFERENCES documents(id) ON DELETE CASCADE,
        folder_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
        access_mode TEXT NOT NULL DEFAULT 'internal',
        created_by TEXT,
        created_by_name TEXT,
        created_by_user_id INTEGER REFERENCES vault_users(id) ON DELETE SET NULL,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        expires_at TEXT,
        disabled_at TEXT,
        item_type TEXT,
        item_id INTEGER
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_share_links_code ON share_links(code)",
    "CREATE INDEX IF NOT EXISTS ix_share_links_document ON share_links(document_id)",
    "CREATE INDEX IF NOT EXISTS ix_share_links_folder ON share_links(folder_id)",
    r#"
    CREATE TABLE IF NOT EXISTS schema_migrations (
        version INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS preview_jobs (
        id INTEGER PRIMARY KEY,
        source_blob_id INTEGER NOT NULL REFERENCES blobs(id) ON DELETE CASCADE,
        recipe TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'queued'
            CHECK (status IN ('queued', 'running', 'ready', 'unsupported', 'failed')),
        attempt_count INTEGER NOT NULL DEFAULT 0,
        lease_token TEXT,
        lease_expires_at TEXT,
        next_attempt_at TEXT,
        last_error_code TEXT,
        last_error_detail TEXT,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        completed_at TEXT,
        last_accessed_at TEXT,
        CONSTRAINT uq_preview_job_source_recipe UNIQUE (source_blob_id, recipe)
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_preview_jobs_dispatch ON preview_jobs(status, next_attempt_at, lease_expires_at, id)",
    "CREATE INDEX IF NOT EXISTS ix_preview_jobs_source_blob ON preview_jobs(source_blob_id)",
    r#"
    CREATE TABLE IF NOT EXISTS preview_renditions (
        id INTEGER PRIMARY KEY,
        preview_job_id INTEGER NOT NULL REFERENCES preview_jobs(id) ON DELETE CASCADE,
        variant TEXT NOT NULL,
        blob_id INTEGER NOT NULL REFERENCES blobs(id),
        mime_type TEXT NOT NULL,
        width INTEGER NOT NULL,
        height INTEGER NOT NULL,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
        CONSTRAINT uq_preview_rendition_variant UNIQUE (preview_job_id, variant)
    )
    "#,
    "CREATE INDEX IF NOT EXISTS ix_preview_renditions_job ON preview_renditions(preview_job_id)",
    "CREATE INDEX IF NOT EXISTS ix_preview_renditions_blob ON preview_renditions(blob_id)",
];

async fn seed_roots_and_ledger(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO folders (
            id, root_key, parent_id, name, is_root, created_at
        )
        VALUES
            (1, 'vault', NULL, '', 1, ?),
            (2, 'archive', NULL, 'Archive', 1, ?)
        ",
    )
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        INSERT INTO schema_migrations (version, name, applied_at)
        VALUES (1, 'content previews', ?)
        ",
    )
    .bind(RELEASE_TIMESTAMP)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_identity_graph(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO vault_users (
            id, issuer, subject, email, name, is_admin, is_active, preferences,
            created_at, last_login_at, last_seen_at
        )
        VALUES
            (
                100, 'https://fixture.invalid', 'alice',
                'alice@fixture.invalid', 'Alice Fixture', 0, 1, '{}',
                '2026-07-22T12:00:00Z', '2026-07-22T14:15:00Z',
                '2026-07-22T14:20:00Z'
            ),
            (
                101, 'https://fixture.invalid', 'eve',
                'eve@fixture.invalid', 'Eve Fixture', 0, 1, '{}',
                '2026-07-22T12:05:00Z', '2026-07-22T14:10:00Z',
                '2026-07-22T14:18:00Z'
            )
        ",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        INSERT INTO vault_groups (id, name, description, created_at)
        VALUES
            (200, 'Fixture Writers', 'Ordinary writers in the migration fixture', ?),
            (201, 'Fixture Readers', 'Read-only users in the migration fixture', ?)
        ",
    )
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        INSERT INTO vault_group_memberships (id, user_id, group_id, created_at)
        VALUES
            (300, 100, 200, ?),
            (301, 101, 201, ?)
        ",
    )
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_folder_graph(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO folders (
            id, root_key, parent_id, name, is_root, created_at, created_by,
            created_by_name, color, icon
        )
        VALUES
            (
                10, 'vault', 1, 'Visual Assets', 0,
                '2026-07-22T12:10:00Z', ?, ?, '#2563eb', 'images'
            ),
            (
                11, 'vault', 10, 'Migration Previews', 0,
                '2026-07-22T12:12:00Z', ?, ?, NULL, NULL
            ),
            (
                12, 'vault', 11, 'Disposable Empty Folder', 0,
                '2026-07-22T12:14:00Z', ?, ?, NULL, NULL
            )
        ",
    )
    .bind(ALICE_ID)
    .bind(ALICE_NAME)
    .bind(ALICE_ID)
    .bind(ALICE_NAME)
    .bind(ALICE_ID)
    .bind(ALICE_NAME)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        r"
        INSERT INTO folder_permissions (
            id, folder_id, group_id, can_view, can_read, can_write, created_at,
            updated_at
        )
        VALUES
            (400, 1, 200, 1, 1, 1, ?, ?),
            (401, 1, 201, 1, 1, 0, ?, ?),
            (402, 2, 200, 1, 1, 1, ?, ?),
            (403, 2, 201, 1, 1, 0, ?, ?),
            (404, 10, 200, 1, 1, 1, ?, ?),
            (405, 10, 201, 1, 1, 0, ?, ?)
        ",
    )
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .bind(RELEASE_TIMESTAMP)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_blob_graph(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    // The migration exercises database relationships only. Physical objects
    // would be dead test data because neither consumer reads them.
    const BLOBS: [(&str, i64); 4] = [
        (
            "3a504af58475594beaa257bab4b9348fbed63bcd87c4a18cb31c0496eeec4480",
            143,
        ),
        (
            "fe405cbd3fd9cc5ae2dbf8ba9ef89f00458ea2def52dd7dfb9953d0ddd1b50ad",
            166,
        ),
        (
            "ed0472b48f6829fe54d6db1ab97afa4566dfc326ec2917720a2a12efe627afc1",
            188,
        ),
        (
            "3deb55095ebcfb503cddd73b41023844e5dcb88c10fa65d5e40d975877799585",
            278,
        ),
    ];
    for (offset, (digest, size_bytes)) in BLOBS.into_iter().enumerate() {
        let offset = i64::try_from(offset)?;
        sqlx::query(
            r"
            INSERT INTO blobs (id, hash_algo, hash, size_bytes, created_at)
            VALUES (?, 'sha256', ?, ?, ?)
            ",
        )
        .bind(500 + offset)
        .bind(digest)
        .bind(size_bytes)
        .bind(format!("2026-07-22T13:2{offset}:00Z"))
        .execute(&mut **transaction)
        .await?;
        sqlx::query(
            r"
            INSERT INTO blob_locations (
                id, blob_id, backend, bucket, object_key, created_at
            )
            VALUES (?, ?, 'local', '', ?, ?)
            ",
        )
        .bind(600 + offset)
        .bind(500 + offset)
        .bind(format!("sha256/{digest}"))
        .bind(format!("2026-07-22T13:2{offset}:00Z"))
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn seed_document_and_preview_graph(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO documents (
            id, folder_id, name, description, created_at, created_by,
            created_by_name, latest_modified_at, latest_modified_by,
            latest_version_number, version_count, current_version_id
        )
        VALUES (
            1000, 11, 'migration-preview.png',
            'A deterministic image and preview graph for upgrade testing',
            '2026-07-22T13:20:00Z', ?, ?, '2026-07-22T13:20:00Z', ?,
            1, 1, '00000000-0000-7000-8000-000000000001'
        )
        ",
    )
    .bind(ALICE_ID)
    .bind(ALICE_NAME)
    .bind(ALICE_ID)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        r"
        INSERT INTO document_versions (
            id, document_id, blob_id, version_number, committed_at,
            committed_by, committed_by_name, message, mime_type,
            original_filename, upload_ip, upload_user_agent, created_via
        )
        VALUES (
            '00000000-0000-7000-8000-000000000001', 1000, 500, 1,
            '2026-07-22T13:20:00Z', ?, ?, 'Initial fixture upload',
            'image/png', 'migration-preview.png', '192.0.2.20',
            'Vault migration fixture', 'upload'
        )
        ",
    )
    .bind(ALICE_ID)
    .bind(ALICE_NAME)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        r"
        INSERT INTO preview_jobs (
            id, source_blob_id, recipe, status, attempt_count, created_at,
            updated_at, completed_at, last_accessed_at
        )
        VALUES (
            700, 500, 'raster-v1', 'ready', 1,
            '2026-07-22T13:21:00Z', '2026-07-22T13:24:00Z',
            '2026-07-22T13:24:00Z', '2026-07-22T14:00:00Z'
        )
        ",
    )
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        r"
        INSERT INTO preview_renditions (
            id, preview_job_id, variant, blob_id, mime_type, width, height,
            created_at
        )
        VALUES
            (
                710, 700, 'small', 501, 'image/webp', 128, 128,
                '2026-07-22T13:22:00Z'
            ),
            (
                711, 700, 'medium', 502, 'image/webp', 256, 256,
                '2026-07-22T13:23:00Z'
            ),
            (
                712, 700, 'large', 503, 'image/webp', 512, 512,
                '2026-07-22T13:24:00Z'
            )
        ",
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_event_graph(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO folder_events (
            id, folder_id, event_type, actor, actor_name, message, created_at
        )
        VALUES
            (
                800, 10, 'create', ?, ?, 'Created Visual Assets',
                '2026-07-22T12:10:00Z'
            ),
            (
                801, 11, 'create', ?, ?,
                'Created Visual Assets/Migration Previews',
                '2026-07-22T12:12:00Z'
            ),
            (
                802, 12, 'create', ?, ?,
                'Created Visual Assets/Migration Previews/Disposable Empty Folder',
                '2026-07-22T12:14:00Z'
            )
        ",
    )
    .bind(ALICE_ID)
    .bind(ALICE_NAME)
    .bind(ALICE_ID)
    .bind(ALICE_NAME)
    .bind(ALICE_ID)
    .bind(ALICE_NAME)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        r"
        INSERT INTO document_events (
            id, document_id, event_type, created_at, actor, actor_name, message,
            result, ip, user_agent
        )
        VALUES (
            900, 1000, 'download', '2026-07-22T14:00:00Z', ?, ?,
            'Downloaded Visual Assets/Migration Previews/migration-preview.png',
            'ok', '192.0.2.20', 'Vault migration fixture'
        )
        ",
    )
    .bind(ALICE_ID)
    .bind(ALICE_NAME)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO state_events (id, event_type, resources, created_at)
        VALUES
            (
                1000, 'folder.created', '["contents","sidebar"]',
                '2026-07-22T12:10:00Z'
            ),
            (
                1001, 'folder.created', '["contents","sidebar"]',
                '2026-07-22T12:12:00Z'
            ),
            (
                1002, 'document.upload',
                '["contents","document_detail","sidebar"]',
                '2026-07-22T13:20:00Z'
            ),
            (
                1003, 'preview.ready', '["contents","document_detail"]',
                '2026-07-22T13:24:00Z'
            ),
            (
                1004, 'document.download', '["document_detail"]',
                '2026-07-22T14:00:00Z'
            )
        "#,
    )
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
        "v2.0.0 fixture integrity check returned {integrity:?}"
    );

    let foreign_key_violations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut *connection)
            .await?;
    anyhow::ensure!(
        foreign_key_violations == 0,
        "v2.0.0 fixture has {foreign_key_violations} foreign-key violations"
    );

    for (table, expected) in [
        ("schema_migrations", 1_i64),
        ("folders", 5),
        ("vault_users", 2),
        ("vault_groups", 2),
        ("vault_group_memberships", 2),
        ("folder_permissions", 6),
        ("folder_events", 3),
        ("documents", 1),
        ("document_versions", 1),
        ("blobs", 4),
        ("blob_locations", 4),
        ("document_events", 1),
        ("state_events", 5),
        ("preview_jobs", 1),
        ("preview_renditions", 3),
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&mut *connection)
            .await?;
        anyhow::ensure!(
            count == expected,
            "v2.0.0 fixture table {table} contains {count} rows; expected {expected}"
        );
    }

    let history = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT version, name, applied_at FROM schema_migrations ORDER BY version",
    )
    .fetch_all(&mut *connection)
    .await?;
    anyhow::ensure!(
        history
            == vec![(
                1,
                "content previews".to_string(),
                RELEASE_TIMESTAMP.to_string(),
            )],
        "v2.0.0 fixture has unexpected migration history: {history:?}"
    );
    Ok(())
}
