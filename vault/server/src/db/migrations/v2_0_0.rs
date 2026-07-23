#![allow(clippy::needless_raw_string_hashes)]

use futures_util::future::BoxFuture;
use sqlx::{Executor, Sqlite, Transaction};

use super::super::invariants::{self, RootInvariantDefinition, RootNameAlias};
use super::BaselineDefinition;

const BASELINE_MIGRATION_VERSION: i64 = 1;
const BASELINE_TARGET_VERSION: &str = "2.0.0";
const BASELINE_MIGRATION_NAME: &str = "content previews";
const VAULT_ROOT_KEY: &str = "vault";
const VAULT_ROOT_NAME: &str = "";
const LEGACY_VAULT_ROOT_NAME: &str = "Vault";

pub const BASELINE: BaselineDefinition = BaselineDefinition {
    migration_version: BASELINE_MIGRATION_VERSION,
    target_version: BASELINE_TARGET_VERSION,
    migration_name: BASELINE_MIGRATION_NAME,
    install: install_boxed,
    validate: validate_boxed,
};

/// Root metadata emitted by the released v2.0.0 binary.
///
/// These values are intentionally pinned here instead of referring to the
/// current application's definitions. A future migration may change current
/// root behavior without changing this historical bootstrap contract.
const ROOT_FOLDERS: [RootInvariantDefinition; 2] = [
    RootInvariantDefinition {
        key: VAULT_ROOT_KEY,
        stored_name: VAULT_ROOT_NAME,
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
const SUPPORTED_SOURCE_ROOT_NAME_ALIASES: &[RootNameAlias] = &[RootNameAlias {
    root_key: VAULT_ROOT_KEY,
    stored_name: LEGACY_VAULT_ROOT_NAME,
}];

/// Exact schema emitted by Vault v2.0.0, the oldest supported migration source.
///
/// This is a bootstrap baseline, not an executable upgrade target. New
/// databases install it and then traverse the same v2.1.0-and-later migration
/// chain as upgraded v2.0.0 databases.
const SCHEMA_STATEMENTS: &[&str] = &[
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

async fn install(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    for statement in SCHEMA_STATEMENTS {
        tx.execute(*statement).await?;
    }
    insert_root_folders(tx).await?;
    sqlx::query("INSERT INTO schema_migrations (version, name) VALUES (?, ?)")
        .bind(BASELINE_MIGRATION_VERSION)
        .bind(BASELINE_MIGRATION_NAME)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn insert_root_folders(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
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

async fn validate_source(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    invariants::validate(tx, &ROOT_FOLDERS, SUPPORTED_SOURCE_ROOT_NAME_ALIASES).await
}

fn install_boxed<'borrow>(
    tx: &'borrow mut Transaction<'_, Sqlite>,
) -> BoxFuture<'borrow, anyhow::Result<()>> {
    Box::pin(install(tx))
}

fn validate_boxed<'borrow>(
    tx: &'borrow mut Transaction<'_, Sqlite>,
) -> BoxFuture<'borrow, anyhow::Result<()>> {
    Box::pin(validate_source(tx))
}
