use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool, Transaction};
use thiserror::Error;

use crate::auth::UserContext;
use crate::folders::{
    ARCHIVE_ROOT, ARCHIVE_ROOT_KEY, FolderError, FolderRecord, access_level, all_folders_in_tx,
    apply_effective_ttl_to_document_in_tx, archive_entry_subtree_folder_ids_from_records,
    build_folder_path_cache, direct_archived_ancestor_in_tx, folder_access_level,
    folder_access_level_in_tx, folder_access_levels_in_tx,
    folder_is_effectively_archived_from_records, folder_path_by_id, folder_path_from_cache,
    get_or_create_folder_path_in_tx, join_path, normalize_folder, parse_public_folder_path,
    require_write_for_folder_path_in_tx, subtree_folder_ids_from_records,
};
use crate::state_events::{record_state_event_in_tx, state_event_resources_json};
use crate::storage::BlobLocation;

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct DocumentRecord {
    pub id: i64,
    pub folder_id: i64,
    pub name: String,
    pub archived_at: Option<String>,
    pub archived_origin_path: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermanentDeleteResult {
    pub path: String,
    pub terminated_uploads: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RetentionSweepResult {
    pub archived: Vec<String>,
    pub deleted: Vec<String>,
    pub skipped: Vec<String>,
    #[serde(skip_serializing)]
    pub terminated_uploads: Vec<String>,
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
    pub locations: Vec<BlobLocation>,
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
    #[error("document changed while the operation was in progress")]
    DocumentStateChanged,
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
    #[error("restore the containing archived folder first")]
    ArchivedParentMustBeRestored,
    #[error("cannot archive a root folder")]
    CannotArchiveRootFolder,
    #[error("folder is already archived")]
    FolderAlreadyArchived,
    #[error("folder is not archived")]
    FolderNotArchived,
    #[error("restore the containing archived folder first")]
    ArchivedFolderParentMustBeRestored,
    #[error("finish or cancel uploads in this folder before archiving it")]
    FolderHasActiveUploads,
    #[error("folder contains separately archived items")]
    FolderContainsIndependentArchiveEntries,
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

#[derive(Debug, FromRow)]
struct VersionDownloadRow {
    document_id: i64,
    folder_id: i64,
    document_name: String,
    archived_at: Option<String>,
    archived_origin_path: Option<String>,
    archived_access: Option<String>,
    version_id: String,
    version_number: i64,
    mime_type: Option<String>,
    original_filename: Option<String>,
    hash_algo: String,
    hash: String,
    size_bytes: i64,
    blob_id: i64,
}

#[derive(Debug, Clone, FromRow)]
struct ExpiredDocumentRow {
    id: i64,
    folder_id: i64,
    name: String,
    archived_at: Option<String>,
    archived_origin_path: Option<String>,
    archived_access: Option<String>,
    expires_at: String,
    expiry_action: Option<String>,
}

struct ArchiveDocumentMutation<'a> {
    document: &'a DocumentRecord,
    archive_policy_folder_id: i64,
    source_path: &'a str,
    archived_access: &'a str,
    user: &'a UserContext,
    meta: &'a ClientMeta,
}

struct ExpiredArchivePlan {
    document: DocumentRecord,
    expires_at: String,
    expiry_action: Option<String>,
    source_path: String,
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
            archived_at,
            archived_origin_path,
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
            archived_at,
            archived_origin_path,
            archived_access
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_optional(pool)
    .await?)
}

async fn try_fetch_document_by_id_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
) -> Result<Option<DocumentRecord>, DocumentError> {
    Ok(sqlx::query_as::<_, DocumentRecord>(
        r"
        SELECT
            id,
            folder_id,
            name,
            archived_at,
            archived_origin_path,
            archived_access
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_optional(&mut **transaction)
    .await?)
}

pub async fn lock_document(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<DocumentLockResult, DocumentError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let document = editable_document_for_write_in_tx(&mut transaction, document_id, user).await?;
    let path = document_path_in_tx(&mut transaction, &document).await?;
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
        record_document_batch_state_in_tx(&mut transaction, "lock").await?;
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
    record_document_batch_state_in_tx(&mut transaction, "lock").await?;
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
    record_document_batch_state_in_tx(&mut transaction, "unlock").await?;
    transaction.commit().await?;
    Ok(DocumentLockResult {
        detail: "Unlocked".to_string(),
    })
}

pub async fn delete_document_forever(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
) -> Result<PermanentDeleteResult, DocumentError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !permanent_delete_allowed_in_tx(&mut transaction, user).await? {
        return Err(DocumentError::InsufficientDocumentAccess);
    }
    let document = document_for_write_in_tx(&mut transaction, document_id, user).await?;
    if !document_is_archive_in_tx(&mut transaction, &document).await? {
        return Err(DocumentError::MoveDocumentToArchiveBeforeDeleting);
    }
    let path = join_path(&[ARCHIVE_ROOT, &document.name]);
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
    }
    let terminated_uploads =
        terminate_document_uploads_in_tx(&mut transaction, document.id).await?;
    let deleted = sqlx::query(
        r"
        DELETE FROM documents
        WHERE id = ?
          AND folder_id = ?
          AND name = ?
        ",
    )
    .bind(document.id)
    .bind(document.folder_id)
    .bind(&document.name)
    .execute(&mut *transaction)
    .await?;
    if deleted.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
    record_document_deleted_state_in_tx(&mut transaction).await?;
    transaction.commit().await?;
    Ok(PermanentDeleteResult {
        path,
        terminated_uploads,
    })
}

