use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use vault_server::storage::{
    BlobReadRange, LocalBlobStorage, LocalDurabilityHook, LocalDurabilityPoint, STORAGE_CHUNK_SIZE,
    StorageError, is_multipart_part_key, multipart_manifest_key_for_hash,
    multipart_part_key_for_hash, multipart_part_key_for_hash_layout, object_key_for_hash,
};

#[derive(Debug)]
struct RecordingDurability {
    events: Mutex<Vec<(LocalDurabilityPoint, PathBuf)>>,
    fail_at: Option<LocalDurabilityPoint>,
}

impl RecordingDurability {
    fn new(fail_at: Option<LocalDurabilityPoint>) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            fail_at,
        }
    }

    fn points(&self) -> Vec<LocalDurabilityPoint> {
        self.events
            .lock()
            .expect("durability events")
            .iter()
            .map(|(point, _)| *point)
            .collect()
    }

    fn clear(&self) {
        self.events.lock().expect("durability events").clear();
    }
}

impl LocalDurabilityHook for RecordingDurability {
    fn before_sync(
        &self,
        point: LocalDurabilityPoint,
        path: &std::path::Path,
    ) -> std::io::Result<()> {
        self.events
            .lock()
            .expect("durability events")
            .push((point, path.to_path_buf()));
        if self.fail_at == Some(point) {
            Err(std::io::Error::other("injected durability failure"))
        } else {
            Ok(())
        }
    }
}

fn test_storage(root: &std::path::Path) -> LocalBlobStorage {
    LocalBlobStorage::new(root, "objects")
}

fn sha256_hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

async fn multipart_sources(root: &std::path::Path) -> (Vec<PathBuf>, String) {
    tokio::fs::create_dir_all(root)
        .await
        .expect("source directory");
    let first = root.join("first.part");
    let second = root.join("second.part");
    tokio::fs::write(&first, b"abc").await.expect("first part");
    tokio::fs::write(&second, b"def")
        .await
        .expect("second part");
    (vec![first, second], sha256_hex(b"abcdef"))
}

async fn deterministic_multipart_part_keys(
    root: &std::path::Path,
    sources: &[PathBuf],
    digest: &str,
) -> Vec<String> {
    let probe = test_storage(root);
    let stored = probe
        .put_part_files(sources, Some(digest))
        .await
        .expect("probe multipart object");
    probe
        .read_multipart_manifest(&stored.object_key)
        .await
        .expect("probe manifest")
        .parts
        .into_iter()
        .map(|part| part.object_key)
        .collect()
}

#[tokio::test]
async fn put_bytes_is_content_addressed_and_deduped() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let storage = test_storage(&temp_dir.path().join("store"));
    let expected_digest = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    let expected_key = format!("objects/sha256/{expected_digest}");

    let first = storage.put_bytes(b"hello").await.expect("put first");
    let second = storage.put_bytes(b"hello").await.expect("put second");

    assert_eq!(first.digest, expected_digest);
    assert_eq!(first.object_key, expected_key);
    assert_eq!(second, first);
    assert_eq!(
        storage
            .read_bytes(&first.object_key)
            .await
            .expect("read back"),
        b"hello",
    );
    assert_eq!(
        storage.list_object_keys().await.expect("keys"),
        [expected_key],
    );
}

#[tokio::test]
async fn put_bytes_repairs_existing_digest_key_with_wrong_bytes() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let storage = test_storage(&temp_dir.path().join("store"));
    let content = b"correct bytes";
    let digest = sha256_hex(content);
    let object_key = object_key_for_hash("objects", "sha256", &digest);
    let object_path = storage.root().join(&object_key);
    tokio::fs::create_dir_all(object_path.parent().expect("object parent"))
        .await
        .expect("object parent");
    tokio::fs::write(&object_path, b"wrong bytes")
        .await
        .expect("corrupt object");

    let stored = storage.put_bytes(content).await.expect("put bytes");

    assert_eq!(stored.object_key, object_key);
    assert_eq!(
        storage
            .read_bytes(&stored.object_key)
            .await
            .expect("repaired bytes"),
        content,
    );
}

