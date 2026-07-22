use serde_json::{Map, Value, json};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::auth::UserContext;
use crate::folders::normalize_folder;
use crate::state_events::record_state_event_in_tx;

const SIDEBAR_SECTION_KEYS: [&str; 4] = ["folders", "favorites", "editing", "archive"];
const MIN_SIDEBAR_SECTION_SIZE: i64 = 32;
const MAX_SIDEBAR_SECTION_SIZE: i64 = 4000;
const MIN_SIDEBAR_SECTION_SIZE_F64: f64 = 32.0;
const MAX_SIDEBAR_SECTION_SIZE_F64: f64 = 4000.0;
const CONTENTS_VIEW_VERSION: i64 = 1;
const DEFAULT_CONTENTS_ICON_SIZE: i64 = 80;
const MIN_CONTENTS_ICON_SIZE: i64 = 56;
const MAX_CONTENTS_ICON_SIZE: i64 = 176;
const MIN_CONTENTS_ICON_SIZE_F64: f64 = 56.0;
const MAX_CONTENTS_ICON_SIZE_F64: f64 = 176.0;

#[derive(Debug, Error)]
pub enum PreferenceError {
    #[error("User preferences require a vault user")]
    UserPreferencesRequireVaultUser,
    #[error("Vault user not found")]
    VaultUserNotFound,
    #[error("{0}")]
    InvalidPatch(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub async fn preferences_for_user(
    pool: &SqlitePool,
    user: &UserContext,
) -> Result<Value, PreferenceError> {
    if user.vault_user_id <= 0 {
        return Ok(normalize_user_preferences(&Value::Object(Map::new())));
    }
    let raw = sqlx::query_scalar::<_, String>("SELECT preferences FROM vault_users WHERE id = ?")
        .bind(user.vault_user_id)
        .fetch_optional(pool)
        .await?;
    let parsed = match raw {
        Some(raw) if !raw.trim().is_empty() => serde_json::from_str::<Value>(&raw)?,
        _ => Value::Object(Map::new()),
    };
    Ok(normalize_user_preferences(&parsed))
}

pub async fn update_preferences_for_user(
    pool: &SqlitePool,
    user: &UserContext,
    raw_patch: &Value,
) -> Result<(Value, bool), PreferenceError> {
    if user.vault_user_id <= 0 {
        return Err(PreferenceError::UserPreferencesRequireVaultUser);
    }
    let patch = clean_user_preference_patch(raw_patch)?;
    let mut transaction = pool.begin().await?;
    let raw = sqlx::query_scalar::<_, String>("SELECT preferences FROM vault_users WHERE id = ?")
        .bind(user.vault_user_id)
        .fetch_optional(&mut *transaction)
        .await?;
    let Some(raw) = raw else {
        return Err(PreferenceError::VaultUserNotFound);
    };
    let existing = if raw.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str::<Value>(&raw)?
    };
    let existing = normalize_user_preferences(&existing);
    let merged = merge_user_preferences(&existing, patch);
    let changed = merged != existing;
    sqlx::query("UPDATE vault_users SET preferences = ? WHERE id = ?")
        .bind(merged.to_string())
        .bind(user.vault_user_id)
        .execute(&mut *transaction)
        .await?;
    if changed {
        record_state_event_in_tx(&mut transaction, "preferences.update", &["preferences"]).await?;
    }
    transaction.commit().await?;
    Ok((merged, changed))
}

#[must_use]
pub fn normalize_user_preferences(raw: &Value) -> Value {
    let mut normalized = default_preferences();
    let Some(raw_object) = raw.as_object() else {
        return Value::Object(normalized);
    };

    if let Some(theme) = raw_object
        .get("themePreference")
        .and_then(Value::as_str)
        .filter(|theme| matches!(*theme, "system" | "light" | "dark"))
    {
        normalized.insert("themePreference".to_string(), json!(theme));
    }
    if let Some(palette) = raw_object
        .get("palettePreference")
        .and_then(Value::as_str)
        .filter(|palette| matches!(*palette, "cozy" | "winui"))
    {
        normalized.insert("palettePreference".to_string(), json!(palette));
    }
    for key in [
        "openFoldersOnClick",
        "alternateRows",
        "doubleClickDownload",
        "downloadLocationGuidanceDismissed",
    ] {
        if let Some(value) = raw_object.get(key).and_then(Value::as_bool) {
            normalized.insert(key.to_string(), json!(value));
        }
    }
    if let Some(version) = raw_object
        .get("whatsNewAcknowledgedVersion")
        .and_then(Value::as_str)
        .filter(|version| is_valid_acknowledged_version(version))
    {
        normalized.insert("whatsNewAcknowledgedVersion".to_string(), json!(version));
    }
    normalized.insert(
        "favoriteItems".to_string(),
        clean_favorite_items(raw_object.get("favoriteItems")),
    );
    normalized.insert(
        "sidebarSectionSizes".to_string(),
        clean_sidebar_section_sizes(raw_object.get("sidebarSectionSizes")),
    );
    normalized.insert(
        "sidebarSectionCollapsed".to_string(),
        clean_sidebar_section_collapsed(raw_object.get("sidebarSectionCollapsed")),
    );
    normalized.insert(
        "contentsViewByFolder".to_string(),
        clean_contents_view_by_folder(raw_object.get("contentsViewByFolder")),
    );

    Value::Object(normalized)
}

pub fn clean_user_preference_patch(raw: &Value) -> Result<Map<String, Value>, PreferenceError> {
    let Some(raw_object) = raw.as_object() else {
        return Err(invalid_patch("Preferences must be an object"));
    };
    let mut cleaned = Map::new();
    for (key, value) in raw_object {
        match key.as_str() {
            "themePreference" => {
                let Some(theme) = value
                    .as_str()
                    .filter(|theme| matches!(*theme, "system" | "light" | "dark"))
                else {
                    return Err(invalid_patch("Invalid theme preference"));
                };
                cleaned.insert(key.clone(), json!(theme));
            }
            "palettePreference" => {
                let Some(palette) = value
                    .as_str()
                    .filter(|palette| matches!(*palette, "cozy" | "winui"))
                else {
                    return Err(invalid_patch("Invalid palette preference"));
                };
                cleaned.insert(key.clone(), json!(palette));
            }
            "openFoldersOnClick"
            | "alternateRows"
            | "doubleClickDownload"
            | "downloadLocationGuidanceDismissed" => {
                let Some(value) = value.as_bool() else {
                    return Err(invalid_patch(format!("{key} must be a boolean")));
                };
                cleaned.insert(key.clone(), json!(value));
            }
            "whatsNewAcknowledgedVersion" => {
                let Some(version) = value
                    .as_str()
                    .filter(|version| is_valid_acknowledged_version(version))
                else {
                    return Err(invalid_patch(
                        "whatsNewAcknowledgedVersion must be a valid version",
                    ));
                };
                cleaned.insert(key.clone(), json!(version));
            }
            "favoriteItems" => {
                cleaned.insert(key.clone(), clean_favorite_items_strict(value)?);
            }
            "sidebarSectionSizes" => {
                cleaned.insert(key.clone(), clean_sidebar_section_sizes_strict(value)?);
            }
            "sidebarSectionCollapsed" => {
                cleaned.insert(key.clone(), clean_sidebar_section_collapsed_strict(value)?);
            }
            "contentsViewByFolder" => {
                cleaned.insert(key.clone(), clean_contents_view_by_folder_strict(value)?);
            }
            _ => return Err(invalid_patch(format!("Unknown preference: {key}"))),
        }
    }
    Ok(cleaned)
}

#[must_use]
pub fn merge_user_preferences(existing: &Value, patch: Map<String, Value>) -> Value {
    let mut merged = normalize_user_preferences(existing);
    let mut patch = patch;
    let contents_view_patch = patch.remove("contentsViewByFolder");
    if let Some(object) = merged.as_object_mut() {
        object.extend(patch);
        if let Some(Value::Object(entries)) = contents_view_patch {
            let target = object
                .entry("contentsViewByFolder".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(target_entries) = target.as_object_mut() {
                target_entries.extend(entries);
            }
        }
    }
    normalize_user_preferences(&merged)
}

fn default_preferences() -> Map<String, Value> {
    Map::from_iter([
        ("themePreference".to_string(), json!("system")),
        ("palettePreference".to_string(), json!("cozy")),
        ("openFoldersOnClick".to_string(), json!(true)),
        ("alternateRows".to_string(), json!(false)),
        ("doubleClickDownload".to_string(), json!(false)),
        (
            "downloadLocationGuidanceDismissed".to_string(),
            json!(false),
        ),
        ("whatsNewAcknowledgedVersion".to_string(), json!("")),
        ("favoriteItems".to_string(), json!([])),
        (
            "sidebarSectionSizes".to_string(),
            clean_sidebar_section_sizes(None),
        ),
        (
            "sidebarSectionCollapsed".to_string(),
            clean_sidebar_section_collapsed(None),
        ),
        ("contentsViewByFolder".to_string(), json!({})),
    ])
}

fn is_valid_acknowledged_version(version: &str) -> bool {
    if version.is_empty() {
        return true;
    }
    version.len() <= 64
        && version.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

fn clean_contents_view_by_folder(raw: Option<&Value>) -> Value {
    let Some(entries) = raw.and_then(Value::as_object) else {
        return json!({});
    };
    Value::Object(
        entries
            .iter()
            .filter_map(|(path, view)| {
                let normalized_path = normalize_folder(Some(path)).ok()?;
                if normalized_path != *path {
                    return None;
                }
                clean_contents_view(view).map(|view| (path.clone(), view))
            })
            .collect(),
    )
}

fn clean_contents_view_by_folder_strict(raw: &Value) -> Result<Value, PreferenceError> {
    let Some(entries) = raw.as_object() else {
        return Err(invalid_patch("contentsViewByFolder must be an object"));
    };
    let mut cleaned = Map::new();
    for (path, view) in entries {
        let normalized_path = normalize_folder(Some(path))
            .map_err(|_| invalid_patch("Contents view folder path is invalid"))?;
        if normalized_path != *path {
            return Err(invalid_patch("Contents view folder path must be canonical"));
        }
        cleaned.insert(path.clone(), clean_contents_view_strict(view)?);
    }
    Ok(Value::Object(cleaned))
}

fn clean_contents_view(raw: &Value) -> Option<Value> {
    let view = raw.as_object()?;
    let mode = view
        .get("mode")
        .and_then(Value::as_str)
        .filter(|mode| matches!(*mode, "details" | "list" | "icons"))
        .unwrap_or("details");
    let icon_size = view
        .get("iconSize")
        .and_then(contents_icon_size_value)
        .unwrap_or(DEFAULT_CONTENTS_ICON_SIZE);
    Some(json!({
        "mode": mode,
        "iconSize": icon_size,
        "version": CONTENTS_VIEW_VERSION,
    }))
}

fn clean_contents_view_strict(raw: &Value) -> Result<Value, PreferenceError> {
    let Some(view) = raw.as_object() else {
        return Err(invalid_patch("Contents views must be objects"));
    };
    for key in view.keys() {
        if !matches!(key.as_str(), "mode" | "iconSize" | "version") {
            return Err(invalid_patch(format!("Unknown contents view field: {key}")));
        }
    }
    let Some(mode) = view
        .get("mode")
        .and_then(Value::as_str)
        .filter(|mode| matches!(*mode, "details" | "list" | "icons"))
    else {
        return Err(invalid_patch("Invalid contents view mode"));
    };
    let Some(icon_size) = view.get("iconSize").and_then(contents_icon_size_value) else {
        return Err(invalid_patch("Contents view iconSize must be numeric"));
    };
    Ok(json!({
        "mode": mode,
        "iconSize": icon_size,
        "version": CONTENTS_VIEW_VERSION,
    }))
}

fn contents_icon_size_value(raw: &Value) -> Option<i64> {
    if let Some(value) = raw.as_i64() {
        return Some(value.clamp(MIN_CONTENTS_ICON_SIZE, MAX_CONTENTS_ICON_SIZE));
    }
    if let Some(value) = raw.as_u64() {
        return Some(
            i64::try_from(value)
                .unwrap_or(i64::MAX)
                .clamp(MIN_CONTENTS_ICON_SIZE, MAX_CONTENTS_ICON_SIZE),
        );
    }
    let rounded = raw
        .as_f64()?
        .round()
        .clamp(MIN_CONTENTS_ICON_SIZE_F64, MAX_CONTENTS_ICON_SIZE_F64);
    format!("{rounded:.0}").parse::<i64>().ok()
}

fn clean_favorite_items(raw: Option<&Value>) -> Value {
    let Some(items) = raw.and_then(Value::as_array) else {
        return json!([]);
    };
    let mut cleaned = Vec::new();
    let mut seen = Vec::new();
    for item in items {
        let Some(item_object) = item.as_object() else {
            continue;
        };
        let Some(item_type) = item_object.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(item_type, "folder" | "document") {
            continue;
        }
        let Some(item_id) = item_object.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if item_id < 1 {
            continue;
        }
        let key = format!("{item_type}:{item_id}");
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        cleaned.push(json!({"type": item_type, "id": item_id}));
    }
    Value::Array(cleaned)
}

fn clean_favorite_items_strict(raw: &Value) -> Result<Value, PreferenceError> {
    let Some(items) = raw.as_array() else {
        return Err(invalid_patch("favoriteItems must be a list"));
    };
    let mut cleaned = Vec::new();
    let mut seen = Vec::new();
    for item in items {
        let cleaned_item = clean_favorite_item_strict(item)?;
        let item_type = cleaned_item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let item_id = cleaned_item
            .get("id")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let key = format!("{item_type}:{item_id}");
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        cleaned.push(cleaned_item);
    }
    Ok(Value::Array(cleaned))
}

fn clean_favorite_item_strict(raw: &Value) -> Result<Value, PreferenceError> {
    let Some(item) = raw.as_object() else {
        return Err(invalid_patch("Favorite items must be objects"));
    };
    match item.get("type").and_then(Value::as_str) {
        Some("folder") => {
            let id = clean_favorite_id(item.get("id"), "Favorite folder")?;
            Ok(json!({"type": "folder", "id": id}))
        }
        Some("document") => {
            let id = clean_favorite_id(item.get("id"), "Favorite document")?;
            Ok(json!({"type": "document", "id": id}))
        }
        _ => Err(invalid_patch(
            "Favorite item type must be folder or document",
        )),
    }
}

fn clean_favorite_id(raw: Option<&Value>, label: &str) -> Result<i64, PreferenceError> {
    let Some(id) = raw.and_then(Value::as_i64) else {
        return Err(invalid_patch(format!("{label} id must be an integer")));
    };
    if id < 1 {
        return Err(invalid_patch(format!("{label} id must be positive")));
    }
    Ok(id)
}

fn clean_sidebar_section_sizes(raw: Option<&Value>) -> Value {
    let defaults = [
        ("folders", 180),
        ("favorites", 95),
        ("editing", 90),
        ("archive", 115),
    ];
    let mut sizes = defaults
        .into_iter()
        .map(|(key, value)| (key.to_string(), json!(value)))
        .collect::<Map<_, _>>();
    let Some(raw_object) = raw.and_then(Value::as_object) else {
        return Value::Object(sizes);
    };
    for key in SIDEBAR_SECTION_KEYS {
        let Some(value) = raw_object.get(key).and_then(sidebar_size_value) else {
            continue;
        };
        sizes.insert(
            key.to_string(),
            json!(value.clamp(MIN_SIDEBAR_SECTION_SIZE, MAX_SIDEBAR_SECTION_SIZE)),
        );
    }
    Value::Object(sizes)
}

fn clean_sidebar_section_sizes_strict(raw: &Value) -> Result<Value, PreferenceError> {
    let Some(raw_object) = raw.as_object() else {
        return Err(invalid_patch("sidebarSectionSizes must be an object"));
    };
    for key in raw_object.keys() {
        if !SIDEBAR_SECTION_KEYS.contains(&key.as_str()) {
            return Err(invalid_patch(format!("Unknown sidebar section: {key}")));
        }
    }
    let defaults = [
        ("folders", 180),
        ("favorites", 95),
        ("editing", 90),
        ("archive", 115),
    ];
    let mut sizes = defaults
        .into_iter()
        .map(|(key, value)| (key.to_string(), json!(value)))
        .collect::<Map<_, _>>();
    for key in SIDEBAR_SECTION_KEYS {
        let Some(raw_value) = raw_object.get(key) else {
            continue;
        };
        let Some(value) = sidebar_size_value(raw_value) else {
            return Err(invalid_patch(format!(
                "{key} sidebar section size must be numeric"
            )));
        };
        sizes.insert(key.to_string(), json!(value));
    }
    Ok(Value::Object(sizes))
}

fn sidebar_size_value(raw: &Value) -> Option<i64> {
    if let Some(value) = raw.as_i64() {
        return Some(value.clamp(MIN_SIDEBAR_SECTION_SIZE, MAX_SIDEBAR_SECTION_SIZE));
    }
    if let Some(value) = raw.as_u64() {
        return Some(
            i64::try_from(value)
                .unwrap_or(i64::MAX)
                .clamp(MIN_SIDEBAR_SECTION_SIZE, MAX_SIDEBAR_SECTION_SIZE),
        );
    }
    let rounded = raw
        .as_f64()?
        .round()
        .clamp(MIN_SIDEBAR_SECTION_SIZE_F64, MAX_SIDEBAR_SECTION_SIZE_F64);
    format!("{rounded:.0}").parse::<i64>().ok()
}

fn clean_sidebar_section_collapsed(raw: Option<&Value>) -> Value {
    let mut collapsed = Map::from_iter([
        ("folders".to_string(), json!(false)),
        ("favorites".to_string(), json!(false)),
        ("editing".to_string(), json!(false)),
        ("archive".to_string(), json!(true)),
    ]);
    let Some(raw_object) = raw.and_then(Value::as_object) else {
        return Value::Object(collapsed);
    };
    for key in SIDEBAR_SECTION_KEYS {
        let Some(value) = raw_object.get(key).and_then(Value::as_bool) else {
            continue;
        };
        collapsed.insert(key.to_string(), json!(value));
    }
    Value::Object(collapsed)
}

fn clean_sidebar_section_collapsed_strict(raw: &Value) -> Result<Value, PreferenceError> {
    let Some(raw_object) = raw.as_object() else {
        return Err(invalid_patch("sidebarSectionCollapsed must be an object"));
    };
    for key in raw_object.keys() {
        if !SIDEBAR_SECTION_KEYS.contains(&key.as_str()) {
            return Err(invalid_patch(format!("Unknown sidebar section: {key}")));
        }
    }
    let mut collapsed = Map::from_iter([
        ("folders".to_string(), json!(false)),
        ("favorites".to_string(), json!(false)),
        ("editing".to_string(), json!(false)),
        ("archive".to_string(), json!(true)),
    ]);
    for key in SIDEBAR_SECTION_KEYS {
        let Some(raw_value) = raw_object.get(key) else {
            continue;
        };
        let Some(value) = raw_value.as_bool() else {
            return Err(invalid_patch(format!(
                "{key} sidebar collapsed state must be a boolean"
            )));
        };
        collapsed.insert(key.to_string(), json!(value));
    }
    Ok(Value::Object(collapsed))
}

fn invalid_patch(message: impl Into<String>) -> PreferenceError {
    PreferenceError::InvalidPatch(message.into())
}
