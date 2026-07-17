use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration as StdDuration, SystemTime};

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client as S3Client, Config as S3ClientConfig};
use axum::body::Bytes;
use futures_util::{Stream, StreamExt, stream};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedRwLockReadGuard, RwLock, Semaphore};
use uuid::Uuid;

use crate::config::Config;

pub const DEFAULT_STORAGE_PREFIX: &str = "objects";
pub const LOCAL_MULTIPART_FORMAT: &str = "vault.local.multipart.v1";
pub const S3_UPLOAD_STAGE_FILENAME: &str = ".vault-s3-upload.stage";
pub const STORAGE_CHUNK_SIZE: usize = 1024 * 1024;
const MAX_LOCAL_MULTIPART_MANIFEST_BYTES: u64 = 512 * 1024;
const LEGACY_S3_STAGE_PREFIX: &str = "vault-s3-upload-";
const LEGACY_S3_STAGE_SUFFIX: &str = ".tmp";
pub const STORAGE_MULTIPART_MAX_PARTS: usize = 1024;

pub type SharedBlobStorage = Arc<dyn BlobStorageBackend>;
/// Storage implementations must emit nonempty frames no larger than [`STORAGE_CHUNK_SIZE`].
pub type BlobByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send + 'static>>;

#[derive(Debug)]
pub(crate) struct LocalObjectReadGuard {
    _guard: OwnedRwLockReadGuard<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobLocation {
    pub backend: String,
    pub bucket: String,
    pub object_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobReadRange {
    pub expected_size: u64,
    pub offset: u64,
    pub length: u64,
}

impl BlobReadRange {
    fn validate(self) -> Result<(), StorageError> {
        if self.length == 0 && (self.expected_size != 0 || self.offset != 0) {
            return Err(StorageError::InvalidRange);
        }
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(StorageError::InvalidRange)?;
        if end > self.expected_size {
            return Err(StorageError::InvalidRange);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredBlob {
    pub hash_algo: String,
    pub digest: String,
    pub size_bytes: u64,
    pub backend: String,
    pub bucket: String,
    pub object_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobWriteKind {
    Bytes,
    File,
    PartFiles,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMultipartPartObject {
    pub object_key: String,
    pub path: PathBuf,
    pub modified_at: Option<SystemTime>,
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalDurabilityPoint {
    DirectoryCreated,
    StagingDirectory,
    ObjectFile,
    ObjectDirectory,
    MultipartPartFile,
    MultipartPartDirectory,
    MultipartManifestFile,
    MultipartManifestDirectory,
}

#[doc(hidden)]
pub trait LocalDurabilityHook: std::fmt::Debug + Send + Sync {
    fn before_sync(&self, point: LocalDurabilityPoint, path: &Path) -> std::io::Result<()>;
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
    #[error("blob is temporarily unavailable during a lifecycle operation")]
    Busy,
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

    async fn readiness_check(&self) -> Result<(), StorageError> {
        self.ensure().await
    }

    fn planned_object_key(
        &self,
        _hash_algo: &str,
        _digest: &str,
        _write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "storage backend cannot plan content-addressed object keys".to_string(),
        ))
    }

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

    async fn put_part_files_in_staging(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
        _staging_dir: &Path,
    ) -> Result<StoredBlob, StorageError> {
        self.put_part_files(part_paths, expected_digest).await
    }

    async fn read_bytes(&self, object_key: &str) -> Result<Vec<u8>, StorageError>;

    async fn read_range(
        &self,
        object_key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, StorageError>;

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError>;

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

    async fn stream_location_range(
        &self,
        backend: &str,
        bucket: &str,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        self.require_location(backend, bucket)?;
        self.stream_range(object_key, range).await
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

/// Returns the locations that the active backend can serve, in failover order.
///
/// Callers provide locations in database order. Exact backend/bucket matches are
/// preferred; an empty bucket is accepted second for legacy rows created before
/// bucket identity was persisted. Locations for another backend or bucket are
/// deliberately skipped instead of being attempted through the active client.
fn ranked_blob_locations<'a>(
    storage: &dyn BlobStorageBackend,
    locations: &'a [BlobLocation],
) -> Vec<&'a BlobLocation> {
    let backend = storage.name();
    let bucket = storage.bucket();
    let mut ranked = locations
        .iter()
        .filter(|location| location.backend == backend && location.bucket == bucket)
        .collect::<Vec<_>>();
    if !bucket.is_empty() {
        ranked.extend(
            locations
                .iter()
                .filter(|location| location.backend == backend && location.bucket.is_empty()),
        );
    }
    ranked
}

/// Retains the most actionable failure observed while probing blob copies.
/// Missing or incompatible copies must not hide corruption, I/O, or lifecycle
/// failures from another otherwise serviceable location.
fn remember_blob_location_error(current: &mut Option<StorageError>, candidate: StorageError) {
    fn priority(error: &StorageError) -> u8 {
        match error {
            StorageError::BackendMismatch | StorageError::NotFound => 0,
            StorageError::Busy => 2,
            _ => 1,
        }
    }
    if current
        .as_ref()
        .is_none_or(|error| priority(&candidate) > priority(error))
    {
        *current = Some(candidate);
    }
}

/// Opens the first usable active-backend copy and validates one bounded frame
/// before returning it to a response or ZIP writer. Failover ends once the
/// returned stream is consumed, so callers can never splice locations after
/// exposing bytes.
pub async fn open_ranked_location_stream(
    storage: &dyn BlobStorageBackend,
    locations: &[BlobLocation],
    range: BlobReadRange,
) -> Result<BlobByteStream, StorageError> {
    range.validate()?;
    let locations = ranked_blob_locations(storage, locations);
    if locations.is_empty() {
        return Err(StorageError::BackendMismatch);
    }
    let mut preferred_error = None;
    for location in locations {
        let mut source = match storage
            .stream_location_range(
                &location.backend,
                &location.bucket,
                &location.object_key,
                range,
            )
            .await
        {
            Ok(source) => source,
            Err(error) => {
                remember_blob_location_error(&mut preferred_error, error);
                continue;
            }
        };
        match source.next().await {
            // Probing consumes the terminal item from a zero-length backend stream. Returning
            // that already-terminated stream is unsafe because not every `Stream` is fused; in
            // particular, `Unfold` panics when polled again after yielding `None`. Hand callers a
            // canonical fused empty stream after the successful probe instead.
            None if range.length == 0 => {
                return Ok(Box::pin(stream::empty::<Result<Bytes, StorageError>>()));
            }
            Some(Ok(first))
                if range.length > 0
                    && !first.is_empty()
                    && first.len() <= STORAGE_CHUNK_SIZE
                    && first.len() as u64 <= range.length =>
            {
                return Ok(Box::pin(
                    stream::once(async move { Ok(first) }).chain(source),
                ));
            }
            Some(Err(error)) => remember_blob_location_error(&mut preferred_error, error),
            Some(Ok(_)) | None => {
                remember_blob_location_error(&mut preferred_error, StorageError::ContentMismatch);
            }
        }
    }
    Err(preferred_error.unwrap_or(StorageError::BackendMismatch))
}

#[derive(Debug, Clone)]
pub struct LocalBlobStorage {
    root: Arc<PathBuf>,
    prefix: Arc<str>,
    lifecycle_locks: Arc<ObjectLifecycleLocks>,
    multipart_inventory: Arc<Mutex<MultipartInventoryCursor>>,
    readiness_permits: Arc<Semaphore>,
    durability_hook: Option<Arc<dyn LocalDurabilityHook>>,
}

#[derive(Debug, Clone)]
pub struct S3StorageSettings {
    pub name: String,
    pub bucket: String,
    pub region: String,
    pub endpoint_url: Option<String>,
    pub allow_insecure_local_http: bool,
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
    lifecycle_locks: Arc<ObjectLifecycleLocks>,
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
    Existing(LocalMultipartManifest),
    Missing,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartPublication {
    Existing,
    Created,
    Replaced,
}

#[derive(Debug, Default)]
struct ObjectLifecycleLocks {
    entries: StdMutex<HashMap<String, Weak<RwLock<()>>>>,
}

impl ObjectLifecycleLocks {
    fn for_object(&self, object_key: &str) -> Arc<RwLock<()>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = entries.get(object_key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(RwLock::new(()));
        entries.insert(object_key.to_string(), Arc::downgrade(&lock));
        lock
    }
}

fn shared_local_lifecycle_locks(root: &Path) -> Arc<ObjectLifecycleLocks> {
    static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, Weak<ObjectLifecycleLocks>>>> =
        OnceLock::new();
    let root_key = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let registry = REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut entries = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.retain(|_, locks| locks.strong_count() > 0);
    if let Some(locks) = entries.get(&root_key).and_then(Weak::upgrade) {
        return locks;
    }
    let locks = Arc::new(ObjectLifecycleLocks::default());
    entries.insert(root_key, Arc::downgrade(&locks));
    locks
}

struct LocalMultipartReadState {
    parts: Vec<LocalMultipartPart>,
    part_index: usize,
    part_offset: u64,
    part_remaining: u64,
    source: Option<fs::File>,
    remaining: u64,
    _read_guard: OwnedRwLockReadGuard<()>,
}

#[derive(Debug, Default)]
struct MultipartInventoryCursor {
    frames: Vec<MultipartInventoryFrame>,
}

#[derive(Debug)]
struct MultipartInventoryFrame {
    entries: std::fs::ReadDir,
}

struct S3ReadState {
    body: ByteStream,
    pending: Option<Bytes>,
    remaining: u64,
    _read_guard: OwnedRwLockReadGuard<()>,
}

#[derive(Debug)]
struct OwnedStageFile {
    path: PathBuf,
    writer: Option<fs::File>,
    armed: bool,
}

impl OwnedStageFile {
    fn new(path: PathBuf, writer: fs::File) -> Self {
        Self {
            path,
            writer: Some(writer),
            armed: true,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn writer(&mut self) -> Result<&mut fs::File, StorageError> {
        self.writer.as_mut().ok_or(StorageError::InvalidStoragePath)
    }

    fn close_writer(&mut self) {
        drop(self.writer.take());
    }

    async fn cleanup(mut self) {
        self.close_writer();
        match fs::remove_file(&self.path).await {
            Ok(()) => self.armed = false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.armed = false,
            Err(error) => tracing::warn!(
                ?error,
                path = %self.path.display(),
                "could not remove S3 upload stage file asynchronously"
            ),
        }
    }
}

impl Drop for OwnedStageFile {
    fn drop(&mut self) {
        self.close_writer();
        if !self.armed {
            return;
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                ?error,
                path = %self.path.display(),
                "could not remove S3 upload stage file"
            ),
        }
    }
}

impl LocalBlobStorage {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, prefix: impl AsRef<str>) -> Self {
        Self::new_with_optional_durability_hook(root.into(), prefix.as_ref(), None)
    }

    #[doc(hidden)]
    #[must_use]
    pub fn new_with_durability_hook(
        root: impl Into<PathBuf>,
        prefix: impl AsRef<str>,
        durability_hook: Arc<dyn LocalDurabilityHook>,
    ) -> Self {
        Self::new_with_optional_durability_hook(root.into(), prefix.as_ref(), Some(durability_hook))
    }

    fn new_with_optional_durability_hook(
        root: PathBuf,
        prefix: &str,
        durability_hook: Option<Arc<dyn LocalDurabilityHook>>,
    ) -> Self {
        Self {
            lifecycle_locks: shared_local_lifecycle_locks(&root),
            multipart_inventory: Arc::new(Mutex::new(MultipartInventoryCursor::default())),
            readiness_permits: Arc::new(Semaphore::new(1)),
            root: Arc::new(root),
            prefix: Arc::from(normalize_storage_prefix(prefix)),
            durability_hook,
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
        self.create_dir_all_durable(self.root(), LocalDurabilityPoint::DirectoryCreated)
            .await
    }

    pub async fn readiness_check(&self) -> Result<(), StorageError> {
        let permit = self
            .readiness_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| StorageError::Busy)?;
        let root = self.root.as_ref().clone();
        let prefix = self.prefix.to_string();
        tokio::spawn(async move {
            let _permit = permit;
            tokio::task::spawn_blocking(move || local_storage_readiness_probe(&root, &prefix))
                .await
                .map_err(|error| StorageError::Io(std::io::Error::other(error)))?
        })
        .await
        .map_err(|error| StorageError::Io(std::io::Error::other(error)))?
    }

    fn before_sync(&self, point: LocalDurabilityPoint, path: &Path) -> Result<(), StorageError> {
        if let Some(hook) = &self.durability_hook {
            hook.before_sync(point, path)?;
        }
        Ok(())
    }

    async fn sync_open_file(
        &self,
        file: &fs::File,
        path: &Path,
        point: LocalDurabilityPoint,
    ) -> Result<(), StorageError> {
        self.before_sync(point, path)?;
        file.sync_all().await?;
        Ok(())
    }

    async fn sync_file_path(
        &self,
        path: &Path,
        point: LocalDurabilityPoint,
    ) -> Result<(), StorageError> {
        #[cfg(windows)]
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .await?;
        #[cfg(not(windows))]
        let file = fs::File::open(path).await?;
        let metadata = file.metadata().await?;
        if !metadata.is_file() {
            return Err(StorageError::ContentMismatch);
        }
        self.sync_open_file(&file, path, point).await
    }

    async fn sync_directory_at(
        &self,
        path: &Path,
        point: LocalDurabilityPoint,
    ) -> Result<(), StorageError> {
        self.before_sync(point, path)?;
        sync_directory(path).await
    }

    async fn create_dir_all_durable(
        &self,
        path: &Path,
        point: LocalDurabilityPoint,
    ) -> Result<(), StorageError> {
        let absolute = std::path::absolute(path).map_err(StorageError::Io)?;
        let mut missing = Vec::new();
        let mut cursor = absolute.as_path();
        loop {
            match fs::symlink_metadata(cursor).await {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
                {
                    break;
                }
                Ok(_) => return Err(StorageError::InvalidStoragePath),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(cursor.to_path_buf());
                    cursor = cursor.parent().ok_or(StorageError::InvalidStoragePath)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        fs::create_dir_all(&absolute).await?;
        for directory in missing.iter().rev() {
            let metadata = fs::symlink_metadata(directory).await?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(StorageError::InvalidStoragePath);
            }
            let parent = directory.parent().ok_or(StorageError::InvalidStoragePath)?;
            self.sync_directory_at(parent, point).await?;
        }
        Ok(())
    }

    #[must_use]
    pub fn object_key_for_hash(&self, hash_algo: &str, digest: &str) -> String {
        object_key_for_hash(&self.prefix, hash_algo, digest)
    }

    pub(crate) fn try_object_read_guard(
        &self,
        object_key: &str,
    ) -> Result<LocalObjectReadGuard, StorageError> {
        let guard = self
            .lifecycle_locks
            .for_object(object_key)
            .try_read_owned()
            .map_err(|_| StorageError::Busy)?;
        Ok(LocalObjectReadGuard { _guard: guard })
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
        let parent = target.parent().ok_or(StorageError::InvalidStoragePath)?;
        self.create_dir_all_durable(parent, LocalDurabilityPoint::DirectoryCreated)
            .await?;
        if file_matches_digest(&target, &digest, data.len() as u64).await? {
            self.sync_file_path(&target, LocalDurabilityPoint::ObjectFile)
                .await?;
            self.sync_directory_at(parent, LocalDurabilityPoint::ObjectDirectory)
                .await?;
        } else {
            let temp_path = temp_sibling_path(&target)?;
            let write_result = async {
                let mut temp = fs::File::create_new(&temp_path).await?;
                temp.write_all(data).await?;
                temp.flush().await?;
                self.sync_open_file(&temp, &temp_path, LocalDurabilityPoint::ObjectFile)
                    .await?;
                drop(temp);
                rename_or_replace(&temp_path, &target).await?;
                self.sync_directory_at(parent, LocalDurabilityPoint::ObjectDirectory)
                    .await
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
        let parent = target.parent().ok_or(StorageError::InvalidStoragePath)?;
        self.create_dir_all_durable(parent, LocalDurabilityPoint::DirectoryCreated)
            .await?;
        if file_matches_digest(&target, &normalized_digest, size_bytes).await? {
            self.sync_file_path(&target, LocalDurabilityPoint::ObjectFile)
                .await?;
            self.sync_directory_at(parent, LocalDurabilityPoint::ObjectDirectory)
                .await?;
        } else {
            let temp_path = temp_sibling_path(&target)?;
            let write_result = async {
                if fs::hard_link(source_path, &temp_path).await.is_err() {
                    fs::copy(source_path, &temp_path).await?;
                }
                self.sync_file_path(&temp_path, LocalDurabilityPoint::ObjectFile)
                    .await?;
                rename_or_replace(&temp_path, &target).await?;
                self.sync_directory_at(parent, LocalDurabilityPoint::ObjectDirectory)
                    .await
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
        self.create_dir_all_durable(&staging_dir, LocalDurabilityPoint::DirectoryCreated)
            .await?;
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
            self.sync_open_file(&output, &temp_path, LocalDurabilityPoint::ObjectFile)
                .await?;
            Ok::<(), StorageError>(())
        }
        .await;
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path).await;
            let _ = self
                .sync_directory_at(&staging_dir, LocalDurabilityPoint::StagingDirectory)
                .await;
        }
        write_result?;

        let digest = lower_hex(&hasher.finalize());
        let object_key = self.object_key_for_hash("sha256", &digest);
        let target = self.object_path(&object_key)?;
        let parent = target.parent().ok_or(StorageError::InvalidStoragePath)?;
        self.create_dir_all_durable(parent, LocalDurabilityPoint::DirectoryCreated)
            .await?;
        if file_matches_digest(&target, &digest, size_bytes).await? {
            self.sync_file_path(&target, LocalDurabilityPoint::ObjectFile)
                .await?;
            self.sync_directory_at(parent, LocalDurabilityPoint::ObjectDirectory)
                .await?;
            fs::remove_file(&temp_path).await?;
            self.sync_directory_at(&staging_dir, LocalDurabilityPoint::StagingDirectory)
                .await?;
        } else {
            rename_or_replace(&temp_path, &target).await?;
            self.sync_directory_at(parent, LocalDurabilityPoint::ObjectDirectory)
                .await?;
            self.sync_directory_at(&staging_dir, LocalDurabilityPoint::StagingDirectory)
                .await?;
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

    pub async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        range.validate()?;
        let read_guard = self
            .lifecycle_locks
            .for_object(object_key)
            .try_read_owned()
            .map_err(|_| StorageError::Busy)?;
        if is_multipart_manifest_key(object_key) {
            let manifest = self.read_multipart_manifest_structure(object_key).await?;
            if manifest.size_bytes != range.expected_size {
                return Err(StorageError::ContentMismatch);
            }
            // Validate every part the requested range can reach before a caller
            // selects this location. Once a response body exposes its prefetched
            // frame, switching copies would splice two objects into one download.
            // Tiny range probes do not pay an O(total multipart parts) stat cost.
            validate_multipart_range_parts(&manifest, range).await?;
            return multipart_range_stream(manifest, range, read_guard).await;
        }

        let target = self.object_path(object_key)?;
        let mut source = match fs::File::open(target).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound);
            }
            Err(error) => return Err(StorageError::Io(error)),
        };
        let metadata = source.metadata().await?;
        if !metadata.is_file() || metadata.len() != range.expected_size {
            return Err(StorageError::ContentMismatch);
        }
        if range.length == 0 {
            return Ok(empty_blob_stream(read_guard));
        }
        source.seek(std::io::SeekFrom::Start(range.offset)).await?;
        Ok(exact_file_stream(source, range.length, read_guard))
    }

    pub async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        self.ensure().await?;
        let mut keys = Vec::new();
        collect_object_keys(self.root(), self.root(), &mut keys)?;
        keys.sort();
        Ok(keys)
    }

    pub async fn scan_multipart_part_objects(
        &self,
        work_limit: usize,
    ) -> Result<(Vec<LocalMultipartPartObject>, bool), StorageError> {
        self.ensure().await?;
        let multipart_root = if self.prefix.is_empty() {
            self.root().join("multipart")
        } else {
            self.root().join(self.prefix.as_ref()).join("multipart")
        };
        if !directory_without_symlink_ancestors(self.root(), &multipart_root).await? {
            self.multipart_inventory.lock().await.frames.clear();
            return Ok((Vec::new(), true));
        }
        let mut cursor = self.multipart_inventory.lock().await;
        if cursor.frames.is_empty() {
            cursor.frames.push(MultipartInventoryFrame {
                entries: std::fs::read_dir(&multipart_root)?,
            });
        }
        let mut parts = Vec::new();
        let mut work = 0_usize;
        while work < work_limit.max(1) {
            let Some(frame) = cursor.frames.last_mut() else {
                break;
            };
            let entry = match frame.entries.next() {
                Some(Ok(entry)) => entry,
                Some(Err(error)) => {
                    cursor.frames.clear();
                    return Err(error.into());
                }
                None => {
                    cursor.frames.pop();
                    continue;
                }
            };
            work += 1;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if multipart_inventory_directory_allowed(&multipart_root, &path) {
                    cursor.frames.push(MultipartInventoryFrame {
                        entries: std::fs::read_dir(&path)?,
                    });
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(&multipart_root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            let object_key = prefixed_key(&self.prefix, &format!("multipart/{relative}"));
            if multipart_manifest_key_for_part(&self.prefix, &object_key).is_none() {
                continue;
            }
            let metadata = entry.metadata()?;
            parts.push(LocalMultipartPartObject {
                object_key,
                path,
                modified_at: metadata.modified().ok(),
            });
        }
        let scan_complete = cursor.frames.is_empty();
        Ok((parts, scan_complete))
    }

    pub async fn multipart_manifest_part_keys(
        &self,
        manifest_key: &str,
    ) -> Result<Vec<String>, StorageError> {
        Ok(self
            .read_multipart_manifest_structure(manifest_key)
            .await?
            .parts
            .into_iter()
            .map(|part| part.object_key)
            .collect())
    }

    #[must_use]
    pub fn multipart_manifest_key_for_part_object(&self, object_key: &str) -> Option<String> {
        multipart_manifest_key_for_part(&self.prefix, object_key)
    }

    pub async fn delete_unreferenced_multipart_part(
        &self,
        object_key: &str,
        minimum_age: StdDuration,
        protect_indeterminate_manifest: bool,
    ) -> Result<bool, StorageError> {
        let manifest_key = multipart_manifest_key_for_part(&self.prefix, object_key)
            .ok_or(StorageError::InvalidObjectKey)?;
        let Ok(_delete_guard) = self
            .lifecycle_locks
            .for_object(&manifest_key)
            .try_write_owned()
        else {
            return Ok(false);
        };
        match self.multipart_manifest_part_keys(&manifest_key).await {
            Ok(parts) if parts.iter().any(|part| part == object_key) => return Ok(false),
            Ok(_) => {}
            Err(
                StorageError::NotFound
                | StorageError::InvalidMultipartManifest
                | StorageError::UnreadableMultipartManifest,
            ) if !protect_indeterminate_manifest => {}
            Err(
                StorageError::NotFound
                | StorageError::InvalidMultipartManifest
                | StorageError::UnreadableMultipartManifest,
            ) => return Ok(false),
            Err(error) => return Err(error),
        }
        let path = self.object_path(object_key)?;
        if !regular_file_without_symlink_ancestors(self.root(), &path).await? {
            return Ok(false);
        }
        let metadata = match fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= minimum_age);
        if !old_enough {
            return Ok(false);
        }
        remove_file_if_present(&path).await?;
        Ok(true)
    }

    pub async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        let _delete_guard = self
            .lifecycle_locks
            .for_object(object_key)
            .write_owned()
            .await;
        let multipart_parts = if is_multipart_manifest_key(object_key) {
            self.multipart_part_paths_for_delete(object_key).await?
        } else {
            None
        };
        let target = self.object_path(object_key)?;
        if let Some(part_paths) = multipart_parts {
            let mut part_directories = HashSet::new();
            for part_path in part_paths {
                remove_file_if_present(&part_path).await?;
                if let Some(parent) = part_path.parent() {
                    part_directories.insert(parent.to_path_buf());
                }
            }
            for directory in part_directories {
                sync_directory(&directory).await?;
            }
        }
        remove_file_if_present(&target).await?;
        if let Some(parent) = target.parent() {
            sync_directory(parent).await?;
        }
        Ok(())
    }

    async fn multipart_part_paths_for_delete(
        &self,
        object_key: &str,
    ) -> Result<Option<Vec<PathBuf>>, StorageError> {
        let payload = match self.read_manifest_payload(object_key).await {
            Ok(payload) => payload,
            Err(
                StorageError::NotFound
                | StorageError::InvalidMultipartManifest
                | StorageError::UnreadableMultipartManifest,
            ) => return Ok(None),
            Err(error) => return Err(error),
        };
        match self.validated_manifest_part_paths(object_key, &payload) {
            Ok(paths) => Ok(Some(paths)),
            Err(StorageError::InvalidMultipartManifest) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn read_multipart_manifest(
        &self,
        object_key: &str,
    ) -> Result<LocalMultipartManifest, StorageError> {
        let manifest = self.read_multipart_manifest_structure(object_key).await?;
        for part in &manifest.parts {
            let metadata = match fs::metadata(&part.path).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(StorageError::NotFound);
                }
                Err(error) => return Err(StorageError::Io(error)),
            };
            if !metadata.is_file() {
                return Err(StorageError::NotFound);
            }
            if metadata.len() != part.size_bytes {
                return Err(StorageError::InvalidMultipartManifest);
            }
        }
        Ok(manifest)
    }

    async fn read_multipart_manifest_structure(
        &self,
        object_key: &str,
    ) -> Result<LocalMultipartManifest, StorageError> {
        let payload = self.read_manifest_payload(object_key).await?;
        let part_paths = self.validated_manifest_part_paths(object_key, &payload)?;
        let parts = payload
            .parts
            .into_iter()
            .zip(part_paths)
            .map(|(raw_part, path)| LocalMultipartPart {
                object_key: raw_part.object_key,
                path,
                size_bytes: raw_part.size_bytes,
            })
            .collect();
        Ok(LocalMultipartManifest {
            hash_algo: payload.hash_algo,
            digest: payload.digest,
            size_bytes: payload.size_bytes,
            parts,
        })
    }

    fn validated_manifest_part_paths(
        &self,
        object_key: &str,
        payload: &ManifestPayload,
    ) -> Result<Vec<PathBuf>, StorageError> {
        if payload.format != LOCAL_MULTIPART_FORMAT
            || payload.hash_algo != "sha256"
            || !is_canonical_sha256_digest(&payload.digest)
            || self.multipart_manifest_key_for_hash(&payload.hash_algo, &payload.digest)
                != object_key
            || payload.parts.len() > STORAGE_MULTIPART_MAX_PARTS
            || (payload.size_bytes == 0 && !payload.parts.is_empty())
            || (payload.size_bytes > 0 && payload.parts.is_empty())
        {
            return Err(StorageError::InvalidMultipartManifest);
        }
        let mut total_size = 0_u64;
        let mut part_sizes = Vec::with_capacity(payload.parts.len());
        for part in &payload.parts {
            if part.size_bytes == 0 {
                return Err(StorageError::InvalidMultipartManifest);
            }
            total_size = total_size
                .checked_add(part.size_bytes)
                .ok_or(StorageError::InvalidMultipartManifest)?;
            part_sizes.push(part.size_bytes);
        }
        if total_size != payload.size_bytes {
            return Err(StorageError::InvalidMultipartManifest);
        }
        let layout_id = multipart_layout_id(&part_sizes);
        let uses_layout_keys = payload.parts.first().is_some_and(|part| {
            part.object_key
                == multipart_part_key_for_hash_layout(
                    &self.prefix,
                    &payload.hash_algo,
                    &payload.digest,
                    &layout_id,
                    1,
                )
        });
        let mut paths = Vec::with_capacity(payload.parts.len());
        for (index, part) in payload.parts.iter().enumerate() {
            let part_number = index + 1;
            let expected_key = if uses_layout_keys {
                multipart_part_key_for_hash_layout(
                    &self.prefix,
                    &payload.hash_algo,
                    &payload.digest,
                    &layout_id,
                    part_number,
                )
            } else {
                self.multipart_part_key_for_hash(&payload.hash_algo, &payload.digest, part_number)
            };
            if part.object_key != expected_key {
                return Err(StorageError::InvalidMultipartManifest);
            }
            paths.push(self.object_path(&part.object_key)?);
        }
        Ok(paths)
    }

    async fn read_manifest_payload(
        &self,
        object_key: &str,
    ) -> Result<ManifestPayload, StorageError> {
        let manifest_path = self.object_path(object_key)?;
        let source = match fs::File::open(&manifest_path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound);
            }
            Err(error) => return Err(StorageError::Io(error)),
        };
        let metadata = source.metadata().await?;
        if !metadata.is_file() || metadata.len() > MAX_LOCAL_MULTIPART_MANIFEST_BYTES {
            return Err(StorageError::InvalidMultipartManifest);
        }
        let capacity =
            usize::try_from(metadata.len()).map_err(|_| StorageError::InvalidMultipartManifest)?;
        let mut manifest_bytes = Vec::with_capacity(capacity);
        source
            .take(MAX_LOCAL_MULTIPART_MANIFEST_BYTES + 1)
            .read_to_end(&mut manifest_bytes)
            .await?;
        if manifest_bytes.len()
            > usize::try_from(MAX_LOCAL_MULTIPART_MANIFEST_BYTES)
                .map_err(|_| StorageError::InvalidMultipartManifest)?
        {
            return Err(StorageError::InvalidMultipartManifest);
        }
        serde_json::from_slice(&manifest_bytes)
            .map_err(|_| StorageError::UnreadableMultipartManifest)
    }

    async fn put_verified_part_manifest(
        &self,
        part_paths: &[PathBuf],
        digest: &str,
    ) -> Result<StoredBlob, StorageError> {
        if part_paths.len() > STORAGE_MULTIPART_MAX_PARTS || !is_canonical_sha256_digest(digest) {
            return Err(StorageError::InvalidMultipartManifest);
        }
        self.ensure().await?;
        let manifest_key = self.multipart_manifest_key_for_hash("sha256", digest);
        let _write_guard = self
            .lifecycle_locks
            .for_object(&manifest_key)
            .write_owned()
            .await;
        let previous_part_paths = self
            .read_multipart_manifest_structure(&manifest_key)
            .await
            .ok()
            .map(|manifest| {
                manifest
                    .parts
                    .into_iter()
                    .map(|part| part.path)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let manifest_state = self
            .verified_multipart_manifest_state(&manifest_key, digest)
            .await?;
        if let MultipartManifestState::Existing(manifest) = manifest_state {
            let size_bytes = manifest.size_bytes;
            self.sync_multipart_manifest(&manifest_key, &manifest)
                .await?;
            return Ok(stored_blob(digest.to_string(), size_bytes, manifest_key));
        }

        let mut publication_rollback_paths = Vec::new();
        let publication = async {
            let (size_bytes, part_entries) = self
                .publish_multipart_part_entries(part_paths, digest, &mut publication_rollback_paths)
                .await?;

            let prospective_manifest = LocalMultipartManifest {
                hash_algo: "sha256".to_string(),
                digest: digest.to_string(),
                size_bytes,
                parts: part_entries
                    .iter()
                    .map(|part| {
                        Ok(LocalMultipartPart {
                            object_key: part.object_key.clone(),
                            path: self.object_path(&part.object_key)?,
                            size_bytes: part.size_bytes,
                        })
                    })
                    .collect::<Result<Vec<_>, StorageError>>()?,
            };
            verify_multipart_manifest_digest(&prospective_manifest).await?;
            self.write_multipart_manifest(&manifest_key, digest, size_bytes, part_entries, true)
                .await?;
            Ok::<_, StorageError>(size_bytes)
        }
        .await;
        let size_bytes = match publication {
            Ok(publication) => publication,
            Err(error) => {
                if let Err(cleanup_error) = self
                    .rollback_multipart_parts(
                        &manifest_key,
                        &publication_rollback_paths,
                        &previous_part_paths,
                    )
                    .await
                {
                    tracing::warn!(
                        ?cleanup_error,
                        manifest_key,
                        "failed to roll back multipart publication"
                    );
                }
                return Err(error);
            }
        };
        Ok(stored_blob(digest.to_string(), size_bytes, manifest_key))
    }

    async fn publish_multipart_part_entries(
        &self,
        part_paths: &[PathBuf],
        digest: &str,
        publication_rollback_paths: &mut Vec<PathBuf>,
    ) -> Result<(u64, Vec<ManifestPartPayload>), StorageError> {
        let mut part_sizes = Vec::with_capacity(part_paths.len());
        let mut size_bytes = 0_u64;
        for part_path in part_paths {
            let part_size = fs::metadata(part_path).await?.len();
            if part_size == 0 {
                return Err(StorageError::InvalidMultipartManifest);
            }
            part_sizes.push(part_size);
            size_bytes = size_bytes
                .checked_add(part_size)
                .ok_or(StorageError::InvalidMultipartManifest)?;
        }
        let layout_id = multipart_layout_id(&part_sizes);
        let part_entries = part_sizes
            .iter()
            .enumerate()
            .map(|(index, part_size)| ManifestPartPayload {
                object_key: multipart_part_key_for_hash_layout(
                    &self.prefix,
                    "sha256",
                    digest,
                    &layout_id,
                    index + 1,
                ),
                size_bytes: *part_size,
            })
            .collect::<Vec<_>>();
        let prospective_manifest = ManifestPayload {
            format: LOCAL_MULTIPART_FORMAT.to_string(),
            hash_algo: "sha256".to_string(),
            digest: digest.to_string(),
            size_bytes,
            parts: part_entries.clone(),
        };
        if serde_json::to_vec(&prospective_manifest)?.len()
            >= usize::try_from(MAX_LOCAL_MULTIPART_MANIFEST_BYTES)
                .map_err(|_| StorageError::InvalidMultipartManifest)?
        {
            return Err(StorageError::InvalidMultipartManifest);
        }
        let mut part_directories = HashSet::new();
        for (part_path, part) in part_paths.iter().zip(&part_entries) {
            let target_path = self.object_path(&part.object_key)?;
            publish_part_file(
                self,
                part_path,
                &target_path,
                part.size_bytes,
                publication_rollback_paths,
            )
            .await?;
            if let Some(parent) = target_path.parent() {
                part_directories.insert(parent.to_path_buf());
            }
        }
        for directory in part_directories {
            self.sync_directory_at(&directory, LocalDurabilityPoint::MultipartPartDirectory)
                .await?;
        }
        Ok((size_bytes, part_entries))
    }

    async fn rollback_multipart_parts(
        &self,
        manifest_key: &str,
        publication_rollback_paths: &[PathBuf],
        previous_part_paths: &HashSet<PathBuf>,
    ) -> Result<(), StorageError> {
        let mut protected = previous_part_paths.clone();
        if let Ok(manifest) = self.read_multipart_manifest_structure(manifest_key).await {
            protected.extend(manifest.parts.into_iter().map(|part| part.path));
        }
        let mut first_error = None;
        for path in publication_rollback_paths {
            if !protected.contains(path)
                && let Err(error) = remove_file_if_present(path).await
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
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
        let parent = manifest_path
            .parent()
            .ok_or(StorageError::InvalidStoragePath)?;
        self.create_dir_all_durable(parent, LocalDurabilityPoint::DirectoryCreated)
            .await?;
        let staging_dir = self.root().join(".vault-staging");
        self.create_dir_all_durable(&staging_dir, LocalDurabilityPoint::DirectoryCreated)
            .await?;
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
            if manifest_bytes.len()
                > usize::try_from(MAX_LOCAL_MULTIPART_MANIFEST_BYTES)
                    .map_err(|_| StorageError::InvalidMultipartManifest)?
            {
                return Err(StorageError::InvalidMultipartManifest);
            }
            let mut temp = fs::File::create_new(&temp_path).await?;
            temp.write_all(&manifest_bytes).await?;
            temp.flush().await?;
            self.sync_open_file(
                &temp,
                &temp_path,
                LocalDurabilityPoint::MultipartManifestFile,
            )
            .await?;
            drop(temp);
            if replace_existing {
                rename_or_replace(&temp_path, &manifest_path).await?;
                self.sync_directory_at(parent, LocalDurabilityPoint::MultipartManifestDirectory)
                    .await?;
                self.sync_directory_at(&staging_dir, LocalDurabilityPoint::StagingDirectory)
                    .await?;
                Ok(true)
            } else {
                match fs::hard_link(&temp_path, &manifest_path).await {
                    Ok(()) => {
                        fs::remove_file(&temp_path).await?;
                        self.sync_directory_at(
                            parent,
                            LocalDurabilityPoint::MultipartManifestDirectory,
                        )
                        .await?;
                        self.sync_directory_at(
                            &staging_dir,
                            LocalDurabilityPoint::StagingDirectory,
                        )
                        .await?;
                        Ok(true)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        fs::remove_file(&temp_path).await?;
                        self.sync_file_path(
                            &manifest_path,
                            LocalDurabilityPoint::MultipartManifestFile,
                        )
                        .await?;
                        self.sync_directory_at(
                            parent,
                            LocalDurabilityPoint::MultipartManifestDirectory,
                        )
                        .await?;
                        self.sync_directory_at(
                            &staging_dir,
                            LocalDurabilityPoint::StagingDirectory,
                        )
                        .await?;
                        Ok(false)
                    }
                    Err(error) => Err(StorageError::Io(error)),
                }
            }
        }
        .await;
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path).await;
            let _ = self
                .sync_directory_at(&staging_dir, LocalDurabilityPoint::StagingDirectory)
                .await;
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
            Ok(existing) => Ok(MultipartManifestState::Existing(existing)),
            Err(StorageError::NotFound) => Ok(MultipartManifestState::Missing),
            Err(
                StorageError::ContentMismatch
                | StorageError::InvalidMultipartManifest
                | StorageError::UnreadableMultipartManifest,
            ) => Ok(MultipartManifestState::Replace),
            Err(error) => Err(error),
        }
    }

    async fn sync_multipart_manifest(
        &self,
        manifest_key: &str,
        manifest: &LocalMultipartManifest,
    ) -> Result<(), StorageError> {
        let mut part_directories = HashSet::new();
        for part in &manifest.parts {
            self.sync_file_path(&part.path, LocalDurabilityPoint::MultipartPartFile)
                .await?;
            if let Some(parent) = part.path.parent() {
                part_directories.insert(parent.to_path_buf());
            }
        }
        for directory in part_directories {
            self.sync_directory_at(&directory, LocalDurabilityPoint::MultipartPartDirectory)
                .await?;
        }
        let manifest_path = self.object_path(manifest_key)?;
        self.sync_file_path(&manifest_path, LocalDurabilityPoint::MultipartManifestFile)
            .await?;
        let parent = manifest_path
            .parent()
            .ok_or(StorageError::InvalidStoragePath)?;
        self.sync_directory_at(parent, LocalDurabilityPoint::MultipartManifestDirectory)
            .await
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

fn empty_blob_stream(read_guard: OwnedRwLockReadGuard<()>) -> BlobByteStream {
    Box::pin(stream::unfold(Some(read_guard), |read_guard| async move {
        drop(read_guard);
        None
    }))
}

fn exact_file_stream(
    source: fs::File,
    remaining: u64,
    read_guard: OwnedRwLockReadGuard<()>,
) -> BlobByteStream {
    Box::pin(stream::try_unfold(
        (source, remaining, read_guard),
        |(mut source, remaining, read_guard)| async move {
            if remaining == 0 {
                return Ok(None);
            }
            let requested = usize::try_from(remaining.min(STORAGE_CHUNK_SIZE as u64))
                .map_err(|_| StorageError::InvalidRange)?;
            let mut buffer = vec![0_u8; requested];
            let read = source.read(&mut buffer).await?;
            if read == 0 {
                return Err(StorageError::ContentMismatch);
            }
            buffer.truncate(read);
            Ok(Some((
                Bytes::from(buffer),
                (source, remaining - read as u64, read_guard),
            )))
        },
    ))
}

async fn multipart_range_stream(
    manifest: LocalMultipartManifest,
    range: BlobReadRange,
    read_guard: OwnedRwLockReadGuard<()>,
) -> Result<BlobByteStream, StorageError> {
    if range.length == 0 {
        return Ok(empty_blob_stream(read_guard));
    }
    let mut part_index = 0_usize;
    let mut part_offset = range.offset;
    while let Some(part) = manifest.parts.get(part_index) {
        if part_offset < part.size_bytes {
            break;
        }
        part_offset -= part.size_bytes;
        part_index += 1;
    }
    let Some(part) = manifest.parts.get(part_index) else {
        return Err(StorageError::InvalidMultipartManifest);
    };
    let part_remaining = part.size_bytes - part_offset;
    let source = open_multipart_part(part, part_offset, StorageError::NotFound).await?;
    let state = LocalMultipartReadState {
        parts: manifest.parts,
        part_index,
        part_offset,
        part_remaining,
        source: Some(source),
        remaining: range.length,
        _read_guard: read_guard,
    };
    Ok(Box::pin(stream::try_unfold(
        state,
        |mut state| async move {
            loop {
                if state.remaining == 0 {
                    return Ok(None);
                }
                if state.part_remaining == 0 {
                    state.source = None;
                    state.part_index += 1;
                    state.part_offset = 0;
                    let Some(part) = state.parts.get(state.part_index) else {
                        return Err(StorageError::ContentMismatch);
                    };
                    state.part_remaining = part.size_bytes;
                    continue;
                }
                if state.source.is_none() {
                    let part = state
                        .parts
                        .get(state.part_index)
                        .ok_or(StorageError::ContentMismatch)?;
                    state.source = Some(
                        open_multipart_part(part, state.part_offset, StorageError::ContentMismatch)
                            .await?,
                    );
                }
                let requested = usize::try_from(
                    state
                        .remaining
                        .min(state.part_remaining)
                        .min(STORAGE_CHUNK_SIZE as u64),
                )
                .map_err(|_| StorageError::InvalidRange)?;
                let mut buffer = vec![0_u8; requested];
                let read = state
                    .source
                    .as_mut()
                    .ok_or(StorageError::ContentMismatch)?
                    .read(&mut buffer)
                    .await?;
                if read == 0 {
                    return Err(StorageError::ContentMismatch);
                }
                let read_bytes = u64::try_from(read).map_err(|_| StorageError::ContentMismatch)?;
                state.part_offset += read_bytes;
                state.part_remaining -= read_bytes;
                state.remaining -= read_bytes;
                buffer.truncate(read);
                return Ok(Some((Bytes::from(buffer), state)));
            }
        },
    )))
}

async fn validate_multipart_range_parts(
    manifest: &LocalMultipartManifest,
    range: BlobReadRange,
) -> Result<(), StorageError> {
    if range.length == 0 {
        return Ok(());
    }
    let range_end = range
        .offset
        .checked_add(range.length)
        .ok_or(StorageError::InvalidRange)?;
    let mut part_start = 0_u64;
    for part in &manifest.parts {
        let part_end = part_start
            .checked_add(part.size_bytes)
            .ok_or(StorageError::InvalidMultipartManifest)?;
        if part_end > range.offset && part_start < range_end {
            let metadata = match fs::metadata(&part.path).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(StorageError::NotFound);
                }
                Err(error) => return Err(StorageError::Io(error)),
            };
            if !metadata.is_file() {
                return Err(StorageError::NotFound);
            }
            if metadata.len() != part.size_bytes {
                return Err(StorageError::InvalidMultipartManifest);
            }
        }
        if part_end >= range_end {
            break;
        }
        part_start = part_end;
    }
    Ok(())
}

async fn open_multipart_part(
    part: &LocalMultipartPart,
    offset: u64,
    missing_error: StorageError,
) -> Result<fs::File, StorageError> {
    let mut source = match fs::File::open(&part.path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(missing_error),
        Err(error) => return Err(StorageError::Io(error)),
    };
    let metadata = source.metadata().await?;
    if !metadata.is_file() || metadata.len() != part.size_bytes {
        return Err(StorageError::ContentMismatch);
    }
    source.seek(std::io::SeekFrom::Start(offset)).await?;
    Ok(source)
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

    async fn readiness_check(&self) -> Result<(), StorageError> {
        LocalBlobStorage::readiness_check(self).await
    }

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        Ok(match write_kind {
            BlobWriteKind::PartFiles => self.multipart_manifest_key_for_hash(hash_algo, digest),
            BlobWriteKind::Bytes | BlobWriteKind::File => {
                self.object_key_for_hash(hash_algo, digest)
            }
        })
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

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        LocalBlobStorage::stream_range(self, object_key, range).await
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
            allow_insecure_local_http: env_flag_from(
                &env_var,
                "VAULT_S3_ALLOW_INSECURE_LOCAL_HTTP",
            ),
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
            allow_insecure_local_http: env_flag_from(
                &env_var,
                "VAULT_R2_ALLOW_INSECURE_LOCAL_HTTP",
            ),
            access_key_id: env_optional_from(&env_var, "VAULT_R2_ACCESS_KEY_ID"),
            secret_access_key: env_optional_from(&env_var, "VAULT_R2_SECRET_ACCESS_KEY"),
            session_token: None,
            prefix: prefix.to_string(),
        }
    }
}

fn validate_s3_endpoint_url(
    endpoint_url: &str,
    allow_insecure_local_http: bool,
) -> Result<String, StorageError> {
    let endpoint = Url::parse(endpoint_url).map_err(|_| {
        StorageError::Configuration("S3-compatible storage endpoint URL is invalid".to_string())
    })?;
    let host = endpoint.host_str().ok_or_else(|| {
        StorageError::Configuration("S3-compatible storage endpoint URL is invalid".to_string())
    })?;
    if !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(StorageError::Configuration(
            "S3-compatible storage endpoint URL is invalid".to_string(),
        ));
    }

    match endpoint.scheme() {
        "https" => {}
        "http" if !allow_insecure_local_http => Err(StorageError::Configuration(
            "S3-compatible storage endpoint must use HTTPS".to_string(),
        ))?,
        "http" if is_loopback_endpoint_host(host) => {}
        "http" => return Err(StorageError::Configuration(
            "S3-compatible storage endpoint may use HTTP only for loopback hosts when explicitly enabled"
                .to_string(),
        )),
        _ => return Err(StorageError::Configuration(
            "S3-compatible storage endpoint URL must use HTTP or HTTPS".to_string(),
        )),
    }
    Ok(endpoint.to_string())
}

fn is_loopback_endpoint_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let normalized_host = host.to_ascii_lowercase();
    if normalized_host == "localhost"
        || (normalized_host.ends_with(".localhost") && normalized_host.len() > ".localhost".len())
    {
        return true;
    }

    match normalized_host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => address.octets()[0] == 127,
        Ok(IpAddr::V6(address)) => {
            address.is_loopback()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|address| address.octets()[0] == 127)
        }
        Err(_) => false,
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
        let endpoint_url = settings
            .endpoint_url
            .as_deref()
            .map(str::trim)
            .filter(|endpoint_url| !endpoint_url.is_empty());
        let endpoint_url = endpoint_url
            .map(|endpoint_url| {
                validate_s3_endpoint_url(endpoint_url, settings.allow_insecure_local_http)
            })
            .transpose()?;
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
        if let Some(endpoint_url) = endpoint_url.as_deref() {
            config_builder = config_builder.endpoint_url(endpoint_url);
        }
        Ok(Self {
            name: Arc::from(name),
            bucket: Arc::from(bucket),
            prefix: Arc::from(normalize_storage_prefix(&settings.prefix)),
            client: S3Client::from_conf(config_builder.build()),
            lifecycle_locks: Arc::new(ObjectLifecycleLocks::default()),
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

    async fn readiness_check(&self) -> Result<(), StorageError> {
        self.client
            .head_bucket()
            .bucket(self.bucket())
            .send()
            .await
            .map_err(remote_storage_error)?;
        Ok(())
    }

    fn planned_object_key(
        &self,
        hash_algo: &str,
        digest: &str,
        _write_kind: BlobWriteKind,
    ) -> Result<String, StorageError> {
        Ok(self.object_key_for_hash(hash_algo, digest))
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
        self.put_preverified_file(source_path, &normalized_digest, size_bytes)
            .await
    }

    async fn put_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
    ) -> Result<StoredBlob, StorageError> {
        if part_paths.is_empty() {
            let digest = sha256_hex(&[]);
            if expected_digest.is_some_and(|expected| digest != expected.to_ascii_lowercase()) {
                return Err(StorageError::ChecksumMismatch);
            }
            return self.put_bytes(&[]).await;
        }
        let staging_dir = common_part_parent(part_paths)?;
        self.put_staged_part_files(part_paths, expected_digest, &staging_dir)
            .await
    }

    async fn put_part_files_in_staging(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
        staging_dir: &Path,
    ) -> Result<StoredBlob, StorageError> {
        self.put_staged_part_files(part_paths, expected_digest, staging_dir)
            .await
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

    async fn stream_range(
        &self,
        object_key: &str,
        range: BlobReadRange,
    ) -> Result<BlobByteStream, StorageError> {
        range.validate()?;
        let read_guard = self
            .lifecycle_locks
            .for_object(object_key)
            .try_read_owned()
            .map_err(|_| StorageError::Busy)?;
        let mut request = self
            .client
            .get_object()
            .bucket(self.bucket())
            .key(object_key);
        let is_partial = range.offset != 0 || range.length != range.expected_size;
        if is_partial {
            let end = range
                .offset
                .checked_add(range.length - 1)
                .ok_or(StorageError::InvalidRange)?;
            request = request.range(format!("bytes={}-{}", range.offset, end));
        }
        let output = request.send().await.map_err(|error| {
            if error
                .as_service_error()
                .is_some_and(aws_sdk_s3::operation::get_object::GetObjectError::is_no_such_key)
            {
                StorageError::NotFound
            } else {
                remote_storage_error(error)
            }
        })?;
        let expected_content_length =
            i64::try_from(range.length).map_err(|_| StorageError::InvalidRange)?;
        if output.content_length() != Some(expected_content_length) {
            return Err(StorageError::ContentMismatch);
        }
        if is_partial {
            let end = range.offset + range.length - 1;
            if !s3_content_range_matches(
                output.content_range(),
                range.offset,
                end,
                range.expected_size,
            ) {
                return Err(StorageError::ContentMismatch);
            }
        } else if output.content_range().is_some() {
            return Err(StorageError::ContentMismatch);
        }
        let state = S3ReadState {
            body: output.body,
            pending: None,
            remaining: range.length,
            _read_guard: read_guard,
        };
        Ok(Box::pin(stream::try_unfold(
            state,
            |mut state| async move {
                let mut empty_frames = 0_u8;
                loop {
                    if state.remaining == 0 {
                        return Ok(None);
                    }
                    let mut bytes = if let Some(bytes) = state.pending.take() {
                        bytes
                    } else {
                        match state.body.next().await {
                            Some(Ok(bytes)) => bytes,
                            Some(Err(error)) => return Err(remote_storage_error(error)),
                            None => return Err(StorageError::ContentMismatch),
                        }
                    };
                    if bytes.is_empty() {
                        empty_frames += 1;
                        if empty_frames == 16 {
                            tokio::task::yield_now().await;
                            empty_frames = 0;
                        }
                        continue;
                    }
                    if bytes.len() as u64 > state.remaining {
                        return Err(StorageError::ContentMismatch);
                    }
                    let emit_len = bytes.len().min(STORAGE_CHUNK_SIZE);
                    let emit = bytes.split_to(emit_len);
                    if !bytes.is_empty() {
                        state.pending = Some(bytes);
                    }
                    state.remaining -= emit.len() as u64;
                    return Ok(Some((emit, state)));
                }
            },
        )))
    }

    async fn list_object_keys(&self) -> Result<Vec<String>, StorageError> {
        Err(StorageError::UnsupportedOperation(
            "Object listing is only implemented for local storage".to_string(),
        ))
    }

    async fn delete_object(&self, object_key: &str) -> Result<(), StorageError> {
        let _delete_guard = self
            .lifecycle_locks
            .for_object(object_key)
            .write_owned()
            .await;
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

    async fn put_preverified_file(
        &self,
        source_path: &Path,
        digest: &str,
        size_bytes: u64,
    ) -> Result<StoredBlob, StorageError> {
        let source_metadata = fs::metadata(source_path).await?;
        if !source_metadata.is_file() || source_metadata.len() != size_bytes {
            return Err(StorageError::SourceSizeChanged);
        }
        let object_key = self.object_key_for_hash("sha256", digest);
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
        Ok(self.stored_blob(digest.to_string(), size_bytes, object_key))
    }

    async fn put_staged_part_files(
        &self,
        part_paths: &[PathBuf],
        expected_digest: Option<&str>,
        staging_dir: &Path,
    ) -> Result<StoredBlob, StorageError> {
        let (stage, actual_digest, size_bytes) =
            stage_part_files_in(part_paths, staging_dir).await?;
        let result = if expected_digest
            .is_some_and(|expected| actual_digest != expected.to_ascii_lowercase())
        {
            Err(StorageError::ChecksumMismatch)
        } else {
            self.put_preverified_file(stage.path(), &actual_digest, size_bytes)
                .await
        };
        stage.cleanup().await;
        result
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

fn multipart_manifest_key_for_part(prefix: &str, object_key: &str) -> Option<String> {
    let cleaned = object_key.trim().trim_start_matches('/').replace('\\', "/");
    if cleaned != object_key {
        return None;
    }
    let normalized_prefix = normalize_storage_prefix(prefix);
    let relative = if normalized_prefix.is_empty() {
        cleaned.as_str()
    } else {
        cleaned.strip_prefix(&format!("{normalized_prefix}/"))?
    };
    let components = relative.split('/').collect::<Vec<_>>();
    let canonical_part_name = |value: &str| {
        let stem = value.strip_suffix(".part")?;
        (stem.len() == 8
            && stem.bytes().all(|byte| byte.is_ascii_digit())
            && stem
                .parse::<usize>()
                .is_ok_and(|part| (1..=STORAGE_MULTIPART_MAX_PARTS).contains(&part)))
        .then_some(())
    };
    let legacy = components.len() == 5 && canonical_part_name(components[4]).is_some();
    let layout = components.len() == 6
        && is_canonical_sha256_digest(components[4])
        && canonical_part_name(components[5]).is_some();
    if (!legacy && !layout)
        || components[0] != "multipart"
        || components[1] != "sha256"
        || !is_canonical_sha256_digest(components[2])
        || components[3] != "parts"
    {
        return None;
    }
    Some(multipart_manifest_key_for_hash(
        &normalized_prefix,
        "sha256",
        components[2],
    ))
}

fn prefixed_key(prefix: &str, key: &str) -> String {
    let normalized = normalize_storage_prefix(prefix);
    if normalized.is_empty() {
        key.to_string()
    } else {
        format!("{normalized}/{key}")
    }
}

#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    lower_hex(&hasher.finalize())
}

fn is_canonical_sha256_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
    storage: &LocalBlobStorage,
    source_path: &Path,
    target_path: &Path,
    size_bytes: u64,
    publication_rollback_paths: &mut Vec<PathBuf>,
) -> Result<PartPublication, StorageError> {
    let source_size = fs::metadata(source_path).await?.len();
    if source_size != size_bytes {
        return Err(StorageError::SourceSizeChanged);
    }
    let parent = target_path
        .parent()
        .ok_or(StorageError::InvalidStoragePath)?;
    storage
        .create_dir_all_durable(parent, LocalDurabilityPoint::DirectoryCreated)
        .await?;
    if file_matches_source(target_path, source_path, size_bytes).await? {
        storage
            .sync_file_path(target_path, LocalDurabilityPoint::MultipartPartFile)
            .await?;
        return Ok(PartPublication::Existing);
    }
    if fs::hard_link(source_path, target_path).await.is_ok() {
        publication_rollback_paths.push(target_path.to_path_buf());
        storage
            .sync_file_path(target_path, LocalDurabilityPoint::MultipartPartFile)
            .await?;
        return Ok(PartPublication::Created);
    }
    if file_matches_source(target_path, source_path, size_bytes).await? {
        storage
            .sync_file_path(target_path, LocalDurabilityPoint::MultipartPartFile)
            .await?;
        return Ok(PartPublication::Existing);
    }

    let temp_path = temp_sibling_path(target_path)?;
    let copy_result = async {
        let mut source = fs::File::open(source_path).await?;
        let mut target = fs::File::create_new(&temp_path).await?;
        publication_rollback_paths.push(temp_path.clone());
        let copied = tokio::io::copy(&mut source, &mut target).await?;
        target.flush().await?;
        if copied != size_bytes {
            return Err(StorageError::SourceSizeChanged);
        }
        storage
            .sync_open_file(&target, &temp_path, LocalDurabilityPoint::MultipartPartFile)
            .await?;
        drop(target);
        match fs::hard_link(&temp_path, target_path).await {
            Ok(()) => {
                publication_rollback_paths.push(target_path.to_path_buf());
                remove_file_if_present(&temp_path).await?;
                Ok(PartPublication::Created)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if file_matches_source(target_path, source_path, size_bytes).await? {
                    storage
                        .sync_file_path(target_path, LocalDurabilityPoint::MultipartPartFile)
                        .await?;
                    remove_file_if_present(&temp_path).await?;
                    Ok(PartPublication::Existing)
                } else {
                    rename_or_replace(&temp_path, target_path).await?;
                    Ok(PartPublication::Replaced)
                }
            }
            Err(error) => Err(error.into()),
        }
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

struct LocalReadinessProbeCleanup {
    path: PathBuf,
    armed: bool,
}

impl LocalReadinessProbeCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn cleanup(mut self) -> Result<(), StorageError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {
                self.armed = false;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.armed = false;
                Ok(())
            }
            Err(error) => Err(StorageError::Io(error)),
        }
    }
}

impl Drop for LocalReadinessProbeCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn local_storage_readiness_probe(root: &Path, prefix: &str) -> Result<(), StorageError> {
    use std::io::{Read as _, Seek as _, Write as _};

    const PROBE_BYTES: &[u8] = b"vault-storage-readiness-v1";

    let root_metadata = std::fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(StorageError::InvalidStoragePath);
    }
    let components = Path::new(prefix)
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(component) => Some(Ok(component.to_os_string())),
            std::path::Component::CurDir => None,
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => Some(Err(StorageError::InvalidStoragePath)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut probe_parent = root.to_path_buf();
    for component in components {
        let candidate = probe_parent.join(component);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(StorageError::InvalidStoragePath);
            }
            Ok(_) => probe_parent = candidate,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(StorageError::Io(error)),
        }
    }

