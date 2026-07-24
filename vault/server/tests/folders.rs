use std::time::Duration;

use tokio::sync::oneshot;
use vault_server::auth::UserContext;
use vault_server::db;
use vault_server::documents::{DocumentRecord, document_path};
use vault_server::folders::{
    ARCHIVE_ROOT_KEY, FolderError, VAULT_ROOT_KEY, access_level, add_folder_permission,
    all_folders, build_folder_path_cache, delete_empty_folder, folder_access_level,
    folder_access_levels, folder_path_by_id, folder_path_from_cache, get_folder_by_path,
    get_folder_by_path_read, get_or_create_folder_path, get_or_create_folder_path_with_created,
    get_root_folder, normalize_folder, parse_public_folder_path, public_folder_path,
    require_folder_read_access, require_folder_write_access, resolve_visible_folder_by_id,
    resolve_visible_folder_by_path, subtree_folder_ids_from_records, validate_permission_flags,
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

#[tokio::test]
async fn public_folder_paths_match_python_normalization() {
    let archive =
        parse_public_folder_path(Some(" /Archive/2026\\June ")).expect("archive public path");
    let vault = parse_public_folder_path(Some(" Project\\Plans/ ")).expect("vault public path");

    assert_eq!(
        normalize_folder(Some(" Project\\Plans/ ")).expect("path"),
        "Project/Plans"
    );
    assert_eq!(archive.root_key, ARCHIVE_ROOT_KEY);
    assert_eq!(archive.relative_path, "2026/June");
    assert_eq!(vault.root_key, VAULT_ROOT_KEY);
    assert_eq!(vault.relative_path, "Project/Plans");
    assert_eq!(
        public_folder_path(ARCHIVE_ROOT_KEY, "2026/June").expect("archive path"),
        "Archive/2026/June",
    );
    assert_eq!(
        public_folder_path(VAULT_ROOT_KEY, "Project/Plans").expect("vault path"),
        "Project/Plans",
    );
    assert!(matches!(
        normalize_folder(Some("Project/../Plans")),
        Err(FolderError::InvalidPath),
    ));
}

#[tokio::test]
async fn get_or_create_folder_path_creates_vault_folders_and_rebuilds_paths() {
    let pool = test_pool().await;

    let created = get_or_create_folder_path_with_created(&pool, Some("Project/Private"))
        .await
        .expect("create path");
    let fetched = get_or_create_folder_path(&pool, Some("Project/Private"))
        .await
        .expect("fetch path");
    let archive_child = get_folder_by_path(&pool, Some("Archive/Child"))
        .await
        .expect("archive child lookup");

    assert_eq!(created.created.len(), 2);
    assert_eq!(created.folder.id, fetched.id);
    assert_eq!(
        folder_path_by_id(&pool, fetched.id)
            .await
            .expect("folder path"),
        "Project/Private",
    );
    assert!(archive_child.is_none());
    assert!(matches!(
        get_or_create_folder_path(&pool, Some("Archive/Child")).await,
        Err(FolderError::ArchiveDoesNotContainFolders),
    ));
}

#[tokio::test]
async fn read_only_folder_path_resolver_handles_roots_deep_paths_and_missing_segments() {
    let pool = test_pool().await;
    let project = get_or_create_folder_path(&pool, Some("Project/Private"))
        .await
        .expect("create path");
    let vault_root = get_root_folder(&pool, VAULT_ROOT_KEY)
        .await
        .expect("vault root");
    let archive_root = get_root_folder(&pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive root");

    assert_eq!(
        get_folder_by_path_read(&pool, " Project\\Private/ ")
            .await
            .expect("deep read"),
        Some(project),
    );
    assert_eq!(
        get_folder_by_path_read(&pool, "")
            .await
            .expect("vault root read"),
        Some(vault_root),
    );
    assert_eq!(
        get_folder_by_path_read(&pool, "Archive")
            .await
            .expect("archive root read"),
        Some(archive_root),
    );
    assert!(
        get_folder_by_path_read(&pool, "Project/Missing/Child")
            .await
            .expect("missing read")
            .is_none()
    );
    assert!(
        get_folder_by_path_read(&pool, "Archive/Child")
            .await
            .expect("archive child read")
            .is_none()
    );
    assert!(matches!(
        get_folder_by_path_read(&pool, "Project/../Private").await,
        Err(FolderError::InvalidPath),
    ));
}

#[tokio::test]
async fn folder_path_cache_handles_roots_children_and_missing_parents() {
    let pool = test_pool().await;
    let project = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("project");
    let private = get_or_create_folder_path(&pool, Some("Project/Private"))
        .await
        .expect("private");
    let archive = get_root_folder(&pool, ARCHIVE_ROOT_KEY)
        .await
        .expect("archive");
    let folders = vault_server::folders::all_folders(&pool)
        .await
        .expect("all folders");
    let cache = build_folder_path_cache(&folders).expect("cache");

    assert_eq!(
        folder_path_from_cache(&project, &cache).expect("project path"),
        "Project",
    );
    assert_eq!(
        folder_path_from_cache(&private, &cache).expect("private path"),
        "Project/Private",
    );
    assert_eq!(
        folder_path_from_cache(&archive, &cache).expect("archive path"),
        "Archive",
    );
}

#[tokio::test]
async fn folder_path_helpers_tolerate_corrupt_parent_cycle() {
    let pool = test_pool().await;
    let first_id = sqlx::query(
        r"
        INSERT INTO folders (root_key, parent_id, name, is_root)
        VALUES ('vault', NULL, 'First', 0)
        ",
    )
    .execute(&pool)
    .await
    .expect("first folder")
    .last_insert_rowid();
    let second_id = sqlx::query(
        r"
        INSERT INTO folders (root_key, parent_id, name, is_root)
        VALUES ('vault', NULL, 'Second', 0)
        ",
    )
    .execute(&pool)
    .await
    .expect("second folder")
    .last_insert_rowid();
    sqlx::query("UPDATE folders SET parent_id = ? WHERE id = ?")
        .bind(second_id)
        .bind(first_id)
        .execute(&pool)
        .await
        .expect("link first to second");
    sqlx::query("UPDATE folders SET parent_id = ? WHERE id = ?")
        .bind(first_id)
        .bind(second_id)
        .execute(&pool)
        .await
        .expect("link second to first");

    let folders = all_folders(&pool).await.expect("all folders");
    let first = folders
        .iter()
        .find(|folder| folder.id == first_id)
        .expect("first folder row");
    let cache = build_folder_path_cache(&folders).expect("path cache");
    let relative = folder_path_from_cache(first, &cache).expect("relative path");
    let mut subtree = subtree_folder_ids_from_records(first_id, &folders);
    subtree.sort_unstable();
    let mut expected = vec![first_id, second_id];
    expected.sort_unstable();
    let doc = DocumentRecord {
        id: 1,
        folder_id: first_id,
        name: "loop.txt".to_string(),
        archived_at: None,
        archived_origin_path: None,
        archived_access: None,
    };
    let full_path = document_path(&pool, &doc).await.expect("document path");

    assert!(!relative.is_empty());
    assert_eq!(subtree, expected);
    assert!(full_path.ends_with("/loop.txt") || full_path == "loop.txt");
}

#[tokio::test]
async fn visible_folder_resolver_returns_only_viewable_canonical_paths() {
    let pool = test_pool().await;
    let root = get_root_folder(&pool, VAULT_ROOT_KEY).await.expect("root");
    let viewers = create_group(&pool, "viewers").await;
    add_folder_permission(&pool, root.id, viewers, true, false, false)
        .await
        .expect("root view access");
    let visible = get_or_create_folder_path(&pool, Some("Project/Visible"))
        .await
        .expect("visible folder");

    let resolved = resolve_visible_folder_by_id(&pool, visible.id, &user(&["viewers"], false))
        .await
        .expect("resolve visible folder")
        .expect("viewable folder");

    assert_eq!(resolved.id, visible.id);
    assert_eq!(resolved.path, "Project/Visible");

    let resolved_by_path =
        resolve_visible_folder_by_path(&pool, " Project\\Visible/ ", &user(&["viewers"], false))
            .await
            .expect("resolve visible folder path")
            .expect("viewable folder path");
    assert_eq!(resolved_by_path, resolved);
}

#[tokio::test]
async fn delete_empty_folder_rechecks_contents_after_waiting_for_writer_gate() {
    let pool = test_pool().await;
    let folder = get_or_create_folder_path(&pool, Some("Race"))
        .await
        .expect("race folder");
    let mut gate = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("writer gate");
    let delete_pool = pool.clone();
    let delete_user = user(&[], true);
    let (started_tx, started_rx) = oneshot::channel();
    let mut deletion = tokio::spawn(async move {
        started_tx.send(()).expect("signal delete start");
        delete_empty_folder(&delete_pool, folder.id, "Race", &delete_user).await
    });
    started_rx.await.expect("delete started");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut deletion)
            .await
            .is_err(),
        "delete completed before the competing writer committed",
    );

    let child_id = sqlx::query(
        r"
        INSERT INTO folders (root_key, parent_id, name, is_root)
        VALUES ('vault', ?, 'LateChild', 0)
        ",
    )
    .bind(folder.id)
    .execute(&mut *gate)
    .await
    .expect("late child")
    .last_insert_rowid();
    let document_id = sqlx::query(
        r"
        INSERT INTO documents (folder_id, name, created_by, created_by_name, latest_modified_by)
        VALUES (?, 'late.txt', 'admin', 'Admin', 'admin')
        ",
    )
    .bind(folder.id)
    .execute(&mut *gate)
    .await
    .expect("late document")
    .last_insert_rowid();
    gate.commit().await.expect("commit competing contents");

    let error = tokio::time::timeout(Duration::from_secs(5), deletion)
        .await
        .expect("delete timed out")
        .expect("delete task")
        .expect_err("nonempty folder must not be deleted");
    assert!(matches!(error, FolderError::FolderNotEmpty));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folders WHERE id IN (?, ?)")
            .bind(folder.id)
            .bind(child_id)
            .fetch_one(&pool)
            .await
            .expect("folder count"),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document count"),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_events")
            .fetch_one(&pool)
            .await
            .expect("state event count"),
        0
    );
}