#[tokio::test]
async fn object_keys_reject_path_traversal() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let storage = test_storage(&temp_dir.path().join("store"));

    let error = storage
        .read_bytes("../vault.db")
        .await
        .expect_err("traversal rejected");

    assert!(matches!(error, StorageError::InvalidObjectKey));
}

#[tokio::test]
async fn range_reader_reads_exact_slice() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let storage = test_storage(temp_dir.path());
    let blob = storage.put_bytes(b"hello world").await.expect("put bytes");

    let range = storage
        .read_range(&blob.object_key, 6, 10)
        .await
        .expect("range");
    let invalid = storage
        .read_range(&blob.object_key, 7, 6)
        .await
        .expect_err("invalid range");
    let mut stream = storage
        .stream_range(
            &blob.object_key,
            BlobReadRange {
                expected_size: 11,
                offset: 6,
                length: 5,
            },
        )
        .await
        .expect("range stream");
    let streamed = stream
        .next()
        .await
        .expect("streamed chunk")
        .expect("streamed range");

    assert_eq!(range, b"world");
    assert_eq!(streamed, b"world"[..]);
    assert!(stream.next().await.is_none());
    assert!(matches!(invalid, StorageError::InvalidRange));
}

#[tokio::test]
async fn range_stream_rejects_object_size_drift_before_returning_headers() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let storage = test_storage(temp_dir.path());
    let blob = storage.put_bytes(b"hello").await.expect("put bytes");
    tokio::fs::write(storage.root().join(&blob.object_key), b"hello!")
        .await
        .expect("replace object with appended bytes");

    let result = storage
        .stream_range(
            &blob.object_key,
            BlobReadRange {
                expected_size: 5,
                offset: 0,
                length: 5,
            },
        )
        .await;

    assert!(matches!(result, Err(StorageError::ContentMismatch)));
}

#[tokio::test]
async fn verified_part_files_promote_to_manifest_without_listing_parts() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let part_dir = temp_dir.path().join("parts");
    tokio::fs::create_dir_all(&part_dir)
        .await
        .expect("part dir");
    let first_part = part_dir.join("1.part");
    let second_part = part_dir.join("2.part");
    tokio::fs::write(&first_part, b"abc").await.expect("part 1");
    tokio::fs::write(&second_part, b"defgh")
        .await
        .expect("part 2");
    let digest = sha256_hex(b"abcdefgh");
    let storage = test_storage(&temp_dir.path().join("store"));

    let blob = storage
        .put_part_files(
            &[PathBuf::from(&first_part), PathBuf::from(&second_part)],
            Some(&digest),
        )
        .await
        .expect("put manifest");
    let published_manifest = storage
        .read_multipart_manifest(&blob.object_key)
        .await
        .expect("published manifest");
    let published_part_paths = published_manifest
        .parts
        .iter()
        .map(|part| part.path.clone())
        .collect::<Vec<_>>();
    assert!(published_part_paths.iter().all(|path| path.is_file()));

    let manifest_key = multipart_manifest_key_for_hash("objects", "sha256", &digest);
    let first_part_key = multipart_part_key_for_hash("objects", "sha256", &digest, 1);
    assert_eq!(blob.object_key, manifest_key);
    assert_eq!(
        storage
            .read_bytes(&blob.object_key)
            .await
            .expect("read manifest"),
        b"abcdefgh",
    );
    assert_eq!(
        storage
            .read_range(&blob.object_key, 2, 5)
            .await
            .expect("manifest range"),
        b"cdef",
    );
    let mut stream = storage
        .stream_range(
            &blob.object_key,
            BlobReadRange {
                expected_size: 8,
                offset: 2,
                length: 4,
            },
        )
        .await
        .expect("multipart stream");
    let mut streamed = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("multipart stream chunk");
        assert!(chunk.len() <= STORAGE_CHUNK_SIZE);
        streamed.extend_from_slice(&chunk);
    }
    assert_eq!(streamed, b"cdef");
    assert_eq!(
        storage.list_object_keys().await.expect("keys"),
        [manifest_key],
    );
    assert!(is_multipart_part_key(&first_part_key));
    assert!(is_multipart_part_key(&multipart_part_key_for_hash_layout(
        "objects", "sha256", &digest, "layout", 1
    )));

    storage
        .delete_object(&blob.object_key)
        .await
        .expect("delete manifest");

    assert_eq!(
        storage.list_object_keys().await.expect("keys after delete"),
        Vec::<String>::new(),
    );
    assert!(published_part_paths.iter().all(|path| !path.exists()));
}

