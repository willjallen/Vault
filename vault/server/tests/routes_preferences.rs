use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{
    VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path, get_root_folder,
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

async fn create_group(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO vault_groups (name) VALUES (?)")
        .bind(name)
        .execute(pool)
        .await
        .expect("create group")
        .last_insert_rowid()
}

async fn insert_versioned_document(pool: &sqlx::SqlitePool, folder_id: i64, name: &str) -> i64 {
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', ?, 3)
        ",
    )
    .bind(format!("preference-test-hash-{name}"))
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
    .expect("document")
    .last_insert_rowid();
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
                created_via
            )
        VALUES
            (?, ?, ?, 1, 'admin', 'Admin', 'Uploaded file', 'text/plain', ?, 'upload')
        ",
    )
    .bind(format!("preference-test-version-{document_id}"))
    .bind(document_id)
    .bind(blob_id)
    .bind(name)
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
    .bind(format!("preference-test-version-{document_id}"))
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

fn authed_get(uri: &str, user: &str, groups: &str) -> Request<Body> {
    authed_request(Method::GET, uri, user, groups, Body::empty())
}

fn authed_patch(uri: &str, user: &str, groups: &str, payload: &Value) -> Request<Body> {
    authed_request(
        Method::PATCH,
        uri,
        user,
        groups,
        Body::from(serde_json::to_vec(&payload).expect("json payload")),
    )
}

fn authed_post(uri: &str, user: &str, groups: &str, payload: &Value) -> Request<Body> {
    authed_request(
        Method::POST,
        uri,
        user,
        groups,
        Body::from(serde_json::to_vec(&payload).expect("json payload")),
    )
}

fn preference_patch(folder_id: i64, document_id: i64) -> Value {
    json!({
        "preferences": {
            "themePreference": "dark",
            "palettePreference": "winui",
            "openFoldersOnClick": false,
            "alternateRows": true,
            "doubleClickDownload": true,
            "favoriteItems": [
                {"type": "folder", "id": folder_id},
                {"type": "document", "id": document_id},
                {"type": "document", "id": document_id}
            ],
            "sidebarSectionSizes": {
                "folders": 240,
                "favorites": 150,
                "archive": 130,
                "editing": 90
            },
            "sidebarSectionCollapsed": {
                "folders": false,
                "favorites": true,
                "archive": false,
                "editing": true
            }
        }
    })
}

fn assert_enriched_preferences(preferences: &Value, folder_id: i64, document_id: i64) {
    let favorites = preferences["favoriteItems"].as_array().expect("favorites");

    assert_eq!(preferences["themePreference"], "dark");
    assert_eq!(preferences["palettePreference"], "winui");
    assert_eq!(preferences["openFoldersOnClick"], false);
    assert_eq!(preferences["alternateRows"], true);
    assert_eq!(preferences["doubleClickDownload"], true);
    assert_eq!(favorites.len(), 2);
    assert_eq!(favorites[0]["type"], "folder");
    assert_eq!(favorites[0]["id"], folder_id);
    assert_eq!(favorites[0]["path"], "Art");
    assert_eq!(favorites[1]["type"], "document");
    assert_eq!(favorites[1]["id"], document_id);
    assert_eq!(favorites[1]["path"], "Art/crate.txt");
    assert_eq!(preferences["sidebarSectionSizes"]["folders"], 240);
    assert_eq!(preferences["sidebarSectionCollapsed"]["favorites"], true);
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
        .header("Content-Type", "application/json")
        .body(body)
        .expect("request")
}

#[tokio::test]
async fn preferences_get_returns_defaults() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);

    let response = app
        .oneshot(authed_get("/api/preferences", "artist", "artists"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["preferences"]["themePreference"], "system");
    assert_eq!(json["preferences"]["palettePreference"], "cozy");
    assert_eq!(json["preferences"]["openFoldersOnClick"], true);
    assert_eq!(json["preferences"]["alternateRows"], false);
    assert_eq!(json["preferences"]["doubleClickDownload"], false);
    assert_eq!(json["preferences"]["favoriteItems"], json!([]));
    assert_eq!(json["preferences"]["sidebarSectionSizes"]["folders"], 180);
    assert_eq!(json["preferences"]["sidebarSectionSizes"]["favorites"], 95);
    assert_eq!(
        json["preferences"]["sidebarSectionCollapsed"]["archive"],
        true
    );
}

