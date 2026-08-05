use std::time::Duration;

use vault_server::auth::UserContext;
use vault_server::blob_lifecycle::collect_unreferenced_blobs_with_limit;
use vault_server::db;
use vault_server::previews::{
    PREVIEW_RECIPE, PreviewError, ResolvePreviewDocumentRequest, ResolvePreviewRequest,
    VisualSource, authorize_resolve_sources, enqueue_preview_jobs, prune_preview_cache,
    requeue_rendition_blob, semantic_icon_key, validate_resolve_request, visual_payloads,
};
use vault_server::storage::LocalBlobStorage;

fn user(groups: &[&str], is_admin: bool) -> UserContext {
    UserContext {
        id: if is_admin { "admin" } else { "user" }.to_string(),
        vault_user_id: if is_admin { 1 } else { 2 },
        issuer: "test".to_string(),
        subject: if is_admin { "admin" } else { "user" }.to_string(),
        name: if is_admin { "Admin" } else { "User" }.to_string(),
        email: String::new(),
        groups: groups.iter().map(ToString::to_string).collect(),
        is_admin,
    }
}

#[test]
fn resolve_rejects_duplicate_document_ids_and_invalid_versions() {
    /*
     * A preview-resolution batch must identify each document once and bind it to a nonblank
     * version. Duplicate document entries and whitespace-only version identifiers are both
     * rejected before authorization or lookup.
     */
    let duplicate = ResolvePreviewRequest {
        documents: vec![
            ResolvePreviewDocumentRequest {
                document_id: 7,
                version_id: "version-a".to_string(),
            },
            ResolvePreviewDocumentRequest {
                document_id: 7,
                version_id: "version-b".to_string(),
            },
        ],
    };
    assert!(matches!(
        validate_resolve_request(&duplicate),
        Err(PreviewError::InvalidResolveRequest)
    ));

    let empty_version = ResolvePreviewRequest {
        documents: vec![ResolvePreviewDocumentRequest {
            document_id: 1,
            version_id: "  ".to_string(),
        }],
    };
    assert!(matches!(
        validate_resolve_request(&empty_version),
        Err(PreviewError::InvalidResolveRequest)
    ));
}

#[test]
fn semantic_icons_cover_previewable_and_common_fallback_types() {
    /*
     * Filename extensions and MIME hints should map common creative, image, PDF, archive, and
     * source-code files to stable semantic icons. Unknown binary content falls back to the
     * generic file icon.
     */
    assert_eq!(semantic_icon_key("scene.blend", None), "app-blender");
    assert_eq!(semantic_icon_key("texture.PNG", None), "file-image");
    assert_eq!(
        semantic_icon_key("notes", Some("application/pdf")),
        "file-pdf"
    );
    assert_eq!(semantic_icon_key("bundle.zip", None), "file-zipper");
    assert_eq!(
        semantic_icon_key("tool.rs", Some("text/plain")),
        "file-code"
    );
    assert_eq!(semantic_icon_key("unknown.bin", None), "file");
}

async fn seed_source(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    version_id: &str,
    digest: &str,
) -> (i64, i64) {
    sqlx::query(
        "INSERT OR IGNORE INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', ?, 1)",
    )
    .bind(digest)
    .execute(pool)
    .await
    .expect("blob");
    let blob_id: i64 =
        sqlx::query_scalar("SELECT id FROM blobs WHERE hash_algo = 'sha256' AND hash = ?")
            .bind(digest)
            .fetch_one(pool)
            .await
            .expect("blob id");
    let document_id = sqlx::query("INSERT INTO documents (folder_id, name) VALUES (?, ?)")
        .bind(folder_id)
        .bind(name)
        .execute(pool)
        .await
        .expect("document")
        .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO document_versions
            (id, document_id, blob_id, version_number, committed_by, mime_type)
        VALUES (?, ?, ?, 1, 'admin', 'image/png')
        ",
    )
    .bind(version_id)
    .bind(document_id)
    .bind(blob_id)
    .execute(pool)
    .await
    .expect("version");
    sqlx::query(
        r"
        UPDATE documents
        SET current_version_id = ?, latest_version_number = 1, version_count = 1
        WHERE id = ?
        ",
    )
    .bind(version_id)
    .bind(document_id)
    .execute(pool)
    .await
    .expect("current version");
    (document_id, blob_id)
}

