use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use sqlx::{Sqlite, SqlitePool, Transaction};
use vault_server::blob_lifecycle::{
    BlobLifecycleError, PendingBlobPublication, begin_blob_publication, collect_unreferenced_blobs,
};
use vault_server::db;
use vault_server::storage::{
    BlobByteStream, BlobReadRange, BlobStorageBackend, BlobWriteKind, LocalBlobStorage,
    StorageError, StoredBlob, sha256_hex,
};

struct TestState {
    temp_dir: tempfile::TempDir,
    pool: SqlitePool,
    storage: LocalBlobStorage,
}

#[derive(Debug, Clone)]
struct FailOnceDeleteStorage {
    inner: LocalBlobStorage,
    delete_attempts: Arc<AtomicUsize>,
    always_fail_key: Option<String>,
}

#[derive(Debug, Clone)]
struct DeleteThenErrorOnceStorage {
    inner: LocalBlobStorage,
    delete_attempts: Arc<AtomicUsize>,
}

#[async_trait]
impl BlobStorageBackend for FailOnceDeleteStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
    }

    async fn put_bytes(&self, data: &[u8]) -> Result<StoredBlob, StorageError> {
        self.inner.put_bytes(data).await
    }

    async fn put_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        self.inner.put_file(source_path, digest, size_bytes).await
    }

    async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        self.inner.put_part_files(part_paths, expected_digest).await
    }

    async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.read_bytes(object_key).await
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        self.inner.read_range(object_key, start, end).await
    }

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner.stream_range(object_key, range).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        if self.always_fail_key.as_deref() == Some(object_key) {
            self.delete_attempts.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::Remote(
                "injected permanent delete failure".to_string(),
            ));
        }
        if self.delete_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(StorageError::Remote(
                "injected first-delete failure".to_string(),
            ));
        }
        self.inner.delete_object(object_key).await
    }
}

#[async_trait]
impl BlobStorageBackend for DeleteThenErrorOnceStorage {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn bucket(&self) -> &str {
        self.inner.bucket()
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        self.inner.ensure().await
    }

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        self.inner.planned_object_key(hash_algo, digest, write_kind)
    }

    async fn put_bytes(&self, data: &[u8]) -> Result<StoredBlob, StorageError> {
        self.inner.put_bytes(data).await
    }

    async fn put_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        self.inner.put_file(source_path, digest, size_bytes).await
    }

    async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        self.inner.put_part_files(part_paths, expected_digest).await
    }

    async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.read_bytes(object_key).await
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        self.inner.read_range(object_key, start, end).await
    }

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.inner.stream_range(object_key, range).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_object_keys().await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.inner.delete_object(object_key).await?;
        if self.delete_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(StorageError::Remote(
                "injected lost delete response".to_string(),
            ));
        }
        Ok(())
    }
}

async fn test_state() -> TestState {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp_dir.path().join("vault.db"))
        .await
        .expect("database");
    let storage = LocalBlobStorage::new(temp_dir.path().join("objects"), "objects");
    TestState {
        temp_dir,
        pool,
        storage,
    }
}

async fn begin_and_store(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    content: &[u8],
) -> (PendingBlobPublication, StoredBlob) {
    let digest = sha256_hex(content);
    let publication = begin_blob_publication(
        pool,
        storage,
        "sha256",
        &digest,
        content.len() as u64,
        BlobWriteKind::Bytes,
    )
    .await
    .expect("begin publication");
    let stored = publication
        .run_storage(storage.put_bytes(content))
        .await
        .expect("store bytes");
    assert_eq!(stored, *publication.planned());
    (publication, stored)
}

