"""Storage backends for content-addressed vault blobs."""

import datetime
import hashlib
import shutil
import threading
import uuid
from collections.abc import Iterator
from contextlib import AbstractContextManager, contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

from .config import (
    OBJECTS_PATH,
    R2_ACCESS_KEY_ID,
    R2_BUCKET,
    R2_ENDPOINT_URL,
    R2_SECRET_ACCESS_KEY,
    S3_ACCESS_KEY_ID,
    S3_BUCKET,
    S3_ENDPOINT_URL,
    S3_REGION,
    S3_SECRET_ACCESS_KEY,
    S3_SESSION_TOKEN,
    STORAGE_BACKEND,
    STORAGE_PREFIX,
)

storage_lock = threading.Lock()
FILES_LOCK_PATH = OBJECTS_PATH / ".vault-storage.lock"
STORAGE_CHUNK_SIZE = 1024 * 1024

msvcrt: Any = None
try:
    import fcntl as _fcntl

    fcntl: Any = _fcntl
except ImportError:  # pragma: no cover - exercised on Windows only
    fcntl = None
    import msvcrt as _msvcrt

    msvcrt = _msvcrt


class StorageError(Exception):
    """Base class for storage backend errors."""


class StorageNotFoundError(StorageError):
    """Raised when a referenced blob is missing from the storage medium."""


class StorageConfigurationError(StorageError):
    """Raised when the configured storage backend cannot serve the request."""


@dataclass(frozen=True)
class StoredBlob:
    hash_algo: str
    digest: str
    size_bytes: int
    backend: str
    bucket: str
    object_key: str


class BlobReader(Protocol):
    def read(self, size: int = -1) -> bytes: ...


class BlobStorageBackend:
    name: str
    bucket: str

    def put_bytes(self, data: bytes, content_type: str | None = None) -> StoredBlob:
        raise NotImplementedError

    def put_file(
        self,
        source_path: Path,
        digest: str,
        size_bytes: int,
        content_type: str | None = None,
    ) -> StoredBlob:
        raise NotImplementedError

    def read_bytes(self, object_key: str, bucket: str | None = None) -> bytes:
        raise NotImplementedError

    def open_reader(
        self,
        object_key: str,
        bucket: str | None = None,
    ) -> AbstractContextManager[BlobReader]:
        raise NotImplementedError

    def list_object_keys(self) -> list[str]:
        raise NotImplementedError

    def delete_object(self, object_key: str) -> None:
        raise NotImplementedError

    def ensure(self) -> None:
        return None


def _prefixed_key(key: str) -> str:
    return f"{STORAGE_PREFIX}/{key}" if STORAGE_PREFIX else key


def object_key_for_hash(hash_algo: str, digest: str) -> str:
    return _prefixed_key(f"{hash_algo}/{digest}")


