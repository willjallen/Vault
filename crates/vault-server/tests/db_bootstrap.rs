use vault_server::db::{self, SQLITE_BUSY_TIMEOUT_MS};

async fn raw_pool(path: &std::path::Path) -> sqlx::SqlitePool {
    use std::str::FromStr;

    let options =
        sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .expect("sqlite options")
            .create_if_missing(true)
            .foreign_keys(false);
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("raw pool")
}

async fn initialize_valid_database(path: &std::path::Path) {
    let pool = db::connect(path).await.expect("valid db");
    pool.close().await;
}

async fn assert_startup_rejected(path: &std::path::Path, detail: &str) {
    let error = db::connect(path).await.expect_err(detail);
    assert!(
        error
            .to_string()
            .contains("Startup refused to alter or drop existing metadata automatically")
    );
}

#[tokio::test]
async fn initializes_sqlite_schema_with_root_folders() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp_dir.path().join("vault.db"))
        .await
        .expect("connect");

    let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(&pool)
        .await
        .expect("busy timeout");
    assert_eq!(
        busy_timeout,
        i64::try_from(SQLITE_BUSY_TIMEOUT_MS).expect("busy timeout fits i64"),
    );

    let roots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM folders WHERE is_root = 1")
        .fetch_one(&pool)
        .await
        .expect("root count");
    assert_eq!(roots, 2);
}

#[tokio::test]
async fn incompatible_existing_schema_is_rejected_without_dropping_data() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    let raw = raw_pool(&db_path).await;
    sqlx::query("CREATE TABLE documents (id INTEGER PRIMARY KEY, path TEXT)")
        .execute(&raw)
        .await
        .expect("create incompatible table");
    sqlx::query("INSERT INTO documents (path) VALUES ('keep-me')")
        .execute(&raw)
        .await
        .expect("insert row");
    raw.close().await;

    assert_startup_rejected(&db_path, "incompatible schema should reject").await;

    let raw = raw_pool(&db_path).await;
    let path: String = sqlx::query_scalar("SELECT path FROM documents")
        .fetch_one(&raw)
        .await
        .expect("existing row");
    assert_eq!(path, "keep-me");
    raw.close().await;
}

#[tokio::test]
async fn known_additive_columns_and_tables_are_added_without_dropping_data() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    initialize_valid_database(&db_path).await;

    let raw = raw_pool(&db_path).await;
    sqlx::query("ALTER TABLE vault_users DROP COLUMN preferences")
        .execute(&raw)
        .await
        .expect("drop legacy missing preferences");
    sqlx::query("DROP TABLE vault_settings")
        .execute(&raw)
        .await
        .expect("drop additive table");
    sqlx::query("ALTER TABLE upload_sessions DROP COLUMN verification_total_bytes")
        .execute(&raw)
        .await
        .expect("drop legacy missing total verification column");
    sqlx::query("ALTER TABLE upload_sessions DROP COLUMN verification_processed_bytes")
        .execute(&raw)
        .await
        .expect("drop legacy missing processed verification column");
    sqlx::query(
        r"
        INSERT INTO vault_users
            (issuer, subject, email, name, is_admin, is_active, created_at)
        VALUES
            ('test', 'alice', 'alice@example.com', 'Alice', 0, 1, CURRENT_TIMESTAMP)
        ",
    )
    .execute(&raw)
    .await
    .expect("insert existing user");
    raw.close().await;

    let pool = db::connect(&db_path).await.expect("additive migration");
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('vault_users')")
            .fetch_all(&pool)
            .await
            .expect("columns");
    assert!(columns.iter().any(|column| column == "preferences"));
    let preferences: String =
        sqlx::query_scalar("SELECT preferences FROM vault_users WHERE subject = 'alice'")
            .fetch_one(&pool)
            .await
            .expect("preferences");
    assert_eq!(preferences, "{}");
    let settings_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'vault_settings'",
    )
    .fetch_one(&pool)
    .await
    .expect("settings table");
    assert_eq!(settings_count, 1);
    let upload_columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('upload_sessions')")
            .fetch_all(&pool)
            .await
            .expect("upload columns");
    assert!(
        upload_columns
            .iter()
            .any(|column| column == "verification_total_bytes")
    );
    assert!(
        upload_columns
            .iter()
            .any(|column| column == "verification_processed_bytes")
    );
}

#[tokio::test]
async fn missing_required_column_is_rejected_without_repairing_table() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    initialize_valid_database(&db_path).await;

    let raw = raw_pool(&db_path).await;
    sqlx::query("ALTER TABLE documents DROP COLUMN current_version_id")
        .execute(&raw)
        .await
        .expect("drop required column");
    raw.close().await;

    assert_startup_rejected(&db_path, "missing model column should reject").await;

    let raw = raw_pool(&db_path).await;
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('documents')")
            .fetch_all(&raw)
            .await
            .expect("columns");
    assert!(!columns.iter().any(|column| column == "current_version_id"));
    raw.close().await;
}