async fn publish_unreferenced(
    pool: &SqlitePool,
    storage: &dyn BlobStorageBackend,
    content: &[u8],
) -> (i64, StoredBlob) {
    let (publication, stored) = begin_and_store(pool, storage, content).await;
    let mut transaction = pool.begin().await.expect("metadata transaction");
    let blob_id = publication
        .prepare_metadata_in_tx(&mut transaction, &stored)
        .await
        .expect("prepare blob metadata");
    publication
        .finish_metadata_in_tx(&mut transaction)
        .await
        .expect("finish blob metadata");
    transaction.commit().await.expect("commit blob metadata");
    (blob_id, stored)
}

async fn insert_document_reference(transaction: &mut Transaction<'_, Sqlite>, blob_id: i64) -> i64 {
    let folder_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&mut **transaction)
            .await
            .expect("vault root");
    let document_id = sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES
            (?, 'shared.txt', 'owner', 'Owner', 'owner')
        ",
    )
    .bind(folder_id)
    .execute(&mut **transaction)
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
            ('shared-version', ?, ?, 1, 'owner', 'Owner', 'Uploaded shared.txt',
             'text/plain', 'shared.txt', 'upload')
        ",
    )
    .bind(document_id)
    .bind(blob_id)
    .execute(&mut **transaction)
    .await
    .expect("document version");
    document_id
}

async fn insert_export_reference(
    transaction: &mut Transaction<'_, Sqlite>,
    blob_id: i64,
    stored: &StoredBlob,
) {
    sqlx::query(
        r"
        INSERT INTO export_jobs
            (id, status, filename, total_items, total_bytes, created_by, created_by_name,
             user_context, expires_at)
        VALUES
            ('shared-export', 'complete', 'shared.zip', 1, ?, 'owner', 'Owner', '{}',
             '2999-01-01T00:00:00Z')
        ",
    )
    .bind(i64::try_from(stored.size_bytes).expect("export size"))
    .execute(&mut **transaction)
    .await
    .expect("export job");
    sqlx::query(
        r"
        INSERT INTO export_artifacts
            (job_id, blob_id, filename, mime_type, size_bytes, hash_algo, hash, expires_at)
        VALUES
            ('shared-export', ?, 'shared.zip', 'application/zip', ?, ?, ?,
             '2999-01-01T00:00:00Z')
        ",
    )
    .bind(blob_id)
    .bind(i64::try_from(stored.size_bytes).expect("artifact size"))
    .bind(&stored.hash_algo)
    .bind(&stored.digest)
    .execute(&mut **transaction)
    .await
    .expect("export artifact");
}

async fn blob_metadata_counts(pool: &SqlitePool, blob_id: i64) -> (i64, i64) {
    sqlx::query_as(
        r"
        SELECT
            (SELECT COUNT(*) FROM blobs WHERE id = ?),
            (SELECT COUNT(*) FROM blob_locations WHERE blob_id = ?)
        ",
    )
    .bind(blob_id)
    .bind(blob_id)
    .fetch_one(pool)
    .await
    .expect("blob metadata counts")
}

#[tokio::test]
async fn collection_removes_local_object_and_metadata() {
    let state = test_state().await;
    let (blob_id, stored) =
        publish_unreferenced(&state.pool, &state.storage, b"disposable bytes").await;

    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (1, 1));
    assert_eq!(
        state
            .storage
            .read_bytes(&stored.object_key)
            .await
            .expect("stored bytes"),
        b"disposable bytes",
    );

    let result = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("garbage collection");

    assert_eq!(result.deleted_blob_ids, vec![blob_id]);
    assert_eq!(result.deleted_objects, vec![stored.object_key.clone()]);
    assert!(result.deferred_objects.is_empty());
    assert!(result.failures.is_empty());
    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (0, 0));
    assert!(matches!(
        state.storage.read_bytes(&stored.object_key).await,
        Err(StorageError::NotFound)
    ));
}