#[tokio::test]
async fn visible_folder_resolver_conceals_hidden_and_missing_folders() {
    let pool = test_pool().await;
    let root = get_root_folder(&pool, VAULT_ROOT_KEY).await.expect("root");
    let viewers = create_group(&pool, "viewers").await;
    add_folder_permission(&pool, root.id, viewers, true, false, false)
        .await
        .expect("root view access");
    let hidden = get_or_create_folder_path(&pool, Some("Hidden"))
        .await
        .expect("hidden folder");
    add_folder_permission(&pool, hidden.id, viewers, false, false, false)
        .await
        .expect("hide folder");

    assert!(
        resolve_visible_folder_by_id(&pool, hidden.id, &user(&["viewers"], false))
            .await
            .expect("resolve hidden folder")
            .is_none()
    );
    assert!(
        resolve_visible_folder_by_path(&pool, "Hidden", &user(&["viewers"], false))
            .await
            .expect("resolve hidden folder path")
            .is_none()
    );
    assert!(
        resolve_visible_folder_by_id(&pool, i64::MAX, &user(&["viewers"], false))
            .await
            .expect("resolve missing folder")
            .is_none()
    );
    assert!(
        resolve_visible_folder_by_path(&pool, "Missing", &user(&["viewers"], false))
            .await
            .expect("resolve missing folder path")
            .is_none()
    );
    assert!(
        resolve_visible_folder_by_id(&pool, i64::MAX, &user(&[], true))
            .await
            .expect("admin resolves missing folder")
            .is_none()
    );
}

