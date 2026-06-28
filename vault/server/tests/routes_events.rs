use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use futures_util::StreamExt;
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::http::{self, AppState};
use vault_server::state_events::{
    notify_state_event_committed, record_state_event, state_events_after,
};
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
        static_dir: "vault/client".into(),
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

async fn insert_state_event(pool: &sqlx::SqlitePool, event_type: &str, resources: &str) -> i64 {
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES (?, ?)
        ",
    )
    .bind(event_type)
    .bind(resources)
    .execute(pool)
    .await
    .expect("state event")
    .last_insert_rowid()
}

async fn first_sse_chunk(response: axum::response::Response) -> String {
    let mut stream = response.into_body().into_data_stream();
    let item = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("sse chunk timeout")
        .expect("sse chunk")
        .expect("sse body");
    String::from_utf8(item.to_vec()).expect("utf8 sse")
}

#[tokio::test]
async fn state_event_writes_store_python_normalized_resources() {
    let (state, _temp_dir) = test_state().await;

    record_state_event(
        &state.db,
        "test.normalized",
        &[" sidebar ", "contents", "sidebar", "", "my_edits"],
    )
    .await
    .expect("record state event");
    record_state_event(&state.db, "test.empty", &["", "  "])
        .await
        .expect("empty state event should no-op");

    let raw_resources: String = sqlx::query_scalar(
        "SELECT resources FROM state_events WHERE event_type = 'test.normalized'",
    )
    .fetch_one(&state.db)
    .await
    .expect("stored resources");
    let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM state_events")
        .fetch_one(&state.db)
        .await
        .expect("event count");
    let records = state_events_after(&state.db, 0)
        .await
        .expect("state events");

    assert_eq!(raw_resources, r#"["contents","my_edits","sidebar"]"#);
    assert_eq!(event_count, 1);
    assert_eq!(
        records[0].payload.resources,
        vec![
            "contents".to_string(),
            "my_edits".to_string(),
            "sidebar".to_string()
        ],
    );
}

fn authed_stream_request_with_last_event_id(last_event_id: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri("/api/events/stream")
        .header("Remote-User", "writer")
        .header("Remote-Name", "Writer")
        .header("Remote-Email", "writer@example.com")
        .header("Remote-Groups", "writers");
    if let Some(last_event_id) = last_event_id {
        builder = builder.header("Last-Event-ID", last_event_id);
    }
    builder.body(Body::empty()).expect("request")
}

fn authed_stream_request(last_event_id: Option<i64>) -> Request<Body> {
    let last_event_id = last_event_id.map(|value| value.to_string());
    authed_stream_request_with_last_event_id(last_event_id.as_deref())
}

#[tokio::test]
async fn event_stream_replays_events_after_last_event_id() {
    let (state, _temp_dir) = test_state().await;
    let event_id = insert_state_event(
        &state.db,
        "test.commit",
        r#"["sidebar", "contents", "contents"]"#,
    )
    .await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_stream_request(Some(0)))
        .await
        .expect("stream response");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(content_type.starts_with("text/event-stream"));
    let chunk = first_sse_chunk(response).await;
    assert!(chunk.contains(&format!("id: {event_id}")), "{chunk}");
    assert!(chunk.contains("event: state"), "{chunk}");
    assert!(
        chunk.contains(r#"data: {"type":"test.commit","resources":["contents","sidebar"]}"#),
        "{chunk}"
    );
}

#[tokio::test]
async fn event_stream_starts_at_latest_without_last_event_id_and_wakes_on_notify() {
    let (state, _temp_dir) = test_state().await;
    insert_state_event(&state.db, "old.commit", r#"["contents"]"#).await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_stream_request(None))
        .await
        .expect("stream response");
    assert_eq!(response.status(), StatusCode::OK);
    let event_id = insert_state_event(&pool, "new.commit", r#"["document_detail"]"#).await;
    notify_state_event_committed();

    let chunk = first_sse_chunk(response).await;
    assert!(chunk.contains(&format!("id: {event_id}")), "{chunk}");
    assert!(
        chunk.contains(r#"data: {"type":"new.commit","resources":["document_detail"]}"#),
        "{chunk}"
    );
    assert!(!chunk.contains("old.commit"), "{chunk}");
}

#[tokio::test]
async fn event_stream_invalid_last_event_id_starts_at_latest_and_wakes_on_notify() {
    let (state, _temp_dir) = test_state().await;
    insert_state_event(&state.db, "old.commit", r#"["contents"]"#).await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_stream_request_with_last_event_id(Some("not-an-id")))
        .await
        .expect("stream response");
    assert_eq!(response.status(), StatusCode::OK);
    let event_id = insert_state_event(&pool, "new.commit", r#"["document_detail"]"#).await;
    notify_state_event_committed();

    let chunk = first_sse_chunk(response).await;
    assert!(chunk.contains(&format!("id: {event_id}")), "{chunk}");
    assert!(
        chunk.contains(r#"data: {"type":"new.commit","resources":["document_detail"]}"#),
        "{chunk}"
    );
    assert!(!chunk.contains("old.commit"), "{chunk}");
}

#[tokio::test]
async fn idle_event_streams_wait_for_notifications_without_blocking_health() {
    let (state, _temp_dir) = test_state().await;
    let pool = state.db.clone();
    let app = http::router(state);
    let mut streams = Vec::new();

    for index in 0..10 {
        let response = app
            .clone()
            .oneshot(authed_stream_request(None))
            .await
            .expect("stream response");
        let status = response.status();
        if status != StatusCode::OK {
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("stream error body");
            panic!(
                "stream {index} open failed with {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        streams.push(response.into_body().into_data_stream());
    }

    for stream in &mut streams {
        assert!(
            tokio::time::timeout(Duration::from_millis(75), stream.next())
                .await
                .is_err(),
            "idle stream emitted before notification",
        );
    }

    let health = tokio::time::timeout(
        Duration::from_millis(250),
        app.clone().oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("health request"),
        ),
    )
    .await
    .expect("health timeout")
    .expect("health response");
    assert_eq!(health.status(), StatusCode::OK);
    let health_body = to_bytes(health.into_body(), usize::MAX)
        .await
        .expect("health body");
    assert_eq!(&health_body[..], b"ok");

    let event_id = insert_state_event(&pool, "notified.commit", r#"["contents"]"#).await;
    notify_state_event_committed();

    for stream in &mut streams {
        let item = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("notified stream timeout")
            .expect("notified stream ended")
            .expect("notified stream chunk");
        let chunk = String::from_utf8(item.to_vec()).expect("utf8 sse");
        assert!(chunk.contains(&format!("id: {event_id}")), "{chunk}");
        assert!(
            chunk.contains(r#"data: {"type":"notified.commit","resources":["contents"]}"#),
            "{chunk}"
        );
    }
}