#[tokio::test]
async fn multipart_publication_rolls_back_parts_created_before_a_later_conflict() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (sources, digest) = multipart_sources(&temp_dir.path().join("sources")).await;
    let part_keys =
        deterministic_multipart_part_keys(&temp_dir.path().join("probe"), &sources, &digest).await;
    let storage = test_storage(&temp_dir.path().join("target"));
    let first_target = storage.root().join(&part_keys[0]);
    let second_target = storage.root().join(&part_keys[1]);
    tokio::fs::create_dir_all(&second_target)
        .await
        .expect("conflicting second target");

    let error = storage
        .put_part_files(&sources, Some(&digest))
        .await
        .expect_err("second part conflict");

    assert!(matches!(error, StorageError::ContentMismatch));
    assert!(!first_target.exists());
    assert!(second_target.is_dir());
    assert!(
        !storage
            .root()
            .join(multipart_manifest_key_for_hash(
                "objects", "sha256", &digest
            ))
            .exists()
    );
}

#[tokio::test]
async fn multipart_rollback_preserves_existing_and_replaced_part_targets() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (sources, digest) = multipart_sources(&temp_dir.path().join("sources")).await;
    let part_keys =
        deterministic_multipart_part_keys(&temp_dir.path().join("probe"), &sources, &digest).await;

    for (case, initial) in [
        ("existing", b"abc".as_slice()),
        ("replaced", b"xxx".as_slice()),
    ] {
        let storage = test_storage(&temp_dir.path().join(case));
        let first_target = storage.root().join(&part_keys[0]);
        let second_target = storage.root().join(&part_keys[1]);
        tokio::fs::create_dir_all(first_target.parent().expect("part parent"))
            .await
            .expect("part parent");
        tokio::fs::write(&first_target, initial)
            .await
            .expect("initial first target");
        tokio::fs::create_dir_all(&second_target)
            .await
            .expect("conflicting second target");

        storage
            .put_part_files(&sources, Some(&digest))
            .await
            .expect_err("second part conflict");

        assert_eq!(
            tokio::fs::read(&first_target)
                .await
                .expect("first target survives"),
            b"abc",
            "{case} target"
        );
    }
}

#[tokio::test]
async fn multipart_manifest_publication_failure_rolls_back_all_created_parts() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (sources, digest) = multipart_sources(&temp_dir.path().join("sources")).await;
    let part_keys =
        deterministic_multipart_part_keys(&temp_dir.path().join("probe"), &sources, &digest).await;
    let storage = test_storage(&temp_dir.path().join("target"));
    let manifest_path = storage.root().join(multipart_manifest_key_for_hash(
        "objects", "sha256", &digest,
    ));
    tokio::fs::create_dir_all(&manifest_path)
        .await
        .expect("conflicting manifest directory");

    storage
        .put_part_files(&sources, Some(&digest))
        .await
        .expect_err("manifest publication conflict");

    assert!(manifest_path.is_dir());
    assert!(
        part_keys
            .iter()
            .all(|part_key| !storage.root().join(part_key).exists())
    );
}

