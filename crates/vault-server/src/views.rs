use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{FromRow, SqlitePool};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

use crate::auth::UserContext;
use crate::documents::{
    AccessPayload, DocumentError, DocumentRecord, access_payload, document_access_level,
};
use crate::folders::{
    ARCHIVE_ROOT, ARCHIVE_ROOT_KEY, FolderError, FolderRecord, VAULT_ROOT_KEY, all_folders,
    build_folder_path_cache, ensure_root_folders, folder_access_level, folder_path_from_cache,
    get_folder_by_path, get_root_folder, join_path, normalize_folder,
};
use crate::preferences::{PreferenceError, preferences_for_user};
use crate::site_settings::{SiteSettingsError, site_settings_for_db};
use crate::version::app_version;

const SIZE_UNITS: [(&str, i128); 4] = [
    ("KB", 1024),
    ("MB", 1024 * 1024),
    ("GB", 1024 * 1024 * 1024),
    ("TB", 1024 * 1024 * 1024 * 1024),
];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[derive(Debug, Error)]
pub enum ViewError {
    #[error("folder not found")]
    FolderNotFound,
    #[error("document not found")]
    DocumentNotFound,
    #[error("insufficient document access")]
    InsufficientDocumentAccess,
    #[error("current document version metadata is inconsistent")]
    InconsistentDocumentVersion,
    #[error(transparent)]
    Preferences(#[from] PreferenceError),
    #[error(transparent)]
    SiteSettings(#[from] SiteSettingsError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Folder(#[from] FolderError),
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SidebarPayload {
    pub folder_children: HashMap<String, Vec<String>>,
    pub folder_metadata: HashMap<String, FolderMetadataPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BootstrapPayload {
    pub auth_mode: String,
    pub base_domain: String,
    pub dev_mode: bool,
    pub site_name: String,
    pub user: UserContext,
    pub preferences: Value,
    pub settings: Value,
    pub version: String,
    pub current_folder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FolderMetadataPayload {
    pub id: i64,
    pub color: String,
    pub icon: String,
    pub access: AccessPayload,
    pub default_ttl_days: Option<i64>,
    pub default_ttl_action: String,
    pub effective_ttl_days: Option<i64>,
    pub effective_ttl_action: String,
    pub effective_ttl_source_id: Option<i64>,
    pub effective_ttl_inherited: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContentsPayload {
    pub folder: String,
    pub q: String,
    pub recursive: bool,
    pub folders: Vec<FolderSummaryPayload>,
    pub documents: Vec<DocumentRowPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MyEditsPayload {
    pub documents: Vec<DocumentRowPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocumentDetailPayload {
    #[serde(flatten)]
    pub row: DocumentRowPayload,
    pub versions: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitialStatePayload {
    pub bootstrap: BootstrapPayload,
    pub contents: ContentsPayload,
    pub sidebar: SidebarPayload,
    pub my_edits: MyEditsPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FolderSummaryPayload {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub color: String,
    pub icon: String,
    pub default_ttl_days: Option<i64>,
    pub default_ttl_action: String,
    pub effective_ttl_days: Option<i64>,
    pub effective_ttl_action: String,
    pub effective_ttl_source_id: Option<i64>,
    pub effective_ttl_inherited: bool,
    pub latest_by: Option<String>,
    pub modified_at: Option<String>,
    pub modified_display: String,
    pub size_bytes: i64,
    pub size_display: String,
    pub access: AccessPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocumentRowPayload {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub folder: String,
    pub archived_from_folder: String,
    pub archived_original_name: String,
    pub archived_original_path: String,
    pub modified_at: Option<String>,
    pub modified_display: String,
    pub latest_by: Option<String>,
    pub latest_message: Option<String>,
    pub latest_version_number: Option<i64>,
    pub version_count: i64,
    pub created_by: Option<String>,
    pub created_by_name: Option<String>,
    pub created_at: Option<String>,
    pub size_bytes: Option<i64>,
    pub size_display: String,
    pub download_url: Option<String>,
    pub lock: LockPayload,
    pub archived: bool,
    pub expires_at: Option<String>,
    pub expiry_action: Option<String>,
    pub access: AccessPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockPayload {
    pub by: Option<String>,
    pub name: Option<String>,
    pub at: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub force_acquired: Option<bool>,
}

#[derive(Debug, Clone)]
struct DocStat {
    folder: String,
    size_bytes: i64,
    mtime: Option<String>,
    latest_by: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct DocumentViewRow {
    id: i64,
    folder_id: i64,
    name: String,
    created_at: Option<String>,
    created_by: Option<String>,
    created_by_name: Option<String>,
    latest_version_number: Option<i64>,
    version_count: i64,
    expires_at: Option<String>,
    expiry_action: Option<String>,
    archived_from_folder: Option<String>,
    archived_original_name: Option<String>,
    archived_access: Option<String>,
    current_version_id: Option<String>,
    has_versions: bool,
    latest_version_id: Option<String>,
    committed_at: Option<String>,
    committed_by: Option<String>,
    committed_by_name: Option<String>,
    latest_message: Option<String>,
    version_number: Option<i64>,
    size_bytes: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
struct DocumentLockRow {
    document_id: i64,
    locked_by: String,
    locked_by_name: Option<String>,
    locked_at: Option<String>,
    locked_ip: Option<String>,
    locked_user_agent: Option<String>,
    force_acquired: bool,
}

#[derive(Debug, Clone, FromRow)]
struct VersionHistoryRow {
    id: String,
    committed_at: Option<String>,
    committed_by: String,
    committed_by_name: Option<String>,
    message: Option<String>,
    version_number: i64,
    created_via: Option<String>,
    hash: String,
    hash_algo: String,
    size_bytes: i64,
    mime_type: Option<String>,
    original_filename: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct EventHistoryRow {
    id: i64,
    event_type: String,
    created_at: Option<String>,
    actor: String,
    actor_name: Option<String>,
    message: Option<String>,
    result: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct FolderEventHistoryRow {
    id: i64,
    event_type: String,
    created_at: Option<String>,
    actor: Option<String>,
    actor_name: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct FolderPermissionPayloadRow {
    id: i64,
    group_id: i64,
    group_name: String,
    can_view: bool,
    can_read: bool,
    can_write: bool,
}

#[derive(Debug, Clone, FromRow)]
struct AvailableGroupRow {
    id: i64,
    name: String,
}

struct ContentsContext<'a> {
    pool: &'a SqlitePool,
    user: &'a UserContext,
    path_cache: &'a HashMap<i64, String>,
    folder_by_id: &'a HashMap<i64, &'a FolderRecord>,
    normalized_folder: &'a str,
    search_query: &'a str,
    recursive: bool,
}

pub async fn build_sidebar_payload(
    pool: &SqlitePool,
    user: &UserContext,
) -> Result<SidebarPayload, ViewError> {
    let vault_root = get_root_folder(pool, VAULT_ROOT_KEY).await?;
    let _archive_root = get_root_folder(pool, ARCHIVE_ROOT_KEY).await?;
    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let mut children = HashMap::from([
        (String::new(), Vec::new()),
        (ARCHIVE_ROOT.to_string(), Vec::new()),
    ]);

    let mut visible_children = Vec::new();
    for folder in folders
        .iter()
        .filter(|folder| folder.parent_id == Some(vault_root.id))
    {
        if folder_access_level(pool, folder.id, user).await? >= 1 {
            visible_children.push(folder_path_from_cache(folder, &path_cache)?);
        }
    }
    visible_children.sort();
    children.insert(String::new(), visible_children);

    let by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let mut metadata = HashMap::new();
    for folder in &folders {
        let level = folder_access_level(pool, folder.id, user).await?;
        if level < 1 {
            continue;
        }
        let path = folder_path_from_cache(folder, &path_cache)?;
        metadata.insert(path, folder_metadata_payload(folder, level, &by_id));
    }

    Ok(SidebarPayload {
        folder_children: children,
        folder_metadata: metadata,
    })
}

pub async fn build_bootstrap_payload(
    pool: &SqlitePool,
    user: &UserContext,
    auth: &crate::auth::AuthSettings,
    config: &crate::config::Config,
    folder: &str,
) -> Result<BootstrapPayload, ViewError> {
    ensure_root_folders(pool).await?;
    let normalized_request = normalize_folder(Some(folder))?;
    let Some(current_folder) = get_folder_by_path(pool, Some(&normalized_request)).await? else {
        return Err(ViewError::FolderNotFound);
    };
    if folder_access_level(pool, current_folder.id, user).await? < 1 {
        return Err(ViewError::FolderNotFound);
    }
    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    Ok(BootstrapPayload {
        auth_mode: auth.mode.as_str().to_string(),
        base_domain: auth.base_domain.clone(),
        dev_mode: auth.dev_mode,
        site_name: if config.site_name.trim().is_empty() {
            "Vault".to_string()
        } else {
            config.site_name.trim().to_string()
        },
        user: user.clone(),
        preferences: build_preferences_payload(pool, user).await?,
        settings: site_settings_for_db(pool).await?,
        version: app_version().to_string(),
        current_folder: folder_path_from_cache(&current_folder, &path_cache)?,
    })
}

pub async fn build_initial_state_payload(
    pool: &SqlitePool,
    user: &UserContext,
    auth: &crate::auth::AuthSettings,
    config: &crate::config::Config,
    folder: &str,
    share_code: Option<String>,
) -> Result<InitialStatePayload, ViewError> {
    let bootstrap = build_bootstrap_payload(pool, user, auth, config, folder).await?;
    let current_folder = bootstrap.current_folder.clone();
    Ok(InitialStatePayload {
        contents: build_contents_payload(pool, &current_folder, user, "", false).await?,
        sidebar: build_sidebar_payload(pool, user).await?,
        my_edits: build_my_edits_payload(pool, user).await?,
        bootstrap,
        share_code,
    })
}

pub async fn build_contents_payload(
    pool: &SqlitePool,
    folder: &str,
    user: &UserContext,
    q: &str,
    recursive: bool,
) -> Result<ContentsPayload, ViewError> {
    let normalized_request = normalize_folder(Some(folder))?;
    let Some(current_folder) = get_folder_by_path(pool, Some(&normalized_request)).await? else {
        return Err(ViewError::FolderNotFound);
    };
    if folder_access_level(pool, current_folder.id, user).await? < 1 {
        return Err(ViewError::FolderNotFound);
    }

    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let normalized_folder = folder_path_from_cache(&current_folder, &path_cache)?;
    let search_query = q.trim().to_string();
    let locks = active_locks_by_document(pool).await?;
    let visible_docs = visible_document_rows(pool, user).await?;
    let stats = docs_stats_for_folder_payloads(&visible_docs, &folder_by_id, &path_cache)?;
    let context = ContentsContext {
        pool,
        user,
        path_cache: &path_cache,
        folder_by_id: &folder_by_id,
        normalized_folder: &normalized_folder,
        search_query: &search_query,
        recursive,
    };
    let mut folder_rows = build_folder_rows(&context, &current_folder, &folders, &stats).await?;
    let mut doc_rows =
        build_document_rows(&context, &visible_docs, &locks, &current_folder).await?;

    folder_rows.sort_by_key(|row| row.name.to_lowercase());
    doc_rows.sort_by_key(|row| row.name.to_lowercase());

    Ok(ContentsPayload {
        folder: normalized_folder,
        q: search_query,
        recursive,
        folders: folder_rows,
        documents: doc_rows,
    })
}

pub async fn build_my_edits_payload(
    pool: &SqlitePool,
    user: &UserContext,
) -> Result<MyEditsPayload, ViewError> {
    ensure_root_folders(pool).await?;
    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let locks = active_locks_by_document(pool).await?;
    let docs = document_view_rows(pool).await?;
    let mut rows = Vec::new();

    for doc in docs {
        let Some(lock) = locks.get(&doc.id).filter(|lock| lock.locked_by == user.id) else {
            continue;
        };
        let record = doc.document_record();
        let level = document_access_level(pool, &record, user).await?;
        if level < 3 {
            continue;
        }
        let Some(folder) = folder_by_id.get(&doc.folder_id) else {
            continue;
        };
        let doc_folder_path = folder_path_from_cache(folder, &path_cache)?;
        let row = document_row_payload(&doc, folder, &doc_folder_path, level, Some(lock))?;
        rows.push((row.path.to_lowercase(), row));
    }

    rows.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(MyEditsPayload {
        documents: rows.into_iter().map(|(_, row)| row).collect(),
    })
}

pub async fn build_document_detail_payload(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<DocumentDetailPayload, ViewError> {
    let Some(doc) = document_view_row_by_id(pool, document_id).await? else {
        return Err(ViewError::DocumentNotFound);
    };
    if current_version_metadata_is_inconsistent(&doc) {
        return Err(ViewError::InconsistentDocumentVersion);
    }
    let record = doc.document_record();
    let level = document_access_level(pool, &record, user).await?;
    if level == 0 {
        return Err(ViewError::DocumentNotFound);
    }
    if level < 2 {
        return Err(ViewError::InsufficientDocumentAccess);
    }

    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let Some(folder) = folder_by_id.get(&doc.folder_id) else {
        return Err(ViewError::DocumentNotFound);
    };
    let doc_folder_path = folder_path_from_cache(folder, &path_cache)?;
    let locks = active_locks_by_document(pool).await?;
    let row = document_row_payload(&doc, folder, &doc_folder_path, level, locks.get(&doc.id))?;
    let versions = document_history_items(pool, doc.id).await?;
    Ok(DocumentDetailPayload { row, versions })
}

pub async fn build_share_document_payload(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<Option<(String, DocumentRowPayload)>, ViewError> {
    let Some(doc) = document_view_row_by_id(pool, document_id).await? else {
        return Ok(None);
    };
    if current_version_metadata_is_inconsistent(&doc) {
        return Err(ViewError::InconsistentDocumentVersion);
    }
    let record = doc.document_record();
    let level = document_access_level(pool, &record, user).await?;
    if level < 1 {
        return Ok(None);
    }

    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let Some(folder) = folder_by_id.get(&doc.folder_id) else {
        return Ok(None);
    };
    let doc_folder_path = folder_path_from_cache(folder, &path_cache)?;
    let locks = active_locks_by_document(pool).await?;
    let row = document_row_payload(&doc, folder, &doc_folder_path, level, locks.get(&doc.id))?;
    Ok(Some((doc_folder_path, row)))
}

pub async fn build_share_folder_payload(
    pool: &SqlitePool,
    folder_id: i64,
    user: &UserContext,
) -> Result<Option<(String, Value)>, ViewError> {
    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let Some(folder) = folder_by_id.get(&folder_id) else {
        return Ok(None);
    };
    let level = folder_access_level(pool, folder.id, user).await?;
    if level < 1 {
        return Ok(None);
    }
    let path = folder_path_from_cache(folder, &path_cache)?;
    let visible_docs = visible_document_rows(pool, user).await?;
    let stats = docs_stats_for_folder_payloads(&visible_docs, &folder_by_id, &path_cache)?;
    let summary = folder_summary_payload(folder, &path, &stats, level, &folder_by_id);
    Ok(Some((path, folder_summary_without_access(summary)?)))
}

pub async fn build_folder_properties_payload(
    pool: &SqlitePool,
    path: &str,
    user: &UserContext,
) -> Result<Value, ViewError> {
    ensure_root_folders(pool).await?;
    let normalized_request = normalize_folder(Some(path))?;
    let Some(folder) = get_folder_by_path(pool, Some(&normalized_request)).await? else {
        return Err(ViewError::FolderNotFound);
    };
    let level = folder_access_level(pool, folder.id, user).await?;
    if level < 1 {
        return Err(ViewError::FolderNotFound);
    }

    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let path = folder_path_from_cache(&folder, &path_cache)?;
    let visible_docs = visible_document_rows(pool, user).await?;
    let stats = docs_stats_for_folder_payloads(&visible_docs, &folder_by_id, &path_cache)?;
    let summary = folder_summary_payload(&folder, &path, &stats, level, &folder_by_id);
    let mut payload = folder_summary_without_access(summary)?;
    if let Some(object) = payload.as_object_mut() {
        let can_manage_permissions = level >= 3;
        object.insert("root".to_string(), json!(folder.is_root));
        object.insert("archived".to_string(), json!(folder_is_archive(&folder)));
        object.insert(
            "created_at".to_string(),
            json!(payload_datetime_iso(folder.created_at.as_deref())),
        );
        object.insert("created_by".to_string(), json!(folder.created_by));
        object.insert(
            "created_by_name".to_string(),
            json!(non_empty_or(
                folder.created_by_name.clone(),
                non_empty_or(folder.created_by.clone(), "System".to_string()),
            )),
        );
        object.insert(
            "counts".to_string(),
            folder_counts_payload(pool, user, &folder, &folders, &visible_docs).await?,
        );
        object.insert(
            "history".to_string(),
            Value::Array(folder_history_payload(pool, folder.id).await?),
        );
        object.insert(
            "permissions".to_string(),
            if can_manage_permissions {
                Value::Array(folder_permissions_payload(pool, folder.id).await?)
            } else {
                json!([])
            },
        );
        object.insert(
            "available_groups".to_string(),
            if can_manage_permissions {
                Value::Array(available_groups_payload(pool).await?)
            } else {
                json!([])
            },
        );
    }
    Ok(payload)
}

async fn visible_document_rows(
    pool: &SqlitePool,
    user: &UserContext,
) -> Result<Vec<DocumentViewRow>, ViewError> {
    let mut visible_docs = Vec::new();
    for row in document_view_rows(pool).await? {
        let record = row.document_record();
        if document_access_level(pool, &record, user).await? >= 1 {
            if current_version_metadata_is_inconsistent(&row) {
                return Err(ViewError::InconsistentDocumentVersion);
            }
            visible_docs.push(row);
        }
    }
    Ok(visible_docs)
}

async fn folder_counts_payload(
    pool: &SqlitePool,
    user: &UserContext,
    folder: &FolderRecord,
    folders: &[FolderRecord],
    visible_docs: &[DocumentViewRow],
) -> Result<Value, ViewError> {
    let subtree_ids = subtree_folder_ids(folder.id, folders);
    let mut visible_folder_count = 0_i64;
    for folder_id in &subtree_ids {
        if folder_access_level(pool, *folder_id, user).await? >= 1 {
            visible_folder_count += 1;
        }
    }
    let document_count = visible_docs
        .iter()
        .filter(|doc| subtree_ids.contains(&doc.folder_id))
        .count();
    Ok(json!({
        "folders": (visible_folder_count - 1).max(0),
        "documents": document_count,
    }))
}

fn subtree_folder_ids(root_id: i64, folders: &[FolderRecord]) -> Vec<i64> {
    let mut ids = Vec::new();
    let mut pending = vec![root_id];
    while let Some(folder_id) = pending.pop() {
        if ids.contains(&folder_id) {
            continue;
        }
        ids.push(folder_id);
        pending.extend(
            folders
                .iter()
                .filter(|folder| folder.parent_id == Some(folder_id))
                .map(|folder| folder.id),
        );
    }
    ids
}

async fn folder_history_payload(
    pool: &SqlitePool,
    folder_id: i64,
) -> Result<Vec<Value>, ViewError> {
    let rows = sqlx::query_as::<_, FolderEventHistoryRow>(
        r"
        SELECT id, event_type, created_at, actor, actor_name, message
        FROM folder_events
        WHERE folder_id = ?
        ORDER BY created_at DESC
        ",
    )
    .bind(folder_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|event| {
            let by = non_empty_or(
                event.actor_name,
                non_empty_or(event.actor, "System".to_string()),
            );
            let message = non_empty_or(event.message, event.event_type.clone());
            json!({
                "id": event.id,
                "type": event.event_type,
                "by": by,
                "message": message,
                "timestamp": payload_datetime_iso(event.created_at.as_deref()),
            })
        })
        .collect())
}

async fn folder_permissions_payload(
    pool: &SqlitePool,
    folder_id: i64,
) -> Result<Vec<Value>, ViewError> {
    let rows = sqlx::query_as::<_, FolderPermissionPayloadRow>(
        r"
        SELECT
            fp.id,
            fp.group_id,
            vg.name AS group_name,
            fp.can_view,
            fp.can_read,
            fp.can_write
        FROM folder_permissions fp
        JOIN vault_groups vg ON vg.id = fp.group_id
        WHERE fp.folder_id = ?
        ORDER BY vg.name
        ",
    )
    .bind(folder_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.id,
                "group_id": row.group_id,
                "group_name": row.group_name,
                "can_view": row.can_view,
                "can_read": row.can_read,
                "can_write": row.can_write,
            })
        })
        .collect())
}

async fn available_groups_payload(pool: &SqlitePool) -> Result<Vec<Value>, ViewError> {
    let rows = sqlx::query_as::<_, AvailableGroupRow>(
        r"
        SELECT id, name
        FROM vault_groups
        ORDER BY name
        ",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|group| json!({"id": group.id, "name": group.name}))
        .collect())
}

pub async fn build_preferences_payload(
    pool: &SqlitePool,
    user: &UserContext,
) -> Result<Value, ViewError> {
    let mut preferences = preferences_for_user(pool, user).await?;
    let favorite_items = resolved_favorite_items(pool, user, &preferences).await?;
    if let Some(object) = preferences.as_object_mut() {
        object.insert("favoriteItems".to_string(), Value::Array(favorite_items));
    }
    Ok(preferences)
}

async fn resolved_favorite_items(
    pool: &SqlitePool,
    user: &UserContext,
    preferences: &Value,
) -> Result<Vec<Value>, ViewError> {
    let Some(raw_items) = preferences.get("favoriteItems").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    if raw_items.is_empty() {
        return Ok(Vec::new());
    }

    let folders = all_folders(pool).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let visible_docs = visible_document_rows(pool, user).await?;
    let stats = docs_stats_for_folder_payloads(&visible_docs, &folder_by_id, &path_cache)?;
    let locks = active_locks_by_document(pool).await?;
    let mut resolved = Vec::new();

    for item in raw_items {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(item_id) = item.get("id").and_then(Value::as_i64) else {
            continue;
        };
        match item_type {
            "folder" => {
                if let Some(row) =
                    favorite_folder_payload(pool, user, item_id, &folder_by_id, &path_cache, &stats)
                        .await?
                {
                    resolved.push(row);
                }
            }
            "document" => {
                if let Some(row) = favorite_document_payload(
                    pool,
                    user,
                    item_id,
                    &visible_docs,
                    &folder_by_id,
                    &path_cache,
                    &locks,
                )
                .await?
                {
                    resolved.push(row);
                }
            }
            _ => {}
        }
    }
    Ok(resolved)
}

async fn favorite_folder_payload(
    pool: &SqlitePool,
    user: &UserContext,
    folder_id: i64,
    folder_by_id: &HashMap<i64, &FolderRecord>,
    path_cache: &HashMap<i64, String>,
    stats: &[DocStat],
) -> Result<Option<Value>, ViewError> {
    let Some(folder) = folder_by_id.get(&folder_id) else {
        return Ok(None);
    };
    let level = folder_access_level(pool, folder.id, user).await?;
    if level < 1 {
        return Ok(None);
    }
    let path = folder_path_from_cache(folder, path_cache)?;
    let mut value = serde_json::to_value(folder_summary_payload(
        folder,
        &path,
        stats,
        level,
        folder_by_id,
    ))?;
    if let Some(object) = value.as_object_mut() {
        object.insert("type".to_string(), json!("folder"));
        object.insert("archived".to_string(), json!(folder_is_archive(folder)));
    }
    Ok(Some(value))
}

async fn favorite_document_payload(
    pool: &SqlitePool,
    user: &UserContext,
    document_id: i64,
    visible_docs: &[DocumentViewRow],
    folder_by_id: &HashMap<i64, &FolderRecord>,
    path_cache: &HashMap<i64, String>,
    locks: &HashMap<i64, DocumentLockRow>,
) -> Result<Option<Value>, ViewError> {
    let Some(doc) = visible_docs.iter().find(|doc| doc.id == document_id) else {
        return Ok(None);
    };
    let Some(folder) = folder_by_id.get(&doc.folder_id) else {
        return Ok(None);
    };
    let record = doc.document_record();
    let level = document_access_level(pool, &record, user).await?;
    if level < 1 {
        return Ok(None);
    }
    let doc_folder_path = folder_path_from_cache(folder, path_cache)?;
    let mut value = serde_json::to_value(document_row_payload(
        doc,
        folder,
        &doc_folder_path,
        level,
        locks.get(&doc.id),
    )?)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("type".to_string(), json!("document"));
    }
    Ok(Some(value))
}

async fn build_folder_rows(
    context: &ContentsContext<'_>,
    current_folder: &FolderRecord,
    folders: &[FolderRecord],
    stats: &[DocStat],
) -> Result<Vec<FolderSummaryPayload>, ViewError> {
    let mut rows = Vec::new();
    for folder in folder_candidates(
        current_folder,
        folders,
        context.path_cache,
        context.normalized_folder,
        context.search_query,
        context.recursive,
    ) {
        let level = folder_access_level(context.pool, folder.id, context.user).await?;
        if level < 1 {
            continue;
        }
        let path = folder_path_from_cache(folder, context.path_cache)?;
        if !context.search_query.is_empty()
            && !matches_query(context.search_query, &[Some(&folder.name), Some(&path)])
        {
            continue;
        }
        rows.push(folder_summary_payload(
            folder,
            &path,
            stats,
            level,
            context.folder_by_id,
        ));
    }
    Ok(rows)
}

async fn build_document_rows(
    context: &ContentsContext<'_>,
    visible_docs: &[DocumentViewRow],
    locks: &HashMap<i64, DocumentLockRow>,
    current_folder: &FolderRecord,
) -> Result<Vec<DocumentRowPayload>, ViewError> {
    let current_is_archive = folder_is_archive(current_folder);
    let mut rows = Vec::new();
    for doc in visible_docs {
        let Some(doc_folder) = context.folder_by_id.get(&doc.folder_id) else {
            continue;
        };
        if folder_is_archive(doc_folder) != current_is_archive {
            continue;
        }
        let doc_folder_path = folder_path_from_cache(doc_folder, context.path_cache)?;
        if !folder_is_in_scope(
            context.normalized_folder,
            &doc_folder_path,
            !context.search_query.is_empty() && context.recursive,
        ) {
            continue;
        }
        if !document_matches_query(doc, doc_folder, &doc_folder_path, context.search_query)? {
            continue;
        }
        let record = doc.document_record();
        let level = document_access_level(context.pool, &record, context.user).await?;
        rows.push(document_row_payload(
            doc,
            doc_folder,
            &doc_folder_path,
            level,
            locks.get(&doc.id),
        )?);
    }
    Ok(rows)
}

fn document_matches_query(
    doc: &DocumentViewRow,
    folder: &FolderRecord,
    doc_folder_path: &str,
    search_query: &str,
) -> Result<bool, ViewError> {
    if search_query.is_empty() {
        return Ok(true);
    }
    let doc_path = join_path(&[doc_folder_path, &doc.name]);
    let archived_original_path = if folder_is_archive(folder) {
        let archived_from = normalize_folder(doc.archived_from_folder.as_deref())?;
        // Python builds the search-only original path with
        // `archived_original_name or doc.name`, so legacy archived rows with an
        // empty original-name field are still discoverable by original path.
        let original_name = doc
            .archived_original_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&doc.name);
        join_path(&[&archived_from, original_name])
    } else {
        String::new()
    };
    Ok(matches_query(
        search_query,
        &[
            Some(&doc.name),
            Some(&doc_path),
            Some(doc_folder_path),
            doc.archived_from_folder.as_deref(),
            Some(&archived_original_path),
        ],
    ))
}

impl DocumentViewRow {
    fn document_record(&self) -> DocumentRecord {
        DocumentRecord {
            id: self.id,
            folder_id: self.folder_id,
            name: self.name.clone(),
            archived_from_folder: self.archived_from_folder.clone(),
            archived_original_name: self.archived_original_name.clone(),
            archived_access: self.archived_access.clone(),
        }
    }
}

fn folder_candidates<'a>(
    current_folder: &FolderRecord,
    folders: &'a [FolderRecord],
    path_cache: &HashMap<i64, String>,
    normalized_folder: &str,
    search_query: &str,
    recursive: bool,
) -> Vec<&'a FolderRecord> {
    if folder_is_archive(current_folder) {
        return Vec::new();
    }
    if !search_query.is_empty() && recursive {
        let current_is_archive = folder_is_archive(current_folder);
        return folders
            .iter()
            .filter(|folder| {
                !folder.is_root
                    && folder.id != current_folder.id
                    && folder_is_archive(folder) == current_is_archive
            })
            .filter_map(|folder| {
                let path = folder_path_from_cache(folder, path_cache).ok()?;
                folder_contains_doc_folder(normalized_folder, &path).then_some(folder)
            })
            .collect::<Vec<_>>();
    }
    folders
        .iter()
        .filter(|folder| folder.parent_id == Some(current_folder.id))
        .collect()
}

fn folder_summary_payload(
    folder: &FolderRecord,
    path: &str,
    stats: &[DocStat],
    level: i64,
    folder_by_id: &HashMap<i64, &FolderRecord>,
) -> FolderSummaryPayload {
    let mut latest: Option<String> = None;
    let mut latest_by = None;
    let mut size = 0_i64;
    for stat in stats {
        if !folder_contains_doc_folder(path, &stat.folder) {
            continue;
        }
        size += stat.size_bytes;
        if timestamp_is_later(stat.mtime.as_deref(), latest.as_deref()) {
            latest.clone_from(&stat.mtime);
            latest_by.clone_from(&stat.latest_by);
        }
    }
    let effective = effective_ttl_policy(folder, folder_by_id);
    FolderSummaryPayload {
        id: folder.id,
        path: path.to_string(),
        name: path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("Vault")
            .to_string(),
        color: folder.color.clone().unwrap_or_default(),
        icon: folder.icon.clone().unwrap_or_default(),
        default_ttl_days: folder.default_ttl_days,
        default_ttl_action: default_ttl_action(folder),
        effective_ttl_days: effective.days,
        effective_ttl_action: effective.action,
        effective_ttl_source_id: effective.source_id,
        effective_ttl_inherited: effective.inherited,
        latest_by,
        modified_at: payload_timestamp(latest.as_deref()),
        modified_display: format_mtime(latest.as_deref()),
        size_bytes: size,
        size_display: format_size(Some(size)),
        access: access_payload(level),
    }
}

fn folder_summary_without_access(summary: FolderSummaryPayload) -> Result<Value, ViewError> {
    let mut value = serde_json::to_value(summary)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("access");
    }
    Ok(value)
}

fn folder_metadata_payload(
    folder: &FolderRecord,
    level: i64,
    folder_by_id: &HashMap<i64, &FolderRecord>,
) -> FolderMetadataPayload {
    let effective = effective_ttl_policy(folder, folder_by_id);
    FolderMetadataPayload {
        id: folder.id,
        color: folder.color.clone().unwrap_or_default(),
        icon: folder.icon.clone().unwrap_or_default(),
        access: access_payload(level),
        default_ttl_days: folder.default_ttl_days,
        default_ttl_action: default_ttl_action(folder),
        effective_ttl_days: effective.days,
        effective_ttl_action: effective.action,
        effective_ttl_source_id: effective.source_id,
        effective_ttl_inherited: effective.inherited,
    }
}

fn document_row_payload(
    doc: &DocumentViewRow,
    folder: &FolderRecord,
    doc_folder: &str,
    level: i64,
    lock: Option<&DocumentLockRow>,
) -> Result<DocumentRowPayload, ViewError> {
    if current_version_metadata_is_inconsistent(doc) {
        return Err(ViewError::InconsistentDocumentVersion);
    }
    let archived = folder_is_archive(folder);
    let doc_path = join_path(&[doc_folder, &doc.name]);
    let archived_from_folder = if archived {
        normalize_folder(doc.archived_from_folder.as_deref())?
    } else {
        String::new()
    };
    let archived_original_name = if archived {
        doc.archived_original_name.clone().unwrap_or_default()
    } else {
        String::new()
    };
    let archived_original_path = if archived_original_name.is_empty() {
        archived_from_folder.clone()
    } else {
        join_path(&[&archived_from_folder, &archived_original_name])
    };
    let modified_at = doc.committed_at.clone();
    Ok(DocumentRowPayload {
        id: doc.id,
        name: doc.name.clone(),
        path: doc_path,
        folder: doc_folder.to_string(),
        archived_from_folder,
        archived_original_name,
        archived_original_path,
        modified_at: payload_timestamp(modified_at.as_deref()),
        modified_display: format_mtime(modified_at.as_deref()),
        latest_by: first_non_empty(doc.committed_by_name.clone(), doc.committed_by.clone()),
        latest_message: doc.latest_message.clone(),
        latest_version_number: doc.version_number.or(doc.latest_version_number),
        version_count: doc.version_count,
        created_by: doc.created_by.clone(),
        created_by_name: doc.created_by_name.clone(),
        created_at: payload_datetime_iso(doc.created_at.as_deref()),
        size_bytes: doc.size_bytes,
        size_display: format_size(doc.size_bytes),
        download_url: doc
            .latest_version_id
            .as_ref()
            .map(|version_id| format!("/documents/{}/versions/{version_id}/download", doc.id)),
        lock: lock_payload(lock),
        archived,
        expires_at: payload_timestamp(doc.expires_at.as_deref()),
        expiry_action: doc.expiry_action.clone(),
        access: access_payload(level),
    })
}

fn current_version_metadata_is_inconsistent(doc: &DocumentViewRow) -> bool {
    let has_current_pointer = doc
        .current_version_id
        .as_deref()
        .is_some_and(|version_id| !version_id.is_empty());
    (has_current_pointer || doc.has_versions) && doc.latest_version_id.is_none()
}

fn docs_stats_for_folder_payloads(
    docs: &[DocumentViewRow],
    folder_by_id: &HashMap<i64, &FolderRecord>,
    path_cache: &HashMap<i64, String>,
) -> Result<Vec<DocStat>, ViewError> {
    let mut stats = Vec::with_capacity(docs.len());
    for doc in docs {
        let Some(folder) = folder_by_id.get(&doc.folder_id) else {
            continue;
        };
        stats.push(DocStat {
            folder: folder_path_from_cache(folder, path_cache)?,
            size_bytes: doc.size_bytes.unwrap_or(0),
            mtime: doc.committed_at.clone(),
            latest_by: first_non_empty(doc.committed_by_name.clone(), doc.committed_by.clone()),
        });
    }
    Ok(stats)
}

async fn document_view_rows(pool: &SqlitePool) -> Result<Vec<DocumentViewRow>, ViewError> {
    Ok(sqlx::query_as::<_, DocumentViewRow>(
        r"
        SELECT
            d.id,
            d.folder_id,
            d.name,
            d.created_at,
            d.created_by,
            d.created_by_name,
            d.latest_version_number,
            d.version_count,
            d.expires_at,
            d.expiry_action,
            d.archived_from_folder,
            d.archived_original_name,
            d.archived_access,
            d.current_version_id,
            EXISTS (
                SELECT 1
                FROM document_versions existing_version
                WHERE existing_version.document_id = d.id
            ) AS has_versions,
            v.id AS latest_version_id,
            v.committed_at,
            v.committed_by,
            v.committed_by_name,
            v.message AS latest_message,
            v.version_number,
            b.size_bytes
        FROM documents d
        LEFT JOIN document_versions v
            ON v.document_id = d.id AND v.id = d.current_version_id
        LEFT JOIN blobs b
            ON b.id = v.blob_id
        ",
    )
    .fetch_all(pool)
    .await?)
}

async fn document_view_row_by_id(
    pool: &SqlitePool,
    document_id: i64,
) -> Result<Option<DocumentViewRow>, ViewError> {
    Ok(sqlx::query_as::<_, DocumentViewRow>(
        r"
        SELECT
            d.id,
            d.folder_id,
            d.name,
            d.created_at,
            d.created_by,
            d.created_by_name,
            d.latest_version_number,
            d.version_count,
            d.expires_at,
            d.expiry_action,
            d.archived_from_folder,
            d.archived_original_name,
            d.archived_access,
            d.current_version_id,
            EXISTS (
                SELECT 1
                FROM document_versions existing_version
                WHERE existing_version.document_id = d.id
            ) AS has_versions,
            v.id AS latest_version_id,
            v.committed_at,
            v.committed_by,
            v.committed_by_name,
            v.message AS latest_message,
            v.version_number,
            b.size_bytes
        FROM documents d
        LEFT JOIN document_versions v
            ON v.document_id = d.id AND v.id = d.current_version_id
        LEFT JOIN blobs b
            ON b.id = v.blob_id
        WHERE d.id = ?
        ",
    )
    .bind(document_id)
    .fetch_optional(pool)
    .await?)
}

async fn document_history_items(
    pool: &SqlitePool,
    document_id: i64,
) -> Result<Vec<Value>, ViewError> {
    let versions = deduped_version_history(pool, document_id).await?;
    let version_signatures = versions.iter().map(version_signature).collect::<Vec<_>>();
    let mut items = Vec::new();
    for version in &versions {
        items.push(json!({
            "id": version.id,
            "type": "version",
            "timestamp": payload_datetime_iso(version.committed_at.as_deref()),
            "display": format_history_timestamp(version.committed_at.as_deref(), "Version"),
            "by": non_empty_or(version.committed_by_name.clone(), version.committed_by.clone()),
            "note": version.message,
            "version_number": version.version_number,
            "created_via": version.created_via,
            "checksum": version.hash,
            "hash_algo": version.hash_algo,
            "size_bytes": version.size_bytes,
            "mime_type": version.mime_type,
            "original_filename": version.original_filename,
            "download_url": format!("/documents/{document_id}/versions/{}/download", version.id),
        }));
    }
    for event in event_history(pool, document_id).await? {
        if version_signatures.contains(&event_signature(&event)) {
            continue;
        }
        let display_fallback = python_title(&event.event_type);
        items.push(json!({
            "id": format!("event-{}", event.id),
            "type": event.event_type,
            "timestamp": payload_datetime_iso(event.created_at.as_deref()),
            "display": format_history_timestamp(event.created_at.as_deref(), &display_fallback),
            "by": non_empty_or(event.actor_name, event.actor),
            "note": event.message,
            "result": event.result,
            "download_url": Value::Null,
        }));
    }
    items.sort_by(|left, right| {
        right
            .get("timestamp")
            .and_then(Value::as_str)
            .cmp(&left.get("timestamp").and_then(Value::as_str))
    });
    Ok(items)
}

async fn deduped_version_history(
    pool: &SqlitePool,
    document_id: i64,
) -> Result<Vec<VersionHistoryRow>, ViewError> {
    let versions = sqlx::query_as::<_, VersionHistoryRow>(
        r"
        SELECT
            v.id,
            v.committed_at,
            v.committed_by,
            v.committed_by_name,
            v.message,
            v.version_number,
            v.created_via,
            b.hash,
            b.hash_algo,
            b.size_bytes,
            v.mime_type,
            v.original_filename
        FROM document_versions v
        JOIN blobs b ON b.id = v.blob_id
        WHERE v.document_id = ?
        ORDER BY v.version_number DESC
        ",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await?;
    let mut filtered = Vec::new();
    let mut last_checksum = None;
    // Match Python's history normalization: only adjacent duplicate checksums in
    // version-number order are collapsed. A repeated checksum after an
    // intervening different version is a real rollback and must remain visible.
    for version in versions {
        if last_checksum.as_deref() == Some(version.hash.as_str()) {
            continue;
        }
        last_checksum = Some(version.hash.clone());
        filtered.push(version);
    }
    Ok(filtered)
}

async fn event_history(
    pool: &SqlitePool,
    document_id: i64,
) -> Result<Vec<EventHistoryRow>, ViewError> {
    Ok(sqlx::query_as::<_, EventHistoryRow>(
        r"
        SELECT
            id,
            event_type,
            created_at,
            actor,
            actor_name,
            message,
            result
        FROM document_events
        WHERE document_id = ?
        ORDER BY created_at DESC
        ",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await?)
}

fn version_signature(version: &VersionHistoryRow) -> (Option<&str>, &str, String, Option<i64>) {
    (
        version.created_via.as_deref(),
        version.message.as_deref().unwrap_or("").trim(),
        version
            .committed_by_name
            .clone()
            .filter(|committed_by_name| !committed_by_name.is_empty())
            .unwrap_or_else(|| version.committed_by.clone()),
        history_signature_timestamp(version.committed_at.as_deref()),
    )
}

fn event_signature(event: &EventHistoryRow) -> (Option<&str>, &str, String, Option<i64>) {
    (
        Some(event.event_type.as_str()),
        event.message.as_deref().unwrap_or("").trim(),
        event
            .actor_name
            .clone()
            .filter(|actor_name| !actor_name.is_empty())
            .unwrap_or_else(|| event.actor.clone()),
        history_signature_timestamp(event.created_at.as_deref()),
    )
}

fn non_empty_or(value: Option<String>, fallback: String) -> String {
    value.filter(|value| !value.is_empty()).unwrap_or(fallback)
}

fn first_non_empty(left: Option<String>, right: Option<String>) -> Option<String> {
    left.filter(|value| !value.is_empty())
        .or_else(|| right.filter(|value| !value.is_empty()))
}

fn history_signature_timestamp(timestamp: Option<&str>) -> Option<i64> {
    timestamp
        .filter(|value| !value.trim().is_empty())
        .and_then(parse_utc_timestamp)
        .map(OffsetDateTime::unix_timestamp)
}

async fn active_locks_by_document(
    pool: &SqlitePool,
) -> Result<HashMap<i64, DocumentLockRow>, ViewError> {
    let rows = sqlx::query_as::<_, DocumentLockRow>(
        r"
        SELECT
            document_id,
            locked_by,
            locked_by_name,
            locked_at,
            locked_ip,
            locked_user_agent,
            force_acquired
        FROM document_locks
        WHERE is_active = 1
        ",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| (row.document_id, row)).collect())
}

fn lock_payload(lock: Option<&DocumentLockRow>) -> LockPayload {
    LockPayload {
        by: lock.map(|lock| lock.locked_by.clone()),
        name: lock.and_then(|lock| lock.locked_by_name.clone()),
        at: lock.and_then(|lock| payload_datetime_iso(lock.locked_at.as_deref())),
        ip: lock.and_then(|lock| lock.locked_ip.clone()),
        user_agent: lock.and_then(|lock| lock.locked_user_agent.clone()),
        force_acquired: lock.map(|lock| lock.force_acquired),
    }
}

fn folder_is_archive(folder: &FolderRecord) -> bool {
    folder.root_key == ARCHIVE_ROOT_KEY
}

fn folder_contains_doc_folder(folder: &str, doc_folder: &str) -> bool {
    if folder.is_empty() {
        return !(doc_folder == ARCHIVE_ROOT
            || doc_folder.starts_with(&format!("{ARCHIVE_ROOT}/")));
    }
    if folder == ARCHIVE_ROOT {
        return doc_folder == ARCHIVE_ROOT || doc_folder.starts_with(&format!("{ARCHIVE_ROOT}/"));
    }
    doc_folder == folder || doc_folder.starts_with(&format!("{folder}/"))
}

fn folder_is_in_scope(target: &str, candidate: &str, recursive: bool) -> bool {
    if recursive {
        folder_contains_doc_folder(target, candidate)
    } else {
        candidate == target
    }
}

fn matches_query(query: &str, values: &[Option<&str>]) -> bool {
    let needle = query.trim().to_lowercase();
    needle.is_empty()
        || values
            .iter()
            .flatten()
            .any(|value| value.to_lowercase().contains(&needle))
}

#[derive(Debug, Clone)]
struct EffectiveTtlPayload {
    days: Option<i64>,
    action: String,
    source_id: Option<i64>,
    inherited: bool,
}

fn effective_ttl_policy(
    folder: &FolderRecord,
    folder_by_id: &HashMap<i64, &FolderRecord>,
) -> EffectiveTtlPayload {
    let mut current = Some(folder);
    let mut seen = Vec::new();
    while let Some(candidate) = current {
        if seen.contains(&candidate.id) {
            break;
        }
        seen.push(candidate.id);
        if let Some((days, action)) = direct_ttl_policy(candidate) {
            if action == "archive" && folder_is_archive(folder) {
                break;
            }
            return EffectiveTtlPayload {
                days: Some(days),
                action,
                source_id: Some(candidate.id),
                inherited: candidate.id != folder.id,
            };
        }
        current = candidate
            .parent_id
            .and_then(|parent_id| folder_by_id.get(&parent_id).copied());
    }
    EffectiveTtlPayload {
        days: None,
        action: "none".to_string(),
        source_id: None,
        inherited: false,
    }
}

fn direct_ttl_policy(folder: &FolderRecord) -> Option<(i64, String)> {
    let days = folder.default_ttl_days?;
    if days < 1 {
        return None;
    }
    let action = folder
        .default_ttl_action
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(action.as_str(), "archive" | "delete").then_some((days, action))
}

fn default_ttl_action(folder: &FolderRecord) -> String {
    folder
        .default_ttl_action
        .clone()
        .filter(|action| !action.trim().is_empty())
        .unwrap_or_else(|| "none".to_string())
}

fn format_size(size_bytes: Option<i64>) -> String {
    let Some(size_bytes) = size_bytes else {
        return "-".to_string();
    };
    if size_bytes < 1024 {
        return format!("{size_bytes} B");
    }
    let bytes = i128::from(size_bytes);
    let (unit, divisor) = SIZE_UNITS
        .iter()
        .copied()
        .find(|(_, divisor)| bytes < *divisor * 1024)
        .unwrap_or(SIZE_UNITS[SIZE_UNITS.len() - 1]);
    let tenths = (bytes * 10 + divisor / 2) / divisor;
    format!("{}.{} {unit}", tenths / 10, tenths % 10)
}

fn format_mtime(timestamp: Option<&str>) -> String {
    let Some(timestamp) = timestamp.filter(|value| !value.trim().is_empty()) else {
        return "Not updated yet".to_string();
    };
    let Some(parsed) = parse_utc_timestamp(timestamp) else {
        return timestamp.to_string();
    };
    let hour_24 = parsed.hour();
    let hour_12 = hour_24 % 12;
    let hour_12 = if hour_12 == 0 { 12 } else { hour_12 };
    let meridiem = if hour_24 < 12 { "am" } else { "pm" };
    format!(
        "{} {}, {} at {}:{:02} {meridiem}",
        month_name(parsed),
        parsed.day(),
        parsed.year(),
        hour_12,
        parsed.minute(),
    )
}

fn format_history_timestamp(timestamp: Option<&str>, fallback: &str) -> String {
    let Some(timestamp) = timestamp.filter(|value| !value.trim().is_empty()) else {
        return fallback.to_string();
    };
    let Some(parsed) = parse_utc_timestamp(timestamp) else {
        return timestamp.to_string();
    };
    format!(
        "{} {:02}, {} {:02}:{:02}",
        month_name(parsed),
        parsed.day(),
        parsed.year(),
        parsed.hour(),
        parsed.minute(),
    )
}

fn python_title(value: &str) -> String {
    let mut titled = String::with_capacity(value.len());
    let mut at_word_start = true;
    for character in value.chars() {
        if character.is_alphanumeric() {
            if at_word_start {
                titled.extend(character.to_uppercase());
            } else {
                titled.extend(character.to_lowercase());
            }
            at_word_start = false;
        } else {
            titled.push(character);
            at_word_start = true;
        }
    }
    titled
}

fn payload_timestamp(timestamp: Option<&str>) -> Option<String> {
    let timestamp = timestamp.filter(|value| !value.trim().is_empty())?;
    Some(
        parse_utc_timestamp(timestamp).map_or_else(|| timestamp.to_string(), format_python_iso_utc),
    )
}

fn payload_datetime_iso(timestamp: Option<&str>) -> Option<String> {
    let timestamp = timestamp.filter(|value| !value.trim().is_empty())?;
    if let Ok(parsed) = OffsetDateTime::parse(timestamp.trim(), &Rfc3339) {
        return Some(format_python_iso_utc(parsed));
    }
    Some(
        parse_utc_timestamp(timestamp)
            .map_or_else(|| timestamp.to_string(), format_python_naive_iso_utc),
    )
}

fn format_python_iso_utc(timestamp: OffsetDateTime) -> String {
    let timestamp = timestamp.to_offset(UtcOffset::UTC);
    let base = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        timestamp.year(),
        timestamp.month() as u8,
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second(),
    );
    let nanosecond = timestamp.nanosecond();
    if nanosecond == 0 {
        return format!("{base}+00:00");
    }
    let microsecond = nanosecond / 1_000;
    format!("{base}.{microsecond:06}+00:00")
}

fn format_python_naive_iso_utc(timestamp: OffsetDateTime) -> String {
    let timestamp = timestamp.to_offset(UtcOffset::UTC);
    let base = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        timestamp.year(),
        timestamp.month() as u8,
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second(),
    );
    let nanosecond = timestamp.nanosecond();
    if nanosecond == 0 {
        return base;
    }
    let microsecond = nanosecond / 1_000;
    format!("{base}.{microsecond:06}")
}

fn timestamp_is_later(candidate: Option<&str>, latest: Option<&str>) -> bool {
    let Some(candidate) = candidate.filter(|value| !value.trim().is_empty()) else {
        return false;
    };
    match (
        parse_utc_timestamp(candidate),
        latest.and_then(parse_utc_timestamp),
    ) {
        (Some(candidate), Some(latest)) => candidate > latest,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => latest.is_none_or(|latest| candidate > latest),
    }
}

fn parse_utc_timestamp(timestamp: &str) -> Option<OffsetDateTime> {
    let trimmed = timestamp.trim();
    if let Ok(parsed) = OffsetDateTime::parse(trimmed, &Rfc3339) {
        return Some(parsed.to_offset(UtcOffset::UTC));
    }
    parse_sqlite_timestamp(trimmed, "[year]-[month]-[day] [hour]:[minute]:[second]")
        .or_else(|| {
            parse_sqlite_timestamp(
                trimmed,
                "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond]",
            )
        })
        .or_else(|| {
            parse_sqlite_timestamp(trimmed, "[year]-[month]-[day]T[hour]:[minute]:[second]")
        })
}

fn parse_sqlite_timestamp(timestamp: &str, format: &str) -> Option<OffsetDateTime> {
    let description = time::format_description::parse_borrowed::<1>(format).ok()?;
    PrimitiveDateTime::parse(timestamp, &description)
        .ok()
        .map(PrimitiveDateTime::assume_utc)
}

fn month_name(timestamp: OffsetDateTime) -> &'static str {
    MONTH_NAMES[timestamp.month() as usize - 1]
}