async fn permanent_delete_allowed_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    user: &UserContext,
) -> Result<bool, DocumentError> {
    if user.is_admin {
        return Ok(true);
    }
    let raw = sqlx::query_scalar::<_, String>(
        "SELECT value FROM vault_settings WHERE key = 'archivePermanentDeleteAdminOnly'",
    )
    .fetch_optional(&mut **transaction)
    .await?;
    let admin_only = match raw {
        Some(raw) => serde_json::from_str::<Value>(&raw)?
            .as_bool()
            .unwrap_or(true),
        None => true,
    };
    Ok(!admin_only)
}

pub async fn sweep_expired_documents(
    pool: &SqlitePool,
    limit: i64,
) -> Result<RetentionSweepResult, DocumentError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let docs = expired_documents_in_tx(&mut transaction, limit).await?;
    if docs.is_empty() {
        transaction.commit().await?;
        return Ok(RetentionSweepResult::default());
    }
    let folders = all_folders_in_tx(&mut transaction).await?;
    let archive_root = folders
        .iter()
        .find(|folder| folder.is_root && folder.root_key == ARCHIVE_ROOT_KEY)
        .cloned()
        .ok_or(FolderError::FolderNotFound)?;
    let folder_by_id = folders
        .iter()
        .map(|folder| (folder.id, folder.clone()))
        .collect::<HashMap<_, _>>();
    let path_cache = build_folder_path_cache(&folders)?;
    let locked_ids =
        active_locked_document_ids_in_tx(&mut transaction, docs.iter().map(|doc| doc.id)).await?;
    let system = system_user();
    let meta = system_meta();
    let timestamp = current_utc_minute_label_in_tx(&mut transaction).await?;
    let mut result = RetentionSweepResult::default();
    let mut archives = Vec::new();
    let mut deletes = Vec::new();
    let mut clears = Vec::new();
    let mut archived_access_by_folder = HashMap::<i64, String>::new();

    for doc in docs {
        let path = expired_document_path(&doc, &folder_by_id, &path_cache)?;
        if locked_ids.contains(&doc.id) {
            result.skipped.push(path);
            continue;
        }
        match normalized_expiry_action(doc.expiry_action.as_deref()).as_deref() {
            Some("archive") => {
                if expired_document_is_archived(&doc, &folder_by_id)? {
                    clears.push(doc);
                } else {
                    let archived_access = if let Some(snapshot) =
                        archived_access_by_folder.get(&doc.folder_id)
                    {
                        snapshot.clone()
                    } else {
                        let snapshot = serde_json::to_string(
                            &archive_access_snapshot_in_tx(&mut transaction, doc.folder_id).await?,
                        )?;
                        archived_access_by_folder.insert(doc.folder_id, snapshot.clone());
                        snapshot
                    };
                    result.archived.push(join_path(&[ARCHIVE_ROOT, &doc.name]));
                    archives.push(ExpiredArchivePlan {
                        expires_at: doc.expires_at.clone(),
                        expiry_action: doc.expiry_action.clone(),
                        document: expired_row_document_record(doc),
                        source_path: path,
                        archived_access,
                    });
                }
            }
            Some("delete") => {
                result.deleted.push(path);
                deletes.push(doc);
            }
            _ => clears.push(doc),
        }
    }

    for document in &clears {
        clear_document_expiry_in_tx(&mut transaction, document).await?;
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
    for document in &deletes {
        result
            .terminated_uploads
            .extend(terminate_document_uploads_in_tx(&mut transaction, document.id).await?);
        delete_expired_document_in_tx(&mut transaction, document).await?;
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
            d.archived_at,
            d.archived_origin_path,
            d.archived_access,
            v.id AS version_id,
            v.version_number,
            v.mime_type,
            v.original_filename,
            b.hash_algo,
            b.hash,
            b.size_bytes,
            b.id AS blob_id
        FROM document_versions v
        JOIN documents d ON d.id = v.document_id
        JOIN blobs b ON b.id = v.blob_id
        WHERE d.id = ? AND v.id = ?
        ",
    )
    .bind(document.id)
    .bind(version_id)
    .fetch_optional(pool)
    .await?
    else {
        return Err(DocumentError::VersionNotFound);
    };
    let locations = sqlx::query_as::<_, (String, String, String)>(
        r"
        SELECT backend, bucket, object_key
        FROM blob_locations
        WHERE blob_id = ?
          AND backend NOT GLOB '_vault_pending:*'
          AND backend NOT GLOB '_vault_deleting:*'
          AND TRIM(backend) != ''
          AND TRIM(object_key) != ''
        ORDER BY id
        ",
    )
    .bind(row.blob_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(backend, bucket, object_key)| BlobLocation {
        backend,
        bucket,
        object_key,
    })
    .collect::<Vec<_>>();
    if locations.is_empty() {
        return Err(DocumentError::BlobHasNoStorageLocation);
    }
    let row_document = DocumentRecord {
        id: row.document_id,
        folder_id: row.folder_id,
        name: row.document_name.clone(),
        archived_at: row.archived_at.clone(),
        archived_origin_path: row.archived_origin_path.clone(),
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
        locations,
    })
}

