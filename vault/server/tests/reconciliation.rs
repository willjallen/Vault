use std::io::SeekFrom;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::time::timeout;
use vault_server::auth::AuthSettings;
use vault_server::blob_lifecycle::{
    BlobLifecycleError, PendingBlobPublication, begin_blob_publication,
};
use vault_server::config::Config;
use vault_server::db;
use vault_server::folders::{VAULT_ROOT_KEY, get_root_folder};
use vault_server::http::AppState;
use vault_server::reconciliation::{
    apply_storage_reconciliation_plan, plan_storage_reconciliation, storage_reconciliation_report,
    storage_reconciliation_report_with_multipart_part_age,
    sweep_unreferenced_multipart_parts_with_options,
};
use vault_server::storage::{
    BlobReadRange, BlobWriteKind, LocalBlobStorage, STORAGE_CHUNK_SIZE, StorageError, StoredBlob,
};

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
    state.local_storage_maintenance.clone()
}

async fn commit_document_publication(
    state: &AppState,
    publication: &PendingBlobPublication,
    stored: &StoredBlob,
    name: &str,
) -> (i64, i64) {
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let mut transaction = state
        .db
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("publication transaction");
    let blob_id = publication
        .prepare_metadata_in_tx(&mut transaction, stored)
        .await
        .expect("prepare publication metadata");
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, ?, 'admin', 'Admin', 'admin')
        ",
    )
    .bind(root.id)
    .bind(name)
    .execute(&mut *transaction)
    .await
    .expect("publication document")
    .last_insert_rowid();
    let version_id = format!("publication-version-{document_id}");
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
    .execute(&mut *transaction)
    .await
    .expect("publication version");
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
    .execute(&mut *transaction)
    .await
    .expect("publication current version");
    publication
        .finish_metadata_in_tx(&mut transaction)
        .await
        .expect("finish publication metadata");
    transaction
        .commit()
        .await
        .expect("commit publication metadata");
    (document_id, blob_id)
}

async fn overwrite_last_byte(path: &std::path::Path, byte: u8) {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .expect("open object for corruption");
    let length = file.metadata().await.expect("object metadata").len();
    assert!(length > 0);
    file.seek(SeekFrom::Start(length - 1))
        .await
        .expect("seek object");
    file.write_all(&[byte]).await.expect("corrupt object");
    file.flush().await.expect("flush corrupt object");
}

async fn local_location_count(state: &AppState, blob_ids: [i64; 3]) -> i64 {
    sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM blob_locations
        WHERE backend = 'local'
          AND blob_id IN (?, ?, ?)
        ",
    )
    .bind(blob_ids[0])
    .bind(blob_ids[1])
    .bind(blob_ids[2])
    .fetch_one(&state.db)
    .await
    .expect("local location count")
}

