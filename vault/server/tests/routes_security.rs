use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use flate2::read::GzDecoder;
use std::io::Read;
use tower::ServiceExt;
use vault_server::auth::{AuthSettings, SecurityHeaderSettings};
use vault_server::config::Config;
use vault_server::db;
use vault_server::http::{self, AppState};
use vault_server::storage::LocalBlobStorage;

async fn test_state(auth: AuthSettings) -> (AppState, tempfile::TempDir) {
    test_state_with_gzip(auth, 1024, 6).await
}

async fn test_state_with_gzip(
    auth: AuthSettings,
    gzip_minimum_size: i64,
    gzip_compresslevel: i64,
) -> (AppState, tempfile::TempDir) {
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
        gzip_minimum_size,
        gzip_compresslevel,
    };
    let db = db::connect(&config.db_path()).await.expect("db");
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    let state = AppState::new(config, auth, db, Arc::new(storage));
    (state, temp_dir)
}

fn static_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vault/client")
}

fn request(method: Method, uri: &str) -> Request<Body> {
    request_with_headers(method, uri, &[])
}

fn request_with_headers(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("request")
}

fn authed_get(uri: &str) -> Request<Body> {
    authed_get_with_headers(uri, &[])
}

fn authed_get_with_headers(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("Remote-User", "admin")
        .header("Remote-Name", "Admin")
        .header("Remote-Email", "admin@example.com")
        .header("Remote-Groups", "vault-admin");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("request")
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

fn gzip_decode(bytes: &[u8]) -> String {
    let mut decoder = GzDecoder::new(bytes);
    let mut body = String::new();
    decoder.read_to_string(&mut body).expect("gzip body");
    body
}

#[tokio::test]
async fn security_headers_are_applied_to_health_and_hsts_follows_https_public_url() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let app = http::router(state);

    let response = app
        .oneshot(request(Method::GET, "/health"))
        .await
        .expect("health");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    assert_eq!(
        response.headers()["permissions-policy"],
        "camera=(), microphone=(), geolocation=(), payment=(), usb=()"
    );
    let csp = response
        .headers()
        .get("content-security-policy")
        .expect("csp")
        .to_str()
        .expect("csp str");
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(csp.contains("script-src 'self' 'nonce-"));
    assert!(!csp.contains("unpkg.com"));
    assert!(!csp.contains("esm.sh"));
    assert!(
        !response
            .headers()
            .contains_key(header::STRICT_TRANSPORT_SECURITY)
    );

    let (state, _temp_dir) = test_state(AuthSettings {
        public_url: "https://vault.example.com".to_string(),
        ..AuthSettings::default()
    })
    .await;
    let secure_response = http::router(state)
        .oneshot(request(Method::GET, "/health"))
        .await
        .expect("secure health");
    let hsts = secure_response
        .headers()
        .get(header::STRICT_TRANSPORT_SECURITY)
        .expect("hsts")
        .to_str()
        .expect("hsts str");
    assert!(hsts.contains("max-age=31536000"));

    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let forwarded_secure_response = http::router(state)
        .oneshot(request_with_headers(
            Method::GET,
            "/health",
            &[("X-Forwarded-Proto", "https, http")],
        ))
        .await
        .expect("forwarded secure health");
    assert!(
        forwarded_secure_response
            .headers()
            .contains_key(header::STRICT_TRANSPORT_SECURITY)
    );
}

#[tokio::test]
async fn app_shell_script_nonce_matches_content_security_policy() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let app = http::router(state);

    let response = app.oneshot(authed_get("/")).await.expect("index");
    assert_eq!(response.status(), StatusCode::OK);
    let csp = response
        .headers()
        .get("content-security-policy")
        .expect("csp")
        .to_str()
        .expect("csp str")
        .to_string();
    let body = response_text(response).await;
    let nonce = body
        .split("<script nonce=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("script nonce");

    assert!(csp.contains(&format!("script-src 'self' 'nonce-{nonce}'")));
    assert!(body.matches(&format!("nonce=\"{nonce}\"")).count() >= 2);
}

#[tokio::test]
async fn security_headers_can_be_disabled_and_csp_can_be_overridden() {
    let (state, _temp_dir) = test_state(AuthSettings {
        security_headers: SecurityHeaderSettings {
            enabled: false,
            ..SecurityHeaderSettings::default()
        },
        ..AuthSettings::default()
    })
    .await;
    let response = http::router(state)
        .oneshot(request(Method::GET, "/health"))
        .await
        .expect("disabled health");
    assert!(!response.headers().contains_key("content-security-policy"));
    assert!(!response.headers().contains_key("x-frame-options"));

    let (state, _temp_dir) = test_state(AuthSettings {
        security_headers: SecurityHeaderSettings {
            content_security_policy: "default-src 'none'; script-src 'nonce-{nonce}'".to_string(),
            ..SecurityHeaderSettings::default()
        },
        ..AuthSettings::default()
    })
    .await;
    let response = http::router(state)
        .oneshot(request(Method::GET, "/health"))
        .await
        .expect("custom csp health");
    let csp = response
        .headers()
        .get("content-security-policy")
        .expect("csp")
        .to_str()
        .expect("csp str");
    assert!(csp.starts_with("default-src 'none'; script-src 'nonce-"));
    assert!(!csp.contains("{nonce}"));
}

#[tokio::test]
async fn gzip_runtime_config_matches_python_middleware_behavior() {
    let (state, _temp_dir) = test_state_with_gzip(AuthSettings::default(), 1, 6).await;
    let response = http::router(state)
        .oneshot(request_with_headers(
            Method::GET,
            "/health",
            &[("Accept-Encoding", "gzip")],
        ))
        .await
        .expect("gzip health");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert_eq!(response.headers()["vary"], "accept-encoding");
    let compressed = response_bytes(response).await;
    assert_eq!(gzip_decode(&compressed), "ok");

    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let below_minimum = http::router(state)
        .oneshot(request_with_headers(
            Method::GET,
            "/health",
            &[("Accept-Encoding", "gzip")],
        ))
        .await
        .expect("below minimum health");
    assert!(
        !below_minimum
            .headers()
            .contains_key(header::CONTENT_ENCODING)
    );
    assert_eq!(response_text(below_minimum).await, "ok");

    let (state, _temp_dir) = test_state_with_gzip(AuthSettings::default(), 0, 6).await;
    let disabled = http::router(state)
        .oneshot(request_with_headers(
            Method::GET,
            "/health",
            &[("Accept-Encoding", "gzip")],
        ))
        .await
        .expect("disabled gzip health");
    assert!(!disabled.headers().contains_key(header::CONTENT_ENCODING));
    assert_eq!(response_text(disabled).await, "ok");
}
