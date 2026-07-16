use std::time::Duration;

use serde_json::json;
use tokio::sync::oneshot;
use vault_server::auth::UserContext;
use vault_server::db;
use vault_server::documents::{
    AccessPayload, ClientMeta, DocumentError, access_payload, archive_access_snapshot,
    archive_folder, delete_document_forever, document_access_level, document_folder_path,
    document_for_read, document_for_write, document_is_archive, document_path,
    editable_document_for_write, fetch_document_by_id, lock_document, move_document,
    normalize_file_name,
};
use vault_server::folders::{
    ARCHIVE_ROOT_KEY, VAULT_ROOT_KEY, add_folder_permission, get_or_create_folder_path,
    get_root_folder,
};

async fn test_pool() -> sqlx::SqlitePool {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("vault.db");
    let pool = db::connect(&db_path).await.expect("db connect");
    let _ = Box::leak(Box::new(temp_dir));
    pool
}

async fn create_group(pool: &sqlx::SqlitePool, name: &str) -> i64 {
    sqlx::query("INSERT INTO vault_groups (name) VALUES (?)")
        .bind(name)
        .execute(pool)
        .await
        .expect("create group")
        .last_insert_rowid()
}

async fn insert_document(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    archived_access: Option<String>,
) -> i64 {
    sqlx::query(
        r"
        INSERT INTO documents
            (folder_id, name, created_by, created_by_name, latest_modified_by, archived_access)
        VALUES
            (?, ?, 'admin', 'Admin', 'admin', ?)
        ",
    )
    .bind(folder_id)
    .bind(name)
    .bind(archived_access)
    .execute(pool)
    .await
    .expect("insert document")
    .last_insert_rowid()
}

fn user(groups: &[&str], is_admin: bool) -> UserContext {
    UserContext {
        id: "user".to_string(),
        vault_user_id: 1,
        issuer: "test".to_string(),
        subject: "user".to_string(),
        name: "User".to_string(),
        email: "user@example.com".to_string(),
        groups: groups.iter().map(|group| (*group).to_string()).collect(),
        is_admin,
    }
}

async fn assert_waiting_on_writer_gate<T>(task: &mut tokio::task::JoinHandle<T>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut *task)
            .await
            .is_err(),
        "operation completed before the competing writer committed",
    );
}

struct ArchiveRaceExpectation {
    archive_root: i64,
    source: i64,
    child: i64,
    late_folder: i64,
    safe: i64,
    original: i64,
    moved_out: i64,
    moved_in: i64,
    late_document: i64,
}

async fn assert_archive_race_state(pool: &sqlx::SqlitePool, expected: ArchiveRaceExpectation) {
    let rows = sqlx::query_as::<_, (i64, i64, Option<String>)>(
        r"
        SELECT id, folder_id, archived_from_folder
        FROM documents
        WHERE id IN (?, ?, ?, ?)
        ORDER BY id
        ",
    )
    .bind(expected.original)
    .bind(expected.moved_out)
    .bind(expected.moved_in)
    .bind(expected.late_document)
    .fetch_all(pool)
    .await
    .expect("documents after archive");
    assert_eq!(rows.len(), 4);
    assert!(rows.contains(&(
        expected.original,
        expected.archive_root,
        Some("Project/Work".to_string()),
    )));
    assert!(rows.contains(&(expected.moved_out, expected.safe, None)));
    assert!(rows.contains(&(
        expected.moved_in,
        expected.archive_root,
        Some("Project/Work/Sub".to_string()),
    )));
    assert!(rows.contains(&(
        expected.late_document,
        expected.archive_root,
        Some("Project/Work/Late".to_string()),
    )));
    let source_folders =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE id IN (?, ?, ?)")
            .bind(expected.source)
            .bind(expected.child)
            .bind(expected.late_folder)
            .fetch_one(pool)
            .await
            .expect("source folder count");
    assert_eq!(source_folders, 0);
}

#[test]
fn file_name_normalization_keeps_basename_from_client_paths() {
    assert_eq!(
        normalize_file_name(r"C:\Users\Artist\plan.txt").expect("windows path"),
        "plan.txt",
    );
    assert_eq!(
        normalize_file_name("Projects/Model/ScoutMaster.plasticity").expect("slash path"),
        "ScoutMaster.plasticity",
    );
    assert_eq!(
        normalize_file_name(" nested / spaced.txt ").expect("trimmed basename"),
        "spaced.txt",
    );
    assert!(normalize_file_name("Projects/..").is_err());
    assert!(normalize_file_name("Projects/bad\nname.txt").is_err());
}