#[tokio::test]
async fn preferences_patch_persists_canonical_ids_and_returns_enriched_favorites() {
    let (state, _temp_dir) = test_state().await;
    let artists = create_group(&state.db, "artists").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, artists, true, true, false)
        .await
        .expect("artist root");
    let art = get_or_create_folder_path(&state.db, Some("Art"))
        .await
        .expect("art");
    add_folder_permission(&state.db, art.id, artists, true, true, false)
        .await
        .expect("artist art");
    let document_id = insert_versioned_document(&state.db, art.id, "crate.txt").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let response = app
        .clone()
        .oneshot(authed_patch(
            "/api/preferences",
            "artist",
            "artists",
            &preference_patch(art.id, document_id),
        ))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_enriched_preferences(&json["preferences"], art.id, document_id);

    let stored = sqlx::query_scalar::<_, String>(
        "SELECT preferences FROM vault_users WHERE issuer = 'headers' AND subject = 'artist'",
    )
    .fetch_one(&pool)
    .await
    .expect("stored preferences");
    let stored: Value = serde_json::from_str(&stored).expect("stored json");
    assert_eq!(
        stored["favoriteItems"],
        json!([
            {"type": "folder", "id": art.id},
            {"type": "document", "id": document_id}
        ]),
    );
    assert!(stored["favoriteItems"][0].get("path").is_none());
    let state_event = sqlx::query_as::<_, (String, String)>(
        "SELECT event_type, resources FROM state_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("state event");
    assert_eq!(
        state_event,
        (
            "preferences.update".to_string(),
            "[\"preferences\"]".to_string()
        )
    );

    let response = app
        .oneshot(authed_get("/api/preferences", "artist", "artists"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["preferences"]["favoriteItems"][0]["path"], "Art");
    assert_eq!(
        json["preferences"]["favoriteItems"][1]["path"],
        "Art/crate.txt",
    );
}

#[tokio::test]
async fn preferences_patch_allows_missing_preferences_as_noop() {
    let (state, _temp_dir) = test_state().await;
    let pool = state.db.clone();
    let app = http::router(state);

    let initial = app
        .clone()
        .oneshot(authed_patch(
            "/api/preferences",
            "artist",
            "artists",
            &json!({"preferences": {"themePreference": "light"}}),
        ))
        .await
        .expect("initial response");
    assert_eq!(initial.status(), StatusCode::OK);
    let event_count_after_initial =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
            .fetch_one(&pool)
            .await
            .expect("initial state event count");

    let response = app
        .clone()
        .oneshot(authed_patch(
            "/api/preferences",
            "artist",
            "artists",
            &json!({}),
        ))
        .await
        .expect("noop response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["preferences"]["themePreference"], "light");
    let event_count_after_noop = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
        .fetch_one(&pool)
        .await
        .expect("noop state event count");
    assert_eq!(event_count_after_noop, event_count_after_initial);

    let response = app
        .oneshot(authed_get("/api/preferences", "artist", "artists"))
        .await
        .expect("response");
    let json = response_json(response).await;

    assert_eq!(json["preferences"]["themePreference"], "light");
}

fn assert_favorites_after_rename(
    preferences_json: &Value,
    art_id: i64,
    props_id: i64,
    document_id: i64,
) {
    let favorites = preferences_json["preferences"]["favoriteItems"]
        .as_array()
        .expect("favorites");
    assert_eq!(
        favorites
            .iter()
            .map(|item| (
                item["type"].as_str().expect("type"),
                item["id"].as_i64().expect("id")
            ))
            .collect::<Vec<_>>(),
        vec![
            ("folder", art_id),
            ("folder", props_id),
            ("document", document_id)
        ],
    );
    assert_eq!(favorites[0]["path"], "Assets");
    assert_eq!(favorites[1]["path"], "Assets/Props");
    assert_eq!(favorites[2]["path"], "Assets/Props/crate.fbx");
}

#[tokio::test]
async fn favorites_resolve_current_targets_after_folder_rename() {
    let (state, _temp_dir) = test_state().await;
    let art = get_or_create_folder_path(&state.db, Some("Art"))
        .await
        .expect("art");
    let props = get_or_create_folder_path(&state.db, Some("Art/Props"))
        .await
        .expect("props");
    let document_id = insert_versioned_document(&state.db, props.id, "crate.fbx").await;
    let app = http::router(state);

    let updated = app
        .clone()
        .oneshot(authed_patch(
            "/api/preferences",
            "artist",
            "vault-admin",
            &json!({
                "preferences": {
                    "favoriteItems": [
                        {"type": "folder", "id": art.id},
                        {"type": "folder", "id": props.id},
                        {"type": "document", "id": document_id}
                    ]
                }
            }),
        ))
        .await
        .expect("preferences patch");
    assert_eq!(updated.status(), StatusCode::OK);

    let renamed = app
        .clone()
        .oneshot(authed_post(
            "/api/rename",
            "artist",
            "vault-admin",
            &json!({
                "items": [{"type": "folder", "id": art.id}],
                "destination_folder": "",
                "name": "Assets"
            }),
        ))
        .await
        .expect("rename");
    let renamed_status = renamed.status();
    let renamed_json = response_json(renamed).await;
    assert_eq!(renamed_status, StatusCode::OK);
    assert_eq!(renamed_json["failed"], json!([]));

    let preferences = app
        .clone()
        .oneshot(authed_get("/api/preferences", "artist", "vault-admin"))
        .await
        .expect("preferences");
    let preferences_json = response_json(preferences).await;
    assert_favorites_after_rename(&preferences_json, art.id, props.id, document_id);

    let old_contents = app
        .clone()
        .oneshot(authed_get(
            "/api/folders/contents?folder=Art",
            "artist",
            "vault-admin",
        ))
        .await
        .expect("old contents");
    assert_eq!(old_contents.status(), StatusCode::NOT_FOUND);

    let old_bootstrap = app
        .clone()
        .oneshot(authed_get(
            "/api/bootstrap?folder=Art",
            "artist",
            "vault-admin",
        ))
        .await
        .expect("old bootstrap");
    assert_eq!(old_bootstrap.status(), StatusCode::NOT_FOUND);

    let new_contents = app
        .oneshot(authed_get(
            "/api/folders/contents?folder=Assets/Props",
            "artist",
            "vault-admin",
        ))
        .await
        .expect("new contents");
    let new_status = new_contents.status();
    let new_json = response_json(new_contents).await;
    assert_eq!(new_status, StatusCode::OK);
    assert_eq!(new_json["documents"][0]["name"], "crate.fbx");
}

#[tokio::test]
async fn preferences_filter_folder_favorite_after_current_parent_becomes_inaccessible() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let confidential = create_group(&state.db, "confidential").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let secret = get_or_create_folder_path(&state.db, Some("Secret"))
        .await
        .expect("secret");
    add_folder_permission(&state.db, secret.id, confidential, true, true, true)
        .await
        .expect("confidential secret");
    let pool = state.db.clone();
    let app = http::router(state);

    let initial = app
        .clone()
        .oneshot(authed_patch(
            "/api/preferences",
            "reader",
            "readers",
            &json!({
                "preferences": {
                    "favoriteItems": [{"type": "folder", "id": project.id}]
                }
            }),
        ))
        .await
        .expect("initial preferences");
    let initial_json = response_json(initial).await;
    assert_eq!(
        initial_json["preferences"]["favoriteItems"][0]["path"],
        "Project",
    );

    sqlx::query("UPDATE folders SET parent_id = ? WHERE id = ?")
        .bind(secret.id)
        .bind(project.id)
        .execute(&pool)
        .await
        .expect("move project under confidential parent");

    let response = app
        .oneshot(authed_get("/api/preferences", "reader", "readers"))
        .await
        .expect("preferences");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["preferences"]["favoriteItems"], json!([]));
}