async fn insert_referenced_multipart_blob(state: &AppState, stored: &StoredBlob, name: &str) {
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let blob_id = sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES (?, ?, ?)")
        .bind(&stored.hash_algo)
        .bind(&stored.digest)
        .bind(i64::try_from(stored.size_bytes).expect("blob size"))
        .execute(&state.db)
        .await
        .expect("multipart blob")
        .last_insert_rowid();
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 'local', '', ?)",
    )
    .bind(blob_id)
    .bind(&stored.object_key)
    .execute(&state.db)
    .await
    .expect("multipart location");
    let document_id = sqlx::query(
        "INSERT INTO documents (folder_id, name, created_by, latest_modified_by) VALUES (?, ?, 'admin', 'admin')",
    )
    .bind(root.id)
    .bind(name)
    .execute(&state.db)
    .await
    .expect("multipart document")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO document_versions
            (id, document_id, blob_id, version_number, committed_by, message, original_filename, created_via)
        VALUES
            (?, ?, ?, 1, 'admin', 'multipart', ?, 'upload')
        ",
    )
    .bind(format!("multipart-version-{document_id}"))
    .bind(document_id)
    .bind(blob_id)
    .bind(name)
    .execute(&state.db)
    .await
    .expect("multipart version");
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
async fn reconciliation_streams_multiframe_regular_objects_for_integrity() {
    let (state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let content = (0..(STORAGE_CHUNK_SIZE * 2 + 17))
        .map(|index| u8::try_from(index % 251).expect("test byte"))
        .collect::<Vec<_>>();
    let (_document_id, _blob_id, object_key) =
        insert_stored_document(&state, root.id, "large-regular.bin", &content).await;
    let storage = local_storage(&state);

    let clean = storage_reconciliation_report(&state.db, &storage, false)
        .await
        .expect("clean multiframe report");
    assert!(clean.corrupt_local_keys.is_empty());

    overwrite_last_byte(
        &storage.root().join(&object_key),
        content[content.len() - 1] ^ 0xff,
    )
    .await;
    let corrupt = storage_reconciliation_report(&state.db, &storage, false)
        .await
        .expect("corrupt multiframe report");

    assert_eq!(corrupt.corrupt_local_keys, vec![object_key]);
}

#[tokio::test]
async fn reconciliation_accepts_zero_bytes_and_rejects_negative_blob_sizes() {
    let (state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let (_empty_document_id, _empty_blob_id, empty_key) =
        insert_stored_document(&state, root.id, "empty.bin", b"").await;
    let (_invalid_document_id, invalid_blob_id, invalid_key) =
        insert_stored_document(&state, root.id, "invalid.bin", b"x").await;
    sqlx::query("UPDATE blobs SET size_bytes = -1 WHERE id = ?")
        .bind(invalid_blob_id)
        .execute(&state.db)
        .await
        .expect("negative blob size");

    let report = reconciliation_report(&state, false).await;

    assert_eq!(report.corrupt_local_keys, vec![invalid_key]);
    assert!(!report.corrupt_local_keys.contains(&empty_key));
    assert!(report.missing_local_keys.is_empty());
}

#[tokio::test]
async fn reconciliation_streams_multiframe_multipart_objects_for_integrity() {
    let (state, _temp_dir) = test_state().await;
    let storage = local_storage(&state);
    let source_dir = state.config.data_dir.join("multiframe-parts");
    tokio::fs::create_dir_all(&source_dir)
        .await
        .expect("multipart source directory");
    let first = vec![b'a'; STORAGE_CHUNK_SIZE - 7];
    let second = vec![b'b'; STORAGE_CHUNK_SIZE + 19];
    let third = vec![b'c'; 23];
    let mut content = Vec::with_capacity(first.len() + second.len() + third.len());
    content.extend_from_slice(&first);
    content.extend_from_slice(&second);
    content.extend_from_slice(&third);
    let part_paths = [
        source_dir.join("first.part"),
        source_dir.join("second.part"),
        source_dir.join("third.part"),
    ];
    for (path, bytes) in part_paths.iter().zip([&first, &second, &third]) {
        tokio::fs::write(path, bytes)
            .await
            .expect("multipart source");
    }
    let stored = storage
        .put_part_files(&part_paths, Some(&sha256_hex(&content)))
        .await
        .expect("multiframe multipart object");
    insert_referenced_multipart_blob(&state, &stored, "large-multipart.bin").await;

    let clean = storage_reconciliation_report(&state.db, &storage, false)
        .await
        .expect("clean multiframe multipart report");
    assert!(clean.corrupt_local_keys.is_empty());

    let manifest = storage
        .read_multipart_manifest(&stored.object_key)
        .await
        .expect("multipart manifest");
    overwrite_last_byte(&manifest.parts[1].path, b'x').await;
    let corrupt = storage_reconciliation_report(&state.db, &storage, false)
        .await
        .expect("corrupt multiframe multipart report");

    assert_eq!(corrupt.corrupt_local_keys, vec![stored.object_key]);
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
    let reapplied = reconciliation_report(&state, true).await;
    let restored_after_reapply = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM blob_locations WHERE blob_id = ? AND backend = 'local' AND object_key = ?",
    )
    .bind(blob_id)
    .bind(&object_key)
    .fetch_one(&state.db)
    .await
    .expect("idempotent restored location");

    assert_eq!(applied.orphan_blob_ids, Vec::<i64>::new());
    assert_eq!(applied.unreferenced_local_keys, Vec::<String>::new());
    assert_eq!(applied.missing_local_keys, Vec::<String>::new());
    assert_eq!(applied.missing_local_location_keys, vec![object_key]);
    assert_eq!(applied.deleted_local_keys, Vec::<String>::new());
    assert_eq!(restored, 1);
    assert_eq!(after.missing_local_location_keys, Vec::<String>::new());
    assert_eq!(reapplied.missing_local_location_keys, Vec::<String>::new());
    assert_eq!(reapplied.deleted_local_keys, Vec::<String>::new());
    assert_eq!(restored_after_reapply, 1);
}

#[tokio::test]
async fn stale_plan_skips_missing_location_restore_when_live_state_changed() {
    let (state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let (removed_document_id, removed_blob_id, removed_key) = insert_stored_document(
        &state,
        root.id,
        "removed-reference.txt",
        b"removed reference",
    )
    .await;
    let identity_content = b"changed identity";
    let (_identity_document_id, identity_blob_id, identity_key) =
        insert_stored_document(&state, root.id, "changed-identity.txt", identity_content).await;
    let (_corrupt_document_id, corrupt_blob_id, corrupt_key) =
        insert_stored_document(&state, root.id, "changed-content.txt", b"changed content").await;
    for blob_id in [removed_blob_id, identity_blob_id, corrupt_blob_id] {
        sqlx::query("DELETE FROM blob_locations WHERE blob_id = ? AND backend = 'local'")
            .bind(blob_id)
            .execute(&state.db)
            .await
            .expect("delete recoverable location");
    }
    let storage = local_storage(&state);
    let plan = plan_storage_reconciliation(&state.db, &storage)
        .await
        .expect("missing-location plan");
    let mut planned_missing = plan.report().missing_local_location_keys.clone();
    planned_missing.sort();
    let mut expected_missing = vec![removed_key, identity_key.clone(), corrupt_key.clone()];
    expected_missing.sort();
    assert_eq!(planned_missing, expected_missing);

    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(removed_document_id)
        .execute(&state.db)
        .await
        .expect("remove live reference");
    sqlx::query("UPDATE blobs SET hash = ? WHERE id = ?")
        .bind(sha256_hex(b"different identity"))
        .bind(identity_blob_id)
        .execute(&state.db)
        .await
        .expect("change blob identity");
    let claiming_publication = begin_blob_publication(
        &state.db,
        &storage,
        "sha256",
        &sha256_hex(identity_content),
        identity_content.len() as u64,
        BlobWriteKind::Bytes,
    )
    .await
    .expect("begin key-claiming publication");
    let claimed_stored = claiming_publication
        .run_storage(storage.put_bytes(identity_content))
        .await
        .expect("write key-claiming publication");
    let (_claiming_document_id, claiming_blob_id) = commit_document_publication(
        &state,
        &claiming_publication,
        &claimed_stored,
        "claimed-key.txt",
    )
    .await;
    assert_ne!(claiming_blob_id, identity_blob_id);
    assert_eq!(claimed_stored.object_key, identity_key);
    overwrite_last_byte(&storage.root().join(&corrupt_key), b'x').await;

    let applied = apply_storage_reconciliation_plan(&state.db, &storage, plan)
        .await
        .expect("apply stale missing-location plan");
    let restored_count =
        local_location_count(&state, [removed_blob_id, identity_blob_id, corrupt_blob_id]).await;
    let remaining_references = sqlx::query_scalar::<_, i64>(
        r"
        SELECT COUNT(*)
        FROM document_versions
        WHERE blob_id IN (?, ?)
        ",
    )
    .bind(identity_blob_id)
    .bind(corrupt_blob_id)
    .fetch_one(&state.db)
    .await
    .expect("remaining references");
    let claimed_location_blob_id = sqlx::query_scalar::<_, i64>(
        "SELECT blob_id FROM blob_locations WHERE backend = 'local' AND object_key = ?",
    )
    .bind(&identity_key)
    .fetch_one(&state.db)
    .await
    .expect("claimed local key");

    assert_eq!(restored_count, 0);
    assert_eq!(remaining_references, 2);
    assert_eq!(claimed_location_blob_id, claiming_blob_id);
    assert!(applied.deleted_local_keys.is_empty());
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
async fn stale_plan_preserves_orphan_claimed_by_same_digest_publication() {
    let (state, _temp_dir) = test_state().await;
    let root = get_root_folder(&state.db, VAULT_ROOT_KEY)
        .await
        .expect("root");
    let content = b"same digest publication";
    let (orphan_document_id, blob_id, object_key) =
        insert_stored_document(&state, root.id, "orphan.txt", content).await;
    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(orphan_document_id)
        .execute(&state.db)
        .await
        .expect("make blob orphan");
    let storage = local_storage(&state);
    let plan = plan_storage_reconciliation(&state.db, &storage)
        .await
        .expect("orphan reconciliation plan");
    assert_eq!(plan.report().orphan_blob_ids, vec![blob_id]);

    let publication = begin_blob_publication(
        &state.db,
        &storage,
        "sha256",
        &sha256_hex(content),
        content.len() as u64,
        BlobWriteKind::Bytes,
    )
    .await
    .expect("begin same-digest publication");
    assert_eq!(publication.blob_id(), blob_id);
    let stored = publication
        .run_storage(storage.put_bytes(content))
        .await
        .expect("write same-digest publication");
    let (document_id, published_blob_id) =
        commit_document_publication(&state, &publication, &stored, "live.txt").await;
    assert_eq!(published_blob_id, blob_id);

    let applied = apply_storage_reconciliation_plan(&state.db, &storage, plan)
        .await
        .expect("apply stale orphan plan");
    let blob_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM blobs WHERE id = ?")
        .bind(blob_id)
        .fetch_one(&state.db)
        .await
        .expect("live blob");
    let location_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM blob_locations WHERE blob_id = ? AND backend = 'local' AND object_key = ?",
    )
    .bind(blob_id)
    .bind(&object_key)
    .fetch_one(&state.db)
    .await
    .expect("live location");
    let reference_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_versions WHERE document_id = ? AND blob_id = ?",
    )
    .bind(document_id)
    .bind(blob_id)
    .fetch_one(&state.db)
    .await
    .expect("live reference");

    assert!(applied.deleted_local_keys.is_empty());
    assert_eq!((blob_count, location_count, reference_count), (1, 1, 1));
    assert_eq!(
        storage
            .read_bytes(&object_key)
            .await
            .expect("live object bytes"),
        content,
    );
}

#[tokio::test]
async fn stale_plan_preserves_raw_key_claimed_by_publication() {
    let (state, _temp_dir) = test_state().await;
    let storage = local_storage(&state);
    let content = b"raw publication race";
    let raw = storage.put_bytes(content).await.expect("raw local object");
    let plan = plan_storage_reconciliation(&state.db, &storage)
        .await
        .expect("raw-key reconciliation plan");
    assert_eq!(
        plan.report().unreferenced_local_keys,
        vec![raw.object_key.clone()]
    );

    let publication = begin_blob_publication(
        &state.db,
        &storage,
        "sha256",
        &sha256_hex(content),
        content.len() as u64,
        BlobWriteKind::Bytes,
    )
    .await
    .expect("begin raw-key publication");
    let stored = publication
        .run_storage(storage.put_bytes(content))
        .await
        .expect("write raw-key publication");
    let (document_id, blob_id) =
        commit_document_publication(&state, &publication, &stored, "claimed-raw.txt").await;

    let applied = apply_storage_reconciliation_plan(&state.db, &storage, plan)
        .await
        .expect("apply stale raw-key plan");
    let location_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM blob_locations WHERE blob_id = ? AND backend = 'local' AND object_key = ?",
    )
    .bind(blob_id)
    .bind(&raw.object_key)
    .fetch_one(&state.db)
    .await
    .expect("claimed raw location");
    let reference_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM document_versions WHERE document_id = ? AND blob_id = ?",
    )
    .bind(document_id)
    .bind(blob_id)
    .fetch_one(&state.db)
    .await
    .expect("claimed raw reference");

    assert!(applied.deleted_local_keys.is_empty());
    assert_eq!((location_count, reference_count), (1, 1));
    assert_eq!(
        storage
            .read_bytes(&raw.object_key)
            .await
            .expect("claimed raw bytes"),
        content,
    );
}