async fn seed_preview_job(
    pool: &sqlx::SqlitePool,
    folder_id: i64,
    name: &str,
    version_id: &str,
    digest: &str,
) -> (i64, i64, i64) {
    let (document_id, source_blob_id) =
        seed_source(pool, folder_id, name, version_id, digest).await;
    enqueue_preview_jobs(pool, &[source_blob_id])
        .await
        .expect("preview job");
    let job_id = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM preview_jobs WHERE source_blob_id = ? AND recipe = ?",
    )
    .bind(source_blob_id)
    .bind(PREVIEW_RECIPE)
    .fetch_one(pool)
    .await
    .expect("preview job id");
    (document_id, source_blob_id, job_id)
}

async fn seed_rendition(
    pool: &sqlx::SqlitePool,
    job_id: i64,
    variant: &str,
    digest: &str,
    size_bytes: i64,
) -> i64 {
    let blob_id =
        sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', ?, ?)")
            .bind(digest)
            .bind(size_bytes)
            .execute(pool)
            .await
            .expect("rendition blob")
            .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO preview_renditions
            (preview_job_id, variant, blob_id, mime_type, width, height)
        VALUES (?, ?, ?, 'image/webp', 1, 1)
        ",
    )
    .bind(job_id)
    .bind(variant)
    .bind(blob_id)
    .execute(pool)
    .await
    .expect("rendition");
    blob_id
}

#[tokio::test]
async fn preview_identity_survives_rename_move_and_shared_content() {
    /*
     * Preview work is keyed by source blob content rather than a document's name, folder, or
     * version row. Renaming and moving one of two documents sharing that blob leaves both
     * with pending descriptors backed by a single preview job.
     */
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("root");
    let first_folder =
        sqlx::query("INSERT INTO folders (root_key, parent_id, name) VALUES ('vault', ?, 'First')")
            .bind(root_id)
            .execute(&pool)
            .await
            .expect("first folder")
            .last_insert_rowid();
    let second_folder = sqlx::query(
        "INSERT INTO folders (root_key, parent_id, name) VALUES ('vault', ?, 'Second')",
    )
    .bind(root_id)
    .execute(&pool)
    .await
    .expect("second folder")
    .last_insert_rowid();
    let digest = "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7c5c9e3d3f8b6b5";
    let (document_id, blob_id) =
        seed_source(&pool, first_folder, "before.png", "version-a", digest).await;
    let (shared_document_id, shared_blob_id) =
        seed_source(&pool, first_folder, "shared.png", "version-b", digest).await;
    assert_eq!(blob_id, shared_blob_id);
    assert_eq!(
        enqueue_preview_jobs(&pool, &[blob_id, shared_blob_id])
            .await
            .unwrap(),
        1
    );

    sqlx::query("UPDATE documents SET name = 'after.png', folder_id = ? WHERE id = ?")
        .bind(second_folder)
        .bind(document_id)
        .execute(&pool)
        .await
        .expect("rename and move");
    let visuals = visual_payloads(
        &pool,
        &[
            VisualSource {
                document_id,
                name: "after.png",
                version_id: Some("version-a"),
                blob_id: Some(blob_id),
                mime_type: Some("image/png"),
                can_read: true,
            },
            VisualSource {
                document_id: shared_document_id,
                name: "shared.png",
                version_id: Some("version-b"),
                blob_id: Some(blob_id),
                mime_type: Some("image/png"),
                can_read: true,
            },
        ],
    )
    .await
    .expect("visuals");
    for document_id in [document_id, shared_document_id] {
        let preview = visuals[&document_id]
            .preview
            .as_ref()
            .expect("preview descriptor");
        assert_eq!(preview.recipe, PREVIEW_RECIPE);
        assert_eq!(preview.status, "pending");
    }
    let job_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM preview_jobs")
        .fetch_one(&pool)
        .await
        .expect("job count");
    assert_eq!(job_count, 1);
}

