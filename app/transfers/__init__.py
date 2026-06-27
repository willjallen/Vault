"""Active transfer state coordination."""

from pathlib import Path

from .engine import TransferEngine
from .file_store import FileTransferStateStore

_store: FileTransferStateStore | None = None
_engine: TransferEngine | None = None


def configure_transfer_store(root: str | Path) -> None:
    """Configure the process-local active transfer state store."""
    global _engine, _store
    if _engine is not None:
        _engine.close()
    _store = FileTransferStateStore(Path(root).resolve())
    _engine = TransferEngine(_store)


def get_transfer_store() -> FileTransferStateStore:
    if _store is None:
        from app.config import TRANSFERS_PATH

        configure_transfer_store(TRANSFERS_PATH / "uploads")
    if _store is None:
        raise RuntimeError("Transfer store is not configured")
    return _store


def get_transfer_engine() -> TransferEngine:
    if _engine is None:
        get_transfer_store()
    if _engine is None:
        raise RuntimeError("Transfer engine is not configured")
    return _engine
