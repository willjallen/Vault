use futures_util::future::BoxFuture;
use sqlx::{Sqlite, Transaction};

use super::super::invariants::{self, RootInvariantDefinition};
use super::MigrationDefinition;

const VAULT_ROOT_KEY: &str = "vault";
const SOURCE_LEGACY_VAULT_ROOT_NAME: &str = "Vault";
const TARGET_VAULT_ROOT_NAME: &str = "";
const TARGET_ROOT_FOLDERS: [RootInvariantDefinition; 2] = [
    RootInvariantDefinition {
        key: VAULT_ROOT_KEY,
        stored_name: TARGET_VAULT_ROOT_NAME,
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
    version: 2,
    target_version: "2.1.0",
    name: "normalize root folders",
    apply: apply_boxed,
    validate_target: validate_target_boxed,
};

/// Compatibility transition for the root representation required by strict
/// folder resolution. Existing migration 1 databases must not skip this step.
async fn apply(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    let vault_roots = sqlx::query_as::<_, (i64, String)>(
        r"
        SELECT id, name
        FROM folders
        WHERE root_key = ?
          AND is_root = 1
          AND parent_id IS NULL
        ",
    )
    .bind(VAULT_ROOT_KEY)
    .fetch_all(&mut **tx)
    .await?;
    if vault_roots.len() != 1 {
        anyhow::bail!(
            "vault root normalization requires exactly one structurally valid root; found {}",
            vault_roots.len()
        );
    }
    let vault_root = &vault_roots[0];

    match vault_root.1.as_str() {
        TARGET_VAULT_ROOT_NAME => {}
        SOURCE_LEGACY_VAULT_ROOT_NAME => {
            let result = sqlx::query(
                r"
                UPDATE folders
                SET name = ?
                WHERE id = ?
                  AND root_key = ?
                  AND is_root = 1
                  AND parent_id IS NULL
                  AND name = ?
                ",
            )
            .bind(TARGET_VAULT_ROOT_NAME)
            .bind(vault_root.0)
            .bind(VAULT_ROOT_KEY)
            .bind(SOURCE_LEGACY_VAULT_ROOT_NAME)
            .execute(&mut **tx)
            .await?;
            if result.rows_affected() != 1 {
                anyhow::bail!(
                    "vault root normalization expected to update one row; updated {}",
                    result.rows_affected()
                );
            }
        }
        _ => anyhow::bail!(
            "vault root normalization found an unsupported stored name on folder {}",
            vault_root.0
        ),
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
    Box::pin(invariants::validate(tx, &TARGET_ROOT_FOLDERS, &[]))
}