#[tokio::test]
async fn multipart_publication_persists_parts_before_the_manifest_commit_point() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (sources, digest) = multipart_sources(&temp_dir.path().join("sources")).await;
    let durability = Arc::new(RecordingDurability::new(None));
    let storage = LocalBlobStorage::new_with_durability_hook(
        temp_dir.path().join("store"),
        "objects",
        durability.clone(),
    );

    storage
        .put_part_files(&sources, Some(&digest))
        .await
        .expect("durable multipart publication");

    let points = durability.points();
    let manifest_file = points
        .iter()
        .position(|point| *point == LocalDurabilityPoint::MultipartManifestFile)
        .expect("manifest file barrier");
    let final_manifest_directory = points
        .iter()
        .rposition(|point| *point == LocalDurabilityPoint::MultipartManifestDirectory)
        .expect("manifest directory barrier");
    let final_staging_directory = points
        .iter()
        .rposition(|point| *point == LocalDurabilityPoint::StagingDirectory)
        .expect("staging directory barrier");
    assert!(points.contains(&LocalDurabilityPoint::MultipartPartFile));
    assert!(points.contains(&LocalDurabilityPoint::MultipartPartDirectory));
    assert!(points.iter().enumerate().all(|(index, point)| {
        !matches!(
            point,
            LocalDurabilityPoint::MultipartPartFile | LocalDurabilityPoint::MultipartPartDirectory
        ) || index < manifest_file
    }));
    assert!(manifest_file < final_manifest_directory);
    assert!(final_manifest_directory < final_staging_directory);

    durability.clear();
    storage
        .put_part_files(&sources, Some(&digest))
        .await
        .expect("durable deduplicated multipart publication");
    let deduplicated = durability.points();
    for required in [
        LocalDurabilityPoint::MultipartPartFile,
        LocalDurabilityPoint::MultipartPartDirectory,
        LocalDurabilityPoint::MultipartManifestFile,
        LocalDurabilityPoint::MultipartManifestDirectory,
    ] {
        assert!(deduplicated.contains(&required), "missing {required:?}");
    }
}

#[tokio::test]
async fn multipart_manifest_sync_failure_rolls_back_uncommitted_parts() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (sources, digest) = multipart_sources(&temp_dir.path().join("sources")).await;
    let part_keys =
        deterministic_multipart_part_keys(&temp_dir.path().join("probe"), &sources, &digest).await;
    let durability = Arc::new(RecordingDurability::new(Some(
        LocalDurabilityPoint::MultipartManifestFile,
    )));
    let storage = LocalBlobStorage::new_with_durability_hook(
        temp_dir.path().join("store"),
        "objects",
        durability,
    );

    let error = storage
        .put_part_files(&sources, Some(&digest))
        .await
        .expect_err("manifest sync failure");

    assert!(matches!(error, StorageError::Io(_)));
    assert!(
        !storage
            .root()
            .join(multipart_manifest_key_for_hash(
                "objects", "sha256", &digest
            ))
            .exists()
    );
    assert!(
        part_keys
            .iter()
            .all(|part_key| !storage.root().join(part_key).exists())
    );
}

#[tokio::test]
async fn object_sync_failure_is_reported_before_publication_succeeds() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let durability = Arc::new(RecordingDurability::new(Some(
        LocalDurabilityPoint::ObjectFile,
    )));
    let storage = LocalBlobStorage::new_with_durability_hook(
        temp_dir.path().join("store"),
        "objects",
        durability,
    );

    let error = storage
        .put_bytes(b"not durable")
        .await
        .expect_err("object sync failure");

    assert!(matches!(error, StorageError::Io(_)));
    assert_eq!(
        storage.list_object_keys().await.expect("object keys"),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn object_directory_sync_failure_is_reported_after_atomic_publication() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let durability = Arc::new(RecordingDurability::new(Some(
        LocalDurabilityPoint::ObjectDirectory,
    )));
    let storage = LocalBlobStorage::new_with_durability_hook(
        temp_dir.path().join("store"),
        "objects",
        durability,
    );
    let data = b"directory barrier";
    let digest = sha256_hex(data);

    let error = storage
        .put_bytes(data)
        .await
        .expect_err("directory sync failure");

    assert!(matches!(error, StorageError::Io(_)));
    assert_eq!(
        tokio::fs::read(
            storage
                .root()
                .join(object_key_for_hash("objects", "sha256", &digest))
        )
        .await
        .expect("atomically published object"),
        data,
    );
}

#[tokio::test]
async fn multipart_directory_sync_failure_never_reports_publication_success() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (sources, digest) = multipart_sources(&temp_dir.path().join("sources")).await;
    let durability = Arc::new(RecordingDurability::new(Some(
        LocalDurabilityPoint::MultipartManifestDirectory,
    )));
    let storage = LocalBlobStorage::new_with_durability_hook(
        temp_dir.path().join("store"),
        "objects",
        durability,
    );
    let manifest_key = multipart_manifest_key_for_hash("objects", "sha256", &digest);

    let error = storage
        .put_part_files(&sources, Some(&digest))
        .await
        .expect_err("manifest directory sync failure");

    assert!(matches!(error, StorageError::Io(_)));
    assert_eq!(
        storage
            .read_bytes(&manifest_key)
            .await
            .expect("valid but unacknowledged multipart object"),
        b"abcdef",
    );
}

