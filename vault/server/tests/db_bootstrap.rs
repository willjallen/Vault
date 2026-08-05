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
    /*
     * Opens a brand-new database through the production connection path. It checks startup
     * applies the configured busy timeout, seeds exactly the Vault and Archive roots, and
     * records the full initial migration chain.
     */
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

    let migration_versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .expect("migration versions");
    assert_eq!(migration_versions, [1, 2, 3]);
}

#[tokio::test]
async fn unversioned_pre_v2_schema_is_rejected_without_mutation() {
    /*
     * Removes preview tables and the migration ledger from an otherwise valid database to
     * resemble an unsupported pre-v2 schema, while adding sentinel user data. It checks
     * startup refuses the database without recreating any missing table or changing the
     * sentinel.
     */
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    initialize_valid_database(&db_path).await;

    let raw = raw_pool(&db_path).await;
    sqlx::query("DROP TABLE preview_renditions")
        .execute(&raw)
        .await
        .expect("drop preview renditions");
    sqlx::query("DROP TABLE preview_jobs")
        .execute(&raw)
        .await
        .expect("drop preview jobs");
    sqlx::query("DROP TABLE schema_migrations")
        .execute(&raw)
        .await
        .expect("drop migration marker");
    sqlx::query("INSERT INTO vault_settings (key, value) VALUES ('keep', 'me')")
        .execute(&raw)
        .await
        .expect("preserved data");
    raw.close().await;

    assert_startup_rejected(&db_path, "pre-v2 schema should reject").await;

    let pool = raw_pool(&db_path).await;
    let value: String = sqlx::query_scalar("SELECT value FROM vault_settings WHERE key = 'keep'")
        .fetch_one(&pool)
        .await
        .expect("preserved setting");
    assert_eq!(value, "me");
    let preview_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('preview_jobs', 'preview_renditions')",
    )
    .fetch_one(&pool)
    .await
    .expect("preview tables");
    assert_eq!(preview_tables, 0);
    let migration_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
    )
    .fetch_one(&pool)
    .await
    .expect("migration table");
    assert_eq!(migration_tables, 0);
    pool.close().await;
}

#[tokio::test]
async fn nonexact_unversioned_schema_is_rejected_before_migration() {
    /*
     * Builds an unversioned pre-v2-shaped database with one unknown extension table. It checks
     * the extra schema object prevents baseline inference and that startup does not create a
     * migration ledger before refusing the database.
     */
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    initialize_valid_database(&db_path).await;

    let raw = raw_pool(&db_path).await;
    sqlx::query("DROP TABLE preview_renditions")
        .execute(&raw)
        .await
        .expect("drop preview renditions");
    sqlx::query("DROP TABLE preview_jobs")
        .execute(&raw)
        .await
        .expect("drop preview jobs");
    sqlx::query("DROP TABLE schema_migrations")
        .execute(&raw)
        .await
        .expect("drop migration marker");
    sqlx::query("CREATE TABLE unknown_extension (id INTEGER PRIMARY KEY)")
        .execute(&raw)
        .await
        .expect("unknown extension");
    raw.close().await;

    assert_startup_rejected(&db_path, "nonexact baseline should reject").await;
    let raw = raw_pool(&db_path).await;
    let migration_table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
    )
    .fetch_one(&raw)
    .await
    .expect("migration table count");
    assert_eq!(migration_table_count, 0);
}

#[tokio::test]
async fn incompatible_existing_schema_is_rejected_without_dropping_data() {
    /*
     * Creates an unrelated `documents` table with a schema and row that conflict with Vault's
     * model. It checks startup recognizes the database as occupied, refuses automatic takeover,
     * and preserves the existing table and data.
     */
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
async fn view_only_database_is_not_mistaken_for_an_empty_database() {
    /*
     * Creates a SQLite database containing only a view and then opens it through Vault startup.
     * It checks any existing schema object prevents fresh initialization, while the view
     * remains usable and no Vault tables are added.
     */
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    let raw = raw_pool(&db_path).await;
    sqlx::query("CREATE VIEW existing_view AS SELECT 1 AS value")
        .execute(&raw)
        .await
        .expect("create existing view");
    raw.close().await;

    assert_startup_rejected(&db_path, "view-only database should reject").await;

    let raw = raw_pool(&db_path).await;
    let view_value: i64 = sqlx::query_scalar("SELECT value FROM existing_view")
        .fetch_one(&raw)
        .await
        .expect("existing view remains");
    assert_eq!(view_value, 1);
    let vault_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'folders'",
    )
    .fetch_one(&raw)
    .await
    .expect("folders table count");
    assert_eq!(vault_tables, 0);
    raw.close().await;
}

