use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client as S3Client, Config as S3ClientConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

use crate::config::Config;

pub const DEFAULT_STORAGE_PREFIX: &str = "objects";
pub const LOCAL_MULTIPART_FORMAT: &str = "vault.local.multipart.v1";
pub const STORAGE_CHUNK_SIZE: usize = 1024 * 1024;

pub type SharedBlobStorage = Arc<dyn BlobStorageBackend>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredBlob {
    pub hash_algo: String,
    pub digest: String,
    pub size_bytes: u64,
    pub backend: String,
    pub bucket: String,
    pub object_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMultipartPart {
    pub object_key: String,
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMultipartManifest {
    pub hash_algo: String,
    pub digest: String,
    pub size_bytes: u64,
    pub parts: Vec<LocalMultipartPart>,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("invalid object key")]
    InvalidObjectKey,
    #[error("invalid byte range")]
    InvalidRange,
    #[error("blob missing from storage")]
    NotFound,
    #[error("source file size changed before storage write")]
    SourceSizeChanged,
    #[error("upload checksum mismatch")]
    ChecksumMismatch,
    #[error("blob content does not match metadata")]
    ContentMismatch,
    #[error("multipart object part already exists with a different size")]
    ConflictingMultipartPart,
    #[error("multipart manifest is invalid")]
    InvalidMultipartManifest,
    #[error("multipart manifest is unreadable")]
    UnreadableMultipartManifest,
    #[error("storage path has no valid file name")]
    InvalidStoragePath,
    #[error("{0}")]
    Configuration(String),
    #[error("storage backend cannot serve this blob location")]
    BackendMismatch,
    #[error("{0}")]
    UnsupportedOperation(String),
    #[error("remote storage operation failed")]
    Remote(String),
    #[error("storage IO failed")]
    Io(#[from] std::io::Error),
    #[error("storage JSON failed")]
    Json(#[from] serde_json::Error),
}

#[async_trait]
pub trait BlobStorageBackend: std::fmt::Debug + Send + Sync {
    fn name(&self) -> &str;

    fn bucket(&self) -> &str;

    async fn ensure(&self) -> Result<(), StorageError>;

    async fn put_bytes(&self, data: &[u8]) -> Result<StoredBlob, StorageError>;

    async fn put_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError>;

    async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError>;

    async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError>;

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError>;

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError>;

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError>;

    async fn read_location_bytes(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
    ) -> Result<Vec<u8>, StorageError> {
        self.require_location(backend, bucket)?;
        self.read_bytes(object_key).await
    }

    async fn read_location_range(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        self.require_location(backend, bucket)?;
        self.read_range(object_key, start, end).await
    }

    async fn delete_location(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
    ) -> Result<(), StorageError> {
        self.require_location(backend, bucket)?;
        self.delete_object(object_key).await
    }

    fn require_location(&self, backend: &str, bucket: &str) -> Result<(), StorageError> {
        if backend == self.name() && (bucket.is_empty() || bucket == self.bucket()) {
            Ok(())
        } else {
            Err(StorageError::BackendMismatch)
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocalBlobStorage {
    root: Arc<PathBuf>,
    prefix: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct S3StorageSettings {
    pub name: String,
    pub bucket: String,
    pub region: String,
    pub endpoint_url: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub prefix: String,
}

#[derive(Debug, Clone)]
pub struct S3CompatibleBlobStorage {
    name: Arc<str>,
    bucket: Arc<str>,
    prefix: Arc<str>,
    client: S3Client,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestPayload {
    format: String,
    hash_algo: String,
    digest: String,
    size_bytes: u64,
    parts: Vec<ManifestPartPayload>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManifestPartPayload {
    object_key: String,
    size_bytes: u64,
}

enum MultipartManifestState {
    Existing(u64),
    Missing,
    Replace,
}

impl LocalBlobStorage {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, prefix: impl AsRef<str>) -> Self {
        Self {
            root: Arc::new(root.into()),
            prefix: Arc::from(normalize_storage_prefix(prefix.as_ref())),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.as_ref()
    }

    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub async fn ensure(&self) -> Result<(), StorageError> {
        fs::create_dir_all(self.root()).await?;
        Ok(())
    }

    #[must_use]
    pub fn object_key_for_hash(&self, hash_algo: &str, digest: &str) -> String {
        object_key_for_hash(&self.prefix, hash_algo, digest)
    }

    #[must_use]
    pub fn multipart_manifest_key_for_hash(&self, hash_algo: &str, digest: &str) -> String {
        multipart_manifest_key_for_hash(&self.prefix, hash_algo, digest)
    }

    #[must_use]
    pub fn multipart_part_key_for_hash(
        &self,
        hash_algo: &str,
        digest: &str,
        part_number: usize,
    ) -> String {
        multipart_part_key_for_hash(&self.prefix, hash_algo, digest, part_number)
    }

    pub async fn put_bytes(&self, data: &[u8]) -> Result<StoredBlob, StorageError> {
        self.ensure().await?;
        let digest = sha256_hex(data);
        let object_key = self.object_key_for_hash("sha256", &digest);
        let target = self.object_path(&object_key)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        if !file_matches_digest(&target, &digest, data.len() as u64).await? {
            let temp_path = temp_sibling_path(&target)?;
            let write_result = async {
                fs::write(&temp_path, data).await?;
                rename_or_replace(&temp_path, &target).await
            }
            .await;
            if write_result.is_err() {
                let _ = fs::remove_file(&temp_path).await;
            }
            write_result?;
        }
        Ok(stored_blob(digest, data.len() as u64, object_key))
    }

    pub async fn put_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        self.ensure().await?;
        let source_size = fs::metadata(source_path).await?.len();
        if source_size != size_bytes {
            return Err(StorageError::SourceSizeChanged);
        }
        let normalized_digest = digest.to_ascii_lowercase();
        let (source_digest, hashed_size) = hash_file(source_path).await?;
        if hashed_size != size_bytes || source_digest != normalized_digest {
            return Err(StorageError::ChecksumMismatch);
        }
        let object_key = self.object_key_for_hash("sha256", &normalized_digest);
        let target = self.object_path(&object_key)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        if !file_matches_digest(&target, &normalized_digest, size_bytes).await? {
            let temp_path = temp_sibling_path(&target)?;
            let write_result = async {
                if fs::rename(source_path, &temp_path).await.is_err() {
                    fs::copy(source_path, &temp_path).await?;
                    let _ = fs::remove_file(source_path).await;
                }
                rename_or_replace(&temp_path, &target).await
            }
            .await;
            if write_result.is_err() {
                let _ = fs::remove_file(&temp_path).await;
            }
            write_result?;
        }
        Ok(stored_blob(normalized_digest, size_bytes, object_key))
    }

    pub async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        if let Some(expected_digest) = expected_digest {
            return self
                .put_verified_part_manifest(part_paths, &expected_digest.to_ascii_lowercase())
                .await;
        }

        self.ensure().await?;
        let staging_dir = self.root().join(".vault-staging");
        fs::create_dir_all(&staging_dir).await?;
        let temp_path = staging_dir.join(format!("upload-{}.tmp", Uuid::new_v4().simple()));
        let mut hasher = Sha256::new();
        let mut size_bytes = 0_u64;
        let write_result = async {
            let mut output = fs::File::create_new(&temp_path).await?;
            for part_path in part_paths {
                let mut source = fs::File::open(part_path).await?;
                let mut buffer = vec![0_u8; STORAGE_CHUNK_SIZE];
                loop {
                    let read = source.read(&mut buffer).await?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                    size_bytes += read as u64;
                    tokio::io::AsyncWriteExt::write_all(&mut output, &buffer[..read]).await?;
                }
            }
            tokio::io::AsyncWriteExt::flush(&mut output).await?;
            Ok::<(), StorageError>(())
        }
        .await;
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path).await;
        }
        write_result?;

        let digest = lower_hex(&hasher.finalize());
        let object_key = self.object_key_for_hash("sha256", &digest);
        let target = self.object_path(&object_key)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        if file_matches_digest(&target, &digest, size_bytes).await? {
            fs::remove_file(&temp_path).await?;
        } else {
            rename_or_replace(&temp_path, &target).await?;
        }
        Ok(stored_blob(digest, size_bytes, object_key))
    }

    pub async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        if is_multipart_manifest_key(object_key) {
            let manifest = self.read_multipart_manifest(object_key).await?;
            if manifest.size_bytes == 0 {
                return Ok(Vec::new());
            }
            return self
                .read_multipart_range(&manifest, 0, manifest.size_bytes - 1)
                .await;
        }
        let target = self.object_path(object_key)?;
        match fs::read(target).await {
            Ok(data) => Ok(data),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound)
            }
            Err(error) => Err(StorageError::Io(error)),
        }
    }

    pub async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        if end < start {
            return Err(StorageError::InvalidRange);
        }
        if is_multipart_manifest_key(object_key) {
            let manifest = self.read_multipart_manifest(object_key).await?;
            if end >= manifest.size_bytes {
                return Err(StorageError::InvalidRange);
            }
            return self.read_multipart_range(&manifest, start, end).await;
        }

        let target = self.object_path(object_key)?;
        let mut source = match fs::File::open(target).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound);
            }
            Err(error) => return Err(StorageError::Io(error)),
        };
        source.seek(std::io::SeekFrom::Start(start)).await?;
        let requested = end - start + 1;
        let capacity = usize::try_from(requested).map_err(|_| StorageError::InvalidRange)?;
        let mut reader = source.take(requested);
        let mut data = Vec::with_capacity(capacity);
        reader.read_to_end(&mut data).await?;
        Ok(data)
    }

    pub async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.ensure().await?;
        let mut keys = Vec::new();
        collect_object_keys(self.root(), self.root(), &mut keys)?;
        keys.sort();
        Ok(keys)
    }

    pub async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        let manifest = if is_multipart_manifest_key(object_key) {
            match self.read_multipart_manifest(object_key).await {
                Ok(manifest) => Some(manifest),
                Err(StorageError::NotFound) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let target = self.object_path(object_key)?;
        if fs::metadata(&target).await.is_ok() {
            fs::remove_file(&target).await?;
        }
        if let Some(manifest) = manifest {
            for part in manifest.parts {
                let _ = fs::remove_file(part.path).await;
            }
        }
        Ok(())
    }

    pub async fn read_multipart_manifest(
        &self,
        object_key: &str,
    ) -> Result<LocalMultipartManifest, StorageError> {
        let manifest_path = self.object_path(object_key)?;
        let manifest_bytes = match fs::read(&manifest_path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound);
            }
            Err(error) => return Err(StorageError::Io(error)),
        };
        let payload: ManifestPayload = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| StorageError::UnreadableMultipartManifest)?;
        if payload.format != LOCAL_MULTIPART_FORMAT
            || payload.hash_algo != "sha256"
            || payload.digest.is_empty()
        {
            return Err(StorageError::InvalidMultipartManifest);
        }
        let mut parts = Vec::with_capacity(payload.parts.len());
        let mut total_size = 0_u64;
        for raw_part in payload.parts {
            let part_path = self.object_path(&raw_part.object_key)?;
            let metadata = match fs::metadata(&part_path).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(StorageError::NotFound);
                }
                Err(error) => return Err(StorageError::Io(error)),
            };
            if !metadata.is_file() {
                return Err(StorageError::NotFound);
            }
            if metadata.len() != raw_part.size_bytes {
                return Err(StorageError::InvalidMultipartManifest);
            }
            total_size += raw_part.size_bytes;
            parts.push(LocalMultipartPart {
                object_key: raw_part.object_key,
                path: part_path,
                size_bytes: raw_part.size_bytes,
            });
        }
        if total_size != payload.size_bytes {
            return Err(StorageError::InvalidMultipartManifest);
        }
        Ok(LocalMultipartManifest {
            hash_algo: payload.hash_algo,
            digest: payload.digest,
            size_bytes: payload.size_bytes,
            parts,
        })
    }

    async fn put_verified_part_manifest(
        &self,
        part_paths: &[PathBuf],
        digest: &str,
    ) -> Result<StoredBlob, StorageError> {
        self.ensure().await?;
        let manifest_key = self.multipart_manifest_key_for_hash("sha256", digest);
        let manifest_state = self
            .verified_multipart_manifest_state(&manifest_key, digest)
            .await?;
        if let MultipartManifestState::Existing(size_bytes) = manifest_state {
            return Ok(stored_blob(digest.to_string(), size_bytes, manifest_key));
        }

        let (size_bytes, part_entries) = self
            .publish_multipart_part_entries(part_paths, digest)
            .await?;

        if matches!(manifest_state, MultipartManifestState::Replace) {
            self.write_multipart_manifest(
                &manifest_key,
                digest,
                size_bytes,
                part_entries.clone(),
                true,
            )
            .await?;
        } else {
            self.publish_multipart_manifest(
                &manifest_key,
                digest,
                size_bytes,
                part_entries.clone(),
            )
            .await?;
        }
        let manifest = self
            .read_and_verify_multipart_manifest(&manifest_key, digest)
            .await?;
        if manifest.size_bytes != size_bytes {
            return Err(StorageError::ContentMismatch);
        }
        Ok(stored_blob(digest.to_string(), size_bytes, manifest_key))
    }

    async fn publish_multipart_part_entries(
        &self,
        part_paths: &[PathBuf],
        digest: &str,
    ) -> Result<(u64, Vec<ManifestPartPayload>), StorageError> {
        let mut part_sizes = Vec::with_capacity(part_paths.len());
        let mut size_bytes = 0_u64;
        for part_path in part_paths {
            let part_size = fs::metadata(part_path).await?.len();
            part_sizes.push(part_size);
            size_bytes += part_size;
        }
        let layout_id = multipart_layout_id(&part_sizes);
        let mut part_entries = Vec::with_capacity(part_paths.len());
        for (index, (part_path, part_size)) in part_paths.iter().zip(part_sizes.iter()).enumerate()
        {
            let part_key = multipart_part_key_for_hash_layout(
                &self.prefix,
                "sha256",
                digest,
                &layout_id,
                index + 1,
            );
            let target_path = self.object_path(&part_key)?;
            publish_part_file(part_path, &target_path, *part_size).await?;
            part_entries.push(ManifestPartPayload {
                object_key: part_key,
                size_bytes: *part_size,
            });
        }
        Ok((size_bytes, part_entries))
    }

    async fn publish_multipart_manifest(
        &self,
        manifest_key: &str,
        digest: &str,
        size_bytes: u64,
        part_entries: Vec<ManifestPartPayload>,
    ) -> Result<(), StorageError> {
        if self
            .write_multipart_manifest(
                manifest_key,
                digest,
                size_bytes,
                part_entries.clone(),
                false,
            )
            .await?
        {
            return Ok(());
        }
        match self
            .verified_multipart_manifest_state(manifest_key, digest)
            .await?
        {
            MultipartManifestState::Existing(_) => Ok(()),
            MultipartManifestState::Missing | MultipartManifestState::Replace => {
                self.write_multipart_manifest(manifest_key, digest, size_bytes, part_entries, true)
                    .await?;
                Ok(())
            }
        }
    }

    async fn write_multipart_manifest(
        &self,
        manifest_key: &str,
        digest: &str,
        size_bytes: u64,
        parts: Vec<ManifestPartPayload>,
        replace_existing: bool,
    ) -> Result<bool, StorageError> {
        let manifest_path = self.object_path(manifest_key)?;
        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let staging_dir = self.root().join(".vault-staging");
        fs::create_dir_all(&staging_dir).await?;
        let temp_path = staging_dir.join(format!("manifest-{}.tmp", Uuid::new_v4().simple()));
        let payload = ManifestPayload {
            format: LOCAL_MULTIPART_FORMAT.to_string(),
            hash_algo: "sha256".to_string(),
            digest: digest.to_string(),
            size_bytes,
            parts,
        };
        let write_result = async {
            let mut manifest_bytes = serde_json::to_vec(&payload)?;
            manifest_bytes.push(b'\n');
            fs::write(&temp_path, manifest_bytes).await?;
            if replace_existing {
                rename_or_replace(&temp_path, &manifest_path).await?;
                Ok(true)
            } else {
                match fs::hard_link(&temp_path, &manifest_path).await {
                    Ok(()) => {
                        let _ = fs::remove_file(&temp_path).await;
                        Ok(true)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let _ = fs::remove_file(&temp_path).await;
                        Ok(false)
                    }
                    Err(error) => Err(StorageError::Io(error)),
                }
            }
        }
        .await;
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path).await;
        }
        write_result
    }

    async fn verified_multipart_manifest_state(
        &self,
        object_key: &str,
        expected_digest: &str,
    ) -> Result<MultipartManifestState, StorageError> {
        match self
            .read_and_verify_multipart_manifest(object_key, expected_digest)
            .await
        {
            Ok(existing) => Ok(MultipartManifestState::Existing(existing.size_bytes)),
            Err(StorageError::NotFound) => Ok(MultipartManifestState::Missing),
            Err(
                StorageError::ContentMismatch
                | StorageError::InvalidMultipartManifest
                | StorageError::UnreadableMultipartManifest,
            ) => Ok(MultipartManifestState::Replace),
            Err(error) => Err(error),
        }
    }

    async fn read_and_verify_multipart_manifest(
        &self,
        object_key: &str,
        expected_digest: &str,
    ) -> Result<LocalMultipartManifest, StorageError> {
        let manifest = self.read_multipart_manifest(object_key).await?;
        if manifest.digest != expected_digest {
            return Err(StorageError::ContentMismatch);
        }
        verify_multipart_manifest_digest(&manifest).await?;
        Ok(manifest)
    }

    async fn read_multipart_range(
        &self,
        manifest: &LocalMultipartManifest,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        if end < start || end >= manifest.size_bytes {
            return Err(StorageError::InvalidRange);
        }
        let requested = end - start + 1;
        let capacity = usize::try_from(requested).map_err(|_| StorageError::InvalidRange)?;
        let mut remaining = requested;
        let mut skipped = 0_u64;
        let mut data = Vec::with_capacity(capacity);

        for part in &manifest.parts {
            let part_start = skipped;
            let part_end = skipped + part.size_bytes;
            skipped = part_end;
            if start >= part_end {
                continue;
            }
            if remaining == 0 {
                break;
            }
            let offset = start.saturating_sub(part_start);
            let available = part.size_bytes - offset;
            let to_read = remaining.min(available);
            let mut source = fs::File::open(&part.path).await?;
            source.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut reader = source.take(to_read);
            reader.read_to_end(&mut data).await?;
            remaining -= to_read;
        }

        if remaining == 0 {
            Ok(data)
        } else {
            Err(StorageError::InvalidMultipartManifest)
        }
    }

    fn object_path(&self, object_key: &str) -> Result<PathBuf, StorageError> {
        let cleaned = object_key.trim().trim_start_matches('/').replace('\\', "/");
        if cleaned.is_empty() {
            return Err(StorageError::InvalidObjectKey);
        }
        let mut target = self.root.as_ref().clone();
        for segment in cleaned.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." {
                return Err(StorageError::InvalidObjectKey);
            }
            target.push(segment);
        }
        Ok(target)
    }
}