#[tokio::test]
async fn visible_folder_resolver_reports_visible_broken_ancestry_after_access_check() {
    let pool = test_pool().await;
    let viewers = create_group(&pool, "viewers").await;
    let detached_id = sqlx::query(
        "INSERT INTO folders (root_key, parent_id, name, is_root) VALUES ('vault', NULL, 'Detached', 0)",
    )
    .execute(&pool)
    .await
    .expect("detached folder")
    .last_insert_rowid();
    add_folder_permission(&pool, detached_id, viewers, true, false, false)
        .await
        .expect("detached view access");

    assert!(
        resolve_visible_folder_by_id(&pool, detached_id, &user(&["outsiders"], false))
            .await
            .expect("hidden malformed folder remains concealed")
            .is_none()
    );
    let viewer_error = resolve_visible_folder_by_id(&pool, detached_id, &user(&["viewers"], false))
        .await
        .expect_err("visible detached folder must report an invariant failure");
    assert!(matches!(viewer_error, FolderError::InvalidStoredHierarchy));
    let admin_error = resolve_visible_folder_by_id(&pool, detached_id, &user(&[], true))
        .await
        .expect_err("admin-visible detached folder must report an invariant failure");
    assert!(matches!(admin_error, FolderError::InvalidStoredHierarchy));
    assert!(
        resolve_visible_folder_by_path(&pool, "Detached", &user(&[], true))
            .await
            .expect("admin resolves detached folder path")
            .is_none()
    );
}

