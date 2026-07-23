mod support;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use support::migration_fixtures::v2_0_0::{
    ARCHIVE_ROOT_ID, DOCUMENT_ID, EMPTY_FOLDER_ID, FIXTURE_WRITER_USER_ID, Fixture,
    MIGRATION_PREVIEWS_FOLDER_ID, VAULT_ROOT_ID, VISUAL_ASSETS_FOLDER_ID,
};
use tower::ServiceExt;
use vault_server::auth::{AuthSettings, UserContext};
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{resolve_visible_folder_by_id, resolve_visible_folder_by_path};
use vault_server::http::{self, AppState};
use vault_server::storage::LocalBlobStorage;

async fn raw_pool(path: &Path) -> SqlitePool {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .expect("raw SQLite options")
        .create_if_missing(false)
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("raw SQLite pool")
}

fn config(data_dir: &Path, db_path: PathBuf) -> Config {
    Config {
        host: "127.0.0.1".parse().expect("host"),
        port: 0,
        data_dir: data_dir.to_path_buf(),
        db_path: Some(db_path),
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
    }
}

fn fixture_writer() -> UserContext {
    UserContext {
        id: "100".to_string(),
        vault_user_id: FIXTURE_WRITER_USER_ID,
        issuer: "https://fixture.invalid".to_string(),
        subject: "alice".to_string(),
        name: "Alice Fixture".to_string(),
        email: "alice@fixture.invalid".to_string(),
        groups: vec!["Fixture Writers".to_string()],
        is_admin: false,
    }
}