#[tokio::test]
async fn active_publication_lease_prevents_gc_until_reference_commit() {
    let state = test_state().await;
    let (publication, stored) =
        begin_and_store(&state.pool, &state.storage, b"publication race bytes").await;
    let blob_id = publication.blob_id();

    let while_publication_is_active = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("collection during publication");
    assert!(while_publication_is_active.deleted_blob_ids.is_empty());
    assert!(while_publication_is_active.deleted_objects.is_empty());

    let mut transaction = state
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("metadata transaction");
    publication
        .prepare_metadata_in_tx(&mut transaction, &stored)
        .await
        .expect("prepare metadata");
    insert_document_reference(&mut transaction, blob_id).await;
    publication
        .finish_metadata_in_tx(&mut transaction)
        .await
        .expect("finish metadata");
    transaction.commit().await.expect("commit reference");
    drop(publication);

    let after_reference_commit = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("collection after reference commit");
    assert!(after_reference_commit.deleted_blob_ids.is_empty());
    assert!(after_reference_commit.deleted_objects.is_empty());
    assert_eq!(
        state
            .storage
            .read_bytes(&stored.object_key)
            .await
            .expect("referenced bytes"),
        b"publication race bytes",
    );
}

#[tokio::test]
async fn collection_drains_every_known_location_before_removing_blob_metadata() {
    let state = test_state().await;
    let (blob_id, stored) =
        publish_unreferenced(&state.pool, &state.storage, b"replicated bytes").await;
    let replica_key = "objects/replicas/replicated-bytes";
    let replica_path = state.storage.root().join(replica_key);
    tokio::fs::create_dir_all(replica_path.parent().expect("replica parent"))
        .await
        .expect("replica directory");
    tokio::fs::write(&replica_path, b"replicated bytes")
        .await
        .expect("replica object");
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 'local', '', ?)",
    )
    .bind(blob_id)
    .bind(replica_key)
    .execute(&state.pool)
    .await
    .expect("replica location");

    let result = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("garbage collection");

    assert_eq!(result.deleted_blob_ids, vec![blob_id]);
    assert_eq!(
        result.deleted_objects,
        vec![replica_key.to_string(), stored.object_key.clone()]
    );
    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (0, 0));
    assert!(tokio::fs::metadata(replica_path).await.is_err());
    assert!(matches!(
        state.storage.read_bytes(&stored.object_key).await,
        Err(StorageError::NotFound)
    ));
}

#[tokio::test]
async fn collection_removes_multipart_manifest_and_all_hidden_parts() {
    let state = test_state().await;
    let first = state.temp_dir.path().join("first.part");
    let second = state.temp_dir.path().join("second.part");
    tokio::fs::write(&first, b"multipart ")
        .await
        .expect("first part");
    tokio::fs::write(&second, b"bytes")
        .await
        .expect("second part");
    let content = b"multipart bytes";
    let digest = sha256_hex(content);
    let publication = begin_blob_publication(
        &state.pool,
        &state.storage,
        "sha256",
        &digest,
        content.len() as u64,
        BlobWriteKind::PartFiles,
    )
    .await
    .expect("begin multipart publication");
    let stored = publication
        .run_storage(
            state
                .storage
                .put_part_files(&[first, second], Some(&digest)),
        )
        .await
        .expect("publish multipart object");
    let manifest = state
        .storage
        .read_multipart_manifest(&stored.object_key)
        .await
        .expect("multipart manifest");
    let part_paths = manifest
        .parts
        .iter()
        .map(|part| part.path.clone())
        .collect::<Vec<_>>();
    let mut transaction = state
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("metadata transaction");
    let blob_id = publication
        .prepare_metadata_in_tx(&mut transaction, &stored)
        .await
        .expect("prepare metadata");
    publication
        .finish_metadata_in_tx(&mut transaction)
        .await
        .expect("finish metadata");
    transaction.commit().await.expect("commit metadata");
    drop(publication);

    let result = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("garbage collection");

    assert_eq!(result.deleted_blob_ids, vec![blob_id]);
    assert_eq!(result.deleted_objects, vec![stored.object_key.clone()]);
    assert!(matches!(
        state.storage.read_bytes(&stored.object_key).await,
        Err(StorageError::NotFound)
    ));
    for part_path in part_paths {
        assert!(tokio::fs::metadata(part_path).await.is_err());
    }
}