#[tokio::test]
async fn visible_folder_resolver_rejects_a_nonbinary_fake_root() {
    let pool = test_pool().await;
    let fake_root_id = sqlx::query(
        "INSERT INTO folders (root_key, parent_id, name, is_root) VALUES ('vault', NULL, '', 2)",
    )
    .execute(&pool)
    .await
    .expect("nonbinary fake root")
    .last_insert_rowid();

    let error = resolve_visible_folder_by_id(&pool, fake_root_id, &user(&[], true))
        .await
        .expect_err("nonbinary fake root must not resolve as canonical ancestry");
    assert!(matches!(error, FolderError::InvalidStoredHierarchy));
}

#[tokio::test]
async fn folder_access_uses_nearest_direct_acl_and_admin_override() {
    let pool = test_pool().await;
    let root = get_root_folder(&pool, VAULT_ROOT_KEY).await.expect("root");
    let writers = create_group(&pool, "writers").await;
    let outsiders = create_group(&pool, "outsiders").await;
    add_folder_permission(&pool, root.id, writers, true, true, true)
        .await
        .expect("writer root");
    add_folder_permission(&pool, root.id, outsiders, true, false, false)
        .await
        .expect("outsider root");

    let open = get_or_create_folder_path(&pool, Some("Open"))
        .await
        .expect("open");
    let secret = get_or_create_folder_path(&pool, Some("Secret"))
        .await
        .expect("secret");
    let plans = get_or_create_folder_path(&pool, Some("Secret/Plans"))
        .await
        .expect("plans");
    add_folder_permission(&pool, secret.id, outsiders, false, false, false)
        .await
        .expect("deny outsiders");

    assert_eq!(
        folder_access_level(&pool, open.id, &user(&["writers"], false))
            .await
            .expect("writer inherited"),
        3,
    );
    assert_eq!(
        folder_access_level(&pool, open.id, &user(&["outsiders"], false))
            .await
            .expect("outsider inherited"),
        1,
    );
    assert_eq!(
        folder_access_level(&pool, plans.id, &user(&["outsiders"], false))
            .await
            .expect("outsider direct deny"),
        0,
    );
    assert_eq!(
        folder_access_level(&pool, plans.id, &user(&["outsiders"], true))
            .await
            .expect("admin override"),
        3,
    );
}