/// Records an authorized download initiation, not confirmed network delivery.
/// Once response headers are returned, disconnects and body-stream failures
/// cannot safely perform a compensating database write.
pub async fn record_download_event(
    pool: &SqlitePool,
    download: &VersionDownload,
    user: &UserContext,
    meta: &ClientMeta,
    current_version: bool,
) -> Result<(), DocumentError> {
    let message = if current_version {
        format!("Started download of {}", download.document_path)
    } else {
        format!(
            "Started download of version v{} of {}",
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
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let document =
        editable_document_for_write_in_tx(&mut transaction, download.document_id, user).await?;
    let path = document_path_in_tx(&mut transaction, &document).await?;
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
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let document = document_for_write_in_tx(&mut transaction, document_id, user).await?;
    if document_is_archive_in_tx(&mut transaction, &document).await? {
        return Err(DocumentError::DocumentAlreadyArchived);
    }
    let folders = all_folders_in_tx(&mut transaction).await?;
    let archive_root = folders
        .iter()
        .find(|folder| folder.is_root && folder.root_key == ARCHIVE_ROOT_KEY)
        .ok_or(FolderError::FolderNotFound)?;
    require_folder_write_in_tx(&mut transaction, archive_root.id, user).await?;
    let source_path = document_path_in_tx(&mut transaction, &document).await?;
    let archived_access = serde_json::to_string(
        &archive_access_snapshot_in_tx(&mut transaction, document.folder_id).await?,
    )?;
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
        release_lock_in_tx(&mut transaction, lock.id, user).await?;
    }
    archive_document_in_tx(
        &mut transaction,
        ArchiveDocumentMutation {
            document: &document,
            archive_policy_folder_id: archive_root.id,
            source_path: &source_path,
            archived_access: &archived_access,
            user,
            meta,
        },
    )
    .await?;
    record_document_batch_state_in_tx(&mut transaction, "archive").await?;
    transaction.commit().await?;
    Ok(join_path(&[ARCHIVE_ROOT, &document.name]))
}

pub async fn restore_document(
    pool: &SqlitePool,
    document_id: i64,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<String, DocumentError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let document = document_for_write_in_tx(&mut transaction, document_id, user).await?;
    if document.archived_at.is_none() {
        return Err(DocumentError::DocumentNotArchived);
    }
    let Some(archived_origin_path) = document.archived_origin_path.as_deref() else {
        return Err(DocumentError::ArchivedDocumentMissingRestoreMetadata);
    };
    if direct_archived_ancestor_in_tx(&mut transaction, document.folder_id, true)
        .await?
        .is_some()
    {
        return Err(DocumentError::ArchivedParentMustBeRestored);
    }
    require_folder_write_in_tx(&mut transaction, document.folder_id, user).await?;
    let target_folder_path = document_folder_path_in_tx(&mut transaction, &document).await?;
    let target_name = document.name.clone();
    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
    }
    ensure_unique_document_name_in_tx(
        &mut transaction,
        document.folder_id,
        &target_name,
        document.id,
    )
    .await?;
    let restored = sqlx::query(
        r"
        UPDATE documents
        SET
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?,
            archived_at = NULL,
            archived_origin_path = NULL,
            archived_access = NULL
        WHERE id = ?
          AND folder_id = ?
          AND name = ?
          AND archived_at = ?
        ",
    )
    .bind(&user.id)
    .bind(document.id)
    .bind(document.folder_id)
    .bind(&document.name)
    .bind(&document.archived_at)
    .execute(&mut *transaction)
    .await?;
    if restored.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
    apply_effective_ttl_to_document_in_tx(&mut transaction, document.id, document.folder_id)
        .await?;
    record_document_event_in_tx(
        &mut transaction,
        document.id,
        user,
        "unarchive",
        &format!("Restored {archived_origin_path} to Vault"),
        meta,
    )
    .await?;
    record_document_batch_state_in_tx(&mut transaction, "restore").await?;
    transaction.commit().await?;
    Ok(join_path(&[&target_folder_path, &target_name]))
}

pub async fn archive_folder(
    pool: &SqlitePool,
    folder_id: i64,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<String, DocumentError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let folders = all_folders_in_tx(&mut transaction).await?;
    let source = folders
        .iter()
        .find(|folder| folder.id == folder_id)
        .cloned()
        .ok_or(FolderError::FolderNotFound)?;
    if source.is_root {
        return Err(DocumentError::CannotArchiveRootFolder);
    }
    if source.root_key != crate::folders::VAULT_ROOT_KEY
        || folder_is_effectively_archived_from_records(source.id, &folders)
    {
        return Err(DocumentError::FolderAlreadyArchived);
    }
    let source_ids = subtree_folder_ids_from_records(source.id, &folders);
    let active_folder_ids = source_ids
        .iter()
        .copied()
        .filter(|id| !folder_is_effectively_archived_from_records(*id, &folders))
        .collect::<Vec<_>>();
    for subtree_id in &active_folder_ids {
        require_folder_write_in_tx(&mut transaction, *subtree_id, user).await?;
    }
    if active_upload_targets_folders_in_tx(&mut transaction, &active_folder_ids).await? {
        return Err(DocumentError::FolderHasActiveUploads);
    }

    let path_cache = build_folder_path_cache(&folders)?;
    let archive_root = folders
        .iter()
        .find(|folder| folder.is_root && folder.root_key == ARCHIVE_ROOT_KEY)
        .cloned()
        .ok_or(FolderError::FolderNotFound)?;
    require_folder_write_in_tx(&mut transaction, archive_root.id, user).await?;
    let source_path = folder_path_from_cache(&source, &path_cache)?;
    let archived_access =
        serde_json::to_string(&archive_access_snapshot_in_tx(&mut transaction, source.id).await?)?;
    let documents = documents_in_folders_in_tx(&mut transaction, &active_folder_ids)
        .await?
        .into_iter()
        .filter(|document| document.archived_at.is_none())
        .collect::<Vec<_>>();
    for document in &documents {
        if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
            ensure_lock_owner_or_admin(&lock, user)?;
            release_lock_in_tx(&mut transaction, lock.id, user).await?;
        }
    }
    let archived = sqlx::query(
        r"
        UPDATE folders
        SET
            archived_at = CURRENT_TIMESTAMP,
            archived_origin_path = ?,
            archived_access = ?
        WHERE id = ?
          AND parent_id = ?
          AND name = ?
          AND archived_at IS NULL
        ",
    )
    .bind(&source_path)
    .bind(&archived_access)
    .bind(source.id)
    .bind(source.parent_id)
    .bind(&source.name)
    .execute(&mut *transaction)
    .await?;
    if archived.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
    for document in &documents {
        apply_effective_ttl_to_document_in_tx(&mut transaction, document.id, archive_root.id)
            .await?;
        record_document_event_in_tx(
            &mut transaction,
            document.id,
            user,
            "archive",
            &format!("Archived with folder {source_path}"),
            meta,
        )
        .await?;
    }
    record_folder_event_for_archive_in_tx(
        &mut transaction,
        source.id,
        user,
        "archive",
        &format!("Archived {source_path}"),
    )
    .await?;
    record_document_batch_state_in_tx(&mut transaction, "archive").await?;
    transaction.commit().await?;
    Ok(join_path(&[ARCHIVE_ROOT, &source.name]))
}

