use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::documents::sweep_expired_documents;
use vault_server::folders::{
    ARCHIVE_ROOT_KEY, VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path,
    get_root_folder,
};
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
    let state = AppState::new(config, AuthSettings::default(), db, Arc::new(storage));
    (state, temp_dir)
}

async fn create_group(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO vault_groups (name) VALUES (?)")
        .bind(name)
        .execute(pool)
        .await
        .expect("create group")
        .last_insert_rowid()
}

async fn grant_writer_roots(pool: &sqlx::SqlitePool) -> (i64, i64, i64) {
    let writers = create_group(pool, "writers").await;
    let root = get_root_folder(pool, VAULT_ROOT_KEY).await.expect("root");
    let archive_root = get_root_folder(pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(pool, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(pool, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    (writers, root.id, archive_root.id)
}

async fn insert_document(pool: &sqlx::SqlitePool, folder_id: i64, name: &str) -> i64 {
    sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, ?, 'admin', 'Admin', 'admin')
        ",
    )
    .bind(folder_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("insert document")
    .last_insert_rowid()
}

async fn insert_document_modified(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    latest_modified_at: &str,
) -> i64 {
    sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by, latest_modified_at)
        VALUES
            (?, ?, 'admin', 'Admin', 'admin', ?)
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

async fn insert_versioned_document_at(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    committed_at: &str,
    committed_by_name: &str,
) -> i64 {
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', ?, 6)
        ",
    )
    .bind(format!("display-{name:0<56}"))
    .execute(pool)
    .await
    .expect("blob")
    .last_insert_rowid();
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, ?, 'admin', 'Admin', 'admin')
        ",
    )
    .bind(folder_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("insert document")
    .last_insert_rowid();
    let version_id = format!("display-version-{document_id}");
    sqlx::query(
        r"
        INSERT INTO document_versions
            (
                id,
                document_id,
                blob_id,
                version_number,
                committed_by,
                committed_by_name,
                message,
                mime_type,
                original_filename,
                created_via,
                committed_at
            )
        VALUES
            (?, ?, ?, 1, 'admin', ?, 'Uploaded display asset', 'text/plain', ?, 'upload', ?)
        ",
    )
    .bind(&version_id)
    .bind(document_id)
    .bind(blob_id)
    .bind(committed_by_name)
    .bind(name)
    .bind(committed_at)
    .execute(pool)
    .await
    .expect("version");
    sqlx::query(
        r"
        UPDATE documents
        SET current_version_id = ?,
            latest_version_number = 1,
            version_count = 1
        WHERE id = ?
        ",
    )
    .bind(version_id)
    .bind(document_id)
    .execute(pool)
    .await
    .expect("current version");
    document_id
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

fn assert_object_keys(value: &Value, expected: &[&str]) {
    let actual = value
        .as_object()
        .expect("json object")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
}

fn authed_get(uri: &str, user: &str, groups: &str) -> Request<Body> {
    authed_request(Method::GET, uri, user, groups, Body::empty())
}

fn authed_form_post(uri: &str, user: &str, groups: &str, body: &str) -> Request<Body> {
    authed_request(
        Method::POST,
        uri,
        user,
        groups,
        Body::from(body.to_string()),
    )
}

fn authed_json_post(uri: &str, user: &str, groups: &str, payload: &Value) -> Request<Body> {
    authed_json_request(Method::POST, uri, user, groups, payload)
}

fn authed_json_patch(uri: &str, user: &str, groups: &str, payload: &Value) -> Request<Body> {
    authed_json_request(Method::PATCH, uri, user, groups, payload)
}

fn authed_json_put(uri: &str, user: &str, groups: &str, payload: &Value) -> Request<Body> {
    authed_json_request(Method::PUT, uri, user, groups, payload)
}

fn authed_json_request(
    method: Method,
    uri: &str,
    user: &str,
    groups: &str,
    payload: &Value,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", user)
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(payload).expect("json payload"),
        ))
        .expect("request")
}

fn authed_request(
    method: Method,
    uri: &str,
    user: &str,
    groups: &str,
    body: Body,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Remote-User", user)
        .header("Remote-Name", user)
        .header("Remote-Email", format!("{user}@example.com"))
        .header("Remote-Groups", groups)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .expect("request")
}

#[tokio::test]
async fn folder_routes_require_authenticated_headers() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/folders/sidebar")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["detail"], "Authentication required");
}

#[tokio::test]
async fn create_folder_requires_authenticated_headers() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/folders")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from("folder=Project"))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["detail"], "Authentication required");
}

#[tokio::test]
async fn create_folder_persists_creator_events_and_state_change() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_form_post(
            "/folders",
            "writer",
            "writers",
            "folder=Project%2FProps",
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;
    let created_id = json["id"].as_i64().expect("created id");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["folder"], "Project/Props");

    let created_row = sqlx::query_as::<_, (String, String)>(
        "SELECT name, created_by_name FROM folders WHERE id = ?",
    )
    .bind(created_id)
    .fetch_one(&pool)
    .await
    .expect("created folder");
    assert_eq!(created_row.0, "Props");
    assert_eq!(created_row.1, "writer");

    let event_row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT event_type, actor_name, message FROM folder_events WHERE folder_id = ?",
    )
    .bind(created_id)
    .fetch_one(&pool)
    .await
    .expect("folder event");
    assert_eq!(event_row.0, "create");
    assert_eq!(event_row.1, "writer");
    assert_eq!(event_row.2, "Created Project/Props");

    let state_row =
        sqlx::query_as::<_, (String, String)>("SELECT event_type, resources FROM state_events")
            .fetch_one(&pool)
            .await
            .expect("state event");
    let resources: Value = serde_json::from_str(&state_row.1).expect("resources json");
    assert_eq!(state_row.0, "folder.created");
    assert_eq!(resources, json!(["contents", "sidebar"]));

    let contents_response = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "writer",
            "writers",
        ))
        .await
        .expect("contents response");
    let contents_json = response_json(contents_response).await;
    assert_eq!(contents_json["folders"][0]["path"], "Project/Props");
}

#[tokio::test]
async fn created_child_folder_inherits_restricted_parent_acl() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let secret = get_or_create_folder_path(&state.db, Some("Secret"))
        .await
        .expect("secret");
    add_folder_permission(&state.db, secret.id, writers, false, false, false)
        .await
        .expect("writer denied secret");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_form_post(
            "/folders",
            "admin",
            "vault-admin",
            "folder=Secret%2FPlans",
        ))
        .await
        .expect("create response");
    let status = response.status();
    let json = response_json(response).await;
    let plans_id = json["id"].as_i64().expect("plans id");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["folder"], "Secret/Plans");

    let child_acl_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folder_permissions WHERE folder_id = ?")
            .bind(plans_id)
            .fetch_one(&pool)
            .await
            .expect("child acl count");
    let writer_contents = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Secret/Plans",
            "writer",
            "writers",
        ))
        .await
        .expect("writer contents");
    let writer_status = writer_contents.status();
    let writer_json = response_json(writer_contents).await;

    assert_eq!(child_acl_count, 0);
    assert_eq!(writer_status, StatusCode::NOT_FOUND);
    assert_eq!(writer_json["detail"], "Folder not found");
}

#[tokio::test]
async fn create_folder_rejects_archive_duplicate_and_missing_write_access() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let readers = create_group(&state.db, "readers").await;
    let hidden = create_group(&state.db, "hidden").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    add_folder_permission(&state.db, root.id, hidden, false, false, false)
        .await
        .expect("hidden root");
    let app = http::router(state);

    let archive_response = app
        .clone()
        .oneshot(authed_form_post(
            "/folders",
            "writer",
            "writers",
            "folder=Archive%2FCold",
        ))
        .await
        .expect("archive response");
    let archive_status = archive_response.status();
    let archive_json = response_json(archive_response).await;
    assert_eq!(archive_status, StatusCode::BAD_REQUEST);
    assert_eq!(archive_json["detail"], "Create folders in Vault");

    let created = app
        .clone()
        .oneshot(authed_form_post(
            "/folders",
            "writer",
            "writers",
            "folder=Project",
        ))
        .await
        .expect("create response");
    assert_eq!(created.status(), StatusCode::OK);

    let duplicate = app
        .clone()
        .oneshot(authed_form_post(
            "/folders",
            "writer",
            "writers",
            "folder=Project",
        ))
        .await
        .expect("duplicate response");
    let duplicate_status = duplicate.status();
    let duplicate_json = response_json(duplicate).await;
    assert_eq!(duplicate_status, StatusCode::BAD_REQUEST);
    assert_eq!(duplicate_json["detail"], "Folder already exists");

    let read_only = app
        .clone()
        .oneshot(authed_form_post(
            "/folders",
            "reader",
            "readers",
            "folder=ReadOnly",
        ))
        .await
        .expect("read only response");
    let read_only_status = read_only.status();
    let read_only_json = response_json(read_only).await;
    assert_eq!(read_only_status, StatusCode::FORBIDDEN);
    assert_eq!(read_only_json["detail"], "Insufficient folder access");

    let invisible = app
        .oneshot(authed_form_post(
            "/folders",
            "outsider",
            "hidden",
            "folder=Invisible",
        ))
        .await
        .expect("invisible response");
    let invisible_status = invisible.status();
    let invisible_json = response_json(invisible).await;
    assert_eq!(invisible_status, StatusCode::NOT_FOUND);
    assert_eq!(invisible_json["detail"], "Folder not found");
}

