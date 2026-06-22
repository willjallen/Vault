"""Application version resolution."""

import os
from pathlib import Path

_VERSION_FILE = Path(__file__).resolve().parent.parent / "VERSION"
_FALLBACK_VERSION = "0.0.0-dev"


def _read_version_file() -> str:
    try:
        return _VERSION_FILE.read_text(encoding="utf-8").strip()
    except OSError:
        return ""


SOURCE_VERSION = _read_version_file() or _FALLBACK_VERSION
APP_VERSION = os.getenv("VAULT_VERSION", "").strip() or SOURCE_VERSION
