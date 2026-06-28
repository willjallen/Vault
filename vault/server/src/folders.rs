use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;

use serde::Serialize;
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool, Transaction};
use thiserror::Error;

use crate::auth::UserContext;
use crate::state_events::state_event_resources_json;

pub const ARCHIVE_ROOT: &str = "Archive";
pub const VAULT_ROOT_KEY: &str = "vault";
pub const ARCHIVE_ROOT_KEY: &str = "archive";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicFolderPath {
    pub root_key: String,
    pub relative_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct FolderRecord {
    pub id: i64,
    pub root_key: String,
    pub parent_id: Option<i64>,
    pub name: String,
    pub is_root: bool,
    pub created_at: Option<String>,
    pub created_by: Option<String>,
    pub created_by_name: Option<String>,
    pub color: Option<String>,
    pub icon: Option<String>,
    pub default_ttl_days: Option<i64>,
    pub default_ttl_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedFolders {
    pub folder: FolderRecord,
    pub created: Vec<FolderRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreatedFolderPayload {
    pub folder: String,
    pub id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderPermissionUpdate {
    pub group_id: i64,
    pub can_view: bool,
    pub can_read: bool,
    pub can_write: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRetentionUpdate {
    pub default_ttl_days: Option<i64>,
    pub default_ttl_action: Option<String>,
}

#[derive(Debug, Error)]
pub enum FolderError {
    #[error("folder path is required")]
    FolderPathRequired,
    #[error("create folders in Vault")]
    CreateFoldersInVault,
    #[error("folder already exists")]
    FolderAlreadyExists,
    #[error("target folder already exists")]
    TargetFolderAlreadyExists,
    #[error("insufficient folder access")]
    InsufficientFolderAccess,
    #[error("folder not found")]
    FolderNotFound,
    #[error("folder name is required")]
    FolderNameRequired,
    #[error("invalid folder name")]
    InvalidFolderName,
    #[error("cannot move a root folder")]
    CannotMoveRootFolder,
    #[error("cannot move a folder into itself")]
    CannotMoveFolderIntoItself,
    #[error("document is locked by another user")]
    DocumentLockedByOtherUser,
    #[error("use archive or restore for Archive moves")]
    UseArchiveOrRestoreForArchiveMoves,
    #[error("invalid folder path")]
    InvalidPath,
    #[error("invalid folder color")]
    InvalidFolderColor,
    #[error("invalid folder icon")]
    InvalidFolderIcon,
    #[error("duplicate group permission")]
    DuplicateGroupPermission,
    #[error("group not found")]
    GroupNotFound,
    #[error("invalid TTL action")]
    InvalidTtlAction,
    #[error("TTL days are required")]
    TtlDaysRequired,
    #[error("TTL days must be between 1 and 3650")]
    TtlDaysOutOfRange,
    #[error("admin access required for delete TTL")]
    DeleteTtlAdminRequired,
    #[error("unknown root key")]
    UnknownRootKey,
    #[error("archive does not contain folders")]
    ArchiveDoesNotContainFolders,
    #[error("write permission requires read and view permission")]
    WriteRequiresReadAndView,
    #[error("read permission requires view permission")]
    ReadRequiresView,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, FromRow)]
struct PermissionRow {
    can_view: bool,
    can_read: bool,
    can_write: bool,
    group_name: String,
}

#[derive(Debug, FromRow)]
struct TtlDocumentRow {
    id: i64,
    folder_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TtlPolicy {
    days: i64,
    action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenameFolderResult {
    pub path: String,
}

#[must_use]
pub fn join_path(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| part.trim_matches('/'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn normalize_folder(folder: Option<&str>) -> Result<String, FolderError> {
    let cleaned = folder.unwrap_or("").trim().replace('\\', "/");
    let cleaned = cleaned.trim_matches('/');
    if cleaned.is_empty() {
        return Ok(String::new());
    }
    let mut parts = Vec::new();
    for part in cleaned.split('/') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part == "." || part == ".." || has_control_char(part) {
            return Err(FolderError::InvalidPath);
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

pub fn parse_public_folder_path(path: Option<&str>) -> Result<PublicFolderPath, FolderError> {
    let normalized = normalize_folder(path)?;
    if normalized == ARCHIVE_ROOT {
        return Ok(PublicFolderPath {
            root_key: ARCHIVE_ROOT_KEY.to_string(),
            relative_path: String::new(),
        });
    }
    if let Some(relative) = normalized.strip_prefix(&format!("{ARCHIVE_ROOT}/")) {
        return Ok(PublicFolderPath {
            root_key: ARCHIVE_ROOT_KEY.to_string(),
            relative_path: relative.to_string(),
        });
    }
    Ok(PublicFolderPath {
        root_key: VAULT_ROOT_KEY.to_string(),
        relative_path: normalized,
    })
}

pub fn public_folder_path(root_key: &str, relative_path: &str) -> Result<String, FolderError> {
    let relative = normalize_folder(Some(relative_path))?;
    match root_key {
        ARCHIVE_ROOT_KEY => {
            if relative.is_empty() {
                Ok(ARCHIVE_ROOT.to_string())
            } else {
                Ok(join_path(&[ARCHIVE_ROOT, &relative]))
            }
        }
        VAULT_ROOT_KEY => Ok(relative),
        _ => Err(FolderError::UnknownRootKey),
    }
}

#[must_use]
pub fn access_level(can_view: bool, can_read: bool, can_write: bool) -> i64 {
    if can_view && can_read && can_write {
        3
    } else if can_view && can_read {
        2
    } else {
        i64::from(can_view)
    }
}

pub fn validate_permission_flags(
    can_view: bool,
    can_read: bool,
    can_write: bool,
) -> Result<(), FolderError> {
    if can_write && (!can_read || !can_view) {
        return Err(FolderError::WriteRequiresReadAndView);
    }
    if can_read && !can_view {
        return Err(FolderError::ReadRequiresView);
    }
    Ok(())
}

pub async fn get_root_folder(
    pool: &SqlitePool,
    root_key: &str,
) -> Result<FolderRecord, FolderError> {
    let name = root_name(root_key)?;
    let insert_result = sqlx::query(
        r"
        INSERT OR IGNORE INTO folders (root_key, parent_id, name, is_root)
        VALUES (?, NULL, ?, 1)
        ",
    )
    .bind(root_key)
    .bind(name)
    .execute(pool)
    .await?;
    let root = fetch_root_folder(pool, root_key).await?;
    if insert_result.rows_affected() > 0 {
        ensure_root_permissions_for_existing_groups(pool, root.id).await?;
    }
    Ok(root)
}

pub async fn ensure_root_folders(
    pool: &SqlitePool,
) -> Result<HashMap<String, FolderRecord>, FolderError> {
    let vault = get_root_folder(pool, VAULT_ROOT_KEY).await?;
    let archive = get_root_folder(pool, ARCHIVE_ROOT_KEY).await?;
    Ok(HashMap::from([
        (VAULT_ROOT_KEY.to_string(), vault),
        (ARCHIVE_ROOT_KEY.to_string(), archive),
    ]))
}

pub async fn get_folder_by_path(
    pool: &SqlitePool,
    path: Option<&str>,
) -> Result<Option<FolderRecord>, FolderError> {
    let parsed = parse_public_folder_path(path)?;
    if parsed.root_key == ARCHIVE_ROOT_KEY && !parsed.relative_path.is_empty() {
        return Ok(None);
    }
    let mut current = get_root_folder(pool, &parsed.root_key).await?;
    if parsed.relative_path.is_empty() {
        return Ok(Some(current));
    }
    for part in parsed.relative_path.split('/') {
        let Some(child) = find_child_folder(pool, current.id, part).await? else {
            return Ok(None);
        };
        current = child;
    }
    Ok(Some(current))
}

pub async fn get_or_create_folder_path(
    pool: &SqlitePool,
    path: Option<&str>,
) -> Result<FolderRecord, FolderError> {
    Ok(get_or_create_folder_path_with_created(pool, path)
        .await?
        .folder)
}

pub async fn get_or_create_folder_path_with_created(
    pool: &SqlitePool,
    path: Option<&str>,
) -> Result<CreatedFolders, FolderError> {
    let parsed = parse_public_folder_path(path)?;
    if parsed.root_key == ARCHIVE_ROOT_KEY && !parsed.relative_path.is_empty() {
        return Err(FolderError::ArchiveDoesNotContainFolders);
    }
    let mut current = get_root_folder(pool, &parsed.root_key).await?;
    let mut created = Vec::new();
    if parsed.relative_path.is_empty() {
        return Ok(CreatedFolders {
            folder: current,
            created,
        });
    }
    for part in parsed.relative_path.split('/') {
        if let Some(child) = find_child_folder(pool, current.id, part).await? {
            current = child;
            continue;
        }
        let inserted = sqlx::query(
            r"
            INSERT INTO folders (root_key, parent_id, name, is_root)
            VALUES (?, ?, ?, 0)
            ",
        )
        .bind(&parsed.root_key)
        .bind(current.id)
        .bind(part)
        .execute(pool)
        .await?;
        current = fetch_folder_by_id(pool, inserted.last_insert_rowid()).await?;
        created.push(current.clone());
    }
    Ok(CreatedFolders {
        folder: current,
        created,
    })
}

pub async fn create_folder_path(
    pool: &SqlitePool,
    path: &str,
    user: &UserContext,
) -> Result<CreatedFolderPayload, FolderError> {
    let normalized = normalize_folder(Some(path))?;
    if normalized.is_empty() {
        return Err(FolderError::FolderPathRequired);
    }
    let parsed = parse_public_folder_path(Some(&normalized))?;
    if parsed.root_key == ARCHIVE_ROOT_KEY {
        return Err(FolderError::CreateFoldersInVault);
    }
    if get_folder_by_path(pool, Some(&normalized)).await?.is_some() {
        return Err(FolderError::FolderAlreadyExists);
    }

    let (parent_path, name) = split_folder_parent_and_name(&normalized)?;
    require_write_for_folder_path(pool, &parent_path, user).await?;

    let mut transaction = pool.begin().await?;
    if get_folder_by_path_in_tx(&mut transaction, &normalized)
        .await?
        .is_some()
    {
        return Err(FolderError::FolderAlreadyExists);
    }
    let parent = get_or_create_vault_folder_path_in_tx(&mut transaction, &parent_path).await?;
    if find_child_folder_in_tx(&mut transaction, parent.id, &name)
        .await?
        .is_some()
    {
        return Err(FolderError::FolderAlreadyExists);
    }
    let inserted = sqlx::query(
        r"
        INSERT INTO folders
            (root_key, parent_id, name, is_root, created_by, created_by_name)
        VALUES
            (?, ?, ?, 0, ?, ?)
        ",
    )
    .bind(&parent.root_key)
    .bind(parent.id)
    .bind(&name)
    .bind(&user.id)
    .bind(&user.name)
    .execute(&mut *transaction)
    .await?;
    let created = fetch_folder_by_id_in_tx(&mut transaction, inserted.last_insert_rowid()).await?;
    record_folder_event_in_tx(
        &mut transaction,
        created.id,
        user,
        "create",
        &format!("Created {normalized}"),
    )
    .await?;
    record_folder_change_in_tx(&mut transaction, "created").await?;
    transaction.commit().await?;

    Ok(CreatedFolderPayload {
        folder: normalized,
        id: created.id,
    })
}

pub async fn update_folder_properties(
    pool: &SqlitePool,
    path: &str,
    color: Option<&str>,
    icon: Option<&str>,
    user: &UserContext,
) -> Result<(), FolderError> {
    let color = sanitize_folder_color(color)?;
    let icon = sanitize_folder_icon(icon)?;
    let folder = get_folder_by_path(pool, Some(path))
        .await?
        .ok_or(FolderError::FolderNotFound)?;
    let level = folder_access_level(pool, folder.id, user).await?;
    if level == 0 {
        return Err(FolderError::FolderNotFound);
    }
    if level < 3 {
        return Err(FolderError::InsufficientFolderAccess);
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("UPDATE folders SET color = ?, icon = ? WHERE id = ?")
        .bind(color)
        .bind(icon)
        .bind(folder.id)
        .execute(&mut *transaction)
        .await?;
    record_folder_event_in_tx(
        &mut transaction,
        folder.id,
        user,
        "metadata",
        "Updated folder appearance",
    )
    .await?;
    record_folder_change_in_tx(&mut transaction, "properties").await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn update_folder_permissions(
    pool: &SqlitePool,
    path: &str,
    permissions: &[FolderPermissionUpdate],
    user: &UserContext,
) -> Result<(), FolderError> {
    let folder = get_folder_by_path(pool, Some(path))
        .await?
        .ok_or(FolderError::FolderNotFound)?;

    let mut seen = HashSet::new();
    for permission in permissions {
        validate_permission_flags(
            permission.can_view,
            permission.can_read,
            permission.can_write,
        )?;
        if !seen.insert(permission.group_id) {
            return Err(FolderError::DuplicateGroupPermission);
        }
    }

    for permission in permissions {
        let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM vault_groups WHERE id = ?")
            .bind(permission.group_id)
            .fetch_optional(pool)
            .await?
            .is_some();
        if !exists {
            return Err(FolderError::GroupNotFound);
        }
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("DELETE FROM folder_permissions WHERE folder_id = ?")
        .bind(folder.id)
        .execute(&mut *transaction)
        .await?;
    for permission in permissions {
        sqlx::query(
            r"
            INSERT INTO folder_permissions
                (folder_id, group_id, can_view, can_read, can_write)
            VALUES
                (?, ?, ?, ?, ?)
            ",
        )
        .bind(folder.id)
        .bind(permission.group_id)
        .bind(permission.can_view)
        .bind(permission.can_read)
        .bind(permission.can_write)
        .execute(&mut *transaction)
        .await?;
    }
    record_folder_event_in_tx(
        &mut transaction,
        folder.id,
        user,
        "permissions",
        "Updated folder permissions",
    )
    .await?;
    record_folder_change_in_tx(&mut transaction, "permissions").await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn update_folder_retention(
    pool: &SqlitePool,
    path: &str,
    update: &FolderRetentionUpdate,
    user: &UserContext,
) -> Result<(), FolderError> {
    let (days, action) = sanitize_ttl_policy(
        update.default_ttl_days,
        update.default_ttl_action.as_deref(),
    )?;
    let folder = get_folder_by_path(pool, Some(path))
        .await?
        .ok_or(FolderError::FolderNotFound)?;
    require_folder_write_access(pool, folder.id, user).await?;
    if matches!(action.as_deref(), Some("delete")) && !user.is_admin {
        return Err(FolderError::DeleteTtlAdminRequired);
    }

    let mut folders = all_folders(pool).await?;
    let Some(target) = folders
        .iter_mut()
        .find(|candidate| candidate.id == folder.id)
    else {
        return Err(FolderError::FolderNotFound);
    };
    target.default_ttl_days = days;
    target.default_ttl_action.clone_from(&action);
    let subtree_ids = subtree_folder_ids_from_records(folder.id, &folders);
    for folder_id in &subtree_ids {
        require_folder_write_access(pool, *folder_id, user).await?;
    }
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder.clone()))
        .collect::<HashMap<_, _>>();

    let mut transaction = pool.begin().await?;
    sqlx::query("UPDATE folders SET default_ttl_days = ?, default_ttl_action = ? WHERE id = ?")
        .bind(days)
        .bind(&action)
        .bind(folder.id)
        .execute(&mut *transaction)
        .await?;
    reapply_ttl_for_subtree_in_tx(&mut transaction, &subtree_ids, &folder_by_id).await?;
    record_folder_event_in_tx(
        &mut transaction,
        folder.id,
        user,
        "retention",
        "Updated folder retention policy",
    )
    .await?;
    record_folder_change_with_resources_in_tx(
        &mut transaction,
        "retention",
        &["contents", "document_detail", "my_edits", "sidebar"],
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn rename_folder(
    pool: &SqlitePool,
    folder_id: i64,
    destination_folder: Option<&str>,
    name: &str,
    user: &UserContext,
) -> Result<RenameFolderResult, FolderError> {
    move_or_rename_folder(pool, folder_id, destination_folder, Some(name), user).await
}

pub async fn move_folder(
    pool: &SqlitePool,
    folder_id: i64,
    destination_folder: &str,
    user: &UserContext,
) -> Result<RenameFolderResult, FolderError> {
    move_or_rename_folder(pool, folder_id, Some(destination_folder), None, user).await
}

async fn move_or_rename_folder(
    pool: &SqlitePool,
    folder_id: i64,
    destination_folder: Option<&str>,
    name: Option<&str>,
    user: &UserContext,
) -> Result<RenameFolderResult, FolderError> {
    let source = fetch_folder_by_id(pool, folder_id).await?;
    if source.is_root {
        return Err(FolderError::CannotMoveRootFolder);
    }
    let target_name = match name {
        Some(name) => normalize_folder_item_name(name)?,
        None => source.name.clone(),
    };
    require_folder_write_access(pool, source.id, user).await?;

    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let source_path = folder_path_from_cache(&source, &path_cache)?;
    let Some(source_parent_id) = source.parent_id else {
        return Err(FolderError::CannotMoveRootFolder);
    };
    let source_parent = folders
        .iter()
        .find(|folder| folder.id == source_parent_id)
        .ok_or(FolderError::FolderNotFound)?;
    let source_parent_path = folder_path_from_cache(source_parent, &path_cache)?;
    let destination_path = match destination_folder {
        Some(path) => normalize_folder(Some(path))?,
        None => source_parent_path.clone(),
    };
    let target_ref = parse_public_folder_path(Some(&destination_path))?;
    if source.root_key != target_ref.root_key {
        return Err(FolderError::UseArchiveOrRestoreForArchiveMoves);
    }
    let target_parent_path = public_folder_path(&target_ref.root_key, &target_ref.relative_path)?;
    let target_path = join_path(&[&target_parent_path, &target_name]);
    if target_path == source_path {
        return Ok(RenameFolderResult { path: source_path });
    }
    if target_path.starts_with(&format!("{source_path}/")) {
        return Err(FolderError::CannotMoveFolderIntoItself);
    }

    let subtree_ids = subtree_folder_ids_from_records(source.id, &folders);
    for subtree_id in &subtree_ids {
        require_folder_write_access(pool, *subtree_id, user).await?;
    }
    ensure_subtree_not_locked_by_other(pool, &subtree_ids, user).await?;
    require_write_for_folder_path(pool, &destination_path, user).await?;

    let mut transaction = pool.begin().await?;
    let target_parent =
        get_or_create_folder_path_in_tx(&mut transaction, &destination_path).await?;
    match find_child_folder_in_tx(&mut transaction, target_parent.id, &target_name).await? {
        Some(existing) if existing.id != source.id => {
            return Err(FolderError::TargetFolderAlreadyExists);
        }
        _ => {}
    }
    sqlx::query("UPDATE folders SET parent_id = ?, name = ? WHERE id = ?")
        .bind(target_parent.id)
        .bind(&target_name)
        .bind(source.id)
        .execute(&mut *transaction)
        .await?;

    let mut updated_folders = all_folders_in_tx(&mut transaction).await?;
    let updated_subtree_ids = subtree_folder_ids_from_records(source.id, &updated_folders);
    let folder_by_id = updated_folders
        .drain(..)
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    reapply_ttl_for_subtree_in_tx(&mut transaction, &updated_subtree_ids, &folder_by_id).await?;

    let event_type = if source_parent_path == target_parent_path && target_name != source.name {
        "rename"
    } else {
        "move"
    };
    let message = if event_type == "rename" {
        format!("Renamed from {} to {target_name}", source.name)
    } else {
        format!("Moved from {source_path} to {target_path}")
    };
    record_folder_event_in_tx(&mut transaction, source.id, user, event_type, &message).await?;
    transaction.commit().await?;
    Ok(RenameFolderResult { path: target_path })
}

pub async fn all_folders(pool: &SqlitePool) -> Result<Vec<FolderRecord>, FolderError> {
    Ok(sqlx::query_as::<_, FolderRecord>(folder_select_sql())
        .fetch_all(pool)
        .await?)
}

pub async fn apply_effective_ttl_to_document_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
    folder_id: i64,
) -> Result<(), FolderError> {
    let folder_by_id = all_folders_in_tx(transaction)
        .await?
        .into_iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    if let Some(policy) = effective_ttl_policy_for_folder(folder_id, &folder_by_id) {
        sqlx::query(
            r"
            UPDATE documents
            SET
                expires_at = datetime(latest_modified_at, '+' || ? || ' days'),
                expiry_action = ?
            WHERE id = ?
            ",
        )
        .bind(policy.days)
        .bind(&policy.action)
        .bind(document_id)
        .execute(&mut **transaction)
        .await?;
    } else {
        sqlx::query("UPDATE documents SET expires_at = NULL, expiry_action = NULL WHERE id = ?")
            .bind(document_id)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
}

pub async fn folder_path_by_id(pool: &SqlitePool, folder_id: i64) -> Result<String, FolderError> {
    let folders = all_folders(pool).await?;
    let cache = build_folder_path_cache(&folders)?;
    let Some(folder) = folders.iter().find(|folder| folder.id == folder_id) else {
        return Err(sqlx::Error::RowNotFound.into());
    };
    folder_path_from_cache(folder, &cache)
}

pub fn build_folder_path_cache(
    folders: &[FolderRecord],
) -> Result<HashMap<i64, String>, FolderError> {
    let by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let mut cache = HashMap::new();
    for folder in folders {
        compute_folder_path(folder.id, &by_id, &mut cache, &mut HashSet::new())?;
    }
    Ok(cache)
}

pub fn folder_path_from_cache<S: BuildHasher>(
    folder: &FolderRecord,
    cache: &HashMap<i64, String, S>,
) -> Result<String, FolderError> {
    if let Some(path) = cache.get(&folder.id) {
        return Ok(path.clone());
    }
    public_folder_path(&folder.root_key, &folder.name)
}

pub async fn folder_access_level(
    pool: &SqlitePool,
    folder_id: i64,
    user: &UserContext,
) -> Result<i64, FolderError> {
    if user.is_admin {
        return Ok(3);
    }
    let groups = user_group_names(user);
    let ancestor_ids = folder_ancestor_ids(pool, folder_id).await?;
    for ancestor_id in ancestor_ids {
        let rows = sqlx::query_as::<_, PermissionRow>(
            r"
            SELECT
                fp.can_view,
                fp.can_read,
                fp.can_write,
                vg.name AS group_name
            FROM folder_permissions fp
            JOIN vault_groups vg ON vg.id = fp.group_id
            WHERE fp.folder_id = ?
            ",
        )
        .bind(ancestor_id)
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            continue;
        }
        return Ok(rows
            .iter()
            .filter(|row| groups.contains(&row.group_name.trim().to_ascii_lowercase()))
            .map(|row| access_level(row.can_view, row.can_read, row.can_write))
            .max()
            .unwrap_or(0));
    }
    Ok(0)
}

pub async fn nearest_existing_folder_for_path(
    pool: &SqlitePool,
    path: &str,
) -> Result<FolderRecord, FolderError> {
    let parsed = parse_public_folder_path(Some(path))?;
    let mut current = get_root_folder(pool, &parsed.root_key).await?;
    if parsed.relative_path.is_empty() {
        return Ok(current);
    }
    for part in parsed.relative_path.split('/') {
        let Some(child) = find_child_folder(pool, current.id, part).await? else {
            return Ok(current);
        };
        current = child;
    }
    Ok(current)
}

pub async fn require_write_for_folder_path(
    pool: &SqlitePool,
    path: &str,
    user: &UserContext,
) -> Result<(), FolderError> {
    let folder = nearest_existing_folder_for_path(pool, path).await?;
    let level = folder_access_level(pool, folder.id, user).await?;
    if level >= 3 {
        return Ok(());
    }
    if level > 0 {
        return Err(FolderError::InsufficientFolderAccess);
    }
    Err(FolderError::FolderNotFound)
}

pub async fn require_folder_read_access(
    pool: &SqlitePool,
    folder_id: i64,
    user: &UserContext,
) -> Result<(), FolderError> {
    let level = folder_access_level(pool, folder_id, user).await?;
    if level >= 2 {
        return Ok(());
    }
    if level > 0 {
        return Err(FolderError::InsufficientFolderAccess);
    }
    Err(FolderError::FolderNotFound)
}

pub async fn require_folder_write_access(
    pool: &SqlitePool,
    folder_id: i64,
    user: &UserContext,
) -> Result<(), FolderError> {
    let level = folder_access_level(pool, folder_id, user).await?;
    if level >= 3 {
        return Ok(());
    }
    if level > 0 {
        return Err(FolderError::InsufficientFolderAccess);
    }
    Err(FolderError::FolderNotFound)
}

pub async fn add_folder_permission(
    pool: &SqlitePool,
    folder_id: i64,
    group_id: i64,
    can_view: bool,
    can_read: bool,
    can_write: bool,
) -> Result<(), FolderError> {
    validate_permission_flags(can_view, can_read, can_write)?;
    sqlx::query(
        r"
        INSERT INTO folder_permissions (folder_id, group_id, can_view, can_read, can_write)
        VALUES (?, ?, ?, ?, ?)
        ON CONFLICT(folder_id, group_id)
        DO UPDATE SET
            can_view = excluded.can_view,
            can_read = excluded.can_read,
            can_write = excluded.can_write,
            updated_at = CURRENT_TIMESTAMP
        ",
    )
    .bind(folder_id)
    .bind(group_id)
    .bind(can_view)
    .bind(can_read)
    .bind(can_write)
    .execute(pool)
    .await?;
    Ok(())
}

async fn fetch_root_folder(pool: &SqlitePool, root_key: &str) -> Result<FolderRecord, FolderError> {
    Ok(sqlx::query_as::<_, FolderRecord>(&format!(
        "{} WHERE root_key = ? AND is_root = 1",
        folder_select_sql()
    ))
    .bind(root_key)
    .fetch_one(pool)
    .await?)
}

async fn fetch_folder_by_id(pool: &SqlitePool, id: i64) -> Result<FolderRecord, FolderError> {
    Ok(
        sqlx::query_as::<_, FolderRecord>(&format!("{} WHERE id = ?", folder_select_sql()))
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

async fn fetch_root_folder_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    root_key: &str,
) -> Result<FolderRecord, FolderError> {
    Ok(sqlx::query_as::<_, FolderRecord>(&format!(
        "{} WHERE root_key = ? AND is_root = 1",
        folder_select_sql()
    ))
    .bind(root_key)
    .fetch_one(&mut **transaction)
    .await?)
}

async fn fetch_folder_by_id_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    id: i64,
) -> Result<FolderRecord, FolderError> {
    Ok(
        sqlx::query_as::<_, FolderRecord>(&format!("{} WHERE id = ?", folder_select_sql()))
            .bind(id)
            .fetch_one(&mut **transaction)
            .await?,
    )
}

async fn find_child_folder(
    pool: &SqlitePool,
    parent_id: i64,
    name: &str,
) -> Result<Option<FolderRecord>, FolderError> {
    Ok(sqlx::query_as::<_, FolderRecord>(&format!(
        "{} WHERE parent_id = ? AND name = ?",
        folder_select_sql()
    ))
    .bind(parent_id)
    .bind(name)
    .fetch_optional(pool)
    .await?)
}

async fn find_child_folder_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    parent_id: i64,
    name: &str,
) -> Result<Option<FolderRecord>, FolderError> {
    Ok(sqlx::query_as::<_, FolderRecord>(&format!(
        "{} WHERE parent_id = ? AND name = ?",
        folder_select_sql()
    ))
    .bind(parent_id)
    .bind(name)
    .fetch_optional(&mut **transaction)
    .await?)
}

async fn all_folders_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<Vec<FolderRecord>, FolderError> {
    Ok(sqlx::query_as::<_, FolderRecord>(folder_select_sql())
        .fetch_all(&mut **transaction)
        .await?)
}

async fn get_folder_by_path_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    path: &str,
) -> Result<Option<FolderRecord>, FolderError> {
    let parsed = parse_public_folder_path(Some(path))?;
    if parsed.root_key == ARCHIVE_ROOT_KEY && !parsed.relative_path.is_empty() {
        return Ok(None);
    }
    let mut current = fetch_root_folder_in_tx(transaction, &parsed.root_key).await?;
    if parsed.relative_path.is_empty() {
        return Ok(Some(current));
    }
    for part in parsed.relative_path.split('/') {
        let Some(child) = find_child_folder_in_tx(transaction, current.id, part).await? else {
            return Ok(None);
        };
        current = child;
    }
    Ok(Some(current))
}

async fn get_or_create_vault_folder_path_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    path: &str,
) -> Result<FolderRecord, FolderError> {
    let relative_path = normalize_folder(Some(path))?;
    get_or_create_folder_path_parts_in_tx(transaction, VAULT_ROOT_KEY, &relative_path).await
}

pub async fn get_or_create_folder_path_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    path: &str,
) -> Result<FolderRecord, FolderError> {
    let parsed = parse_public_folder_path(Some(path))?;
    if parsed.root_key == ARCHIVE_ROOT_KEY && !parsed.relative_path.is_empty() {
        return Err(FolderError::ArchiveDoesNotContainFolders);
    }
    get_or_create_folder_path_parts_in_tx(transaction, &parsed.root_key, &parsed.relative_path)
        .await
}

async fn get_or_create_folder_path_parts_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    root_key: &str,
    relative_path: &str,
) -> Result<FolderRecord, FolderError> {
    let mut current = fetch_root_folder_in_tx(transaction, root_key).await?;
    if relative_path.is_empty() {
        return Ok(current);
    }
    for part in relative_path.split('/') {
        if let Some(child) = find_child_folder_in_tx(transaction, current.id, part).await? {
            current = child;
            continue;
        }
        let inserted = sqlx::query(
            r"
            INSERT INTO folders (root_key, parent_id, name, is_root)
            VALUES (?, ?, ?, 0)
            ",
        )
        .bind(root_key)
        .bind(current.id)
        .bind(part)
        .execute(&mut **transaction)
        .await?;
        current = fetch_folder_by_id_in_tx(transaction, inserted.last_insert_rowid()).await?;
    }
    Ok(current)
}

async fn folder_ancestor_ids(pool: &SqlitePool, folder_id: i64) -> Result<Vec<i64>, FolderError> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut current_id = Some(folder_id);
    while let Some(id) = current_id {
        if !seen.insert(id) {
            break;
        }
        let folder = fetch_folder_by_id(pool, id).await?;
        ids.push(folder.id);
        current_id = folder.parent_id;
    }
    Ok(ids)
}

fn compute_folder_path(
    folder_id: i64,
    by_id: &HashMap<i64, &FolderRecord>,
    cache: &mut HashMap<i64, String>,
    visiting: &mut HashSet<i64>,
) -> Result<String, FolderError> {
    if let Some(path) = cache.get(&folder_id) {
        return Ok(path.clone());
    }
    let Some(folder) = by_id.get(&folder_id) else {
        return Ok(String::new());
    };
    if !visiting.insert(folder_id) {
        let fallback = public_folder_path(&folder.root_key, &folder.name)?;
        cache.insert(folder_id, fallback.clone());
        return Ok(fallback);
    }
    let path = if folder.is_root || folder.parent_id.is_none() {
        public_folder_path(&folder.root_key, "")?
    } else if let Some(parent_id) = folder.parent_id {
        if by_id.contains_key(&parent_id) {
            let parent_path = compute_folder_path(parent_id, by_id, cache, visiting)?;
            join_path(&[&parent_path, &folder.name])
        } else {
            public_folder_path(&folder.root_key, &folder.name)?
        }
    } else {
        public_folder_path(&folder.root_key, &folder.name)?
    };
    visiting.remove(&folder_id);
    cache.insert(folder_id, path.clone());
    Ok(path)
}

async fn record_folder_event_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    folder_id: i64,
    user: &UserContext,
    event_type: &str,
    message: &str,
) -> Result<(), FolderError> {
    sqlx::query(
        r"
        INSERT INTO folder_events (folder_id, event_type, actor, actor_name, message)
        VALUES (?, ?, ?, ?, ?)
        ",
    )
    .bind(folder_id)
    .bind(event_type)
    .bind(&user.id)
    .bind(&user.name)
    .bind(message)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn record_folder_change_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    event_type: &str,
) -> Result<(), FolderError> {
    record_folder_change_with_resources_in_tx(transaction, event_type, &["contents", "sidebar"])
        .await
}

async fn record_folder_change_with_resources_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    event_type: &str,
    resources: &[&str],
) -> Result<(), FolderError> {
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES (?, ?)
        ",
    )
    .bind(format!("folder.{event_type}"))
    .bind(state_event_resources_json(resources))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn split_folder_parent_and_name(normalized: &str) -> Result<(String, String), FolderError> {
    let mut parts = normalized.rsplitn(2, '/');
    let name = normalize_item_name(parts.next())?;
    let parent = parts.next().unwrap_or_default().to_string();
    Ok((parent, name))
}

fn normalize_item_name(name: Option<&str>) -> Result<String, FolderError> {
    let cleaned = name
        .unwrap_or_default()
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return Err(FolderError::FolderPathRequired);
    }
    if cleaned == "." || cleaned == ".." || cleaned.contains('/') || has_control_char(&cleaned) {
        return Err(FolderError::InvalidPath);
    }
    Ok(cleaned)
}

fn normalize_folder_item_name(name: &str) -> Result<String, FolderError> {
    let cleaned = name
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return Err(FolderError::FolderNameRequired);
    }
    if cleaned == "." || cleaned == ".." || cleaned.contains('/') || has_control_char(&cleaned) {
        return Err(FolderError::InvalidFolderName);
    }
    Ok(cleaned)
}

fn sanitize_folder_color(value: Option<&str>) -> Result<Option<String>, FolderError> {
    let normalized = value.unwrap_or_default().trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Ok(None);
    }
    if matches!(
        normalized.as_str(),
        "blue" | "teal" | "green" | "amber" | "rose" | "violet" | "slate"
    ) {
        Ok(Some(normalized))
    } else {
        Err(FolderError::InvalidFolderColor)
    }
}

fn sanitize_folder_icon(value: Option<&str>) -> Result<Option<String>, FolderError> {
    let normalized = value.unwrap_or_default().trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Ok(None);
    }
    let mut chars = normalized.chars();
    let Some(first) = chars.next() else {
        return Ok(None);
    };
    if normalized.len() > 64 || !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(FolderError::InvalidFolderIcon);
    }
    if chars.all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    }) {
        Ok(Some(normalized))
    } else {
        Err(FolderError::InvalidFolderIcon)
    }
}