class LocalBlobStorage(BlobStorageBackend):
    name = "local"
    bucket = ""

    def __init__(self, root: Path | None = None) -> None:
        self.root = root or OBJECTS_PATH

    def ensure(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)

    def _object_path(self, object_key: str) -> Path:
        cleaned = object_key.strip().lstrip("/").replace("\\", "/")
        target = (self.root / cleaned).resolve()
        if self.root not in target.parents and target != self.root:
            raise StorageConfigurationError("Invalid object key")
        return target

    def put_bytes(self, data: bytes, content_type: str | None = None) -> StoredBlob:
        del content_type
        digest = hashlib.sha256(data).hexdigest()
        object_key = object_key_for_hash("sha256", digest)
        target = self._object_path(object_key)
        target.parent.mkdir(parents=True, exist_ok=True)
        if not target.exists():
            temp_path = target.with_name(f"{target.name}.tmp-{uuid.uuid4().hex}")
            temp_path.write_bytes(data)
            temp_path.replace(target)
        return StoredBlob(
            hash_algo="sha256",
            digest=digest,
            size_bytes=len(data),
            backend=self.name,
            bucket=self.bucket,
            object_key=object_key,
        )

    def put_file(
        self,
        source_path: Path,
        digest: str,
        size_bytes: int,
        content_type: str | None = None,
    ) -> StoredBlob:
        del content_type
        if source_path.stat().st_size != size_bytes:
            raise StorageError("Source file size changed before storage write")
        object_key = object_key_for_hash("sha256", digest)
        target = self._object_path(object_key)
        target.parent.mkdir(parents=True, exist_ok=True)
        if not target.exists():
            temp_path = target.with_name(f"{target.name}.tmp-{uuid.uuid4().hex}")
            try:
                with source_path.open("rb") as source, temp_path.open("xb") as output:
                    shutil.copyfileobj(source, output, STORAGE_CHUNK_SIZE)
                temp_path.replace(target)
            except Exception:
                temp_path.unlink(missing_ok=True)
                raise
        return StoredBlob(
            hash_algo="sha256",
            digest=digest,
            size_bytes=size_bytes,
            backend=self.name,
            bucket=self.bucket,
            object_key=object_key,
        )

    def read_bytes(self, object_key: str, bucket: str | None = None) -> bytes:
        del bucket
        with self.open_reader(object_key) as reader:
            return reader.read()

    @contextmanager
    def open_reader(
        self,
        object_key: str,
        bucket: str | None = None,
    ) -> Iterator[BlobReader]:
        del bucket
        target = self._object_path(object_key)
        if not target.exists() or not target.is_file():
            raise StorageNotFoundError("Blob missing from storage")
        with target.open("rb") as source:
            yield source

    def list_object_keys(self) -> list[str]:
        self.ensure()
        keys: list[str] = []
        for path in self.root.rglob("*"):
            if not path.is_file() or path.name.startswith(".vault-storage.lock"):
                continue
            keys.append(str(path.relative_to(self.root)).replace("\\", "/"))
        return sorted(keys)

    def delete_object(self, object_key: str) -> None:
        target = self._object_path(object_key)
        if target.exists() and target.is_file():
            target.unlink()


