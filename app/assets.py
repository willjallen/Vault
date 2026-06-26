"""Static asset manifest helpers."""

import json
from functools import lru_cache
from pathlib import Path

STATIC_DIR = Path(__file__).parent / "static"
DIST_DIR = STATIC_DIR / "dist"
MANIFEST_PATH = DIST_DIR / "manifest.json"
REQUIRED_ASSETS = {"app.js", "styles.css"}


@lru_cache
def static_asset_manifest() -> dict[str, str]:
    try:
        raw_manifest = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise RuntimeError(
            "Static asset manifest is missing or invalid. Run npm run build:assets."
        ) from exc
    if not isinstance(raw_manifest, dict):
        raise RuntimeError("Static asset manifest must be a JSON object")
    manifest: dict[str, str] = {}
    for key, value in raw_manifest.items():
        if not isinstance(key, str) or not isinstance(value, str):
            raise RuntimeError("Static asset manifest entries must be strings")
        if not value.startswith("/static/dist/") or "://" in value:
            raise RuntimeError("Static asset manifest must reference local dist assets")
        manifest[key] = value
    missing_assets = REQUIRED_ASSETS - set(manifest)
    if missing_assets:
        missing = ", ".join(sorted(missing_assets))
        raise RuntimeError(f"Static asset manifest is missing required entries: {missing}")
    return manifest


def static_asset_path(name: str) -> str:
    manifest = static_asset_manifest()
    try:
        return manifest[name]
    except KeyError as exc:
        raise RuntimeError(f"Static asset manifest is missing {name}") from exc


def validate_static_assets() -> None:
    manifest = static_asset_manifest()
    for name, url in manifest.items():
        relative_path = url.removeprefix("/static/")
        asset_path = STATIC_DIR / relative_path
        try:
            asset_stat = asset_path.stat()
        except OSError as exc:
            raise RuntimeError(f"Static asset {name} does not exist: {url}") from exc
        if not asset_path.is_file() or asset_stat.st_size <= 0:
            raise RuntimeError(f"Static asset {name} is empty or invalid: {url}")


def clear_static_asset_cache() -> None:
    static_asset_manifest.cache_clear()