#[tokio::test]
async fn resolve_requires_read_access_in_one_bounded_batch() {
    /*
     * Folder visibility alone is insufficient to resolve a content preview because doing so
     * exposes document bytes. A view-only group member is denied, while an administrator can
     * resolve the same requested document and version.
     */
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("root");
    let folder_id = sqlx::query(
        "INSERT INTO folders (root_key, parent_id, name) VALUES ('vault', ?, 'Visible')",
    )
    .bind(root_id)
    .execute(&pool)
    .await
    .expect("folder")
    .last_insert_rowid();
    let group_id = sqlx::query("INSERT INTO vault_groups (name) VALUES ('viewers')")
        .execute(&pool)
        .await
        .expect("group")
        .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO folder_permissions (folder_id, group_id, can_view, can_read, can_write)
        VALUES (?, ?, 1, 0, 0)
        ",
    )
    .bind(folder_id)
    .bind(group_id)
    .execute(&pool)
    .await
    .expect("view permission");
    let digest = "dbc1b4c900ffe48d575b5da5c638040125f65db0fe3e24494b76ea986457d986";
    let (document_id, _) =
        seed_source(&pool, folder_id, "preview.png", "version-read", digest).await;
    let request = ResolvePreviewRequest {
        documents: vec![ResolvePreviewDocumentRequest {
            document_id,
            version_id: "version-read".to_string(),
        }],
    };
    let error = authorize_resolve_sources(&pool, &user(&["viewers"], false), &request)
        .await
        .expect_err("view-only users cannot resolve content previews");
    assert!(matches!(error, PreviewError::InsufficientDocumentAccess));
    let authorized = authorize_resolve_sources(&pool, &user(&[], true), &request)
        .await
        .expect("admin resolve");
    assert_eq!(authorized.len(), 1);
    assert_eq!(authorized[0].document_id, document_id);
}

#[tokio::test]
async fn rendition_blobs_are_strong_references_until_last_shared_source_is_deleted() {
    /*
     * A rendition remains rooted by its preview job while any document still references the
     * shared source blob. Deleting the last source document cascades the job, after which a
     * subsequent collection can reclaim the rendition blob.
     */
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let storage = LocalBlobStorage::new(temp.path().join("objects"), "");
    storage.ensure().await.expect("storage");
    let root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("root");
    let digest = "e52d9c508c502347344d8c07ad91cbd6068afc75ff6292f062a09ca381c89e71";
    let (first_document, source_blob_id) =
        seed_source(&pool, root_id, "one.png", "version-one", digest).await;
    let (second_document, _) = seed_source(&pool, root_id, "two.png", "version-two", digest).await;
    enqueue_preview_jobs(&pool, &[source_blob_id])
        .await
        .expect("preview job");
    let job_id: i64 = sqlx::query_scalar("SELECT id FROM preview_jobs")
        .fetch_one(&pool)
        .await
        .expect("job id");
    let output_blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', 'ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb', 1)
        ",
    )
    .execute(&pool)
    .await
    .expect("output blob")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO preview_renditions
            (preview_job_id, variant, blob_id, mime_type, width, height)
        VALUES (?, 'small', ?, 'image/webp', 1, 1)
        ",
    )
    .bind(job_id)
    .bind(output_blob_id)
    .execute(&pool)
    .await
    .expect("rendition");
    sqlx::query("UPDATE preview_jobs SET status = 'ready' WHERE id = ?")
        .bind(job_id)
        .execute(&pool)
        .await
        .expect("ready");

    collect_unreferenced_blobs_with_limit(&pool, &storage, 32)
        .await
        .expect("initial gc");
    let output_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE id = ?")
        .bind(output_blob_id)
        .fetch_one(&pool)
        .await
        .expect("output exists");
    assert_eq!(output_exists, 1);

    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(first_document)
        .execute(&pool)
        .await
        .expect("delete first document");
    collect_unreferenced_blobs_with_limit(&pool, &storage, 32)
        .await
        .expect("shared-source gc");
    let job_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM preview_jobs WHERE id = ?")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .expect("job exists");
    assert_eq!(job_exists, 1);

    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(second_document)
        .execute(&pool)
        .await
        .expect("delete second document");
    collect_unreferenced_blobs_with_limit(&pool, &storage, 32)
        .await
        .expect("source gc");
    let job_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM preview_jobs WHERE id = ?")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .expect("job cascade");
    assert_eq!(job_exists, 0);
    collect_unreferenced_blobs_with_limit(&pool, &storage, 32)
        .await
        .expect("rendition gc");
    let output_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE id = ?")
        .bind(output_blob_id)
        .fetch_one(&pool)
        .await
        .expect("output deleted");
    assert_eq!(output_exists, 0);
}

