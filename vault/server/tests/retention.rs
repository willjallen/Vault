use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;
use vault_server::auth::{AuthMode, AuthSettings, UserContext};
use vault_server::config::Config;
use vault_server::db;
use vault_server::documents::{ClientMeta, restore_document, sweep_expired_documents};
use vault_server::folders::{
    apply_effective_ttl_to_document_in_tx, folder_path_by_id, get_or_create_folder_path,
};
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
    let state = AppState::new(config, auth, db, Arc::new(storage));
    (state, temp_dir)
}

fn admin_user() -> UserContext {
    UserContext {
        id: "admin".to_string(),
        vault_user_id: 1,
        issuer: "headers".to_string(),
        subject: "admin".to_string(),
        name: "Admin".to_string(),
        email: "admin@example.com".to_string(),
        groups: vec!["vault-admin".to_string()],
        is_admin: true,
    }
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

fn dev_post(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
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

async fn set_folder_ttl(pool: &sqlx::SqlitePool, folder_id: i64, action: &str, days: i64) {
    sqlx::query("UPDATE folders SET default_ttl_days = ?, default_ttl_action = ? WHERE id = ?")
        .bind(days)
        .bind(action)
        .bind(folder_id)
        .execute(pool)
        .await
        .expect("set ttl");
}

async fn insert_expired_document(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    action: &str,
) -> i64 {
    sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, latest_modified_at, expires_at, expiry_action)
        VALUES
            (?, ?, datetime('now', '-31 days'), datetime('now', '-1 day'), ?)
        ",
    )
    .bind(folder_id)
    .bind(name)
    .bind(action)
    .execute(pool)
    .await
    .expect("insert document")
    .last_insert_rowid()
}

async fn insert_document_modified_at(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    latest_modified_at: &str,
) -> i64 {
    sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, latest_modified_at)
        VALUES
            (?, ?, ?)
        ",
    )
    .bind(folder_id)
    .bind(name)
    .bind(latest_modified_at)
    .execute(pool)
    .await
    .expect("insert document")
    .last_insert_rowid()
}

#[tokio::test]
async fn expired_archive_ttl_moves_document_to_flat_archive_and_restore_reapplies_policy() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    set_folder_ttl(&state.db, project.id, "archive", 30).await;
    let document_id = insert_expired_document(&state.db, project.id, "plan.txt", "archive").await;

    let result = sweep_expired_documents(&state.db, 250)
        .await
        .expect("sweep");

    assert_eq!(result.archived, vec!["Archive/plan.txt"]);
    assert!(result.deleted.is_empty());
    assert!(result.skipped.is_empty());

    let archived = sqlx::query(
        r"
        SELECT
            f.root_key,
            d.folder_id,
            d.archived_from_folder,
            d.archived_original_name,
            d.expires_at,
            d.expiry_action
        FROM documents d
        JOIN folders f ON f.id = d.folder_id
        WHERE d.id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&state.db)
    .await
    .expect("archived row");
    assert_eq!(archived.get::<String, _>("root_key"), "archive");
    assert_eq!(
        archived.get::<Option<String>, _>("archived_from_folder"),
        Some("Project".to_string())
    );
    assert_eq!(
        archived.get::<Option<String>, _>("archived_original_name"),
        Some("plan.txt".to_string())
    );
    assert_eq!(archived.get::<Option<String>, _>("expires_at"), None);
    assert_eq!(archived.get::<Option<String>, _>("expiry_action"), None);

    let resources = sqlx::query_scalar::<_, String>(
        "SELECT resources FROM state_events WHERE event_type = 'retention.expired'",
    )
    .fetch_one(&state.db)
    .await
    .expect("state event");
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&resources).expect("resources"),
        vec!["contents", "document_detail", "my_edits", "sidebar"]
    );

    restore_document(
        &state.db,
        document_id,
        &admin_user(),
        &ClientMeta {
            ip: None,
            user_agent: None,
        },
    )
    .await
    .expect("restore document");
    let restored = sqlx::query(
        "SELECT folder_id, expiry_action, datetime(expires_at) > datetime('now', '+29 days') AS future_expiry FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(&state.db)
    .await
    .expect("restored row");
    assert_eq!(
        folder_path_by_id(&state.db, restored.get::<i64, _>("folder_id"))
            .await
            .expect("folder path"),
        "Project",
    );
    assert_eq!(
        restored.get::<Option<String>, _>("expiry_action"),
        Some("archive".to_string())
    );
    assert_eq!(restored.get::<i64, _>("future_expiry"), 1);
}

