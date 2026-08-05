mod support;

use std::path::Path;
use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use support::migration_fixtures::v2_0_0::{
    ARCHIVE_ROOT_ID, DOCUMENT_ID, Fixture as V2_0_0Fixture, MIGRATION_PREVIEWS_FOLDER_ID,
};
use support::migration_fixtures::v2_1_0::{
    ACTIVE_CREATE_UPLOAD_ID, ARCHIVED_DOCUMENT_ID, COMPLETE_CREATE_UPLOAD_ID,
    COMPLETING_CREATE_UPLOAD_ID, EXISTING_PATH_ARCHIVED_DOCUMENT_ID, Fixture as V2_1_0Fixture,
};
use vault_server::db;

const CURRENT_HISTORY: [(i64, &str); 3] = [
    (1, "content previews"),
    (2, "normalize root folders"),
    (3, "preserve stored item identities"),
];
const BASELINE_TABLES: [&str; 21] = [
    "folders",
    "folder_events",
    "vault_users",
    "vault_groups",
    "vault_group_memberships",
    "folder_permissions",
    "vault_settings",
    "blobs",
    "blob_locations",
    "documents",
    "document_locks",
    "document_versions",
    "document_events",
    "upload_sessions",
    "upload_parts",
    "export_jobs",
    "export_artifacts",
    "state_events",
    "share_links",
    "preview_jobs",
    "preview_renditions",
];
const BASELINE_SNAPSHOT_QUERIES: [(&str, &str); 12] = [
    (
        "folders",
        r"
        SELECT json_array(
            id, root_key, parent_id, name, is_root, created_at, created_by,
            created_by_name, color, icon, default_ttl_days, default_ttl_action
        )
        FROM folders
        ORDER BY id
        ",
    ),
    (
        "folder_events",
        r"
        SELECT json_array(
            id, folder_id, event_type, actor, actor_name, message, created_at
        )
        FROM folder_events
        ORDER BY id
        ",
    ),
    (
        "vault_users",
        r"
        SELECT json_array(
            id, issuer, subject, email, name, is_admin, is_active, preferences,
            created_at, last_login_at, last_seen_at
        )
        FROM vault_users
        ORDER BY id
        ",
    ),
    (
        "vault_groups",
        r"
        SELECT json_array(id, name, description, created_at)
        FROM vault_groups
        ORDER BY id
        ",
    ),
    (
        "vault_group_memberships",
        r"
        SELECT json_array(id, user_id, group_id, created_at)
        FROM vault_group_memberships
        ORDER BY id
        ",
    ),
    (
        "folder_permissions",
        r"
        SELECT json_array(
            id, folder_id, group_id, can_view, can_read, can_write, created_at,
            updated_at
        )
        FROM folder_permissions
        ORDER BY id
        ",
    ),
    (
        "blobs",
        r"
        SELECT json_array(id, hash_algo, hash, size_bytes, created_at)
        FROM blobs
        ORDER BY id
        ",
    ),
    (
        "blob_locations",
        r"
        SELECT json_array(id, blob_id, backend, bucket, object_key, created_at)
        FROM blob_locations
        ORDER BY id
        ",
    ),
    (
        "documents",
        r"
        SELECT json_array(
            id, folder_id, name, description, created_at, created_by,
            created_by_name, latest_modified_at, latest_modified_by,
            latest_version_number, version_count, current_version_id, expires_at,
            expiry_action
        )
        FROM documents
        ORDER BY id
        ",
    ),
    (
        "document_versions",
        r"
        SELECT json_array(
            id, document_id, blob_id, version_number, committed_at, committed_by,
            committed_by_name, message, mime_type, original_filename, upload_ip,
            upload_user_agent, created_via
        )
        FROM document_versions
        ORDER BY id
        ",
    ),
    (
        "document_events",
        r"
        SELECT json_array(
            id, document_id, event_type, created_at, actor, actor_name, message,
            result, ip, user_agent
        )
        FROM document_events
        ORDER BY id
        ",
    ),
    (
        "state_events",
        r"
        SELECT json_array(id, event_type, resources, created_at)
        FROM state_events
        ORDER BY id
        ",
    ),
];

