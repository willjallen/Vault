use std::sync::Arc;
use std::time::Duration;

use vault_server::db;
use vault_server::previews::{
    PreviewExecutionContext, PreviewPruneResult, UnsupportedPreviewProvider,
};
use vault_server::storage::{LocalBlobStorage, SharedBlobStorage};

async fn wait_for_preview_event(pool: &sqlx::SqlitePool) -> i64 {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let count = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM state_events WHERE event_type = 'preview.changed'",
            )
            .fetch_one(pool)
            .await
            .expect("preview event count");
            if count > 0 {
                return count;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("preview event timeout")
}

async fn start_context(
    pool: &sqlx::SqlitePool,
    storage: SharedBlobStorage,
) -> Arc<PreviewExecutionContext> {
    let context = Arc::new(PreviewExecutionContext::with_provider(Arc::new(
        UnsupportedPreviewProvider,
    )));
    context
        .start(pool.clone(), storage, 1)
        .await
        .expect("start preview context");
    context
}

#[tokio::test]
async fn pruning_jobs_without_renditions_coalesces_a_preview_event() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let local_storage = LocalBlobStorage::new(temp.path().join("objects"), "");
    local_storage.ensure().await.expect("storage");
    let storage: SharedBlobStorage = Arc::new(local_storage);
    let context = start_context(&pool, Arc::clone(&storage)).await;

    for deleted_job_id in [41, 42] {
        context
            .apply_prune_result(
                &pool,
                storage.as_ref(),
                PreviewPruneResult {
                    deleted_job_ids: vec![deleted_job_id],
                    released_blob_ids: Vec::new(),
                },
            )
            .await;
    }

    assert_eq!(wait_for_preview_event(&pool).await, 1);
    context.shutdown().await;
}

#[tokio::test]
async fn failed_preview_event_insert_is_retried() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    sqlx::query("DROP TABLE state_events")
        .execute(&pool)
        .await
        .expect("drop state events");
    let local_storage = LocalBlobStorage::new(temp.path().join("objects"), "");
    local_storage.ensure().await.expect("storage");
    let storage: SharedBlobStorage = Arc::new(local_storage);
    let context = start_context(&pool, storage).await;
    context.notify_changed();
    tokio::time::sleep(Duration::from_millis(400)).await;

    sqlx::query(
        r"
        CREATE TABLE state_events (
            id INTEGER PRIMARY KEY,
            event_type TEXT NOT NULL,
            resources TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )
        ",
    )
    .execute(&pool)
    .await
    .expect("restore state events");

    assert_eq!(wait_for_preview_event(&pool).await, 1);
    context.shutdown().await;
}
