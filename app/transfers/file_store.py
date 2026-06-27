"""Filesystem-backed active transfer state with an in-process hot cache."""

from __future__ import annotations

import datetime as dt
import json
import shutil
import threading
import uuid
from pathlib import Path
from typing import Any

from .models import (
    InvalidTransferPart,
    TransferConflict,
    TransferNotFound,
    TransferPart,
    TransferProgress,
    TransferSession,
)


class FileTransferStateStore:
    """Store active transfer state on disk and cache it in memory.

    The filesystem sidecars are the restart-safe source of truth. The in-process
    dictionaries are only hot caches and locks.
    """

    def __init__(self, root: Path) -> None:
        self.root = root
        self._lock = threading.RLock()
        self._session_locks: dict[str, threading.RLock] = {}
        self._sessions: dict[str, TransferSession] = {}
        self._parts: dict[str, dict[int, TransferPart]] = {}
        self._progress: dict[str, TransferProgress] = {}

    def session_dir(self, session_id: str) -> Path:
        return self.root / session_id

    def parts_dir(self, session_id: str) -> Path:
        return self.session_dir(session_id) / "parts"

    def tmp_dir(self, session_id: str) -> Path:
        return self.session_dir(session_id) / "tmp"

    def part_path(self, session_id: str, part_number: int) -> Path:
        return self.parts_dir(session_id) / f"{part_number:08d}.part"

    def part_metadata_path(self, session_id: str, part_number: int) -> Path:
        return self.parts_dir(session_id) / f"{part_number:08d}.json"

    def session_lock(self, session_id: str) -> threading.RLock:
        with self._lock:
            lock = self._session_locks.get(session_id)
            if lock is None:
                lock = threading.RLock()
                self._session_locks[session_id] = lock
            return lock

    def create_session(
        self,
        *,
        session_id: str,
        owner_id: str,
        mode: str,
        filename: str,
        total_size: int,
        chunk_size: int,
        part_count: int,
        expires_at: dt.datetime | None,
        created_at: dt.datetime,
    ) -> TransferSession:
        state = TransferSession(
            id=session_id,
            owner_id=owner_id,
            mode=mode,
            filename=filename,
            total_size=total_size,
            chunk_size=chunk_size,
            part_count=part_count,
            expires_at=expires_at,
            created_at=created_at,
        )
        with self.session_lock(session_id):
            self.tmp_dir(session_id).mkdir(parents=True, exist_ok=True)
            self.parts_dir(session_id).mkdir(parents=True, exist_ok=True)
            self._write_json_atomic(
                self.session_dir(session_id) / "session.json",
                _session_json(state),
            )
            with self._lock:
                self._sessions[session_id] = state
                self._parts[session_id] = {}
                self._progress.pop(session_id, None)
        return state

    def get_session(self, session_id: str) -> TransferSession:
        with self._lock:
            cached = self._sessions.get(session_id)
        if cached is not None:
            return cached
        session_path = self.session_dir(session_id) / "session.json"
        if not session_path.exists():
            raise TransferNotFound("Transfer session not found")
        state = _session_from_json(self._read_json(session_path))
        with self._lock:
            self._sessions[session_id] = state
        return state

    def list_parts(self, session_id: str) -> list[TransferPart]:
        with self._lock:
            cached = self._parts.get(session_id)
            if cached is not None:
                return [cached[key] for key in sorted(cached)]
        parts = self._scan_parts(session_id)
        with self._lock:
            self._parts[session_id] = parts
        return [parts[key] for key in sorted(parts)]

    def get_part(self, session_id: str, part_number: int) -> TransferPart | None:
        parts = {part.part_number: part for part in self.list_parts(session_id)}
        return parts.get(part_number)

    def put_part_file(
        self,
        *,
        session_id: str,
        part_number: int,
        temp_path: Path,
        offset: int,
        size_bytes: int,
        sha256: str,
    ) -> TransferPart:
        with self.session_lock(session_id):
            existing = self.get_part(session_id, part_number)
            if existing is not None:
                if (
                    existing.offset == offset
                    and existing.size_bytes == size_bytes
                    and existing.sha256 == sha256
                    and existing.path.exists()
                ):
                    return existing
                raise TransferConflict("Upload part already exists with different content")
            final_path = self.part_path(session_id, part_number)
            final_path.parent.mkdir(parents=True, exist_ok=True)
            temp_path.replace(final_path)
            part = TransferPart(
                part_number=part_number,
                offset=offset,
                size_bytes=size_bytes,
                sha256=sha256,
                path=final_path,
            )
            self._write_json_atomic(
                self.part_metadata_path(session_id, part_number),
                _part_json(part),
            )
            with self._lock:
                self._parts.setdefault(session_id, {})[part_number] = part
            return part

    def part_paths(self, session_id: str, part_count: int) -> list[Path]:
        parts = {part.part_number: part for part in self.list_parts(session_id)}
        paths: list[Path] = []
        for part_number in range(1, part_count + 1):
            part = parts.get(part_number)
            if part is None or not part.path.exists():
                raise InvalidTransferPart("Upload session has missing parts")
            paths.append(part.path)
        return paths

    def has_recoverable_parts(
        self,
        *,
        session_id: str,
        total_size: int,
        chunk_size: int,
        part_count: int,
    ) -> bool:
        parts = {part.part_number: part for part in self.list_parts(session_id)}
        for part_number in range(1, part_count + 1):
            part = parts.get(part_number)
            if part is None or not part.path.exists():
                return False
            expected_offset = (part_number - 1) * chunk_size
            expected_size = min(chunk_size, total_size - expected_offset)
            if part.offset != expected_offset or part.size_bytes != expected_size:
                return False
            if part.path.stat().st_size != part.size_bytes:
                return False
        return True

    def set_verification_progress(
        self,
        session_id: str,
        *,
        processed_bytes: int,
        total_bytes: int,
    ) -> None:
        with self._lock:
            self._progress[session_id] = TransferProgress(
                processed_bytes=max(0, processed_bytes),
                total_bytes=max(0, total_bytes),
            )

    def get_verification_progress(self, session_id: str) -> TransferProgress | None:
        with self._lock:
            return self._progress.get(session_id)

    def clear_verification_progress(self, session_id: str) -> None:
        with self._lock:
            self._progress.pop(session_id, None)

    def clear_session(self, session_id: str) -> None:
        with self.session_lock(session_id):
            shutil.rmtree(self.session_dir(session_id), ignore_errors=True)
            with self._lock:
                self._sessions.pop(session_id, None)
                self._parts.pop(session_id, None)
                self._progress.pop(session_id, None)

    def _scan_parts(self, session_id: str) -> dict[int, TransferPart]:
        parts_dir = self.parts_dir(session_id)
        if not parts_dir.exists():
            return {}
        parts: dict[int, TransferPart] = {}
        for metadata_path in sorted(parts_dir.glob("*.json")):
            data = self._read_json(metadata_path)
            part = _part_from_json(data, self.part_path(session_id, int(data["part_number"])))
            if part.path.exists():
                parts[part.part_number] = part
        return parts

    @staticmethod
    def _read_json(path: Path) -> dict[str, Any]:
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise TransferNotFound("Transfer state is unreadable") from exc
        if not isinstance(value, dict):
            raise TransferNotFound("Transfer state is unreadable")
        return value

    @staticmethod
    def _write_json_atomic(path: Path, data: dict[str, object]) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        temp_path = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
        try:
            temp_path.write_text(
                json.dumps(data, separators=(",", ":"), sort_keys=True) + "\n",
                encoding="utf-8",
            )
            temp_path.replace(path)
        finally:
            temp_path.unlink(missing_ok=True)