#[tokio::test]
async fn preferences_filter_document_favorite_after_current_folder_becomes_inaccessible() {
    let (state, _temp_dir) = test_state().await;
    let readers = create_group(&state.db, "readers").await;
    let confidential = create_group(&state.db, "confidential").await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    add_folder_permission(&state.db, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    let project = get_or_create_folder_path(&state.db, Some("Project"))
        .await
        .expect("project");
    let secret = get_or_create_folder_path(&state.db, Some("Secret"))
        .await
        .expect("secret");
    add_folder_permission(&state.db, secret.id, confidential, true, true, true)
        .await
        .expect("confidential secret");
    let document_id = insert_versioned_document(&state.db, project.id, "brief.txt").await;
    let pool = state.db.clone();
    let app = http::router(state);

    let initial = app
        .clone()
        .oneshot(authed_patch(
            "/api/preferences",
            "reader",
            "readers",
            &json!({
                "preferences": {
                    "favoriteItems": [{"type": "document", "id": document_id}]
                }
            }),
        ))
        .await
        .expect("initial preferences");
    let initial_json = response_json(initial).await;
    assert_eq!(
        initial_json["preferences"]["favoriteItems"][0]["path"],
        "Project/brief.txt",
    );

    sqlx::query("UPDATE documents SET folder_id = ? WHERE id = ?")
        .bind(secret.id)
        .bind(document_id)
        .execute(&pool)
        .await
        .expect("move document under confidential folder");

    let response = app
        .oneshot(authed_get("/api/preferences", "reader", "readers"))
        .await
        .expect("preferences");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["preferences"]["favoriteItems"], json!([]));
}

