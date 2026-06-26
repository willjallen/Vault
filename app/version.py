"""Application version resolution."""

from pathlib import Path

_VERSION_FILE = Path(__file__).resolve().parent.parent / "VERSION"


def _read_version_file() -> str:
    try:
        version = _VERSION_FILE.read_text(encoding="utf-8").strip()
    except OSError as exc:
        raise RuntimeError("VERSION file is required") from exc
    if not version:
        raise RuntimeError("VERSION file is empty")
    return version


APP_VERSION = _read_version_file()