#[async_trait]
impl BlobStorageBackend for LocalBlobStorage {
    fn name(&self) -> &'static str {
        "local"
    }

    fn bucket(&self) -> &'static str {
        ""
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        LocalBlobStorage::ensure(self).await
    }

    async fn put_bytes(&self, data: &[u8]) -> Result<StoredBlob, StorageError> {
        LocalBlobStorage::put_bytes(self, data).await
    }

    async fn put_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        LocalBlobStorage::put_file(self, source_path, digest, size_bytes).await
    }

    async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        LocalBlobStorage::put_part_files(self, part_paths, expected_digest).await
    }

    async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        LocalBlobStorage::read_bytes(self, object_key).await
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        LocalBlobStorage::read_range(self, object_key, start, end).await
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        LocalBlobStorage::list_object_keys(self).await
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        LocalBlobStorage::delete_object(self, object_key).await
    }
}

impl S3StorageSettings {
    #[must_use]
    pub fn s3_from_env(prefix: &str) -> Self {
        Self::s3_from_env_with(prefix, |name| std::env::var(name).ok())
    }

    #[must_use]
    pub fn s3_from_env_with<F>(prefix: &str, env_var: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        Self {
            name: "s3".to_string(),
            bucket: env_trimmed_from(&env_var, "VAULT_S3_BUCKET"),
            region: env_trimmed_or_from(&env_var, "VAULT_S3_REGION", "us-east-1"),
            endpoint_url: env_optional_from(&env_var, "VAULT_S3_ENDPOINT_URL"),
            access_key_id: env_optional_fallback_from(
                &env_var,
                "VAULT_S3_ACCESS_KEY_ID",
                "AWS_ACCESS_KEY_ID",
            ),
            secret_access_key: env_optional_fallback_from(
                &env_var,
                "VAULT_S3_SECRET_ACCESS_KEY",
                "AWS_SECRET_ACCESS_KEY",
            ),
            session_token: env_optional_fallback_from(
                &env_var,
                "VAULT_S3_SESSION_TOKEN",
                "AWS_SESSION_TOKEN",
            ),
            prefix: prefix.to_string(),
        }
    }