    let probe_path = probe_parent.join(format!(
        ".vault-storage.lock.readiness-{}",
        Uuid::new_v4().simple()
    ));
    let mut probe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&probe_path)?;
    let cleanup = LocalReadinessProbeCleanup::new(probe_path);
    let operation = (|| {
        probe.write_all(PROBE_BYTES)?;
        probe.sync_all()?;
        if !probe.metadata()?.is_file() || probe.metadata()?.len() != PROBE_BYTES.len() as u64 {
            return Err(StorageError::ContentMismatch);
        }
        probe.seek(std::io::SeekFrom::Start(0))?;
        let mut observed = [0_u8; PROBE_BYTES.len()];
        probe.read_exact(&mut observed)?;
        if observed != PROBE_BYTES {
            return Err(StorageError::ContentMismatch);
        }
        let mut trailing = [0_u8; 1];
        if probe.read(&mut trailing)? != 0 {
            return Err(StorageError::ContentMismatch);
        }
        Ok(())
    })();
    drop(probe);
    let cleanup_result = cleanup.cleanup();
    operation.and(cleanup_result)
}

fn temp_sibling_path(target: &Path) -> Result<PathBuf, StorageError> {
    let name = target
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or(StorageError::InvalidStoragePath)?;
    Ok(target.with_file_name(format!("{name}.tmp-{}", Uuid::new_v4().simple())))
}

