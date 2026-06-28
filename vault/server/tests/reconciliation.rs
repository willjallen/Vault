use std::sync::Arc;

use sha2::{Digest, Sha256};
use vault_server::auth::AuthSettings;
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{VAULT_ROOT_KEY, get_root_folder};
use vault_server::http::AppState;
use vault_server::reconciliation::storage_reconciliation_report;
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

async fn insert_stored_document(
    state: &AppState,
    folder_id: i64,
    name: &str,
    content: &[u8],
) -> (i64, i64, String) {
    let stored = state.storage.put_bytes(content).await.expect("stored blob");
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES (?, ?, ?)
        ",
    )
    .bind(&stored.hash_algo)
    .bind(&stored.digest)
    .bind(i64::try_from(stored.size_bytes).expect("blob size"))
    .execute(&state.db)
    .await
    .expect("blob")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO blob_locations (blob_id, backend, bucket, object_key)
        VALUES (?, ?, ?, ?)
        ",
    )
    .bind(blob_id)
    .bind(&stored.backend)
    .bind(&stored.bucket)
    .bind(&stored.object_key)
    .execute(&state.db)
    .await
    .expect("blob location");
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
    .execute(&state.db)
    .await
    .expect("document")
    .last_insert_rowid();
    let version_id = format!("reconcile-version-{document_id}");
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
    .bind(&version_id)
    .bind(document_id)
    .bind(blob_id)
    .bind(name)
    .execute(&state.db)
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
    .execute(&state.db)
    .await
    .expect("current version");
    (document_id, blob_id, stored.object_key)
}

fn local_storage(state: &AppState) -> LocalBlobStorage {
    LocalBlobStorage::new(state.config.objects_path(), &state.config.storage_prefix)
}

async fn reconciliation_report(
    state: &AppState,
    apply: bool,
) -> vault_server::reconciliation::StorageReconciliationReport {
    let storage = local_storage(state);
    storage_reconciliation_report(&state.db, &storage, apply)
        .await
        .expect("storage report")
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    lower_hex(&digest)
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[tokio::test]
async fn report_flags_corrupt_referenced_local_object_without_deleting_it() {
    let (state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let (_document_id, _blob_id, object_key) =
        insert_stored_document(&state, root.id, "kept.txt", b"trusted content").await;
    let object_path = local_storage(&state).root().join(&object_key);
    tokio::fs::write(&object_path, b"corrupt content")
        .await
        .expect("corrupt object");

    let report = reconciliation_report(&state, false).await;
    let applied = reconciliation_report(&state, true).await;

    assert_eq!(report.orphan_blob_ids, Vec::<i64>::new());
    assert_eq!(report.unreferenced_local_keys, Vec::<String>::new());
    assert_eq!(report.missing_local_keys, Vec::<String>::new());
    assert_eq!(report.corrupt_local_keys, vec![object_key]);
    assert_eq!(applied.deleted_local_keys, Vec::<String>::new());
    assert_eq!(
        tokio::fs::read(&object_path).await.expect("object bytes"),
        b"corrupt content",
    );
}

#[tokio::test]
async fn apply_restores_missing_local_location_metadata() {
    let (state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let (_document_id, blob_id, object_key) =
        insert_stored_document(&state, root.id, "kept.txt", b"referenced content").await;
    sqlx::query("DELETE FROM blob_locations WHERE blob_id = ? AND backend = 'local'")
        .bind(blob_id)
        .execute(&state.db)
        .await
        .expect("delete location");

    let applied = reconciliation_report(&state, true).await;
    let restored = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM blob_locations WHERE blob_id = ? AND backend = 'local' AND object_key = ?",
    )
    .bind(blob_id)
    .bind(&object_key)
    .fetch_one(&state.db)
    .await
    .expect("restored location");
    let after = reconciliation_report(&state, false).await;

    assert_eq!(applied.orphan_blob_ids, Vec::<i64>::new());
    assert_eq!(applied.unreferenced_local_keys, Vec::<String>::new());
    assert_eq!(applied.missing_local_keys, Vec::<String>::new());
    assert_eq!(applied.missing_local_location_keys, vec![object_key]);
    assert_eq!(applied.deleted_local_keys, Vec::<String>::new());
    assert_eq!(restored, 1);
    assert_eq!(after.missing_local_location_keys, Vec::<String>::new());
}

#[tokio::test]
async fn apply_removes_orphan_blob_metadata_and_local_object() {
    let (state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let (document_id, blob_id, object_key) =
        insert_stored_document(&state, root.id, "dead.txt", b"orphan me").await;
    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(document_id)
        .execute(&state.db)
        .await
        .expect("delete document");

    let before = reconciliation_report(&state, false).await;
    let applied = reconciliation_report(&state, true).await;
    let after = reconciliation_report(&state, false).await;
    let blob_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM blobs")
        .fetch_one(&state.db)
        .await
        .expect("blob count");
    let location_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM blob_locations")
        .fetch_one(&state.db)
        .await
        .expect("location count");

    assert_eq!(before.orphan_blob_ids, vec![blob_id]);
    assert_eq!(before.missing_local_keys, Vec::<String>::new());
    assert_eq!(applied.orphan_blob_ids, vec![blob_id]);
    assert_eq!(applied.deleted_local_keys, vec![object_key]);
    assert_eq!(after.orphan_blob_ids, Vec::<i64>::new());
    assert_eq!(after.unreferenced_local_keys, Vec::<String>::new());
    assert_eq!(blob_count, 0);
    assert_eq!(location_count, 0);
    assert_eq!(
        local_storage(&state)
            .list_object_keys()
            .await
            .expect("object keys"),
        Vec::<String>::new(),
    );
}

#[tokio::test]
async fn apply_preserves_remote_orphan_metadata_without_local_delete_support() {
    let (state, _temp_dir) = test_state().await;
    let blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', ?, 12)
        ",
    )
    .bind(sha256_hex(b"remote bytes"))
    .execute(&state.db)
    .await
    .expect("remote blob")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO blob_locations (blob_id, backend, bucket, object_key)
        VALUES (?, 's3', 'vault-prod', 'objects/sha256/remote-only')
        ",
    )
    .bind(blob_id)
    .execute(&state.db)
    .await
    .expect("remote location");

    let applied = reconciliation_report(&state, true).await;
    let blob_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM blobs")
        .fetch_one(&state.db)
        .await
        .expect("blob count");
    let location = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT blob_id, backend, object_key FROM blob_locations",
    )
    .fetch_one(&state.db)
    .await
    .expect("remote location");

    assert_eq!(applied.orphan_blob_ids, vec![blob_id]);
    assert_eq!(applied.deleted_local_keys, Vec::<String>::new());
    assert_eq!(blob_count, 1);
    assert_eq!(
        location,
        (
            blob_id,
            "s3".to_string(),
            "objects/sha256/remote-only".to_string(),
        ),
    );
}