fn sanitize_ttl_policy(
    days: Option<i64>,
    action: Option<&str>,
) -> Result<(Option<i64>, Option<String>), FolderError> {
    let normalized_action = action.unwrap_or_default().trim().to_ascii_lowercase();
    if normalized_action.is_empty() || normalized_action == "none" {
        return Ok((None, None));
    }
    if !matches!(normalized_action.as_str(), "archive" | "delete") {
        return Err(FolderError::InvalidTtlAction);
    }
    let Some(days) = days else {
        return Err(FolderError::TtlDaysRequired);
    };
    if !(1..=3650).contains(&days) {
        return Err(FolderError::TtlDaysOutOfRange);
    }
    Ok((Some(days), Some(normalized_action)))
}

async fn reapply_ttl_for_subtree_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    subtree_ids: &[i64],
    folder_by_id: &HashMap<i64, FolderRecord>,
) -> Result<(), FolderError> {
    if subtree_ids.is_empty() {
        return Ok(());
    }
    let mut builder =
        QueryBuilder::<Sqlite>::new("SELECT id, folder_id FROM documents WHERE folder_id IN (");
    let mut separated = builder.separated(", ");
    for folder_id in subtree_ids {
        separated.push_bind(folder_id);
    }
    separated.push_unseparated(")");
    let docs = builder
        .build_query_as::<TtlDocumentRow>()
        .fetch_all(&mut **transaction)
        .await?;
    for doc in docs {
        if let Some(policy) = effective_ttl_policy_for_folder(doc.folder_id, folder_by_id) {
            sqlx::query(
                r"
                UPDATE documents
                SET
                    expires_at = datetime(latest_modified_at, '+' || ? || ' days'),
                    expiry_action = ?
                WHERE id = ?
                ",
            )
            .bind(policy.days)
            .bind(&policy.action)
            .bind(doc.id)
            .execute(&mut **transaction)
            .await?;
        } else {
            sqlx::query(
                "UPDATE documents SET expires_at = NULL, expiry_action = NULL WHERE id = ?",
            )
            .bind(doc.id)
            .execute(&mut **transaction)
            .await?;
        }
    }
    Ok(())
}

