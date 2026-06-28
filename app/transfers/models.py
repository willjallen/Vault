"""Typed active-transfer records stored outside the canonical metadata DB."""

from __future__ import annotations

import datetime as dt
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class TransferPart:
    part_number: int
    offset: int
    size_bytes: int
    sha256: str | None
    path: Path

    def payload(self) -> dict[str, object]:
        return {
            "part_number": self.part_number,
            "offset": self.offset,
            "size_bytes": self.size_bytes,
            "sha256": self.sha256,
        }


@dataclass(frozen=True)
class TransferProgress:
    processed_bytes: int
    total_bytes: int

    def payload(self) -> dict[str, int]:
        return {
            "processed_bytes": min(self.processed_bytes, self.total_bytes),
            "total_bytes": self.total_bytes,
        }


@dataclass(frozen=True)
class CompletedAssembly:
    part_paths: tuple[Path, ...]
    size_bytes: int
    sha256: str


@dataclass(frozen=True)
class TransferSession:
    id: str
    owner_id: str
    mode: str
    filename: str
    total_size: int
    chunk_size: int
    part_count: int
    expires_at: dt.datetime | None
    created_at: dt.datetime

    def expected_part_bounds(self, part_number: int) -> tuple[int, int]:
        if part_number < 1 or part_number > self.part_count:
            raise InvalidTransferPart("Invalid part number")
        offset = (part_number - 1) * self.chunk_size
        size = min(self.chunk_size, self.total_size - offset)
        return offset, size


class TransferStoreError(Exception):
    """Base active-transfer store error."""


class TransferNotFound(TransferStoreError):
    """The active transfer session does not exist."""


class TransferConflict(TransferStoreError):
    """The active transfer state conflicts with the request."""


class InvalidTransferPart(TransferStoreError):
    """The requested transfer part is invalid."""
