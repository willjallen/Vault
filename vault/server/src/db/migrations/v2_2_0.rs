use futures_util::future::BoxFuture;
use sqlx::{Sqlite, Transaction};

use crate::folders::{VAULT_ROOT_KEY, parse_public_folder_path};

use super::super::invariants::{self, RootInvariantDefinition};
use super::MigrationDefinition;

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
    version: 3,
    target_version: "2.2.0",
    name: "preserve stored item identities",
    apply: apply_boxed,
    validate_target: validate_target_boxed,
};

#[derive(Debug, sqlx::FromRow)]
struct LegacyArchivedDocument {
    id: i64,
    name: String,
    archived_from_folder: String,
    archived_original_name: Option<String>,
    archived_at: String,
}

async fn apply(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    add_upload_target_identity(tx).await?;
    add_archive_lifecycle_columns(tx).await?;
    normalize_legacy_archived_documents(tx).await?;
    replace_archive_indexes(tx).await?;
    drop_legacy_restore_columns(tx).await?;
    create_archive_invariant_triggers(tx).await?;
    Ok(())
}

async fn add_upload_target_identity(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    sqlx::query(
        r"
        ALTER TABLE upload_sessions
        ADD COLUMN target_folder_id INTEGER
            REFERENCES folders(id) ON DELETE SET NULL
        ",
    )
    .execute(&mut **tx)
    .await?;

    // A path-only session cannot be backfilled safely: the path may have been
    // reused by a different folder after the upload began. Preserve check-ins,
    // which already bind to a document ID, but require create uploads to restart.
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'failed',
            verification_total_bytes = 0,
            verification_processed_bytes = 0,
            error = 'Upload target identity is unavailable after upgrade; restart the upload',
            updated_at = CURRENT_TIMESTAMP
        WHERE mode = 'create'
          AND status IN ('active', 'completing')
        ",
    )
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        r"
        CREATE INDEX ix_upload_sessions_target_folder_status
        ON upload_sessions(target_folder_id, status)
        WHERE mode = 'create'
        ",
    )
    .execute(&mut **tx)
    .await?;

    sqlx::query(
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
    )
    .execute(&mut **tx)
    .await?;

    sqlx::query(
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
    )
    .execute(&mut **tx)
    .await?;

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
    if invalid_active_create_uploads != 0 {
        anyhow::bail!(
            "upload_target_invariant_failed reason=missing_target_folder \
             active_create_uploads={invalid_active_create_uploads}"
        );
    }
    Ok(())
}

async fn add_archive_lifecycle_columns(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    for statement in [
        "ALTER TABLE documents ADD COLUMN archived_at TEXT",
        "ALTER TABLE documents ADD COLUMN archived_origin_path TEXT",
        "ALTER TABLE folders ADD COLUMN archived_at TEXT",
        "ALTER TABLE folders ADD COLUMN archived_origin_path TEXT",
        "ALTER TABLE folders ADD COLUMN archived_access TEXT",
    ] {
        sqlx::query(statement).execute(&mut **tx).await?;
    }
    Ok(())
}

async fn normalize_legacy_archived_documents(
    tx: &mut Transaction<'_, Sqlite>,
) -> anyhow::Result<()> {
    let archived = sqlx::query_as::<_, LegacyArchivedDocument>(
        r"
        SELECT
            d.id,
            d.name,
            d.archived_from_folder,
            d.archived_original_name,
            COALESCE(
                (
                    SELECT MAX(e.created_at)
                    FROM document_events e
                    WHERE e.document_id = d.id
                      AND e.event_type = 'archive'
                ),
                d.latest_modified_at,
                d.created_at,
                CURRENT_TIMESTAMP
            ) AS archived_at
        FROM documents d
        WHERE d.archived_from_folder IS NOT NULL
        ORDER BY d.id
        ",
    )
    .fetch_all(&mut **tx)
    .await?;

    let vault_root_id: i64 = sqlx::query_scalar(
        "SELECT id FROM folders WHERE root_key = ? AND is_root = 1 AND parent_id IS NULL",
    )
    .bind(VAULT_ROOT_KEY)
    .fetch_one(&mut **tx)
    .await?;

    for document in archived {
        let parsed = parse_public_folder_path(Some(&document.archived_from_folder))?;
        anyhow::ensure!(
            parsed.root_key == VAULT_ROOT_KEY,
            "archived document {} has non-Vault origin {:?}",
            document.id,
            document.archived_from_folder
        );
        let folder_id =
            resolve_or_create_vault_path(tx, vault_root_id, &parsed.relative_path).await?;
        let original_name = document
            .archived_original_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&document.name);
        let origin_path = if parsed.relative_path.is_empty() {
            original_name.to_string()
        } else {
            format!("{}/{original_name}", parsed.relative_path)
        };
        sqlx::query(
            r"
            UPDATE documents
            SET
                folder_id = ?,
                name = ?,
                archived_at = ?,
                archived_origin_path = ?,
                archived_access = COALESCE(archived_access, '{}')
            WHERE id = ?
              AND archived_from_folder = ?
            ",
        )
        .bind(folder_id)
        .bind(original_name)
        .bind(&document.archived_at)
        .bind(origin_path)
        .bind(document.id)
        .bind(&document.archived_from_folder)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn resolve_or_create_vault_path(
    tx: &mut Transaction<'_, Sqlite>,
    vault_root_id: i64,
    relative_path: &str,
) -> anyhow::Result<i64> {
    let mut current_id = vault_root_id;
    for name in relative_path.split('/').filter(|name| !name.is_empty()) {
        if let Some(folder_id) = sqlx::query_scalar::<_, i64>(
            r"
            SELECT id
            FROM folders
            WHERE parent_id = ?
              AND root_key = ?
              AND is_root = 0
              AND name = ?
            ORDER BY id
            LIMIT 1
            ",
        )
        .bind(current_id)
        .bind(VAULT_ROOT_KEY)
        .bind(name)
        .fetch_optional(&mut **tx)
        .await?
        {
            current_id = folder_id;
            continue;
        }
        current_id = sqlx::query(
            r"
            INSERT INTO folders (root_key, parent_id, name, is_root)
            VALUES (?, ?, ?, 0)
            ",
        )
        .bind(VAULT_ROOT_KEY)
        .bind(current_id)
        .bind(name)
        .execute(&mut **tx)
        .await?
        .last_insert_rowid();
    }
    Ok(current_id)
}

async fn replace_archive_indexes(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
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
    ] {
        sqlx::query(statement).execute(&mut **tx).await?;
    }
    Ok(())
}

async fn drop_legacy_restore_columns(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    sqlx::query("ALTER TABLE documents DROP COLUMN archived_from_folder")
        .execute(&mut **tx)
        .await?;
    sqlx::query("ALTER TABLE documents DROP COLUMN archived_original_name")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn create_archive_invariant_triggers(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
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
        sqlx::query(statement).execute(&mut **tx).await?;
    }
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

async fn validate_target(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    invariants::validate(tx, &TARGET_ROOT_FOLDERS, &[]).await?;
    validate_upload_target_identity(tx).await?;
    validate_archive_lifecycle(tx).await
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