#[tokio::test]
async fn sidebar_exposes_only_visible_root_children() {
    let (state, _temp_dir) = test_state().await;
    let viewers = create_group(&state.db, "viewers").await;
    let outsiders = create_group(&state.db, "outsiders").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, viewers, true, true, false)
        .await
        .expect("viewer root");
    add_folder_permission(&state.db, root.id, outsiders, true, true, false)
        .await
        .expect("outsider root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let hidden = get_or_create_folder_path(&state.db, Some("Hidden"))
        .await
        .expect("hidden");
    add_folder_permission(&state.db, project.id, viewers, true, false, false)
        .await
        .expect("viewer project");
    add_folder_permission(&state.db, hidden.id, outsiders, true, true, false)
        .await
        .expect("outsider hidden");

    let app = http::router(state);
    let response = app
        .oneshot(authed_get("/api/folders/sidebar", "viewer", "viewers"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["folder_children"][""][0], "Project");
    assert!(
        json["folder_children"][""]
            .as_array()
            .expect("children")
            .len()
            == 1
    );
    assert_eq!(
        json["folder_metadata"]["Project"]["access"]["visible"],
        true
    );
    assert_eq!(json["folder_metadata"]["Project"]["access"]["read"], false);
}

#[tokio::test]
async fn sidebar_root_children_use_python_path_sorting() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_roots(&state.db).await;
    get_or_create_folder_path(&state.db, Some("alpha"))
        .await
        .expect("alpha");
    get_or_create_folder_path(&state.db, Some("Beta"))
        .await
        .expect("Beta");
    let app = http::router(state);

    let response = app
        .oneshot(authed_get("/api/folders/sidebar", "writer", "writers"))
        .await
        .expect("sidebar");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["folder_children"][""], json!(["Beta", "alpha"]));
}

#[tokio::test]
async fn folder_contents_returns_document_access_and_hides_inaccessible_folder() {
    let (state, _temp_dir) = test_state().await;
    let viewers = create_group(&state.db, "viewers").await;
    let outsiders = create_group(&state.db, "outsiders").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, viewers, true, true, false)
        .await
        .expect("viewer root");
    add_folder_permission(&state.db, root.id, outsiders, true, true, false)
        .await
        .expect("outsider root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, viewers, true, false, false)
        .await
        .expect("viewer project");
    let document_id = insert_document(&state.db, project.id, "plan.txt").await;

    let app = http::router(state);
    let viewer_response = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "viewer",
            "viewers",
        ))
        .await
        .expect("viewer response");
    let viewer_status = viewer_response.status();
    let viewer_json = response_json(viewer_response).await;
    let outsider_response = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "outsider",
            "outsiders",
        ))
        .await
        .expect("outsider response");
    let outsider_status = outsider_response.status();
    let outsider_json = response_json(outsider_response).await;

    assert_eq!(viewer_status, StatusCode::OK);
    assert_eq!(viewer_json["folder"], "Project");
    assert_eq!(viewer_json["documents"][0]["id"], document_id);
    assert_eq!(viewer_json["documents"][0]["name"], "plan.txt");
    assert_eq!(viewer_json["documents"][0]["access"]["visible"], true);
    assert_eq!(viewer_json["documents"][0]["access"]["read"], false);
    assert_eq!(viewer_json["documents"][0]["access"]["write"], false);
    assert_eq!(outsider_status, StatusCode::NOT_FOUND);
    assert_eq!(outsider_json["detail"], "Folder not found");
}

#[tokio::test]
async fn folder_contents_rejects_visible_inconsistent_current_version_metadata() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let corrupt = insert_versioned_document_at(
        &state.db,
        project.id,
        "corrupt.txt",
        "2026-06-26T18:00:00Z",
        "Author",
    )
    .await;
    sqlx::query("UPDATE documents SET current_version_id = 'missing-version' WHERE id = ?")
        .bind(corrupt)
        .execute(&state.db)
        .await
        .expect("corrupt current version pointer");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "reader",
            "readers",
        ))
        .await
        .expect("contents response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        json["detail"],
        "Current document version metadata is inconsistent"
    );

    sqlx::query("UPDATE documents SET current_version_id = 'missing-version', version_count = 0 WHERE id = ?")
        .bind(corrupt)
        .execute(&pool)
        .await
        .expect("stale pointer with stale count");

    let response = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "reader",
            "readers",
        ))
        .await
        .expect("contents response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        json["detail"],
        "Current document version metadata is inconsistent"
    );

    sqlx::query("UPDATE documents SET current_version_id = NULL, version_count = 0 WHERE id = ?")
        .bind(corrupt)
        .execute(&pool)
        .await
        .expect("empty pointer with existing version rows");

    let response = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "reader",
            "readers",
        ))
        .await
        .expect("contents response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        json["detail"],
        "Current document version metadata is inconsistent"
    );
}

#[tokio::test]
async fn folder_contents_recursive_scope_only_expands_search_results() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_roots(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let _concept = get_or_create_folder_path(&state.db, Some("Project/Concept"))
        .await
        .expect("concept");
    let refs = get_or_create_folder_path(&state.db, Some("Project/Concept/Refs"))
        .await
        .expect("refs");
    insert_document(&state.db, project.id, "overview.txt").await;
    let nested_doc = insert_document(&state.db, refs.id, "concept-notes.txt").await;
    let app = http::router(state);

    let recursive_without_search = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project&recursive=true",
            "writer",
            "writers",
        ))
        .await
        .expect("recursive without search");
    let plain_json = response_json(recursive_without_search).await;
    assert_eq!(plain_json["recursive"], true);
    assert_eq!(plain_json["q"], "");
    assert_eq!(plain_json["folders"][0]["path"], "Project/Concept");
    assert_eq!(plain_json["folders"].as_array().expect("folders").len(), 1);
    assert_eq!(plain_json["documents"][0]["name"], "overview.txt");
    assert_eq!(
        plain_json["documents"].as_array().expect("documents").len(),
        1
    );

    let recursive_search = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project&q=%20concept%20&recursive=true",
            "writer",
            "writers",
        ))
        .await
        .expect("recursive search");
    let search_json = response_json(recursive_search).await;
    assert_eq!(search_json["recursive"], true);
    assert_eq!(search_json["q"], "concept");
    assert_eq!(
        search_json["folders"]
            .as_array()
            .expect("folders")
            .iter()
            .map(|row| row["path"].as_str().expect("path"))
            .collect::<Vec<_>>(),
        vec!["Project/Concept", "Project/Concept/Refs"],
    );
    assert_eq!(search_json["documents"][0]["id"], nested_doc);
    assert_eq!(
        search_json["documents"][0]["path"],
        "Project/Concept/Refs/concept-notes.txt"
    );
    assert_eq!(
        search_json["documents"]
            .as_array()
            .expect("documents")
            .len(),
        1
    );
}

#[tokio::test]
async fn folder_contents_sort_and_search_use_python_unicode_lowercase() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_roots(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    get_or_create_folder_path(&state.db, Some("Project/Äther"))
        .await
        .expect("upper unicode folder");
    get_or_create_folder_path(&state.db, Some("Project/äardvark"))
        .await
        .expect("lower unicode folder");
    insert_document(&state.db, project.id, "Éclair.txt").await;
    insert_document(&state.db, project.id, "éagle.txt").await;
    let app = http::router(state);

    let plain = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "writer",
            "writers",
        ))
        .await
        .expect("plain contents");
    let plain_json = response_json(plain).await;
    assert_eq!(
        plain_json["folders"]
            .as_array()
            .expect("folders")
            .iter()
            .map(|row| row["name"].as_str().expect("folder name"))
            .collect::<Vec<_>>(),
        vec!["äardvark", "Äther"],
    );
    assert_eq!(
        plain_json["documents"]
            .as_array()
            .expect("documents")
            .iter()
            .map(|row| row["name"].as_str().expect("document name"))
            .collect::<Vec<_>>(),
        vec!["éagle.txt", "Éclair.txt"],
    );

    let folder_search = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project&q=%C3%A4&recursive=true",
            "writer",
            "writers",
        ))
        .await
        .expect("folder search");
    let folder_search_json = response_json(folder_search).await;
    assert_eq!(folder_search_json["q"], "ä");
    assert_eq!(
        folder_search_json["folders"]
            .as_array()
            .expect("folders")
            .iter()
            .map(|row| row["name"].as_str().expect("folder name"))
            .collect::<Vec<_>>(),
        vec!["äardvark", "Äther"],
    );

    let document_search = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project&q=%C3%A9&recursive=true",
            "writer",
            "writers",
        ))
        .await
        .expect("document search");
    let document_search_json = response_json(document_search).await;
    assert_eq!(document_search_json["q"], "é");
    assert_eq!(
        document_search_json["documents"]
            .as_array()
            .expect("documents")
            .iter()
            .map(|row| row["name"].as_str().expect("document name"))
            .collect::<Vec<_>>(),
        vec!["éagle.txt", "Éclair.txt"],
    );
}

