use serde_json::{Map, Value, json};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::state_events::state_event_resources_json;

#[derive(Debug, Error)]
pub enum SiteSettingsError {
    #[error("{0}")]
    InvalidPatch(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub async fn site_settings_for_db(pool: &SqlitePool) -> Result<Value, SiteSettingsError> {
    let rows = sqlx::query_as::<_, (String, String)>("SELECT key, value FROM vault_settings")
        .fetch_all(pool)
        .await?;
    let mut raw = Map::new();
    for (key, value) in rows {
        raw.insert(key, serde_json::from_str::<Value>(&value)?);
    }
    Ok(normalize_site_settings(&Value::Object(raw)))
}

pub async fn archive_permanent_delete_admin_only(
    pool: &SqlitePool,
) -> Result<bool, SiteSettingsError> {
    Ok(site_settings_for_db(pool)
        .await?
        .get("archivePermanentDeleteAdminOnly")
        .and_then(Value::as_bool)
        .unwrap_or(true))
}

pub async fn update_admin_site_settings(
    pool: &SqlitePool,
    raw_patch: &Value,
) -> Result<Value, SiteSettingsError> {
    let patch = clean_site_setting_patch(raw_patch)?;
    let mut transaction = pool.begin().await?;
    for (key, value) in patch {
        sqlx::query(
            r"
            INSERT INTO vault_settings (key, value, updated_at)
            VALUES (?, ?, CURRENT_TIMESTAMP)
            ON CONFLICT(key)
            DO UPDATE SET
                value = excluded.value,
                updated_at = CURRENT_TIMESTAMP
            ",
        )
        .bind(key)
        .bind(value.to_string())
        .execute(&mut *transaction)
        .await?;
    }
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES ('admin.settings.updated', ?)
        ",
    )
    .bind(state_event_resources_json(&["admin", "settings"]))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    site_settings_for_db(pool).await
}

#[must_use]
pub fn normalize_site_settings(raw: &Value) -> Value {
    let mut normalized =
        Map::from_iter([("archivePermanentDeleteAdminOnly".to_string(), json!(true))]);
    if let Some(value) = raw
        .as_object()
        .and_then(|object| object.get("archivePermanentDeleteAdminOnly"))
        .and_then(Value::as_bool)
    {
        normalized.insert("archivePermanentDeleteAdminOnly".to_string(), json!(value));
    }
    Value::Object(normalized)
}

pub fn clean_site_setting_patch(raw: &Value) -> Result<Map<String, Value>, SiteSettingsError> {
    let Some(raw_object) = raw.as_object() else {
        return Err(invalid_patch("Settings must be an object"));
    };
    let mut cleaned = Map::new();
    for (key, value) in raw_object {
        match key.as_str() {
            "archivePermanentDeleteAdminOnly" => {
                let Some(value) = value.as_bool() else {
                    return Err(invalid_patch(format!("{key} must be a boolean")));
                };
                cleaned.insert(key.clone(), json!(value));
            }
            _ => return Err(invalid_patch(format!("Unknown setting: {key}"))),
        }
    }
    Ok(cleaned)
}

fn invalid_patch(message: impl Into<String>) -> SiteSettingsError {
    SiteSettingsError::InvalidPatch(message.into())
}