    #[must_use]
    pub fn r2_from_env(prefix: &str) -> Self {
        Self::r2_from_env_with(prefix, |name| std::env::var(name).ok())
    }

    #[must_use]
    pub fn r2_from_env_with<F>(prefix: &str, env_var: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let account_id = env_trimmed_from(&env_var, "VAULT_R2_ACCOUNT_ID");
        let endpoint_url = env_optional_from(&env_var, "VAULT_R2_ENDPOINT_URL").or_else(|| {
            if account_id.is_empty() {
                None
            } else {
                Some(format!("https://{account_id}.r2.cloudflarestorage.com"))
            }
        });
        Self {
            name: "r2".to_string(),
            bucket: env_trimmed_from(&env_var, "VAULT_R2_BUCKET"),
            region: "auto".to_string(),
            endpoint_url,
            access_key_id: env_optional_from(&env_var, "VAULT_R2_ACCESS_KEY_ID"),
            secret_access_key: env_optional_from(&env_var, "VAULT_R2_SECRET_ACCESS_KEY"),
            session_token: None,
            prefix: prefix.to_string(),
        }
    }
}

impl S3CompatibleBlobStorage {
    pub async fn from_settings(settings: S3StorageSettings) -> Result<Self, StorageError> {
        let name = settings.name.trim().to_ascii_lowercase();
        let bucket = settings.bucket.trim().to_string();
        if bucket.is_empty() {
            return Err(StorageError::Configuration(format!(
                "VAULT_{}_BUCKET is required for {name} storage",
                name.to_ascii_uppercase()
            )));
        }
        let region = settings.region.trim();
        let shared_config = aws_config::defaults(BehaviorVersion::latest()).region(Region::new(
            if region.is_empty() {
                "us-east-1".to_string()
            } else {
                region.to_string()
            },
        ));
        let shared_config = match (
            settings.access_key_id.as_deref(),
            settings.secret_access_key.as_deref(),
        ) {
            (Some(access_key_id), Some(secret_access_key))
                if !access_key_id.trim().is_empty() && !secret_access_key.trim().is_empty() =>
            {
                let credentials = Credentials::new(
                    access_key_id.trim().to_string(),
                    secret_access_key.trim().to_string(),
                    settings
                        .session_token
                        .as_deref()
                        .map(str::trim)
                        .filter(|token| !token.is_empty())
                        .map(ToOwned::to_owned),
                    None,
                    "vault",
                );
                shared_config.credentials_provider(SharedCredentialsProvider::new(credentials))
            }
            _ => shared_config,
        }
        .load()
        .await;
        let mut config_builder = S3ClientConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(
                shared_config
                    .region()
                    .cloned()
                    .unwrap_or_else(|| Region::new("us-east-1")),
            )
            .force_path_style(true);
        if let Some(credentials_provider) = shared_config.credentials_provider() {
            config_builder = config_builder.credentials_provider(credentials_provider.clone());
        }
        if let Some(endpoint_url) = settings.endpoint_url.as_deref() {
            let endpoint_url = endpoint_url.trim();
            if !endpoint_url.is_empty() {
                config_builder = config_builder.endpoint_url(endpoint_url);
            }
        }
        Ok(Self {
            name: Arc::from(name),
            bucket: Arc::from(bucket),
            prefix: Arc::from(normalize_storage_prefix(&settings.prefix)),
            client: S3Client::from_conf(config_builder.build()),
        })
    }