pub async fn restore_folder(
    pool: &SqlitePool,
    folder_id: i64,
    user: &UserContext,
) -> Result<String, DocumentError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let folders = all_folders_in_tx(&mut transaction).await?;
    let source = folders
        .iter()
        .find(|folder| folder.id == folder_id)
        .cloned()
        .ok_or(FolderError::FolderNotFound)?;
    if source.archived_at.is_none() {
        return Err(DocumentError::FolderNotArchived);
    }
    if direct_archived_ancestor_in_tx(&mut transaction, source.id, false)
        .await?
        .is_some()
    {
        return Err(DocumentError::ArchivedFolderParentMustBeRestored);
    }
    let parent_id = source
        .parent_id
        .ok_or(DocumentError::CannotArchiveRootFolder)?;
    let restored_folder_ids = archive_entry_subtree_folder_ids_from_records(source.id, &folders);
    require_archived_folder_subtree_write_in_tx(
        &mut transaction,
        &source,
        &restored_folder_ids,
        user,
    )
    .await?;
    require_folder_write_in_tx(&mut transaction, parent_id, user).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let restored_path = folder_path_from_cache(&source, &path_cache)?;
    let origin_path = source
        .archived_origin_path
        .as_deref()
        .ok_or(DocumentError::ArchivedDocumentMissingRestoreMetadata)?;
    clear_folder_archive_marker_in_tx(&mut transaction, &source, parent_id).await?;
    restore_folder_document_ttls_in_tx(&mut transaction, &restored_folder_ids).await?;
    record_folder_event_for_archive_in_tx(
        &mut transaction,
        source.id,
        user,
        "unarchive",
        &format!("Restored {origin_path} to Vault"),
    )
    .await?;
    record_document_batch_state_in_tx(&mut transaction, "restore").await?;
    transaction.commit().await?;
    Ok(restored_path)
}

async fn clear_folder_archive_marker_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    source: &FolderRecord,
    parent_id: i64,
) -> Result<(), DocumentError> {
    let conflict = sqlx::query_scalar::<_, i64>(
        r"
        SELECT id
        FROM folders
        WHERE parent_id = ?
          AND name = ?
          AND archived_at IS NULL
          AND id != ?
        LIMIT 1
        ",
    )
    .bind(parent_id)
    .bind(&source.name)
    .bind(source.id)
    .fetch_optional(&mut **transaction)
    .await?;
    if conflict.is_some() {
        return Err(DocumentError::Folder(
            FolderError::TargetFolderAlreadyExists,
        ));
    }
    let restored = sqlx::query(
        r"
        UPDATE folders
        SET
            archived_at = NULL,
            archived_origin_path = NULL,
            archived_access = NULL
        WHERE id = ?
          AND parent_id = ?
          AND name = ?
          AND archived_at = ?
        ",
    )
    .bind(source.id)
    .bind(parent_id)
    .bind(&source.name)
    .bind(&source.archived_at)
    .execute(&mut **transaction)
    .await?;
    if restored.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
    Ok(())
}

async fn restore_folder_document_ttls_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    restored_folder_ids: &[i64],
) -> Result<(), DocumentError> {
    let updated_folders = all_folders_in_tx(transaction).await?;
    let documents = documents_in_folders_in_tx(transaction, restored_folder_ids).await?;
    for document in documents {
        if document.archived_at.is_none()
            && !folder_is_effectively_archived_from_records(document.folder_id, &updated_folders)
        {
            apply_effective_ttl_to_document_in_tx(transaction, document.id, document.folder_id)
                .await?;
        }
    }
    Ok(())
}

