use futures_util::future::BoxFuture;
use sqlx::{Sqlite, Transaction};

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
    name: "bind uploads to target folders",
    apply: apply_boxed,
    validate_target: validate_target_boxed,
};

async fn apply(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
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

async fn validate_target(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    invariants::validate(tx, &TARGET_ROOT_FOLDERS, &[]).await?;
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