#[tokio::test]
async fn noncanonical_schema_is_rejected_without_additive_changes() {
    /*
     * Removes several columns and a table that an older bootstrap path might have added back,
     * then stores a sentinel user. It checks current startup refuses structural drift
     * without performing additive repair or altering existing rows.
     */
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

    assert_startup_rejected(&db_path, "noncanonical schema should reject").await;

    let raw = raw_pool(&db_path).await;
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('vault_users')")
            .fetch_all(&raw)
            .await
            .expect("columns");
    assert!(!columns.iter().any(|column| column == "preferences"));
    let user_name: String =
        sqlx::query_scalar("SELECT name FROM vault_users WHERE subject = 'alice'")
            .fetch_one(&raw)
            .await
            .expect("existing user");
    assert_eq!(user_name, "Alice");
    let settings_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'vault_settings'",
    )
    .fetch_one(&raw)
    .await
    .expect("settings table");
    assert_eq!(settings_count, 0);
    let upload_columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('upload_sessions')")
            .fetch_all(&raw)
            .await
            .expect("upload columns");
    assert!(
        upload_columns
            .iter()
            .all(|column| column != "verification_total_bytes")
    );
    assert!(
        upload_columns
            .iter()
            .all(|column| column != "verification_processed_bytes")
    );
    raw.close().await;
}

#[tokio::test]
async fn legacy_share_link_schema_is_rejected_without_mutation() {
    /*
     * Replaces the current share-link table with a populated legacy definition and snapshots its
     * SQL, columns, and row. It checks startup refuses the mismatch without rebuilding the table
     * or changing any legacy share data.
     */
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    initialize_valid_database(&db_path).await;

    let raw = raw_pool(&db_path).await;
    sqlx::query("DROP TABLE share_links")
        .execute(&raw)
        .await
        .expect("replace share links fixture");
    sqlx::query(
        r"
        CREATE TABLE share_links (
            id INTEGER PRIMARY KEY,
            code TEXT NOT NULL UNIQUE,
            item_type TEXT NOT NULL,
            item_id INTEGER NOT NULL,
            created_by TEXT,
            created_by_name TEXT,
            created_at TEXT NOT NULL,
            expires_at TEXT
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("legacy share links fixture");
    sqlx::query(
        r"
        INSERT INTO share_links (
            id, code, item_type, item_id, created_by, created_by_name, created_at
        )
        VALUES (7, 'keep-share', 'document', 42, 'alice', 'Alice', '2026-01-02 03:04:05')
        ",
    )
    .execute(&raw)
    .await
    .expect("legacy share row");
    let table_sql_before: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'share_links'",
    )
    .fetch_one(&raw)
    .await
    .expect("legacy table SQL");
    let columns_before: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('share_links') ORDER BY cid")
            .fetch_all(&raw)
            .await
            .expect("legacy columns");
    raw.close().await;

    assert_startup_rejected(&db_path, "legacy share schema should reject").await;

    let raw = raw_pool(&db_path).await;
    let table_sql_after: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'share_links'",
    )
    .fetch_one(&raw)
    .await
    .expect("share table SQL after rejection");
    let columns_after: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('share_links') ORDER BY cid")
            .fetch_all(&raw)
            .await
            .expect("share columns after rejection");
    assert_eq!(table_sql_after, table_sql_before);
    assert_eq!(columns_after, columns_before);
    let row: (
        i64,
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
    ) = sqlx::query_as(
        r"
            SELECT id, code, item_type, item_id, created_by, created_by_name, created_at
            FROM share_links
            ",
    )
    .fetch_one(&raw)
    .await
    .expect("legacy share row after rejection");
    assert_eq!(
        row,
        (
            7,
            "keep-share".to_string(),
            "document".to_string(),
            42,
            Some("alice".to_string()),
            Some("Alice".to_string()),
            "2026-01-02 03:04:05".to_string(),
        )
    );
    raw.close().await;
}

