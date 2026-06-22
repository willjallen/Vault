# Copyright (c) 2024 The Allen Family
"""Storage backends for content-addressed vault blobs."""

import datetime
import hashlib
import threading
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from fastapi import HTTPException

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

msvcrt: Any = None
try:
    import fcntl as _fcntl

    fcntl: Any = _fcntl
except ImportError:  # pragma: no cover - exercised on Windows only
    fcntl = None
    import msvcrt as _msvcrt

    msvcrt = _msvcrt


@dataclass(frozen=True)
class StoredBlob:
    hash_algo: str
    digest: str
    size_bytes: int
    backend: str
    bucket: str
    object_key: str
    mime_type: str | None = None


class BlobStorageBackend:
    name: str
    bucket: str

    def put_bytes(self, data: bytes, mime_type: str | None = None) -> StoredBlob:
        raise NotImplementedError

    def read_bytes(self, object_key: str, bucket: str | None = None) -> bytes:
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

    def __init__(self, root: Path = OBJECTS_PATH) -> None:
        self.root = root

    def ensure(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)

    def _object_path(self, object_key: str) -> Path:
        cleaned = object_key.strip().lstrip("/").replace("\\", "/")
        target = (self.root / cleaned).resolve()
        if self.root not in target.parents and target != self.root:
            raise HTTPException(status_code=400, detail="Invalid object key")
        return target

    def put_bytes(self, data: bytes, mime_type: str | None = None) -> StoredBlob:
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
            mime_type=mime_type,
        )

    def read_bytes(self, object_key: str, bucket: str | None = None) -> bytes:
        target = self._object_path(object_key)
        if not target.exists() or not target.is_file():
            raise HTTPException(status_code=404, detail="Blob missing from storage")
        return target.read_bytes()


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
            raise RuntimeError(f"VAULT_{name.upper()}_BUCKET is required for {name} storage")
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
                raise RuntimeError("Install boto3 to use s3 or r2 storage") from exc
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

    def put_bytes(self, data: bytes, mime_type: str | None = None) -> StoredBlob:
        digest = hashlib.sha256(data).hexdigest()
        object_key = object_key_for_hash("sha256", digest)
        try:
            self.client.head_object(Bucket=self.bucket, Key=object_key)
        except Exception:
            kwargs = {"Bucket": self.bucket, "Key": object_key, "Body": data}
            if mime_type:
                kwargs["ContentType"] = mime_type
            self.client.put_object(**kwargs)
        return StoredBlob(
            hash_algo="sha256",
            digest=digest,
            size_bytes=len(data),
            backend=self.name,
            bucket=self.bucket,
            object_key=object_key,
            mime_type=mime_type,
        )

    def read_bytes(self, object_key: str, bucket: str | None = None) -> bytes:
        try:
            response = self.client.get_object(Bucket=bucket or self.bucket, Key=object_key)
            return response["Body"].read()
        except Exception as exc:
            raise HTTPException(status_code=404, detail="Blob missing from storage") from exc


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
    raise RuntimeError(f"Unsupported VAULT_STORAGE_BACKEND: {name}")


_backend_cache: dict[str, BlobStorageBackend] = {}


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