#[tokio::test]
async fn folder_contents_and_properties_default_to_vault_root() {
    let (state, _temp_dir) = test_state().await;
    let (_, root_id, _) = grant_writer_roots(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let root_document = insert_versioned_document_at(
        &state.db,
        root_id,
        "root.txt",
        "2026-06-26T18:00:00Z",
        "Root Author",
    )
    .await;
    insert_versioned_document_at(
        &state.db,
        project.id,
        "project.txt",
        "2026-06-26T19:00:00Z",
        "Project Author",
    )
    .await;
    let app = http::router(state);

    let contents = app
        .clone()
        .oneshot(authed_get("/api/folders/contents", "writer", "writers"))
        .await
        .expect("contents");
    let contents_status = contents.status();
    let contents_json = response_json(contents).await;

    assert_eq!(contents_status, StatusCode::OK);
    assert_eq!(contents_json["folder"], "");
    assert_eq!(contents_json["folders"][0]["path"], "Project");
    assert_eq!(contents_json["documents"][0]["id"], root_document);
    assert_eq!(contents_json["documents"][0]["path"], "root.txt");

    let properties = app
        .oneshot(authed_get("/api/folders/properties", "writer", "writers"))
        .await
        .expect("properties");
    let properties_status = properties.status();
    let properties_json = response_json(properties).await;

    assert_eq!(properties_status, StatusCode::OK);
    assert_eq!(properties_json["path"], "");
    assert_eq!(properties_json["name"], "Vault");
    assert_eq!(properties_json["root"], true);
    assert_eq!(
        properties_json["counts"],
        json!({"folders": 1, "documents": 2})
    );
    assert_eq!(properties_json["size_bytes"], 12);
    assert_eq!(properties_json["latest_by"], "Project Author");
}

struct PayloadShapeFixture {
    document_id: i64,
    assets_id: i64,
}

async fn seed_payload_shape_fixture(state: &AppState) -> PayloadShapeFixture {
    grant_writer_roots(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let assets = get_or_create_folder_path(&state.db, Some("Project/Assets"))
        .await
        .expect("assets");
    sqlx::query(
        r"
        UPDATE folders
        SET color = '#445566',
            icon = 'palette',
            default_ttl_days = 14,
            default_ttl_action = 'archive'
        WHERE id = ?
        ",
    )
    .bind(assets.id)
    .execute(&state.db)
    .await
    .expect("folder metadata");
    let document_id = insert_versioned_document_at(
        &state.db,
        project.id,
        "project-overview.txt",
        "2026-06-26T18:00:00Z",
        "Project Author",
    )
    .await;
    insert_versioned_document_at(
        &state.db,
        assets.id,
        "asset.txt",
        "2026-06-26 19:03:00",
        "Asset Author",
    )
    .await;
    PayloadShapeFixture {
        document_id,
        assets_id: assets.id,
    }
}

fn assert_sidebar_payload_shape(sidebar_json: &Value, assets_id: i64) {
    assert_object_keys(sidebar_json, &["folder_children", "folder_metadata"]);
    assert_eq!(sidebar_json["folder_children"][""], json!(["Project"]));
    assert_eq!(sidebar_json["folder_children"]["Archive"], json!([]));
    let metadata = &sidebar_json["folder_metadata"]["Project/Assets"];
    assert_object_keys(
        metadata,
        &[
            "access",
            "color",
            "default_ttl_action",
            "default_ttl_days",
            "effective_ttl_action",
            "effective_ttl_days",
            "effective_ttl_inherited",
            "effective_ttl_source_id",
            "icon",
            "id",
        ],
    );
    assert_object_keys(&metadata["access"], &["read", "visible", "write"]);
    assert_eq!(metadata["color"], "#445566");
    assert_eq!(metadata["icon"], "palette");
    assert_eq!(metadata["default_ttl_days"], 14);
    assert_eq!(metadata["default_ttl_action"], "archive");
    assert_eq!(metadata["effective_ttl_days"], 14);
    assert_eq!(metadata["effective_ttl_action"], "archive");
    assert_eq!(metadata["effective_ttl_source_id"], assets_id);
    assert_eq!(metadata["effective_ttl_inherited"], false);
    assert_eq!(
        metadata["access"],
        json!({"visible": true, "read": true, "write": true})
    );
}

fn assert_contents_payload_base(contents_json: &Value) {
    assert_object_keys(
        contents_json,
        &["documents", "folder", "folders", "q", "recursive"],
    );
    assert_eq!(contents_json["folder"], "Project");
    assert_eq!(contents_json["q"], "");
    assert_eq!(contents_json["recursive"], false);
}

fn assert_folder_summary_payload_shape(folder_row: &Value) {
    assert_object_keys(
        folder_row,
        &[
            "access",
            "color",
            "default_ttl_action",
            "default_ttl_days",
            "effective_ttl_action",
            "effective_ttl_days",
            "effective_ttl_inherited",
            "effective_ttl_source_id",
            "icon",
            "id",
            "latest_by",
            "modified_at",
            "modified_display",
            "name",
            "path",
            "size_bytes",
            "size_display",
        ],
    );
    assert_eq!(folder_row["path"], "Project/Assets");
    assert_eq!(folder_row["name"], "Assets");
    assert_eq!(folder_row["color"], "#445566");
    assert_eq!(folder_row["icon"], "palette");
    assert_eq!(folder_row["latest_by"], "Asset Author");
    assert_eq!(folder_row["modified_at"], "2026-06-26T19:03:00+00:00");
    assert_eq!(folder_row["modified_display"], "Jun 26, 2026 at 7:03 pm");
    assert_eq!(folder_row["size_bytes"], 6);
    assert_eq!(folder_row["size_display"], "6 B");
    assert_eq!(
        folder_row["access"],
        json!({"visible": true, "read": true, "write": true}),
    );
}

fn assert_document_row_payload_shape(document_row: &Value, document_id: i64) {
    assert_object_keys(
        document_row,
        &[
            "access",
            "archived",
            "archived_from_folder",
            "archived_original_name",
            "archived_original_path",
            "created_at",
            "created_by",
            "created_by_name",
            "download_url",
            "expires_at",
            "expiry_action",
            "folder",
            "id",
            "latest_by",
            "latest_message",
            "latest_version_number",
            "lock",
            "modified_at",
            "modified_display",
            "name",
            "path",
            "size_bytes",
            "size_display",
            "version_count",
        ],
    );
    assert_object_keys(&document_row["access"], &["read", "visible", "write"]);
    assert_object_keys(
        &document_row["lock"],
        &["at", "by", "force_acquired", "ip", "name", "user_agent"],
    );
    assert_eq!(document_row["id"], document_id);
    assert_eq!(document_row["name"], "project-overview.txt");
    assert_eq!(document_row["path"], "Project/project-overview.txt");
    assert_eq!(document_row["folder"], "Project");
    assert_eq!(document_row["archived"], false);
    assert_eq!(document_row["archived_from_folder"], "");
    assert_eq!(document_row["archived_original_name"], "");
    assert_eq!(document_row["archived_original_path"], "");
    assert_eq!(document_row["modified_at"], "2026-06-26T18:00:00+00:00");
    assert_eq!(document_row["modified_display"], "Jun 26, 2026 at 6:00 pm");
    assert_eq!(document_row["latest_by"], "Project Author");
    assert_eq!(document_row["latest_message"], "Uploaded display asset");
    assert_eq!(document_row["latest_version_number"], 1);
    assert_eq!(document_row["version_count"], 1);
    assert_eq!(document_row["created_by"], "admin");
    assert_eq!(document_row["created_by_name"], "Admin");
    assert_eq!(document_row["size_bytes"], 6);
    assert_eq!(document_row["size_display"], "6 B");
    assert_eq!(
        document_row["download_url"],
        format!("/documents/{document_id}/versions/display-version-{document_id}/download"),
    );
    assert_eq!(document_row["expires_at"], Value::Null);
    assert_eq!(document_row["expiry_action"], Value::Null);
    assert_eq!(
        document_row["access"],
        json!({"visible": true, "read": true, "write": true}),
    );
}

#[tokio::test]
async fn folder_sidebar_and_contents_payloads_expose_python_compatible_shape() {
    let (state, _temp_dir) = test_state().await;
    let fixture = seed_payload_shape_fixture(&state).await;
    let app = http::router(state);

    let sidebar_response = app
        .clone()
        .oneshot(authed_get("/api/folders/sidebar", "writer", "writers"))
        .await
        .expect("sidebar");
    let contents_response = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "writer",
            "writers",
        ))
        .await
        .expect("contents");
    assert_eq!(sidebar_response.status(), StatusCode::OK);
    assert_eq!(contents_response.status(), StatusCode::OK);

    let sidebar_json = response_json(sidebar_response).await;
    let contents_json = response_json(contents_response).await;
    assert_sidebar_payload_shape(&sidebar_json, fixture.assets_id);
    assert_contents_payload_base(&contents_json);
    assert_folder_summary_payload_shape(&contents_json["folders"][0]);
    assert_document_row_payload_shape(&contents_json["documents"][0], fixture.document_id);
}

