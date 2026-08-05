//! Runtime-generated database created from the released Vault 2.1.0 contract.
//!
//! The schema and migration ledger are pinned from upstream tag `v2.1.0`,
//! commit `372f2f3f6980a5119f55f829de2cd8801d87b42b`. The released 2.1.0 database
//! contract is byte-identical to 2.0.0. This builder reuses the independently
//! pinned 2.0.0 fixture and adds deterministic upgrade states supported by 2.1.

#![allow(dead_code)]

use std::path::Path;
use std::str::FromStr;

use anyhow::Context;
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

use super::v2_0_0;

const RELEASE_TIMESTAMP: &str = "2026-07-22T22:42:38Z";
const ALICE_USER_CONTEXT: &str = concat!(
    r#"{"id":"fixture:alice","vault_user_id":100,"issuer":"https://fixture.invalid","#,
    r#""subject":"alice","name":"Alice Fixture","email":"alice@fixture.invalid","#,
    r#""groups":["Fixture Writers"],"is_admin":false}"#,
);
const COMPLETED_ALICE_USER_CONTEXT: &str = concat!(
    r#"{"id":"fixture:alice","vault_user_id":100,"issuer":"https://fixture.invalid","#,
    r#""subject":"alice","name":"Alice Fixture","email":"alice@fixture.invalid","#,
    r#""groups":["Fixture Writers"],"is_admin":false,"#,
    r#""_upload_part_manifest_sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}"#,
);
pub const ACTIVE_CREATE_UPLOAD_ID: &str = "v2-1-active-create";
pub const COMPLETING_CREATE_UPLOAD_ID: &str = "v2-1-completing-create";
pub const ACTIVE_CHECKIN_UPLOAD_ID: &str = "v2-1-active-checkin";
pub const COMPLETING_CHECKIN_UPLOAD_ID: &str = "v2-1-completing-checkin";
pub const COMPLETE_CREATE_UPLOAD_ID: &str = "v2-1-complete-create";
pub const ARCHIVED_DOCUMENT_ID: i64 = 1100;
pub const ARCHIVED_VERSION_ONE_ID: &str = "00000000-0000-7000-8000-000000000010";
pub const ARCHIVED_VERSION_TWO_ID: &str = "00000000-0000-7000-8000-000000000011";
pub const EXISTING_PATH_ARCHIVED_DOCUMENT_ID: i64 = 1101;
pub const EXISTING_PATH_ARCHIVED_VERSION_ID: &str = "00000000-0000-7000-8000-000000000012";

#[derive(Debug)]
pub struct Fixture {
    baseline: v2_0_0::Fixture,
}

impl Fixture {
    pub async fn create() -> anyhow::Result<Self> {
        let baseline = v2_0_0::Fixture::create().await?;
        let options =
            SqliteConnectOptions::from_str(&format!("sqlite://{}", baseline.db_path().display()))?
                .create_if_missing(false)
                .foreign_keys(true)
                .journal_mode(SqliteJournalMode::Delete);
        let mut connection = sqlx::SqliteConnection::connect_with(&options)
            .await
            .context("open v2.1.0 fixture database")?;
        let mut transaction = connection
            .begin()
            .await
            .context("begin v2.1.0 fixture transaction")?;

        seed_upload_sessions(&mut transaction).await?;
        seed_upload_parts(&mut transaction).await?;
        seed_archived_document_graph(&mut transaction).await?;
        transaction.commit().await?;
        validate_fixture(&mut connection).await?;
        connection.close().await?;

        Ok(Self { baseline })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.baseline.root()
    }

    #[must_use]
    pub fn db_path(&self) -> &Path {
        self.baseline.db_path()
    }
}