#[tokio::test]
async fn unexpected_model_column_is_rejected_without_rebuilding_table() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    initialize_valid_database(&db_path).await;

    let raw = raw_pool(&db_path).await;
    sqlx::query("DROP TABLE vault_groups")
        .execute(&raw)
        .await
        .expect("drop vault groups");
    sqlx::query(
        r"
        CREATE TABLE vault_groups (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT,
            created_at TEXT NOT NULL,
            legacy_required TEXT NOT NULL,
            CONSTRAINT uq_vault_groups_name UNIQUE (name)
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate with unexpected column");
    raw.close().await;

    assert_startup_rejected(&db_path, "unexpected column should reject").await;

    let raw = raw_pool(&db_path).await;
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('vault_groups')")
            .fetch_all(&raw)
            .await
            .expect("columns");
    assert!(columns.iter().any(|column| column == "legacy_required"));
    raw.close().await;
}

#[tokio::test]
async fn missing_or_wrong_unique_index_is_rejected_on_startup() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    initialize_valid_database(&db_path).await;

    let raw = raw_pool(&db_path).await;
    sqlx::query("DROP INDEX uq_document_locks_active_document")
        .execute(&raw)
        .await
        .expect("drop index");
    sqlx::query(
        r"
        CREATE INDEX uq_document_locks_active_document
        ON document_locks (document_id)
        ",
    )
    .execute(&raw)
    .await
    .expect("replace with non-unique index");
    raw.close().await;

    assert_startup_rejected(&db_path, "wrong index should reject").await;
}

#[tokio::test]
async fn unexpected_unique_index_is_rejected_on_startup() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    initialize_valid_database(&db_path).await;

    let raw = raw_pool(&db_path).await;
    sqlx::query(
        r"
        CREATE UNIQUE INDEX uq_documents_global_name
        ON documents (name)
        ",
    )
    .execute(&raw)
    .await
    .expect("create unexpected unique index");
    raw.close().await;

    assert_startup_rejected(&db_path, "unexpected unique index should reject").await;

    let raw = raw_pool(&db_path).await;
    let unique: i64 =
        sqlx::query_scalar("SELECT [unique] FROM pragma_index_list('documents') WHERE name = 'uq_documents_global_name'")
            .fetch_one(&raw)
            .await
            .expect("unexpected index remains");
    assert_eq!(unique, 1);
    raw.close().await;
}