#[tokio::test]
async fn folder_contents_and_properties_use_python_modified_display_format() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let concept = get_or_create_folder_path(&state.db, Some("Project/Concept"))
        .await
        .expect("concept");
    add_folder_permission(&state.db, project.id, readers, true, true, false)
        .await
        .expect("reader project");
    add_folder_permission(&state.db, concept.id, readers, true, true, false)
        .await
        .expect("reader concept");
    insert_versioned_document_at(
        &state.db,
        concept.id,
        "rfc.txt",
        "2026-06-26T18:00:00Z",
        "Rfc Author",
    )
    .await;
    insert_versioned_document_at(
        &state.db,
        concept.id,
        "clock.txt",
        "2026-06-26 19:03:00",
        "Sqlite Author",
    )
    .await;
    let app = http::router(state);

    let project_contents = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "reader",
            "readers",
        ))
        .await
        .expect("project contents");
    let project_json = response_json(project_contents).await;
    assert_eq!(
        project_json["folders"][0]["modified_display"],
        "Jun 26, 2026 at 7:03 pm",
    );
    assert_eq!(
        project_json["folders"][0]["modified_at"],
        "2026-06-26T19:03:00+00:00",
    );
    assert_eq!(project_json["folders"][0]["latest_by"], "Sqlite Author");

    let concept_contents = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project/Concept",
            "reader",
            "readers",
        ))
        .await
        .expect("concept contents");
    let concept_json = response_json(concept_contents).await;
    assert_eq!(
        concept_json["documents"][0]["modified_display"],
        "Jun 26, 2026 at 7:03 pm",
    );
    assert_eq!(
        concept_json["documents"][0]["modified_at"],
        "2026-06-26T19:03:00+00:00",
    );

    let properties = app
        .oneshot(authed_get(
            "/api/folders/properties?path=Project",
            "reader",
            "readers",
        ))
        .await
        .expect("properties");
    let properties_json = response_json(properties).await;
    assert_eq!(
        properties_json["modified_display"],
        "Jun 26, 2026 at 7:03 pm",
    );
    assert_eq!(properties_json["modified_at"], "2026-06-26T19:03:00+00:00",);
    assert_eq!(properties_json["latest_by"], "Sqlite Author");
    assert!(properties_json.get("access").is_none());
}

#[tokio::test]
async fn folder_properties_hide_inaccessible_descendant_stats() {
    let (state, _temp_dir) = test_state().await;
    let artists = create_group(&state.db, "artists").await;
    let confidential = create_group(&state.db, "confidential").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, artists, true, true, false)
        .await
        .expect("artist root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let private = get_or_create_folder_path(&state.db, Some("Project/Private"))
        .await
        .expect("private");
    add_folder_permission(&state.db, project.id, artists, true, true, false)
        .await
        .expect("artist project");
    add_folder_permission(&state.db, private.id, confidential, true, true, false)
        .await
        .expect("confidential private");
    insert_document(&state.db, private.id, "secret.txt").await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(
            "/api/folders/properties?path=Project",
            "artist",
            "artists",
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["path"], "Project");
    assert_eq!(json["counts"], json!({"folders": 0, "documents": 0}));
    assert_eq!(json["size_bytes"], 0);
    assert_eq!(json["latest_by"], Value::Null);
    assert_eq!(json["modified_at"], Value::Null);
    assert_eq!(json["permissions"], json!([]));
    assert_eq!(json["available_groups"], json!([]));
}

#[tokio::test]
async fn folder_and_document_created_timestamps_use_python_datetime_iso_shape() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_roots(&state.db).await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_versioned_document_at(
        &state.db,
        project.id,
        "plan.txt",
        "2026-06-26 19:03:00",
        "Author",
    )
    .await;
    sqlx::query(
        r"
        UPDATE folders
        SET
            created_at = '2026-06-26 17:00:00.654321',
            created_by = 'folder-creator',
            created_by_name = ''
        WHERE id = ?
        ",
    )
    .bind(project.id)
    .execute(&state.db)
    .await
    .expect("folder created timestamp");
    sqlx::query(
        r"
        UPDATE documents
        SET created_at = '2026-06-26 18:01:02.123456'
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("document created timestamp");
    sqlx::query(
        r"
        INSERT INTO folder_events
            (folder_id, event_type, created_at, actor, actor_name, message)
        VALUES
            (?, 'metadata', '2026-06-26 17:30:00', 'folder-actor', '', '')
        ",
    )
    .bind(project.id)
    .execute(&state.db)
    .await
    .expect("folder event");
    let app = http::router(state);

    let contents = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "writer",
            "writers",
        ))
        .await
        .expect("contents");
    let contents_json = response_json(contents).await;
    assert_eq!(
        contents_json["documents"][0]["created_at"],
        "2026-06-26T18:01:02.123456",
    );
    assert_eq!(
        contents_json["documents"][0]["modified_at"],
        "2026-06-26T19:03:00+00:00",
    );

    let properties = app
        .oneshot(authed_get(
            "/api/folders/properties?path=Project",
            "writer",
            "writers",
        ))
        .await
        .expect("properties");
    let properties_json = response_json(properties).await;
    assert_eq!(properties_json["created_at"], "2026-06-26T17:00:00.654321",);
    assert_eq!(properties_json["created_by_name"], "folder-creator");
    assert_eq!(properties_json["history"][0]["by"], "folder-actor");
    assert_eq!(properties_json["history"][0]["message"], "metadata");
    assert_eq!(
        properties_json["history"][0]["timestamp"],
        "2026-06-26T17:30:00"
    );
}

#[tokio::test]
async fn folder_properties_patch_persists_appearance_history_and_state_event() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, writers, true, true, true)
        .await
        .expect("writer project");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_patch(
            "/api/folders/properties",
            "writer",
            "writers",
            &json!({"path": "Project", "color": "Teal", "icon": "Folder-Tree"}),
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["color"], "teal");
    assert_eq!(json["icon"], "folder-tree");
    assert_eq!(json["history"][0]["type"], "metadata");
    assert_eq!(json["history"][0]["by"], "writer");
    assert_eq!(json["history"][0]["message"], "Updated folder appearance",);
    assert_eq!(json["available_groups"][0]["name"], "writers");

    let stored =
        sqlx::query_as::<_, (String, String)>("SELECT color, icon FROM folders WHERE id = ?")
            .bind(project.id)
            .fetch_one(&pool)
            .await
            .expect("stored appearance");
    assert_eq!(stored.0, "teal");
    assert_eq!(stored.1, "folder-tree");

    let state_event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, resources FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");
    assert_eq!(state_event.0, "folder.properties");
    assert_eq!(
        serde_json::from_str::<Value>(&state_event.1).expect("resources json"),
        json!(["contents", "sidebar"]),
    );
}

#[tokio::test]
async fn folder_properties_patch_rejects_invalid_values_and_missing_write_access() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let readers = create_group(&state.db, "readers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, writers, true, true, true)
        .await
        .expect("writer project");
    add_folder_permission(&state.db, project.id, readers, true, true, false)
        .await
        .expect("reader project");
    let app = http::router(state);

    let invalid_color = app
        .clone()
        .oneshot(authed_json_patch(
            "/api/folders/properties",
            "writer",
            "writers",
            &json!({"path": "Project", "color": "purple", "icon": "folder"}),
        ))
        .await
        .expect("invalid color");
    let invalid_color_status = invalid_color.status();
    let invalid_color_json = response_json(invalid_color).await;
    assert_eq!(invalid_color_status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_color_json["detail"], "Invalid folder color");

    let invalid_icon = app
        .clone()
        .oneshot(authed_json_patch(
            "/api/folders/properties",
            "writer",
            "writers",
            &json!({"path": "Project", "color": "blue", "icon": "-bad"}),
        ))
        .await
        .expect("invalid icon");
    let invalid_icon_status = invalid_icon.status();
    let invalid_icon_json = response_json(invalid_icon).await;
    assert_eq!(invalid_icon_status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_icon_json["detail"], "Invalid folder icon");

    let read_only = app
        .oneshot(authed_json_patch(
            "/api/folders/properties",
            "reader",
            "readers",
            &json!({"path": "Project", "color": "blue", "icon": "folder"}),
        ))
        .await
        .expect("read only");
    let read_only_status = read_only.status();
    let read_only_json = response_json(read_only).await;
    assert_eq!(read_only_status, StatusCode::FORBIDDEN);
    assert_eq!(read_only_json["detail"], "Insufficient folder access");
}

#[tokio::test]
async fn folder_permissions_put_replaces_permissions_history_and_state_event() {
    let (state, _temp_dir) = test_state().await;
    let artists = create_group(&state.db, "artists").await;
    let reviewers = create_group(&state.db, "reviewers").await;
    let old_group = create_group(&state.db, "old-group").await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, old_group, true, true, true)
        .await
        .expect("old permission");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_put(
            "/api/folders/permissions",
            "admin",
            "vault-admin",
            &json!({
                "path": "Project",
                "permissions": [
                    {"group_id": reviewers, "can_view": true, "can_read": true, "can_write": false},
                    {"group_id": artists, "can_view": true, "can_read": true, "can_write": true}
                ]
            }),
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["permissions"].as_array().expect("permissions").len(),
        2
    );
    assert_eq!(json["permissions"][0]["group_name"], "artists");
    assert_eq!(json["permissions"][0]["can_write"], true);
    assert_eq!(json["permissions"][1]["group_name"], "reviewers");
    assert_eq!(json["permissions"][1]["can_write"], false);
    assert_eq!(json["history"][0]["type"], "permissions");
    assert_eq!(json["history"][0]["by"], "admin");
    assert_eq!(json["history"][0]["message"], "Updated folder permissions");

    let permission_rows = sqlx::query_as::<_, (String, bool, bool, bool)>(
        r"
        SELECT vg.name, fp.can_view, fp.can_read, fp.can_write
        FROM folder_permissions fp
        JOIN vault_groups vg ON vg.id = fp.group_id
        WHERE fp.folder_id = ?
        ORDER BY vg.name
        ",
    )
    .bind(project.id)
    .fetch_all(&pool)
    .await
    .expect("permission rows");
    assert_eq!(
        permission_rows,
        vec![
            ("artists".to_string(), true, true, true),
            ("reviewers".to_string(), true, true, false),
        ],
    );

    let state_event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, resources FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");
    assert_eq!(state_event.0, "folder.permissions");
    assert_eq!(
        serde_json::from_str::<Value>(&state_event.1).expect("resources json"),
        json!(["contents", "sidebar"]),
    );

    let artist_contents = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "artist",
            "artists",
        ))
        .await
        .expect("artist contents");
    assert_eq!(artist_contents.status(), StatusCode::OK);
}

