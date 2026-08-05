use std::time::Duration;

use vault_server::db;
use vault_server::uploads::UploadHashCoordinator;

#[tokio::test]
async fn upload_hash_coordinator_keeps_active_states_at_the_cache_bound() {
    /*
     * Seeds one more active upload than the hash coordinator can cache, then fills the cache
     * before scheduling the extra session. It checks that the existing active entries remain
     * available and that admitting new work never pushes the cache past its hard bound.
     */
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp_dir.path().join("vault.db"))
        .await
        .expect("database");
    let coordinator = UploadHashCoordinator::new();
    let capacity = UploadHashCoordinator::cache_capacity();
    let target_folder_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("upload target");
    let mut transaction = pool.begin().await.expect("seed transaction");
    for index in 0..=capacity {
        sqlx::query(
            r"
            INSERT INTO upload_sessions
                (
                    id, mode, status, target_folder_id, filename, total_size,
                    chunk_size, part_count, created_by, user_context, expires_at
                )
            VALUES
                (?, 'create', 'active', ?, 'bounded.txt', 1, 1, 1,
                 'owner', '{}', '2999-01-01T00:00:00Z')
            ",
        )
        .bind(format!("session-{index}"))
        .bind(target_folder_id)
        .execute(&mut *transaction)
        .await
        .expect("upload session");
    }
    transaction.commit().await.expect("seed sessions");

    for index in 0..capacity {
        coordinator.schedule(
            pool.clone(),
            temp_dir.path().join("transfers"),
            format!("session-{index}"),
        );
    }
    for _ in 0..500 {
        if coordinator.cached_session_count().await == capacity {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(coordinator.cached_session_count().await, capacity);
    assert!(coordinator.preverified_bytes("session-0").await.is_some());

    coordinator.schedule(
        pool,
        temp_dir.path().join("transfers"),
        format!("session-{capacity}"),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(coordinator.cached_session_count().await, capacity);
    assert!(coordinator.preverified_bytes("session-0").await.is_some());
    assert!(
        coordinator
            .preverified_bytes(&format!("session-{capacity}"))
            .await
            .is_none()
    );
}