fn effective_ttl_policy_for_folder(
    folder_id: i64,
    folder_by_id: &HashMap<i64, FolderRecord>,
) -> Option<TtlPolicy> {
    let folder = folder_by_id.get(&folder_id)?;
    let mut current = Some(folder);
    let mut seen = HashSet::new();
    while let Some(candidate) = current {
        if !seen.insert(candidate.id) {
            break;
        }
        if let Some(policy) = direct_ttl_policy(candidate) {
            if policy.action == "archive" && folder_is_archive(folder) {
                return None;
            }
            return Some(policy);
        }
        current = candidate
            .parent_id
            .and_then(|parent_id| folder_by_id.get(&parent_id));
    }
    None
}

fn direct_ttl_policy(folder: &FolderRecord) -> Option<TtlPolicy> {
    let days = folder.default_ttl_days?;
    if days < 1 {
        return None;
    }
    let action = folder
        .default_ttl_action
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(action.as_str(), "archive" | "delete").then_some(TtlPolicy { days, action })
}

#[must_use]
pub fn subtree_folder_ids_from_records(root_id: i64, folders: &[FolderRecord]) -> Vec<i64> {
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    for folder in folders {
        if let Some(parent_id) = folder.parent_id {
            children.entry(parent_id).or_default().push(folder.id);
        }
    }
    let mut pending = vec![root_id];
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    while let Some(folder_id) = pending.pop() {
        if !seen.insert(folder_id) {
            continue;
        }
        ids.push(folder_id);
        if let Some(child_ids) = children.get(&folder_id) {
            pending.extend(child_ids);
        }
    }
    ids
}