#[tokio::test]
async fn expired_delete_ttl_deletes_unlocked_documents_and_skips_locked_documents() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let temp = get_or_create_folder_path(&state.db, Some("Temp"))
        .await
        .expect("temp");
    set_folder_ttl(&state.db, temp.id, "delete", 1).await;
    let deleted_id = insert_expired_document(&state.db, temp.id, "scratch.txt", "delete").await;
    let locked_id = insert_expired_document(&state.db, temp.id, "locked.txt", "delete").await;
    sqlx::query(
        "INSERT INTO document_locks (document_id, locked_by, is_active) VALUES (?, 'user', 1)",
    )
    .bind(locked_id)
    .execute(&state.db)
    .await
    .expect("lock");

    let result = sweep_expired_documents(&state.db, 250)
        .await
        .expect("sweep");

    assert_eq!(result.deleted, vec!["Temp/scratch.txt"]);
    assert_eq!(result.skipped, vec!["Temp/locked.txt"]);
    assert!(result.archived.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE id = ?")
            .bind(deleted_id)
            .fetch_one(&state.db)
            .await
            .expect("deleted count"),
        0,
    );
    let locked = sqlx::query("SELECT expires_at, expiry_action FROM documents WHERE id = ?")
        .bind(locked_id)
        .fetch_one(&state.db)
        .await
        .expect("locked row");
    assert!(locked.get::<Option<String>, _>("expires_at").is_some());
    assert_eq!(
        locked.get::<Option<String>, _>("expiry_action"),
        Some("delete".to_string())
    );
}

#[tokio::test]
async fn plain_folders_do_not_expire_old_documents_or_emit_state() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let safe = get_or_create_folder_path(&state.db, Some("Safe"))
        .await
        .expect("safe");
    let document_id = insert_document_modified_at(
        &state.db,
        safe.id,
        "old-but-safe.txt",
        "2025-06-01 00:00:00",
    )
    .await;

    let result = sweep_expired_documents(&state.db, 250)
        .await
        .expect("sweep");

    assert!(result.archived.is_empty());
    assert!(result.deleted.is_empty());
    assert!(result.skipped.is_empty());
    let document = sqlx::query("SELECT expires_at, expiry_action FROM documents WHERE id = ?")
        .bind(document_id)
        .fetch_one(&state.db)
        .await
        .expect("document row");
    assert_eq!(document.get::<Option<String>, _>("expires_at"), None);
    assert_eq!(document.get::<Option<String>, _>("expiry_action"), None);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM state_events WHERE event_type = 'retention.expired'",
        )
        .fetch_one(&state.db)
        .await
        .expect("state event count"),
        0,
    );
}

#[tokio::test]
async fn child_folder_inherits_parent_delete_ttl_without_expiring_plain_siblings() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let temp = get_or_create_folder_path(&state.db, Some("Temp"))
        .await
        .expect("temp");
    set_folder_ttl(&state.db, temp.id, "delete", 1).await;
    let child = get_or_create_folder_path(&state.db, Some("Temp/Keep"))
        .await
        .expect("child");
    let safe = get_or_create_folder_path(&state.db, Some("Safe"))
        .await
        .expect("safe");
    let child_document_id =
        insert_document_modified_at(&state.db, child.id, "child-safe.txt", "2025-06-01 00:00:00")
            .await;
    let safe_document_id = insert_document_modified_at(
        &state.db,
        safe.id,
        "old-but-outside-scope.txt",
        "2025-06-01 00:00:00",
    )
    .await;
    let mut transaction = state.db.begin().await.expect("transaction");
    apply_effective_ttl_to_document_in_tx(&mut transaction, child_document_id, child.id)
        .await
        .expect("apply inherited ttl");
    apply_effective_ttl_to_document_in_tx(&mut transaction, safe_document_id, safe.id)
        .await
        .expect("apply plain ttl");
    transaction.commit().await.expect("commit");

    let child_expiry = sqlx::query(
        r"
        SELECT
            expiry_action,
            datetime(expires_at) <= datetime('now') AS expired
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(child_document_id)
    .fetch_one(&state.db)
    .await
    .expect("child expiry");
    assert_eq!(
        child_expiry.get::<Option<String>, _>("expiry_action"),
        Some("delete".to_string())
    );
    assert_eq!(child_expiry.get::<i64, _>("expired"), 1);
    let safe_expiry = sqlx::query("SELECT expires_at, expiry_action FROM documents WHERE id = ?")
        .bind(safe_document_id)
        .fetch_one(&state.db)
        .await
        .expect("safe expiry");
    assert_eq!(safe_expiry.get::<Option<String>, _>("expires_at"), None);
    assert_eq!(safe_expiry.get::<Option<String>, _>("expiry_action"), None);

    let result = sweep_expired_documents(&state.db, 250)
        .await
        .expect("sweep");

    assert_eq!(result.deleted, vec!["Temp/Keep/child-safe.txt"]);
    assert!(result.archived.is_empty());
    assert!(result.skipped.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE id = ?")
            .bind(child_document_id)
            .fetch_one(&state.db)
            .await
            .expect("child count"),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE id = ?")
            .bind(safe_document_id)
            .fetch_one(&state.db)
            .await
            .expect("safe count"),
        1,
    );
}

#[tokio::test]
async fn debug_sweep_ttl_route_returns_real_document_retention_result() {
    let (state, _temp_dir) = test_state(dev_auth()).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    set_folder_ttl(&state.db, project.id, "archive", 30).await;
    insert_expired_document(&state.db, project.id, "route.txt", "archive").await;
    let app = http::router(state);

    let response = app
        .oneshot(dev_post("/api/admin/debug/sweep-ttl"))
        .await
        .expect("sweep ttl");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;

    assert_eq!(body["action"], "sweep-ttl");
    assert_eq!(
        body["result"]["documents"]["archived"],
        json!(["Archive/route.txt"])
    );
}