pub async fn delete_archived_folder_forever(
    pool: &SqlitePool,
    folder_id: i64,
    user: &UserContext,
) -> Result<PermanentDeleteResult, DocumentError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !permanent_delete_allowed_in_tx(&mut transaction, user).await? {
        return Err(DocumentError::InsufficientDocumentAccess);
    }
    let folders = all_folders_in_tx(&mut transaction).await?;
    let source = folders
        .iter()
        .find(|folder| folder.id == folder_id)
        .cloned()
        .ok_or(FolderError::FolderNotFound)?;
    if source.archived_at.is_none() {
        return Err(DocumentError::FolderNotArchived);
    }
    let owned_source_ids = archive_entry_subtree_folder_ids_from_records(source.id, &folders);
    require_archived_folder_subtree_write_in_tx(&mut transaction, &source, &owned_source_ids, user)
        .await?;
    let source_ids = subtree_folder_ids_from_records(source.id, &folders);
    let documents = documents_in_folders_in_tx(&mut transaction, &source_ids).await?;
    let source_id_set = source_ids.iter().copied().collect::<HashSet<_>>();
    let has_independent_archive_entries = documents
        .iter()
        .any(|document| document.archived_at.is_some())
        || folders.iter().any(|folder| {
            folder.id != source.id
                && folder.archived_at.is_some()
                && source_id_set.contains(&folder.id)
        });
    if has_independent_archive_entries {
        return Err(DocumentError::FolderContainsIndependentArchiveEntries);
    }
    let mut terminated_uploads = Vec::new();
    for document in &documents {
        if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
            ensure_lock_owner_or_admin(&lock, user)?;
        }
        terminated_uploads
            .extend(terminate_document_uploads_in_tx(&mut transaction, document.id).await?);
    }
    terminated_uploads
        .extend(terminate_create_uploads_in_folders_in_tx(&mut transaction, &source_ids).await?);
    terminated_uploads.sort();
    terminated_uploads.dedup();
    let path = source
        .archived_origin_path
        .clone()
        .unwrap_or_else(|| source.name.clone());
    let deleted =
        sqlx::query("DELETE FROM folders WHERE id = ? AND is_root = 0 AND archived_at = ?")
            .bind(source.id)
            .bind(&source.archived_at)
            .execute(&mut *transaction)
            .await?;
    if deleted.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
    record_document_deleted_state_in_tx(&mut transaction).await?;
    transaction.commit().await?;
    Ok(PermanentDeleteResult {
        path,
        terminated_uploads,
    })
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
    let normalized_name = name.map(normalize_file_name).transpose()?;
    let normalized_destination = destination_folder
        .map(|path| normalize_folder(Some(path)))
        .transpose()?;
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let document = document_for_write_in_tx(&mut transaction, document_id, user).await?;
    let target_name = match normalized_name {
        Some(name) => name,
        None => document.name.clone(),
    };
    let source_path = document_path_in_tx(&mut transaction, &document).await?;
    let destination_path = match normalized_destination {
        Some(path) => path,
        None => document_folder_path_in_tx(&mut transaction, &document).await?,
    };
    let source_root_key: String = sqlx::query_scalar("SELECT root_key FROM folders WHERE id = ?")
        .bind(document.folder_id)
        .fetch_one(&mut *transaction)
        .await?;
    let target_ref = parse_public_folder_path(Some(&destination_path))?;
    if source_root_key != target_ref.root_key {
        return Err(DocumentError::UseArchiveOrRestoreForArchiveMoves);
    }
    if document_is_archive_in_tx(&mut transaction, &document).await? {
        return if name.is_some() {
            Err(DocumentError::RestoreArchivedBeforeRenaming)
        } else {
            Err(DocumentError::UseArchiveOrRestoreForArchiveMoves)
        };
    }
    require_write_for_folder_path_in_tx(&mut transaction, &destination_path, user).await?;

    if let Some(lock) = active_lock_in_tx(&mut transaction, document.id).await? {
        ensure_lock_owner_or_admin(&lock, user)?;
    }
    let target_folder =
        get_or_create_folder_path_in_tx(&mut transaction, &destination_path).await?;
    let duplicate_id = sqlx::query_scalar::<_, i64>(
        r"
        SELECT id
        FROM documents
        WHERE folder_id = ?
          AND name = ?
          AND archived_at IS NULL
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
    let moved = sqlx::query(
        r"
        UPDATE documents
        SET
            folder_id = ?,
            name = ?,
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?
        WHERE id = ?
          AND folder_id = ?
          AND name = ?
          AND archived_at IS ?
        ",
    )
    .bind(target_folder.id)
    .bind(&target_name)
    .bind(&user.id)
    .bind(document.id)
    .bind(document.folder_id)
    .bind(&document.name)
    .bind(&document.archived_at)
    .execute(&mut *transaction)
    .await?;
    if moved.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
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
    let batch_event_type = if name.is_some() { "rename" } else { "move" };
    record_document_batch_state_in_tx(&mut transaction, batch_event_type).await?;
    transaction.commit().await?;
    Ok(target_path)
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
    if document.archived_at.is_some() {
        return Ok(true);
    }
    let folders = crate::folders::all_folders(pool).await?;
    Ok(folder_is_effectively_archived_from_records(
        document.folder_id,
        &folders,
    ))
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

async fn archive_access_snapshot_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    folder_id: i64,
) -> Result<HashMap<String, i64>, DocumentError> {
    let groups = all_groups_in_tx(transaction).await?;
    let mut snapshot = HashMap::new();
    for group in groups {
        let user = group_access_context(&group);
        let level = folder_access_level_in_tx(transaction, folder_id, &user).await?;
        if level > 0 {
            snapshot.insert(group.id.to_string(), level);
        }
    }
    Ok(snapshot)
}

fn archived_access_snapshot_for_document<'a>(
    document: &'a DocumentRecord,
    folders: &'a [FolderRecord],
) -> Result<&'a str, DocumentError> {
    if document.archived_at.is_some() {
        return document
            .archived_access
            .as_deref()
            .ok_or(DocumentError::ArchivedDocumentMissingRestoreMetadata);
    }
    direct_archive_marker_for_folder(document.folder_id, folders)?
        .archived_access
        .as_deref()
        .ok_or(DocumentError::ArchivedDocumentMissingRestoreMetadata)
}

fn direct_archive_marker_for_folder(
    folder_id: i64,
    folders: &[FolderRecord],
) -> Result<&FolderRecord, DocumentError> {
    let by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let mut current = by_id.get(&folder_id).copied();
    let mut visited = HashSet::new();
    while let Some(folder) = current {
        if !visited.insert(folder.id) {
            return Err(FolderError::InvalidStoredHierarchy.into());
        }
        if folder.archived_at.is_some() {
            return Ok(folder);
        }
        current = folder
            .parent_id
            .and_then(|parent_id| by_id.get(&parent_id).copied());
    }
    Err(DocumentError::FolderNotArchived)
}