#[tokio::test]
async fn a_rendition_deduplicated_to_its_source_does_not_create_a_gc_cycle() {
    /*
     * A generated rendition may deduplicate to exactly the same blob row as its source.
     * Once the source document is deleted, that self-reference must not keep either the preview
     * job or blob alive indefinitely.
     */
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let storage = LocalBlobStorage::new(temp.path().join("objects"), "");
    storage.ensure().await.expect("storage");
    let root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("root");
    let digest = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    let (document_id, source_blob_id) =
        seed_source(&pool, root_id, "same.webp", "version-same", digest).await;
    enqueue_preview_jobs(&pool, &[source_blob_id])
        .await
        .expect("preview job");
    let job_id: i64 = sqlx::query_scalar("SELECT id FROM preview_jobs")
        .fetch_one(&pool)
        .await
        .expect("job id");
    sqlx::query(
        r"
        INSERT INTO preview_renditions
            (preview_job_id, variant, blob_id, mime_type, width, height)
        VALUES (?, 'small', ?, 'image/webp', 1, 1)
        ",
    )
    .bind(job_id)
    .bind(source_blob_id)
    .execute(&pool)
    .await
    .expect("self-deduplicated rendition");
    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(document_id)
        .execute(&pool)
        .await
        .expect("delete document");

    collect_unreferenced_blobs_with_limit(&pool, &storage, 32)
        .await
        .expect("gc self-deduplicated preview");

    let source_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE id = ?")
        .bind(source_blob_id)
        .fetch_one(&pool)
        .await
        .expect("source count");
    let job_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM preview_jobs WHERE id = ?")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .expect("job count");
    assert_eq!(source_exists, 0);
    assert_eq!(job_exists, 0);
}