#[tokio::test]
async fn collection_preserves_shared_document_and_export_references_until_the_last_is_deleted() {
    let state = test_state().await;
    let (publication, stored) = begin_and_store(&state.pool, &state.storage, b"shared bytes").await;
    let mut transaction = state.pool.begin().await.expect("metadata transaction");
    let blob_id = publication
        .prepare_metadata_in_tx(&mut transaction, &stored)
        .await
        .expect("prepare metadata");
    let document_id = insert_document_reference(&mut transaction, blob_id).await;
    insert_export_reference(&mut transaction, blob_id, &stored).await;
    publication
        .finish_metadata_in_tx(&mut transaction)
        .await
        .expect("finish metadata");
    transaction.commit().await.expect("commit references");
    drop(publication);

    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(document_id)
        .execute(&state.pool)
        .await
        .expect("delete document");
    let while_export_references = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("collection with export reference");
    assert!(while_export_references.deleted_blob_ids.is_empty());
    assert!(while_export_references.deleted_objects.is_empty());
    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (1, 1));
    assert_eq!(
        state
            .storage
            .read_bytes(&stored.object_key)
            .await
            .expect("shared object"),
        b"shared bytes",
    );

    sqlx::query("DELETE FROM export_jobs WHERE id = 'shared-export'")
        .execute(&state.pool)
        .await
        .expect("delete export");
    let after_last_reference = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("collection after last reference");
    assert_eq!(after_last_reference.deleted_blob_ids, vec![blob_id]);
    assert_eq!(
        after_last_reference.deleted_objects,
        vec![stored.object_key.clone()]
    );
    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (0, 0));
    assert!(matches!(
        state.storage.read_bytes(&stored.object_key).await,
        Err(StorageError::NotFound)
    ));
}

#[tokio::test]
async fn failed_delete_preserves_metadata_and_retries_on_the_next_collection() {
    let state = test_state().await;
    let delete_attempts = Arc::new(AtomicUsize::new(0));
    let storage = FailOnceDeleteStorage {
        inner: state.storage.clone(),
        delete_attempts: delete_attempts.clone(),
        always_fail_key: None,
    };
    let (blob_id, stored) = publish_unreferenced(&state.pool, &storage, b"retry bytes").await;

    let first = collect_unreferenced_blobs(&state.pool, &storage)
        .await
        .expect("first collection");
    assert!(first.deleted_blob_ids.is_empty());
    assert!(first.deleted_objects.is_empty());
    assert_eq!(first.failures.len(), 1);
    assert_eq!(first.failures[0].blob_id, blob_id);
    assert_eq!(first.failures[0].object_key, stored.object_key);
    assert_eq!(delete_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (1, 1));
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("object retained for retry"),
        b"retry bytes",
    );
    let blocked_publication = begin_blob_publication(
        &state.pool,
        &storage,
        "sha256",
        &stored.digest,
        stored.size_bytes,
        BlobWriteKind::Bytes,
    )
    .await
    .expect_err("deletion tombstone must block key reuse");
    assert!(matches!(
        blocked_publication,
        BlobLifecycleError::DeletionInProgress
    ));

    let second = collect_unreferenced_blobs(&state.pool, &storage)
        .await
        .expect("second collection");
    assert_eq!(second.deleted_blob_ids, vec![blob_id]);
    assert_eq!(second.deleted_objects, vec![stored.object_key.clone()]);
    assert!(second.failures.is_empty());
    assert_eq!(delete_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (0, 0));
    assert!(matches!(
        storage.read_bytes(&stored.object_key).await,
        Err(StorageError::NotFound)
    ));
}