type FolderRow = (i64, String, Option<i64>, String, i64);
type MigrationRow = (i64, String, String);
type BaselineDataSnapshot = Vec<(&'static str, Vec<String>)>;
type PreviewDataSnapshot = (Vec<String>, Vec<String>);
type ArchiveRelatedSnapshot = Vec<(&'static str, Vec<String>)>;

struct ArchiveSnapshot {
    missing_path_document: String,
    existing_path_document: String,
    missing_path_related: ArchiveRelatedSnapshot,
    existing_path_related: ArchiveRelatedSnapshot,
    state_events: Vec<String>,
}

async fn v2_0_0_fixture() -> V2_0_0Fixture {
    V2_0_0Fixture::create()
        .await
        .expect("generate pinned v2.0.0 database")
}

async fn v2_1_0_fixture() -> V2_1_0Fixture {
    V2_1_0Fixture::create()
        .await
        .expect("generate pinned v2.1.0 database")
}

async fn raw_pool(path: &Path) -> SqlitePool {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .expect("raw SQLite options")
        .create_if_missing(false)
        .foreign_keys(false);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("raw SQLite pool")
}

async fn migration_history(pool: &SqlitePool) -> Vec<MigrationRow> {
    sqlx::query_as(
        r"
        SELECT version, name, applied_at
        FROM schema_migrations
        ORDER BY version
        ",
    )
    .fetch_all(pool)
    .await
    .expect("migration history")
}

async fn baseline_row_counts(pool: &SqlitePool) -> Vec<(&'static str, i64)> {
    let mut counts = Vec::with_capacity(BASELINE_TABLES.len());
    for table in BASELINE_TABLES {
        let count = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap_or_else(|error| panic!("count fixture table {table}: {error}"));
        counts.push((table, count));
    }
    counts
}

async fn baseline_data_snapshot(pool: &SqlitePool) -> BaselineDataSnapshot {
    let mut snapshot = Vec::with_capacity(BASELINE_SNAPSHOT_QUERIES.len());
    for (table, query) in BASELINE_SNAPSHOT_QUERIES {
        let rows = sqlx::query_scalar(query)
            .fetch_all(pool)
            .await
            .unwrap_or_else(|error| panic!("snapshot fixture table {table}: {error}"));
        snapshot.push((table, rows));
    }
    snapshot
}

async fn preview_data_snapshot(pool: &SqlitePool) -> PreviewDataSnapshot {
    let jobs = sqlx::query_scalar(
        r"
        SELECT json_array(
            id, source_blob_id, recipe, status, attempt_count, lease_token,
            lease_expires_at, next_attempt_at, last_error_code, last_error_detail,
            created_at, updated_at, completed_at, last_accessed_at
        )
        FROM preview_jobs
        ORDER BY id
        ",
    )
    .fetch_all(pool)
    .await
    .expect("preview job snapshot");
    let renditions = sqlx::query_scalar(
        r"
        SELECT json_array(
            id, preview_job_id, variant, blob_id, mime_type, width, height,
            created_at
        )
        FROM preview_renditions
        ORDER BY id
        ",
    )
    .fetch_all(pool)
    .await
    .expect("preview rendition snapshot");
    (jobs, renditions)
}

async fn upload_session_relationship_snapshot(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(
        r"
        SELECT json_array(
            id, mode, folder_path, document_id, filename, total_size, chunk_size,
            part_count, mime_type, note, rename_to_upload, created_by,
            created_by_name, user_context, upload_ip, upload_user_agent,
            created_at, expires_at, completed_at, aborted_at,
            result_document_id, result_version_id, result_path
        )
        FROM upload_sessions
        ORDER BY id
        ",
    )
    .fetch_all(pool)
    .await
    .expect("upload session relationship snapshot")
}

async fn upload_parts_snapshot(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(
        r"
        SELECT json_array(
            id, session_id, part_number, offset_bytes, size_bytes, sha256,
            storage_path, created_at
        )
        FROM upload_parts
        ORDER BY id
        ",
    )
    .fetch_all(pool)
    .await
    .expect("upload parts snapshot")
}

async fn upload_state_snapshot(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(
        r"
        SELECT json_array(
            id, status, target_folder_id, verification_total_bytes,
            verification_processed_bytes, document_id, error,
            result_document_id, result_version_id, result_path
        )
        FROM upload_sessions
        ORDER BY id
        ",
    )
    .fetch_all(pool)
    .await
    .expect("upload state snapshot")
}

async fn upload_updated_at_snapshot(pool: &SqlitePool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT id, updated_at FROM upload_sessions ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("upload updated-at snapshot")
}

async fn document_locks_snapshot(pool: &SqlitePool, document_id: i64) -> Vec<String> {
    sqlx::query_scalar(
        r"
        SELECT json_array(
            id, document_id, locked_by, locked_by_name, locked_at, is_active,
            locked_ip, locked_user_agent, force_acquired, released_at,
            released_by
        )
        FROM document_locks
        WHERE document_id = ?
        ORDER BY id
        ",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await
    .expect("document lock snapshot")
}

async fn archived_document_stable_snapshot(pool: &SqlitePool, document_id: i64) -> String {
    sqlx::query_scalar(
        r"
        SELECT json_array(
            id, description, created_at, created_by, created_by_name,
            latest_modified_at, latest_modified_by, latest_version_number,
            version_count, current_version_id, expires_at, expiry_action,
            archived_access
        )
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await
    .expect("archived document stable snapshot")
}

async fn archive_related_snapshot(pool: &SqlitePool, document_id: i64) -> ArchiveRelatedSnapshot {
    const QUERIES: [(&str, &str); 6] = [
        (
            "document_versions",
            r"
            SELECT json_array(
                id, document_id, blob_id, version_number, committed_at,
                committed_by, committed_by_name, message, mime_type,
                original_filename, upload_ip, upload_user_agent, created_via
            )
            FROM document_versions
            WHERE document_id = ?
            ORDER BY version_number
            ",
        ),
        (
            "blobs",
            r"
            SELECT json_array(b.id, b.hash_algo, b.hash, b.size_bytes, b.created_at)
            FROM blobs b
            WHERE b.id IN (
                SELECT blob_id FROM document_versions WHERE document_id = ?
            )
            ORDER BY b.id
            ",
        ),
        (
            "blob_locations",
            r"
            SELECT json_array(
                l.id, l.blob_id, l.backend, l.bucket, l.object_key, l.created_at
            )
            FROM blob_locations l
            WHERE l.blob_id IN (
                SELECT blob_id FROM document_versions WHERE document_id = ?
            )
            ORDER BY l.id
            ",
        ),
        (
            "document_events",
            r"
            SELECT json_array(
                id, document_id, event_type, created_at, actor, actor_name,
                message, result, ip, user_agent
            )
            FROM document_events
            WHERE document_id = ?
            ORDER BY id
            ",
        ),
        (
            "document_locks",
            r"
            SELECT json_array(
                id, document_id, locked_by, locked_by_name, locked_at,
                is_active, locked_ip, locked_user_agent, force_acquired,
                released_at, released_by
            )
            FROM document_locks
            WHERE document_id = ?
            ORDER BY id
            ",
        ),
        (
            "share_links",
            r"
            SELECT json_array(
                id, code, target_type, document_id, folder_id, access_mode,
                created_by, created_by_name, created_by_user_id, created_at,
                expires_at, disabled_at, item_type, item_id
            )
            FROM share_links
            WHERE document_id = ?
            ORDER BY id
            ",
        ),
    ];

    let mut snapshot = Vec::with_capacity(QUERIES.len());
    for (table, query) in QUERIES {
        let rows = sqlx::query_scalar(query)
            .bind(document_id)
            .fetch_all(pool)
            .await
            .unwrap_or_else(|error| panic!("snapshot archived document table {table}: {error}"));
        snapshot.push((table, rows));
    }
    snapshot
}

async fn archive_state_event_snapshot(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(
        r"
        SELECT json_array(id, event_type, resources, created_at)
        FROM state_events
        WHERE event_type = 'batch.archive'
        ORDER BY id
        ",
    )
    .fetch_all(pool)
    .await
    .expect("archive state event snapshot")
}

fn assert_current_history(rows: &[MigrationRow]) {
    let actual = rows
        .iter()
        .map(|(version, name, _)| (*version, name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(actual, CURRENT_HISTORY);
}

async fn startup_error(path: &Path) -> String {
    match db::connect(path).await {
        Ok(pool) => {
            pool.close().await;
            panic!("database startup unexpectedly succeeded");
        }
        Err(error) => format!("{error:#}"),
    }
}

async fn set_legacy_vault_root(pool: &SqlitePool) -> i64 {
    let root_id: i64 = sqlx::query_scalar(
        r"
        SELECT id
        FROM folders
        WHERE root_key = 'vault'
          AND is_root = 1
          AND parent_id IS NULL
        ",
    )
    .fetch_one(pool)
    .await
    .expect("vault root");
    let result = sqlx::query("UPDATE folders SET name = 'Vault' WHERE id = ?")
        .bind(root_id)
        .execute(pool)
        .await
        .expect("derive legacy root representation");
    assert_eq!(result.rows_affected(), 1);
    root_id
}

#[tokio::test]
async fn exact_v2_0_0_fixture_upgrades_to_current_without_changing_baseline_data() {
    /*
     * Snapshots the released v2.0.0 fixture's ledger, rows, preview data, and folder hierarchy
     * before opening it with the current server. It checks pending migrations append the
     * exact known history without changing released data or root identities and leave no
     * foreign-key violations.
     */
    let fixture = v2_0_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let history_before = migration_history(&raw).await;
    assert_eq!(history_before.len(), 1);
    assert_eq!(
        (history_before[0].0, history_before[0].1.as_str()),
        CURRENT_HISTORY[0]
    );
    let row_counts_before = baseline_row_counts(&raw).await;
    let data_before = baseline_data_snapshot(&raw).await;
    let previews_before = preview_data_snapshot(&raw).await;
    assert_eq!(previews_before.0.len(), 1);
    assert_eq!(previews_before.1.len(), 3);

    let folders_before: Vec<FolderRow> =
        sqlx::query_as("SELECT id, root_key, parent_id, name, is_root FROM folders ORDER BY id")
            .fetch_all(&raw)
            .await
            .expect("v2.0.0 folders");
    let canonical_vault_roots = folders_before
        .iter()
        .filter(|(_, root_key, parent_id, name, is_root)| {
            root_key == "vault" && parent_id.is_none() && name.is_empty() && *is_root == 1
        })
        .count();
    assert_eq!(
        canonical_vault_roots, 1,
        "the exact v2.0.0 fixture must retain its released root representation"
    );
    raw.close().await;

    let pool = db::connect(&db_path).await.expect("upgrade v2.0.0 fixture");
    let history_after = migration_history(&pool).await;
    assert_current_history(&history_after);
    assert_eq!(
        history_after[0], history_before[0],
        "the released v2.0.0 baseline ledger row must be preserved exactly"
    );
    assert_eq!(baseline_row_counts(&pool).await, row_counts_before);
    assert_eq!(baseline_data_snapshot(&pool).await, data_before);
    assert_eq!(preview_data_snapshot(&pool).await, previews_before);

    let folders_after: Vec<FolderRow> =
        sqlx::query_as("SELECT id, root_key, parent_id, name, is_root FROM folders ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("migrated folders");
    assert_eq!(folders_after, folders_before);

    let foreign_key_violations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&pool)
            .await
            .expect("foreign-key check");
    assert_eq!(foreign_key_violations, 0);
    pool.close().await;
}

async fn assert_upload_target_schema(pool: &SqlitePool) {
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('upload_sessions')")
            .fetch_all(pool)
            .await
            .expect("upload session columns");
    assert!(columns.iter().any(|column| column == "target_folder_id"));
    let target_foreign_key: (String, String, String) = sqlx::query_as(
        r#"
        SELECT "table", "from", on_delete
        FROM pragma_foreign_key_list('upload_sessions')
        WHERE "from" = 'target_folder_id'
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("target folder foreign key");
    assert_eq!(
        target_foreign_key,
        (
            "folders".to_string(),
            "target_folder_id".to_string(),
            "SET NULL".to_string(),
        )
    );
}

async fn assert_migrated_upload_states(pool: &SqlitePool) {
    assert_eq!(
        upload_state_snapshot(pool).await,
        vec![
            r#"["v2-1-active-checkin","active",null,0,0,1000,null,null,null,null]"#.to_string(),
            concat!(
                r#"["v2-1-active-create","failed",null,0,0,null,"#,
                r#""Upload target identity is unavailable after upgrade; restart the upload","#,
                "null,null,null]"
            )
            .to_string(),
            concat!(
                r#"["v2-1-complete-create","complete",null,143,143,null,null,1000,"#,
                r#""00000000-0000-7000-8000-000000000001","#,
                r#""Visual Assets/Migration Previews/migration-preview.png"]"#
            )
            .to_string(),
            r#"["v2-1-completing-checkin","completing",null,4,2,1000,null,null,null,null]"#
                .to_string(),
            concat!(
                r#"["v2-1-completing-create","failed",null,0,0,null,"#,
                r#""Upload target identity is unavailable after upgrade; restart the upload","#,
                "null,null,null]"
            )
            .to_string(),
        ]
    );
}

async fn assert_upload_relationship_integrity(pool: &SqlitePool) {
    let completed_result_resolves: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM upload_sessions s
        JOIN documents d ON d.id = s.result_document_id
        JOIN document_versions v
          ON v.id = s.result_version_id
         AND v.document_id = d.id
        WHERE s.id = ?
        ",
    )
    .bind(COMPLETE_CREATE_UPLOAD_ID)
    .fetch_one(pool)
    .await
    .expect("completed upload result identities");
    assert_eq!(completed_result_resolves, 1);

    let orphaned_parts: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM upload_parts p
        LEFT JOIN upload_sessions s ON s.id = p.session_id
        WHERE s.id IS NULL
        ",
    )
    .fetch_one(pool)
    .await
    .expect("orphaned upload parts");
    assert_eq!(orphaned_parts, 0);
    let foreign_key_violations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(pool)
            .await
            .expect("foreign-key check");
    assert_eq!(foreign_key_violations, 0);
}

async fn assert_upload_timestamp_changes(pool: &SqlitePool, before: &[(String, String)]) {
    let after = upload_updated_at_snapshot(pool).await;
    assert_eq!(after.len(), before.len());
    for ((before_id, before_value), (after_id, after_value)) in before.iter().zip(&after) {
        assert_eq!(after_id, before_id);
        if before_id == ACTIVE_CREATE_UPLOAD_ID || before_id == COMPLETING_CREATE_UPLOAD_ID {
            assert_ne!(after_value, before_value);
        } else {
            assert_eq!(after_value, before_value);
        }
    }
}

async fn assert_active_create_requires_target(pool: &SqlitePool) {
    let missing_target = sqlx::query(
        r"
        INSERT INTO upload_sessions (
            id, mode, status, folder_path, filename, total_size, chunk_size,
            part_count, created_by, user_context, expires_at
        )
        VALUES (
            'missing-target', 'create', 'active', 'Visual Assets',
            'missing.txt', 1, 1, 1, 'fixture:alice', '{}',
            '2999-01-01T00:00:00Z'
        )
        ",
    )
    .execute(pool)
    .await
    .expect_err("active create upload without target identity must be rejected");
    assert!(
        missing_target
            .to_string()
            .contains("active create upload requires a target folder identity")
    );
}

#[tokio::test]
async fn v2_1_0_upgrade_invalidates_ambiguous_create_uploads_and_preserves_checkins() {
    /*
     * Upgrades active and completing upload sessions, a completed result, schema-valid legacy
     * upload-part sentinels, and a check-in lock. It verifies that only ambiguous in-flight
     * creates are invalidated and all database relationships remain coherent.
     */
    let fixture = v2_1_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let history_before = migration_history(&raw).await;
    assert_eq!(history_before.len(), 1);
    assert_eq!(
        (history_before[0].0, history_before[0].1.as_str()),
        CURRENT_HISTORY[0]
    );
    let upload_relationships_before = upload_session_relationship_snapshot(&raw).await;
    let upload_parts_before = upload_parts_snapshot(&raw).await;
    let upload_updated_at_before = upload_updated_at_snapshot(&raw).await;
    let checkin_locks_before = document_locks_snapshot(&raw, DOCUMENT_ID).await;
    assert_eq!(upload_relationships_before.len(), 5);
    assert_eq!(upload_parts_before.len(), 4);
    assert_eq!(checkin_locks_before.len(), 1);
    raw.close().await;

    let pool = db::connect(&db_path).await.expect("upgrade v2.1.0 fixture");
    let history_after = migration_history(&pool).await;
    assert_current_history(&history_after);
    assert_eq!(history_after[0], history_before[0]);
    assert_eq!(
        upload_session_relationship_snapshot(&pool).await,
        upload_relationships_before
    );
    assert_eq!(upload_parts_snapshot(&pool).await, upload_parts_before);
    assert_eq!(
        document_locks_snapshot(&pool, DOCUMENT_ID).await,
        checkin_locks_before
    );
    assert_upload_timestamp_changes(&pool, &upload_updated_at_before).await;
    assert_upload_target_schema(&pool).await;
    assert_migrated_upload_states(&pool).await;
    assert_upload_relationship_integrity(&pool).await;
    assert_active_create_requires_target(&pool).await;
    pool.close().await;
}

async fn capture_archive_snapshot(pool: &SqlitePool) -> ArchiveSnapshot {
    let missing_path_related = archive_related_snapshot(pool, ARCHIVED_DOCUMENT_ID).await;
    let existing_path_related =
        archive_related_snapshot(pool, EXISTING_PATH_ARCHIVED_DOCUMENT_ID).await;
    assert_eq!(
        missing_path_related
            .iter()
            .map(|(_, rows)| rows.len())
            .collect::<Vec<_>>(),
        vec![2, 2, 2, 3, 1, 1]
    );
    assert_eq!(
        existing_path_related
            .iter()
            .map(|(_, rows)| rows.len())
            .collect::<Vec<_>>(),
        vec![1, 1, 1, 1, 0, 0]
    );
    let state_events = archive_state_event_snapshot(pool).await;
    assert_eq!(state_events.len(), 2);
    ArchiveSnapshot {
        missing_path_document: archived_document_stable_snapshot(pool, ARCHIVED_DOCUMENT_ID).await,
        existing_path_document: archived_document_stable_snapshot(
            pool,
            EXISTING_PATH_ARCHIVED_DOCUMENT_ID,
        )
        .await,
        missing_path_related,
        existing_path_related,
        state_events,
    }
}

async fn assert_released_v2_1_archive_source(pool: &SqlitePool) {
    let legacy_archives: Vec<(i64, i64, String, String, String)> = sqlx::query_as(
        r"
        SELECT
            id, folder_id, name, archived_from_folder, archived_original_name
        FROM documents
        WHERE id IN (?, ?)
        ORDER BY id
        ",
    )
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(EXISTING_PATH_ARCHIVED_DOCUMENT_ID)
    .fetch_all(pool)
    .await
    .expect("released v2.1 archive rows");
    assert_eq!(
        legacy_archives,
        vec![
            (
                ARCHIVED_DOCUMENT_ID,
                ARCHIVE_ROOT_ID,
                "payload.bin".to_string(),
                "Projects/Incoming".to_string(),
                "payload.bin".to_string(),
            ),
            (
                EXISTING_PATH_ARCHIVED_DOCUMENT_ID,
                ARCHIVE_ROOT_ID,
                "existing-path.bin".to_string(),
                "Visual Assets/Migration Previews".to_string(),
                "existing-path.bin".to_string(),
            ),
        ]
    );
}

async fn assert_archive_snapshot_preserved(pool: &SqlitePool, before: &ArchiveSnapshot) {
    assert_eq!(
        archived_document_stable_snapshot(pool, ARCHIVED_DOCUMENT_ID).await,
        before.missing_path_document
    );
    assert_eq!(
        archived_document_stable_snapshot(pool, EXISTING_PATH_ARCHIVED_DOCUMENT_ID).await,
        before.existing_path_document
    );
    assert_eq!(
        archive_related_snapshot(pool, ARCHIVED_DOCUMENT_ID).await,
        before.missing_path_related
    );
    assert_eq!(
        archive_related_snapshot(pool, EXISTING_PATH_ARCHIVED_DOCUMENT_ID).await,
        before.existing_path_related
    );
    assert_eq!(
        archive_state_event_snapshot(pool).await,
        before.state_events
    );
}

async fn assert_reconstructed_archive_migrated(pool: &SqlitePool) {
    let migrated_missing_path: (String, String, String, String, String, String, String) =
        sqlx::query_as(
            r"
        SELECT
            f.root_key,
            parent.name,
            f.name,
            d.name,
            d.archived_at,
            d.archived_origin_path,
            d.archived_access
        FROM documents d
        JOIN folders f ON f.id = d.folder_id
        JOIN folders parent ON parent.id = f.parent_id
        WHERE d.id = ?
        ",
        )
        .bind(ARCHIVED_DOCUMENT_ID)
        .fetch_one(pool)
        .await
        .expect("migrated archive with reconstructed path");
    assert_eq!(
        migrated_missing_path,
        (
            "vault".to_string(),
            "Projects".to_string(),
            "Incoming".to_string(),
            "payload.bin".to_string(),
            "2026-07-22T22:35:00Z".to_string(),
            "Projects/Incoming/payload.bin".to_string(),
            r#"{"200":3,"201":2}"#.to_string(),
        )
    );
}

async fn assert_existing_path_archive_migrated(pool: &SqlitePool) {
    let migrated_existing_path: (i64, String, String, String, String, String) = sqlx::query_as(
        r"
        SELECT
            d.folder_id,
            d.name,
            d.archived_at,
            d.archived_origin_path,
            d.archived_access,
            f.root_key
        FROM documents d
        JOIN folders f ON f.id = d.folder_id
        WHERE d.id = ?
        ",
    )
    .bind(EXISTING_PATH_ARCHIVED_DOCUMENT_ID)
    .fetch_one(pool)
    .await
    .expect("migrated archive with existing path");
    assert_eq!(
        migrated_existing_path,
        (
            MIGRATION_PREVIEWS_FOLDER_ID,
            "existing-path.bin".to_string(),
            "2026-07-22T22:36:00Z".to_string(),
            "Visual Assets/Migration Previews/existing-path.bin".to_string(),
            r#"{"200":3,"201":2}"#.to_string(),
            "vault".to_string(),
        )
    );
}

async fn assert_archive_relationship_integrity(pool: &SqlitePool) {
    let resolved_current_versions: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM documents d
        JOIN document_versions v
          ON v.id = d.current_version_id
         AND v.document_id = d.id
        WHERE d.id IN (?, ?)
        ",
    )
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(EXISTING_PATH_ARCHIVED_DOCUMENT_ID)
    .fetch_one(pool)
    .await
    .expect("archived current-version identities");
    assert_eq!(resolved_current_versions, 2);

    let valid_share_identity: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM share_links
        WHERE document_id = ?
          AND item_type = 'document'
          AND item_id = document_id
        ",
    )
    .bind(ARCHIVED_DOCUMENT_ID)
    .fetch_one(pool)
    .await
    .expect("archived document share identity");
    assert_eq!(valid_share_identity, 1);

    let physical_archive_documents: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM documents d
        JOIN folders f ON f.id = d.folder_id
        WHERE f.root_key = 'archive'
        ",
    )
    .fetch_one(pool)
    .await
    .expect("physical archive documents");
    assert_eq!(physical_archive_documents, 0);

    let reconstructed_paths: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM folders child
        JOIN folders parent ON parent.id = child.parent_id
        JOIN folders root ON root.id = parent.parent_id
        WHERE child.name = 'Incoming'
          AND parent.name = 'Projects'
          AND root.root_key = 'vault'
          AND root.is_root = 1
        ",
    )
    .fetch_one(pool)
    .await
    .expect("reconstructed archive paths");
    assert_eq!(reconstructed_paths, 1);
    let foreign_key_violations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(pool)
            .await
            .expect("foreign-key check");
    assert_eq!(foreign_key_violations, 0);
}

async fn assert_archive_schema(pool: &SqlitePool) {
    let document_columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('documents')")
            .fetch_all(pool)
            .await
            .expect("document columns");
    assert!(
        !document_columns
            .iter()
            .any(|name| name == "archived_from_folder")
    );
    assert!(
        !document_columns
            .iter()
            .any(|name| name == "archived_original_name")
    );
    assert!(document_columns.iter().any(|name| name == "archived_at"));
    assert!(
        document_columns
            .iter()
            .any(|name| name == "archived_origin_path")
    );
}

#[tokio::test]
async fn archive_identity_migration_preserves_released_v2_1_document_graphs() {
    /*
     * Upgrades two production-shaped 2.1 archives: one whose nested origin path was deleted and
     * one whose origin still exists. It verifies transformed archive metadata while preserving
     * document identities, versions, blobs, locations, events, locks, shares, and state events.
     */
    let fixture = v2_1_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let history_before = migration_history(&raw).await;
    assert_eq!(history_before.len(), 1);
    assert_eq!(
        (history_before[0].0, history_before[0].1.as_str()),
        CURRENT_HISTORY[0]
    );
    let archive_before = capture_archive_snapshot(&raw).await;
    assert_released_v2_1_archive_source(&raw).await;
    raw.close().await;

    let pool = db::connect(&db_path)
        .await
        .expect("upgrade released v2.1 archive graphs");
    let history_after = migration_history(&pool).await;
    assert_current_history(&history_after);
    assert_eq!(history_after[0], history_before[0]);
    assert_archive_snapshot_preserved(&pool, &archive_before).await;
    assert_reconstructed_archive_migrated(&pool).await;
    assert_existing_path_archive_migrated(&pool).await;
    assert_archive_relationship_integrity(&pool).await;
    assert_archive_schema(&pool).await;
    pool.close().await;
}

#[tokio::test]
async fn derived_v2_0_0_incident_state_normalizes_legacy_root_without_replacing_descendants() {
    /*
     * Derives the historical named Vault-root incident from the v2.0.0 fixture and adds a child
     * tied to that root ID. It checks migration normalizes the existing root in place, preserves
     * its descendant and unrelated baseline data, and produces exactly one canonical Vault root.
     */
    let fixture = v2_0_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let history_before = migration_history(&raw).await;
    let data_before = baseline_data_snapshot(&raw).await;
    let previews_before = preview_data_snapshot(&raw).await;
    let vault_root_id = set_legacy_vault_root(&raw).await;
    let descendant_id: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) + 1000 FROM folders")
        .fetch_one(&raw)
        .await
        .expect("descendant id");
    sqlx::query(
        r"
        INSERT INTO folders (id, root_key, parent_id, name, is_root)
        VALUES (?, 'vault', ?, 'Migration incident descendant', 0)
        ",
    )
    .bind(descendant_id)
    .bind(vault_root_id)
    .execute(&raw)
    .await
    .expect("incident descendant");
    raw.close().await;

    let pool = db::connect(&db_path)
        .await
        .expect("upgrade derived legacy-root v2.0.0 database");
    let history_after = migration_history(&pool).await;
    assert_current_history(&history_after);
    assert_eq!(history_after[0], history_before[0]);
    assert_eq!(preview_data_snapshot(&pool).await, previews_before);

    let root: FolderRow = sqlx::query_as(
        r"
        SELECT id, root_key, parent_id, name, is_root
        FROM folders
        WHERE id = ?
        ",
    )
    .bind(vault_root_id)
    .fetch_one(&pool)
    .await
    .expect("normalized vault root");
    assert_eq!(
        root,
        (vault_root_id, "vault".to_string(), None, String::new(), 1,)
    );

    let descendant: FolderRow = sqlx::query_as(
        r"
        SELECT id, root_key, parent_id, name, is_root
        FROM folders
        WHERE id = ?
        ",
    )
    .bind(descendant_id)
    .fetch_one(&pool)
    .await
    .expect("preserved descendant");
    assert_eq!(
        descendant,
        (
            descendant_id,
            "vault".to_string(),
            Some(vault_root_id),
            "Migration incident descendant".to_string(),
            0,
        )
    );

    let strict_root_matches: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM folders
        WHERE root_key = 'vault'
          AND is_root = 1
          AND parent_id IS NULL
          AND name = ''
        ",
    )
    .fetch_one(&pool)
    .await
    .expect("strict root matches");
    assert_eq!(strict_root_matches, 1);
    let data_after = baseline_data_snapshot(&pool).await;
    for ((table_before, rows_before), (table_after, rows_after)) in
        data_before.iter().zip(&data_after)
    {
        assert_eq!(table_after, table_before);
        if *table_before == "folders" {
            continue;
        }
        assert_eq!(
            rows_after, rows_before,
            "migration unexpectedly changed {table_before}"
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn migration_two_rolls_back_root_normalization_and_ledger_on_final_validation_failure() {
    /*
     * Combines the legacy root representation with a preexisting orphan event that will fail
     * final foreign-key validation. It checks the entire migration transaction rolls back:
     * the root name, ledger, preview data, orphan row, and original violation all remain
     * exactly as they were.
     */
    let fixture = v2_0_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let vault_root_id = set_legacy_vault_root(&raw).await;
    let orphan_event_id: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) + 1000 FROM folder_events")
            .fetch_one(&raw)
            .await
            .expect("orphan event id");
    sqlx::query(
        r"
        INSERT INTO folder_events (
            id, folder_id, event_type, actor, actor_name, message, created_at
        )
        VALUES (
            ?, 9223372036854770000, 'migration.rollback', 'fixture:alice',
            'Alice Fixture', 'Must survive rejected migration',
            '2000-01-01 00:00:00'
        )
        ",
    )
    .bind(orphan_event_id)
    .execute(&raw)
    .await
    .expect("foreign-key violation");
    let history_before = migration_history(&raw).await;
    assert_eq!(history_before.len(), 1);
    let previews_before = preview_data_snapshot(&raw).await;
    raw.close().await;

    let error = startup_error(&db_path).await;
    assert!(
        error.contains("foreign-key validation found 1 violations"),
        "unexpected final-validation error: {error}"
    );

    let raw = raw_pool(&db_path).await;
    assert_eq!(migration_history(&raw).await, history_before);
    assert_eq!(preview_data_snapshot(&raw).await, previews_before);
    let vault_root_name: String = sqlx::query_scalar("SELECT name FROM folders WHERE id = ?")
        .bind(vault_root_id)
        .fetch_one(&raw)
        .await
        .expect("rolled-back vault root");
    assert_eq!(vault_root_name, "Vault");
    let orphan_event: (i64, i64, String) = sqlx::query_as(
        r"
        SELECT id, folder_id, message
        FROM folder_events
        WHERE id = ?
        ",
    )
    .bind(orphan_event_id)
    .fetch_one(&raw)
    .await
    .expect("original orphan event");
    assert_eq!(
        orphan_event,
        (
            orphan_event_id,
            9_223_372_036_854_770_000,
            "Must survive rejected migration".to_string(),
        )
    );
    let foreign_key_violations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&raw)
            .await
            .expect("foreign-key violations after rollback");
    assert_eq!(foreign_key_violations, 1);
    raw.close().await;
}