#[cfg(unix)]
async fn rename_or_replace(source: &Path, target: &Path) -> Result<(), StorageError> {
    fs::rename(source, target).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn rename_or_replace(source: &Path, target: &Path) -> Result<(), StorageError> {
    // `std::fs::rename` does not replace an existing Windows target. Preserve
    // the live object and report the repair failure rather than introducing a
    // destructive unlink-then-rename power-loss window.
    fs::rename(source, target).await?;
    Ok(())
}

async fn remove_file_if_present(path: &Path) -> Result<(), StorageError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StorageError::Io(error)),
    }
}

#[cfg(unix)]
async fn sync_directory(path: &Path) -> Result<(), StorageError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
        .await
        .map_err(|error| StorageError::Io(std::io::Error::other(error)))??;
    Ok(())
}

#[cfg(windows)]
async fn sync_directory(path: &Path) -> Result<(), StorageError> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?
            .sync_all()
    })
    .await
    .map_err(|error| StorageError::Io(std::io::Error::other(error)))??;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
async fn sync_directory(_path: &Path) -> Result<(), StorageError> {
    Err(StorageError::UnsupportedOperation(
        "durable local storage directory publication is unsupported on this platform".to_string(),
    ))
}

fn common_part_parent(part_paths: &[PathBuf]) -> Result<PathBuf, StorageError> {
    let parent = part_paths
        .first()
        .and_then(|path| path.parent())
        .ok_or(StorageError::InvalidStoragePath)?;
    if part_paths.iter().any(|path| path.parent() != Some(parent)) {
        return Err(StorageError::InvalidStoragePath);
    }
    Ok(parent.to_path_buf())
}

