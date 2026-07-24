mod support;

use std::path::Path;
use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use support::migration_fixtures::v2_0_0::Fixture as V2_0_0Fixture;
use support::migration_fixtures::v2_1_0::{
    ACTIVE_CHECKIN_UPLOAD_ID, ACTIVE_CREATE_UPLOAD_ID, COMPLETE_CREATE_UPLOAD_ID,
    COMPLETING_CREATE_UPLOAD_ID, Fixture as V2_1_0Fixture,
};
use vault_server::db;

const CURRENT_HISTORY: [(i64, &str); 4] = [
    (1, "content previews"),
    (2, "normalize root folders"),
    (3, "bind uploads to target folders"),
    (4, "preserve archived item identities"),
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

#[tokio::test]
async fn v2_1_0_upgrade_invalidates_ambiguous_create_uploads_and_preserves_checkins() {
    let fixture = v2_1_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    assert_eq!(migration_history(&raw).await.len(), 2);
    raw.close().await;

    let pool = db::connect(&db_path).await.expect("upgrade v2.1.0 fixture");
    assert_current_history(&migration_history(&pool).await);

    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('upload_sessions')")
            .fetch_all(&pool)
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
    .fetch_one(&pool)
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

    let upload_states: Vec<(String, String, Option<i64>, Option<String>)> = sqlx::query_as(
        r"
        SELECT id, status, target_folder_id, error
        FROM upload_sessions
        ORDER BY id
        ",
    )
    .fetch_all(&pool)
    .await
    .expect("migrated upload states");
    let restart_error =
        Some("Upload target identity is unavailable after upgrade; restart the upload".to_string());
    assert_eq!(
        upload_states,
        vec![
            (
                ACTIVE_CHECKIN_UPLOAD_ID.to_string(),
                "active".to_string(),
                None,
                None,
            ),
            (
                ACTIVE_CREATE_UPLOAD_ID.to_string(),
                "failed".to_string(),
                None,
                restart_error.clone(),
            ),
            (
                COMPLETE_CREATE_UPLOAD_ID.to_string(),
                "complete".to_string(),
                None,
                None,
            ),
            (
                COMPLETING_CREATE_UPLOAD_ID.to_string(),
                "failed".to_string(),
                None,
                restart_error,
            ),
        ]
    );

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
    .execute(&pool)
    .await
    .expect_err("active create upload without target identity must be rejected");
    assert!(
        missing_target
            .to_string()
            .contains("active create upload requires a target folder identity")
    );
    pool.close().await;
}

#[tokio::test]
async fn archive_identity_migration_normalizes_legacy_rows_without_permanent_compatibility_state() {
    let fixture = v2_1_0_fixture().await;
    let db_path = fixture.db_path().to_path_buf();
    let raw = raw_pool(&db_path).await;
    let archive_root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'archive' AND is_root = 1")
            .fetch_one(&raw)
            .await
            .expect("archive root");
    let document_id = sqlx::query(
        r"
        INSERT INTO documents (
            folder_id,
            name,
            created_by,
            latest_modified_by,
            archived_from_folder,
            archived_original_name,
            archived_access
        )
        VALUES (?, 'legacy-display-name', 'fixture:alice', 'fixture:alice',
                'Projects/Incoming', 'payload.bin', '{}')
        ",
    )
    .bind(archive_root_id)
    .execute(&raw)
    .await
    .expect("legacy archived document")
    .last_insert_rowid();
    raw.close().await;

    let pool = db::connect(&db_path)
        .await
        .expect("upgrade archived legacy row");
    let migrated: (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        r"
        SELECT
            f.name,
            d.name,
            d.archived_at,
            d.archived_origin_path,
            d.archived_access
        FROM documents d
        JOIN folders f ON f.id = d.folder_id
        WHERE d.id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("migrated archived document");
    assert_eq!(migrated.0, "Incoming");
    assert_eq!(migrated.1, "payload.bin");
    assert!(migrated.2.is_some());
    assert_eq!(migrated.3.as_deref(), Some("Projects/Incoming/payload.bin"));
    assert_eq!(migrated.4.as_deref(), Some("{}"));

    let document_columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('documents')")
            .fetch_all(&pool)
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
    assert_current_history(&migration_history(&pool).await);
    pool.close().await;
}

#[tokio::test]
async fn derived_v2_0_0_incident_state_normalizes_legacy_root_without_replacing_descendants() {
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