async fn require_archived_folder_subtree_write_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    archive_marker: &FolderRecord,
    folder_ids: &[i64],
    user: &UserContext,
) -> Result<(), DocumentError> {
    if user.is_admin {
        return Ok(());
    }
    let folders = all_folders_in_tx(transaction).await?;
    let archive_root = folders
        .iter()
        .find(|candidate| candidate.is_root && candidate.root_key == ARCHIVE_ROOT_KEY)
        .ok_or(FolderError::FolderNotFound)?;
    let archive_level = folder_access_level_in_tx(transaction, archive_root.id, user).await?;
    let snapshot = parse_archived_access(archive_marker.archived_access.as_deref())?;
    let groups = user_group_names(user);
    let source_level = all_groups_in_tx(transaction)
        .await?
        .iter()
        .filter(|group| groups.contains(&group.name.trim().to_ascii_lowercase()))
        .filter_map(|group| snapshot.get(&group.id.to_string()).copied())
        .max()
        .unwrap_or(0);
    if archive_level.min(source_level) < 3 {
        return Err(DocumentError::InsufficientDocumentAccess);
    }
    let current_levels = folder_access_levels_in_tx(transaction, folder_ids, user).await?;
    if folder_ids
        .iter()
        .any(|folder_id| current_levels.get(folder_id).copied().unwrap_or(0) < 3)
    {
        return Err(DocumentError::InsufficientDocumentAccess);
    }
    Ok(())
}

pub async fn archived_folder_access_level(
    pool: &SqlitePool,
    folder: &FolderRecord,
    user: &UserContext,
) -> Result<i64, DocumentError> {
    if user.is_admin {
        return Ok(3);
    }
    let folders = crate::folders::all_folders(pool).await?;
    let archive_root = folders
        .iter()
        .find(|candidate| candidate.is_root && candidate.root_key == ARCHIVE_ROOT_KEY)
        .ok_or(FolderError::FolderNotFound)?;
    let archive_level = folder_access_level(pool, archive_root.id, user).await?;
    let current_level = folder_access_level(pool, folder.id, user).await?;
    if archive_level <= 0 || current_level <= 0 {
        return Ok(0);
    }
    let marker = direct_archive_marker_for_folder(folder.id, &folders)?;
    let snapshot = parse_archived_access(marker.archived_access.as_deref())?;
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
    Ok(archive_level.min(current_level).min(source_level))
}

pub async fn archived_access_level(
    pool: &SqlitePool,
    document: &DocumentRecord,
    user: &UserContext,
) -> Result<i64, DocumentError> {
    if user.is_admin {
        return Ok(3);
    }
    let folders = crate::folders::all_folders(pool).await?;
    let archive_root = folders
        .iter()
        .find(|folder| folder.is_root && folder.root_key == ARCHIVE_ROOT_KEY)
        .ok_or(FolderError::FolderNotFound)?;
    let archive_level = folder_access_level(pool, archive_root.id, user).await?;
    if archive_level <= 0 {
        return Ok(0);
    }
    let current_level = folder_access_level(pool, document.folder_id, user).await?;
    if current_level <= 0 {
        return Ok(0);
    }
    let snapshot_json = archived_access_snapshot_for_document(document, &folders)?;
    let snapshot = parse_archived_access(Some(snapshot_json))?;
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
    Ok(archive_level.min(current_level).min(source_level))
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

async fn document_access_level_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document: &DocumentRecord,
    user: &UserContext,
) -> Result<i64, DocumentError> {
    if user.is_admin {
        return Ok(3);
    }
    if document_is_archive_in_tx(transaction, document).await? {
        return archived_access_level_in_tx(transaction, document, user).await;
    }
    Ok(folder_access_level_in_tx(transaction, document.folder_id, user).await?)
}

async fn archived_access_level_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document: &DocumentRecord,
    user: &UserContext,
) -> Result<i64, DocumentError> {
    if user.is_admin {
        return Ok(3);
    }
    let folders = all_folders_in_tx(transaction).await?;
    let archive_root = folders
        .iter()
        .find(|folder| folder.is_root && folder.root_key == ARCHIVE_ROOT_KEY)
        .ok_or(FolderError::FolderNotFound)?;
    let archive_level = folder_access_level_in_tx(transaction, archive_root.id, user).await?;
    if archive_level <= 0 {
        return Ok(0);
    }
    let current_level = folder_access_level_in_tx(transaction, document.folder_id, user).await?;
    if current_level <= 0 {
        return Ok(0);
    }
    let snapshot_json = archived_access_snapshot_for_document(document, &folders)?;
    let snapshot = parse_archived_access(Some(snapshot_json))?;
    let groups = user_group_names(user);
    if groups.is_empty() {
        return Ok(0);
    }
    let source_level = all_groups_in_tx(transaction)
        .await?
        .iter()
        .filter(|group| groups.contains(&group.name.trim().to_ascii_lowercase()))
        .filter_map(|group| snapshot.get(&group.id.to_string()).copied())
        .max()
        .unwrap_or(0);
    Ok(archive_level.min(current_level).min(source_level))
}

async fn document_for_write_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
    user: &UserContext,
) -> Result<DocumentRecord, DocumentError> {
    let document = try_fetch_document_by_id_in_tx(transaction, document_id)
        .await?
        .ok_or(DocumentError::DocumentNotFound)?;
    let level = document_access_level_in_tx(transaction, &document, user).await?;
    if level >= 3 {
        return Ok(document);
    }
    if level > 0 {
        return Err(DocumentError::InsufficientDocumentAccess);
    }
    Err(DocumentError::DocumentNotFound)
}