#[derive(Debug, Clone, Copy)]
enum InvalidHistory {
    Empty,
    Gap,
    Changed,
    Future,
}

impl InvalidHistory {
    const fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Gap => "gap",
            Self::Changed => "changed",
            Self::Future => "future",
        }
    }

    const fn expected_error(self) -> &'static str {
        match self {
            Self::Empty => "required Vault 2.0.0 baseline entry",
            Self::Gap | Self::Changed => "unsupported migration history entry",
            Self::Future => "this app knows only",
        }
    }
}

#[tokio::test]
async fn migration_history_must_be_an_exact_known_prefix() {
    /*
     * Restarts fixtures whose migration history is empty, has a gap, changes a known name, or
     * contains a future entry. It checks each unsupported ledger shape gets its specific refusal
     * and that failed startup never rewrites the supplied history.
     */
    for invalid_history in [
        InvalidHistory::Empty,
        InvalidHistory::Gap,
        InvalidHistory::Changed,
        InvalidHistory::Future,
    ] {
        let fixture = v2_0_0_fixture().await;
        let db_path = fixture.db_path().to_path_buf();
        let pool = db::connect(&db_path)
            .await
            .expect("prepare current database");
        pool.close().await;

        let raw = raw_pool(&db_path).await;
        match invalid_history {
            InvalidHistory::Empty => {
                sqlx::query("DELETE FROM schema_migrations")
                    .execute(&raw)
                    .await
                    .expect("empty history");
            }
            InvalidHistory::Gap => {
                sqlx::query("DELETE FROM schema_migrations WHERE version = 1")
                    .execute(&raw)
                    .await
                    .expect("gap history");
            }
            InvalidHistory::Changed => {
                sqlx::query(
                    "UPDATE schema_migrations SET name = 'changed migration' WHERE version = 1",
                )
                .execute(&raw)
                .await
                .expect("changed history");
            }
            InvalidHistory::Future => {
                sqlx::query(
                    "INSERT INTO schema_migrations (version, name) VALUES (5, 'future migration')",
                )
                .execute(&raw)
                .await
                .expect("future history");
            }
        }
        let history_before = migration_history(&raw).await;
        raw.close().await;

        let error = startup_error(&db_path).await;
        assert!(
            error.contains(invalid_history.expected_error()),
            "{} history returned unexpected error: {error}",
            invalid_history.label()
        );

        let raw = raw_pool(&db_path).await;
        assert_eq!(
            migration_history(&raw).await,
            history_before,
            "{} history was mutated after startup refusal",
            invalid_history.label()
        );
        raw.close().await;
    }
}