#[tokio::test]
async fn document_paths_follow_folder_public_paths() {
    let pool = test_pool().await;
    let project = get_or_create_folder_path(&pool, Some("Project/Private"))
        .await
        .expect("folder");
    let document_id = insert_document(&pool, project.id, "plan.txt", None).await;
    let document = fetch_document_by_id(&pool, document_id)
        .await
        .expect("document");

    assert!(
        !document_is_archive(&pool, &document)
            .await
            .expect("archive flag")
    );
    assert_eq!(
        document_folder_path(&pool, &document)
            .await
            .expect("folder path"),
        "Project/Private",
    );
    assert_eq!(
        document_path(&pool, &document)
            .await
            .expect("document path"),
        "Project/Private/plan.txt",
    );
}

#[tokio::test]
async fn active_document_access_delegates_to_folder_acl() {
    let pool = test_pool().await;
    let project = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("folder");
    let readers = create_group(&pool, "readers").await;
    let writers = create_group(&pool, "writers").await;
    add_folder_permission(&pool, project.id, readers, true, true, false)
        .await
        .expect("reader permission");
    add_folder_permission(&pool, project.id, writers, true, true, true)
        .await
        .expect("writer permission");
    let document_id = insert_document(&pool, project.id, "plan.txt", None).await;
    let document = fetch_document_by_id(&pool, document_id)
        .await
        .expect("document");

    let reader_level = document_access_level(&pool, &document, &user(&["readers"], false))
        .await
        .expect("reader level");
    let writer_level = document_access_level(&pool, &document, &user(&["writers"], false))
        .await
        .expect("writer level");
    let outsider_level = document_access_level(&pool, &document, &user(&["outsiders"], false))
        .await
        .expect("outsider level");

    assert_eq!(reader_level, 2);
    assert_eq!(
        access_payload(reader_level),
        AccessPayload {
            visible: true,
            read: true,
            write: false,
        },
    );
    assert_eq!(writer_level, 3);
    assert_eq!(outsider_level, 0);
}

#[tokio::test]
async fn document_access_helpers_preserve_read_write_and_hidden_semantics() {
    let pool = test_pool().await;
    let project = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("folder");
    let viewers = create_group(&pool, "viewers").await;
    let readers = create_group(&pool, "readers").await;
    let writers = create_group(&pool, "writers").await;
    add_folder_permission(&pool, project.id, viewers, true, false, false)
        .await
        .expect("viewer permission");
    add_folder_permission(&pool, project.id, readers, true, true, false)
        .await
        .expect("reader permission");
    add_folder_permission(&pool, project.id, writers, true, true, true)
        .await
        .expect("writer permission");
    let document_id = insert_document(&pool, project.id, "plan.txt", None).await;

    document_for_read(&pool, document_id, &user(&["readers"], false))
        .await
        .expect("reader can read");
    assert!(matches!(
        document_for_read(&pool, document_id, &user(&["viewers"], false))
            .await
            .expect_err("viewer cannot read"),
        DocumentError::InsufficientDocumentAccess
    ));
    assert!(matches!(
        document_for_read(&pool, document_id, &user(&["outsiders"], false))
            .await
            .expect_err("outsider is hidden"),
        DocumentError::DocumentNotFound
    ));

    document_for_write(&pool, document_id, &user(&["writers"], false))
        .await
        .expect("writer can write");
    assert!(matches!(
        document_for_write(&pool, document_id, &user(&["readers"], false))
            .await
            .expect_err("reader cannot write"),
        DocumentError::InsufficientDocumentAccess
    ));
    assert!(matches!(
        document_for_write(&pool, document_id, &user(&["outsiders"], false))
            .await
            .expect_err("outsider write is hidden"),
        DocumentError::DocumentNotFound
    ));

    editable_document_for_write(&pool, document_id, &user(&["writers"], false))
        .await
        .expect("writer can edit active document");
}