class S3CompatibleBlobStorage(BlobStorageBackend):
    def __init__(
        self,
        *,
        name: str,
        bucket: str,
        region: str = S3_REGION,
        endpoint_url: str | None = None,
        access_key_id: str | None = None,
        secret_access_key: str | None = None,
        session_token: str | None = None,
    ) -> None:
        if not bucket:
            raise StorageConfigurationError(
                f"VAULT_{name.upper()}_BUCKET is required for {name} storage",
            )
        self.name = name
        self.bucket = bucket
        self.region = region
        self.endpoint_url = endpoint_url
        self.access_key_id = access_key_id
        self.secret_access_key = secret_access_key
        self.session_token = session_token
        self._client = None

    @property
    def client(self) -> Any:
        if self._client is None:
            try:
                import boto3  # type: ignore[import-untyped]
            except ImportError as exc:  # pragma: no cover - depends on optional package install
                raise StorageConfigurationError("Install boto3 to use s3 or r2 storage") from exc
            kwargs: dict[str, Any] = {"region_name": self.region}
            if self.endpoint_url:
                kwargs["endpoint_url"] = self.endpoint_url
            if self.access_key_id:
                kwargs["aws_access_key_id"] = self.access_key_id
            if self.secret_access_key:
                kwargs["aws_secret_access_key"] = self.secret_access_key
            if self.session_token:
                kwargs["aws_session_token"] = self.session_token
            self._client = boto3.client("s3", **kwargs)
        return self._client

    def put_bytes(self, data: bytes, content_type: str | None = None) -> StoredBlob:
        digest = hashlib.sha256(data).hexdigest()
        object_key = object_key_for_hash("sha256", digest)
        try:
            self.client.head_object(Bucket=self.bucket, Key=object_key)
        except Exception:
            kwargs = {"Bucket": self.bucket, "Key": object_key, "Body": data}
            if content_type:
                kwargs["ContentType"] = content_type
            self.client.put_object(**kwargs)
        return StoredBlob(
            hash_algo="sha256",
            digest=digest,
            size_bytes=len(data),
            backend=self.name,
            bucket=self.bucket,
            object_key=object_key,
        )

    def put_file(
        self,
        source_path: Path,
        digest: str,
        size_bytes: int,
        content_type: str | None = None,
    ) -> StoredBlob:
        if source_path.stat().st_size != size_bytes:
            raise StorageError("Source file size changed before storage write")
        object_key = object_key_for_hash("sha256", digest)
        try:
            self.client.head_object(Bucket=self.bucket, Key=object_key)
        except Exception:
            upload_kwargs: dict[str, Any] = {}
            if content_type:
                upload_kwargs["ExtraArgs"] = {"ContentType": content_type}
            with source_path.open("rb") as source:
                self.client.upload_fileobj(source, self.bucket, object_key, **upload_kwargs)
        return StoredBlob(
            hash_algo="sha256",
            digest=digest,
            size_bytes=size_bytes,
            backend=self.name,
            bucket=self.bucket,
            object_key=object_key,
        )

    def read_bytes(self, object_key: str, bucket: str | None = None) -> bytes:
        with self.open_reader(object_key, bucket) as reader:
            return reader.read()

    @contextmanager
    def open_reader(
        self,
        object_key: str,
        bucket: str | None = None,
    ) -> Iterator[BlobReader]:
        try:
            response = self.client.get_object(Bucket=bucket or self.bucket, Key=object_key)
        except Exception as exc:
            raise StorageNotFoundError("Blob missing from storage") from exc
        body = response["Body"]
        try:
            yield body
        finally:
            body.close()

    def list_object_keys(self) -> list[str]:
        raise StorageConfigurationError("Object listing is only implemented for local storage")

    def delete_object(self, object_key: str) -> None:
        del object_key
        raise StorageConfigurationError("Object deletion is only implemented for local storage")


def _build_backend(name: str) -> BlobStorageBackend:
    if name == "local":
        return LocalBlobStorage()
    if name == "s3":
        return S3CompatibleBlobStorage(
            name="s3",
            bucket=S3_BUCKET,
            region=S3_REGION,
            endpoint_url=S3_ENDPOINT_URL,
            access_key_id=S3_ACCESS_KEY_ID,
            secret_access_key=S3_SECRET_ACCESS_KEY,
            session_token=S3_SESSION_TOKEN,
        )
    if name == "r2":
        return S3CompatibleBlobStorage(
            name="r2",
            bucket=R2_BUCKET,
            region="auto",
            endpoint_url=R2_ENDPOINT_URL,
            access_key_id=R2_ACCESS_KEY_ID,
            secret_access_key=R2_SECRET_ACCESS_KEY,
        )
    raise StorageConfigurationError(f"Unsupported VAULT_STORAGE_BACKEND: {name}")


_backend_cache: dict[str, BlobStorageBackend] = {}