#[tokio::test]
async fn v2_shaped_database_without_baseline_ledger_is_rejected_without_inference() {
    /*
     * Removes only the migration ledger from an otherwise exact v2.0.0 fixture after
     * snapshotting all data. It checks startup will not infer provenance from schema shape
     * and does not recreate the ledger or mutate any baseline or preview rows.
     */
    let fixture = v2_0_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let row_counts_before = baseline_row_counts(&raw).await;
    let data_before = baseline_data_snapshot(&raw).await;
    let previews_before = preview_data_snapshot(&raw).await;
    sqlx::query("DROP TABLE schema_migrations")
        .execute(&raw)
        .await
        .expect("remove baseline ledger");
    raw.close().await;

    let error = startup_error(&db_path).await;
    assert!(
        error.contains("no schema_migrations ledger")
            && error.contains("oldest supported source baseline is Vault 2.0.0"),
        "unexpected missing-baseline-ledger error: {error}"
    );

    let raw = raw_pool(&db_path).await;
    let ledger_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
    )
    .fetch_one(&raw)
    .await
    .expect("baseline ledger table count");
    assert_eq!(ledger_tables, 0);
    assert_eq!(baseline_row_counts(&raw).await, row_counts_before);
    assert_eq!(baseline_data_snapshot(&raw).await, data_before);
    assert_eq!(preview_data_snapshot(&raw).await, previews_before);
    raw.close().await;
}