fn authed_request(method: Method, uri: &str, payload: Option<&Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Remote-User", "alice")
        .header("Remote-Name", "Alice Fixture")
        .header("Remote-Email", "alice@fixture.invalid")
        .header("Remote-Groups", "Fixture Writers");
    let body = match payload {
        Some(payload) => {
            builder = builder.header("Content-Type", "application/json");
            Body::from(serde_json::to_vec(payload).expect("JSON payload"))
        }
        None => Body::empty(),
    };
    builder.body(body).expect("request")
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    serde_json::from_slice(&body).expect("JSON response")
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One ordered upgrade canary keeps every mutation and audit assertion together.
async fn derived_v2_0_0_incident_state_upgrade_restores_folder_mutation_routes() {
    let fixture = Fixture::create()
        .await
        .expect("generate pinned v2.0.0 database");
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let update = sqlx::query(
        r"
        UPDATE folders
        SET name = 'Vault'
        WHERE id = ?
          AND root_key = 'vault'
          AND parent_id IS NULL
          AND is_root = 1
          AND name = ''
        ",
    )
    .bind(VAULT_ROOT_ID)
    .execute(&raw)
    .await
    .expect("derive the incident root from the v2.0.0 fixture");
    assert_eq!(update.rows_affected(), 1);
    raw.close().await;

    let pool = db::connect(&db_path)
        .await
        .expect("upgrade the derived incident-state v2.0.0 database");
    let root: (i64, String) =
        sqlx::query_as("SELECT id, name FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("migrated Vault root");
    assert_eq!(root, (VAULT_ROOT_ID, String::new()));

    let writer = fixture_writer();
    let resolved_by_id = resolve_visible_folder_by_id(&pool, MIGRATION_PREVIEWS_FOLDER_ID, &writer)
        .await
        .expect("strict ID resolution after migration")
        .expect("fixture preview folder visible after migration");
    assert_eq!(resolved_by_id.id, MIGRATION_PREVIEWS_FOLDER_ID);
    assert_eq!(resolved_by_id.path, "Visual Assets/Migration Previews");
    let resolved_by_path =
        resolve_visible_folder_by_path(&pool, "Visual Assets/Migration Previews", &writer)
            .await
            .expect("strict path resolution after migration")
            .expect("fixture preview path visible after migration");
    assert_eq!(resolved_by_path, resolved_by_id);

    let auth = AuthSettings {
        header_auth_issuer: "https://fixture.invalid".to_string(),
        ..AuthSettings::default()
    };
    let config = config(fixture.root(), db_path);
    let storage = LocalBlobStorage::new(config.objects_path(), &config.storage_prefix);
    let state = AppState::new(config, auth, pool.clone(), Arc::new(storage));
    let app = http::router(state);

    let move_folder = app
        .clone()
        .oneshot(authed_request(
            Method::POST,
            "/api/move",
            Some(&json!({
                "items": [{"type": "folder", "id": EMPTY_FOLDER_ID}],
                "destination_folder": "Visual Assets"
            })),
        ))
        .await
        .expect("move response");
    assert_eq!(move_folder.status(), StatusCode::OK);
    let move_json = response_json(move_folder).await;
    assert_eq!(move_json["failed"], json!([]));
    assert_eq!(move_json["ok"][0]["item"]["id"], EMPTY_FOLDER_ID);
    assert_eq!(
        move_json["ok"][0]["item"]["path"],
        "Visual Assets/Migration Previews/Disposable Empty Folder"
    );
    assert_eq!(
        move_json["ok"][0]["detail"],
        "Visual Assets/Disposable Empty Folder"
    );

    let rename = app
        .clone()
        .oneshot(authed_request(
            Method::POST,
            "/api/rename",
            Some(&json!({
                "items": [{"type": "folder", "id": VISUAL_ASSETS_FOLDER_ID}],
                "name": "Visual Assets Renamed"
            })),
        ))
        .await
        .expect("rename response");
    assert_eq!(rename.status(), StatusCode::OK);
    let rename_json = response_json(rename).await;
    assert_eq!(rename_json["failed"], json!([]));
    assert_eq!(rename_json["ok"][0]["item"]["id"], VISUAL_ASSETS_FOLDER_ID);
    assert_eq!(rename_json["ok"][0]["item"]["path"], "Visual Assets");
    assert_eq!(rename_json["ok"][0]["detail"], "Visual Assets Renamed");

    let renamed_previews =
        resolve_visible_folder_by_id(&pool, MIGRATION_PREVIEWS_FOLDER_ID, &writer)
            .await
            .expect("strict descendant resolution after parent rename")
            .expect("fixture preview folder remains visible after parent rename");
    assert_eq!(renamed_previews.id, MIGRATION_PREVIEWS_FOLDER_ID);
    assert_eq!(
        renamed_previews.path,
        "Visual Assets Renamed/Migration Previews"
    );

    let delete = app
        .clone()
        .oneshot(authed_request(
            Method::DELETE,
            "/api/folders/12?path=Visual%20Assets%20Renamed%2FDisposable%20Empty%20Folder",
            None,
        ))
        .await
        .expect("delete response");
    assert_eq!(delete.status(), StatusCode::OK);
    assert_eq!(
        response_json(delete).await,
        json!({
            "folder": "Visual Assets Renamed/Disposable Empty Folder",
            "id": EMPTY_FOLDER_ID
        })
    );

    let archive = app
        .oneshot(authed_request(
            Method::POST,
            "/api/archive",
            Some(&json!({
                "items": [{"type": "folder", "id": MIGRATION_PREVIEWS_FOLDER_ID}]
            })),
        ))
        .await
        .expect("archive response");
    assert_eq!(archive.status(), StatusCode::OK);
    let archive_json = response_json(archive).await;
    assert_eq!(archive_json["failed"], json!([]));
    assert_eq!(
        archive_json["ok"][0]["item"]["id"],
        MIGRATION_PREVIEWS_FOLDER_ID
    );
    assert_eq!(
        archive_json["ok"][0]["item"]["path"],
        "Visual Assets Renamed/Migration Previews"
    );
    assert_eq!(archive_json["ok"][0]["detail"], "Archive");

    let preserved_visual_assets: (String, i64) =
        sqlx::query_as("SELECT name, parent_id FROM folders WHERE id = ?")
            .bind(VISUAL_ASSETS_FOLDER_ID)
            .fetch_one(&pool)
            .await
            .expect("renamed fixture parent");
    assert_eq!(
        preserved_visual_assets,
        ("Visual Assets Renamed".to_string(), VAULT_ROOT_ID)
    );
    let removed_folder_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM folders WHERE id IN (?, ?)")
            .bind(MIGRATION_PREVIEWS_FOLDER_ID)
            .bind(EMPTY_FOLDER_ID)
            .fetch_one(&pool)
            .await
            .expect("removed fixture folders");
    assert_eq!(removed_folder_count, 0);

    let archived_document: (i64, String, String) = sqlx::query_as(
        r"
        SELECT folder_id, name, archived_from_folder
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(DOCUMENT_ID)
    .fetch_one(&pool)
    .await
    .expect("archived fixture document");
    assert_eq!(
        archived_document,
        (
            ARCHIVE_ROOT_ID,
            "migration-preview.png".to_string(),
            "Visual Assets Renamed/Migration Previews".to_string(),
        )
    );

    let mutation_events: Vec<String> = sqlx::query_scalar(
        r"
        SELECT event_type
        FROM state_events
        WHERE event_type IN ('batch.move', 'batch.rename', 'folder.deleted', 'batch.archive')
        ORDER BY id
        ",
    )
    .fetch_all(&pool)
    .await
    .expect("mutation state events");
    assert_eq!(
        mutation_events,
        [
            "batch.move",
            "batch.rename",
            "folder.deleted",
            "batch.archive"
        ]
    );
    pool.close().await;
}