    #[must_use]
    pub fn object_key_for_hash(&self, hash_algo: &str, digest: &str) -> String {
        object_key_for_hash(&self.prefix, hash_algo, digest)
    }
}

#[async_trait]
impl BlobStorageBackend for S3CompatibleBlobStorage {
    fn name(&self) -> &str {
        &self.name
    }

    fn bucket(&self) -> &str {
        &self.bucket
    }

    async fn ensure(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn put_bytes(&self, data: &[u8]) -> Result<StoredBlob, StorageError> {
        let digest = sha256_hex(data);
        let object_key = self.object_key_for_hash("sha256", &digest);
        self.client
            .put_object()
            .bucket(self.bucket())
            .key(&object_key)
            .body(ByteStream::from(data.to_vec()))
            .send()
            .await
            .map_err(remote_storage_error)?;
        Ok(self.stored_blob(digest, data.len() as u64, object_key))
    }

    async fn put_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        let source_size = fs::metadata(source_path).await?.len();
        if source_size != size_bytes {
            return Err(StorageError::SourceSizeChanged);
        }
        let normalized_digest = digest.to_ascii_lowercase();
        let (source_digest, hashed_size) = hash_file(source_path).await?;
        if hashed_size != size_bytes || source_digest != normalized_digest {
            return Err(StorageError::ChecksumMismatch);
        }
        let object_key = self.object_key_for_hash("sha256", &normalized_digest);
        self.client
            .put_object()
            .bucket(self.bucket())
            .key(&object_key)
            .body(
                ByteStream::from_path(source_path)
                    .await
                    .map_err(remote_storage_error)?,
            )
            .send()
            .await
            .map_err(remote_storage_error)?;
        Ok(self.stored_blob(normalized_digest, size_bytes, object_key))
    }