#[tokio::test]
async fn pruning_releases_derivatives_without_touching_sources_and_expires_old_recipes() {
    /*
     * Quota pruning removes a current preview job and releases its derivative without deleting
     * the source blob or document. Separately, age pruning removes an obsolete unsupported
     * recipe even when the cache is otherwise under budget.
     */
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("root");
    let digest = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881";
    let (document_id, source_blob_id) =
        seed_source(&pool, root_id, "source.png", "version-source", digest).await;
    enqueue_preview_jobs(&pool, &[source_blob_id])
        .await
        .expect("preview job");
    let current_job_id: i64 = sqlx::query_scalar("SELECT id FROM preview_jobs WHERE recipe = ?")
        .bind(PREVIEW_RECIPE)
        .fetch_one(&pool)
        .await
        .expect("current job");
    let output_blob_id = sqlx::query(
        r"
        INSERT INTO blobs (hash_algo, hash, size_bytes)
        VALUES ('sha256', '594e519ae499312b29433b7dd8a97ff068defcba9755b6d5d00e84c1c8c35053', 100)
        ",
    )
    .execute(&pool)
    .await
    .expect("output blob")
    .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO preview_renditions
            (preview_job_id, variant, blob_id, mime_type, width, height)
        VALUES (?, 'small', ?, 'image/webp', 1, 1)
        ",
    )
    .bind(current_job_id)
    .bind(output_blob_id)
    .execute(&pool)
    .await
    .expect("rendition");
    sqlx::query("UPDATE preview_jobs SET status = 'ready' WHERE id = ?")
        .bind(current_job_id)
        .execute(&pool)
        .await
        .expect("ready");

    let quota_prune = prune_preview_cache(&pool, 0, Duration::from_hours(8_760), 32)
        .await
        .expect("quota prune");
    assert_eq!(quota_prune.deleted_job_ids, vec![current_job_id]);
    assert_eq!(quota_prune.released_blob_ids, vec![output_blob_id]);
    let source_still_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE id = ?")
        .bind(source_blob_id)
        .fetch_one(&pool)
        .await
        .expect("source exists");
    assert_eq!(source_still_exists, 1);
    let document_still_exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .expect("document exists");
    assert_eq!(document_still_exists, 1);

    let historical_job_id = sqlx::query(
        r"
        INSERT INTO preview_jobs
            (source_blob_id, recipe, status, updated_at, completed_at)
        VALUES (?, 'raster-obsolete', 'unsupported', '2000-01-01', '2000-01-01')
        ",
    )
    .bind(source_blob_id)
    .execute(&pool)
    .await
    .expect("historical job")
    .last_insert_rowid();
    let age_prune = prune_preview_cache(&pool, i64::MAX, Duration::from_hours(24), 32)
        .await
        .expect("age prune");
    assert_eq!(age_prune.deleted_job_ids, vec![historical_job_id]);
    assert!(age_prune.released_blob_ids.is_empty());
}

#[tokio::test]
async fn quota_pruning_excludes_document_and_export_rooted_renditions() {
    /*
     * Rendition blobs that are also rooted by a live document or export artifact cannot be
     * reclaimed merely to satisfy preview-cache quota. Even a zero-byte budget therefore
     * leaves their preview job and blob references intact.
     */
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("root");
    let (_, source_blob_id, job_id) = seed_preview_job(
        &pool,
        root_id,
        "rooted.png",
        "version-rooted",
        "1111111111111111111111111111111111111111111111111111111111111111",
    )
    .await;
    sqlx::query(
        r"
        INSERT INTO preview_renditions
            (preview_job_id, variant, blob_id, mime_type, width, height)
        VALUES (?, 'small', ?, 'image/webp', 1, 1)
        ",
    )
    .bind(job_id)
    .bind(source_blob_id)
    .execute(&pool)
    .await
    .expect("document-rooted rendition");
    let export_blob_id = seed_rendition(
        &pool,
        job_id,
        "medium",
        "2222222222222222222222222222222222222222222222222222222222222222",
        500,
    )
    .await;
    sqlx::query(
        r"
        INSERT INTO export_jobs
            (id, status, filename, total_items, total_bytes, created_by, user_context, expires_at)
        VALUES ('export-preview-root', 'ready', 'rooted.zip', 1, 500, 'admin', '{}',
                datetime('now', '+1 day'))
        ",
    )
    .execute(&pool)
    .await
    .expect("export job");
    sqlx::query(
        r"
        INSERT INTO export_artifacts
            (job_id, blob_id, filename, mime_type, size_bytes, hash, expires_at)
        VALUES ('export-preview-root', ?, 'rooted.zip', 'application/zip', 500, ?,
                datetime('now', '+1 day'))
        ",
    )
    .bind(export_blob_id)
    .bind("2222222222222222222222222222222222222222222222222222222222222222")
    .execute(&pool)
    .await
    .expect("export artifact");
    sqlx::query("UPDATE preview_jobs SET status = 'ready' WHERE id = ?")
        .bind(job_id)
        .execute(&pool)
        .await
        .expect("ready preview");

    let pruned = prune_preview_cache(&pool, 0, Duration::from_hours(8_760), 32)
        .await
        .expect("quota prune");
    assert!(pruned.deleted_job_ids.is_empty());
    assert!(pruned.released_blob_ids.is_empty());
    let job_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM preview_jobs WHERE id = ?")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .expect("job exists");
    assert_eq!(job_exists, 1);
}

