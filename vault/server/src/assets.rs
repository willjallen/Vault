use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::http::HeaderMap;
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

use crate::views::InitialStatePayload;

const REQUIRED_ASSETS: [&str; 2] = ["app.js", "styles.css"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticAssetManifest {
    pub app_js: String,
    pub styles_css: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticAsset {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

#[derive(Debug, Error)]
pub enum AssetError {
    #[error("static asset manifest is missing or invalid")]
    InvalidManifest,
    #[error("static asset manifest is missing required entries")]
    MissingManifestEntry,
    #[error("static asset path is invalid")]
    InvalidStaticPath,
    #[error("static asset not found")]
    StaticAssetNotFound,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub async fn load_static_asset_manifest(
    static_dir: &Path,
) -> Result<StaticAssetManifest, AssetError> {
    let manifest_path = static_dir.join("dist").join("manifest.json");
    let raw_manifest = tokio::fs::read_to_string(manifest_path).await?;
    let manifest = serde_json::from_str::<HashMap<String, String>>(&raw_manifest)?;
    for required in REQUIRED_ASSETS {
        if !manifest.contains_key(required) {
            return Err(AssetError::MissingManifestEntry);
        }
    }
    let app_js = validated_manifest_path(&manifest, "app.js")?;
    let styles_css = validated_manifest_path(&manifest, "styles.css")?;
    validate_manifest_asset(static_dir, &app_js).await?;
    validate_manifest_asset(static_dir, &styles_css).await?;
    Ok(StaticAssetManifest { app_js, styles_css })
}

pub async fn validate_static_assets(static_dir: &Path) -> Result<(), AssetError> {
    load_static_asset_manifest(static_dir).await.map(|_| ())
}

pub async fn read_static_asset(static_dir: &Path, path: &str) -> Result<StaticAsset, AssetError> {
    let relative = clean_static_path(path)?;
    let full_path = static_dir.join(&relative);
    let metadata = tokio::fs::metadata(&full_path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::StaticAssetNotFound
        } else {
            error.into()
        }
    })?;
    if !metadata.is_file() {
        return Err(AssetError::StaticAssetNotFound);
    }
    let bytes = tokio::fs::read(&full_path).await?;
    Ok(StaticAsset {
        content_type: content_type_for_path(&full_path),
        bytes,
    })
}

pub fn app_shell_html(
    initial_state: &InitialStatePayload,
    manifest: &StaticAssetManifest,
    headers: &HeaderMap,
    nonce: &str,
) -> Result<String, AssetError> {
    let title = escape_html(&initial_state.bootstrap.site_name);
    let appearance_override = json!({
        "palette": normalize_appearance_header(headers, "x-vault-palette", &["cozy", "winui"]),
        "theme": normalize_appearance_header(headers, "x-vault-theme", &["system", "light", "dark"]),
    });
    let appearance_json = json_for_script(&appearance_override)?;
    let preferences_json = json_for_script(&initial_state.bootstrap.preferences)?;
    let state_json = json_for_script(initial_state)?;
    let nonce = escape_html(nonce);
    Ok(format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>{title}</title>
    <script nonce="{nonce}">
      (() => {{
        const appearanceOverride = {appearance_json};
        const syncedPreferences = {preferences_json};
        const defaultPreferences = {{
          themePreference: "system",
          palettePreference: "cozy",
          openFoldersOnClick: true,
          alternateRows: false,
          doubleClickDownload: false,
        }};
        const allowed = new Set(["system", "light", "dark"]);
        const allowedPalettes = new Set(["cozy", "winui"]);
        const normalizeBoolean = (value, fallback) => {{
          if (value === true || value === false) return value;
          return fallback;
        }};
        const source = syncedPreferences && typeof syncedPreferences === "object" ? syncedPreferences : {{}};
        const preferences = {{
          themePreference: allowed.has(source.themePreference) ? source.themePreference : defaultPreferences.themePreference,
          palettePreference: allowedPalettes.has(source.palettePreference) ? source.palettePreference : defaultPreferences.palettePreference,
          openFoldersOnClick: normalizeBoolean(source.openFoldersOnClick, defaultPreferences.openFoldersOnClick),
          alternateRows: normalizeBoolean(source.alternateRows, defaultPreferences.alternateRows),
          doubleClickDownload: normalizeBoolean(source.doubleClickDownload, defaultPreferences.doubleClickDownload),
        }};
        const hostTheme = appearanceOverride && allowed.has(appearanceOverride.theme) ? appearanceOverride.theme : "";
        const hostPalette = appearanceOverride && allowedPalettes.has(appearanceOverride.palette) ? appearanceOverride.palette : "";
        const prefersDark = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
        const effectiveTheme = hostTheme || preferences.themePreference;
        const resolved = effectiveTheme === "system" ? (prefersDark ? "dark" : "light") : effectiveTheme;
        const effectivePalette = hostPalette || preferences.palettePreference;
        document.documentElement.dataset.themePreference = preferences.themePreference;
        if (hostTheme) document.documentElement.dataset.themeOverride = hostTheme;
        document.documentElement.dataset.theme = resolved;
        document.documentElement.dataset.palettePreference = preferences.palettePreference;
        if (hostPalette) document.documentElement.dataset.paletteOverride = hostPalette;
        document.documentElement.dataset.palette = effectivePalette;
        document.documentElement.dataset.openFoldersOnClick = String(preferences.openFoldersOnClick);
        document.documentElement.dataset.alternateRows = String(preferences.alternateRows);
        document.documentElement.dataset.doubleClickDownload = String(preferences.doubleClickDownload);
        document.documentElement.style.colorScheme = resolved;
      }})();
    </script>
    <link rel="stylesheet" href="{styles_css}" />
  </head>
  <body class="app-root-page">
    <div id="app-root" class="app-root"></div>

    <script nonce="{nonce}">
      window.__INITIAL_STATE__ = {state_json};
    </script>
    <script type="module" src="{app_js}"></script>
  </body>
</html>
"#,
        app_js = manifest.app_js,
        styles_css = manifest.styles_css,
    ))
}

fn validated_manifest_path(
    manifest: &HashMap<String, String>,
    name: &str,
) -> Result<String, AssetError> {
    let value = manifest
        .get(name)
        .ok_or(AssetError::MissingManifestEntry)?
        .trim();
    if !value.starts_with("/static/dist/") || value.contains("://") {
        return Err(AssetError::InvalidManifest);
    }
    Ok(value.to_string())
}

async fn validate_manifest_asset(static_dir: &Path, asset_url: &str) -> Result<(), AssetError> {
    let relative = asset_url
        .strip_prefix("/static/")
        .ok_or(AssetError::InvalidManifest)?;
    let asset_path = static_dir.join(clean_static_path(relative)?);
    let metadata = tokio::fs::metadata(asset_path).await?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(AssetError::InvalidManifest);
    }
    Ok(())
}

fn clean_static_path(path: &str) -> Result<PathBuf, AssetError> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains('\\') {
        return Err(AssetError::InvalidStaticPath);
    }
    let mut relative = PathBuf::new();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.chars().any(char::is_control) {
            return Err(AssetError::InvalidStaticPath);
        }
        relative.push(part);
    }
    Ok(relative)
}

fn normalize_appearance_header(
    headers: &HeaderMap,
    name: &str,
    allowed: &[&str],
) -> Option<String> {
    let value = headers
        .get(name)
        .and_then(|value| value.to_str().ok())?
        .trim()
        .to_ascii_lowercase();
    allowed.contains(&value.as_str()).then_some(value)
}

fn json_for_script<T: Serialize>(value: &T) -> Result<String, AssetError> {
    Ok(serde_json::to_string(value)?
        .replace('<', "\\u003C")
        .replace('>', "\\u003E")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn content_type_for_path(path: &Path) -> String {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
    .to_string()
}
