use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;
use vault_server::auth::{AuthMode, AuthSettings};
use vault_server::config::Config;
use vault_server::db;
use vault_server::http::{self, AppState};
use vault_server::storage::LocalBlobStorage;

async fn test_state(auth: AuthSettings) -> (AppState, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        host: "127.0.0.1".parse().expect("host"),
        port: 0,
        data_dir: temp_dir.path().to_path_buf(),
        db_path: Some(temp_dir.path().join("vault.db")),
        objects_path: None,
        transfers_path: None,
        static_dir: "app/static".into(),
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
    let state = AppState::new(config, auth, db, Arc::new(storage));
    (state, temp_dir)
}

fn dev_auth() -> AuthSettings {
    AuthSettings {
        mode: AuthMode::Dev,
        dev_mode: true,
        dev_auth_enabled: true,
        base_domain: "localhost".to_string(),
        ..AuthSettings::default()
    }
}

fn admin_json(uri: &str, payload: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("Remote-User", "admin")
        .header("Remote-Name", "Admin")
        .header("Remote-Email", "admin@example.com")
        .header("Remote-Groups", "vault-admin")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(payload).expect("json payload"),
        ))
        .expect("request")
}

fn dev_post(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn dev_json(uri: &str, payload: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(payload).expect("json payload"),
        ))
        .expect("request")
}

fn dev_get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