#[tokio::test]
async fn folder_access_batch_preserves_boundary_group_and_flag_semantics() {
    let pool = test_pool().await;
    let root = get_root_folder(&pool, VAULT_ROOT_KEY).await.expect("root");
    let root_users = create_group(&pool, "root-users").await;
    let readers = create_group(&pool, "\tReAdErS\u{2003}").await;
    let writers = create_group(&pool, "writers").await;
    let unrelated = create_group(&pool, "unrelated").await;
    add_folder_permission(&pool, root.id, root_users, true, true, true)
        .await
        .expect("root access");

    let open = get_or_create_folder_path(&pool, Some("Open"))
        .await
        .expect("open");
    let boundary = get_or_create_folder_path(&pool, Some("Boundary"))
        .await
        .expect("boundary");
    let descendant = get_or_create_folder_path(&pool, Some("Boundary/Descendant"))
        .await
        .expect("descendant");
    let reopened = get_or_create_folder_path(&pool, Some("Boundary/Descendant/Reopened"))
        .await
        .expect("reopened");
    add_folder_permission(&pool, boundary.id, readers, true, true, false)
        .await
        .expect("reader boundary");
    add_folder_permission(&pool, boundary.id, writers, true, true, true)
        .await
        .expect("writer boundary");
    add_folder_permission(&pool, boundary.id, unrelated, true, false, false)
        .await
        .expect("unrelated boundary");
    add_folder_permission(&pool, reopened.id, root_users, true, true, true)
        .await
        .expect("reopened boundary");

    let levels = folder_access_levels(
        &pool,
        &[open.id, boundary.id, descendant.id, descendant.id],
        &user(&[" ROOT-USERS ", " readers ", "WRITERS"], false),
    )
    .await
    .expect("batch access");
    assert_eq!(levels.len(), 3);
    assert_eq!(levels[&open.id], 3);
    assert_eq!(levels[&boundary.id], 3);
    assert_eq!(levels[&descendant.id], 3);

    let reader_levels = folder_access_levels(
        &pool,
        &[boundary.id, descendant.id],
        &user(&["readers"], false),
    )
    .await
    .expect("reader access");
    assert_eq!(reader_levels[&boundary.id], 2);
    assert_eq!(reader_levels[&descendant.id], 2);

    let denied = folder_access_levels(
        &pool,
        &[boundary.id, descendant.id, reopened.id],
        &user(&["root-users"], false),
    )
    .await
    .expect("unmatched boundary");
    assert_eq!(denied[&boundary.id], 0);
    assert_eq!(denied[&descendant.id], 0);
    assert_eq!(denied[&reopened.id], 3);
}

#[tokio::test]
async fn folder_access_batch_handles_cycles_and_more_than_sqlite_bind_limit() {
    let pool = test_pool().await;
    let root = get_root_folder(&pool, VAULT_ROOT_KEY).await.expect("root");
    let readers = create_group(&pool, "readers").await;
    add_folder_permission(&pool, root.id, readers, true, true, false)
        .await
        .expect("root reader");

    let first_id = sqlx::query(
        "INSERT INTO folders (root_key, parent_id, name, is_root) VALUES ('vault', NULL, 'Cycle A', 0)",
    )
    .execute(&pool)
    .await
    .expect("first cycle folder")
    .last_insert_rowid();
    let second_id = sqlx::query(
        "INSERT INTO folders (root_key, parent_id, name, is_root) VALUES ('vault', ?, 'Cycle B', 0)",
    )
    .bind(first_id)
    .execute(&pool)
    .await
    .expect("second cycle folder")
    .last_insert_rowid();
    sqlx::query("UPDATE folders SET parent_id = ? WHERE id = ?")
        .bind(second_id)
        .bind(first_id)
        .execute(&pool)
        .await
        .expect("close parent cycle");
    add_folder_permission(&pool, second_id, readers, true, true, false)
        .await
        .expect("cycle boundary");

    sqlx::query(
        r"
        WITH RECURSIVE numbers(value) AS (
            VALUES (1)
            UNION ALL
            SELECT value + 1 FROM numbers WHERE value < 1100
        )
        INSERT INTO folders (root_key, parent_id, name, is_root)
        SELECT 'vault', ?, printf('Wide-%04d', value), 0
        FROM numbers
        ",
    )
    .bind(root.id)
    .execute(&pool)
    .await
    .expect("wide folders");
    let mut folder_ids = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM folders WHERE parent_id = ? AND name LIKE 'Wide-%' ORDER BY id",
    )
    .bind(root.id)
    .fetch_all(&pool)
    .await
    .expect("wide folder ids");
    assert_eq!(folder_ids.len(), 1_100);
    folder_ids.push(first_id);
    folder_ids.push(i64::MAX);

    let levels = folder_access_levels(&pool, &folder_ids, &user(&[" readers "], false))
        .await
        .expect("large batch access");
    assert_eq!(levels.len(), 1_101);
    assert_eq!(levels[&first_id], 2);
    assert!(!levels.contains_key(&i64::MAX));
    assert!(levels.values().all(|level| *level == 2));

    assert!(matches!(
        folder_access_level(&pool, i64::MAX, &user(&["readers"], false)).await,
        Err(FolderError::Database(sqlx::Error::RowNotFound)),
    ));
    assert_eq!(
        folder_access_level(&pool, i64::MAX, &user(&[], true))
            .await
            .expect("admin short circuit"),
        3,
    );
}