async fn editable_document_for_write_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
    user: &UserContext,
) -> Result<DocumentRecord, DocumentError> {
    let document = document_for_write_in_tx(transaction, document_id, user).await?;
    if document_is_archive_in_tx(transaction, &document).await? {
        return Err(DocumentError::RestoreBeforeEditing);
    }
    Ok(document)
}

async fn document_path_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document: &DocumentRecord,
) -> Result<String, DocumentError> {
    let folder_path = document_folder_path_in_tx(transaction, document).await?;
    Ok(join_path(&[&folder_path, &document.name]))
}

async fn document_folder_path_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document: &DocumentRecord,
) -> Result<String, DocumentError> {
    let folders = all_folders_in_tx(transaction).await?;
    let path_cache = build_folder_path_cache(&folders)?;
    let folder = folders
        .iter()
        .find(|folder| folder.id == document.folder_id)
        .ok_or(FolderError::FolderNotFound)?;
    Ok(folder_path_from_cache(folder, &path_cache)?)
}

async fn document_is_archive_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document: &DocumentRecord,
) -> Result<bool, DocumentError> {
    if document.archived_at.is_some() {
        return Ok(true);
    }
    let folders = all_folders_in_tx(transaction).await?;
    Ok(folder_is_effectively_archived_from_records(
        document.folder_id,
        &folders,
    ))
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

async fn expired_documents_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    limit: i64,
) -> Result<Vec<ExpiredDocumentRow>, DocumentError> {
    Ok(sqlx::query_as::<_, ExpiredDocumentRow>(
        r"
        SELECT
            id,
            folder_id,
            name,
            archived_at,
            archived_origin_path,
            archived_access,
            expires_at,
            expiry_action
        FROM documents
        WHERE expires_at IS NOT NULL
          AND datetime(expires_at) <= datetime('now')
        ORDER BY datetime(expires_at), id
        LIMIT ?
        ",
    )
    .bind(limit.max(1))
    .fetch_all(&mut **transaction)
    .await?)
}

async fn active_locked_document_ids_in_tx<I>(
    transaction: &mut Transaction<'_, Sqlite>,
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
        .fetch_all(&mut **transaction)
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
    if document.archived_at.is_some() {
        return Ok(true);
    }
    let folders = folder_by_id.values().cloned().collect::<Vec<_>>();
    if !folder_by_id.contains_key(&document.folder_id) {
        return Err(FolderError::FolderNotFound.into());
    }
    Ok(folder_is_effectively_archived_from_records(
        document.folder_id,
        &folders,
    ))
}

fn expired_row_document_record(document: ExpiredDocumentRow) -> DocumentRecord {
    DocumentRecord {
        id: document.id,
        folder_id: document.folder_id,
        name: document.name,
        archived_at: document.archived_at,
        archived_origin_path: document.archived_origin_path,
        archived_access: document.archived_access,
    }
}

fn normalized_expiry_action(action: Option<&str>) -> Option<String> {
    let action = action?.trim().to_ascii_lowercase();
    matches!(action.as_str(), "archive" | "delete").then_some(action)
}

async fn clear_document_expiry_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document: &ExpiredDocumentRow,
) -> Result<(), DocumentError> {
    let cleared = sqlx::query(
        r"
        UPDATE documents
        SET expires_at = NULL, expiry_action = NULL
        WHERE id = ?
          AND folder_id = ?
          AND name = ?
          AND expires_at = ?
          AND expiry_action IS ?
          AND datetime(expires_at) <= datetime('now')
          AND NOT EXISTS (
              SELECT 1
              FROM document_locks
              WHERE document_id = documents.id AND is_active = 1
          )
        ",
    )
    .bind(document.id)
    .bind(document.folder_id)
    .bind(&document.name)
    .bind(&document.expires_at)
    .bind(&document.expiry_action)
    .execute(&mut **transaction)
    .await?;
    if cleared.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
    Ok(())
}

async fn archive_expired_document_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    plan: &ExpiredArchivePlan,
    archive_policy_folder_id: i64,
    timestamp: &str,
    user: &UserContext,
    meta: &ClientMeta,
) -> Result<(), DocumentError> {
    let archived = sqlx::query(
        r"
        UPDATE documents
        SET
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?,
            archived_at = CURRENT_TIMESTAMP,
            archived_origin_path = ?,
            archived_access = ?
        WHERE id = ?
          AND folder_id = ?
          AND name = ?
          AND archived_at IS NULL
          AND expires_at = ?
          AND expiry_action IS ?
          AND datetime(expires_at) <= datetime('now')
          AND lower(trim(expiry_action)) = 'archive'
          AND NOT EXISTS (
              SELECT 1
              FROM document_locks
              WHERE document_id = documents.id AND is_active = 1
          )
        ",
    )
    .bind(&user.id)
    .bind(&plan.source_path)
    .bind(&plan.archived_access)
    .bind(plan.document.id)
    .bind(plan.document.folder_id)
    .bind(&plan.document.name)
    .bind(&plan.expires_at)
    .bind(&plan.expiry_action)
    .execute(&mut **transaction)
    .await?;
    if archived.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
    apply_effective_ttl_to_document_in_tx(transaction, plan.document.id, archive_policy_folder_id)
        .await?;
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

async fn delete_expired_document_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document: &ExpiredDocumentRow,
) -> Result<(), DocumentError> {
    let deleted = sqlx::query(
        r"
        DELETE FROM documents
        WHERE id = ?
          AND folder_id = ?
          AND name = ?
          AND expires_at = ?
          AND expiry_action IS ?
          AND datetime(expires_at) <= datetime('now')
          AND lower(trim(expiry_action)) = 'delete'
          AND NOT EXISTS (
              SELECT 1
              FROM document_locks
              WHERE document_id = documents.id AND is_active = 1
          )
        ",
    )
    .bind(document.id)
    .bind(document.folder_id)
    .bind(&document.name)
    .bind(&document.expires_at)
    .bind(&document.expiry_action)
    .execute(&mut **transaction)
    .await?;
    if deleted.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
    Ok(())
}