def _timestamp(value: dt.datetime | None) -> str | None:
    return value.isoformat() if value is not None else None


def _parse_timestamp(value: object) -> dt.datetime | None:
    if value in {None, ""}:
        return None
    if not isinstance(value, str):
        raise TransferNotFound("Transfer state is unreadable")
    try:
        timestamp = dt.datetime.fromisoformat(value)
    except ValueError as exc:
        raise TransferNotFound("Transfer state is unreadable") from exc
    if timestamp.tzinfo is None:
        timestamp = timestamp.replace(tzinfo=dt.UTC)
    return timestamp


def _session_json(session: TransferSession) -> dict[str, object]:
    return {
        "chunk_size": session.chunk_size,
        "created_at": _timestamp(session.created_at),
        "expires_at": _timestamp(session.expires_at),
        "filename": session.filename,
        "id": session.id,
        "mode": session.mode,
        "owner_id": session.owner_id,
        "part_count": session.part_count,
        "total_size": session.total_size,
    }


def _session_from_json(data: dict[str, Any]) -> TransferSession:
    return TransferSession(
        id=str(data["id"]),
        owner_id=str(data["owner_id"]),
        mode=str(data["mode"]),
        filename=str(data["filename"]),
        total_size=int(data["total_size"]),
        chunk_size=int(data["chunk_size"]),
        part_count=int(data["part_count"]),
        expires_at=_parse_timestamp(data.get("expires_at")),
        created_at=_parse_timestamp(data.get("created_at")) or dt.datetime.now(tz=dt.UTC),
    )


def _part_json(part: TransferPart) -> dict[str, object]:
    return {
        "offset": part.offset,
        "part_number": part.part_number,
        "sha256": part.sha256,
        "size_bytes": part.size_bytes,
    }


def _part_from_json(data: dict[str, Any], path: Path) -> TransferPart:
    return TransferPart(
        part_number=int(data["part_number"]),
        offset=int(data["offset"]),
        size_bytes=int(data["size_bytes"]),
        sha256=str(data["sha256"]),
        path=path,
    )