#[tokio::test]
async fn restarting_an_already_current_database_is_idempotent() {
    /*
     * Upgrades a v2.0.0 fixture once, snapshots its migration history, data, previews, and
     * roots, then opens it again. It checks a current database receives no repeated
     * migration effects and retains exactly the same two roots and rows across restart.
     */
    let fixture = v2_0_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let first_pool = db::connect(&db_path).await.expect("first startup");
    let history_after_first_start = migration_history(&first_pool).await;
    let row_counts_after_first_start = baseline_row_counts(&first_pool).await;
    let data_after_first_start = baseline_data_snapshot(&first_pool).await;
    let previews_after_first_start = preview_data_snapshot(&first_pool).await;
    let roots_after_first_start: Vec<FolderRow> = sqlx::query_as(
        r"
        SELECT id, root_key, parent_id, name, is_root
        FROM folders
        WHERE is_root = 1
        ORDER BY id
        ",
    )
    .fetch_all(&first_pool)
    .await
    .expect("roots after first startup");
    first_pool.close().await;

    let second_pool = db::connect(&db_path).await.expect("second startup");
    assert_eq!(
        migration_history(&second_pool).await,
        history_after_first_start
    );
    assert_eq!(
        baseline_row_counts(&second_pool).await,
        row_counts_after_first_start
    );
    assert_eq!(
        baseline_data_snapshot(&second_pool).await,
        data_after_first_start
    );
    assert_eq!(
        preview_data_snapshot(&second_pool).await,
        previews_after_first_start
    );
    let roots_after_second_start: Vec<FolderRow> = sqlx::query_as(
        r"
        SELECT id, root_key, parent_id, name, is_root
        FROM folders
        WHERE is_root = 1
        ORDER BY id
        ",
    )
    .fetch_all(&second_pool)
    .await
    .expect("roots after second startup");
    assert_eq!(roots_after_second_start, roots_after_first_start);
    assert_eq!(roots_after_second_start.len(), 2);
    second_pool.close().await;
}