#[tokio::test]
async fn permanently_failing_location_does_not_starve_other_blob_locations() {
    let state = test_state().await;
    let delete_attempts = Arc::new(AtomicUsize::new(0));
    let primary_key = state
        .storage
        .planned_object_key(
            "sha256",
            &sha256_hex(b"partially removable replicas"),
            BlobWriteKind::Bytes,
        )
        .expect("primary key");
    let storage = FailOnceDeleteStorage {
        inner: state.storage.clone(),
        delete_attempts,
        always_fail_key: Some(primary_key.clone()),
    };
    let (blob_id, stored) =
        publish_unreferenced(&state.pool, &storage, b"partially removable replicas").await;
    assert_eq!(stored.object_key, primary_key);
    let replica_key = "objects/replicas/removable-copy";
    let replica_path = state.storage.root().join(replica_key);
    tokio::fs::create_dir_all(replica_path.parent().expect("replica parent"))
        .await
        .expect("replica directory");
    tokio::fs::write(&replica_path, b"partially removable replicas")
        .await
        .expect("replica object");
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, 'local', '', ?)",
    )
    .bind(blob_id)
    .bind(replica_key)
    .execute(&state.pool)
    .await
    .expect("replica location");

    let first = collect_unreferenced_blobs(&state.pool, &storage)
        .await
        .expect("first collection");
    assert_eq!(first.failures.len(), 1);
    let second = collect_unreferenced_blobs(&state.pool, &storage)
        .await
        .expect("second collection");

    assert!(second.deleted_objects.contains(&replica_key.to_string()));
    assert!(tokio::fs::metadata(replica_path).await.is_err());
    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (1, 1));
    assert_eq!(
        state
            .storage
            .read_bytes(&stored.object_key)
            .await
            .expect("failed primary retained"),
        b"partially removable replicas",
    );
}

#[tokio::test]
async fn lost_delete_response_survives_database_reconnect_and_retries_idempotently() {
    let state = test_state().await;
    let database_path = state.temp_dir.path().join("vault.db");
    let delete_attempts = Arc::new(AtomicUsize::new(0));
    let storage = DeleteThenErrorOnceStorage {
        inner: state.storage.clone(),
        delete_attempts: delete_attempts.clone(),
    };
    let (blob_id, stored) =
        publish_unreferenced(&state.pool, &storage, b"ambiguous delete bytes").await;

    let first = collect_unreferenced_blobs(&state.pool, &storage)
        .await
        .expect("first collection");
    assert_eq!(first.failures.len(), 1);
    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (1, 1));
    assert!(matches!(
        storage.read_bytes(&stored.object_key).await,
        Err(StorageError::NotFound)
    ));
    state.pool.close().await;

    let reopened = db::connect(&database_path).await.expect("reopen database");
    let second = collect_unreferenced_blobs(&reopened, &storage)
        .await
        .expect("collection after restart");
    assert_eq!(second.deleted_blob_ids, vec![blob_id]);
    assert_eq!(second.deleted_objects, vec![stored.object_key.clone()]);
    assert!(second.failures.is_empty());
    assert_eq!(delete_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(blob_metadata_counts(&reopened, blob_id).await, (0, 0));
}