    async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        let (temp_path, actual_digest, size_bytes) = stage_part_files(part_paths).await?;
        let result = async {
            if expected_digest
                .is_some_and(|expected| actual_digest != expected.to_ascii_lowercase())
            {
                return Err(StorageError::ChecksumMismatch);
            }
            self.put_file(&temp_path, &actual_digest, size_bytes).await
        }
        .await;
        let _ = fs::remove_file(&temp_path).await;
        result
    }

    async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        let output = self
            .client
            .get_object()
            .bucket(self.bucket())
            .key(object_key)
            .send()
            .await
            .map_err(|_| StorageError::NotFound)?;
        Ok(output
            .body
            .collect()
            .await
            .map_err(remote_storage_error)?
            .into_bytes()
            .to_vec())
    }

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError> {
        if end < start {
            return Err(StorageError::InvalidRange);
        }
        let output = self
            .client
            .get_object()
            .bucket(self.bucket())
            .key(object_key)
            .range(format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(|_| StorageError::NotFound)?;
        Ok(output
            .body
            .collect()
            .await
            .map_err(remote_storage_error)?
            .into_bytes()
            .to_vec())
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "Object listing is only implemented for local storage".to_string(),
        ))
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(self.bucket())
            .key(object_key)
            .send()
            .await
            .map_err(remote_storage_error)?;
        Ok(())
    }
}