pub async fn remove_s3_upload_stage_file(staging_dir: &Path) -> Result<bool, StorageError> {
    let directory_metadata = fs::symlink_metadata(staging_dir).await?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(StorageError::InvalidStoragePath);
    }
    let stage_path = staging_dir.join(S3_UPLOAD_STAGE_FILENAME);
    let stage_metadata = match fs::symlink_metadata(&stage_path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if stage_metadata.file_type().is_symlink() || !stage_metadata.is_file() {
        return Err(StorageError::InvalidStoragePath);
    }
    fs::remove_file(stage_path).await?;
    Ok(true)
}

pub async fn sweep_legacy_s3_stage_files(
    temp_dir: &Path,
    minimum_age: StdDuration,
    work_batch_size: usize,
) -> Result<Vec<String>, StorageError> {
    if !temp_dir.is_absolute() || work_batch_size == 0 {
        return if work_batch_size == 0 {
            Ok(Vec::new())
        } else {
            Err(StorageError::InvalidStoragePath)
        };
    }
    let supplied_metadata = fs::symlink_metadata(temp_dir).await?;
    if supplied_metadata.file_type().is_symlink() || !supplied_metadata.is_dir() {
        return Err(StorageError::InvalidStoragePath);
    }
    let temp_dir = fs::canonicalize(temp_dir).await?;
    if temp_dir.parent().is_none() {
        return Err(StorageError::InvalidStoragePath);
    }
    let directory_metadata = fs::symlink_metadata(&temp_dir).await?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(StorageError::InvalidStoragePath);
    }
    let mut entries = fs::read_dir(&temp_dir).await?;
    let now = SystemTime::now();
    let mut batch_work = 0_usize;
    let mut deleted = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if deleted.len() == work_batch_size {
            break;
        }
        batch_work += 1;
        if batch_work == work_batch_size {
            batch_work = 0;
            tokio::task::yield_now().await;
        }
        let Some(file_name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        if !is_legacy_s3_stage_filename(&file_name) || !entry.file_type().await?.is_file() {
            continue;
        }
        let path = temp_dir.join(&file_name);
        if entry.path() != path {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_none_or(|age| age < minimum_age)
        {
            continue;
        }
        let current_directory = fs::symlink_metadata(&temp_dir).await?;
        if current_directory.file_type().is_symlink() || !current_directory.is_dir() {
            return Err(StorageError::InvalidStoragePath);
        }
        let current_metadata = match fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !same_file_snapshot(&metadata, &current_metadata)
            || current_metadata
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_none_or(|age| age < minimum_age)
        {
            continue;
        }
        match fs::remove_file(&path).await {
            Ok(()) => deleted.push(file_name),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(deleted)
}

fn same_file_snapshot(first: &std::fs::Metadata, current: &std::fs::Metadata) -> bool {
    if current.file_type().is_symlink()
        || !current.is_file()
        || first.len() != current.len()
        || first.modified().ok() != current.modified().ok()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        first.dev() == current.dev() && first.ino() == current.ino()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn is_legacy_s3_stage_filename(file_name: &str) -> bool {
    file_name
        .strip_prefix(LEGACY_S3_STAGE_PREFIX)
        .and_then(|name| name.strip_suffix(LEGACY_S3_STAGE_SUFFIX))
        .is_some_and(|uuid| {
            uuid.len() == 32
                && uuid
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

async fn stage_part_files_in(
    part_paths: &[PathBuf],
    staging_dir: &Path,
) -> Result<(OwnedStageFile, String, u64), StorageError> {
    let directory_metadata = fs::symlink_metadata(staging_dir).await?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(StorageError::InvalidStoragePath);
    }
    remove_s3_upload_stage_file(staging_dir).await?;
    let stage_path = staging_dir.join(S3_UPLOAD_STAGE_FILENAME);
    let output = fs::File::create_new(&stage_path).await?;
    let mut stage = OwnedStageFile::new(stage_path, output);
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
                tokio::io::AsyncWriteExt::write_all(stage.writer()?, &buffer[..read]).await?;
            }
        }
        tokio::io::AsyncWriteExt::flush(stage.writer()?).await?;
        Ok::<(), StorageError>(())
    }
    .await;
    stage.close_writer();
    write_result?;
    Ok((stage, lower_hex(&hasher.finalize()), size_bytes))
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

fn env_flag_from<F>(env_var: &F, name: &str) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    env_var(name).is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn remote_storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::Remote(error.to_string())
}

fn s3_content_range_matches(
    value: Option<&str>,
    expected_start: u64,
    expected_end: u64,
    expected_total: u64,
) -> bool {
    let Some((unit, value)) = value.and_then(|value| value.trim().split_once(' ')) else {
        return false;
    };
    if !unit.eq_ignore_ascii_case("bytes") {
        return false;
    }
    let Some((span, total)) = value.split_once('/') else {
        return false;
    };
    let Some((start, end)) = span.split_once('-') else {
        return false;
    };
    start.parse::<u64>().ok() == Some(expected_start)
        && end.parse::<u64>().ok() == Some(expected_end)
        && total.parse::<u64>().ok() == Some(expected_total)
}

fn collect_object_keys(
    root: &Path,
    path: &Path,
    keys: &mut Vec<String>,
) -> Result<(), StorageError> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_dir() {
        return Ok(());
    }
    for entry_result in std::fs::read_dir(path)? {
        let entry = entry_result?;
        let entry_path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_object_keys(root, &entry_path, keys)?;
            continue;
        }
        if !file_type.is_file() {
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

async fn regular_file_without_symlink_ancestors(
    root: &Path,
    path: &Path,
) -> Result<bool, StorageError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| StorageError::InvalidObjectKey)?;
    let root_metadata = fs::symlink_metadata(root).await?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Ok(false);
    }
    let components = relative.components().collect::<Vec<_>>();
    let mut current = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            return Ok(false);
        }
        let is_last = index + 1 == components.len();
        if (is_last && !metadata.is_file()) || (!is_last && !metadata.is_dir()) {
            return Ok(false);
        }
    }
    Ok(!components.is_empty())
}

async fn directory_without_symlink_ancestors(
    root: &Path,
    path: &Path,
) -> Result<bool, StorageError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| StorageError::InvalidObjectKey)?;
    let root_metadata = fs::symlink_metadata(root).await?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Ok(false);
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn multipart_inventory_directory_allowed(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>();
    let Some(components) = components else {
        return false;
    };
    match components.as_slice() {
        ["sha256"] => true,
        ["sha256", digest] | ["sha256", digest, "parts"] => is_canonical_sha256_digest(digest),
        ["sha256", digest, "parts", layout] => {
            is_canonical_sha256_digest(digest) && is_canonical_sha256_digest(layout)
        }
        _ => false,
    }
}