#[tokio::test]
async fn apply_deletes_untracked_key_and_finalizes_reservation_metadata() {
    let (state, _temp_dir) = test_state().await;
    let storage = local_storage(&state);
    let raw = storage
        .put_bytes(b"delete untracked object")
        .await
        .expect("untracked local object");
    let plan = plan_storage_reconciliation(&state.db, &storage)
        .await
        .expect("untracked-key plan");

    let applied = apply_storage_reconciliation_plan(&state.db, &storage, plan)
        .await
        .expect("apply untracked-key plan");
    let metadata_counts = sqlx::query_as::<_, (i64, i64)>(
        "SELECT (SELECT COUNT(*) FROM blobs), (SELECT COUNT(*) FROM blob_locations)",
    )
    .fetch_one(&state.db)
    .await
    .expect("reservation metadata counts");

    assert_eq!(applied.deleted_local_keys, vec![raw.object_key.clone()]);
    assert!(matches!(
        storage.read_bytes(&raw.object_key).await,
        Err(StorageError::NotFound)
    ));
    assert_eq!(metadata_counts, (0, 0));
}

#[tokio::test]
async fn untracked_key_tombstone_blocks_publication_until_apply_finishes() {
    let (state, _temp_dir) = test_state().await;
    let storage = local_storage(&state);
    let content = b"collector wins";
    let raw = storage.put_bytes(content).await.expect("untracked object");
    let plan = plan_storage_reconciliation(&state.db, &storage)
        .await
        .expect("untracked-key plan");
    let held_stream = storage
        .stream_range(
            &raw.object_key,
            BlobReadRange {
                expected_size: raw.size_bytes,
                offset: 0,
                length: raw.size_bytes,
            },
        )
        .await
        .expect("held object stream");

    let apply_pool = state.db.clone();
    let apply_storage = storage.clone();
    let apply_task = tokio::spawn(async move {
        apply_storage_reconciliation_plan(&apply_pool, &apply_storage, plan).await
    });
    timeout(Duration::from_secs(5), async {
        loop {
            let tombstone = sqlx::query_scalar::<_, i64>(
                r"
                SELECT 1
                FROM blob_locations
                WHERE object_key = ?
                  AND backend GLOB '_vault_deleting:*:local'
                ",
            )
            .bind(&raw.object_key)
            .fetch_optional(&state.db)
            .await
            .expect("deletion tombstone");
            if tombstone.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("deletion tombstone timeout");

    let blocked = begin_blob_publication(
        &state.db,
        &storage,
        "sha256",
        &raw.digest,
        raw.size_bytes,
        BlobWriteKind::Bytes,
    )
    .await
    .expect_err("deletion tombstone must block publication");
    assert!(matches!(blocked, BlobLifecycleError::DeletionInProgress));

    drop(held_stream);
    let applied = timeout(Duration::from_secs(10), apply_task)
        .await
        .expect("reconciliation apply timeout")
        .expect("reconciliation apply task")
        .expect("reconciliation apply");
    assert_eq!(applied.deleted_local_keys, vec![raw.object_key.clone()]);

    let publication = begin_blob_publication(
        &state.db,
        &storage,
        "sha256",
        &raw.digest,
        raw.size_bytes,
        BlobWriteKind::Bytes,
    )
    .await
    .expect("retry publication");
    let stored = publication
        .run_storage(storage.put_bytes(content))
        .await
        .expect("retry storage write");
    commit_document_publication(&state, &publication, &stored, "collector-retry.txt").await;

    assert_eq!(
        storage
            .read_bytes(&raw.object_key)
            .await
            .expect("republished bytes"),
        content,
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

#[tokio::test]
async fn reconciliation_removes_only_aged_parts_outside_the_current_layout() {
    let (state, _temp_dir) = test_state().await;
    let storage = local_storage(&state);
    let source_dir = state.config.data_dir.join("multipart-sources");
    tokio::fs::create_dir_all(&source_dir)
        .await
        .expect("source directory");
    let old_first = source_dir.join("old-first.part");
    let old_second = source_dir.join("old-second.part");
    let new_first = source_dir.join("new-first.part");
    let new_second = source_dir.join("new-second.part");
    for (path, bytes) in [
        (&old_first, b"abc".as_slice()),
        (&old_second, b"defgh".as_slice()),
        (&new_first, b"abcd".as_slice()),
        (&new_second, b"efgh".as_slice()),
    ] {
        tokio::fs::write(path, bytes).await.expect("source part");
    }
    let digest = sha256_hex(b"abcdefgh");
    let old = storage
        .put_part_files(&[old_first, old_second], Some(&digest))
        .await
        .expect("old layout");
    let old_manifest = storage
        .read_multipart_manifest(&old.object_key)
        .await
        .expect("old manifest");
    tokio::fs::remove_file(storage.root().join(&old.object_key))
        .await
        .expect("remove old manifest");
    let current = storage
        .put_part_files(&[new_first, new_second], Some(&digest))
        .await
        .expect("current layout");
    let current_manifest = storage
        .read_multipart_manifest(&current.object_key)
        .await
        .expect("current manifest");
    insert_referenced_multipart_blob(&state, &current, "multipart.bin").await;

    let mut dry_run = storage_reconciliation_report_with_multipart_part_age(
        &state.db,
        &storage,
        false,
        Duration::ZERO,
    )
    .await
    .expect("dry-run reconciliation");
    let mut applied = storage_reconciliation_report_with_multipart_part_age(
        &state.db,
        &storage,
        true,
        Duration::ZERO,
    )
    .await
    .expect("apply reconciliation");
    let mut old_keys = old_manifest
        .parts
        .iter()
        .map(|part| part.object_key.clone())
        .collect::<Vec<_>>();
    old_keys.sort();
    dry_run.unreferenced_multipart_part_keys.sort();
    applied.deleted_multipart_part_keys.sort();

    assert_eq!(dry_run.unreferenced_multipart_part_keys, old_keys);
    assert_eq!(applied.deleted_multipart_part_keys, old_keys);
    assert!(old_manifest.parts.iter().all(|part| !part.path.exists()));
    assert!(
        current_manifest
            .parts
            .iter()
            .all(|part| part.path.is_file())
    );
    assert_eq!(
        storage
            .read_bytes(&current.object_key)
            .await
            .expect("current layout remains readable"),
        b"abcdefgh"
    );
}

#[tokio::test]
async fn multipart_part_sweep_respects_age_and_pending_publications() {
    let (state, _temp_dir) = test_state().await;
    let storage = local_storage(&state);
    let source = state.config.data_dir.join("orphan-source.part");
    tokio::fs::write(&source, b"orphan bytes")
        .await
        .expect("orphan source");
    let digest = sha256_hex(b"orphan bytes");
    let stored = storage
        .put_part_files(&[source], Some(&digest))
        .await
        .expect("multipart object");
    let manifest = storage
        .read_multipart_manifest(&stored.object_key)
        .await
        .expect("manifest");
    tokio::fs::remove_file(storage.root().join(&stored.object_key))
        .await
        .expect("remove manifest");
    let blob_id =
        sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', ?, ?)")
            .bind(&digest)
            .bind(i64::try_from(stored.size_bytes).expect("size"))
            .execute(&state.db)
            .await
            .expect("pending blob")
            .last_insert_rowid();
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, '_vault_pending:test:local', '', ?)",
    )
    .bind(blob_id)
    .bind(&stored.object_key)
    .execute(&state.db)
    .await
    .expect("pending location");

    let young = sweep_unreferenced_multipart_parts_with_options(
        &state.db,
        &storage,
        Duration::from_hours(1),
        1024,
    )
    .await
    .expect("young sweep");
    let pending =
        sweep_unreferenced_multipart_parts_with_options(&state.db, &storage, Duration::ZERO, 1024)
            .await
            .expect("pending sweep");
    assert!(young.deleted_multipart_part_keys.is_empty());
    assert!(pending.deleted_multipart_part_keys.is_empty());
    assert!(manifest.parts[0].path.is_file());

    sqlx::query("DELETE FROM blob_locations WHERE blob_id = ?")
        .bind(blob_id)
        .execute(&state.db)
        .await
        .expect("remove pending lease");
    sqlx::query("DELETE FROM blobs WHERE id = ?")
        .bind(blob_id)
        .execute(&state.db)
        .await
        .expect("remove pending blob");
    let deleted =
        sweep_unreferenced_multipart_parts_with_options(&state.db, &storage, Duration::ZERO, 1024)
            .await
            .expect("unleased sweep");
    assert_eq!(
        deleted.deleted_multipart_part_keys,
        vec![manifest.parts[0].object_key.clone()]
    );
    assert!(!manifest.parts[0].path.exists());
}

#[tokio::test]
async fn multipart_part_sweep_preserves_referenced_parts_when_the_manifest_is_missing() {
    let (state, _temp_dir) = test_state().await;
    let storage = local_storage(&state);
    let source = state.config.data_dir.join("referenced-source.part");
    tokio::fs::write(&source, b"referenced bytes")
        .await
        .expect("referenced source");
    let stored = storage
        .put_part_files(&[source], Some(&sha256_hex(b"referenced bytes")))
        .await
        .expect("referenced multipart object");
    let manifest = storage
        .read_multipart_manifest(&stored.object_key)
        .await
        .expect("referenced manifest");
    insert_referenced_multipart_blob(&state, &stored, "referenced.bin").await;
    tokio::fs::remove_file(storage.root().join(&stored.object_key))
        .await
        .expect("remove referenced manifest");

    let swept =
        sweep_unreferenced_multipart_parts_with_options(&state.db, &storage, Duration::ZERO, 1024)
            .await
            .expect("referenced missing-manifest sweep");

    assert!(swept.deleted_multipart_part_keys.is_empty());
    assert!(manifest.parts[0].path.is_file());
}