impl S3CompatibleBlobStorage {
    fn stored_blob(&self, digest: String, size_bytes: u64, object_key: String) -> StoredBlob {
        StoredBlob {
            hash_algo: "sha256".to_string(),
            digest,
            size_bytes,
            backend: self.name().to_string(),
            bucket: self.bucket().to_string(),
            object_key,
        }
    }
}

pub async fn configured_blob_storage(config: &Config) -> Result<SharedBlobStorage, StorageError> {
    match config.storage_backend.trim().to_ascii_lowercase().as_str() {
        "local" => Ok(Arc::new(LocalBlobStorage::new(
            config.objects_path(),
            &config.storage_prefix,
        ))),
        "s3" => Ok(Arc::new(
            S3CompatibleBlobStorage::from_settings(S3StorageSettings::s3_from_env(
                &config.storage_prefix,
            ))
            .await?,
        )),
        "r2" => Ok(Arc::new(
            S3CompatibleBlobStorage::from_settings(S3StorageSettings::r2_from_env(
                &config.storage_prefix,
            ))
            .await?,
        )),
        backend => Err(StorageError::Configuration(format!(
            "Unsupported VAULT_STORAGE_BACKEND: {backend}"
        ))),
    }
}

#[must_use]
pub fn normalize_storage_prefix(prefix: &str) -> String {
    prefix.trim().trim_matches('/').replace('\\', "/")
}

