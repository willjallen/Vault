use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::sync::oneshot;
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
        export_max_active_jobs: 256,
        export_max_active_jobs_per_user: 16,
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

async fn assert_waiting_on_writer_gate<T>(task: &mut tokio::task::JoinHandle<T>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut *task)
            .await
            .is_err(),
        "retention sweep completed before the competing writer committed",
    );
}

async fn assert_retention_race_state(
    pool: &sqlx::SqlitePool,
    temp_id: i64,
    safe_id: i64,
    renewed_id: i64,
    locked_id: i64,
    moved_id: i64,
) {
    let renewed = sqlx::query_as::<_, (i64, Option<String>, i64)>(
        r"
        SELECT folder_id, expiry_action, datetime(expires_at) > datetime('now')
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(renewed_id)
    .fetch_one(pool)
    .await
    .expect("renewed document remains");
    assert_eq!(renewed, (temp_id, Some("delete".to_string()), 1));
    let locked = sqlx::query_as::<_, (i64, Option<String>, i64)>(
        r"
        SELECT
            d.folder_id,
            d.expiry_action,
            EXISTS(
                SELECT 1
                FROM document_locks l
                WHERE l.document_id = d.id AND l.is_active = 1
            )
        FROM documents d
        WHERE d.id = ?
        ",
    )
    .bind(locked_id)
    .fetch_one(pool)
    .await
    .expect("locked document remains");
    assert_eq!(locked, (temp_id, Some("delete".to_string()), 1));
    let moved = sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
        "SELECT folder_id, expires_at, expiry_action FROM documents WHERE id = ?",
    )
    .bind(moved_id)
    .fetch_one(pool)
    .await
    .expect("moved document remains");
    assert_eq!(moved, (safe_id, None, None));
    let retention_events = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM state_events WHERE event_type = 'retention.expired'",
    )
    .fetch_one(pool)
    .await
    .expect("retention state events");
    assert_eq!(retention_events, 0);
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
            d.archived_at,
            d.archived_origin_path,
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
    assert_eq!(archived.get::<String, _>("root_key"), "vault");
    assert_eq!(archived.get::<i64, _>("folder_id"), project.id);
    assert!(archived.get::<Option<String>, _>("archived_at").is_some());
    assert_eq!(
        archived.get::<Option<String>, _>("archived_origin_path"),
        Some("Project/plan.txt".to_string())
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
        r"
        INSERT INTO upload_sessions
            (
                id, mode, status, document_id, filename, total_size,
                chunk_size, part_count, created_by, user_context, expires_at
            )
        VALUES
            ('retention-checkin', 'checkin', 'active', ?, 'scratch.txt',
             1, 1, 1, 'user', '{}', '2999-01-01T00:00:00Z')
        ",
    )
    .bind(deleted_id)
    .execute(&state.db)
    .await
    .expect("dependent retention upload");
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
    assert_eq!(result.terminated_uploads, vec!["retention-checkin"]);
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

#[tokio::test]
async fn sweep_rechecks_renewed_locked_and_moved_documents_after_writer_gate() {
    let (state, _temp_dir) = test_state(AuthSettings::default()).await;
    let temp = get_or_create_folder_path(&state.db, Some("Temp"))
        .await
        .expect("temp folder");
    let safe = get_or_create_folder_path(&state.db, Some("Safe"))
        .await
        .expect("safe folder");
    let renewed_id = insert_expired_document(&state.db, temp.id, "renewed.txt", "delete").await;
    let locked_id = insert_expired_document(&state.db, temp.id, "locked.txt", "delete").await;
    let moved_id = insert_expired_document(&state.db, temp.id, "moved.txt", "delete").await;

    let mut gate = state
        .db
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("writer gate");
    let sweep_pool = state.db.clone();
    let (started_tx, started_rx) = oneshot::channel();
    let mut sweep = tokio::spawn(async move {
        started_tx.send(()).expect("signal sweep start");
        sweep_expired_documents(&sweep_pool, 250).await
    });
    started_rx.await.expect("sweep started");
    assert_waiting_on_writer_gate(&mut sweep).await;

    sqlx::query("UPDATE documents SET expires_at = datetime('now', '+30 days') WHERE id = ?")
        .bind(renewed_id)
        .execute(&mut *gate)
        .await
        .expect("renew document");
    sqlx::query(
        "INSERT INTO document_locks (document_id, locked_by, is_active) VALUES (?, 'editor', 1)",
    )
    .bind(locked_id)
    .execute(&mut *gate)
    .await
    .expect("lock document");
    sqlx::query(
        r"
        UPDATE documents
        SET folder_id = ?, expires_at = NULL, expiry_action = NULL
        WHERE id = ?
        ",
    )
    .bind(safe.id)
    .bind(moved_id)
    .execute(&mut *gate)
    .await
    .expect("move document out of retention scope");
    gate.commit().await.expect("commit retention changes");

    let result = tokio::time::timeout(Duration::from_secs(5), sweep)
        .await
        .expect("retention sweep timed out")
        .expect("sweep task")
        .expect("retention sweep");
    assert!(result.archived.is_empty());
    assert!(result.deleted.is_empty());
    assert_eq!(result.skipped, vec!["Temp/locked.txt"]);

    assert_retention_race_state(&state.db, temp.id, safe.id, renewed_id, locked_id, moved_id).await;
}