#[tokio::test]
async fn missing_required_column_is_rejected_without_repairing_table() {
    /*
     * Drops the required current-version column from the documents table before restart. It
     * checks schema validation refuses startup and leaves the altered table untouched rather
     * than adding the column back automatically.
     */
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
    /*
     * Recreates the groups table with an additional required legacy column. It checks startup
     * treats extra model columns as incompatible drift and preserves the altered definition
     * instead of destructively rebuilding it.
     */
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
    /*
     * Replaces the partial unique active-lock index with an ordinary index on the same column.
     * It checks validation compares index semantics—not just its name or columns—and refuses
     * startup when concurrent lock uniqueness is no longer enforced.
     */
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
    /*
     * Adds a global uniqueness rule for document names that is absent from the canonical model.
     * It checks startup rejects the extra data constraint and leaves the index in place for
     * explicit operator review.
     */
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
    /*
     * Builds separate group tables with the required unique constraint missing, attached to the
     * wrong column, or paired with no primary key. It checks startup rejects each independently,
     * proving table identity constraints are validated structurally.
     */
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
    /*
     * Creates one model table without its required parent reference and another with an
     * unexpected cascading reference. It checks both missing and surplus foreign-key
     * behavior are considered incompatible schema drift.
     */
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
#[allow(clippy::too_many_lines)]
async fn nullability_type_and_check_constraint_drift_are_rejected_on_startup() {
    /*
     * Constructs databases with a newly nullable column, a changed SQLite type, a changed
     * default, an extra check, a modified check expression, and a case-changed string
     * literal. It checks startup detects every semantic form of column or constraint drift,
     * including differences that superficial SQL normalization could hide.
     */
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
            folder_id BIGINT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
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

    let wrong_default_path = temp_dir.path().join("wrong-default.db");
    initialize_valid_database(&wrong_default_path).await;
    let raw = raw_pool(&wrong_default_path).await;
    sqlx::query("DROP TABLE folder_events")
        .execute(&raw)
        .await
        .expect("drop folder events");
    sqlx::query(
        r"
        CREATE TABLE folder_events (
            id INTEGER PRIMARY KEY,
            folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
            event_type TEXT NOT NULL,
            actor TEXT,
            actor_name TEXT,
            message TEXT,
            created_at TEXT NOT NULL DEFAULT '1970-01-01 00:00:00'
        )
        ",
    )
    .execute(&raw)
    .await
    .expect("recreate with wrong default");
    sqlx::query("CREATE INDEX ix_folder_events_folder_id ON folder_events(folder_id)")
        .execute(&raw)
        .await
        .expect("folder index");
    raw.close().await;
    assert_startup_rejected(&wrong_default_path, "wrong column default should reject").await;

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

    let changed_check_path = temp_dir.path().join("changed-check.db");
    initialize_valid_database(&changed_check_path).await;
    let raw = raw_pool(&changed_check_path).await;
    sqlx::query("PRAGMA writable_schema = ON")
        .execute(&raw)
        .await
        .expect("enable writable schema");
    sqlx::query(
        r"
        UPDATE sqlite_master
        SET sql = replace(sql, '''failed''))', '''failed'', ''paused''))')
        WHERE type = 'table' AND name = 'preview_jobs'
        ",
    )
    .execute(&raw)
    .await
    .expect("change preview status check");
    sqlx::query("PRAGMA writable_schema = OFF")
        .execute(&raw)
        .await
        .expect("disable writable schema");
    let changed_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'preview_jobs'",
    )
    .fetch_one(&raw)
    .await
    .expect("changed preview table sql");
    assert!(changed_sql.contains("paused"));
    raw.close().await;
    assert_startup_rejected(
        &changed_check_path,
        "changed check expression should reject",
    )
    .await;

    let literal_case_path = temp_dir.path().join("literal-case.db");
    initialize_valid_database(&literal_case_path).await;
    let raw = raw_pool(&literal_case_path).await;
    sqlx::query("PRAGMA writable_schema = ON")
        .execute(&raw)
        .await
        .expect("enable writable schema");
    sqlx::query(
        r"
        UPDATE sqlite_master
        SET sql = replace(sql, '''queued''', '''QUEUED''')
        WHERE type = 'table' AND name = 'preview_jobs'
        ",
    )
    .execute(&raw)
    .await
    .expect("change preview status literal case");
    sqlx::query("PRAGMA writable_schema = OFF")
        .execute(&raw)
        .await
        .expect("disable writable schema");
    let changed_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'preview_jobs'",
    )
    .fetch_one(&raw)
    .await
    .expect("changed literal-case preview table sql");
    assert!(changed_sql.contains("'QUEUED'"));
    raw.close().await;
    assert_startup_rejected(&literal_case_path, "case-changed SQL literal should reject").await;
}

#[tokio::test]
async fn unexpected_trigger_on_model_table_is_rejected_on_startup() {
    /*
     * Adds a destructive trigger to an otherwise canonical model table. It checks startup
     * refuses hidden database behavior not declared by the application and preserves the
     * trigger rather than executing or silently dropping it.
     */
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
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'trigger' AND name = ?")
            .bind("vault_groups_delete_documents")
            .fetch_one(&raw)
            .await
            .expect("trigger remains");
    assert_eq!(trigger, "vault_groups_delete_documents");
    raw.close().await;
}
