use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool, Transaction};
use thiserror::Error;

use crate::auth::UserContext;
use crate::folders::{
    ARCHIVE_ROOT, ARCHIVE_ROOT_KEY, FolderError, FolderRecord, access_level, all_folders,
    apply_effective_ttl_to_document_in_tx, build_folder_path_cache, folder_access_level,
    folder_path_by_id, folder_path_from_cache, get_or_create_folder_path_in_tx, get_root_folder,
    join_path, normalize_folder, parse_public_folder_path, require_write_for_folder_path,
    subtree_folder_ids_from_records,
};
use crate::state_events::state_event_resources_json;

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct DocumentRecord {
    pub id: i64,
    pub folder_id: i64,
    pub name: String,
    pub archived_from_folder: Option<String>,
    pub archived_original_name: Option<String>,
    pub archived_access: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessPayload {
    pub visible: bool,
    pub read: bool,
    pub write: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientMeta {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentLockResult {
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RetentionSweepResult {
    pub archived: Vec<String>,
    pub deleted: Vec<String>,
    pub skipped: Vec<String>,
}

impl RetentionSweepResult {
    #[must_use]
    pub fn has_state_changes(&self) -> bool {
        !self.archived.is_empty() || !self.deleted.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDownload {
    pub document_id: i64,
    pub document_path: String,
    pub version_id: String,
    pub version_number: i64,
    pub filename: String,
    pub mime_type: Option<String>,
    pub hash_algo: String,
    pub hash: String,
    pub size_bytes: i64,
    pub backend: String,
    pub bucket: String,
    pub object_key: String,
}

#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("document not found")]
    DocumentNotFound,
    #[error("insufficient document access")]
    InsufficientDocumentAccess,
    #[error("restore this file before editing")]
    RestoreBeforeEditing,
    #[error("document is locked by another user")]
    DocumentLockedByOtherUser,
    #[error("document is not locked")]
    DocumentNotLocked,
    #[error("move the document to Archive before deleting")]
    MoveDocumentToArchiveBeforeDeleting,
    #[error("file name is required")]
    FileNameRequired,
    #[error("invalid file name")]
    InvalidFileName,
    #[error("a document already exists at that path")]
    DocumentPathAlreadyExists,
    #[error("restore archived files before renaming")]
    RestoreArchivedBeforeRenaming,
    #[error("use archive or restore for Archive moves")]
    UseArchiveOrRestoreForArchiveMoves,
    #[error("document is already archived")]
    DocumentAlreadyArchived,
    #[error("document is not archived")]
    DocumentNotArchived,
    #[error("archived document is missing restore metadata")]
    ArchivedDocumentMissingRestoreMetadata,
    #[error("cannot archive a root folder")]
    CannotArchiveRootFolder,
    #[error("folder is already archived")]
    FolderAlreadyArchived,
    #[error("folder has no files to archive")]
    FolderHasNoFilesToArchive,
    #[error("document has no versions")]
    DocumentHasNoVersions,
    #[error("current document version metadata is inconsistent")]
    InconsistentCurrentVersion,
    #[error("version not found")]
    VersionNotFound,
    #[error("blob has no storage location")]
    BlobHasNoStorageLocation,
    #[error(transparent)]
    Folder(#[from] FolderError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, FromRow)]
struct GroupRecord {
    id: i64,
    name: String,
}

#[derive(Debug, Clone, FromRow)]
struct ActiveLockRecord {
    id: i64,
    locked_by: String,
    locked_by_name: Option<String>,
}

#[derive(Debug)]
struct ArchiveDocumentItem {
    document: DocumentRecord,
    source_path: String,
    source_folder_path: String,
    archived_access: String,
}

#[derive(Debug, FromRow)]
struct VersionDownloadRow {
    document_id: i64,
    folder_id: i64,
    document_name: String,
    archived_from_folder: Option<String>,
    archived_original_name: Option<String>,
    archived_access: Option<String>,
    version_id: String,
    version_number: i64,
    mime_type: Option<String>,
    original_filename: Option<String>,
    hash_algo: String,
    hash: String,
    size_bytes: i64,
    backend: Option<String>,
    bucket: Option<String>,
    object_key: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct ExpiredDocumentRow {
    id: i64,
    folder_id: i64,
    name: String,
    expiry_action: Option<String>,
}

struct ArchiveDocumentMutation<'a> {
    document: &'a DocumentRecord,
    archive_folder_id: i64,
    source_path: &'a str,
    source_folder_path: &'a str,
    source_name: &'a str,
    archived_access: &'a str,
    user: &'a UserContext,
    meta: &'a ClientMeta,
}

struct ExpiredArchivePlan {
    document: DocumentRecord,
    source_path: String,
    source_folder_path: String,
    archived_access: String,
}

pub async fn fetch_document_by_id(
    pool: &SqlitePool,
    document_id: i64,
) -> Result<DocumentRecord, DocumentError> {
    Ok(sqlx::query_as::<_, DocumentRecord>(
        r"
        SELECT
            id,
            folder_id,
            name,
            archived_from_folder,
            archived_original_name,
            archived_access
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await?)
}

pub async fn try_fetch_document_by_id(
    pool: &SqlitePool,
    document_id: i64,
) -> Result<Option<DocumentRecord>, DocumentError> {
    Ok(sqlx::query_as::<_, DocumentRecord>(
        r"
        SELECT
            id,
            folder_id,
            name,
            archived_from_folder,
            archived_original_name,
            archived_access
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn lock_document(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<DocumentLockResult, DocumentError> {
    let document = editable_document_for_write(pool, document_id, user).await?;
    let path = document_path(pool, &document).await?;
    let mut transaction = pool.begin().await?;
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
        transaction.commit().await?;
        return Ok(DocumentLockResult {
            detail: lock.locked_by_name.unwrap_or(lock.locked_by),
        });
    }
    sqlx::query(
        r"
        INSERT INTO document_locks
            (document_id, locked_by, locked_by_name, locked_ip, locked_user_agent, force_acquired)
        VALUES
            (?, ?, ?, ?, ?, 0)
        ",
    )
    .bind(document.id)
    .bind(&user.id)
    .bind(&user.name)
    .bind(&meta.ip)
    .bind(&meta.user_agent)
    .execute(&mut *transaction)
    .await?;
    record_document_event_in_tx(
        &mut transaction,
        document.id,
        user,
        "lock",
        &format!("Locked {path}"),
        meta,
    )
    .await?;
    transaction.commit().await?;
    Ok(DocumentLockResult {
        detail: user.name.clone(),
    })
}

pub async fn unlock_document(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<DocumentLockResult, DocumentError> {
    let document = document_for_write(pool, document_id, user).await?;
    let path = document_path(pool, &document).await?;
    let mut transaction = pool.begin().await?;
    let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? else {
        return Err(DocumentError::DocumentNotLocked);
    };
    ensure_lock_owner_or_admin(&lock, user)?;
    sqlx::query(
        r"
        UPDATE document_locks
        SET is_active = 0, released_at = CURRENT_TIMESTAMP, released_by = ?
        WHERE id = ?
        ",
    )
    .bind(&user.id)
    .bind(lock.id)
    .execute(&mut *transaction)
    .await?;
    record_document_event_in_tx(
        &mut transaction,
        document.id,
        user,
        "release",
        &format!("Released lock for {path}"),
        meta,
    )
    .await?;
    transaction.commit().await?;
    Ok(DocumentLockResult {
        detail: "Unlocked".to_string(),
    })
}

pub async fn delete_document_forever(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<String, DocumentError> {
    let document = if user.is_admin {
        try_fetch_document_by_id(pool, document_id)
            .await?
            .ok_or(DocumentError::DocumentNotFound)?
    } else {
        document_for_write(pool, document_id, user).await?
    };
    if !document_is_archive(pool, &document).await? {
        return Err(DocumentError::MoveDocumentToArchiveBeforeDeleting);
    }
    let path = document_path(pool, &document).await?;
    let mut transaction = pool.begin().await?;
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
    }
    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(document.id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(path)
}

pub async fn sweep_expired_documents(
    pool: &SqlitePool,
    limit: i64,
) -> Result<RetentionSweepResult, DocumentError> {
    let archive_root = get_root_folder(pool, ARCHIVE_ROOT_KEY).await?;
    let folders = all_folders(pool).await?;
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder.clone()))
        .collect::<HashMap<_, _>>();
    let path_cache = build_folder_path_cache(&folders)?;
    let docs = expired_documents(pool, limit).await?;
    if docs.is_empty() {
        return Ok(RetentionSweepResult::default());
    }
    let locked_ids = active_locked_document_ids(pool, docs.iter().map(|doc| doc.id)).await?;
    let system = system_user();
    let meta = system_meta();
    let timestamp = current_utc_minute_label(pool).await?;
    let mut result = RetentionSweepResult::default();
    let mut archives = Vec::new();
    let mut deletes = Vec::new();
    let mut clears = Vec::new();

    for doc in docs {
        let path = expired_document_path(&doc, &folder_by_id, &path_cache)?;
        if locked_ids.contains(&doc.id) {
            result.skipped.push(path);
            continue;
        }
        match normalized_expiry_action(doc.expiry_action.as_deref()).as_deref() {
            Some("archive") => {
                if expired_document_is_archived(&doc, &folder_by_id)? {
                    clears.push(doc.id);
                } else {
                    let source_folder_path =
                        expired_document_folder_path(&doc, &folder_by_id, &path_cache)?;
                    let archived_access = serde_json::to_string(
                        &archive_access_snapshot(pool, doc.folder_id).await?,
                    )?;
                    result.archived.push(join_path(&[ARCHIVE_ROOT, &doc.name]));
                    archives.push(ExpiredArchivePlan {
                        document: expired_row_document_record(doc),
                        source_path: path,
                        source_folder_path,
                        archived_access,
                    });
                }
            }
            Some("delete") => {
                result.deleted.push(path);
                deletes.push(doc.id);
            }
            _ => clears.push(doc.id),
        }
    }

    let mut transaction = pool.begin().await?;
    for document_id in clears {
        clear_document_expiry_in_tx(&mut transaction, document_id).await?;
    }
    for plan in archives {
        archive_expired_document_in_tx(
            &mut transaction,
            &plan,
            archive_root.id,
            &timestamp,
            &system,
            &meta,
        )
        .await?;
    }
    for document_id in deletes {
        sqlx::query("DELETE FROM documents WHERE id = ?")
            .bind(document_id)
            .execute(&mut *transaction)
            .await?;
    }
    if result.has_state_changes() {
        record_retention_expired_state_in_tx(&mut transaction).await?;
    }
    transaction.commit().await?;
    Ok(result)
}

pub async fn current_version_download(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<VersionDownload, DocumentError> {
    let document = document_for_read(pool, document_id, user).await?;
    let current_version_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT current_version_id FROM documents WHERE id = ?",
    )
    .bind(document.id)
    .fetch_one(pool)
    .await?;
    let Some(version_id) = current_version_id.filter(|version_id| !version_id.is_empty()) else {
        let has_versions = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM document_versions WHERE document_id = ? LIMIT 1",
        )
        .bind(document.id)
        .fetch_optional(pool)
        .await?
        .is_some();
        return if has_versions {
            Err(DocumentError::InconsistentCurrentVersion)
        } else {
            Err(DocumentError::DocumentHasNoVersions)
        };
    };
    match version_download_by_id(pool, document_id, &version_id, user).await {
        Err(DocumentError::VersionNotFound) => Err(DocumentError::InconsistentCurrentVersion),
        result => result,
    }
}

pub async fn checkout_version_download(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<VersionDownload, DocumentError> {
    editable_document_for_write(pool, document_id, user).await?;
    current_version_download(pool, document_id, user).await
}

pub async fn version_download_by_id(
    pool: &SqlitePool,
    document_id: i64,
    version_id: &str,
    user: &UserContext,
) -> Result<VersionDownload, DocumentError> {
    let document = document_for_read(pool, document_id, user).await?;
    let Some(row) = sqlx::query_as::<_, VersionDownloadRow>(
        r"
        SELECT
            d.id AS document_id,
            d.folder_id,
            d.name AS document_name,
            d.archived_from_folder,
            d.archived_original_name,
            d.archived_access,
            v.id AS version_id,
            v.version_number,
            v.mime_type,
            v.original_filename,
            b.hash_algo,
            b.hash,
            b.size_bytes,
            bl.backend,
            bl.bucket,
            bl.object_key
        FROM document_versions v
        JOIN documents d ON d.id = v.document_id
        JOIN blobs b ON b.id = v.blob_id
        LEFT JOIN blob_locations bl ON bl.blob_id = b.id
        WHERE d.id = ? AND v.id = ?
        ORDER BY CASE WHEN bl.backend = 'local' THEN 0 ELSE 1 END, bl.id
        LIMIT 1
        ",
    )
    .bind(document.id)
    .bind(version_id)
    .fetch_optional(pool)
    .await?
    else {
        return Err(DocumentError::VersionNotFound);
    };
    let object_key = row
        .object_key
        .filter(|object_key| !object_key.trim().is_empty())
        .ok_or(DocumentError::BlobHasNoStorageLocation)?;
    let backend = row
        .backend
        .filter(|backend| !backend.trim().is_empty())
        .ok_or(DocumentError::BlobHasNoStorageLocation)?;
    let row_document = DocumentRecord {
        id: row.document_id,
        folder_id: row.folder_id,
        name: row.document_name.clone(),
        archived_from_folder: row.archived_from_folder.clone(),
        archived_original_name: row.archived_original_name.clone(),
        archived_access: row.archived_access.clone(),
    };
    let level = document_access_level(pool, &row_document, user).await?;
    if level < 2 {
        return if level > 0 {
            Err(DocumentError::InsufficientDocumentAccess)
        } else {
            Err(DocumentError::DocumentNotFound)
        };
    }
    Ok(VersionDownload {
        document_id: row.document_id,
        document_path: document_path(pool, &row_document).await?,
        version_id: row.version_id,
        version_number: row.version_number,
        filename: row.original_filename.unwrap_or(row.document_name),
        mime_type: row.mime_type,
        hash_algo: row.hash_algo,
        hash: row.hash,
        size_bytes: row.size_bytes,
        backend,
        bucket: row.bucket.unwrap_or_default(),
        object_key,
    })
}

pub async fn record_download_event(
    pool: &SqlitePool,
    download: &VersionDownload,
    user: &UserContext,
    meta: &ClientMeta,
    current_version: bool,
) -> Result<(), DocumentError> {
    let message = if current_version {
        format!("Downloaded {}", download.document_path)
    } else {
        format!(
            "Downloaded version v{} of {}",
            download.version_number, download.document_path
        )
    };
    let mut transaction = pool.begin().await?;
    record_document_event_in_tx(
        &mut transaction,
        download.document_id,
        user,
        "download",
        &message,
        meta,
    )
    .await?;
    record_document_state_in_tx(&mut transaction, "download", &["document_detail"]).await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn record_checkout_event_and_lock(
    pool: &SqlitePool,
    download: &VersionDownload,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<(), DocumentError> {
    let document = editable_document_for_write(pool, download.document_id, user).await?;
    let path = document_path(pool, &document).await?;
    let mut transaction = pool.begin().await?;
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
    } else {
        sqlx::query(
            r"
            INSERT INTO document_locks
                (document_id, locked_by, locked_by_name, locked_ip, locked_user_agent, force_acquired)
            VALUES
                (?, ?, ?, ?, ?, 0)
            ",
        )
        .bind(document.id)
        .bind(&user.id)
        .bind(&user.name)
        .bind(&meta.ip)
        .bind(&meta.user_agent)
        .execute(&mut *transaction)
        .await?;
    }
    record_document_event_in_tx(
        &mut transaction,
        document.id,
        user,
        "checkout",
        &format!("Checked out {path}"),
        meta,
    )
    .await?;
    record_document_state_in_tx(
        &mut transaction,
        "checkout",
        &["contents", "document_detail", "my_edits"],
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn archive_document(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<String, DocumentError> {
    let document = document_for_write(pool, document_id, user).await?;
    if document_is_archive(pool, &document).await? {
        return Err(DocumentError::DocumentAlreadyArchived);
    }
    let archive_root = get_root_folder(pool, ARCHIVE_ROOT_KEY).await?;
    require_folder_write(pool, archive_root.id, user).await?;
    let source_path = document_path(pool, &document).await?;
    let source_folder_path = document_folder_path(pool, &document).await?;
    let archived_access =
        serde_json::to_string(&archive_access_snapshot(pool, document.folder_id).await?)?;

    let mut transaction = pool.begin().await?;
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
        release_lock_in_tx(&mut transaction, lock.id, user).await?;
    }
    archive_document_in_tx(
        &mut transaction,
        ArchiveDocumentMutation {
            document: &document,
            archive_folder_id: archive_root.id,
            source_path: &source_path,
            source_folder_path: &source_folder_path,
            source_name: &document.name,
            archived_access: &archived_access,
            user,
            meta,
        },
    )
    .await?;
    transaction.commit().await?;
    Ok(join_path(&[ARCHIVE_ROOT, &document.name]))
}

pub async fn restore_document(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<String, DocumentError> {
    let document = document_for_write(pool, document_id, user).await?;
    if !document_is_archive(pool, &document).await? {
        return Err(DocumentError::DocumentNotArchived);
    }
    let source_path = document_path(pool, &document).await?;
    let Some(archived_from_folder) = document.archived_from_folder.as_deref() else {
        return Err(DocumentError::ArchivedDocumentMissingRestoreMetadata);
    };
    let Some(archived_original_name) = document.archived_original_name.as_deref() else {
        return Err(DocumentError::ArchivedDocumentMissingRestoreMetadata);
    };
    let target_folder_path = normalize_folder(Some(archived_from_folder))?;
    let target_name = normalize_file_name(archived_original_name)?;
    require_write_for_folder_path(pool, &target_folder_path, user).await?;

    let mut transaction = pool.begin().await?;
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
    }
    let target_folder =
        get_or_create_folder_path_in_tx(&mut transaction, &target_folder_path).await?;
    ensure_unique_document_name_in_tx(
        &mut transaction,
        target_folder.id,
        &target_name,
        document.id,
    )
    .await?;
    sqlx::query(
        r"
        UPDATE documents
        SET
            folder_id = ?,
            name = ?,
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?,
            archived_from_folder = NULL,
            archived_original_name = NULL,
            archived_access = NULL
        WHERE id = ?
        ",
    )
    .bind(target_folder.id)
    .bind(&target_name)
    .bind(&user.id)
    .bind(document.id)
    .execute(&mut *transaction)
    .await?;
    apply_effective_ttl_to_document_in_tx(&mut transaction, document.id, target_folder.id).await?;
    record_document_event_in_tx(
        &mut transaction,
        document.id,
        user,
        "unarchive",
        &format!("Restored to Vault from {source_path}"),
        meta,
    )
    .await?;
    transaction.commit().await?;
    Ok(join_path(&[&target_folder_path, &target_name]))
}

pub async fn archive_folder(
    pool: &SqlitePool,
    folder_id: i64,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<String, DocumentError> {
    let folders = all_folders(pool).await?;
    let source = folders
        .iter()
        .find(|folder| folder.id == folder_id)
        .ok_or(FolderError::FolderNotFound)?;
    if source.is_root {
        return Err(DocumentError::CannotArchiveRootFolder);
    }
    if source.root_key == ARCHIVE_ROOT_KEY {
        return Err(DocumentError::FolderAlreadyArchived);
    }
    let source_ids = subtree_folder_ids_from_records(source.id, &folders);
    for subtree_id in &source_ids {
        require_folder_write(pool, *subtree_id, user).await?;
    }
    let docs = documents_in_folders(pool, &source_ids).await?;
    if docs.is_empty() {
        return Err(DocumentError::FolderHasNoFilesToArchive);
    }

    let path_cache = build_folder_path_cache(&folders)?;
    let archive_root = get_root_folder(pool, ARCHIVE_ROOT_KEY).await?;
    require_folder_write(pool, archive_root.id, user).await?;
    let mut archive_items = Vec::with_capacity(docs.len());
    for document in &docs {
        let Some(folder) = folders
            .iter()
            .find(|folder| folder.id == document.folder_id)
        else {
            return Err(DocumentError::Folder(FolderError::FolderNotFound));
        };
        let folder_path = folder_path_from_cache(folder, &path_cache)?;
        archive_items.push(ArchiveDocumentItem {
            document: document.clone(),
            source_path: join_path(&[&folder_path, &document.name]),
            source_folder_path: folder_path,
            archived_access: serde_json::to_string(
                &archive_access_snapshot(pool, document.folder_id).await?,
            )?,
        });
    }

    let mut transaction = pool.begin().await?;
    for item in &archive_items {
        if let Some(lock) = active_lock_in_tx(&mut transaction, item.document.id).await? {
            ensure_lock_owner_or_admin(&lock, user)?;
            release_lock_in_tx(&mut transaction, lock.id, user).await?;
        }
        archive_document_in_tx(
            &mut transaction,
            ArchiveDocumentMutation {
                document: &item.document,
                archive_folder_id: archive_root.id,
                source_path: &item.source_path,
                source_folder_path: &item.source_folder_path,
                source_name: &item.document.name,
                archived_access: &item.archived_access,
                user,
                meta,
            },
        )
        .await?;
    }
    delete_folders_in_tx(&mut transaction, &source_ids).await?;
    transaction.commit().await?;
    Ok(ARCHIVE_ROOT.to_string())
}

pub async fn rename_document(
    pool: &SqlitePool,
    document_id: i64,
    destination_folder: Option<&str>,
    name: &str,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<String, DocumentError> {
    move_or_rename_document(
        pool,
        document_id,
        destination_folder,
        Some(name),
        user,
        meta,
    )
    .await
}

pub async fn move_document(
    pool: &SqlitePool,
    document_id: i64,
    destination_folder: &str,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<String, DocumentError> {
    move_or_rename_document(
        pool,
        document_id,
        Some(destination_folder),
        None,
        user,
        meta,
    )
    .await
}

async fn move_or_rename_document(
    pool: &SqlitePool,
    document_id: i64,
    destination_folder: Option<&str>,
    name: Option<&str>,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<String, DocumentError> {
    let document = document_for_write(pool, document_id, user).await?;
    let target_name = match name {
        Some(name) => normalize_file_name(name)?,
        None => document.name.clone(),
    };
    let source_path = document_path(pool, &document).await?;
    let destination_path = match destination_folder {
        Some(path) => normalize_folder(Some(path))?,
        None => document_folder_path(pool, &document).await?,
    };
    let source_root_key: String = sqlx::query_scalar("SELECT root_key FROM folders WHERE id = ?")
        .bind(document.folder_id)
        .fetch_one(pool)
        .await?;
    let target_ref = parse_public_folder_path(Some(&destination_path))?;
    if source_root_key != target_ref.root_key {
        return Err(DocumentError::UseArchiveOrRestoreForArchiveMoves);
    }
    let target_is_archive = target_ref.root_key == ARCHIVE_ROOT_KEY;
    if name.is_some() && document_is_archive(pool, &document).await? {
        return Err(DocumentError::RestoreArchivedBeforeRenaming);
    }
    require_write_for_folder_path(pool, &destination_path, user).await?;

    let mut transaction = pool.begin().await?;
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
    }
    let target_folder =
        get_or_create_folder_path_in_tx(&mut transaction, &destination_path).await?;
    if !target_is_archive {
        let duplicate_id = sqlx::query_scalar::<_, i64>(
            r"
            SELECT id
            FROM documents
            WHERE folder_id = ?
              AND name = ?
              AND archived_from_folder IS NULL
              AND id != ?
            LIMIT 1
            ",
        )
        .bind(target_folder.id)
        .bind(&target_name)
        .bind(document.id)
        .fetch_optional(&mut *transaction)
        .await?;
        if duplicate_id.is_some() {
            return Err(DocumentError::DocumentPathAlreadyExists);
        }
    }
    sqlx::query(
        r"
        UPDATE documents
        SET
            folder_id = ?,
            name = ?,
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?
        WHERE id = ?
        ",
    )
    .bind(target_folder.id)
    .bind(&target_name)
    .bind(&user.id)
    .bind(document.id)
    .execute(&mut *transaction)
    .await?;
    apply_effective_ttl_to_document_in_tx(&mut transaction, document.id, target_folder.id).await?;

    let target_path = join_path(&[&destination_path, &target_name]);
    record_document_event_in_tx(
        &mut transaction,
        document.id,
        user,
        "move",
        &format!("Moved from {source_path} to {target_path}"),
        meta,
    )
    .await?;
    transaction.commit().await?;
    Ok(target_path)
}

pub async fn record_document_batch_state(
    pool: &SqlitePool,
    event_type: &str,
) -> Result<(), DocumentError> {
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES (?, ?)
        ",
    )
    .bind(format!("batch.{event_type}"))
    .bind(batch_state_resources_json())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_document_deleted_state(pool: &SqlitePool) -> Result<(), DocumentError> {
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES ('document.deleted', ?)
        ",
    )
    .bind(batch_state_resources_json())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn document_folder_path(
    pool: &SqlitePool,
    document: &DocumentRecord,
) -> Result<String, DocumentError> {
    Ok(folder_path_by_id(pool, document.folder_id).await?)
}

pub async fn document_path(
    pool: &SqlitePool,
    document: &DocumentRecord,
) -> Result<String, DocumentError> {
    let folder_path = document_folder_path(pool, document).await?;
    Ok(join_path(&[&folder_path, &document.name]))
}

pub async fn document_is_archive(
    pool: &SqlitePool,
    document: &DocumentRecord,
) -> Result<bool, DocumentError> {
    let root_key: String = sqlx::query_scalar("SELECT root_key FROM folders WHERE id = ?")
        .bind(document.folder_id)
        .fetch_one(pool)
        .await?;
    Ok(root_key == ARCHIVE_ROOT_KEY)
}

pub async fn archive_access_snapshot(
    pool: &SqlitePool,
    folder_id: i64,
) -> Result<HashMap<String, i64>, DocumentError> {
    let groups = all_groups(pool).await?;
    let mut snapshot = HashMap::new();
    for group in groups {
        let user = group_access_context(&group);
        let level = folder_access_level(pool, folder_id, &user).await?;
        if level > 0 {
            snapshot.insert(group.id.to_string(), level);
        }
    }
    Ok(snapshot)
}

pub async fn archived_access_level(
    pool: &SqlitePool,
    document: &DocumentRecord,
    user: &UserContext,
) -> Result<i64, DocumentError> {
    let archive_level = folder_access_level(pool, document.folder_id, user).await?;
    if archive_level <= 0 {
        return Ok(0);
    }
    let snapshot = parse_archived_access(document.archived_access.as_deref())?;
    let groups = user_group_names(user);
    if groups.is_empty() {
        return Ok(0);
    }
    let source_level = all_groups(pool)
        .await?
        .iter()
        .filter(|group| groups.contains(&group.name.trim().to_ascii_lowercase()))
        .filter_map(|group| snapshot.get(&group.id.to_string()).copied())
        .max()
        .unwrap_or(0);
    Ok(archive_level.min(source_level))
}

pub async fn document_access_level(
    pool: &SqlitePool,
    document: &DocumentRecord,
    user: &UserContext,
) -> Result<i64, DocumentError> {
    if user.is_admin {
        return Ok(3);
    }
    if document_is_archive(pool, document).await? {
        return archived_access_level(pool, document, user).await;
    }
    Ok(folder_access_level(pool, document.folder_id, user).await?)
}

pub async fn editable_document_for_write(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<DocumentRecord, DocumentError> {
    let document = document_for_write(pool, document_id, user).await?;
    if document_is_archive(pool, &document).await? {
        return Err(DocumentError::RestoreBeforeEditing);
    }
    Ok(document)
}

pub async fn document_for_write(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<DocumentRecord, DocumentError> {
    let document = try_fetch_document_by_id(pool, document_id)
        .await?
        .ok_or(DocumentError::DocumentNotFound)?;
    let level = document_access_level(pool, &document, user).await?;
    if level >= 3 {
        return Ok(document);
    }
    if level > 0 {
        return Err(DocumentError::InsufficientDocumentAccess);
    }
    Err(DocumentError::DocumentNotFound)
}

pub async fn document_for_read(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<DocumentRecord, DocumentError> {
    let document = try_fetch_document_by_id(pool, document_id)
        .await?
        .ok_or(DocumentError::DocumentNotFound)?;
    let level = document_access_level(pool, &document, user).await?;
    if level >= 2 {
        return Ok(document);
    }
    if level > 0 {
        return Err(DocumentError::InsufficientDocumentAccess);
    }
    Err(DocumentError::DocumentNotFound)
}

async fn expired_documents(
    pool: &SqlitePool,
    limit: i64,
) -> Result<Vec<ExpiredDocumentRow>, DocumentError> {
    Ok(sqlx::query_as::<_, ExpiredDocumentRow>(
        r"
        SELECT id, folder_id, name, expiry_action
        FROM documents
        WHERE expires_at IS NOT NULL
          AND datetime(expires_at) <= datetime('now')
        ORDER BY datetime(expires_at), id
        LIMIT ?
        ",
    )
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?)
}

async fn active_locked_document_ids<I>(
    pool: &SqlitePool,
    document_ids: I,
) -> Result<HashSet<i64>, DocumentError>
where
    I: IntoIterator<Item = i64>,
{
    let document_ids = document_ids.into_iter().collect::<Vec<_>>();
    if document_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT document_id FROM document_locks WHERE is_active = 1 AND document_id IN (",
    );
    let mut separated = builder.separated(", ");
    for document_id in document_ids {
        separated.push_bind(document_id);
    }
    separated.push_unseparated(")");
    Ok(builder
        .build_query_scalar::<i64>()
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect())
}

fn expired_document_path(
    document: &ExpiredDocumentRow,
    folder_by_id: &HashMap<i64, FolderRecord>,
    path_cache: &HashMap<i64, String>,
) -> Result<String, DocumentError> {
    let folder_path = expired_document_folder_path(document, folder_by_id, path_cache)?;
    Ok(join_path(&[&folder_path, &document.name]))
}

fn expired_document_folder_path(
    document: &ExpiredDocumentRow,
    folder_by_id: &HashMap<i64, FolderRecord>,
    path_cache: &HashMap<i64, String>,
) -> Result<String, DocumentError> {
    let folder = folder_by_id
        .get(&document.folder_id)
        .ok_or(FolderError::FolderNotFound)?;
    Ok(folder_path_from_cache(folder, path_cache)?)
}

fn expired_document_is_archived(
    document: &ExpiredDocumentRow,
    folder_by_id: &HashMap<i64, FolderRecord>,
) -> Result<bool, DocumentError> {
    let folder = folder_by_id
        .get(&document.folder_id)
        .ok_or(FolderError::FolderNotFound)?;
    Ok(folder.root_key == ARCHIVE_ROOT_KEY)
}

fn expired_row_document_record(document: ExpiredDocumentRow) -> DocumentRecord {
    DocumentRecord {
        id: document.id,
        folder_id: document.folder_id,
        name: document.name,
        archived_from_folder: None,
        archived_original_name: None,
        archived_access: None,
    }
}

fn normalized_expiry_action(action: Option<&str>) -> Option<String> {
    let action = action?.trim().to_ascii_lowercase();
    matches!(action.as_str(), "archive" | "delete").then_some(action)
}

async fn clear_document_expiry_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
) -> Result<(), DocumentError> {
    sqlx::query("UPDATE documents SET expires_at = NULL, expiry_action = NULL WHERE id = ?")
        .bind(document_id)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn archive_expired_document_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    plan: &ExpiredArchivePlan,
    archive_folder_id: i64,
    timestamp: &str,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<(), DocumentError> {
    sqlx::query(
        r"
        UPDATE documents
        SET
            folder_id = ?,
            name = ?,
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?,
            archived_from_folder = ?,
            archived_original_name = ?,
            archived_access = ?
        WHERE id = ?
        ",
    )
    .bind(archive_folder_id)
    .bind(&plan.document.name)
    .bind(&user.id)
    .bind(&plan.source_folder_path)
    .bind(&plan.document.name)
    .bind(&plan.archived_access)
    .bind(plan.document.id)
    .execute(&mut **transaction)
    .await?;
    apply_effective_ttl_to_document_in_tx(transaction, plan.document.id, archive_folder_id).await?;
    record_document_event_in_tx(
        transaction,
        plan.document.id,
        user,
        "archive",
        &format!("Expired at {timestamp}; archived from {}", plan.source_path),
        meta,
    )
    .await?;
    Ok(())
}

async fn record_retention_expired_state_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), DocumentError> {
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES ('retention.expired', ?)
        ",
    )
    .bind(state_event_resources_json(&[
        "contents",
        "document_detail",
        "my_edits",
        "sidebar",
    ]))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn current_utc_minute_label(pool: &SqlitePool) -> Result<String, DocumentError> {
    Ok(
        sqlx::query_scalar("SELECT strftime('%Y-%m-%d %H:%M UTC', 'now')")
            .fetch_one(pool)
            .await?,
    )
}

fn system_user() -> UserContext {
    UserContext {
        id: "system".to_string(),
        vault_user_id: 0,
        issuer: "system".to_string(),
        subject: "system".to_string(),
        name: "System".to_string(),
        email: String::new(),
        groups: Vec::new(),
        is_admin: true,
    }
}

const fn system_meta() -> ClientMeta {
    ClientMeta {
        ip: None,
        user_agent: None,
    }
}

async fn active_lock_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
) -> Result<Option<ActiveLockRecord>, DocumentError> {
    Ok(sqlx::query_as::<_, ActiveLockRecord>(
        r"
        SELECT id, locked_by, locked_by_name
        FROM document_locks
        WHERE document_id = ? AND is_active = 1
        ",
    )
    .bind(document_id)
    .fetch_optional(&mut **transaction)
    .await?)
}

async fn release_lock_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    lock_id: i64,
    user: &UserContext,
) -> Result<(), DocumentError> {
    sqlx::query(
        r"
        UPDATE document_locks
        SET is_active = 0, released_at = CURRENT_TIMESTAMP, released_by = ?
        WHERE id = ?
        ",
    )
    .bind(&user.id)
    .bind(lock_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn ensure_lock_owner_or_admin(
    lock: &ActiveLockRecord,
    user: &UserContext,
) -> Result<(), DocumentError> {
    if lock.locked_by == user.id || user.is_admin {
        Ok(())
    } else {
        Err(DocumentError::DocumentLockedByOtherUser)
    }
}

async fn require_folder_write(
    pool: &SqlitePool,
    folder_id: i64,
    user: &UserContext,
) -> Result<(), DocumentError> {
    let level = folder_access_level(pool, folder_id, user).await?;
    if level >= 3 {
        return Ok(());
    }
    if level > 0 {
        return Err(DocumentError::Folder(FolderError::InsufficientFolderAccess));
    }
    Err(DocumentError::Folder(FolderError::FolderNotFound))
}

async fn ensure_unique_document_name_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    folder_id: i64,
    name: &str,
    document_id: i64,
) -> Result<(), DocumentError> {
    let duplicate_id = sqlx::query_scalar::<_, i64>(
        r"
        SELECT id
        FROM documents
        WHERE folder_id = ?
          AND name = ?
          AND archived_from_folder IS NULL
          AND id != ?
        LIMIT 1
        ",
    )
    .bind(folder_id)
    .bind(name)
    .bind(document_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if duplicate_id.is_some() {
        return Err(DocumentError::DocumentPathAlreadyExists);
    }
    Ok(())
}

async fn archive_document_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    mutation: ArchiveDocumentMutation<'_>,
) -> Result<(), DocumentError> {
    sqlx::query(
        r"
        UPDATE documents
        SET
            folder_id = ?,
            name = ?,
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?,
            archived_from_folder = ?,
            archived_original_name = ?,
            archived_access = ?
        WHERE id = ?
        ",
    )
    .bind(mutation.archive_folder_id)
    .bind(mutation.source_name)
    .bind(&mutation.user.id)
    .bind(mutation.source_folder_path)
    .bind(mutation.source_name)
    .bind(mutation.archived_access)
    .bind(mutation.document.id)
    .execute(&mut **transaction)
    .await?;
    apply_effective_ttl_to_document_in_tx(
        transaction,
        mutation.document.id,
        mutation.archive_folder_id,
    )
    .await?;
    record_document_event_in_tx(
        transaction,
        mutation.document.id,
        mutation.user,
        "archive",
        &format!("Archived from {}", mutation.source_path),
        mutation.meta,
    )
    .await?;
    Ok(())
}

async fn documents_in_folders(
    pool: &SqlitePool,
    folder_ids: &[i64],
) -> Result<Vec<DocumentRecord>, DocumentError> {
    if folder_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new(
        r"
        SELECT
            id,
            folder_id,
            name,
            archived_from_folder,
            archived_original_name,
            archived_access
        FROM documents
        WHERE folder_id IN (
        ",
    );
    let mut separated = builder.separated(", ");
    for folder_id in folder_ids {
        separated.push_bind(folder_id);
    }
    separated.push_unseparated(") ORDER BY id");
    Ok(builder
        .build_query_as::<DocumentRecord>()
        .fetch_all(pool)
        .await?)
}

async fn delete_folders_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    folder_ids: &[i64],
) -> Result<(), DocumentError> {
    if folder_ids.is_empty() {
        return Ok(());
    }
    let mut builder = QueryBuilder::<Sqlite>::new("DELETE FROM folders WHERE id IN (");
    let mut separated = builder.separated(", ");
    for folder_id in folder_ids {
        separated.push_bind(folder_id);
    }
    separated.push_unseparated(")");
    builder.build().execute(&mut **transaction).await?;
    Ok(())
}

async fn record_document_event_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
    user: &UserContext,
    event_type: &str,
    message: &str,
    meta: &ClientMeta,
) -> Result<(), DocumentError> {
    sqlx::query(
        r"
        INSERT INTO document_events
            (document_id, event_type, actor, actor_name, message, result, ip, user_agent)
        VALUES
            (?, ?, ?, ?, ?, 'ok', ?, ?)
        ",
    )
    .bind(document_id)
    .bind(event_type)
    .bind(&user.id)
    .bind(&user.name)
    .bind(message)
    .bind(&meta.ip)
    .bind(&meta.user_agent)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn record_document_state_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    event_type: &str,
    resources: &[&str],
) -> Result<(), DocumentError> {
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES (?, ?)
        ",
    )
    .bind(format!("document.{event_type}"))
    .bind(state_event_resources_json(resources))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn batch_state_resources_json() -> String {
    state_event_resources_json(&[
        "contents",
        "document_detail",
        "my_edits",
        "preferences",
        "sidebar",
    ])
}

#[must_use]
pub fn access_payload(level: i64) -> AccessPayload {
    AccessPayload {
        visible: level >= 1,
        read: level >= 2,
        write: level >= 3,
    }
}

pub fn parse_archived_access(
    archived_access: Option<&str>,
) -> Result<HashMap<String, i64>, DocumentError> {
    let Some(raw) = archived_access else {
        return Ok(HashMap::new());
    };
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let value = serde_json::from_str::<Value>(raw)?;
    let Some(object) = value.as_object() else {
        return Ok(HashMap::new());
    };
    Ok(object
        .iter()
        .filter_map(|(key, value)| value.as_i64().map(|level| (key.clone(), level)))
        .collect())
}

async fn all_groups(pool: &SqlitePool) -> Result<Vec<GroupRecord>, DocumentError> {
    Ok(
        sqlx::query_as::<_, GroupRecord>("SELECT id, name FROM vault_groups ORDER BY name")
            .fetch_all(pool)
            .await?,
    )
}

fn group_access_context(group: &GroupRecord) -> UserContext {
    UserContext {
        id: format!("group:{}", group.id),
        vault_user_id: 0,
        issuer: "group".to_string(),
        subject: group.name.clone(),
        name: group.name.clone(),
        email: String::new(),
        groups: vec![group.name.clone()],
        is_admin: false,
    }
}

pub fn normalize_file_name(name: &str) -> Result<String, DocumentError> {
    let cleaned = name
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return Err(DocumentError::FileNameRequired);
    }
    if cleaned == "." || cleaned == ".." || cleaned.contains('/') || has_control_char(&cleaned) {
        return Err(DocumentError::InvalidFileName);
    }
    Ok(cleaned)
}

fn has_control_char(value: &str) -> bool {
    value
        .chars()
        .any(|character| character < ' ' || character == '\u{7f}')
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

#[must_use]
pub fn access_payload_from_flags(can_view: bool, can_read: bool, can_write: bool) -> AccessPayload {
    access_payload(access_level(can_view, can_read, can_write))
}