#[tokio::test]
async fn debug_tools_are_hidden_outside_dev_mode() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let app = http::router(state);

    let denied = app
        .clone()
        .oneshot(admin_json(
            "/api/admin/debug/error",
            &json!({"kind": "server"}),
        ))
        .await
        .expect("debug denied");
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);

    let timeout_denied = app
        .oneshot(admin_json("/api/admin/debug/timeout", &json!({})))
        .await
        .expect("timeout denied");
    assert_eq!(timeout_denied.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dev_mode_exposes_debug_error_and_timeout_tools() {
    let (state, _temp_dir) = test_state(dev_auth()).await;
    let app = http::router(state);

    let server_error = app
        .clone()
        .oneshot(dev_json(
            "/api/admin/debug/error",
            &json!({"kind": "server"}),
        ))
        .await
        .expect("server error");
    assert_eq!(server_error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response_json(server_error).await["detail"],
        "Debug server error"
    );

    let bad_request = app
        .clone()
        .oneshot(dev_json(
            "/api/admin/debug/error",
            &json!({"kind": "bad-request"}),
        ))
        .await
        .expect("bad request");
    assert_eq!(bad_request.status(), StatusCode::BAD_REQUEST);

    let timeout = app
        .oneshot(dev_post("/api/admin/debug/timeout"))
        .await
        .expect("timeout");
    assert_eq!(timeout.status(), StatusCode::OK);
    let timeout = response_json(timeout).await;
    assert_eq!(timeout["action"], "timeout");
    assert_eq!(timeout["seconds"], 10);
    assert_eq!(timeout["stream_retry_ms"], 10000);
    assert_eq!(timeout["dev_mode"], true);
    assert_eq!(timeout["ok"], true);
}

#[tokio::test]
async fn dev_debug_timeout_sends_retry_and_closes_existing_event_stream() {
    let (state, _temp_dir) = test_state(dev_auth()).await;
    let app = http::router(state);
    let stream_response = app
        .clone()
        .oneshot(dev_get("/api/events/stream"))
        .await
        .expect("event stream");
    assert_eq!(stream_response.status(), StatusCode::OK);
    let mut stream = stream_response.into_body().into_data_stream();

    let timeout = app
        .oneshot(dev_post("/api/admin/debug/timeout"))
        .await
        .expect("timeout");
    assert_eq!(timeout.status(), StatusCode::OK);

    let retry = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("retry timeout")
        .expect("retry chunk")
        .expect("retry bytes");
    assert_eq!(
        String::from_utf8(retry.to_vec()).expect("utf8 retry"),
        "retry: 10000\n\n"
    );
    let closed = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("stream close timeout");
    assert!(
        closed.is_none(),
        "debug timeout stream must close after retry"
    );
}

#[tokio::test]
async fn dev_debug_seed_emit_report_sweep_and_reset_work() {
    let (state, _temp_dir) = test_state(dev_auth()).await;
    let db = state.db.clone();
    let app = http::router(state);

    let seeded = app
        .clone()
        .oneshot(dev_post("/api/admin/debug/seed"))
        .await
        .expect("seed");
    assert_eq!(seeded.status(), StatusCode::OK);
    let seeded = response_json(seeded).await;
    assert_eq!(seeded["action"], "seed");
    assert_eq!(seeded["folder"], "Debug Samples");
    let document_id = seeded["document_id"].as_i64().expect("document id");
    let document_row = sqlx::query(
        r"
        SELECT documents.name, folders.name AS folder_name
        FROM documents
        JOIN folders ON folders.id = documents.folder_id
        WHERE documents.id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&db)
    .await
    .expect("seed document");
    assert_eq!(
        document_row.get::<String, _>("folder_name"),
        "Debug Samples"
    );
    assert!(
        document_row
            .get::<String, _>("name")
            .starts_with("debug-sample-")
    );

    let emitted = app
        .clone()
        .oneshot(dev_json(
            "/api/admin/debug/emit-state",
            &json!({"resources": ["contents", "sidebar", "not-real"]}),
        ))
        .await
        .expect("emit state");
    assert_eq!(emitted.status(), StatusCode::OK);
    let emitted = response_json(emitted).await;
    assert_eq!(emitted["resources"], json!(["contents", "sidebar"]));
    let event =
        sqlx::query("SELECT event_type, resources FROM state_events ORDER BY id DESC LIMIT 1")
            .fetch_one(&db)
            .await
            .expect("state event");
    assert_eq!(event.get::<String, _>("event_type"), "debug.refresh");
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&event.get::<String, _>("resources"))
            .expect("resources"),
        vec!["contents".to_string(), "sidebar".to_string()]
    );

    let storage_report = app
        .clone()
        .oneshot(dev_post("/api/admin/debug/storage-report"))
        .await
        .expect("storage report");
    assert_eq!(storage_report.status(), StatusCode::OK);
    let storage_report = response_json(storage_report).await;
    assert_eq!(storage_report["action"], "storage-report");
    assert!(storage_report["report"]["missing_local_keys"].is_array());
    assert!(storage_report["report"]["orphan_blob_ids"].is_array());

    let swept = app
        .clone()
        .oneshot(dev_post("/api/admin/debug/sweep-ttl"))
        .await
        .expect("sweep ttl");
    assert_eq!(swept.status(), StatusCode::OK);
    let swept = response_json(swept).await;
    assert_eq!(swept["action"], "sweep-ttl");
    assert!(swept["result"]["documents"]["archived"].is_array());
    assert!(swept["result"]["transfers"]["expired_uploads"].is_array());

    let reset = app
        .clone()
        .oneshot(dev_post("/api/admin/debug/reset-database"))
        .await
        .expect("reset");
    assert_eq!(reset.status(), StatusCode::OK);
    assert_eq!(response_json(reset).await["reload"], true);
    let remaining_documents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM documents")
        .fetch_one(&db)
        .await
        .expect("document count");
    assert_eq!(remaining_documents, 0);

    let bootstrap = app
        .oneshot(dev_get("/api/bootstrap"))
        .await
        .expect("bootstrap");
    assert_eq!(bootstrap.status(), StatusCode::OK);
    assert_eq!(response_json(bootstrap).await["dev_mode"], true);
}

#[tokio::test]
async fn dev_debug_emit_state_uses_default_resources_when_omitted() {
    let (state, _temp_dir) = test_state(dev_auth()).await;
    let db = state.db.clone();
    let app = http::router(state);

    let emitted = app
        .oneshot(dev_json("/api/admin/debug/emit-state", &json!({})))
        .await
        .expect("emit state");
    assert_eq!(emitted.status(), StatusCode::OK);
    let emitted = response_json(emitted).await;
    assert_eq!(
        emitted["resources"],
        json!(["contents", "sidebar", "my_edits"])
    );

    let event =
        sqlx::query("SELECT event_type, resources FROM state_events ORDER BY id DESC LIMIT 1")
            .fetch_one(&db)
            .await
            .expect("state event");
    assert_eq!(event.get::<String, _>("event_type"), "debug.refresh");
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&event.get::<String, _>("resources"))
            .expect("resources"),
        vec![
            "contents".to_string(),
            "my_edits".to_string(),
            "sidebar".to_string()
        ]
    );
}