#[tokio::test]
async fn quota_pruning_stops_after_the_minimum_lru_jobs_reach_the_budget() {
    /*
     * Three ready previews consume equal space and have distinct access times, while the budget
     * permits two. Pruning releases only the oldest job and stops as soon as the remaining
     * cache reaches the target.
     */
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("root");
    let mut jobs = Vec::new();
    let mut outputs = Vec::new();
    for (ordinal, source_digest, output_digest) in [
        (
            1,
            "3333333333333333333333333333333333333333333333333333333333333333",
            "4444444444444444444444444444444444444444444444444444444444444444",
        ),
        (
            2,
            "5555555555555555555555555555555555555555555555555555555555555555",
            "6666666666666666666666666666666666666666666666666666666666666666",
        ),
        (
            3,
            "7777777777777777777777777777777777777777777777777777777777777777",
            "8888888888888888888888888888888888888888888888888888888888888888",
        ),
    ] {
        let (_, _, job_id) = seed_preview_job(
            &pool,
            root_id,
            &format!("lru-{ordinal}.png"),
            &format!("version-lru-{ordinal}"),
            source_digest,
        )
        .await;
        let output_blob_id = seed_rendition(&pool, job_id, "small", output_digest, 100).await;
        sqlx::query("UPDATE preview_jobs SET status = 'ready', last_accessed_at = ? WHERE id = ?")
            .bind(format!("2000-01-0{ordinal} 00:00:00"))
            .bind(job_id)
            .execute(&pool)
            .await
            .expect("ready preview");
        jobs.push(job_id);
        outputs.push(output_blob_id);
    }

    let pruned = prune_preview_cache(&pool, 200, Duration::from_hours(8_760), 32)
        .await
        .expect("quota prune");
    assert_eq!(pruned.deleted_job_ids, vec![jobs[0]]);
    assert_eq!(pruned.released_blob_ids, vec![outputs[0]]);
    let remaining_jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM preview_jobs")
        .fetch_one(&pool)
        .await
        .expect("remaining jobs");
    assert_eq!(remaining_jobs, 2);
}

