use serde_json::json;
use vault_server::auth::UserContext;
use vault_server::db;
use vault_server::documents::{
    AccessPayload, DocumentError, access_payload, archive_access_snapshot, document_access_level,
    document_folder_path, document_for_read, document_for_write, document_is_archive,
    document_path, editable_document_for_write, fetch_document_by_id, normalize_file_name,
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