#[tokio::test]
async fn current_database_with_nonboolean_root_flag_refuses_restart_without_repair() {
    /*
     * Corrupts the Vault root flag after bringing a fixture fully current, then attempts another
     * startup. It checks invariant validation reports the precise bad root, preserves the
     * invalid value for diagnosis, and leaves migration history unchanged.
     */
    let fixture = v2_0_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let pool = db::connect(&db_path)
        .await
        .expect("prepare current database");
    pool.close().await;

    let raw = raw_pool(&db_path).await;
    let result =
        sqlx::query("UPDATE folders SET is_root = 2 WHERE root_key = 'vault' AND is_root = 1")
            .execute(&raw)
            .await
            .expect("set invalid root flag");
    assert_eq!(result.rows_affected(), 1);
    let history_before = migration_history(&raw).await;
    raw.close().await;

    let error = startup_error(&db_path).await;
    assert!(
        error.contains("folder_invariant_failed reason=invalid_root_flag folder_id=1"),
        "unexpected invalid-root-flag error: {error}"
    );

    let raw = raw_pool(&db_path).await;
    let root_flag: i64 = sqlx::query_scalar(
        "SELECT is_root FROM folders WHERE root_key = 'vault' AND parent_id IS NULL",
    )
    .fetch_one(&raw)
    .await
    .expect("invalid root flag after refusal");
    assert_eq!(root_flag, 2);
    assert_eq!(migration_history(&raw).await, history_before);
    raw.close().await;
}

