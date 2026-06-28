use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::get_or_create_folder_path;
use vault_server::http::{self, AppState};
use vault_server::storage::LocalBlobStorage;

async fn test_state() -> (AppState, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        host: "127.0.0.1".parse().expect("host"),
        port: 0,
        data_dir: temp_dir.path().to_path_buf(),
        db_path: Some(temp_dir.path().join("vault.db")),
        objects_path: None,
        transfers_path: None,
        static_dir: static_dir(),
        storage_backend: "local".to_string(),
        storage_prefix: String::new(),
        site_name: "Vault".to_string(),
        max_upload_bytes: 5 * 1024 * 1024 * 1024,
        transfer_chunk_bytes: 32 * 1024 * 1024,
        transfer_session_ttl_seconds: 86_400,
        export_ttl_seconds: 86_400,
        export_workers: 1,
        export_zip_compression_threshold_bytes: 3 * 1024 * 1024 * 1024,
        export_zip_compresslevel: 1,
        ttl_sweep_interval_seconds: 60,
        gzip_minimum_size: 1024,
        gzip_compresslevel: 6,
    };
    let db = db::connect(&config.db_path()).await.expect("db");
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    let state = AppState::new(config, AuthSettings::default(), db, Arc::new(storage));
    (state, temp_dir)
}

fn static_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/static")
}

async fn response_text(response: axum::response::Response) -> String {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(body.to_vec()).expect("utf8 body")
}

async fn response_bytes(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec()
}

fn authed_get(uri: &str) -> Request<Body> {
    authed_get_with_appearance(uri, Some("winui"), Some("dark"))
}

fn authed_get_with_appearance(
    uri: &str,
    palette: Option<&str>,
    theme: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("Remote-User", "admin")
        .header("Remote-Name", "Admin")
        .header("Remote-Email", "admin@example.com")
        .header("Remote-Groups", "vault-admin");
    if let Some(palette) = palette {
        builder = builder.header("X-Vault-Palette", palette);
    }
    if let Some(theme) = theme {
        builder = builder.header("X-Vault-Theme", theme);
    }
    builder.body(Body::empty()).expect("request")
}

fn authed_get_without_appearance(uri: &str) -> Request<Body> {
    authed_get_with_appearance(uri, None, None)
}

fn asset_path(manifest: &Value, name: &str) -> String {
    manifest[name].as_str().expect("manifest asset").to_string()
}

#[tokio::test]
async fn index_renders_bootstrap_state_and_manifest_assets() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(static_dir().join("dist/manifest.json")).expect("manifest"),
    )
    .expect("manifest json");
    let app_js = asset_path(&manifest, "app.js");
    let styles_css = asset_path(&manifest, "styles.css");

    let response = app.oneshot(authed_get("/")).await.expect("index");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    let body = response_text(response).await;
    assert!(body.contains(r#"<div id="app-root" class="app-root"></div>"#));
    assert!(body.contains("window.__INITIAL_STATE__ = "));
    assert!(body.contains(r#""bootstrap":{"#));
    assert!(body.contains(r#""current_folder":"""#));
    assert!(body.contains(r#""palette":"winui""#));
    assert!(body.contains(r#""theme":"dark""#));
    assert!(body.contains("dataset.paletteOverride"));
    assert!(body.contains("dataset.themeOverride"));
    assert!(body.contains(&format!(r#"href="{styles_css}""#)));
    assert!(body.contains(&format!(r#"src="{app_js}""#)));
}

#[tokio::test]
async fn index_ignores_folder_query_parameter() {
    let (state, _temp_dir) = test_state().await;
    get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project folder");
    let app = http::router(state);

    let response = app
        .oneshot(authed_get("/?folder=Project"))
        .await
        .expect("index with query");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;

    assert!(body.contains(r#""current_folder":"""#));
    assert!(!body.contains(r#""current_folder":"Project""#));
    assert!(!body.contains("?folder="));
}

#[tokio::test]
async fn index_ignores_invalid_host_appearance_headers() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_get_with_appearance(
            "/",
            Some("purple"),
            Some("solarized"),
        ))
        .await
        .expect("index");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;

    assert!(body.contains(r#""palette":null"#));
    assert!(body.contains(r#""theme":null"#));
    assert!(!body.contains("purple"));
    assert!(!body.contains("solarized"));
}

#[tokio::test]
async fn index_bootstrap_script_keeps_preference_booleans_strict() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let response = app.oneshot(authed_get("/")).await.expect("index");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;

    assert!(body.contains("if (value === true || value === false) return value;"));
    assert!(!body.contains(r#"value === "true""#));
    assert!(!body.contains(r#"value === "false""#));
}

#[tokio::test]
async fn share_entry_renders_app_state_with_share_code_and_rejects_bad_codes() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_get("/s/sharecode_123"))
        .await
        .expect("share entry");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;
    assert!(body.contains(r#""share_code":"sharecode_123""#));
    assert!(!body.contains("?folder="));

    let response = app
        .oneshot(authed_get("/s/not-a-valid-code!"))
        .await
        .expect("bad share entry");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn static_assets_are_served_from_manifest_and_missing_paths_404() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(static_dir().join("dist/manifest.json")).expect("manifest"),
    )
    .expect("manifest json");
    let app_js = asset_path(&manifest, "app.js");

    let response = app
        .clone()
        .oneshot(authed_get_without_appearance(&app_js))
        .await
        .expect("static asset");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/javascript; charset=utf-8"
    );
    assert!(response_bytes(response).await.len() > 1024);

    let missing = app
        .clone()
        .oneshot(authed_get_without_appearance("/static/dist/missing.js"))
        .await
        .expect("missing asset");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let traversal = app
        .oneshot(authed_get_without_appearance("/static/../Cargo.toml"))
        .await
        .expect("invalid asset");
    assert_eq!(traversal.status(), StatusCode::NOT_FOUND);
}