#[tokio::test]
async fn unique_constraint_and_primary_key_drift_are_rejected_on_startup() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let missing_unique_path = temp_dir.path().join("missing-unique.db");
    initialize_valid_database(&missing_unique_path).await;
    let raw = raw_pool(&missing_unique_path).await;
    sqlx::query("DROP TABLE vault_groups")
        .execute(&raw)
        .await
        .expect("drop vault groups");
    sqlx::query(
        r"
        CREATE TABLE vault_groups (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT,
            created_at TEXT NOT NULL
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate without unique");
    raw.close().await;
    assert_startup_rejected(&missing_unique_path, "missing unique should reject").await;

    let wrong_unique_path = temp_dir.path().join("wrong-unique.db");
    initialize_valid_database(&wrong_unique_path).await;
    let raw = raw_pool(&wrong_unique_path).await;
    sqlx::query("DROP TABLE vault_groups")
        .execute(&raw)
        .await
        .expect("drop vault groups");
    sqlx::query(
        r"
        CREATE TABLE vault_groups (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT,
            created_at TEXT NOT NULL,
            CONSTRAINT uq_vault_groups_name UNIQUE (id)
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate with wrong unique");
    raw.close().await;
    assert_startup_rejected(&wrong_unique_path, "wrong unique should reject").await;

    let missing_primary_path = temp_dir.path().join("missing-primary.db");
    initialize_valid_database(&missing_primary_path).await;
    let raw = raw_pool(&missing_primary_path).await;
    sqlx::query("DROP TABLE vault_groups")
        .execute(&raw)
        .await
        .expect("drop vault groups");
    sqlx::query(
        r"
        CREATE TABLE vault_groups (
            id INTEGER NOT NULL,
            name TEXT NOT NULL,
            description TEXT,
            created_at TEXT NOT NULL,
            CONSTRAINT uq_vault_groups_name UNIQUE (name)
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate without primary key");
    raw.close().await;
    assert_startup_rejected(&missing_primary_path, "missing primary key should reject").await;
}

#[tokio::test]
async fn foreign_key_drift_is_rejected_on_startup() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let missing_fk_path = temp_dir.path().join("missing-fk.db");
    initialize_valid_database(&missing_fk_path).await;
    let raw = raw_pool(&missing_fk_path).await;
    sqlx::query("DROP TABLE folder_events")
        .execute(&raw)
        .await
        .expect("drop folder events");
    sqlx::query(
        r"
        CREATE TABLE folder_events (
            id INTEGER PRIMARY KEY,
            folder_id INTEGER NOT NULL,
            event_type TEXT NOT NULL,
            actor TEXT,
            actor_name TEXT,
            message TEXT,
            created_at TEXT NOT NULL
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate without foreign key");
    sqlx::query("CREATE INDEX ix_folder_events_folder_id ON folder_events(folder_id)")
        .execute(&raw)
        .await
        .expect("folder index");
    raw.close().await;
    assert_startup_rejected(&missing_fk_path, "missing foreign key should reject").await;

    let unexpected_fk_path = temp_dir.path().join("unexpected-fk.db");
    initialize_valid_database(&unexpected_fk_path).await;
    let raw = raw_pool(&unexpected_fk_path).await;
    sqlx::query("DROP TABLE vault_groups")
        .execute(&raw)
        .await
        .expect("drop vault groups");
    sqlx::query(
        r"
        CREATE TABLE vault_groups (
            id INTEGER PRIMARY KEY REFERENCES folders(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            description TEXT,
            created_at TEXT NOT NULL,
            CONSTRAINT uq_vault_groups_name UNIQUE (name)
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate with unexpected foreign key");
    raw.close().await;
    assert_startup_rejected(&unexpected_fk_path, "unexpected foreign key should reject").await;
}

#[tokio::test]
async fn nullability_type_and_check_constraint_drift_are_rejected_on_startup() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let nullable_path = temp_dir.path().join("nullable.db");
    initialize_valid_database(&nullable_path).await;
    let raw = raw_pool(&nullable_path).await;
    sqlx::query("DROP TABLE folder_events")
        .execute(&raw)
        .await
        .expect("drop folder events");
    sqlx::query(
        r"
        CREATE TABLE folder_events (
            id INTEGER PRIMARY KEY,
            folder_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
            event_type TEXT NOT NULL,
            actor TEXT,
            actor_name TEXT,
            message TEXT,
            created_at TEXT NOT NULL
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate with nullable required column");
    sqlx::query("CREATE INDEX ix_folder_events_folder_id ON folder_events(folder_id)")
        .execute(&raw)
        .await
        .expect("folder index");
    raw.close().await;
    assert_startup_rejected(&nullable_path, "nullable required column should reject").await;

    let wrong_type_path = temp_dir.path().join("wrong-type.db");
    initialize_valid_database(&wrong_type_path).await;
    let raw = raw_pool(&wrong_type_path).await;
    sqlx::query("DROP TABLE folder_events")
        .execute(&raw)
        .await
        .expect("drop folder events");
    sqlx::query(
        r"
        CREATE TABLE folder_events (
            id INTEGER PRIMARY KEY,
            folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
            event_type TEXT NOT NULL,
            actor TEXT,
            actor_name TEXT,
            message TEXT,
            created_at TEXT NOT NULL
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate with wrong type");
    sqlx::query("CREATE INDEX ix_folder_events_folder_id ON folder_events(folder_id)")
        .execute(&raw)
        .await
        .expect("folder index");
    raw.close().await;
    assert_startup_rejected(&wrong_type_path, "wrong column type should reject").await;

    let check_path = temp_dir.path().join("check.db");
    initialize_valid_database(&check_path).await;
    let raw = raw_pool(&check_path).await;
    sqlx::query("DROP TABLE vault_groups")
        .execute(&raw)
        .await
        .expect("drop vault groups");
    sqlx::query(
        r"
        CREATE TABLE vault_groups (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT,
            created_at TEXT NOT NULL,
            CONSTRAINT uq_vault_groups_name UNIQUE (name),
            CONSTRAINT ck_vault_groups_not_blocked CHECK (name != 'blocked')
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate with check constraint");
    raw.close().await;
    assert_startup_rejected(&check_path, "unexpected check should reject").await;
}

#[tokio::test]
async fn unexpected_trigger_on_model_table_is_rejected_on_startup() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    initialize_valid_database(&db_path).await;

    let raw = raw_pool(&db_path).await;
    sqlx::query(
        r"
        CREATE TRIGGER vault_groups_delete_documents
        AFTER INSERT ON vault_groups
        BEGIN
            DELETE FROM documents;
        END
        ",
    )
    .execute(&raw)
    .await
    .expect("create trigger");
    raw.close().await;

    let error = db::connect(&db_path)
        .await
        .expect_err("unexpected trigger should reject");
    assert!(
        error
            .to_string()
            .contains("Startup refused to alter or drop existing metadata automatically")
    );

    let raw = raw_pool(&db_path).await;
    let trigger: String =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'trigger'")
            .fetch_one(&raw)
            .await
            .expect("trigger remains");
    assert_eq!(trigger, "vault_groups_delete_documents");
    raw.close().await;
}