#[tokio::test]
async fn stale_cancelled_publication_is_pruned_without_harming_retry_reference() {
    let state = test_state().await;
    let content = b"cancelled then retried";
    let (cancelled_publication, _cancelled_stored) =
        begin_and_store(&state.pool, &state.storage, content).await;
    let blob_id = cancelled_publication.blob_id();
    drop(cancelled_publication);

    let (retry_publication, retry_stored) =
        begin_and_store(&state.pool, &state.storage, content).await;
    let mut transaction = state
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("retry metadata transaction");
    retry_publication
        .prepare_metadata_in_tx(&mut transaction, &retry_stored)
        .await
        .expect("prepare retry metadata");
    insert_document_reference(&mut transaction, blob_id).await;
    retry_publication
        .finish_metadata_in_tx(&mut transaction)
        .await
        .expect("finish retry metadata");
    transaction.commit().await.expect("commit retry");
    drop(retry_publication);
    sqlx::query(
        "UPDATE blob_locations SET created_at = '2000-01-01T00:00:00Z' WHERE backend GLOB '_vault_pending:*'",
    )
    .execute(&state.pool)
    .await
    .expect("age cancelled publication");

    let result = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("stale lease maintenance");

    assert!(result.deleted_blob_ids.is_empty());
    assert!(result.deleted_objects.is_empty());
    assert_eq!(blob_metadata_counts(&state.pool, blob_id).await, (1, 1));
    let canonical_backend: String =
        sqlx::query_scalar("SELECT backend FROM blob_locations WHERE blob_id = ?")
            .bind(blob_id)
            .fetch_one(&state.pool)
            .await
            .expect("canonical location");
    assert_eq!(canonical_backend, "local");
    assert_eq!(
        state
            .storage
            .read_bytes(&retry_stored.object_key)
            .await
            .expect("retry bytes"),
        content,
    );
}

#[tokio::test]
async fn fresh_publication_is_deferred_while_abandoned_and_stale_publications_are_collected() {
    let state = test_state().await;
    let (fresh_publication, fresh_stored) =
        begin_and_store(&state.pool, &state.storage, b"fresh pending bytes").await;
    let fresh_blob_id = fresh_publication.blob_id();
    drop(fresh_publication);

    let while_fresh = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("collection with fresh publication");
    assert!(while_fresh.deleted_blob_ids.is_empty());
    assert!(while_fresh.deleted_objects.is_empty());
    assert!(while_fresh.failures.is_empty());
    assert_eq!(
        blob_metadata_counts(&state.pool, fresh_blob_id).await,
        (1, 1)
    );
    assert_eq!(
        state
            .storage
            .read_bytes(&fresh_stored.object_key)
            .await
            .expect("fresh pending object remains protected"),
        b"fresh pending bytes",
    );

    let (abandoned_publication, abandoned_stored) =
        begin_and_store(&state.pool, &state.storage, b"explicitly abandoned bytes").await;
    let abandoned_blob_id = abandoned_publication.blob_id();
    abandoned_publication
        .abandon(Some(&abandoned_stored))
        .await
        .expect("abandon publication");
    let after_abandonment = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("collection after explicit abandonment");
    assert_eq!(after_abandonment.deleted_blob_ids, vec![abandoned_blob_id]);
    assert_eq!(
        after_abandonment.deleted_objects,
        vec![abandoned_stored.object_key.clone()]
    );
    assert_eq!(
        blob_metadata_counts(&state.pool, abandoned_blob_id).await,
        (0, 0)
    );
    assert!(matches!(
        state.storage.read_bytes(&abandoned_stored.object_key).await,
        Err(StorageError::NotFound)
    ));
    assert_eq!(
        blob_metadata_counts(&state.pool, fresh_blob_id).await,
        (1, 1)
    );

    sqlx::query("UPDATE blob_locations SET created_at = '2000-01-01T00:00:00Z' WHERE blob_id = ?")
        .bind(fresh_blob_id)
        .execute(&state.pool)
        .await
        .expect("age crashed publication");
    let after_stale = collect_unreferenced_blobs(&state.pool, &state.storage)
        .await
        .expect("collection after publication became stale");
    assert_eq!(after_stale.deleted_blob_ids, vec![fresh_blob_id]);
    assert_eq!(
        after_stale.deleted_objects,
        vec![fresh_stored.object_key.clone()]
    );
    assert!(after_stale.failures.is_empty());
    assert_eq!(
        blob_metadata_counts(&state.pool, fresh_blob_id).await,
        (0, 0)
    );
    assert!(matches!(
        state.storage.read_bytes(&fresh_stored.object_key).await,
        Err(StorageError::NotFound)
    ));
}