#[tokio::test]
async fn archived_document_access_is_capped_by_archive_and_source_snapshot() {
    let pool = test_pool().await;
    let vault_root = get_root_folder(&pool, VAULT_ROOT_KEY).await.expect("vault");
    let archive_root = get_root_folder(&pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive");
    let readers = create_group(&pool, "readers").await;
    let writers = create_group(&pool, "writers").await;
    let outsiders = create_group(&pool, "outsiders").await;
    add_folder_permission(&pool, vault_root.id, outsiders, true, true, true)
        .await
        .expect("outsider root");
    let project = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("project");
    add_folder_permission(&pool, project.id, readers, true, true, false)
        .await
        .expect("reader source");
    add_folder_permission(&pool, project.id, writers, true, true, true)
        .await
        .expect("writer source");
    add_folder_permission(&pool, archive_root.id, readers, true, true, true)
        .await
        .expect("reader archive");
    add_folder_permission(&pool, archive_root.id, writers, true, false, false)
        .await
        .expect("writer archive");

    let snapshot = archive_access_snapshot(&pool, project.id)
        .await
        .expect("snapshot");
    let archived_access = json!(snapshot).to_string();
    let document_id =
        insert_document(&pool, archive_root.id, "plan.txt", Some(archived_access)).await;
    let document = fetch_document_by_id(&pool, document_id)
        .await
        .expect("document");

    assert!(
        document_is_archive(&pool, &document)
            .await
            .expect("archive flag")
    );
    assert_eq!(
        document_access_level(&pool, &document, &user(&["readers"], false))
            .await
            .expect("reader archived"),
        2,
    );
    assert_eq!(
        document_access_level(&pool, &document, &user(&["writers"], false))
            .await
            .expect("writer archived"),
        1,
    );
    assert_eq!(
        document_access_level(&pool, &document, &user(&["outsiders"], false))
            .await
            .expect("outsider archived"),
        0,
    );
    assert_eq!(
        document_access_level(&pool, &document, &user(&["outsiders"], true))
            .await
            .expect("admin archived"),
        3,
    );
    document_for_write(&pool, document_id, &user(&["outsiders"], true))
        .await
        .expect("admin can mutate archived metadata");
    assert!(matches!(
        editable_document_for_write(&pool, document_id, &user(&["outsiders"], true))
            .await
            .expect_err("archived documents must be restored before editing"),
        DocumentError::RestoreBeforeEditing
    ));
}

#[tokio::test]
async fn delete_forever_rechecks_archive_state_after_restore_wins_writer_gate() {
    let pool = test_pool().await;
    let project = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("project");
    let archive_root = get_root_folder(&pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    let document_id = insert_document(&pool, archive_root.id, "restore.txt", None).await;
    sqlx::query(
        r"
        UPDATE documents
        SET archived_from_folder = 'Project', archived_original_name = name
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .execute(&pool)
    .await
    .expect("archive metadata");

    let mut gate = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("writer gate");
    let delete_pool = pool.clone();
    let delete_user = user(&[], true);
    let (started_tx, started_rx) = oneshot::channel();
    let mut deletion = tokio::spawn(async move {
        started_tx.send(()).expect("signal deletion start");
        delete_document_forever(&delete_pool, document_id, &delete_user).await
    });
    started_rx.await.expect("deletion started");
    assert_waiting_on_writer_gate(&mut deletion).await;

    sqlx::query(
        r"
        UPDATE documents
        SET
            folder_id = ?,
            archived_from_folder = NULL,
            archived_original_name = NULL,
            archived_access = NULL
        WHERE id = ?
        ",
    )
    .bind(project.id)
    .bind(document_id)
    .execute(&mut *gate)
    .await
    .expect("restore document");
    gate.commit().await.expect("commit restore");

    let error = tokio::time::timeout(Duration::from_secs(5), deletion)
        .await
        .expect("delete forever timed out")
        .expect("delete task")
        .expect_err("restored document must not be deleted");
    assert!(matches!(
        error,
        DocumentError::MoveDocumentToArchiveBeforeDeleting
    ));
    let remaining = sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
        r"
        SELECT folder_id, archived_from_folder, archived_original_name
        FROM documents
        WHERE id = ?
        ",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("restored document remains");
    assert_eq!(remaining, (project.id, None, None));
}

#[tokio::test]
async fn lock_rechecks_document_after_archive_wins_writer_gate() {
    let pool = test_pool().await;
    let source = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("source folder");
    let archive_root = get_root_folder(&pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    let document_id = insert_document(&pool, source.id, "locking.txt", None).await;
    let mut gate = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("writer gate");
    let lock_pool = pool.clone();
    let lock_user = user(&[], true);
    let (started_tx, started_rx) = oneshot::channel();
    let mut locking = tokio::spawn(async move {
        started_tx.send(()).expect("signal lock start");
        lock_document(
            &lock_pool,
            document_id,
            &lock_user,
            &ClientMeta {
                ip: None,
                user_agent: None,
            },
        )
        .await
    });
    started_rx.await.expect("lock started");
    assert_waiting_on_writer_gate(&mut locking).await;

    sqlx::query(
        r"
        UPDATE documents
        SET
            folder_id = ?,
            archived_from_folder = 'Project',
            archived_original_name = name,
            archived_access = '{}'
        WHERE id = ?
        ",
    )
    .bind(archive_root.id)
    .bind(document_id)
    .execute(&mut *gate)
    .await
    .expect("archive document");
    gate.commit().await.expect("commit archive");

    let error = tokio::time::timeout(Duration::from_secs(5), locking)
        .await
        .expect("lock timed out")
        .expect("lock task")
        .expect_err("archived document must not be locked");
    assert!(matches!(error, DocumentError::RestoreBeforeEditing));
    let lock_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM document_locks WHERE document_id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("lock count");
    assert_eq!(lock_count, 0);
}

#[tokio::test]
async fn move_rechecks_document_after_archive_wins_writer_gate() {
    let pool = test_pool().await;
    let source = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("source folder");
    get_or_create_folder_path(&pool, Some("Safe"))
        .await
        .expect("safe folder");
    let archive_root = get_root_folder(&pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    let document_id = insert_document(&pool, source.id, "moving.txt", None).await;
    let mut gate = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("writer gate");
    let move_pool = pool.clone();
    let move_user = user(&[], true);
    let (started_tx, started_rx) = oneshot::channel();
    let mut moving = tokio::spawn(async move {
        started_tx.send(()).expect("signal move start");
        move_document(
            &move_pool,
            document_id,
            "Safe",
            &move_user,
            &ClientMeta {
                ip: None,
                user_agent: None,
            },
        )
        .await
    });
    started_rx.await.expect("move started");
    assert_waiting_on_writer_gate(&mut moving).await;

    sqlx::query(
        r"
        UPDATE documents
        SET
            folder_id = ?,
            archived_from_folder = 'Project',
            archived_original_name = name,
            archived_access = '{}'
        WHERE id = ?
        ",
    )
    .bind(archive_root.id)
    .bind(document_id)
    .execute(&mut *gate)
    .await
    .expect("archive document");
    gate.commit().await.expect("commit archive");

    let error = tokio::time::timeout(Duration::from_secs(5), moving)
        .await
        .expect("move timed out")
        .expect("move task")
        .expect_err("archived document must not be moved directly");
    assert!(matches!(
        error,
        DocumentError::UseArchiveOrRestoreForArchiveMoves
    ));
    let state = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT folder_id, archived_from_folder FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("archived document state");
    assert_eq!(state, (archive_root.id, Some("Project".to_string())));
}

#[tokio::test]
async fn archive_folder_uses_subtree_and_documents_committed_before_writer_gate_releases() {
    let pool = test_pool().await;
    let source = get_or_create_folder_path(&pool, Some("Project/Work"))
        .await
        .expect("source folder");
    let child = get_or_create_folder_path(&pool, Some("Project/Work/Sub"))
        .await
        .expect("source child");
    let safe = get_or_create_folder_path(&pool, Some("Safe"))
        .await
        .expect("safe folder");
    let outside = get_or_create_folder_path(&pool, Some("Outside"))
        .await
        .expect("outside folder");
    let archive_root = get_root_folder(&pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    let original_id = insert_document(&pool, source.id, "original.txt", None).await;
    let moved_out_id = insert_document(&pool, child.id, "outbound.txt", None).await;
    let moved_in_id = insert_document(&pool, outside.id, "inbound.txt", None).await;

    let mut gate = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("writer gate");
    let archive_pool = pool.clone();
    let archive_user = user(&[], true);
    let (started_tx, started_rx) = oneshot::channel();
    let mut archive = tokio::spawn(async move {
        started_tx.send(()).expect("signal archive start");
        archive_folder(
            &archive_pool,
            source.id,
            &archive_user,
            &ClientMeta {
                ip: None,
                user_agent: None,
            },
        )
        .await
    });
    started_rx.await.expect("archive started");
    assert_waiting_on_writer_gate(&mut archive).await;

    let late_folder_id = sqlx::query(
        r"
        INSERT INTO folders (root_key, parent_id, name, is_root)
        VALUES ('vault', ?, 'Late', 0)
        ",
    )
    .bind(source.id)
    .execute(&mut *gate)
    .await
    .expect("late folder")
    .last_insert_rowid();
    let late_document_id = sqlx::query(
        r"
        INSERT INTO documents (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES (?, 'late.txt', 'admin', 'Admin', 'admin')
        ",
    )
    .bind(late_folder_id)
    .execute(&mut *gate)
    .await
    .expect("late document")
    .last_insert_rowid();
    sqlx::query("UPDATE documents SET folder_id = ? WHERE id = ?")
        .bind(child.id)
        .bind(moved_in_id)
        .execute(&mut *gate)
        .await
        .expect("move document into subtree");
    sqlx::query("UPDATE documents SET folder_id = ? WHERE id = ?")
        .bind(safe.id)
        .bind(moved_out_id)
        .execute(&mut *gate)
        .await
        .expect("move document out of subtree");
    gate.commit().await.expect("commit competing moves");

    let result = tokio::time::timeout(Duration::from_secs(5), archive)
        .await
        .expect("folder archive timed out")
        .expect("archive task")
        .expect("archive folder");
    assert_eq!(result, "Archive");

    assert_archive_race_state(
        &pool,
        ArchiveRaceExpectation {
            archive_root: archive_root.id,
            source: source.id,
            child: child.id,
            late_folder: late_folder_id,
            safe: safe.id,
            original: original_id,
            moved_out: moved_out_id,
            moved_in: moved_in_id,
            late_document: late_document_id,
        },
    )
    .await;
}