#[tokio::test]
async fn preferences_patch_rejects_invalid_payloads_without_changing_existing_values() {
    let (state, _temp_dir) = test_state().await;
    let app = http::router(state);
    let initial = app
        .clone()
        .oneshot(authed_patch(
            "/api/preferences",
            "artist",
            "artists",
            &json!({"preferences": {"themePreference": "light"}}),
        ))
        .await
        .expect("initial response");

    assert_eq!(initial.status(), StatusCode::OK);

    let invalid_cases = [
        (
            json!({"preferences": {"themePreference": "solarized"}}),
            "Invalid theme preference",
        ),
        (
            json!({"preferences": {"sidebarWidth": 320}}),
            "Unknown preference: sidebarWidth",
        ),
        (
            json!({"preferences": {"favoriteItems": "Art"}}),
            "favoriteItems must be a list",
        ),
        (
            json!({"preferences": {"favoriteItems": [{"type": "document", "id": 0}]}}),
            "Favorite document id must be positive",
        ),
        (
            json!({"preferences": {"favoriteItems": [{"type": "folder", "path": "Art"}]}}),
            "Favorite folder id must be an integer",
        ),
        (
            json!({"preferences": {"sidebarSectionSizes": {"folders": "wide"}}}),
            "folders sidebar section size must be numeric",
        ),
        (
            json!({"preferences": {"sidebarSectionCollapsed": {"favorites": "yes"}}}),
            "favorites sidebar collapsed state must be a boolean",
        ),
    ];

    for (payload, detail) in invalid_cases {
        let response = app
            .clone()
            .oneshot(authed_patch(
                "/api/preferences",
                "artist",
                "artists",
                &payload,
            ))
            .await
            .expect("response");
        let status = response.status();
        let json = response_json(response).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["detail"], detail);
    }

    let response = app
        .oneshot(authed_get("/api/preferences", "artist", "artists"))
        .await
        .expect("response");
    let status = response.status();
    let json = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["preferences"]["themePreference"], "light");
    assert!(json["preferences"].get("sidebarWidth").is_none());
}