#[tokio::test]
async fn folder_access_helpers_preserve_read_write_and_hidden_semantics() {
    let pool = test_pool().await;
    let root = get_root_folder(&pool, VAULT_ROOT_KEY).await.expect("root");
    let viewers = create_group(&pool, "viewers").await;
    let readers = create_group(&pool, "readers").await;
    let writers = create_group(&pool, "writers").await;
    add_folder_permission(&pool, root.id, viewers, true, false, false)
        .await
        .expect("viewer root");
    add_folder_permission(&pool, root.id, readers, true, true, false)
        .await
        .expect("reader root");
    add_folder_permission(&pool, root.id, writers, true, true, true)
        .await
        .expect("writer root");

    let project = get_or_create_folder_path(&pool, Some("Project"))
        .await
        .expect("project");
    let secret = get_or_create_folder_path(&pool, Some("Project/Secret"))
        .await
        .expect("secret");
    add_folder_permission(&pool, secret.id, viewers, false, false, false)
        .await
        .expect("viewer direct deny");

    require_folder_read_access(&pool, project.id, &user(&["readers"], false))
        .await
        .expect("reader can read");
    assert!(matches!(
        require_folder_read_access(&pool, project.id, &user(&["viewers"], false))
            .await
            .expect_err("viewer cannot read"),
        FolderError::InsufficientFolderAccess
    ));
    assert!(matches!(
        require_folder_read_access(&pool, project.id, &user(&["outsiders"], false))
            .await
            .expect_err("outsider is hidden"),
        FolderError::FolderNotFound
    ));

    require_folder_write_access(&pool, project.id, &user(&["writers"], false))
        .await
        .expect("writer can write");
    assert!(matches!(
        require_folder_write_access(&pool, project.id, &user(&["readers"], false))
            .await
            .expect_err("reader cannot write"),
        FolderError::InsufficientFolderAccess
    ));
    assert!(matches!(
        require_folder_read_access(&pool, secret.id, &user(&["viewers"], false))
            .await
            .expect_err("direct deny is hidden"),
        FolderError::FolderNotFound
    ));
}

#[test]
fn permission_flag_validation_matches_api_contract() {
    assert_eq!(access_level(true, true, true), 3);
    assert_eq!(access_level(true, true, false), 2);
    assert_eq!(access_level(true, false, false), 1);
    assert_eq!(access_level(false, false, false), 0);
    assert!(matches!(
        validate_permission_flags(false, false, true),
        Err(FolderError::WriteRequiresReadAndView),
    ));
    assert!(matches!(
        validate_permission_flags(false, true, false),
        Err(FolderError::ReadRequiresView),
    ));
}
