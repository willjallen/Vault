# Copyright (c) 2024 The Allen Family
"""Filesystem helpers for the vault repository."""

import datetime
import threading
import uuid
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from pathlib import Path

from fastapi import HTTPException

from .config import FILES_PATH
from .models import Document

storage_lock = threading.Lock()
FILES_LOCK_PATH = FILES_PATH / ".vault-files.lock"

try:
    import fcntl
except ImportError:  # pragma: no cover - exercised on Windows only
    fcntl = None
    import msvcrt


def ensure_storage() -> None:
    """Ensure the repository root exists on disk."""
    FILES_PATH.mkdir(parents=True, exist_ok=True)


def _acquire_process_lock(lock_file: object) -> None:
    if fcntl is not None:
        fcntl.flock(lock_file, fcntl.LOCK_EX)
        return

    lock_file.truncate(1)
    lock_file.flush()
    lock_file.seek(0)
    msvcrt.locking(lock_file.fileno(), msvcrt.LK_LOCK, 1)


def _release_process_lock(lock_file: object) -> None:
    if fcntl is not None:
        fcntl.flock(lock_file, fcntl.LOCK_UN)
        return

    lock_file.seek(0)
    msvcrt.locking(lock_file.fileno(), msvcrt.LK_UNLCK, 1)


@contextmanager
def storage_write_lock() -> Iterator[None]:
    """Cross-process, cross-thread lock for repository mutations."""
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
    """Generate a filesystem-friendly version identifier."""
    timestamp = datetime.datetime.now(tz=datetime.UTC).strftime("%Y%m%d%H%M%S%f")
    return f"{timestamp}-{uuid.uuid4().hex[:8]}"


def safe_path(rel_path: str) -> Path:
    """Normalize and validate a relative repository path."""
    cleaned = rel_path.strip().lstrip("/").replace("\\", "/")
    if not cleaned:
        raise HTTPException(status_code=400, detail="Path cannot be empty")
    parts = [part for part in cleaned.split("/") if part]
    if parts and parts[0] == ".versions":
        raise HTTPException(status_code=400, detail="Invalid path")
    dest = (FILES_PATH / cleaned).resolve()
    if FILES_PATH not in dest.parents and dest != FILES_PATH:
        raise HTTPException(status_code=400, detail="Invalid path")
    return dest


def version_dir(doc_id: int) -> Path:
    """Return the directory that contains all versions for a document."""
    return FILES_PATH / ".versions" / str(doc_id)


def version_file_path(doc: Document, version_id: str, filename: str | None = None) -> Path:
    """Build the path to a specific version file for a document."""
    suffix = Path(filename or doc.path).suffix
    version_filename = f"{version_id}{suffix}" if suffix else version_id
    return version_dir(doc.id) / version_filename


class StagedChange:
    """Holds rollback/finalize callbacks for a staged filesystem change."""

    def __init__(self, rollback: Callable[[], None], finalize: Callable[[], None]) -> None:
        self.rollback = rollback
        self.finalize = finalize


def stage_write(target: Path, data: bytes) -> StagedChange:
    """Write data to a temporary file and swap it into place on finalize."""
    ensure_storage()
    target.parent.mkdir(parents=True, exist_ok=True)
    temp_path = target.with_name(f"{target.name}.tmp-{uuid.uuid4().hex}")
    temp_path.write_bytes(data)

    def rollback() -> None:
        if temp_path.exists():
            temp_path.unlink(missing_ok=True)

    def finalize() -> None:
        backup_path: Path | None = None
        if target.exists():
            backup_path = target.with_name(f"{target.name}.bak-{uuid.uuid4().hex}")
            target.replace(backup_path)
        try:
            temp_path.replace(target)
        except Exception:
            if backup_path and backup_path.exists():
                backup_path.replace(target)
            raise
        if backup_path and backup_path.exists():
            backup_path.unlink()

    return StagedChange(rollback, finalize)


def stage_move(source: Path, target: Path) -> StagedChange:
    """Stage a move to a new location and apply it on finalize."""
    ensure_storage()
    target.parent.mkdir(parents=True, exist_ok=True)
    if not source.exists():
        raise HTTPException(status_code=404, detail="Source path missing")
    if target.exists():
        raise HTTPException(status_code=400, detail="Target already exists")

    def rollback() -> None:
        # No changes have been applied yet; nothing to undo.
        return None

    def finalize() -> None:
        if not source.exists():
            raise HTTPException(status_code=404, detail="Source path missing")
        if target.exists():
            raise HTTPException(status_code=400, detail="Target already exists")
        target.parent.mkdir(parents=True, exist_ok=True)
        source.replace(target)

    return StagedChange(rollback, finalize)
