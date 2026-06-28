use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, SqlitePool};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

use crate::auth::{AuthSettings, effective_admin_from_parts};
use crate::site_settings::{SiteSettingsError, site_settings_for_db};
use crate::state_events::state_event_resources_json;

#[derive(Debug, Error)]
pub enum AdminError {
    #[error("At least one active admin is required")]
    LastActiveAdminRequired,
    #[error("User not found")]
    UserNotFound,
    #[error("Group name is required")]
    GroupNameRequired,
    #[error("Invalid group name")]
    InvalidGroupName,
    #[error("Group already exists")]
    GroupAlreadyExists,
    #[error("Group not found")]
    GroupNotFound,
    #[error("Group or user not found")]
    GroupOrUserNotFound,
    #[error("Membership not found")]
    MembershipNotFound,
    #[error("Group is used by folder permissions")]
    GroupUsedByFolderPermissions,
    #[error(transparent)]
    Settings(#[from] SiteSettingsError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminDirectoryPayload {
    pub users: Vec<AdminUserPayload>,
    pub groups: Vec<AdminGroupPayload>,
    pub dev_mode: bool,
    pub settings: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminUserUpdatePayload {
    pub is_admin: Option<bool>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminGroupRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminGroupMemberRequest {
    pub user_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminUserPayload {
    pub id: i64,
    pub issuer: String,
    pub subject: String,
    pub email: String,
    pub name: String,
    pub is_admin: bool,
    pub is_active: bool,
    pub created_at: Option<String>,
    pub last_login_at: Option<String>,
    pub last_seen_at: Option<String>,
    pub groups: Vec<AdminUserGroupPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminUserGroupPayload {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminGroupPayload {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub members: Vec<AdminGroupMemberPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminGroupMemberPayload {
    pub id: i64,
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, FromRow)]
struct UserRow {
    id: i64,
    issuer: String,
    subject: String,
    email: Option<String>,
    name: String,
    is_admin: bool,
    is_active: bool,
    created_at: Option<String>,
    last_login_at: Option<String>,
    last_seen_at: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct GroupRow {
    id: i64,
    name: String,
    description: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct MembershipRow {
    user_id: i64,
    group_id: i64,
}

pub async fn build_admin_directory_payload(
    pool: &SqlitePool,
    auth: &AuthSettings,
) -> Result<AdminDirectoryPayload, AdminError> {
    let users = sqlx::query_as::<_, UserRow>(
        r"
        SELECT
            id,
            issuer,
            subject,
            email,
            name,
            is_admin,
            is_active,
            created_at,
            last_login_at,
            last_seen_at
        FROM vault_users
        ORDER BY name, email, id
        ",
    )
    .fetch_all(pool)
    .await?;
    let groups = sqlx::query_as::<_, GroupRow>(
        r"
        SELECT id, name, description
        FROM vault_groups
        ORDER BY name
        ",
    )
    .fetch_all(pool)
    .await?;
    let memberships = sqlx::query_as::<_, MembershipRow>(
        r"
        SELECT user_id, group_id
        FROM vault_group_memberships
        ",
    )
    .fetch_all(pool)
    .await?;

    let users_by_id = users
        .iter()
        .map(|user| (user.id, user))
        .collect::<HashMap<_, _>>();
    let groups_by_id = groups
        .iter()
        .map(|group| (group.id, group))
        .collect::<HashMap<_, _>>();
    let mut group_ids_by_user: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut user_ids_by_group: HashMap<i64, Vec<i64>> = HashMap::new();
    for membership in memberships {
        if users_by_id.contains_key(&membership.user_id)
            && groups_by_id.contains_key(&membership.group_id)
        {
            group_ids_by_user
                .entry(membership.user_id)
                .or_default()
                .push(membership.group_id);
            user_ids_by_group
                .entry(membership.group_id)
                .or_default()
                .push(membership.user_id);
        }
    }

    Ok(AdminDirectoryPayload {
        users: users
            .iter()
            .map(|user| user_payload(user, auth, &groups_by_id, &group_ids_by_user))
            .collect(),
        groups: groups
            .iter()
            .map(|group| group_payload(group, &users_by_id, &user_ids_by_group))
            .collect(),
        dev_mode: auth.dev_mode,
        settings: site_settings_for_db(pool).await?,
    })
}

pub async fn update_user(
    pool: &SqlitePool,
    auth: &AuthSettings,
    user_id: i64,
    payload: &AdminUserUpdatePayload,
) -> Result<(), AdminError> {
    let target = user_by_id(pool, user_id)
        .await?
        .ok_or(AdminError::UserNotFound)?;
    if payload.is_admin == Some(false) || payload.is_active == Some(false) {
        ensure_not_last_active_admin(pool, auth, &target).await?;
    }
    sqlx::query(
        r"
        UPDATE vault_users
        SET
            is_admin = COALESCE(?, is_admin),
            is_active = COALESCE(?, is_active)
        WHERE id = ?
        ",
    )
    .bind(payload.is_admin)
    .bind(payload.is_active)
    .bind(user_id)
    .execute(pool)
    .await?;
    record_admin_change(pool, "admin.user.updated", &["admin"]).await?;
    Ok(())
}

pub async fn create_group(
    pool: &SqlitePool,
    request: &AdminGroupRequest,
) -> Result<(), AdminError> {
    let name = normalize_group_name(&request.name)?;
    if find_group_by_normalized_name(pool, &name).await?.is_some() {
        return Err(AdminError::GroupAlreadyExists);
    }
    sqlx::query("INSERT INTO vault_groups (name, description) VALUES (?, ?)")
        .bind(name)
        .bind(normalize_optional_description(
            request.description.as_deref(),
        ))
        .execute(pool)
        .await?;
    record_admin_change(pool, "admin.group.created", &["admin"]).await?;
    Ok(())
}

pub async fn update_group(
    pool: &SqlitePool,
    auth: &AuthSettings,
    group_id: i64,
    request: &AdminGroupRequest,
) -> Result<(), AdminError> {
    let group = group_by_id(pool, group_id)
        .await?
        .ok_or(AdminError::GroupNotFound)?;
    let name = normalize_group_name(&request.name)?;
    if let Some(existing) = find_group_by_normalized_name(pool, &name).await?
        && existing.id != group.id
    {
        return Err(AdminError::GroupAlreadyExists);
    }
    ensure_active_admin_after_group_change(
        pool,
        auth,
        GroupAdminChange::Rename {
            group_id,
            new_name: name.clone(),
        },
    )
    .await?;
    sqlx::query("UPDATE vault_groups SET name = ?, description = ? WHERE id = ?")
        .bind(name)
        .bind(normalize_optional_description(
            request.description.as_deref(),
        ))
        .bind(group_id)
        .execute(pool)
        .await?;
    record_admin_change(pool, "admin.group.updated", &["admin"]).await?;
    Ok(())
}

pub async fn delete_group(
    pool: &SqlitePool,
    auth: &AuthSettings,
    group_id: i64,
) -> Result<(), AdminError> {
    let group = group_by_id(pool, group_id)
        .await?
        .ok_or(AdminError::GroupNotFound)?;
    let permission_id = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM folder_permissions WHERE group_id = ? LIMIT 1",
    )
    .bind(group.id)
    .fetch_optional(pool)
    .await?;
    if permission_id.is_some() {
        return Err(AdminError::GroupUsedByFolderPermissions);
    }
    ensure_active_admin_after_group_change(pool, auth, GroupAdminChange::Delete { group_id })
        .await?;
    sqlx::query("DELETE FROM vault_groups WHERE id = ?")
        .bind(group_id)
        .execute(pool)
        .await?;
    record_admin_change(pool, "admin.group.deleted", &["admin"]).await?;
    Ok(())
}

pub async fn add_group_member(
    pool: &SqlitePool,
    group_id: i64,
    request: &AdminGroupMemberRequest,
) -> Result<(), AdminError> {
    if group_by_id(pool, group_id).await?.is_none()
        || user_by_id(pool, request.user_id).await?.is_none()
    {
        return Err(AdminError::GroupOrUserNotFound);
    }
    sqlx::query(
        r"
        INSERT OR IGNORE INTO vault_group_memberships (user_id, group_id)
        VALUES (?, ?)
        ",
    )
    .bind(request.user_id)
    .bind(group_id)
    .execute(pool)
    .await?;
    record_admin_change(pool, "admin.group.member.added", &["admin"]).await?;
    Ok(())
}

pub async fn remove_group_member(
    pool: &SqlitePool,
    auth: &AuthSettings,
    group_id: i64,
    user_id: i64,
) -> Result<(), AdminError> {
    let membership_exists = sqlx::query_scalar::<_, i64>(
        r"
        SELECT id
        FROM vault_group_memberships
        WHERE group_id = ? AND user_id = ?
        ",
    )
    .bind(group_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .is_some();
    if !membership_exists {
        return Err(AdminError::MembershipNotFound);
    }
    ensure_active_admin_after_group_change(
        pool,
        auth,
        GroupAdminChange::RemoveMembership { group_id, user_id },
    )
    .await?;
    sqlx::query("DELETE FROM vault_group_memberships WHERE group_id = ? AND user_id = ?")
        .bind(group_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    record_admin_change(pool, "admin.group.member.removed", &["admin"]).await?;
    Ok(())
}

fn user_payload(
    user: &UserRow,
    auth: &AuthSettings,
    groups_by_id: &HashMap<i64, &GroupRow>,
    group_ids_by_user: &HashMap<i64, Vec<i64>>,
) -> AdminUserPayload {
    let mut group_ids = group_ids_by_user.get(&user.id).cloned().unwrap_or_default();
    group_ids.sort_by_key(|group_id| {
        groups_by_id
            .get(group_id)
            .map(|group| group.name.to_ascii_lowercase())
            .unwrap_or_default()
    });
    let groups = group_ids
        .into_iter()
        .filter_map(|group_id| groups_by_id.get(&group_id).copied())
        .map(|group| AdminUserGroupPayload {
            id: group.id,
            name: group.name.clone(),
        })
        .collect::<Vec<_>>();
    let group_names = groups
        .iter()
        .map(|group| group.name.clone())
        .collect::<Vec<_>>();

    AdminUserPayload {
        id: user.id,
        issuer: user.issuer.clone(),
        subject: user.subject.clone(),
        email: user.email.clone().unwrap_or_default(),
        name: user.name.clone(),
        is_admin: effective_admin_from_parts(
            auth,
            user.is_admin,
            user.email.as_deref(),
            &group_names,
        ),
        is_active: user.is_active,
        created_at: payload_datetime_iso(user.created_at.as_deref()),
        last_login_at: payload_datetime_iso(user.last_login_at.as_deref()),
        last_seen_at: payload_datetime_iso(user.last_seen_at.as_deref()),
        groups,
    }
}

fn payload_datetime_iso(timestamp: Option<&str>) -> Option<String> {
    let timestamp = timestamp.filter(|value| !value.trim().is_empty())?;
    let trimmed = timestamp.trim();
    if let Ok(parsed) = OffsetDateTime::parse(trimmed, &Rfc3339) {
        return Some(format_python_aware_iso_utc(parsed));
    }
    Some(
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
            .map_or_else(|| trimmed.to_string(), format_python_naive_iso_utc),
    )
}

fn parse_sqlite_timestamp(timestamp: &str, format: &str) -> Option<PrimitiveDateTime> {
    let description = time::format_description::parse_borrowed::<1>(format).ok()?;
    PrimitiveDateTime::parse(timestamp, &description).ok()
}

fn format_python_aware_iso_utc(timestamp: OffsetDateTime) -> String {
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

fn format_python_naive_iso_utc(timestamp: PrimitiveDateTime) -> String {
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

async fn ensure_not_last_active_admin(
    pool: &SqlitePool,
    auth: &AuthSettings,
    target: &UserRow,
) -> Result<(), AdminError> {
    if !target.is_active {
        return Ok(());
    }
    let groups_by_user =
        group_names_by_user_after_group_change(pool, GroupAdminChange::None).await?;
    let group_names = groups_by_user.get(&target.id).cloned().unwrap_or_default();
    if !effective_admin_from_parts(auth, target.is_admin, target.email.as_deref(), &group_names) {
        return Ok(());
    }
    let active_admins = active_admin_user_ids(pool, auth, &groups_by_user).await?;
    if active_admins.len() == 1 && active_admins[0] == target.id {
        return Err(AdminError::LastActiveAdminRequired);
    }
    Ok(())
}

async fn ensure_active_admin_after_group_change(
    pool: &SqlitePool,
    auth: &AuthSettings,
    change: GroupAdminChange,
) -> Result<(), AdminError> {
    let groups_by_user = group_names_by_user_after_group_change(pool, change).await?;
    if active_admin_user_ids(pool, auth, &groups_by_user)
        .await?
        .is_empty()
    {
        return Err(AdminError::LastActiveAdminRequired);
    }
    Ok(())
}

async fn active_admin_user_ids(
    pool: &SqlitePool,
    auth: &AuthSettings,
    groups_by_user: &HashMap<i64, Vec<String>>,
) -> Result<Vec<i64>, AdminError> {
    let users = all_users(pool).await?;
    Ok(users
        .into_iter()
        .filter(|user| user.is_active)
        .filter(|user| {
            effective_admin_from_parts(
                auth,
                user.is_admin,
                user.email.as_deref(),
                groups_by_user
                    .get(&user.id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )
        })
        .map(|user| user.id)
        .collect())
}

#[derive(Debug, Clone)]
enum GroupAdminChange {
    None,
    Delete { group_id: i64 },
    Rename { group_id: i64, new_name: String },
    RemoveMembership { group_id: i64, user_id: i64 },
}

async fn group_names_by_user_after_group_change(
    pool: &SqlitePool,
    change: GroupAdminChange,
) -> Result<HashMap<i64, Vec<String>>, AdminError> {
    let groups = groups_by_id(pool).await?;
    let memberships = all_memberships(pool).await?;
    let mut group_names_by_user: HashMap<i64, Vec<String>> = HashMap::new();
    for membership in memberships {
        if matches!(change, GroupAdminChange::Delete { group_id } if membership.group_id == group_id)
        {
            continue;
        }
        if matches!(
            change,
            GroupAdminChange::RemoveMembership { group_id, user_id }
                if membership.group_id == group_id && membership.user_id == user_id
        ) {
            continue;
        }
        if let GroupAdminChange::Rename { group_id, new_name } = &change
            && membership.group_id == *group_id
        {
            group_names_by_user
                .entry(membership.user_id)
                .or_default()
                .push(new_name.clone());
            continue;
        }
        if let Some(group) = groups.get(&membership.group_id) {
            group_names_by_user
                .entry(membership.user_id)
                .or_default()
                .push(group.name.clone());
        }
    }
    Ok(group_names_by_user)
}

async fn user_by_id(pool: &SqlitePool, user_id: i64) -> Result<Option<UserRow>, AdminError> {
    Ok(
        sqlx::query_as::<_, UserRow>(&format!("{} WHERE id = ?", user_select_sql()))
            .bind(user_id)
            .fetch_optional(pool)
            .await?,
    )
}

async fn all_users(pool: &SqlitePool) -> Result<Vec<UserRow>, AdminError> {
    Ok(sqlx::query_as::<_, UserRow>(user_select_sql())
        .fetch_all(pool)
        .await?)
}

async fn group_by_id(pool: &SqlitePool, group_id: i64) -> Result<Option<GroupRow>, AdminError> {
    Ok(
        sqlx::query_as::<_, GroupRow>(
            "SELECT id, name, description FROM vault_groups WHERE id = ?",
        )
        .bind(group_id)
        .fetch_optional(pool)
        .await?,
    )
}

async fn find_group_by_normalized_name(
    pool: &SqlitePool,
    name: &str,
) -> Result<Option<GroupRow>, AdminError> {
    let lowered = name.to_ascii_lowercase();
    Ok(sqlx::query_as::<_, GroupRow>(
        r"
        SELECT id, name, description
        FROM vault_groups
        WHERE lower(name) = ?
        ",
    )
    .bind(lowered)
    .fetch_optional(pool)
    .await?)
}

async fn groups_by_id(pool: &SqlitePool) -> Result<HashMap<i64, GroupRow>, AdminError> {
    Ok(
        sqlx::query_as::<_, GroupRow>("SELECT id, name, description FROM vault_groups")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|group| (group.id, group))
            .collect(),
    )
}

async fn all_memberships(pool: &SqlitePool) -> Result<Vec<MembershipRow>, AdminError> {
    Ok(sqlx::query_as::<_, MembershipRow>(
        r"
        SELECT user_id, group_id
        FROM vault_group_memberships
        ",
    )
    .fetch_all(pool)
    .await?)
}

async fn record_admin_change(
    pool: &SqlitePool,
    event_type: &str,
    resources: &[&str],
) -> Result<(), AdminError> {
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES (?, ?)
        ",
    )
    .bind(event_type)
    .bind(state_event_resources_json(resources))
    .execute(pool)
    .await?;
    Ok(())
}

fn normalize_group_name(name: &str) -> Result<String, AdminError> {
    let cleaned = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return Err(AdminError::GroupNameRequired);
    }
    if cleaned.contains('/') || cleaned.contains('\\') || cleaned == "." || cleaned == ".." {
        return Err(AdminError::InvalidGroupName);
    }
    Ok(cleaned)
}

fn normalize_optional_description(description: Option<&str>) -> Option<String> {
    description
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(ToString::to_string)
}

fn user_select_sql() -> &'static str {
    r"
    SELECT
        id,
        issuer,
        subject,
        email,
        name,
        is_admin,
        is_active,
        created_at,
        last_login_at,
        last_seen_at
    FROM vault_users
    "
}

fn group_payload(
    group: &GroupRow,
    users_by_id: &HashMap<i64, &UserRow>,
    user_ids_by_group: &HashMap<i64, Vec<i64>>,
) -> AdminGroupPayload {
    let mut user_ids = user_ids_by_group
        .get(&group.id)
        .cloned()
        .unwrap_or_default();
    user_ids.sort_by_key(|user_id| {
        users_by_id
            .get(user_id)
            .map(|user| user.name.to_ascii_lowercase())
            .unwrap_or_default()
    });
    let members = user_ids
        .into_iter()
        .filter_map(|user_id| users_by_id.get(&user_id).copied())
        .map(|user| AdminGroupMemberPayload {
            id: user.id,
            name: user.name.clone(),
            email: user.email.clone().unwrap_or_default(),
        })
        .collect::<Vec<_>>();

    AdminGroupPayload {
        id: group.id,
        name: group.name.clone(),
        description: group.description.clone().unwrap_or_default(),
        members,
    }
}