#[tokio::test]
async fn quota_pruning_does_not_evict_a_job_for_output_still_shared_by_a_live_job() {
    /*
     * The oldest ready job shares its output with a still-running job, so deleting it would not
     * actually release that storage. Quota enforcement skips that ineffective candidate and
     * evicts the independently owned ready rendition instead.
     */
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let root_id: i64 =
        sqlx::query_scalar("SELECT id FROM folders WHERE root_key = 'vault' AND is_root = 1")
            .fetch_one(&pool)
            .await
            .expect("root");
    let mut job_ids = Vec::new();
    for (ordinal, digest) in [
        (
            1,
            "9999999999999999999999999999999999999999999999999999999999999999",
        ),
        (
            2,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ),
        (
            3,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ),
    ] {
        let (_, _, job_id) = seed_preview_job(
            &pool,
            root_id,
            &format!("shared-{ordinal}.png"),
            &format!("version-shared-{ordinal}"),
            digest,
        )
        .await;
        job_ids.push(job_id);
    }
    let shared_blob_id =
        sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', ?, 100)")
            .bind("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
            .execute(&pool)
            .await
            .expect("shared rendition blob")
            .last_insert_rowid();
    for job_id in &job_ids[..2] {
        sqlx::query(
            r"
            INSERT INTO preview_renditions
                (preview_job_id, variant, blob_id, mime_type, width, height)
            VALUES (?, 'small', ?, 'image/webp', 1, 1)
            ",
        )
        .bind(job_id)
        .bind(shared_blob_id)
        .execute(&pool)
        .await
        .expect("shared rendition");
    }
    let unique_blob_id = seed_rendition(
        &pool,
        job_ids[2],
        "small",
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        100,
    )
    .await;
    for (job_id, status, last_accessed_at) in [
        (job_ids[0], "ready", "2000-01-01 00:00:00"),
        (job_ids[1], "running", "2000-01-03 00:00:00"),
        (job_ids[2], "ready", "2000-01-02 00:00:00"),
    ] {
        sqlx::query("UPDATE preview_jobs SET status = ?, last_accessed_at = ? WHERE id = ?")
            .bind(status)
            .bind(last_accessed_at)
            .bind(job_id)
            .execute(&pool)
            .await
            .expect("preview state");
    }

    let pruned = prune_preview_cache(&pool, 100, Duration::from_hours(8_760), 32)
        .await
        .expect("quota prune");
    assert_eq!(pruned.deleted_job_ids, vec![job_ids[2]]);
    assert_eq!(pruned.released_blob_ids, vec![unique_blob_id]);
    let shared_jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM preview_jobs WHERE id IN (?, ?)")
            .bind(job_ids[0])
            .bind(job_ids[1])
            .fetch_one(&pool)
            .await
            .expect("shared jobs");
    assert_eq!(shared_jobs, 2);
}

#[tokio::test]
async fn an_unavailable_shared_rendition_requeues_every_affected_job() {
    /*
     * Two ready preview jobs point at the same rendition blob when one consumer reports that
     * output unavailable. Invalidating the shared blob removes every rendition link, returns
     * the released blob id, and queues both producers for regeneration.
     */
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let first_source =
        sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', ?, 1)")
            .bind("4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7c5e3d3f8b6b5")
            .execute(&pool)
            .await
            .expect("first source")
            .last_insert_rowid();
    let second_source =
        sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', ?, 1)")
            .bind("dbc1b4c900ffe48d575b5da5c638040125f65db0fe3e24494b76ea986457d986")
            .execute(&pool)
            .await
            .expect("second source")
            .last_insert_rowid();
    enqueue_preview_jobs(&pool, &[first_source, second_source])
        .await
        .expect("preview jobs");
    sqlx::query("UPDATE preview_jobs SET status = 'ready'")
        .execute(&pool)
        .await
        .expect("ready jobs");
    let output_blob =
        sqlx::query("INSERT INTO blobs (hash_algo, hash, size_bytes) VALUES ('sha256', ?, 1)")
            .bind("ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb")
            .execute(&pool)
            .await
            .expect("output blob")
            .last_insert_rowid();
    sqlx::query(
        r"
        INSERT INTO preview_renditions (preview_job_id, variant, blob_id, mime_type, width, height)
        SELECT id, 'small', ?, 'image/webp', 1, 1 FROM preview_jobs
        ",
    )
    .bind(output_blob)
    .execute(&pool)
    .await
    .expect("shared renditions");

    let released = requeue_rendition_blob(&pool, first_source, PREVIEW_RECIPE, "small")
        .await
        .expect("requeue missing rendition");
    assert_eq!(released, Some(output_blob));
    let queued: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM preview_jobs WHERE status = 'queued'")
            .fetch_one(&pool)
            .await
            .expect("queued jobs");
    assert_eq!(queued, 2);
    let renditions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM preview_renditions")
        .fetch_one(&pool)
        .await
        .expect("renditions");
    assert_eq!(renditions, 0);
}