def configure_storage(
    *,
    backend: str = "local",
    objects_path: str | Path | None = None,
    prefix: str = "objects",
    s3_bucket: str = "",
    s3_region: str = "us-east-1",
    s3_endpoint_url: str | None = None,
    s3_access_key_id: str = "",
    s3_secret_access_key: str = "",
    s3_session_token: str = "",
    r2_bucket: str = "",
    r2_endpoint_url: str | None = None,
    r2_access_key_id: str = "",
    r2_secret_access_key: str = "",
) -> None:
    """Configure process-local blob storage globals."""
    global FILES_LOCK_PATH
    global OBJECTS_PATH, STORAGE_BACKEND, STORAGE_PREFIX
    global S3_ACCESS_KEY_ID, S3_BUCKET, S3_ENDPOINT_URL
    global S3_REGION, S3_SECRET_ACCESS_KEY, S3_SESSION_TOKEN
    global R2_ACCESS_KEY_ID, R2_BUCKET, R2_ENDPOINT_URL, R2_SECRET_ACCESS_KEY

    from . import config

    STORAGE_BACKEND = backend.strip().lower() or "local"
    STORAGE_PREFIX = prefix.strip().strip("/")
    if objects_path is not None:
        OBJECTS_PATH = Path(objects_path).resolve()
    S3_BUCKET = s3_bucket.strip()
    S3_REGION = s3_region.strip() or "us-east-1"
    S3_ENDPOINT_URL = s3_endpoint_url.strip() if s3_endpoint_url else None
    S3_ACCESS_KEY_ID = s3_access_key_id.strip()
    S3_SECRET_ACCESS_KEY = s3_secret_access_key.strip()
    S3_SESSION_TOKEN = s3_session_token.strip()
    R2_BUCKET = r2_bucket.strip()
    R2_ENDPOINT_URL = r2_endpoint_url.strip() if r2_endpoint_url else None
    R2_ACCESS_KEY_ID = r2_access_key_id.strip()
    R2_SECRET_ACCESS_KEY = r2_secret_access_key.strip()

    FILES_LOCK_PATH = OBJECTS_PATH / ".vault-storage.lock"
    _backend_cache.clear()

    config.STORAGE_BACKEND = STORAGE_BACKEND
    config.STORAGE_PREFIX = STORAGE_PREFIX
    config.OBJECTS_PATH = OBJECTS_PATH
    config.S3_BUCKET = S3_BUCKET
    config.S3_REGION = S3_REGION
    config.S3_ENDPOINT_URL = S3_ENDPOINT_URL
    config.S3_ACCESS_KEY_ID = S3_ACCESS_KEY_ID
    config.S3_SECRET_ACCESS_KEY = S3_SECRET_ACCESS_KEY
    config.S3_SESSION_TOKEN = S3_SESSION_TOKEN
    config.R2_BUCKET = R2_BUCKET
    config.R2_ENDPOINT_URL = R2_ENDPOINT_URL
    config.R2_ACCESS_KEY_ID = R2_ACCESS_KEY_ID
    config.R2_SECRET_ACCESS_KEY = R2_SECRET_ACCESS_KEY


def get_storage_backend(name: str | None = None) -> BlobStorageBackend:
    backend_name = (name or STORAGE_BACKEND or "local").strip().lower()
    if backend_name not in _backend_cache:
        _backend_cache[backend_name] = _build_backend(backend_name)
    return _backend_cache[backend_name]


def ensure_storage() -> None:
    """Ensure the configured storage backend can accept writes."""
    get_storage_backend().ensure()


def _acquire_process_lock(lock_file: Any) -> None:
    if fcntl is not None:
        fcntl.flock(lock_file, fcntl.LOCK_EX)
        return

    lock_file.truncate(1)
    lock_file.flush()
    lock_file.seek(0)
    msvcrt.locking(lock_file.fileno(), msvcrt.LK_LOCK, 1)


def _release_process_lock(lock_file: Any) -> None:
    if fcntl is not None:
        fcntl.flock(lock_file, fcntl.LOCK_UN)
        return

    lock_file.seek(0)
    msvcrt.locking(lock_file.fileno(), msvcrt.LK_UNLCK, 1)


@contextmanager
def storage_write_lock() -> Iterator[None]:
    """Cross-process, cross-thread lock for metadata and blob mutations."""
    ensure_storage()
    FILES_LOCK_PATH.parent.mkdir(parents=True, exist_ok=True)
    with storage_lock:
        lock_file = FILES_LOCK_PATH.open("a+b")
        try:
            _acquire_process_lock(lock_file)
            yield
        finally:
            _release_process_lock(lock_file)
            lock_file.close()


def new_version_id() -> str:
    """Generate a stable public version identifier."""
    timestamp = datetime.datetime.now(tz=datetime.UTC).strftime("%Y%m%d%H%M%S%f")
    return f"{timestamp}-{uuid.uuid4().hex[:8]}"
