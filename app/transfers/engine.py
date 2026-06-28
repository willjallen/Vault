"""Active transfer execution outside request route handlers."""

from __future__ import annotations

import asyncio
import hashlib
import os
import queue
import tempfile
import threading
from collections.abc import AsyncIterator
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from starlette.types import Receive

from .file_store import FileTransferStateStore
from .models import (
    CompletedAssembly,
    InvalidTransferPart,
    TransferConflict,
    TransferPart,
    TransferSession,
)

INGEST_BUFFER_BYTES = 4 * 1024 * 1024
INGEST_QUEUE_BUFFERS = 4
ASSEMBLY_READ_BYTES = 4 * 1024 * 1024

BufferItem = bytes | None


@dataclass
class _PartSpoolResult:
    path: Path
    sha256: str | None
    size_bytes: int


@dataclass
class _AssemblyState:
    condition: threading.Condition = field(
        default_factory=lambda: threading.Condition(threading.RLock()),
    )
    next_part_number: int = 1
    size_bytes: int = 0
    digest: hashlib._Hash = field(default_factory=hashlib.sha256)
    running: bool = False
    complete: bool = False
    sha256: str | None = None
    error: Exception | None = None


class TransferEngine:
    """Coordinate hot upload work outside the HTTP route layer."""

    def __init__(self, store: FileTransferStateStore) -> None:
        self.store = store
        self._lock = threading.RLock()
        self._assemblies: dict[str, _AssemblyState] = {}
        self._executor = ThreadPoolExecutor(max_workers=32, thread_name_prefix="vault-transfer")

    def close(self) -> None:
        self._executor.shutdown(wait=False, cancel_futures=True)

    def clear_session(self, session_id: str) -> None:
        with self._lock:
            self._assemblies.pop(session_id, None)
        self.store.clear_session(session_id)

    async def ingest_part_stream(
        self,
        *,
        session: TransferSession,
        part_number: int,
        offset: int,
        expected_size: int,
        expected_sha256: str | None,
        stream: AsyncIterator[bytes],
    ) -> TransferPart:
        result = await self._spool_stream(
            session_id=session.id,
            expected_size=expected_size,
            expected_sha256=expected_sha256,
            stream=stream,
        )
        try:
            part = self.store.put_part_file(
                session_id=session.id,
                part_number=part_number,
                temp_path=result.path,
                offset=offset,
                size_bytes=result.size_bytes,
                sha256=result.sha256,
            )
        except TransferConflict:
            raise
        finally:
            result.path.unlink(missing_ok=True)
        self.schedule_assembly(session)
        return part

    async def ingest_part_receive(
        self,
        *,
        session: TransferSession,
        part_number: int,
        offset: int,
        expected_size: int,
        expected_sha256: str | None,
        receive: Receive,
    ) -> TransferPart:
        result = await self._spool_receive(
            session_id=session.id,
            expected_size=expected_size,
            expected_sha256=expected_sha256,
            receive=receive,
        )
        try:
            part = self.store.put_part_file(
                session_id=session.id,
                part_number=part_number,
                temp_path=result.path,
                offset=offset,
                size_bytes=result.size_bytes,
                sha256=result.sha256,
            )
        except TransferConflict:
            raise
        finally:
            result.path.unlink(missing_ok=True)
        self.schedule_assembly(session)
        return part

    async def _spool_stream(
        self,
        *,
        session_id: str,
        expected_size: int,
        expected_sha256: str | None,
        stream: AsyncIterator[bytes],
    ) -> _PartSpoolResult:
        buffers: queue.Queue[BufferItem] = queue.Queue(maxsize=INGEST_QUEUE_BUFFERS)
        worker = asyncio.get_running_loop().run_in_executor(
            self._executor,
            _write_part_buffers,
            buffers,
            self.store.tmp_dir(session_id),
            expected_sha256 is not None,
        )

        async def enqueue(item: BufferItem) -> None:
            await _enqueue_part_buffer(buffers, worker, item)

        size_bytes = 0
        try:
            async for chunk in stream:
                if not chunk:
                    continue
                size_bytes += len(chunk)
                if size_bytes > expected_size:
                    raise InvalidTransferPart("Upload part is too large")
                await enqueue(chunk)
            await enqueue(None)
            result = await worker
        except Exception:
            await _discard_partial_part(buffers, worker)
            raise
        if result.size_bytes != expected_size:
            result.path.unlink(missing_ok=True)
            raise InvalidTransferPart("Upload part size does not match session")
        if expected_sha256 and result.sha256 != expected_sha256.lower():
            result.path.unlink(missing_ok=True)
            raise InvalidTransferPart("Upload part checksum mismatch")
        return result

    async def _spool_receive(
        self,
        *,
        session_id: str,
        expected_size: int,
        expected_sha256: str | None,
        receive: Receive,
    ) -> _PartSpoolResult:
        buffers: queue.Queue[BufferItem] = queue.Queue(maxsize=INGEST_QUEUE_BUFFERS)
        worker = asyncio.get_running_loop().run_in_executor(
            self._executor,
            _write_part_buffers,
            buffers,
            self.store.tmp_dir(session_id),
            expected_sha256 is not None,
        )

        async def enqueue(item: BufferItem) -> None:
            await _enqueue_part_buffer(buffers, worker, item)

        size_bytes = 0
        try:
            # Upload part PUT is the service's byte-hot path. Consume raw ASGI
            # receive messages instead of Starlette's Request.stream() wrapper so
            # the event loop only produces bounded byte buffers; hashing and file
            # writes run in worker threads below.
            while True:
                message = await receive()
                if message.get("type") == "http.disconnect":
                    raise InvalidTransferPart("Upload disconnected")
                chunk = message.get("body", b"")
                if not isinstance(chunk, bytes):
                    raise InvalidTransferPart("Upload body chunk is invalid")
                if chunk:
                    size_bytes += len(chunk)
                    if size_bytes > expected_size:
                        raise InvalidTransferPart("Upload part is too large")
                    await enqueue(chunk)
                if not bool(message.get("more_body", False)):
                    break
            await enqueue(None)
            result = await worker
        except Exception:
            await _discard_partial_part(buffers, worker)
            raise
        if result.size_bytes != expected_size:
            result.path.unlink(missing_ok=True)
            raise InvalidTransferPart("Upload part size does not match session")
        if expected_sha256 and result.sha256 != expected_sha256.lower():
            result.path.unlink(missing_ok=True)
            raise InvalidTransferPart("Upload part checksum mismatch")
        return result

    def schedule_assembly(self, session: TransferSession) -> None:
        state = self._assembly_state(session)
        with state.condition:
            if state.complete or state.running:
                return
            state.running = True
        self._executor.submit(self._assemble_available_parts, session, state)

    def wait_for_completed_assembly(self, session: TransferSession) -> CompletedAssembly:
        state = self._assembly_state(session)
        self.schedule_assembly(session)
        with state.condition:
            while not state.complete and state.error is None:
                state.condition.wait()
            if state.error is not None:
                raise state.error
            if state.sha256 is None:
                raise InvalidTransferPart("Upload assembly is incomplete")
            return CompletedAssembly(
                part_paths=tuple(self.store.part_paths(session.id, session.part_count)),
                size_bytes=state.size_bytes,
                sha256=state.sha256,
            )

    def _assembly_state(self, session: TransferSession) -> _AssemblyState:
        with self._lock:
            state = self._assemblies.get(session.id)
            if state is None:
                state = _AssemblyState()
                self._assemblies[session.id] = state
            return state

    def _assemble_available_parts(
        self,
        session: TransferSession,
        state: _AssemblyState,
    ) -> None:
        try:
            while True:
                with state.condition:
                    part_number = state.next_part_number
                if part_number > session.part_count:
                    break
                part = self.store.get_part(session.id, part_number)
                if part is None:
                    break
                # Completed local uploads are committed as verified part
                # manifests. This worker only walks contiguous parts in file
                # order to compute the canonical whole-file digest; it avoids
                # rewriting bytes that the part PUT path has already durably
                # accepted.
                written = _hash_part_for_assembly(part.path, state.digest)
                with state.condition:
                    state.size_bytes += written
                    state.next_part_number += 1
                    if state.next_part_number > session.part_count:
                        if state.size_bytes != session.total_size:
                            raise InvalidTransferPart("Upload assembly size mismatch")
                        state.sha256 = state.digest.hexdigest()
                        state.complete = True
                    state.condition.notify_all()
        except Exception as exc:
            with state.condition:
                state.error = exc
                state.condition.notify_all()
        finally:
            should_reschedule = False
            with state.condition:
                state.running = False
                should_reschedule = (
                    not state.complete
                    and state.error is None
                    and self.store.get_part(session.id, state.next_part_number) is not None
                )
                state.condition.notify_all()
            if should_reschedule:
                self.schedule_assembly(session)


