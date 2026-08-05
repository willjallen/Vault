use std::time::Duration;

use tokio::sync::oneshot;
use vault_server::auth::UserContext;
use vault_server::db;
use vault_server::documents::{
    AccessPayload, ClientMeta, DocumentError, access_payload, archive_document, archive_folder,
    delete_archived_folder_forever, delete_document_forever, document_access_level,
    document_folder_path, document_for_read, document_for_write, document_is_archive,
    document_path, editable_document_for_write, fetch_document_by_id, lock_document, move_document,
    normalize_file_name, restore_document, restore_folder,
};
use vault_server::folders::{
    ARCHIVE_ROOT_KEY, FolderError, FolderRetentionUpdate, VAULT_ROOT_KEY, add_folder_permission,
    create_folder_path, get_or_create_folder_path, get_root_folder, move_folder, rename_folder,
    update_folder_retention,
};
use vault_server::views::build_contents_payload;

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
        SELECT id, folder_id, archived_at
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
    assert!(rows.contains(&(expected.original, expected.source, None)));
    assert!(rows.contains(&(expected.moved_out, expected.safe, None)));
    assert!(rows.contains(&(expected.moved_in, expected.child, None)));
    assert!(rows.contains(&(expected.late_document, expected.late_folder, None)));
    let source_folders =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE id IN (?, ?, ?)")
            .bind(expected.source)
            .bind(expected.child)
            .bind(expected.late_folder)
            .fetch_one(pool)
            .await
            .expect("source folder count");
    assert_eq!(source_folders, 3);
    let source_archived: Option<String> =
        sqlx::query_scalar("SELECT archived_at FROM folders WHERE id = ?")
            .bind(expected.source)
            .fetch_one(pool)
            .await
            .expect("source archive marker");
    assert!(source_archived.is_some());
}

#[test]
fn file_name_normalization_keeps_basename_from_client_paths() {
    /*
     * Normalizes Windows paths, slash-separated paths, and whitespace around a client-supplied
     * basename. It checks directory components are discarded while traversal-only and
     * control-bearing names are rejected instead of becoming stored document names.
     */
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
    /*
     * Creates an active document inside a nested Vault folder and reloads its persisted record.
     * It checks the document is not mistaken for archived content and that both its folder
     * path and full public path are reconstructed from the live hierarchy.
     */
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
    /*
     * Places reader and writer grants on a document's parent folder, then evaluates the document
     * for readers, writers, and an unrelated user. It checks document access inherits the
     * folder's read/write level and converts that level into the expected client access
     * flags.
     */
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
    /*
     * Gives separate groups view-only, read, and write access to a document's folder and calls
     * the read, write, and editable lookup helpers. It checks insufficient visible access
     * produces an explicit denial, wholly hidden documents look missing, and only a writer
     * can edit the active document.
     */
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
    /*
     * Stores a document in Archive with a snapshot of its source-folder ACL and different
     * current permissions on the Archive root. It checks effective access is the
     * intersection of those two boundaries, except for an administrator, and that even an
     * administrator must restore the document before editing its content.
     */
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

    let document_id = insert_document(&pool, project.id, "plan.txt", None).await;
    archive_document(
        &pool,
        document_id,
        &user(&["outsiders"], true),
        &ClientMeta {
            ip: None,
            user_agent: None,
        },
    )
    .await
    .expect("archive document");
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
    /*
     * Blocks permanent deletion behind a SQLite writer, restores the document in the winning
     * transaction, and then releases the delete. It checks deletion re-reads the committed
     * state, refuses to remove an active document, and leaves the restored location and
     * archive metadata intact.
     */
    let pool = test_pool().await;
    let project = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("project");
    let document_id = insert_document(&pool, project.id, "restore.txt", None).await;
    archive_document(
        &pool,
        document_id,
        &user(&[], true),
        &ClientMeta {
            ip: None,
            user_agent: None,
        },
    )
    .await
    .expect("archive document");

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
            archived_at = NULL,
            archived_origin_path = NULL,
            archived_access = NULL
        WHERE id = ?
        ",
    )
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
        SELECT folder_id, archived_at, archived_origin_path
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
    /*
     * Starts a lock operation behind a held writer transaction, archives the document first, and
     * then lets the lock continue. It checks the lock path observes the new archived state,
     * requires restoration, and creates no stale lock row.
     */
    let pool = test_pool().await;
    let source = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("source folder");
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
            archived_at = CURRENT_TIMESTAMP,
            archived_origin_path = 'Project/locking.txt',
            archived_access = '{}'
        WHERE id = ?
        ",
    )
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
    /*
     * Starts a normal move while another transaction holds the writer gate, then archives the
     * document before the move can acquire it. It checks the move rejects the now-archived item
     * and preserves its Archive location and original source metadata.
     */
    let pool = test_pool().await;
    let source = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("source folder");
    get_or_create_folder_path(&pool, Some("Safe"))
        .await
        .expect("safe folder");
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
            archived_at = CURRENT_TIMESTAMP,
            archived_origin_path = 'Project/moving.txt',
            archived_access = '{}'
        WHERE id = ?
        ",
    )
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
        "SELECT folder_id, archived_at FROM documents WHERE id = ?",
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await
    .expect("archived document state");
    assert_eq!(state.0, source.id);
    assert!(state.1.is_some());
}