async fn seed_upload_sessions(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO upload_sessions (
            id, mode, status, folder_path, document_id, filename, total_size,
            chunk_size, part_count, mime_type, note, rename_to_upload,
            verification_total_bytes, verification_processed_bytes, created_by,
            created_by_name, user_context, upload_ip, upload_user_agent,
            created_at, updated_at, expires_at, completed_at, result_document_id,
            result_version_id, result_path
        )
        VALUES
            (
                ?, 'create', 'active', 'Visual Assets/Migration Previews',
                NULL, 'active-create.txt', 6, 3, 2, 'text/plain',
                'Partially uploaded create', 0, 0, 0,
                'fixture:alice', 'Alice Fixture', ?, '192.0.2.20',
                'Vault 2.1 migration fixture',
                '2026-07-22T22:42:38Z', '2026-07-22T22:42:38Z',
                '2999-01-01T00:00:00Z', NULL, NULL, NULL, NULL
            ),
            (
                ?, 'create', 'completing', 'Visual Assets/Migration Previews',
                NULL, 'completing-create.txt', 6, 3, 2, 'text/plain',
                'Fully transferred create', 0, 6, 6,
                'fixture:alice', 'Alice Fixture', ?, '192.0.2.20',
                'Vault 2.1 migration fixture',
                '2026-07-22T22:42:38Z', '2026-07-22T22:42:38Z',
                '2999-01-01T00:00:00Z', NULL, NULL, NULL, NULL
            ),
            (
                ?, 'checkin', 'active', NULL, 1000, 'migration-preview.png',
                4, 2, 2, 'image/png', 'Partial check-in', 1, 0, 0,
                'fixture:alice', 'Alice Fixture', ?, '192.0.2.20',
                'Vault 2.1 migration fixture',
                '2026-07-22T22:42:38Z', '2026-07-22T22:42:38Z',
                '2999-01-01T00:00:00Z', NULL, NULL, NULL, NULL
            ),
            (
                ?, 'checkin', 'completing', NULL, 1000,
                'migration-preview.png', 4, 2, 2, 'image/png',
                'Verifying check-in', 1, 4, 2,
                'fixture:alice', 'Alice Fixture', ?, '192.0.2.20',
                'Vault 2.1 migration fixture',
                '2026-07-22T22:42:38Z', '2026-07-22T22:42:38Z',
                '2999-01-01T00:00:00Z', NULL, NULL, NULL, NULL
            ),
            (
                ?, 'create', 'complete', 'Visual Assets/Migration Previews',
                NULL, 'migration-preview.png', 143, 100, 2, 'image/png',
                'Completed create with resolved identities', 0, 143, 143,
                'fixture:alice', 'Alice Fixture', ?, '192.0.2.20',
                'Vault 2.1 migration fixture',
                '2026-07-22T13:19:00Z', '2026-07-22T13:20:00Z',
                '2999-01-01T00:00:00Z', '2026-07-22T13:20:00Z', 1000,
                '00000000-0000-7000-8000-000000000001',
                'Visual Assets/Migration Previews/migration-preview.png'
            )
        ",
    )
    .bind(ACTIVE_CREATE_UPLOAD_ID)
    .bind(ALICE_USER_CONTEXT)
    .bind(COMPLETING_CREATE_UPLOAD_ID)
    .bind(ALICE_USER_CONTEXT)
    .bind(ACTIVE_CHECKIN_UPLOAD_ID)
    .bind(ALICE_USER_CONTEXT)
    .bind(COMPLETING_CHECKIN_UPLOAD_ID)
    .bind(ALICE_USER_CONTEXT)
    .bind(COMPLETE_CREATE_UPLOAD_ID)
    .bind(COMPLETED_ALICE_USER_CONTEXT)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        UPDATE document_versions
        SET message = 'Uploaded migration-preview.png',
            upload_user_agent = 'Vault 2.1 migration fixture'
        WHERE id = '00000000-0000-7000-8000-000000000001'
        ",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        INSERT INTO document_locks (
            id, document_id, locked_by, locked_by_name, locked_at, is_active,
            locked_ip, locked_user_agent, force_acquired
        )
        VALUES (
            1201, 1000, 'fixture:alice', 'Alice Fixture',
            '2026-07-22T22:40:00Z', 1, '192.0.2.20',
            'Vault 2.1 migration fixture', 0
        )
        ",
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_upload_parts(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    // Vault 2.1 did not use upload_parts; active chunk metadata lived in the
    // transfer directory. These rows exercise preservation of valid legacy data.
    sqlx::query(
        r"
        INSERT INTO upload_parts (
            id, session_id, part_number, offset_bytes, size_bytes, sha256,
            storage_path, created_at
        )
        VALUES
            (
                2000, ?, 1, 0, 3,
                'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad',
                'uploads/v2-1-active-create/00000001.part', ?
            ),
            (
                2001, ?, 1, 0, 3,
                'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad',
                'uploads/v2-1-completing-create/00000001.part', ?
            ),
            (
                2002, ?, 2, 3, 3,
                'cb8379ac2098aa165029e3938a51da0bcecfc008fd6795f401178647f96c5b34',
                'uploads/v2-1-completing-create/00000002.part', ?
            ),
            (
                2003, ?, 1, 0, 2,
                '31b25869b39f1baa9e7fc279255901b696c36629e57294d4455f479534139852',
                'uploads/v2-1-active-checkin/00000001.part', ?
            )
        ",
    )
    .bind(ACTIVE_CREATE_UPLOAD_ID)
    .bind(RELEASE_TIMESTAMP)
    .bind(COMPLETING_CREATE_UPLOAD_ID)
    .bind(RELEASE_TIMESTAMP)
    .bind(COMPLETING_CREATE_UPLOAD_ID)
    .bind(RELEASE_TIMESTAMP)
    .bind(ACTIVE_CHECKIN_UPLOAD_ID)
    .bind(RELEASE_TIMESTAMP)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_archived_document_graph(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    seed_archive_blobs_and_locations(transaction).await?;
    seed_archived_documents(transaction).await?;
    seed_archived_document_versions(transaction).await?;
    seed_archive_events_and_relations(transaction).await?;
    seed_archive_state_events(transaction).await
}

async fn seed_archive_blobs_and_locations(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO blobs (id, hash_algo, hash, size_bytes, created_at)
        VALUES
            (
                510, 'sha256',
                '6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b',
                11, '2026-07-22T22:20:00Z'
            ),
            (
                511, 'sha256',
                'd4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35',
                17, '2026-07-22T22:25:00Z'
            ),
            (
                512, 'sha256',
                '4e07408562bedb8b60ce05c1decfe3ad16b72230967de01f640b7e4729b49fce',
                23, '2026-07-22T22:26:00Z'
            )
        ",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        INSERT INTO blob_locations (
            id, blob_id, backend, bucket, object_key, created_at
        )
        VALUES
            (
                610, 510, 'local', '',
                'sha256/6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b',
                '2026-07-22T22:20:00Z'
            ),
            (
                611, 511, 'local', '',
                'sha256/d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35',
                '2026-07-22T22:25:00Z'
            ),
            (
                612, 512, 'local', '',
                'sha256/4e07408562bedb8b60ce05c1decfe3ad16b72230967de01f640b7e4729b49fce',
                '2026-07-22T22:26:00Z'
            )
        ",
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_archived_documents(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO documents (
            id, folder_id, name, description, created_at, created_by,
            created_by_name, latest_modified_at, latest_modified_by,
            latest_version_number, version_count, current_version_id,
            archived_from_folder, archived_original_name, archived_access
        )
        VALUES
            (
                ?, 2, 'payload.bin',
                'A two-version document archived by Vault 2.1',
                '2026-07-22T22:20:00Z', 'fixture:alice', 'Alice Fixture',
                '2026-07-22T22:35:00Z', 'fixture:alice', 2, 2, ?,
                'Projects/Incoming', 'payload.bin', '{"200":3,"201":2}'
            ),
            (
                ?, 2, 'existing-path.bin',
                'An archived document whose source folder still exists',
                '2026-07-22T22:26:00Z', 'fixture:alice', 'Alice Fixture',
                '2026-07-22T22:36:00Z', 'fixture:alice', 1, 1, ?,
                'Visual Assets/Migration Previews', 'existing-path.bin',
                '{"200":3,"201":2}'
            )
        "#,
    )
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(ARCHIVED_VERSION_TWO_ID)
    .bind(EXISTING_PATH_ARCHIVED_DOCUMENT_ID)
    .bind(EXISTING_PATH_ARCHIVED_VERSION_ID)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_archived_document_versions(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO document_versions (
            id, document_id, blob_id, version_number, committed_at,
            committed_by, committed_by_name, message, mime_type,
            original_filename, upload_ip, upload_user_agent, created_via
        )
        VALUES
            (
                ?, ?, 510, 1, '2026-07-22T22:20:00Z',
                'fixture:alice', 'Alice Fixture', 'Initial archived payload',
                'application/octet-stream', 'payload.bin', '192.0.2.21',
                'Vault 2.1 migration fixture', 'upload'
            ),
            (
                ?, ?, 511, 2, '2026-07-22T22:25:00Z',
                'fixture:alice', 'Alice Fixture', 'Checked-in revision',
                'application/octet-stream', 'payload.bin', '192.0.2.21',
                'Vault 2.1 migration fixture', 'checkin'
            ),
            (
                ?, ?, 512, 1, '2026-07-22T22:26:00Z',
                'fixture:alice', 'Alice Fixture', 'Initial archived payload',
                'application/octet-stream', 'existing-path.bin', '192.0.2.21',
                'Vault 2.1 migration fixture', 'upload'
            )
        ",
    )
    .bind(ARCHIVED_VERSION_ONE_ID)
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(ARCHIVED_VERSION_TWO_ID)
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(EXISTING_PATH_ARCHIVED_VERSION_ID)
    .bind(EXISTING_PATH_ARCHIVED_DOCUMENT_ID)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_archive_events_and_relations(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r"
        INSERT INTO document_events (
            id, document_id, event_type, created_at, actor, actor_name,
            message, result, ip, user_agent
        )
        VALUES
            (
                910, ?, 'checkout', '2026-07-22T22:24:00Z',
                'fixture:alice', 'Alice Fixture',
                'Checked out Projects/Incoming/payload.bin',
                'ok', '192.0.2.21', 'Vault 2.1 migration fixture'
            ),
            (
                911, ?, 'release', '2026-07-22T22:25:00Z',
                'fixture:alice', 'Alice Fixture',
                'Released lock for Projects/Incoming/payload.bin',
                'ok', '192.0.2.21', 'Vault 2.1 migration fixture'
            ),
            (
                912, ?, 'archive', '2026-07-22T22:35:00Z',
                'fixture:alice', 'Alice Fixture',
                'Archived from Projects/Incoming/payload.bin', 'ok',
                '192.0.2.21', 'Vault 2.1 migration fixture'
            ),
            (
                913, ?, 'archive', '2026-07-22T22:36:00Z',
                'fixture:alice', 'Alice Fixture',
                'Archived from Visual Assets/Migration Previews/existing-path.bin',
                'ok', '192.0.2.21', 'Vault 2.1 migration fixture'
            )
        ",
    )
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(EXISTING_PATH_ARCHIVED_DOCUMENT_ID)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        INSERT INTO document_locks (
            id, document_id, locked_by, locked_by_name, locked_at, is_active,
            locked_ip, locked_user_agent, released_at, released_by
        )
        VALUES (
            1200, ?, 'fixture:alice', 'Alice Fixture',
            '2026-07-22T22:24:00Z', 0, '192.0.2.21',
            'Vault 2.1 migration fixture', '2026-07-22T22:25:00Z',
            'fixture:alice'
        )
        ",
    )
    .bind(ARCHIVED_DOCUMENT_ID)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r"
        INSERT INTO share_links (
            id, code, target_type, document_id, access_mode, created_by,
            created_by_name, created_by_user_id, created_at, item_type, item_id
        )
        VALUES (
            1300, 'v21-archived-document', 'document', ?, 'internal',
            'fixture:alice', 'Alice Fixture', 100,
            '2026-07-22T22:30:00Z', 'document', ?
        )
        ",
    )
    .bind(ARCHIVED_DOCUMENT_ID)
    .bind(ARCHIVED_DOCUMENT_ID)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn seed_archive_state_events(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO state_events (id, event_type, resources, created_at)
        VALUES
            (
                1010, 'batch.archive',
                '["contents","document_detail","my_edits","preferences","sidebar"]',
                '2026-07-22T22:35:00Z'
            ),
            (
                1011, 'batch.archive',
                '["contents","document_detail","my_edits","preferences","sidebar"]',
                '2026-07-22T22:36:00Z'
            )
        "#,
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn validate_fixture(connection: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    validate_integrity_and_history(connection).await?;
    validate_table_counts(connection).await?;
    validate_upload_parts(connection).await?;
    validate_upload_sessions(connection).await?;
    validate_archived_documents(connection).await
}

async fn validate_integrity_and_history(
    connection: &mut sqlx::SqliteConnection,
) -> anyhow::Result<()> {
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut *connection)
        .await?;
    anyhow::ensure!(
        integrity == "ok",
        "v2.1.0 fixture integrity check returned {integrity:?}"
    );
    let foreign_key_violations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut *connection)
            .await?;
    anyhow::ensure!(
        foreign_key_violations == 0,
        "v2.1.0 fixture has {foreign_key_violations} foreign-key violations"
    );
    let history = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT version, name, applied_at FROM schema_migrations ORDER BY version",
    )
    .fetch_all(&mut *connection)
    .await?;
    anyhow::ensure!(
        history
            == vec![(
                1,
                "content previews".to_string(),
                "2026-07-22T14:31:29Z".to_string(),
            )],
        "v2.1.0 fixture has unexpected migration history: {history:?}"
    );
    Ok(())
}