async def _enqueue_part_buffer(
    buffers: queue.Queue[BufferItem],
    worker: asyncio.Future[_PartSpoolResult],
    item: BufferItem,
) -> None:
    # Do not use asyncio.to_thread() for queue.put() here. Under many
    # simultaneous upload parts, writer workers can occupy the executor while
    # waiting for queue input; enqueueing must therefore stay nonblocking on the
    # event loop and yield until bounded queue capacity is available.
    while True:
        if worker.done():
            await worker
        try:
            buffers.put_nowait(item)
            return
        except queue.Full:
            await asyncio.sleep(0)


async def _discard_partial_part(
    buffers: queue.Queue[BufferItem],
    worker: asyncio.Future[_PartSpoolResult],
) -> None:
    if not worker.done():
        await _signal_part_writer_stop(buffers, worker)
    try:
        partial = await worker
    except Exception:
        return
    partial.path.unlink(missing_ok=True)


async def _signal_part_writer_stop(
    buffers: queue.Queue[BufferItem],
    worker: asyncio.Future[_PartSpoolResult],
) -> None:
    while not worker.done():
        try:
            buffers.put_nowait(None)
            return
        except queue.Full:
            await asyncio.sleep(0)


def _write_part_buffers(
    buffers: queue.Queue[BufferItem],
    temp_dir: Path,
    hash_part: bool,
) -> _PartSpoolResult:
    temp_dir.mkdir(parents=True, exist_ok=True)
    temp_file = tempfile.NamedTemporaryFile(
        prefix="vault-upload-part-",
        dir=temp_dir,
        delete=False,
    )
    temp_path = Path(temp_file.name)
    digest = hashlib.sha256() if hash_part else None
    size_bytes = 0
    pending: list[bytes] = []
    pending_size = 0

    def flush_pending() -> None:
        nonlocal pending, pending_size
        if pending:
            _write_pending_buffers(temp_file.fileno(), temp_file, pending)
            pending = []
            pending_size = 0

    try:
        with temp_file:
            while True:
                item = buffers.get()
                if item is None:
                    break
                if digest is not None:
                    digest.update(item)
                # Batch existing ASGI body buffers in the worker thread. On
                # POSIX, writev avoids an extra Python bytearray copy before
                # the filesystem write; the route task stays focused on ingress.
                pending.append(item)
                size_bytes += len(item)
                pending_size += len(item)
                if pending_size >= INGEST_BUFFER_BYTES:
                    flush_pending()
            flush_pending()
        return _PartSpoolResult(
            path=temp_path,
            sha256=digest.hexdigest() if digest is not None else None,
            size_bytes=size_bytes,
        )
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise


def _write_pending_buffers(fd: int, fallback_file: Any, buffers: list[bytes]) -> None:
    if hasattr(os, "writev"):
        _writev_all(fd, buffers)
        return
    for item in buffers:
        fallback_file.write(item)


def _writev_all(fd: int, buffers: list[bytes]) -> None:
    pending: list[memoryview] = [memoryview(item) for item in buffers if item]
    while pending:
        written = os.writev(fd, pending)
        if written <= 0:
            raise OSError("writev wrote no bytes")
        while pending and written >= len(pending[0]):
            written -= len(pending[0])
            pending.pop(0)
        if pending and written:
            pending[0] = pending[0][written:]


def _hash_part_for_assembly(
    part_path: Path,
    digest: hashlib._Hash,
) -> int:
    processed = 0
    with part_path.open("rb") as source:
        while True:
            chunk = source.read(ASSEMBLY_READ_BYTES)
            if not chunk:
                break
            digest.update(chunk)
            processed += len(chunk)
    return processed