async fn terminate_document_uploads_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    document_id: i64,
) -> Result<Vec<String>, sqlx::Error> {
    let session_ids = sqlx::query_scalar::<_, String>(
        "SELECT id FROM upload_sessions WHERE document_id = ? ORDER BY id",
    )
    .bind(document_id)
    .fetch_all(&mut **transaction)
    .await?;
    if session_ids.is_empty() {
        return Ok(session_ids);
    }
    sqlx::query(
        r"
        UPDATE upload_sessions
        SET status = 'aborted',
            aborted_at = CURRENT_TIMESTAMP,
            updated_at = CURRENT_TIMESTAMP
        WHERE document_id = ?
          AND status IN ('active', 'completing')
        ",
    )
    .bind(document_id)
    .execute(&mut **transaction)
    .await?;
    Ok(session_ids)
}

async fn active_upload_targets_folders_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    folder_ids: &[i64],
) -> Result<bool, DocumentError> {
    if folder_ids.is_empty() {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, i64>(
        r"
        SELECT EXISTS (
            SELECT 1
            FROM upload_sessions
            WHERE status IN ('active', 'completing')
              AND target_folder_id IN (
                  SELECT CAST(value AS INTEGER) FROM json_each(?)
              )
        )
        ",
    )
    .bind(serde_json::to_string(folder_ids)?)
    .fetch_one(&mut **transaction)
    .await?
        != 0)
}

async fn terminate_create_uploads_in_folders_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    folder_ids: &[i64],
) -> Result<Vec<String>, DocumentError> {
    if folder_ids.is_empty() {
        return Ok(Vec::new());
    }
    let folder_ids_json = serde_json::to_string(folder_ids)?;
    let session_ids = sqlx::query_scalar::<_, String>(
        r"
        SELECT id
        FROM upload_sessions
        WHERE status IN ('active', 'completing')
          AND target_folder_id IN (
              SELECT CAST(value AS INTEGER) FROM json_each(?)
          )
        ORDER BY id
        ",
    )
    .bind(&folder_ids_json)
    .fetch_all(&mut **transaction)
    .await?;
    if !session_ids.is_empty() {
        sqlx::query(
            r"
            UPDATE upload_sessions
            SET
                status = 'aborted',
                aborted_at = CURRENT_TIMESTAMP,
                updated_at = CURRENT_TIMESTAMP
            WHERE status IN ('active', 'completing')
              AND target_folder_id IN (
                  SELECT CAST(value AS INTEGER) FROM json_each(?)
              )
            ",
        )
        .bind(folder_ids_json)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(session_ids)
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

async fn current_utc_minute_label_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<String, DocumentError> {
    Ok(
        sqlx::query_scalar("SELECT strftime('%Y-%m-%d %H:%M UTC', 'now')")
            .fetch_one(&mut **transaction)
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

async fn require_folder_write_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    folder_id: i64,
    user: &UserContext,
) -> Result<(), DocumentError> {
    let level = folder_access_level_in_tx(transaction, folder_id, user).await?;
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
          AND archived_at IS NULL
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
    let archived = sqlx::query(
        r"
        UPDATE documents
        SET
            latest_modified_at = CURRENT_TIMESTAMP,
            latest_modified_by = ?,
            archived_at = CURRENT_TIMESTAMP,
            archived_origin_path = ?,
            archived_access = ?
        WHERE id = ?
          AND folder_id = ?
          AND name = ?
          AND archived_at IS NULL
        ",
    )
    .bind(&mutation.user.id)
    .bind(mutation.source_path)
    .bind(mutation.archived_access)
    .bind(mutation.document.id)
    .bind(mutation.document.folder_id)
    .bind(&mutation.document.name)
    .execute(&mut **transaction)
    .await?;
    if archived.rows_affected() != 1 {
        return Err(DocumentError::DocumentStateChanged);
    }
    apply_effective_ttl_to_document_in_tx(
        transaction,
        mutation.document.id,
        mutation.archive_policy_folder_id,
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

async fn documents_in_folders_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
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
            archived_at,
            archived_origin_path,
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
        .fetch_all(&mut **transaction)
        .await?)
}

async fn record_folder_event_for_archive_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    folder_id: i64,
    user: &UserContext,
    event_type: &str,
    message: &str,
) -> Result<(), DocumentError> {
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

async fn record_document_batch_state_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    event_type: &str,
) -> Result<(), DocumentError> {
    record_state_event_in_tx(
        transaction,
        &format!("batch.{event_type}"),
        batch_state_resources(),
    )
    .await?;
    Ok(())
}

async fn record_document_deleted_state_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), DocumentError> {
    record_state_event_in_tx(transaction, "document.deleted", batch_state_resources()).await?;
    Ok(())
}

fn batch_state_resources() -> &'static [&'static str] {
    &[
        "contents",
        "document_detail",
        "my_edits",
        "preferences",
        "sidebar",
    ]
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

async fn all_groups_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<Vec<GroupRecord>, DocumentError> {
    Ok(
        sqlx::query_as::<_, GroupRecord>("SELECT id, name FROM vault_groups ORDER BY name")
            .fetch_all(&mut **transaction)
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