#[tokio::test]
async fn folder_permissions_put_requires_admin_and_does_not_mutate() {
    let (state, _temp_dir) = test_state().await;
    let artists = create_group(&state.db, "artists").await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_put(
            "/api/folders/permissions",
            "artist",
            "artists",
            &json!({
                "path": "Project",
                "permissions": [
                    {"group_id": artists, "can_view": true, "can_read": true, "can_write": true}
                ]
            }),
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;
    let permission_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folder_permissions WHERE folder_id = ?")
            .bind(project.id)
            .fetch_one(&pool)
            .await
            .expect("permission count");

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["detail"], "Admin access required");
    assert_eq!(permission_count, 0);
}

#[tokio::test]
async fn folder_permissions_put_rejects_invalid_rows_before_mutating() {
    let (state, _temp_dir) = test_state().await;
    let artists = create_group(&state.db, "artists").await;
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&state.db, project.id, artists, true, true, false)
        .await
        .expect("artist permission");
    let pool = state.db.clone();
    let app = http::router(state);

    let invalid_flags = app
        .clone()
        .oneshot(authed_json_put(
            "/api/folders/permissions",
            "admin",
            "vault-admin",
            &json!({
                "path": "Project",
                "permissions": [
                    {"group_id": artists, "can_view": false, "can_read": false, "can_write": true}
                ]
            }),
        ))
        .await
        .expect("invalid flags");
    let invalid_flags_status = invalid_flags.status();
    let invalid_flags_json = response_json(invalid_flags).await;
    assert_eq!(invalid_flags_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid_flags_json["detail"],
        "Write permission requires read and view permission",
    );

    let duplicate = app
        .clone()
        .oneshot(authed_json_put(
            "/api/folders/permissions",
            "admin",
            "vault-admin",
            &json!({
                "path": "Project",
                "permissions": [
                    {"group_id": artists, "can_view": true, "can_read": true, "can_write": false},
                    {"group_id": artists, "can_view": true, "can_read": true, "can_write": true}
                ]
            }),
        ))
        .await
        .expect("duplicate");
    let duplicate_status = duplicate.status();
    let duplicate_json = response_json(duplicate).await;
    assert_eq!(duplicate_status, StatusCode::BAD_REQUEST);
    assert_eq!(duplicate_json["detail"], "Duplicate group permission");

    let missing_group = app
        .oneshot(authed_json_put(
            "/api/folders/permissions",
            "admin",
            "vault-admin",
            &json!({
                "path": "Project",
                "permissions": [
                    {"group_id": 99999, "can_view": true, "can_read": true, "can_write": false}
                ]
            }),
        ))
        .await
        .expect("missing group");
    let missing_group_status = missing_group.status();
    let missing_group_json = response_json(missing_group).await;
    assert_eq!(missing_group_status, StatusCode::NOT_FOUND);
    assert_eq!(missing_group_json["detail"], "Group not found");

    let stored =
        sqlx::query_as::<_, (i64, bool, bool, bool)>(
            "SELECT group_id, can_view, can_read, can_write FROM folder_permissions WHERE folder_id = ?",
        )
        .bind(project.id)
        .fetch_one(&pool)
        .await
        .expect("stored permission");
    assert_eq!(stored, (artists, true, true, false));
}

#[tokio::test]
async fn folder_retention_put_reapplies_subtree_policy() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let concept = get_or_create_folder_path(&state.db, Some("Project/Concept"))
        .await
        .expect("concept");
    let doc_id =
        insert_document_modified(&state.db, concept.id, "sketch.png", "2026-06-01 00:00:00").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let update = app
        .clone()
        .oneshot(authed_json_put(
            "/api/folders/retention",
            "writer",
            "writers",
            &json!({
                "path": "Project",
                "default_ttl_action": "archive",
                "default_ttl_days": 30
            }),
        ))
        .await
        .expect("update response");
    let update_status = update.status();
    let update_json = response_json(update).await;

    assert_eq!(update_status, StatusCode::OK);
    assert_eq!(update_json["default_ttl_action"], "archive");
    assert_eq!(update_json["default_ttl_days"], 30);
    assert_eq!(update_json["effective_ttl_action"], "archive");
    assert_eq!(update_json["effective_ttl_days"], 30);
    assert_eq!(update_json["history"][0]["type"], "retention");
    assert_eq!(
        update_json["history"][0]["message"],
        "Updated folder retention policy",
    );

    let stored_doc = sqlx::query_as::<_, (String, String)>(
        "SELECT expires_at, expiry_action FROM documents WHERE id = ?",
    )
    .bind(doc_id)
    .fetch_one(&pool)
    .await
    .expect("stored doc");
    assert_eq!(stored_doc.0, "2026-07-01 00:00:00");
    assert_eq!(stored_doc.1, "archive");

    let state_event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, resources FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");
    assert_eq!(state_event.0, "folder.retention");
    assert_eq!(
        serde_json::from_str::<Value>(&state_event.1).expect("resources json"),
        json!(["contents", "document_detail", "my_edits", "sidebar"]),
    );

    let contents = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project",
            "writer",
            "writers",
        ))
        .await
        .expect("contents response");
    let contents_json = response_json(contents).await;
    assert_eq!(contents_json["folders"][0]["path"], "Project/Concept");
    assert_eq!(contents_json["folders"][0]["default_ttl_action"], "none");
    assert_eq!(contents_json["folders"][0]["default_ttl_days"], Value::Null);
    assert_eq!(
        contents_json["folders"][0]["effective_ttl_action"],
        "archive"
    );
    assert_eq!(contents_json["folders"][0]["effective_ttl_days"], 30);
    assert_eq!(contents_json["folders"][0]["effective_ttl_inherited"], true);

    let child_contents = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Project/Concept",
            "writer",
            "writers",
        ))
        .await
        .expect("child contents response");
    let child_json = response_json(child_contents).await;
    assert_eq!(child_json["documents"][0]["expiry_action"], "archive");
    assert_eq!(
        child_json["documents"][0]["expires_at"],
        "2026-07-01T00:00:00+00:00"
    );
}

#[tokio::test]
async fn folder_retention_put_clears_policy_and_document_expiry() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let concept = get_or_create_folder_path(&state.db, Some("Project/Concept"))
        .await
        .expect("concept");
    let doc_id =
        insert_document_modified(&state.db, concept.id, "sketch.png", "2026-06-01 00:00:00").await;
    sqlx::query(
        "UPDATE folders SET default_ttl_days = 30, default_ttl_action = 'archive' WHERE id = ?",
    )
    .bind(project.id)
    .execute(&state.db)
    .await
    .expect("seed project ttl");
    sqlx::query(
        "UPDATE documents SET expires_at = '2026-07-01 00:00:00', expiry_action = 'archive' WHERE id = ?",
    )
    .bind(doc_id)
    .execute(&state.db)
    .await
    .expect("seed document ttl");
    let pool = state.db.clone();
    let app = http::router(state);

    let clear = app
        .oneshot(authed_json_put(
            "/api/folders/retention",
            "writer",
            "writers",
            &json!({
                "path": "Project",
                "default_ttl_action": "none",
                "default_ttl_days": null
            }),
        ))
        .await
        .expect("clear response");
    let clear_status = clear.status();
    let clear_json = response_json(clear).await;

    assert_eq!(clear_status, StatusCode::OK);
    assert_eq!(clear_json["default_ttl_action"], "none");
    assert_eq!(clear_json["default_ttl_days"], Value::Null);
    assert_eq!(clear_json["effective_ttl_action"], "none");

    let cleared_doc = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT expires_at, expiry_action FROM documents WHERE id = ?",
    )
    .bind(doc_id)
    .fetch_one(&pool)
    .await
    .expect("cleared doc");
    let project_policy = sqlx::query_as::<_, (Option<i64>, Option<String>)>(
        "SELECT default_ttl_days, default_ttl_action FROM folders WHERE id = ?",
    )
    .bind(project.id)
    .fetch_one(&pool)
    .await
    .expect("project policy");

    assert_eq!(cleared_doc, (None, None));
    assert_eq!(project_policy, (None, None));
}