async fn ensure_subtree_not_locked_by_other(
    pool: &SqlitePool,
    folder_ids: &[i64],
    user: &UserContext,
) -> Result<(), FolderError> {
    if user.is_admin || folder_ids.is_empty() {
        return Ok(());
    }
    let mut builder = QueryBuilder::<Sqlite>::new(
        r"
        SELECT dl.id
        FROM document_locks dl
        JOIN documents d ON d.id = dl.document_id
        WHERE dl.is_active = 1
          AND dl.locked_by <>
        ",
    );
    builder.push_bind(&user.id);
    builder.push(" AND d.folder_id IN (");
    let mut separated = builder.separated(", ");
    for folder_id in folder_ids {
        separated.push_bind(folder_id);
    }
    separated.push_unseparated(") LIMIT 1");
    let locked_by_other = builder
        .build_query_scalar::<i64>()
        .fetch_optional(pool)
        .await?
        .is_some();
    if locked_by_other {
        return Err(FolderError::DocumentLockedByOtherUser);
    }
    Ok(())
}

fn folder_is_archive(folder: &FolderRecord) -> bool {
    folder.root_key == ARCHIVE_ROOT_KEY
}

async fn ensure_root_permissions_for_existing_groups(
    pool: &SqlitePool,
    folder_id: i64,
) -> Result<(), FolderError> {
    sqlx::query(
        r"
        INSERT OR IGNORE INTO folder_permissions
            (folder_id, group_id, can_view, can_read, can_write)
        SELECT ?, id, 1, 1, 1 FROM vault_groups
        ",
    )
    .bind(folder_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn user_group_names(user: &UserContext) -> HashSet<String> {
    user.groups
        .iter()
        .filter_map(|group| {
            let group = group.trim().to_ascii_lowercase();
            if group.is_empty() { None } else { Some(group) }
        })
        .collect()
}

fn root_name(root_key: &str) -> Result<&'static str, FolderError> {
    match root_key {
        VAULT_ROOT_KEY => Ok("Vault"),
        ARCHIVE_ROOT_KEY => Ok("Archive"),
        _ => Err(FolderError::UnknownRootKey),
    }
}

fn has_control_char(value: &str) -> bool {
    value
        .chars()
        .any(|character| character < ' ' || character == '\u{7f}')
}

fn folder_select_sql() -> &'static str {
    r"
    SELECT
        id,
        root_key,
        parent_id,
        name,
        is_root,
        created_at,
        created_by,
        created_by_name,
        color,
        icon,
        default_ttl_days,
        default_ttl_action
    FROM folders
    "
}