#[tokio::test]
async fn malformed_hierarchy_refuses_upgrade_without_committing_pending_migration() {
    /*
     * Gives a v2.0.0 fixture both the migratable legacy root and a folder whose parent does not
     * exist. It checks final hierarchy validation aborts the pending migration, leaving the old
     * root spelling, ledger, preview data, and malformed row untouched.
     */
    let fixture = v2_0_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let history_before = migration_history(&raw).await;
    assert_eq!(history_before.len(), 1);
    let previews_before = preview_data_snapshot(&raw).await;
    set_legacy_vault_root(&raw).await;
    let broken_id: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) + 1000 FROM folders")
        .fetch_one(&raw)
        .await
        .expect("broken folder id");
    sqlx::query(
        r"
        INSERT INTO folders (id, root_key, parent_id, name, is_root)
        VALUES (?, 'vault', 9223372036854770000, 'Broken ancestry', 0)
        ",
    )
    .bind(broken_id)
    .execute(&raw)
    .await
    .expect("malformed folder");
    raw.close().await;

    let error = startup_error(&db_path).await;
    assert!(
        error.contains("folder_invariant_failed reason=missing_parent"),
        "unexpected malformed-hierarchy error: {error}"
    );

    let raw = raw_pool(&db_path).await;
    assert_eq!(migration_history(&raw).await, history_before);
    assert_eq!(preview_data_snapshot(&raw).await, previews_before);
    let vault_root_name: String =
        sqlx::query_scalar("SELECT name FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&raw)
            .await
            .expect("legacy vault root");
    assert_eq!(vault_root_name, "Vault");
    let broken_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM folders WHERE id = ?")
        .bind(broken_id)
        .fetch_one(&raw)
        .await
        .expect("broken folder remains");
    assert_eq!(broken_rows, 1);
    raw.close().await;
}