#[tokio::test]
async fn folder_retention_put_rejects_delete_for_non_admin_and_invalid_payloads() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let pool = state.db.clone();
    let app = http::router(state);

    let delete = app
        .clone()
        .oneshot(authed_json_put(
            "/api/folders/retention",
            "writer",
            "writers",
            &json!({
                "path": "Project",
                "default_ttl_action": "delete",
                "default_ttl_days": 7
            }),
        ))
        .await
        .expect("delete response");
    let delete_status = delete.status();
    let delete_json = response_json(delete).await;
    assert_eq!(delete_status, StatusCode::FORBIDDEN);
    assert_eq!(
        delete_json["detail"],
        "Admin access required for delete TTL"
    );

    let invalid_action = app
        .clone()
        .oneshot(authed_json_put(
            "/api/folders/retention",
            "writer",
            "writers",
            &json!({
                "path": "Project",
                "default_ttl_action": "purge",
                "default_ttl_days": 7
            }),
        ))
        .await
        .expect("invalid action");
    let invalid_action_status = invalid_action.status();
    let invalid_action_json = response_json(invalid_action).await;
    assert_eq!(invalid_action_status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_action_json["detail"], "Invalid TTL action");

    let missing_days = app
        .clone()
        .oneshot(authed_json_put(
            "/api/folders/retention",
            "writer",
            "writers",
            &json!({
                "path": "Project",
                "default_ttl_action": "archive",
                "default_ttl_days": null
            }),
        ))
        .await
        .expect("missing days");
    let missing_days_status = missing_days.status();
    let missing_days_json = response_json(missing_days).await;
    assert_eq!(missing_days_status, StatusCode::BAD_REQUEST);
    assert_eq!(missing_days_json["detail"], "TTL days are required");

    let out_of_range = app
        .oneshot(authed_json_put(
            "/api/folders/retention",
            "writer",
            "writers",
            &json!({
                "path": "Project",
                "default_ttl_action": "archive",
                "default_ttl_days": 3651
            }),
        ))
        .await
        .expect("out of range");
    let out_of_range_status = out_of_range.status();
    let out_of_range_json = response_json(out_of_range).await;
    assert_eq!(out_of_range_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        out_of_range_json["detail"],
        "TTL days must be between 1 and 3650",
    );

    let project_policy = sqlx::query_as::<_, (Option<i64>, Option<String>)>(
        "SELECT default_ttl_days, default_ttl_action FROM folders WHERE id = ?",
    )
    .bind(project.id)
    .fetch_one(&pool)
    .await
    .expect("project policy");
    assert_eq!(project_policy, (None, None));
}

#[tokio::test]
async fn folder_retention_put_rejects_inaccessible_descendants_without_mutating() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let confidential = create_group(&state.db, "confidential").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let private = get_or_create_folder_path(&state.db, Some("Project/Private"))
        .await
        .expect("private");
    add_folder_permission(&state.db, private.id, confidential, true, true, true)
        .await
        .expect("confidential private");
    let secret_id =
        insert_document_modified(&state.db, private.id, "secret.txt", "2026-06-01 00:00:00").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let update = app
        .oneshot(authed_json_put(
            "/api/folders/retention",
            "writer",
            "writers",
            &json!({
                "path": "Project",
                "default_ttl_action": "archive",
                "default_ttl_days": 30
            }),
        ))
        .await
        .expect("update response");
    let update_status = update.status();
    let update_json = response_json(update).await;

    assert_eq!(update_status, StatusCode::NOT_FOUND);
    assert_eq!(update_json["detail"], "Folder not found");

    let project_policy = sqlx::query_as::<_, (Option<i64>, Option<String>)>(
        "SELECT default_ttl_days, default_ttl_action FROM folders WHERE id = ?",
    )
    .bind(project.id)
    .fetch_one(&pool)
    .await
    .expect("project policy");
    let secret_expiry = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT expires_at, expiry_action FROM documents WHERE id = ?",
    )
    .bind(secret_id)
    .fetch_one(&pool)
    .await
    .expect("secret expiry");

    assert_eq!(project_policy, (None, None));
    assert_eq!(secret_expiry, (None, None));
}

#[tokio::test]
async fn rename_folder_updates_path_history_and_state() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let old = get_or_create_folder_path(&state.db, Some("Project/Old"))
        .await
        .expect("old folder");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({"items": [{"type": "folder", "id": old.id}], "name": "New"}),
        ))
        .await
        .expect("rename response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"][0]["detail"], "Project/New");
    assert_eq!(json["ok"][0]["item"]["path"], "Project/Old");
    assert_eq!(json["failed"], json!([]));

    let renamed =
        sqlx::query_as::<_, (String, i64)>("SELECT name, parent_id FROM folders WHERE id = ?")
            .bind(old.id)
            .fetch_one(&pool)
            .await
            .expect("renamed folder");
    let event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, message FROM folder_events WHERE folder_id = ?",
    )
    .bind(old.id)
    .fetch_one(&pool)
    .await
    .expect("folder event");
    let state_event = sqlx::query_scalar::<_, String>(
        "SELECT event_type FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");

    assert_eq!(renamed, ("New".to_string(), project.id));
    assert_eq!(
        event,
        ("rename".to_string(), "Renamed from Old to New".to_string())
    );
    assert_eq!(state_event, "batch.rename");
}

#[tokio::test]
async fn rename_folder_rejects_root_duplicate_and_cycle() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    get_or_create_folder_path(&state.db, Some("Project/Taken"))
        .await
        .expect("taken folder");
    let duplicate_source = get_or_create_folder_path(&state.db, Some("Project/DupeSource"))
        .await
        .expect("duplicate source");
    let old = get_or_create_folder_path(&state.db, Some("Project/Old"))
        .await
        .expect("old folder");
    get_or_create_folder_path(&state.db, Some("Project/Old/Sub"))
        .await
        .expect("sub folder");
    let pool = state.db.clone();
    let app = http::router(state);

    let root_response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({"items": [{"type": "folder", "id": root.id}], "name": "VaultRenamed"}),
        ))
        .await
        .expect("root rename");
    assert_eq!(
        response_json(root_response).await["failed"][0]["detail"],
        "Cannot move a root folder",
    );

    let duplicate = app
        .clone()
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({"items": [{"type": "folder", "id": duplicate_source.id}], "name": "Taken"}),
        ))
        .await
        .expect("duplicate rename");
    assert_eq!(
        response_json(duplicate).await["failed"][0]["detail"],
        "A folder already exists at that path",
    );

    let invalid_destination = app
        .clone()
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "folder", "id": old.id}],
                "destination_folder": "..",
                "name": "InvalidDestination"
            }),
        ))
        .await
        .expect("invalid destination rename");
    let invalid_status = invalid_destination.status();
    let invalid_json = response_json(invalid_destination).await;
    assert_eq!(invalid_status, StatusCode::OK);
    assert_eq!(invalid_json["ok"], json!([]));
    assert_eq!(invalid_json["failed"][0]["detail"], "Invalid folder path",);

    let cycle = app
        .clone()
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "folder", "id": old.id}],
                "destination_folder": "Project/Old/Sub",
                "name": "Moved"
            }),
        ))
        .await
        .expect("cycle rename");
    assert_eq!(
        response_json(cycle).await["failed"][0]["detail"],
        "Cannot move a folder into itself",
    );

    let unchanged =
        sqlx::query_as::<_, (String, i64)>("SELECT name, parent_id FROM folders WHERE id = ?")
            .bind(old.id)
            .fetch_one(&pool)
            .await
            .expect("old folder unchanged");
    assert_eq!(unchanged, ("Old".to_string(), project.id));
}

#[tokio::test]
async fn rename_folder_rejects_locked_descendant_without_mutating() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let old = get_or_create_folder_path(&state.db, Some("Project/Old"))
        .await
        .expect("old folder");
    let locked_doc = insert_document(&state.db, old.id, "locked.txt").await;
    sqlx::query(
        r"
        INSERT INTO document_locks (document_id, locked_by, locked_by_name)
        VALUES (?, 'other', 'Other')
        ",
    )
    .bind(locked_doc)
    .execute(&state.db)
    .await
    .expect("lock document");
    let pool = state.db.clone();
    let app = http::router(state);

    let locked = app
        .oneshot(authed_json_post(
            "/api/rename",
            "writer",
            "writers",
            &json!({"items": [{"type": "folder", "id": old.id}], "name": "OldRenamed"}),
        ))
        .await
        .expect("locked rename");
    assert_eq!(
        response_json(locked).await["failed"][0]["detail"],
        "Document is locked by another user",
    );

    let unchanged =
        sqlx::query_as::<_, (String, i64)>("SELECT name, parent_id FROM folders WHERE id = ?")
            .bind(old.id)
            .fetch_one(&pool)
            .await
            .expect("old folder unchanged");
    assert_eq!(unchanged, ("Old".to_string(), project.id));
}