#[tokio::test]
async fn archive_folder_uses_subtree_and_documents_committed_before_writer_gate_releases() {
    /*
     * Pauses a folder archive behind a writer, then atomically adds a descendant, moves one
     * document into the subtree, and moves another out before releasing the archive. It checks
     * the operation archives the committed subtree snapshot—including late and inbound
     * content—while preserving outbound content and removing only the archived source
     * folders.
     */
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
    assert_eq!(result, "Archive/Work");

    assert_archive_race_state(
        &pool,
        ArchiveRaceExpectation {
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

#[tokio::test]
async fn restore_target_identity_survives_source_folder_rename() {
    /*
     * Archives a document, renames its original folder, and recreates a different folder at the
     * old path. Restore follows the stored folder identity to the renamed source instead of
     * silently rebinding the document to the replacement.
     */
    let pool = test_pool().await;
    let actor = user(&[], true);
    let source = get_or_create_folder_path(&pool, Some("Projects/Incoming"))
        .await
        .expect("source folder");
    let document_id = insert_document(&pool, source.id, "renamed-source.txt", None).await;
    let meta = ClientMeta {
        ip: None,
        user_agent: None,
    };

    archive_document(&pool, document_id, &actor, &meta)
        .await
        .expect("archive document");
    rename_folder(&pool, source.id, Some("Projects"), "Accepted", &actor)
        .await
        .expect("rename original source");
    let replacement = create_folder_path(&pool, "Projects/Incoming", &actor)
        .await
        .expect("create replacement at archived path");

    let restored_path = restore_document(&pool, document_id, &actor, &meta)
        .await
        .expect("restore document");
    let stored_folder_id: i64 = sqlx::query_scalar("SELECT folder_id FROM documents WHERE id = ?")
        .bind(document_id)
        .fetch_one(&pool)
        .await
        .expect("restored document");

    assert_ne!(source.id, replacement.id);
    assert_eq!(
        stored_folder_id, source.id,
        "restore silently rebound the document to replacement folder {}",
        replacement.id
    );
    assert_eq!(restored_path, "Projects/Accepted/renamed-source.txt");
}

#[tokio::test]
async fn restore_target_identity_blocks_delete_ttl_drift_after_source_move() {
    /*
     * Archives a document, moves its source folder, and recreates the old path with automatic
     * deletion enabled. Restore returns the document to the moved source identity without
     * inheriting the replacement folder's retention policy.
     */
    let pool = test_pool().await;
    let actor = user(&[], true);
    let source = get_or_create_folder_path(&pool, Some("Records/Incoming"))
        .await
        .expect("source folder");
    get_or_create_folder_path(&pool, Some("LongTerm"))
        .await
        .expect("move destination");
    let document_id = insert_document(&pool, source.id, "retention.txt", None).await;
    let meta = ClientMeta {
        ip: None,
        user_agent: None,
    };

    archive_document(&pool, document_id, &actor, &meta)
        .await
        .expect("archive document");
    move_folder(&pool, source.id, "LongTerm", &actor)
        .await
        .expect("move original source");
    let replacement = create_folder_path(&pool, "Records/Incoming", &actor)
        .await
        .expect("create replacement at archived path");
    update_folder_retention(
        &pool,
        "Records/Incoming",
        &FolderRetentionUpdate {
            default_ttl_days: Some(1),
            default_ttl_action: Some("delete".to_string()),
        },
        &actor,
    )
    .await
    .expect("set replacement retention");

    restore_document(&pool, document_id, &actor, &meta)
        .await
        .expect("restore document");
    let restored: (i64, Option<String>) =
        sqlx::query_as("SELECT folder_id, expiry_action FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("restored document");

    assert_ne!(source.id, replacement.id);
    assert_eq!(
        restored.1, None,
        "restore inherited the replacement folder's automatic-delete policy"
    );
    assert_eq!(restored.0, source.id);
}

#[tokio::test]
async fn restore_target_identity_blocks_acl_drift_after_ancestor_rename() {
    /*
     * Archives a hidden document, renames its source ancestor, and recreates a readable tree at
     * the old path. Restore keeps the original folder identity and its deny boundary rather
     * than exposing the document through the replacement ACLs.
     */
    let pool = test_pool().await;
    let actor = user(&[], true);
    let readers = create_group(&pool, "readers").await;
    let root = get_root_folder(&pool, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    add_folder_permission(&pool, root.id, readers, true, true, false)
        .await
        .expect("reader root access");
    let ancestor = get_or_create_folder_path(&pool, Some("Projects"))
        .await
        .expect("source ancestor");
    let source = get_or_create_folder_path(&pool, Some("Projects/Incoming"))
        .await
        .expect("source folder");
    add_folder_permission(&pool, source.id, readers, false, false, false)
        .await
        .expect("hide original source from readers");
    let document_id = insert_document(&pool, source.id, "private.txt", None).await;
    let meta = ClientMeta {
        ip: None,
        user_agent: None,
    };

    archive_document(&pool, document_id, &actor, &meta)
        .await
        .expect("archive document");
    rename_folder(&pool, ancestor.id, Some(""), "ActiveProjects", &actor)
        .await
        .expect("rename source ancestor");
    create_folder_path(&pool, "Projects", &actor)
        .await
        .expect("recreate old ancestor");
    let replacement = create_folder_path(&pool, "Projects/Incoming", &actor)
        .await
        .expect("recreate old subtree");

    restore_document(&pool, document_id, &actor, &meta)
        .await
        .expect("restore document");
    let restored = fetch_document_by_id(&pool, document_id)
        .await
        .expect("restored document");
    let reader_access = document_access_level(&pool, &restored, &user(&["readers"], false))
        .await
        .expect("reader access");

    assert_ne!(source.id, replacement.id);
    assert_eq!(
        reader_access, 0,
        "restore exposed the document through replacement folder {}",
        replacement.id
    );
    assert_eq!(restored.folder_id, source.id);
}

#[tokio::test]
async fn archive_identity_model_folder_restore_retains_the_original_subtree() {
    /*
     * Archives a folder with a nested document, renames its parent, and restores the archived
     * root. The same folder, child, and document identities reappear beneath the parent's
     * current name, and the archive marker is cleared from the restored root.
     */
    let pool = test_pool().await;
    let actor = user(&[], true);
    let parent = get_or_create_folder_path(&pool, Some("Projects"))
        .await
        .expect("parent");
    let source = get_or_create_folder_path(&pool, Some("Projects/Incoming"))
        .await
        .expect("source");
    let child = get_or_create_folder_path(&pool, Some("Projects/Incoming/Nested"))
        .await
        .expect("child");
    let document_id = insert_document(&pool, child.id, "payload.bin", None).await;
    let meta = ClientMeta {
        ip: None,
        user_agent: None,
    };

    archive_folder(&pool, source.id, &actor, &meta)
        .await
        .expect("archive folder");
    rename_folder(&pool, parent.id, Some(""), "Accepted", &actor)
        .await
        .expect("rename parent");
    let restored_path = restore_folder(&pool, source.id, &actor)
        .await
        .expect("restore folder");

    let stored_document_folder: i64 =
        sqlx::query_scalar("SELECT folder_id FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document folder");
    let archived_at: Option<String> =
        sqlx::query_scalar("SELECT archived_at FROM folders WHERE id = ?")
            .bind(source.id)
            .fetch_one(&pool)
            .await
            .expect("folder marker");
    assert_eq!(restored_path, "Accepted/Incoming");
    assert_eq!(stored_document_folder, child.id);
    assert!(archived_at.is_none());
}

#[tokio::test]
async fn archive_identity_model_folder_restore_rejects_a_reused_name_without_rebinding() {
    /*
     * Archives a folder and creates a replacement with the same path before attempting restore.
     * The collision is rejected without moving the archived document or clearing the original
     * folder's archive marker.
     */
    let pool = test_pool().await;
    let actor = user(&[], true);
    let source = get_or_create_folder_path(&pool, Some("Projects/Incoming"))
        .await
        .expect("source");
    let document_id = insert_document(&pool, source.id, "payload.bin", None).await;
    let meta = ClientMeta {
        ip: None,
        user_agent: None,
    };

    archive_folder(&pool, source.id, &actor, &meta)
        .await
        .expect("archive folder");
    let replacement = create_folder_path(&pool, "Projects/Incoming", &actor)
        .await
        .expect("replacement");
    let error = restore_folder(&pool, source.id, &actor)
        .await
        .expect_err("name conflict must block restore");

    assert!(matches!(
        error,
        DocumentError::Folder(FolderError::TargetFolderAlreadyExists)
    ));
    assert_ne!(source.id, replacement.id);
    let state: (i64, Option<String>) =
        sqlx::query_as("SELECT folder_id, archived_at FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("archived document");
    assert_eq!(state.0, source.id);
    assert!(
        state.1.is_none(),
        "folder archive must not mark every document"
    );
    let folder_marker: Option<String> =
        sqlx::query_scalar("SELECT archived_at FROM folders WHERE id = ?")
            .bind(source.id)
            .fetch_one(&pool)
            .await
            .expect("source marker");
    assert!(folder_marker.is_some());
}

#[tokio::test]
async fn archive_identity_model_parent_restore_preserves_an_independent_child_archive() {
    /*
     * Archives a child independently before archiving its parent. Both remain separate archive
     * roots, and restoring the parent restores only its owned content while the child and its
     * document stay archived.
     */
    let pool = test_pool().await;
    let actor = user(&[], true);
    let parent = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("parent");
    let child = get_or_create_folder_path(&pool, Some("Project/Independent"))
        .await
        .expect("child");
    let parent_document = insert_document(&pool, parent.id, "parent.txt", None).await;
    let child_document = insert_document(&pool, child.id, "child.txt", None).await;
    let meta = ClientMeta {
        ip: None,
        user_agent: None,
    };

    archive_folder(&pool, child.id, &actor, &meta)
        .await
        .expect("archive child");
    archive_folder(&pool, parent.id, &actor, &meta)
        .await
        .expect("archive parent");
    let archive = build_contents_payload(&pool, "Archive", &actor, "", false)
        .await
        .expect("archive contents");
    assert_eq!(
        archive
            .folders
            .iter()
            .map(|folder| folder.id)
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([parent.id, child.id])
    );
    let parent_archive_path = archive
        .folders
        .iter()
        .find(|folder| folder.id == parent.id)
        .expect("parent archive root")
        .path
        .clone();
    let parent_contents = build_contents_payload(&pool, &parent_archive_path, &actor, "", false)
        .await
        .expect("parent archive contents");
    assert!(
        parent_contents
            .folders
            .iter()
            .all(|folder| folder.id != child.id),
        "independently archived child must remain a separate archive root"
    );
    restore_folder(&pool, parent.id, &actor)
        .await
        .expect("restore parent");

    let child_marker: Option<String> =
        sqlx::query_scalar("SELECT archived_at FROM folders WHERE id = ?")
            .bind(child.id)
            .fetch_one(&pool)
            .await
            .expect("child marker");
    let parent_record = fetch_document_by_id(&pool, parent_document)
        .await
        .expect("parent document");
    let child_record = fetch_document_by_id(&pool, child_document)
        .await
        .expect("child document");
    assert!(child_marker.is_some());
    assert!(
        !document_is_archive(&pool, &parent_record)
            .await
            .expect("parent document state")
    );
    assert!(
        document_is_archive(&pool, &child_record)
            .await
            .expect("child document state")
    );
}

#[tokio::test]
async fn archive_identity_model_parent_delete_never_cascades_into_separate_archive_entries() {
    /*
     * Archives a document and child folder independently before archiving their parent.
     * Permanent parent deletion is blocked until those separate entries are explicitly
     * removed, preventing an aggregate delete from cascading across archive ownership
     * boundaries.
     */
    let pool = test_pool().await;
    let actor = user(&[], true);
    let parent = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("parent");
    let child = get_or_create_folder_path(&pool, Some("Project/Independent"))
        .await
        .expect("child");
    insert_document(&pool, parent.id, "owned-by-parent.txt", None).await;
    let independent_document =
        insert_document(&pool, parent.id, "independent-document.txt", None).await;
    insert_document(&pool, child.id, "independent-child.txt", None).await;
    let meta = ClientMeta {
        ip: None,
        user_agent: None,
    };

    archive_document(&pool, independent_document, &actor, &meta)
        .await
        .expect("archive document separately");
    archive_folder(&pool, child.id, &actor, &meta)
        .await
        .expect("archive child separately");
    archive_folder(&pool, parent.id, &actor, &meta)
        .await
        .expect("archive parent");

    let error = delete_archived_folder_forever(&pool, parent.id, &actor)
        .await
        .expect_err("parent delete must preserve separate archive entries");
    assert!(matches!(
        error,
        DocumentError::FolderContainsIndependentArchiveEntries
    ));

    delete_archived_folder_forever(&pool, child.id, &actor)
        .await
        .expect("delete independent child explicitly");
    let error = delete_archived_folder_forever(&pool, parent.id, &actor)
        .await
        .expect_err("independent document must still block parent delete");
    assert!(matches!(
        error,
        DocumentError::FolderContainsIndependentArchiveEntries
    ));
    assert!(
        fetch_document_by_id(&pool, independent_document)
            .await
            .is_ok(),
        "blocked parent deletion must preserve the separately archived document"
    );

    delete_document_forever(&pool, independent_document, &actor)
        .await
        .expect("delete independent document explicitly");
    delete_archived_folder_forever(&pool, parent.id, &actor)
        .await
        .expect("delete parent once it owns the remaining subtree");
}

#[tokio::test]
async fn archive_identity_model_aggregate_mutations_require_write_access_to_the_owned_subtree() {
    /*
     * An administrator archives a tree containing a descendant hidden from an otherwise writable
     * user. That user cannot restore or permanently delete the aggregate, and both denied
     * operations preserve the restricted document and archive marker.
     */
    let pool = test_pool().await;
    let writers = create_group(&pool, "writers").await;
    let restricted = create_group(&pool, "restricted").await;
    let vault_root = get_root_folder(&pool, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");
    add_folder_permission(&pool, vault_root.id, writers, true, true, true)
        .await
        .expect("vault permission");
    add_folder_permission(&pool, archive_root.id, writers, true, true, true)
        .await
        .expect("archive permission");
    let parent = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("parent");
    let restricted_child = get_or_create_folder_path(&pool, Some("Project/Restricted"))
        .await
        .expect("restricted child");
    add_folder_permission(&pool, parent.id, writers, true, true, true)
        .await
        .expect("parent permission");
    add_folder_permission(&pool, restricted_child.id, restricted, true, true, true)
        .await
        .expect("restricted boundary");
    let protected_document =
        insert_document(&pool, restricted_child.id, "protected.txt", None).await;
    sqlx::query(
        r"
        INSERT INTO vault_settings (key, value)
        VALUES ('archivePermanentDeleteAdminOnly', 'false')
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        ",
    )
    .execute(&pool)
    .await
    .expect("allow writer permanent delete");
    let admin = user(&[], true);
    let writer = user(&["writers"], false);
    archive_folder(
        &pool,
        parent.id,
        &admin,
        &ClientMeta {
            ip: None,
            user_agent: None,
        },
    )
    .await
    .expect("admin archives aggregate");

    let delete_error = delete_archived_folder_forever(&pool, parent.id, &writer)
        .await
        .expect_err("writer must not delete a restricted descendant");
    assert!(matches!(
        delete_error,
        DocumentError::InsufficientDocumentAccess
    ));
    let restore_error = restore_folder(&pool, parent.id, &writer)
        .await
        .expect_err("writer must not restore a restricted descendant");
    assert!(matches!(
        restore_error,
        DocumentError::InsufficientDocumentAccess
    ));

    assert!(
        fetch_document_by_id(&pool, protected_document)
            .await
            .is_ok(),
        "denied aggregate mutations must preserve restricted data"
    );
    let marker: Option<String> = sqlx::query_scalar("SELECT archived_at FROM folders WHERE id = ?")
        .bind(parent.id)
        .fetch_one(&pool)
        .await
        .expect("parent marker");
    assert!(marker.is_some());
}

#[tokio::test]
async fn archive_identity_model_folder_archive_blocks_an_active_bound_upload() {
    /*
     * Binds an active create upload to a descendant of the folder being archived. The upload
     * prevents the aggregate archive from starting, leaving the source folder unmarked and
     * available as the stable upload target.
     */
    let pool = test_pool().await;
    let actor = user(&[], true);
    let source = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("source");
    let child = get_or_create_folder_path(&pool, Some("Project/Incoming"))
        .await
        .expect("child");
    sqlx::query(
        r"
        INSERT INTO upload_sessions
            (
                id, mode, status, target_folder_id, filename, total_size,
                chunk_size, part_count, created_by, user_context, expires_at
            )
        VALUES
            ('bound-upload', 'create', 'active', ?, 'pending.bin', 1, 1, 1,
             'admin', '{}', '2999-01-01T00:00:00Z')
        ",
    )
    .bind(child.id)
    .execute(&pool)
    .await
    .expect("upload");

    let error = archive_folder(
        &pool,
        source.id,
        &actor,
        &ClientMeta {
            ip: None,
            user_agent: None,
        },
    )
    .await
    .expect_err("active upload must block folder archive");
    assert!(matches!(error, DocumentError::FolderHasActiveUploads));
    let marker: Option<String> = sqlx::query_scalar("SELECT archived_at FROM folders WHERE id = ?")
        .bind(source.id)
        .fetch_one(&pool)
        .await
        .expect("source marker");
    assert!(marker.is_none());
}

#[tokio::test]
async fn archive_identity_model_archive_is_flat_but_archived_folders_are_browsable() {
    /*
     * Archives one folder subtree and one loose document as independent entries. The Archive
     * root stays flat, while the archived folder's synthetic path remains browsable through
     * its child and preserves the nested document's original path.
     */
    let pool = test_pool().await;
    let actor = user(&[], true);
    let source = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("source");
    let child = get_or_create_folder_path(&pool, Some("Project/Nested"))
        .await
        .expect("child");
    let nested_document = insert_document(&pool, child.id, "inside.txt", None).await;
    let loose_folder = get_or_create_folder_path(&pool, Some("Loose"))
        .await
        .expect("loose folder");
    let loose_document = insert_document(&pool, loose_folder.id, "loose.txt", None).await;
    let meta = ClientMeta {
        ip: None,
        user_agent: None,
    };

    archive_folder(&pool, source.id, &actor, &meta)
        .await
        .expect("archive folder");
    archive_document(&pool, loose_document, &actor, &meta)
        .await
        .expect("archive document");

    let archive = build_contents_payload(&pool, "Archive", &actor, "", false)
        .await
        .expect("archive contents");
    assert_eq!(archive.folders.len(), 1);
    assert_eq!(archive.folders[0].id, source.id);
    assert_eq!(archive.documents.len(), 1);
    assert_eq!(archive.documents[0].id, loose_document);
    assert!(archive.folders[0].path.starts_with("Archive/@"));

    let archived_root = build_contents_payload(&pool, &archive.folders[0].path, &actor, "", false)
        .await
        .expect("browse archived root");
    assert_eq!(archived_root.folders.len(), 1);
    assert_eq!(archived_root.folders[0].id, child.id);
    assert!(archived_root.documents.is_empty());

    let archived_child =
        build_contents_payload(&pool, &archived_root.folders[0].path, &actor, "", false)
            .await
            .expect("browse archived child");
    assert_eq!(archived_child.documents.len(), 1);
    assert_eq!(archived_child.documents[0].id, nested_document);
    assert_eq!(
        archived_child.documents[0].archived_original_path,
        "Project/Nested/inside.txt"
    );
}
