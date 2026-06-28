use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, SqlitePool};
use thiserror::Error;
use uuid::Uuid;

use crate::auth::UserContext;
use crate::documents::{DocumentError, document_access_level, try_fetch_document_by_id};
use crate::folders::{
    FolderError, FolderRecord, all_folders, folder_access_level, get_folder_by_path,
};
use crate::views::{self, ViewError};

const SHARE_ACCESS_MODE_INTERNAL: &str = "internal";

#[derive(Debug, Deserialize)]
pub struct CreateShareLinkRequest {
    pub target_type: String,
    pub document_id: Option<i64>,
    pub folder_id: Option<i64>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreateShareLinkResponse {
    pub code: String,
    pub url: String,
    pub target_type: String,
    pub document_id: Option<i64>,
    pub folder_id: Option<i64>,
    pub access_mode: String,
}

#[derive(Debug, Error)]
pub enum ShareError {
    #[error("invalid share target")]
    InvalidShareTarget,
    #[error("document id is required")]
    DocumentIdRequired,
    #[error("share link not found")]
    ShareLinkNotFound,
    #[error("share link expired")]
    ShareLinkExpired,
    #[error("could not create share link")]
    CouldNotCreateShareLink,
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Folder(#[from] FolderError),
    #[error(transparent)]
    View(#[from] ViewError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
struct ShareTarget {
    target_type: String,
    document_id: Option<i64>,
    folder_id: Option<i64>,
}

#[derive(Debug, FromRow)]
struct ShareLinkRow {
    id: i64,
    code: String,
    target_type: Option<String>,
    document_id: Option<i64>,
    folder_id: Option<i64>,
    expires_at: Option<String>,
    disabled_at: Option<String>,
}

pub async fn create_share_link(
    pool: &SqlitePool,
    public_url: &str,
    request: CreateShareLinkRequest,
    user: &UserContext,
) -> Result<CreateShareLinkResponse, ShareError> {
    let target = create_share_target(pool, request, user).await?;
    let code = generate_share_code(pool).await?;
    let created_by_user_id = (user.vault_user_id > 0).then_some(user.vault_user_id);
    let item_id = target.document_id.or(target.folder_id);
    sqlx::query(
        r"
        INSERT INTO share_links
            (
                code,
                target_type,
                document_id,
                folder_id,
                access_mode,
                created_by,
                created_by_name,
                created_by_user_id,
                item_type,
                item_id
            )
        VALUES
            (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ",
    )
    .bind(&code)
    .bind(&target.target_type)
    .bind(target.document_id)
    .bind(target.folder_id)
    .bind(SHARE_ACCESS_MODE_INTERNAL)
    .bind(&user.id)
    .bind(&user.name)
    .bind(created_by_user_id)
    .bind(&target.target_type)
    .bind(item_id)
    .execute(pool)
    .await?;

    Ok(CreateShareLinkResponse {
        url: share_url(public_url, &code),
        code,
        target_type: target.target_type,
        document_id: target.document_id,
        folder_id: target.folder_id,
        access_mode: SHARE_ACCESS_MODE_INTERNAL.to_string(),
    })
}

pub async fn resolve_share_link(
    pool: &SqlitePool,
    code: &str,
    user: &UserContext,
) -> Result<Value, ShareError> {
    if !valid_share_code(code) {
        return Err(ShareError::ShareLinkNotFound);
    }
    let Some(link) = fetch_share_link(pool, code).await? else {
        return Err(ShareError::ShareLinkNotFound);
    };
    if link.disabled_at.is_some() {
        return Err(ShareError::ShareLinkNotFound);
    }
    if share_link_expired(pool, link.expires_at.as_deref()).await? {
        return Err(ShareError::ShareLinkExpired);
    }
    let target_type = normalized_link_target(link.target_type.as_deref())?;
    match target_type.as_str() {
        "document" => resolve_document_share(pool, &link, user).await,
        "folder" => resolve_folder_share(pool, &link, user).await,
        _ => Err(ShareError::ShareLinkNotFound),
    }
}

async fn create_share_target(
    pool: &SqlitePool,
    request: CreateShareLinkRequest,
    user: &UserContext,
) -> Result<ShareTarget, ShareError> {
    let target_type = normalize_share_target_type(&request.target_type)?;
    if target_type == "document" {
        let document_id = request.document_id.ok_or(ShareError::DocumentIdRequired)?;
        let document = try_fetch_document_by_id(pool, document_id)
            .await?
            .ok_or(DocumentError::DocumentNotFound)?;
        if document_access_level(pool, &document, user).await? < 1 {
            return Err(DocumentError::DocumentNotFound.into());
        }
        return Ok(ShareTarget {
            target_type,
            document_id: Some(document.id),
            folder_id: None,
        });
    }

    let folder = if let Some(folder_id) = request.folder_id {
        fetch_folder_by_id(pool, folder_id).await?
    } else {
        get_folder_by_path(pool, request.path.as_deref()).await?
    }
    .ok_or(FolderError::FolderNotFound)?;
    if folder_access_level(pool, folder.id, user).await? < 1 {
        return Err(FolderError::FolderNotFound.into());
    }
    Ok(ShareTarget {
        target_type,
        document_id: None,
        folder_id: Some(folder.id),
    })
}

async fn resolve_document_share(
    pool: &SqlitePool,
    link: &ShareLinkRow,
    user: &UserContext,
) -> Result<Value, ShareError> {
    let Some(document_id) = link.document_id else {
        return Err(ShareError::ShareLinkNotFound);
    };
    if try_fetch_document_by_id(pool, document_id).await?.is_none() {
        delete_share_link(pool, link.id).await?;
        return Err(ShareError::ShareLinkNotFound);
    }
    let Some((folder, document)) =
        views::build_share_document_payload(pool, document_id, user).await?
    else {
        return Err(ShareError::ShareLinkNotFound);
    };
    Ok(json!({
        "code": link.code,
        "target_type": "document",
        "document_id": document_id,
        "folder": folder,
        "document": document,
    }))
}

async fn resolve_folder_share(
    pool: &SqlitePool,
    link: &ShareLinkRow,
    user: &UserContext,
) -> Result<Value, ShareError> {
    let Some(folder_id) = link.folder_id else {
        return Err(ShareError::ShareLinkNotFound);
    };
    if fetch_folder_by_id(pool, folder_id).await?.is_none() {
        delete_share_link(pool, link.id).await?;
        return Err(ShareError::ShareLinkNotFound);
    }
    let Some((folder, folder_item)) =
        views::build_share_folder_payload(pool, folder_id, user).await?
    else {
        return Err(ShareError::ShareLinkNotFound);
    };
    Ok(json!({
        "code": link.code,
        "target_type": "folder",
        "folder_id": folder_id,
        "folder": folder,
        "folder_item": folder_item,
    }))
}

async fn fetch_share_link(
    pool: &SqlitePool,
    code: &str,
) -> Result<Option<ShareLinkRow>, ShareError> {
    Ok(sqlx::query_as::<_, ShareLinkRow>(
        r"
        SELECT
            id,
            code,
            COALESCE(
                target_type,
                CASE
                    WHEN item_type IN ('document', 'file') THEN 'document'
                    WHEN item_type = 'folder' THEN 'folder'
                    ELSE item_type
                END
            ) AS target_type,
            COALESCE(
                document_id,
                CASE WHEN item_type IN ('document', 'file') THEN item_id END
            ) AS document_id,
            COALESCE(
                folder_id,
                CASE WHEN item_type = 'folder' THEN item_id END
            ) AS folder_id,
            expires_at,
            disabled_at
        FROM share_links
        WHERE code = ?
        ",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?)
}

async fn fetch_folder_by_id(
    pool: &SqlitePool,
    folder_id: i64,
) -> Result<Option<FolderRecord>, ShareError> {
    Ok(all_folders(pool)
        .await?
        .into_iter()
        .find(|folder| folder.id == folder_id))
}

async fn delete_share_link(pool: &SqlitePool, link_id: i64) -> Result<(), ShareError> {
    sqlx::query("DELETE FROM share_links WHERE id = ?")
        .bind(link_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn generate_share_code(pool: &SqlitePool) -> Result<String, ShareError> {
    for _ in 0..20 {
        let code = Uuid::new_v4().simple().to_string();
        let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM share_links WHERE code = ?")
            .bind(&code)
            .fetch_optional(pool)
            .await?
            .is_some();
        if !exists {
            return Ok(code);
        }
    }
    Err(ShareError::CouldNotCreateShareLink)
}

async fn share_link_expired(
    pool: &SqlitePool,
    expires_at: Option<&str>,
) -> Result<bool, ShareError> {
    let Some(expires_at) = expires_at.filter(|value| !value.trim().is_empty()) else {
        return Ok(false);
    };
    let expired = sqlx::query_scalar::<_, i64>(
        "SELECT CASE WHEN datetime(?) <= CURRENT_TIMESTAMP THEN 1 ELSE 0 END",
    )
    .bind(expires_at)
    .fetch_one(pool)
    .await?;
    Ok(expired == 1)
}

fn normalize_share_target_type(value: &str) -> Result<String, ShareError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "document" | "file" => Ok("document".to_string()),
        "folder" => Ok("folder".to_string()),
        _ => Err(ShareError::InvalidShareTarget),
    }
}

fn normalized_link_target(value: Option<&str>) -> Result<String, ShareError> {
    value
        .ok_or(ShareError::ShareLinkNotFound)
        .and_then(normalize_share_target_type)
        .map_err(|error| match error {
            ShareError::InvalidShareTarget => ShareError::ShareLinkNotFound,
            error => error,
        })
}

#[must_use]
pub fn valid_share_code(code: &str) -> bool {
    (8..=64).contains(&code.len())
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn share_url(public_url: &str, code: &str) -> String {
    let public_url = public_url.trim().trim_end_matches('/');
    if public_url.is_empty() {
        return format!("/s/{code}");
    }
    format!("{public_url}/s/{code}")
}