async fn validate_table_counts(connection: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    for (table, expected) in [
        ("upload_sessions", 5_i64),
        ("upload_parts", 4),
        ("documents", 3),
        ("document_versions", 4),
        ("document_events", 5),
        ("document_locks", 2),
        ("share_links", 1),
        ("blobs", 7),
        ("blob_locations", 7),
        ("state_events", 7),
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&mut *connection)
            .await?;
        anyhow::ensure!(
            count == expected,
            "v2.1.0 fixture table {table} contains {count} rows; expected {expected}"
        );
    }
    Ok(())
}

async fn validate_upload_parts(connection: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    let invalid_parts: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM upload_parts p
        JOIN upload_sessions s ON s.id = p.session_id
        WHERE p.part_number < 1
           OR p.part_number > s.part_count
           OR p.offset_bytes != (p.part_number - 1) * s.chunk_size
           OR p.size_bytes != CASE
                WHEN p.part_number < s.part_count THEN s.chunk_size
                ELSE s.total_size - p.offset_bytes
              END
           OR length(p.sha256) != 64
        ",
    )
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        invalid_parts == 0,
        "v2.1.0 fixture contains {invalid_parts} upload parts with invalid geometry"
    );
    Ok(())
}

async fn validate_upload_sessions(connection: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    let checkin_locks: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM upload_sessions s
        JOIN document_locks l
          ON l.document_id = s.document_id
         AND l.is_active = 1
         AND l.locked_by = s.created_by
        WHERE s.id IN (?, ?)
        ",
    )
    .bind(ACTIVE_CHECKIN_UPLOAD_ID)
    .bind(COMPLETING_CHECKIN_UPLOAD_ID)
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        checkin_locks == 2,
        "v2.1.0 check-in sessions do not resolve to their owner lock"
    );
    let resolved_complete_results: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM upload_sessions s
        JOIN documents d ON d.id = s.result_document_id
        JOIN document_versions v
         ON v.id = s.result_version_id
         AND v.document_id = d.id
        WHERE s.id = ?
          AND v.message = 'Uploaded migration-preview.png'
          AND v.upload_user_agent = s.upload_user_agent
          AND v.committed_at = s.completed_at
          AND length(json_extract(
                s.user_context,
                '$._upload_part_manifest_sha256'
              )) = 64
        ",
    )
    .bind(COMPLETE_CREATE_UPLOAD_ID)
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        resolved_complete_results == 1,
        "v2.1.0 completed upload result identities do not resolve"
    );
    let invalid_user_contexts: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM upload_sessions
        WHERE json_extract(user_context, '$.id') IS NOT created_by
           OR json_extract(user_context, '$.vault_user_id') IS NOT 100
           OR json_extract(user_context, '$.issuer') IS NOT 'https://fixture.invalid'
           OR json_extract(user_context, '$.subject') IS NOT 'alice'
           OR json_extract(user_context, '$.name') IS NOT created_by_name
           OR json_extract(user_context, '$.email') IS NOT 'alice@fixture.invalid'
           OR json_extract(user_context, '$.groups[0]') IS NOT 'Fixture Writers'
           OR json_extract(user_context, '$.is_admin') IS NOT 0
        ",
    )
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        invalid_user_contexts == 0,
        "v2.1.0 upload sessions contain {invalid_user_contexts} invalid user contexts"
    );
    Ok(())
}

async fn validate_archived_documents(
    connection: &mut sqlx::SqliteConnection,
) -> anyhow::Result<()> {
    let resolved_archive_versions: i64 = sqlx::query_scalar(
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
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        resolved_archive_versions == 2,
        "v2.1.0 archived document current-version identities do not resolve"
    );
    let valid_archive_shares: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM share_links
        WHERE document_id = ?
          AND item_type = 'document'
          AND item_id = document_id
        ",
    )
    .bind(ARCHIVED_DOCUMENT_ID)
    .fetch_one(&mut *connection)
    .await?;
    anyhow::ensure!(
        valid_archive_shares == 1,
        "v2.1.0 archived document share identity does not resolve"
    );
    Ok(())
}