#[must_use]
pub fn object_key_for_hash(prefix: &str, hash_algo: &str, digest: &str) -> String {
    prefixed_key(
        prefix,
        &format!("{hash_algo}/{}", digest.to_ascii_lowercase()),
    )
}

#[must_use]
pub fn multipart_manifest_key_for_hash(prefix: &str, hash_algo: &str, digest: &str) -> String {
    prefixed_key(
        prefix,
        &format!(
            "multipart/{hash_algo}/{}/manifest.json",
            digest.to_ascii_lowercase()
        ),
    )
}

#[must_use]
pub fn multipart_part_key_for_hash(
    prefix: &str,
    hash_algo: &str,
    digest: &str,
    part_number: usize,
) -> String {
    prefixed_key(
        prefix,
        &format!(
            "multipart/{hash_algo}/{}/parts/{part_number:08}.part",
            digest.to_ascii_lowercase()
        ),
    )
}

#[must_use]
pub fn multipart_part_key_for_hash_layout(
    prefix: &str,
    hash_algo: &str,
    digest: &str,
    layout_id: &str,
    part_number: usize,
) -> String {
    prefixed_key(
        prefix,
        &format!(
            "multipart/{hash_algo}/{}/parts/{}/{part_number:08}.part",
            digest.to_ascii_lowercase(),
            layout_id.to_ascii_lowercase(),
        ),
    )
}

#[must_use]
pub fn is_multipart_manifest_key(object_key: &str) -> bool {
    let cleaned = object_key.trim().trim_start_matches('/').replace('\\', "/");
    cleaned.ends_with("/manifest.json") && format!("/{cleaned}").contains("/multipart/")
}

#[must_use]
pub fn is_multipart_part_key(object_key: &str) -> bool {
    let cleaned = object_key.trim().trim_start_matches('/').replace('\\', "/");
    format!("/{cleaned}").contains("/multipart/") && cleaned.contains("/parts/")
}

fn prefixed_key(prefix: &str, key: &str) -> String {
    let normalized = normalize_storage_prefix(prefix);
    if normalized.is_empty() {
        key.to_string()
    } else {
        format!("{normalized}/{key}")
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    lower_hex(&hasher.finalize())
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn multipart_layout_id(part_sizes: &[u64]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((part_sizes.len() as u64).to_be_bytes());
    for size in part_sizes {
        hasher.update(size.to_be_bytes());
    }
    lower_hex(&hasher.finalize())
}

async fn hash_file(path: &Path) -> Result<(String, u64), StorageError> {
    let mut source = match fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(StorageError::NotFound);
        }
        Err(error) => return Err(StorageError::Io(error)),
    };
    let mut hasher = Sha256::new();
    let mut size_bytes = 0_u64;
    let mut buffer = vec![0_u8; STORAGE_CHUNK_SIZE];
    loop {
        let read = source.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size_bytes += read as u64;
    }
    Ok((lower_hex(&hasher.finalize()), size_bytes))
}

