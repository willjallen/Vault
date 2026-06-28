use std::path::PathBuf;

use sha2::{Digest, Sha256};
use vault_server::storage::{
    LocalBlobStorage, StorageError, is_multipart_part_key, multipart_manifest_key_for_hash,
    multipart_part_key_for_hash, object_key_for_hash,
};

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

    assert_eq!(range, b"world");
    assert!(matches!(invalid, StorageError::InvalidRange));
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

    let manifest_key = multipart_manifest_key_for_hash("objects", "sha256", &digest);
    let first_part_key = multipart_part_key_for_hash("objects", "sha256", &digest, 1);
    let second_part_key = multipart_part_key_for_hash("objects", "sha256", &digest, 2);
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
    assert_eq!(
        storage.list_object_keys().await.expect("keys"),
        [manifest_key],
    );
    assert!(is_multipart_part_key(&first_part_key));

    storage
        .delete_object(&blob.object_key)
        .await
        .expect("delete manifest");

    assert_eq!(
        storage.list_object_keys().await.expect("keys after delete"),
        Vec::<String>::new(),
    );
    assert!(!storage.root().join(first_part_key).exists());
    assert!(!storage.root().join(second_part_key).exists());
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
