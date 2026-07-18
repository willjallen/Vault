use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use vault_server::db;
use vault_server::previews::{
    PreviewExecutionContext, PreviewProvider, PreviewProviderFailure, PreviewRenderRequest,
    RenderedPreview, enqueue_preview_jobs,
};
use vault_server::storage::{LocalBlobStorage, SharedBlobStorage};

#[derive(Debug)]
struct FailingProvider;

#[async_trait]
impl PreviewProvider for FailingProvider {
    fn supports(&self, _mime_type: Option<&str>, _filename: Option<&str>) -> bool {
        true
    }

    async fn render(
        &self,
        _request: PreviewRenderRequest,
    ) -> Result<Vec<RenderedPreview>, PreviewProviderFailure> {
        Err(PreviewProviderFailure::Failed)
    }
}

#[tokio::test]
async fn enqueue_revives_only_cooled_down_transient_failures() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let source_ids = Vec::from([
        sqlx::query(
            "INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', 'old-transient', 1)",
        )
        .execute(&pool)
        .await
        .expect("old transient source")
        .last_insert_rowid(),
        sqlx::query(
            "INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', 'fresh-transient', 1)",
        )
        .execute(&pool)
        .await
        .expect("fresh transient source")
        .last_insert_rowid(),
        sqlx::query(
            "INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', 'deterministic', 1)",
        )
        .execute(&pool)
        .await
        .expect("deterministic source")
        .last_insert_rowid(),
    ]);
    enqueue_preview_jobs(&pool, &source_ids)
        .await
        .expect("initial jobs");
    sqlx::query(
        r"
        UPDATE preview_jobs
        SET status = 'failed', attempt_count = 3, next_attempt_at = NULL,
            completed_at = datetime('now', '-16 minutes'), last_error_code = 'storage'
        WHERE source_blob_id = ?
        ",
    )
    .bind(source_ids[0])
    .execute(&pool)
    .await
    .expect("old transient failure");
    sqlx::query(
        r"
        UPDATE preview_jobs
        SET status = 'failed', attempt_count = 3, next_attempt_at = NULL,
            completed_at = CURRENT_TIMESTAMP, last_error_code = 'storage'
        WHERE source_blob_id = ?
        ",
    )
    .bind(source_ids[1])
    .execute(&pool)
    .await
    .expect("fresh transient failure");
    sqlx::query(
        r"
        UPDATE preview_jobs
        SET status = 'failed', attempt_count = 3, next_attempt_at = NULL,
            completed_at = datetime('now', '-16 minutes'), last_error_code = 'invalid_source'
        WHERE source_blob_id = ?
        ",
    )
    .bind(source_ids[2])
    .execute(&pool)
    .await
    .expect("deterministic failure");

    let revived = enqueue_preview_jobs(&pool, &source_ids)
        .await
        .expect("revive eligible job");
    assert_eq!(revived, 1);
    let states = sqlx::query_as::<_, (i64, String, i64)>(
        "SELECT source_blob_id, status, attempt_count FROM preview_jobs ORDER BY source_blob_id",
    )
    .fetch_all(&pool)
    .await
    .expect("job states");
    assert_eq!(states[0], (source_ids[0], "queued".to_string(), 0));
    assert_eq!(states[1], (source_ids[1], "failed".to_string(), 3));
    assert_eq!(states[2], (source_ids[2], "failed".to_string(), 3));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_terminal_worker_failure_releases_partial_rendition_references() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let storage = LocalBlobStorage::new(temp.path().join("objects"), "");
    storage.ensure().await.expect("storage");
    let stored_source = storage.put_bytes(b"a").await.expect("source bytes");
    let source_blob_id =
        sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES (?, ?, ?)")
            .bind(&stored_source.hash_algo)
            .bind(&stored_source.digest)
            .bind(i64::try_from(stored_source.size_bytes).expect("source size"))
            .execute(&pool)
            .await
            .expect("source blob")
            .last_insert_rowid();
    sqlx::query(
        "INSERT INTO blob_locations (blob_id, backend, bucket, object_key) VALUES (?, ?, ?, ?)",
    )
    .bind(source_blob_id)
    .bind(&stored_source.backend)
    .bind(&stored_source.bucket)
    .bind(&stored_source.object_key)
    .execute(&pool)
    .await
    .expect("source location");
    let root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("root");
    let document_id = sqlx::query("INSERT INTO documents (folder_id, name) VALUES (?, 'a.png')")
        .bind(root_id)
        .execute(&pool)
        .await
        .expect("document")
        .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO document_versions
            (id, document_id, blob_id, version_number, committed_by, mime_type)
        VALUES ('failure-version', ?, ?, 1, 'admin', 'image/png')
        ",
    )
    .bind(document_id)
    .bind(source_blob_id)
    .execute(&pool)
    .await
    .expect("version");
    sqlx::query(
        "UPDATE documents SET current_version_id = 'failure-version', version_count = 1 WHERE id = ?",
    )
    .bind(document_id)
    .execute(&pool)
    .await
    .expect("current version");
    enqueue_preview_jobs(&pool, &[source_blob_id])
        .await
        .expect("job");
    let job_id: i64 = sqlx::query_scalar("SELECT id FROM preview_jobs WHERE source_blob_id = ?")
        .bind(source_blob_id)
        .fetch_one(&pool)
        .await
        .expect("job id");
    let partial_blob_id = sqlx::query(
        "INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', 'partial-output', 1)",
    )
    .execute(&pool)
    .await
    .expect("partial blob")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO preview_renditions
            (preview_job_id, variant, blob_id, mime_type, width, height)
        VALUES (?, 'small', ?, 'image/webp', 1, 1)
        ",
    )
    .bind(job_id)
    .bind(partial_blob_id)
    .execute(&pool)
    .await
    .expect("partial rendition");

    let storage: SharedBlobStorage = Arc::new(storage);
    let execution = Arc::new(PreviewExecutionContext::with_provider(Arc::new(
        FailingProvider,
    )));
    execution
        .start(pool.clone(), storage, 1)
        .await
        .expect("start worker");
    let final_status = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status: String = sqlx::query_scalar("SELECT status FROM preview_jobs WHERE id = ?")
                .bind(job_id)
                .fetch_one(&pool)
                .await
                .expect("job status");
            if status == "failed" {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("worker completion");
    execution.shutdown().await;
    assert_eq!(final_status, "failed");
    let rendition_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM preview_renditions WHERE preview_job_id = ?")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .expect("rendition count");
    assert_eq!(rendition_count, 0);
}