async fn file_matches_digest(
    path: &Path,
    digest: &str,
    size_bytes: u64,
) -> Result<bool, StorageError> {
    let metadata = match fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StorageError::Io(error)),
    };
    if !metadata.is_file() {
        return Err(StorageError::ContentMismatch);
    }
    if metadata.len() != size_bytes {
        return Ok(false);
    }
    match hash_file(path).await {
        Ok((actual_digest, actual_size)) => {
            Ok(actual_size == size_bytes && actual_digest == digest.to_ascii_lowercase())
        }
        Err(StorageError::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

async fn file_matches_source(
    target_path: &Path,
    source_path: &Path,
    size_bytes: u64,
) -> Result<bool, StorageError> {
    let metadata = match fs::metadata(target_path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StorageError::Io(error)),
    };
    if !metadata.is_file() {
        return Err(StorageError::ContentMismatch);
    }
    if metadata.len() != size_bytes {
        return Ok(false);
    }
    let (source_digest, source_size) = hash_file(source_path).await?;
    if source_size != size_bytes {
        return Err(StorageError::SourceSizeChanged);
    }
    let (target_digest, target_size) = match hash_file(target_path).await {
        Ok(result) => result,
        Err(StorageError::NotFound) => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(target_size == size_bytes && target_digest == source_digest)
}

async fn publish_part_file(
    source_path: &Path,
    target_path: &Path,
    size_bytes: u64,
) -> Result<(), StorageError> {
    let source_size = fs::metadata(source_path).await?.len();
    if source_size != size_bytes {
        return Err(StorageError::SourceSizeChanged);
    }
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    if file_matches_source(target_path, source_path, size_bytes).await? {
        return Ok(());
    }
    if fs::hard_link(source_path, target_path).await.is_ok() {
        return Ok(());
    }
    if file_matches_source(target_path, source_path, size_bytes).await? {
        return Ok(());
    }

    let temp_path = temp_sibling_path(target_path)?;
    let copy_result = async {
        let mut source = fs::File::open(source_path).await?;
        let mut target = fs::File::create_new(&temp_path).await?;
        let copied = tokio::io::copy(&mut source, &mut target).await?;
        target.flush().await?;
        if copied != size_bytes {
            return Err(StorageError::SourceSizeChanged);
        }
        rename_or_replace(&temp_path, target_path).await
    }
    .await;
    if copy_result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    copy_result
}

async fn verify_multipart_manifest_digest(
    manifest: &LocalMultipartManifest,
) -> Result<(), StorageError> {
    let mut hasher = Sha256::new();
    let mut size_bytes = 0_u64;
    let mut buffer = vec![0_u8; STORAGE_CHUNK_SIZE];
    for part in &manifest.parts {
        let mut source = fs::File::open(&part.path).await?;
        loop {
            let read = source.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size_bytes += read as u64;
        }
    }
    if size_bytes != manifest.size_bytes || lower_hex(&hasher.finalize()) != manifest.digest {
        return Err(StorageError::ContentMismatch);
    }
    Ok(())
}

fn stored_blob(digest: String, size_bytes: u64, object_key: String) -> StoredBlob {
    StoredBlob {
        hash_algo: "sha256".to_string(),
        digest,
        size_bytes,
        backend: "local".to_string(),
        bucket: String::new(),
        object_key,
    }
}

fn temp_sibling_path(target: &Path) -> Result<PathBuf, StorageError> {
    let name = target
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or(StorageError::InvalidStoragePath)?;
    Ok(target.with_file_name(format!("{name}.tmp-{}", Uuid::new_v4().simple())))
}

async fn rename_or_replace(source: &Path, target: &Path) -> Result<(), StorageError> {
    if fs::rename(source, target).await.is_err() {
        let _ = fs::remove_file(target).await;
        fs::rename(source, target).await?;
    }
    Ok(())
}

async fn stage_part_files(part_paths: &[PathBuf]) -> Result<(PathBuf, String, u64), StorageError> {
    let temp_path =
        std::env::temp_dir().join(format!("vault-s3-upload-{}.tmp", Uuid::new_v4().simple()));
    let mut output = fs::File::create_new(&temp_path).await?;
    let mut hasher = Sha256::new();
    let mut size_bytes = 0_u64;
    let write_result = async {
        let mut buffer = vec![0_u8; STORAGE_CHUNK_SIZE];
        for part_path in part_paths {
            let mut source = fs::File::open(part_path).await?;
            loop {
                let read = source.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                size_bytes = size_bytes
                    .checked_add(u64::try_from(read).map_err(|_| StorageError::InvalidRange)?)
                    .ok_or(StorageError::InvalidRange)?;
                tokio::io::AsyncWriteExt::write_all(&mut output, &buffer[..read]).await?;
            }
        }
        tokio::io::AsyncWriteExt::flush(&mut output).await?;
        Ok::<(), StorageError>(())
    }
    .await;
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    write_result?;
    Ok((temp_path, lower_hex(&hasher.finalize()), size_bytes))
}

fn env_trimmed_from<F>(env_var: &F, name: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    env_var(name).unwrap_or_default().trim().to_string()
}

fn env_trimmed_or_from<F>(env_var: &F, name: &str, default: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let value = env_trimmed_from(env_var, name);
    if value.is_empty() {
        default.to_string()
    } else {
        value
    }
}

fn env_optional_from<F>(env_var: &F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let value = env_trimmed_from(env_var, name);
    if value.is_empty() { None } else { Some(value) }
}

fn env_optional_fallback_from<F>(env_var: &F, primary: &str, fallback: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    env_optional_from(env_var, primary).or_else(|| env_optional_from(env_var, fallback))
}

fn remote_storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::Remote(error.to_string())
}

fn collect_object_keys(
    root: &Path,
    path: &Path,
    keys: &mut Vec<String>,
) -> Result<(), StorageError> {
    for entry_result in std::fs::read_dir(path)? {
        let entry = entry_result?;
        let entry_path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_object_keys(root, &entry_path, keys)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Some(file_name) = entry_path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if file_name.starts_with(".vault-storage.lock") {
            continue;
        }
        let Ok(relative) = entry_path.strip_prefix(root) else {
            continue;
        };
        let key = relative.to_string_lossy().replace('\\', "/");
        if key.starts_with(".vault-staging/") || is_multipart_part_key(&key) {
            continue;
        }
        keys.push(key);
    }
    Ok(())
}
