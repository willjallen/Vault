"""Vault-wide settings and validation."""

import datetime as dt

from sqlalchemy import select
from sqlalchemy.orm import Session

from .models import VaultSetting

SITE_SETTING_DEFAULTS: dict[str, object] = {
    "archivePermanentDeleteAdminOnly": True,
}
BOOLEAN_SITE_SETTINGS = {"archivePermanentDeleteAdminOnly"}


def normalize_site_settings(raw: object) -> dict[str, object]:
    normalized = dict(SITE_SETTING_DEFAULTS)
    if not isinstance(raw, dict):
        return normalized
    for key in BOOLEAN_SITE_SETTINGS:
        value = raw.get(key)
        if isinstance(value, bool):
            normalized[key] = value
    return normalized


def clean_site_setting_patch(raw: object) -> dict[str, object]:
    if not isinstance(raw, dict):
        raise ValueError("Settings must be an object")
    cleaned: dict[str, object] = {}
    for key, value in raw.items():
        if key not in SITE_SETTING_DEFAULTS:
            raise ValueError(f"Unknown setting: {key}")
        if key in BOOLEAN_SITE_SETTINGS:
            if not isinstance(value, bool):
                raise ValueError(f"{key} must be a boolean")
            cleaned[key] = value
    return cleaned


def site_settings_for_db(db: Session) -> dict[str, object]:
    rows = db.execute(select(VaultSetting)).scalars().all()
    return normalize_site_settings({row.key: row.value for row in rows})


def merge_site_settings(db: Session, patch: dict[str, object]) -> dict[str, object]:
    cleaned = clean_site_setting_patch(patch)
    existing = {row.key: row for row in db.execute(select(VaultSetting)).scalars().all()}
    timestamp = dt.datetime.now(tz=dt.UTC)
    for key, value in cleaned.items():
        row = existing.get(key)
        if row:
            row.value = value
            row.updated_at = timestamp
        else:
            db.add(VaultSetting(key=key, value=value, updated_at=timestamp))
    db.flush()
    return site_settings_for_db(db)


def archive_permanent_delete_admin_only(db: Session) -> bool:
    return bool(site_settings_for_db(db)["archivePermanentDeleteAdminOnly"])