#[tokio::test]
async fn move_folder_moves_subtree_reapplies_ttl_and_prunes_nested_items() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let old = get_or_create_folder_path(&state.db, Some("Project/Old"))
        .await
        .expect("old folder");
    let destination = get_or_create_folder_path(&state.db, Some("Destination"))
        .await
        .expect("destination");
    sqlx::query(
        "UPDATE folders SET default_ttl_days = 30, default_ttl_action = 'archive' WHERE id = ?",
    )
    .bind(destination.id)
    .execute(&state.db)
    .await
    .expect("destination ttl");
    let document_id = insert_document(&state.db, old.id, "nested.txt").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({
                "items": [
                    {"type": "folder", "id": old.id},
                    {"type": "document", "id": document_id}
                ],
                "destination_folder": "Destination"
            }),
        ))
        .await
        .expect("move response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"].as_array().expect("ok").len(), 1);
    assert_eq!(json["ok"][0]["item"]["type"], "folder");
    assert_eq!(json["ok"][0]["item"]["id"], old.id);
    assert_eq!(json["ok"][0]["detail"], "Destination/Old");
    assert_eq!(json["failed"], json!([]));

    let moved_folder =
        sqlx::query_as::<_, (String, i64)>("SELECT name, parent_id FROM folders WHERE id = ?")
            .bind(old.id)
            .fetch_one(&pool)
            .await
            .expect("moved folder");
    let document = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT folder_id, expiry_action FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document");
    let event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, message FROM folder_events WHERE folder_id = ?",
    )
    .bind(old.id)
    .fetch_one(&pool)
    .await
    .expect("folder event");
    let state_event = sqlx::query_scalar::<_, String>(
        "SELECT event_type FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");
    let document_move_events = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_events WHERE document_id = ? AND event_type = 'move'",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document move events");

    assert_eq!(moved_folder, ("Old".to_string(), destination.id));
    assert_eq!(document, (old.id, Some("archive".to_string())));
    assert_eq!(event.0, "move");
    assert_eq!(event.1, "Moved from Project/Old to Destination/Old");
    assert_eq!(state_event, "batch.move");
    assert_eq!(document_move_events, 0);
    assert_ne!(project.id, destination.id);
}

#[tokio::test]
async fn move_folder_out_of_delete_ttl_scope_clears_descendant_expiry_before_sweep() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let temp = get_or_create_folder_path(&state.db, Some("Temp"))
        .await
        .expect("temp");
    let work = get_or_create_folder_path(&state.db, Some("Temp/Work"))
        .await
        .expect("work");
    let safe = get_or_create_folder_path(&state.db, Some("Safe"))
        .await
        .expect("safe");
    sqlx::query(
        "UPDATE folders SET default_ttl_days = 1, default_ttl_action = 'delete' WHERE id = ?",
    )
    .bind(temp.id)
    .execute(&state.db)
    .await
    .expect("set ttl");
    let document_id = insert_document(&state.db, work.id, "asset.fbx").await;
    sqlx::query(
        r"
        UPDATE documents
        SET latest_modified_at = '2025-06-01 00:00:00',
            expires_at = '2025-06-02 00:00:00',
            expiry_action = 'delete'
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("seed expired ttl");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "folder", "id": work.id}],
                "destination_folder": "Safe"
            }),
        ))
        .await
        .expect("move response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"][0]["detail"], "Safe/Work");
    assert_eq!(json["failed"], json!([]));

    let moved_folder =
        sqlx::query_as::<_, (String, i64)>("SELECT name, parent_id FROM folders WHERE id = ?")
            .bind(work.id)
            .fetch_one(&pool)
            .await
            .expect("moved folder");
    let document = sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
        "SELECT folder_id, expires_at, expiry_action FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("document");
    assert_eq!(moved_folder, ("Work".to_string(), safe.id));
    assert_eq!(document, (work.id, None, None));

    let sweep = sweep_expired_documents(&pool, 250).await.expect("sweep");
    assert!(sweep.deleted.is_empty());
    assert!(sweep.archived.is_empty());
    assert!(sweep.skipped.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document count"),
        1,
    );
}

#[tokio::test]
async fn move_folder_rejects_descendant_destination_without_creating_missing_parent() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/move",
            "writer",
            "writers",
            &json!({
                "items": [{"type": "folder", "id": project.id}],
                "destination_folder": "Project/NewParent"
            }),
        ))
        .await
        .expect("move response");
    let json = response_json(response).await;

    assert_eq!(json["ok"], json!([]));
    assert_eq!(
        json["failed"][0]["detail"],
        "Cannot move a folder into itself",
    );

    let project_parent = sqlx::query_scalar::<_, i64>("SELECT parent_id FROM folders WHERE id = ?")
        .bind(project.id)
        .fetch_one(&pool)
        .await
        .expect("project parent");
    let new_parent_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE name = 'NewParent'")
            .fetch_one(&pool)
            .await
            .expect("new parent count");
    let non_root_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE is_root = 0")
            .fetch_one(&pool)
            .await
            .expect("folder count");
    let folder_events = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folder_events")
        .fetch_one(&pool)
        .await
        .expect("folder event count");
    let state_events = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("state event count");

    assert_eq!(project_parent, root.id);
    assert_eq!(new_parent_count, 0);
    assert_eq!(non_root_count, 1);
    assert_eq!(folder_events, 0);
    assert_eq!(state_events, 0);
}

#[tokio::test]
async fn archive_folder_path_item_reports_stale_path_as_item_failure_without_mutating() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_document(&state.db, project.id, "plan.txt").await;
    let pool = state.db.clone();
    let app = http::router(state);

    sqlx::query("UPDATE folders SET name = 'Renamed' WHERE id = ?")
        .bind(project.id)
        .execute(&pool)
        .await
        .expect("rename project before stale archive");

    let response = app
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({"items": [{"type": "folder", "path": " /Project\\ " }]}),
        ))
        .await
        .expect("archive response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"], json!([]));
    assert_eq!(
        json["failed"][0]["item"],
        json!({"type": "folder", "path": "Project"})
    );
    assert_eq!(json["failed"][0]["detail"], "Folder not found");

    let document_folder =
        sqlx::query_scalar::<_, i64>("SELECT folder_id FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document folder");
    let folder_name = sqlx::query_scalar::<_, String>("SELECT name FROM folders WHERE id = ?")
        .bind(project.id)
        .fetch_one(&pool)
        .await
        .expect("folder name");
    let event_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_events")
        .fetch_one(&pool)
        .await
        .expect("document events");
    let state_event_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("state events");

    assert_eq!(document_folder, project.id);
    assert_eq!(folder_name, "Renamed");
    assert_eq!(event_count, 0);
    assert_eq!(state_event_count, 0);
}

#[tokio::test]
async fn archive_folder_archives_descendant_documents_and_prunes_nested_items() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let old = get_or_create_folder_path(&state.db, Some("Project/Old"))
        .await
        .expect("old folder");
    let sub = get_or_create_folder_path(&state.db, Some("Project/Old/Sub"))
        .await
        .expect("sub folder");
    let top_doc = insert_document(&state.db, old.id, "top.txt").await;
    let nested_doc = insert_document(&state.db, sub.id, "nested.txt").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({
                "items": [
                    {"type": "folder", "id": old.id},
                    {"type": "document", "id": nested_doc}
                ]
            }),
        ))
        .await
        .expect("archive response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"].as_array().expect("ok").len(), 1);
    assert_eq!(json["ok"][0]["detail"], "Archive");
    assert_eq!(json["failed"], json!([]));

    let remaining_folders =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE id IN (?, ?)")
            .bind(old.id)
            .bind(sub.id)
            .fetch_one(&pool)
            .await
            .expect("remaining folders");
    let archived_docs = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT folder_id, name, archived_from_folder FROM documents ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("archived docs");
    let event_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_events WHERE event_type = 'archive'",
    )
    .fetch_one(&pool)
    .await
    .expect("archive events");
    let state_event = sqlx::query_scalar::<_, String>(
        "SELECT event_type FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");

    assert_eq!(remaining_folders, 0);
    assert_eq!(
        archived_docs[0],
        (
            archive_root.id,
            "top.txt".to_string(),
            "Project/Old".to_string()
        )
    );
    assert_eq!(
        archived_docs[1],
        (
            archive_root.id,
            "nested.txt".to_string(),
            "Project/Old/Sub".to_string()
        )
    );
    assert_eq!(event_count, 2);
    assert_eq!(state_event, "batch.archive");
    assert_ne!(top_doc, nested_doc);
}

#[tokio::test]
async fn archive_allows_duplicate_display_names_and_returns_flat_contents() {
    let (state, _temp_dir) = test_state().await;
    let (_, _, archive_root_id) = grant_writer_roots(&state.db).await;
    let one = get_or_create_folder_path(&state.db, Some("One"))
        .await
        .expect("one folder");
    let two = get_or_create_folder_path(&state.db, Some("Two"))
        .await
        .expect("two folder");
    let first = insert_document(&state.db, one.id, "plan.txt").await;
    let second = insert_document(&state.db, two.id, "plan.txt").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let archive = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({
                "items": [
                    {"type": "document", "id": first},
                    {"type": "document", "id": second}
                ]
            }),
        ))
        .await
        .expect("archive response");
    let archive_json = response_json(archive).await;
    assert_eq!(archive_json["ok"].as_array().expect("ok").len(), 2);
    assert_eq!(archive_json["failed"], json!([]));

    let mut archived_rows = sqlx::query_as::<_, (i64, i64, String, String, String)>(
        r"
        SELECT id, folder_id, name, archived_from_folder, archived_original_name
        FROM documents
        WHERE folder_id = ? AND name = 'plan.txt'
        ORDER BY archived_from_folder
        ",
    )
    .bind(archive_root_id)
    .fetch_all(&pool)
    .await
    .expect("archived rows");
    assert_eq!(archived_rows.len(), 2);
    assert_eq!(
        archived_rows
            .iter()
            .map(|row| (row.2.as_str(), row.3.as_str(), row.4.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("plan.txt", "One", "plan.txt"),
            ("plan.txt", "Two", "plan.txt")
        ],
    );

    let contents = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Archive",
            "writer",
            "writers",
        ))
        .await
        .expect("archive contents");
    let contents_json = response_json(contents).await;
    assert_eq!(contents_json["folders"], json!([]));
    assert_eq!(
        contents_json["documents"]
            .as_array()
            .expect("documents")
            .len(),
        2,
    );
    let mut original_paths = contents_json["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .map(|row| {
            (
                row["archived_from_folder"].as_str().expect("from"),
                row["archived_original_path"].as_str().expect("path"),
            )
        })
        .collect::<Vec<_>>();
    original_paths.sort_unstable();
    assert_eq!(
        original_paths,
        vec![("One", "One/plan.txt"), ("Two", "Two/plan.txt")],
    );
    archived_rows.sort_by_key(|row| row.0);
    assert_eq!(archived_rows[0].0, first);
    assert_eq!(archived_rows[1].0, second);
}