#[tokio::test]
async fn put_file_sync_failure_preserves_the_source_for_retry() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let source = temp_dir.path().join("source.bin");
    tokio::fs::write(&source, b"retryable source")
        .await
        .expect("source");
    let digest = sha256_hex(b"retryable source");
    let durability = Arc::new(RecordingDurability::new(Some(
        LocalDurabilityPoint::ObjectFile,
    )));
    let storage = LocalBlobStorage::new_with_durability_hook(
        temp_dir.path().join("store"),
        "objects",
        durability,
    );

    let error = storage
        .put_file(&source, &digest, 16)
        .await
        .expect_err("file sync failure");

    assert!(matches!(error, StorageError::Io(_)));
    assert_eq!(
        tokio::fs::read(&source).await.expect("retry source"),
        b"retryable source"
    );
    assert_eq!(
        storage.list_object_keys().await.expect("object keys"),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn assembled_part_file_sync_failure_leaves_no_visible_object() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (sources, _) = multipart_sources(&temp_dir.path().join("sources")).await;
    let durability = Arc::new(RecordingDurability::new(Some(
        LocalDurabilityPoint::ObjectFile,
    )));
    let storage = LocalBlobStorage::new_with_durability_hook(
        temp_dir.path().join("store"),
        "objects",
        durability,
    );

    let error = storage
        .put_part_files(&sources, None)
        .await
        .expect_err("assembled file sync failure");

    assert!(matches!(error, StorageError::Io(_)));
    assert_eq!(
        storage.list_object_keys().await.expect("object keys"),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn assembled_part_staging_sync_failure_never_reports_publication_success() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (sources, digest) = multipart_sources(&temp_dir.path().join("sources")).await;
    let durability = Arc::new(RecordingDurability::new(Some(
        LocalDurabilityPoint::StagingDirectory,
    )));
    let storage = LocalBlobStorage::new_with_durability_hook(
        temp_dir.path().join("store"),
        "objects",
        durability,
    );

    let error = storage
        .put_part_files(&sources, None)
        .await
        .expect_err("staging directory sync failure");

    assert!(matches!(error, StorageError::Io(_)));
    assert_eq!(
        storage
            .read_bytes(&object_key_for_hash("objects", "sha256", &digest))
            .await
            .expect("valid but unacknowledged object"),
        b"abcdef",
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One ordered concurrency scenario must retain both live streams.
async fn multipart_stream_lease_blocks_only_its_own_object_deletion() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let part_dir = temp_dir.path().join("parts");
    tokio::fs::create_dir_all(&part_dir)
        .await
        .expect("part directory");
    let storage = test_storage(&temp_dir.path().join("store"));

    let mut stored = Vec::new();
    for (name, first, second) in [
        ("a", b"abc".as_slice(), b"def".as_slice()),
        ("b", b"ghi".as_slice(), b"jkl".as_slice()),
    ] {
        let first_path = part_dir.join(format!("{name}-1.part"));
        let second_path = part_dir.join(format!("{name}-2.part"));
        tokio::fs::write(&first_path, first)
            .await
            .expect("first part");
        tokio::fs::write(&second_path, second)
            .await
            .expect("second part");
        let content = [first, second].concat();
        stored.push(
            storage
                .put_part_files(&[first_path, second_path], Some(&sha256_hex(&content)))
                .await
                .expect("multipart object"),
        );
    }
    let first_part_paths = storage
        .read_multipart_manifest(&stored[0].object_key)
        .await
        .expect("first manifest")
        .parts
        .into_iter()
        .map(|part| part.path)
        .collect::<Vec<_>>();

    let mut first_stream = storage
        .stream_range(
            &stored[0].object_key,
            BlobReadRange {
                expected_size: 6,
                offset: 0,
                length: 6,
            },
        )
        .await
        .expect("first stream");
    assert_eq!(
        first_stream
            .next()
            .await
            .expect("first object chunk")
            .expect("first object bytes"),
        b"abc"[..]
    );
    let deleting_first = tokio::spawn({
        let storage = storage.clone();
        let object_key = stored[0].object_key.clone();
        async move { storage.delete_object(&object_key).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match storage
                .stream_range(
                    &stored[0].object_key,
                    BlobReadRange {
                        expected_size: 6,
                        offset: 0,
                        length: 6,
                    },
                )
                .await
            {
                Err(StorageError::Busy) => break,
                Ok(stream) => drop(stream),
                Err(error) => panic!("unexpected same-object stream error: {error}"),
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delete writer became observable");
    assert!(!deleting_first.is_finished());

    let mut second_stream = storage
        .stream_range(
            &stored[1].object_key,
            BlobReadRange {
                expected_size: 6,
                offset: 0,
                length: 6,
            },
        )
        .await
        .expect("unrelated stream is not head-of-line blocked");
    let mut second_bytes = Vec::new();
    while let Some(chunk) = second_stream.next().await {
        second_bytes.extend_from_slice(&chunk.expect("second object bytes"));
    }
    assert_eq!(second_bytes, b"ghijkl");
    storage
        .delete_object(&stored[1].object_key)
        .await
        .expect("unrelated delete");

    let mut first_bytes = b"abc".to_vec();
    while let Some(chunk) = first_stream.next().await {
        first_bytes.extend_from_slice(&chunk.expect("remaining first bytes"));
    }
    assert_eq!(first_bytes, b"abcdef");
    deleting_first
        .await
        .expect("delete task")
        .expect("deferred delete");
    assert!(!storage.root().join(&stored[0].object_key).exists());
    assert!(first_part_paths.iter().all(|path| !path.exists()));
}

#[tokio::test]
async fn multipart_delete_rejects_manifest_that_points_at_another_blobs_parts() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let source_dir = temp_dir.path().join("parts");
    tokio::fs::create_dir_all(&source_dir)
        .await
        .expect("source directory");
    let first_source = source_dir.join("first.part");
    let second_source = source_dir.join("second.part");
    tokio::fs::write(&first_source, b"first object")
        .await
        .expect("first source");
    tokio::fs::write(&second_source, b"second object")
        .await
        .expect("second source");
    let first_digest = sha256_hex(b"first object");
    let second_digest = sha256_hex(b"second object");
    let storage = test_storage(&temp_dir.path().join("store"));
    let first = storage
        .put_part_files(&[first_source], Some(&first_digest))
        .await
        .expect("first multipart object");
    let second = storage
        .put_part_files(&[second_source], Some(&second_digest))
        .await
        .expect("second multipart object");
    let second_manifest = storage
        .read_multipart_manifest(&second.object_key)
        .await
        .expect("second manifest");
    let protected_part = second_manifest.parts[0].clone();
    let first_manifest_path = storage.root().join(&first.object_key);
    let mut tampered: serde_json::Value = serde_json::from_slice(
        &tokio::fs::read(&first_manifest_path)
            .await
            .expect("first manifest bytes"),
    )
    .expect("first manifest json");
    tampered["parts"][0]["object_key"] = protected_part.object_key.clone().into();
    tokio::fs::write(
        &first_manifest_path,
        serde_json::to_vec(&tampered).expect("tampered manifest json"),
    )
    .await
    .expect("tamper first manifest");

    let read_result = storage
        .stream_range(
            &first.object_key,
            BlobReadRange {
                expected_size: first.size_bytes,
                offset: 0,
                length: first.size_bytes,
            },
        )
        .await;
    storage
        .delete_object(&first.object_key)
        .await
        .expect("invalid manifest is removed without following its part reference");

    assert!(matches!(
        read_result,
        Err(StorageError::InvalidMultipartManifest)
    ));
    assert!(!first_manifest_path.exists());
    assert!(protected_part.path.is_file());
    assert_eq!(
        storage
            .read_bytes(&second.object_key)
            .await
            .expect("second object remains readable"),
        b"second object",
    );
}

#[tokio::test]
async fn verified_part_files_repair_corrupt_existing_manifest_parts() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let part_dir = temp_dir.path().join("parts");
    tokio::fs::create_dir_all(&part_dir)
        .await
        .expect("part dir");
    let first_part = part_dir.join("1.part");
    let second_part = part_dir.join("2.part");
    tokio::fs::write(&first_part, b"abc").await.expect("part 1");
    tokio::fs::write(&second_part, b"def")
        .await
        .expect("part 2");
    let digest = sha256_hex(b"abcdef");
    let storage = test_storage(&temp_dir.path().join("store"));
    let blob = storage
        .put_part_files(
            &[PathBuf::from(&first_part), PathBuf::from(&second_part)],
            Some(&digest),
        )
        .await
        .expect("put manifest");
    let manifest = storage
        .read_multipart_manifest(&blob.object_key)
        .await
        .expect("manifest");
    tokio::fs::write(&manifest.parts[0].path, b"xyz")
        .await
        .expect("corrupt part");
    let repair_first_part = part_dir.join("repair-1.part");
    let repair_second_part = part_dir.join("repair-2.part");
    tokio::fs::write(&repair_first_part, b"abc")
        .await
        .expect("repair part 1");
    tokio::fs::write(&repair_second_part, b"def")
        .await
        .expect("repair part 2");

    let repaired = storage
        .put_part_files(&[repair_first_part, repair_second_part], Some(&digest))
        .await
        .expect("repair manifest");

    assert_eq!(repaired.object_key, blob.object_key);
    assert_eq!(
        storage
            .read_bytes(&blob.object_key)
            .await
            .expect("repaired multipart"),
        b"abcdef",
    );
}

#[tokio::test]
async fn verified_part_files_with_different_chunking_use_distinct_part_layouts() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let part_dir = temp_dir.path().join("parts");
    tokio::fs::create_dir_all(&part_dir)
        .await
        .expect("part dir");
    let old_first = part_dir.join("old-1.part");
    let old_second = part_dir.join("old-2.part");
    let new_first = part_dir.join("new-1.part");
    let new_second = part_dir.join("new-2.part");
    tokio::fs::write(&old_first, b"abc")
        .await
        .expect("old part 1");
    tokio::fs::write(&old_second, b"defgh")
        .await
        .expect("old part 2");
    tokio::fs::write(&new_first, b"abcd")
        .await
        .expect("new part 1");
    tokio::fs::write(&new_second, b"efgh")
        .await
        .expect("new part 2");
    let digest = sha256_hex(b"abcdefgh");
    let storage = test_storage(&temp_dir.path().join("store"));
    let first = storage
        .put_part_files(
            &[PathBuf::from(&old_first), PathBuf::from(&old_second)],
            Some(&digest),
        )
        .await
        .expect("first manifest");
    let first_manifest = storage
        .read_multipart_manifest(&first.object_key)
        .await
        .expect("first manifest payload");
    tokio::fs::remove_file(storage.root().join(&first.object_key))
        .await
        .expect("remove manifest only");

    let second = storage
        .put_part_files(&[new_first, new_second], Some(&digest))
        .await
        .expect("second manifest");
    let second_manifest = storage
        .read_multipart_manifest(&second.object_key)
        .await
        .expect("second manifest payload");

    assert_eq!(first.object_key, second.object_key);
    assert_ne!(
        first_manifest.parts[0].object_key,
        second_manifest.parts[0].object_key,
    );
    assert_eq!(
        storage
            .read_bytes(&second.object_key)
            .await
            .expect("second multipart"),
        b"abcdefgh",
    );
}

#[tokio::test]
async fn unverified_part_files_are_assembled_into_content_addressed_blob() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let part_dir = temp_dir.path().join("parts");
    tokio::fs::create_dir_all(&part_dir)
        .await
        .expect("part dir");
    let first_part = part_dir.join("1.part");
    let second_part = part_dir.join("2.part");
    tokio::fs::write(&first_part, b"chunk")
        .await
        .expect("part 1");
    tokio::fs::write(&second_part, b"ed").await.expect("part 2");
    let digest = sha256_hex(b"chunked");
    let storage = test_storage(&temp_dir.path().join("store"));

    let blob = storage
        .put_part_files(
            &[PathBuf::from(&first_part), PathBuf::from(&second_part)],
            None,
        )
        .await
        .expect("put assembled");

    assert_eq!(blob.digest, digest);
    assert_eq!(
        blob.object_key,
        object_key_for_hash("objects", "sha256", &digest)
    );
    assert_eq!(
        storage
            .read_bytes(&blob.object_key)
            .await
            .expect("read assembled"),
        b"chunked",
    );
    assert_eq!(
        storage.list_object_keys().await.expect("keys"),
        [blob.object_key],
    );
}

#[tokio::test]
async fn multipart_inventory_preserves_legacy_storage_prefix_spelling() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (sources, digest) = multipart_sources(&temp_dir.path().join("sources")).await;
    let root = temp_dir.path().join("store");
    let legacy_prefix = " /team//./objects\\archive/ ";
    let storage = LocalBlobStorage::new(&root, legacy_prefix);

    let stored = storage
        .put_part_files(&sources, Some(&digest))
        .await
        .expect("multipart object");
    let mut expected_part_keys = storage
        .read_multipart_manifest(&stored.object_key)
        .await
        .expect("multipart manifest")
        .parts
        .into_iter()
        .map(|part| part.object_key)
        .collect::<Vec<_>>();
    let restarted_storage = LocalBlobStorage::new(&root, legacy_prefix);
    let (parts, scan_complete) = restarted_storage
        .scan_multipart_part_objects(1_024)
        .await
        .expect("multipart inventory");
    let mut actual_part_keys = parts
        .into_iter()
        .map(|part| part.object_key)
        .collect::<Vec<_>>();
    expected_part_keys.sort();
    actual_part_keys.sort();

    assert_eq!(restarted_storage.prefix(), "team//./objects/archive");
    assert!(scan_complete);
    assert_eq!(actual_part_keys, expected_part_keys);
    assert_eq!(
        restarted_storage
            .read_bytes(&stored.object_key)
            .await
            .expect("legacy-prefixed object remains readable"),
        b"abcdef",
    );
}

#[tokio::test]
async fn multipart_inventory_is_bounded_strict_and_symlink_safe() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let storage = test_storage(&temp_dir.path().join("store"));
    let digest = sha256_hex(b"inventory");
    let layout = sha256_hex(b"layout");
    let parts_root = storage
        .root()
        .join("objects/multipart/sha256")
        .join(&digest)
        .join("parts");
    let legacy = parts_root.join("00000001.part");
    let layout_part = parts_root.join(&layout).join("00000002.part");
    tokio::fs::create_dir_all(layout_part.parent().expect("layout parent"))
        .await
        .expect("multipart directories");
    tokio::fs::write(&legacy, b"a").await.expect("legacy part");
    tokio::fs::write(&layout_part, b"b")
        .await
        .expect("layout part");
    for invalid in ["00000000.part", "00000003.part.extra", "éééé.part"] {
        tokio::fs::write(parts_root.join(invalid), b"unsafe")
            .await
            .expect("invalid inventory entry");
    }

    #[cfg(unix)]
    let outside = {
        let outside = temp_dir.path().join("outside");
        tokio::fs::create_dir_all(&outside)
            .await
            .expect("outside directory");
        tokio::fs::write(outside.join("00000003.part"), b"sentinel")
            .await
            .expect("outside sentinel");
        std::os::unix::fs::symlink(&outside, parts_root.join(sha256_hex(b"symlink-layout")))
            .expect("layout symlink");
        outside
    };

    let mut inventoried = Vec::new();
    for _ in 0..32 {
        let (batch, complete) = storage
            .scan_multipart_part_objects(1)
            .await
            .expect("bounded inventory");
        assert!(batch.len() <= 1);
        inventoried.extend(batch.into_iter().map(|part| part.path));
        if complete {
            break;
        }
    }
    inventoried.sort();
    let mut expected = vec![legacy, layout_part];
    expected.sort();
    assert_eq!(inventoried, expected);
    #[cfg(unix)]
    assert_eq!(
        tokio::fs::read(outside.join("00000003.part"))
            .await
            .expect("outside sentinel survives"),
        b"sentinel"
    );
}
