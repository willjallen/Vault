use vault_server::auth::UserContext;
use vault_server::db;
use vault_server::documents::{DocumentRecord, document_path};
use vault_server::folders::{
    ARCHIVE_ROOT_KEY, FolderError, VAULT_ROOT_KEY, access_level, add_folder_permission,
    all_folders, build_folder_path_cache, folder_access_level, folder_path_by_id,
    folder_path_from_cache, get_folder_by_path, get_or_create_folder_path,
    get_or_create_folder_path_with_created, get_root_folder, normalize_folder,
    parse_public_folder_path, public_folder_path, require_folder_read_access,
    require_folder_write_access, subtree_folder_ids_from_records, validate_permission_flags,
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
        archived_from_folder: None,
        archived_original_name: None,
        archived_access: None,
    };
    let full_path = document_path(&pool, &doc).await.expect("document path");

    assert!(!relative.is_empty());
    assert_eq!(subtree, expected);
    assert!(full_path.ends_with("/loop.txt") || full_path == "loop.txt");
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