#[tokio::test]
async fn missing_required_root_refuses_upgrade_without_seeding_a_replacement() {
    /*
     * Deletes the Vault root from a v2.0.0 fixture before current startup. It checks the
     * required root-count invariant aborts migration without inventing a replacement or
     * advancing the ledger and preview schema.
     */
    let fixture = v2_0_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let history_before = migration_history(&raw).await;
    let previews_before = preview_data_snapshot(&raw).await;
    let result = sqlx::query("DELETE FROM folders WHERE root_key = 'vault' AND is_root = 1")
        .execute(&raw)
        .await
        .expect("remove vault root");
    assert_eq!(result.rows_affected(), 1);
    raw.close().await;

    let error = startup_error(&db_path).await;
    assert!(
        error.contains("folder_invariant_failed reason=wrong_root_count expected=2 actual=1"),
        "unexpected missing-root error: {error}"
    );

    let raw = raw_pool(&db_path).await;
    let vault_roots: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&raw)
            .await
            .expect("vault root count");
    assert_eq!(vault_roots, 0);
    assert_eq!(migration_history(&raw).await, history_before);
    assert_eq!(preview_data_snapshot(&raw).await, previews_before);
    raw.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_startups_serialize_the_same_pending_migration_chain() {
    /*
     * Opens the same legacy-root fixture concurrently on two runtime threads while one migration
     * chain is pending. It checks startup serialization lets both callers succeed with one
     * complete ledger, exactly two roots, and a single normalized Vault root rather than
     * racing duplicate migration effects.
     */
    let fixture = v2_0_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    set_legacy_vault_root(&raw).await;
    raw.close().await;
    let left_path = db_path.clone();
    let right_path = db_path.clone();

    let (left, right) = tokio::join!(db::connect(&left_path), db::connect(&right_path));
    let left_pool = left.expect("left concurrent startup");
    let right_pool = right.expect("right concurrent startup");

    assert_current_history(&migration_history(&left_pool).await);
    let root_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM folders WHERE is_root = 1")
        .fetch_one(&right_pool)
        .await
        .expect("root count after concurrent startup");
    assert_eq!(root_count, 2);
    let canonical_vault_roots: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM folders
        WHERE root_key = 'vault'
          AND is_root = 1
          AND parent_id IS NULL
          AND name = ''
        ",
    )
    .fetch_one(&right_pool)
    .await
    .expect("canonical vault roots");
    assert_eq!(canonical_vault_roots, 1);

    left_pool.close().await;
    right_pool.close().await;
}