#[tokio::test]
async fn archive_contents_search_matches_original_path_when_original_name_is_empty() {
    let (state, _temp_dir) = test_state().await;
    let (writers, _, archive_root_id) = grant_writer_roots(&state.db).await;
    let archived_id = sqlx::query(
        r"
        INSERT INTO documents
            (
                folder_id,
                name,
                created_by,
                created_by_name,
                latest_modified_by,
                archived_from_folder,
                archived_original_name,
                archived_access
            )
        VALUES
            (?, 'archived-display.txt', 'admin', 'Admin', 'admin', 'One', '', ?)
        ",
    )
    .bind(archive_root_id)
    .bind(format!("{{\"{writers}\":3}}"))
    .execute(&state.db)
    .await
    .expect("archived document")
    .last_insert_rowid();
    let app = http::router(state);

    let response = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Archive&q=One%2Farchived-display.txt",
            "writer",
            "writers",
        ))
        .await
        .expect("archive search");
    let status = response.status();
    let body = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["folders"], json!([]));
    assert_eq!(
        body["documents"]
            .as_array()
            .expect("documents")
            .iter()
            .map(|row| row["id"].as_i64().expect("id"))
            .collect::<Vec<_>>(),
        vec![archived_id],
    );
    assert_eq!(body["documents"][0]["archived_original_path"], "One",);
}

#[tokio::test]
async fn restore_document_recreates_missing_original_folder_from_archive_metadata() {
    let (state, _temp_dir) = test_state().await;
    grant_writer_roots(&state.db).await;
    let source = get_or_create_folder_path(&state.db, Some("Project/Sub"))
        .await
        .expect("source folder");
    let document_id = insert_document(&state.db, source.id, "restore.txt").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let archive = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("archive response");
    assert_eq!(
        response_json(archive).await["ok"][0]["detail"],
        "Archive/restore.txt",
    );
    sqlx::query("DELETE FROM folders WHERE id = ?")
        .bind(source.id)
        .execute(&pool)
        .await
        .expect("delete original folder");

    let restore = app
        .oneshot(authed_json_post(
            "/api/restore",
            "writer",
            "writers",
            &json!({"items": [{"type": "document", "id": document_id}]}),
        ))
        .await
        .expect("restore response");
    let restore_json = response_json(restore).await;
    assert_eq!(restore_json["ok"][0]["detail"], "Project/Sub/restore.txt");

    let restored_folder = get_or_create_folder_path(&pool, Some("Project/Sub"))
        .await
        .expect("restored folder lookup");
    let restored_doc = sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
        r"
        SELECT folder_id, archived_from_folder, archived_original_name
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("restored doc");
    assert_eq!(restored_doc, (restored_folder.id, None, None));
}

#[tokio::test]
async fn archive_folder_rejects_locked_descendant_without_mutating() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let old = get_or_create_folder_path(&state.db, Some("Project/Old"))
        .await
        .expect("old folder");
    let document_id = insert_document(&state.db, old.id, "locked.txt").await;
    sqlx::query(
        r"
        INSERT INTO document_locks (document_id, locked_by, locked_by_name)
        VALUES (?, 'other', 'Other')
        ",
    )
    .bind(document_id)
    .execute(&state.db)
    .await
    .expect("lock document");
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({"items": [{"type": "folder", "id": old.id}]}),
        ))
        .await
        .expect("archive response");
    let json = response_json(response).await;

    assert_eq!(
        json["failed"][0]["detail"],
        "Document is locked by another user"
    );

    let folder_exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE id = ?")
        .bind(old.id)
        .fetch_one(&pool)
        .await
        .expect("folder exists");
    let document_folder =
        sqlx::query_scalar::<_, i64>("SELECT folder_id FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document folder");
    assert_eq!(folder_exists, 1);
    assert_eq!(document_folder, old.id);
}

#[tokio::test]
async fn archive_folder_rejects_inaccessible_descendant_without_mutating() {
    let (state, _temp_dir) = test_state().await;
    let writers = create_group(&state.db, "writers").await;
    let confidential = create_group(&state.db, "confidential").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(&state.db, archive_root.id, writers, true, true, true)
        .await
        .expect("writer archive root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project folder");
    let private = get_or_create_folder_path(&state.db, Some("Project/Private"))
        .await
        .expect("private folder");
    add_folder_permission(&state.db, project.id, writers, true, true, true)
        .await
        .expect("writer project");
    add_folder_permission(&state.db, private.id, confidential, true, true, true)
        .await
        .expect("confidential private");
    let visible_id = insert_document(&state.db, project.id, "visible.txt").await;
    let secret_id = insert_document(&state.db, private.id, "secret.txt").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .oneshot(authed_json_post(
            "/api/archive",
            "writer",
            "writers",
            &json!({"items": [{"type": "folder", "id": project.id}]}),
        ))
        .await
        .expect("archive response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"], json!([]));
    assert_eq!(json["failed"].as_array().expect("failed").len(), 1);
    assert_eq!(json["failed"][0]["detail"], "Folder not found");

    let document_rows = sqlx::query_as::<_, (i64, i64, String, Option<String>)>(
        r"
        SELECT id, folder_id, name, archived_from_folder
        FROM documents
        ORDER BY id
        ",
    )
    .fetch_all(&pool)
    .await
    .expect("document rows");
    let folders_left =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE id IN (?, ?)")
            .bind(project.id)
            .bind(private.id)
            .fetch_one(&pool)
            .await
            .expect("folders left");
    let document_events = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_events")
        .fetch_one(&pool)
        .await
        .expect("document events");
    let state_events = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("state events");

    assert_eq!(
        document_rows,
        vec![
            (visible_id, project.id, "visible.txt".to_string(), None),
            (secret_id, private.id, "secret.txt".to_string(), None),
        ],
    );
    assert_eq!(folders_left, 2);
    assert_eq!(document_events, 0);
    assert_eq!(state_events, 0);
}

#[tokio::test]
async fn failed_folder_archive_preserves_source_folder_acl() {
    let (state, _temp_dir) = test_state().await;
    let outsiders = create_group(&state.db, "outsiders").await;
    let confidential = create_group(&state.db, "confidential").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let archive_root = get_root_folder(&state.db, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&state.db, root.id, outsiders, true, true, true)
        .await
        .expect("outsider root");
    add_folder_permission(&state.db, archive_root.id, outsiders, true, true, true)
        .await
        .expect("outsider archive root");
    let secret = get_or_create_folder_path(&state.db, Some("Secret"))
        .await
        .expect("secret");
    let plans = get_or_create_folder_path(&state.db, Some("Secret/Plans"))
        .await
        .expect("plans");
    add_folder_permission(&state.db, secret.id, outsiders, true, true, true)
        .await
        .expect("outsider secret");
    add_folder_permission(&state.db, plans.id, confidential, true, true, true)
        .await
        .expect("confidential plans");
    let document_id = insert_document(&state.db, plans.id, "roadmap.txt").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_json_post(
            "/api/archive",
            "outsider",
            "outsiders",
            &json!({"items": [{"type": "folder", "id": plans.id}]}),
        ))
        .await
        .expect("archive response");
    let json = response_json(response).await;

    assert_eq!(json["ok"], json!([]));
    assert_eq!(json["failed"][0]["detail"], "Folder not found");

    let plan_permissions = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        r"
        SELECT group_id, can_view, can_read, can_write
        FROM folder_permissions
        WHERE folder_id = ?
        ",
    )
    .bind(plans.id)
    .fetch_all(&pool)
    .await
    .expect("plans permissions");
    add_folder_permission(&pool, secret.id, outsiders, false, false, false)
        .await
        .expect("deny outsider secret");

    let hidden = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Secret/Plans",
            "outsider",
            "outsiders",
        ))
        .await
        .expect("hidden contents");
    let hidden_status = hidden.status();
    let hidden_json = response_json(hidden).await;
    let document_folder =
        sqlx::query_scalar::<_, i64>("SELECT folder_id FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document folder");
    let folder_events = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM document_events")
        .fetch_one(&pool)
        .await
        .expect("document events");
    let state_events = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("state events");

    assert_eq!(plan_permissions, vec![(confidential, 1, 1, 1)]);
    assert_eq!(hidden_status, StatusCode::NOT_FOUND);
    assert_eq!(hidden_json["detail"], "Folder not found");
    assert_eq!(document_folder, plans.id);
    assert_eq!(folder_events, 0);
    assert_eq!(state_events, 0);
}
