"""HTTP routes for the vault service."""

import asyncio
import datetime as dt
import hashlib
import io
import json
import logging
import mimetypes
import re
import secrets
import zipfile
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import quote

from fastapi import APIRouter, Depends, File, Form, HTTPException, Request, Response, UploadFile
from fastapi.responses import HTMLResponse, RedirectResponse, StreamingResponse
from fastapi.templating import Jinja2Templates
from pydantic import BaseModel, Field
from sqlalchemy import delete, select
from sqlalchemy.orm import Session

from . import db as db_runtime
from .auth import (
    UserContext,
    current_user,
    logout_response,
    oidc_callback_response,
    oidc_login_response,
    require_admin,
    vault_user_is_effective_admin,
)
from .config import AUTH_MODE, BASE_DOMAIN, DEV_MODE, SITE_NAME, TTL_SWEEP_INTERVAL_SECONDS
from .db import SessionLocal, get_db
from .models import (
    Blob,
    BlobLocation,
    Document,
    DocumentEvent,
    DocumentLock,
    DocumentVersion,
    Folder,
    FolderEvent,
    FolderPermission,
    ShareLink,
    StateEvent,
    VaultGroup,
    VaultGroupMembership,
    VaultUser,
)
from .preferences import (
    clean_user_preference_patch,
    merge_user_preferences,
    normalize_user_preferences,
)
from .site_settings import (
    archive_permanent_delete_admin_only,
    merge_site_settings,
    site_settings_for_db,
)
from .storage import (
    StorageConfigurationError,
    StorageError,
    StorageNotFoundError,
    get_storage_backend,
    new_version_id,
    object_key_for_hash,
    storage_write_lock,
)
from .version import APP_VERSION

templates = Jinja2Templates(directory=str(Path(__file__).parent / "templates"))
logger = logging.getLogger(__name__)

router = APIRouter()
ARCHIVE_ROOT = "Archive"
VAULT_ROOT_KEY = "vault"
ARCHIVE_ROOT_KEY = "archive"
ROOT_NAMES = {VAULT_ROOT_KEY: "Vault", ARCHIVE_ROOT_KEY: "Archive"}
FOLDER_COLOR_TOKENS = {"blue", "teal", "green", "amber", "rose", "violet", "slate"}
FOLDER_ICON_PATTERN = re.compile(r"^[a-z0-9][a-z0-9-]{0,63}$")
SHARE_CODE_PATTERN = re.compile(r"^[A-Za-z0-9_-]{8,64}$")
TTL_ACTIONS = {"archive", "delete"}
APPEARANCE_PALETTES = {"cozy", "winui"}
APPEARANCE_THEMES = {"system", "light", "dark"}
SYSTEM_USER: UserContext = {
    "id": "system",
    "vault_user_id": 0,
    "issuer": "system",
    "subject": "system",
    "name": "System",
    "email": "",
    "groups": [],
    "is_admin": True,
}
SYSTEM_META = {"ip": None, "user_agent": None}
_ttl_sweeper_task: asyncio.Task[None] | None = None
_debug_event_stream_generation = 0
_debug_event_stream_retry_ms = 3000


def configure_router_runtime(
    *,
    auth_mode: str | None = None,
    base_domain: str | None = None,
    dev_mode: bool | None = None,
    site_name: str | None = None,
    ttl_sweep_interval_seconds: int | None = None,
) -> None:
    """Configure process-local route globals that are normally loaded from env."""
    global AUTH_MODE, BASE_DOMAIN, DEV_MODE, SITE_NAME, TTL_SWEEP_INTERVAL_SECONDS
    global _debug_event_stream_generation, _debug_event_stream_retry_ms

    from . import config

    _debug_event_stream_generation = 0
    _debug_event_stream_retry_ms = 3000
    if auth_mode is not None:
        AUTH_MODE = auth_mode.strip().lower() or "headers"
        config.AUTH_MODE = AUTH_MODE
    if dev_mode is not None:
        DEV_MODE = bool(dev_mode)
        config.DEV_MODE = DEV_MODE
    if base_domain is not None:
        BASE_DOMAIN = base_domain.strip() or "localhost"
        config.BASE_DOMAIN = BASE_DOMAIN
    if site_name is not None:
        SITE_NAME = site_name.strip() or "Vault"
        config.SITE_NAME = SITE_NAME
    if ttl_sweep_interval_seconds is not None:
        TTL_SWEEP_INTERVAL_SECONDS = max(10, ttl_sweep_interval_seconds)
        config.TTL_SWEEP_INTERVAL_SECONDS = TTL_SWEEP_INTERVAL_SECONDS


@dataclass(frozen=True)
class DocStat:
    folder: str
    size_bytes: int
    mtime: dt.datetime | None
    latest_by: str | None


@dataclass(frozen=True)
class PublicFolderPath:
    root_key: str
    relative_path: str


@dataclass(frozen=True)
class NormalizedActionItem:
    type: str
    id: int | None = None
    path: str | None = None
    strict_path: bool = False


class ActionItem(BaseModel):
    type: str
    id: int | None = None
    path: str | None = None


class ActionPayload(BaseModel):
    items: list[ActionItem] = Field(default_factory=list)
    destination_folder: str | None = None
    name: str | None = None


class AdminUserUpdate(BaseModel):
    is_admin: bool | None = None
    is_active: bool | None = None


class AdminGroupPayload(BaseModel):
    name: str
    description: str | None = None


class AdminGroupMemberPayload(BaseModel):
    user_id: int


class FolderPropertiesPayload(BaseModel):
    path: str
    color: str | None = None
    icon: str | None = None


class FolderRetentionPayload(BaseModel):
    path: str
    default_ttl_days: int | None = None
    default_ttl_action: str | None = None


class FolderPermissionPayload(BaseModel):
    group_id: int
    can_view: bool = True
    can_read: bool = True
    can_write: bool = False


class FolderPermissionsPayload(BaseModel):
    path: str
    permissions: list[FolderPermissionPayload] = Field(default_factory=list)


class ShareLinkPayload(BaseModel):
    target_type: str
    document_id: int | None = None
    folder_id: int | None = None
    path: str | None = None


class UserPreferencesPayload(BaseModel):
    preferences: dict[str, object] = Field(default_factory=dict)


class AdminSettingsPayload(BaseModel):
    settings: dict[str, object] = Field(default_factory=dict)


class DebugErrorPayload(BaseModel):
    kind: str = "server"


class DebugStateEventPayload(BaseModel):
    resources: list[str] = Field(default_factory=lambda: ["contents", "sidebar", "my_edits"])


DOCUMENT_EVENT_RESOURCES: dict[str, tuple[str, ...]] = {
    "download": ("document_detail",),
    "checkout": ("contents", "document_detail", "my_edits"),
    "lock": ("contents", "document_detail", "my_edits"),
    "release": ("contents", "document_detail", "my_edits"),
    "move": ("contents", "sidebar", "document_detail"),
    "archive": ("contents", "sidebar", "document_detail", "my_edits"),
    "unarchive": ("contents", "sidebar", "document_detail", "my_edits"),
}

VERSION_CHANGE_RESOURCES: dict[str, tuple[str, ...]] = {
    "upload": ("contents", "sidebar", "document_detail"),
    "checkin": ("contents", "document_detail", "my_edits"),
}


def client_meta(request: Request) -> dict[str, str | None]:
    """Extract IP and user agent for auditing."""
    xff = request.headers.get("x-forwarded-for")
    ip = (xff.split(",")[0].strip() if xff else None) or (
        request.client.host if request.client else None
    )
    ua = request.headers.get("user-agent")
    return {"ip": ip, "user_agent": ua}


def now_utc() -> dt.datetime:
    return dt.datetime.now(tz=dt.UTC)


def normalize_folder(folder: str | None) -> str:
    cleaned = (folder or "").strip().replace("\\", "/").strip("/")
    if not cleaned:
        return ""
    parts = [part.strip() for part in cleaned.split("/") if part.strip()]
    if any(part in {".", ".."} or has_control_char(part) for part in parts):
        raise HTTPException(status_code=400, detail="Invalid folder path")
    return "/".join(parts)


def has_control_char(value: str) -> bool:
    return any(ord(char) < 32 or ord(char) == 127 for char in value)


def normalize_item_name(name: str | None, label: str = "Name") -> str:
    cleaned = (name or "").replace("\\", "/").split("/")[-1].strip()
    if not cleaned:
        raise HTTPException(status_code=400, detail=f"{label} is required")
    if cleaned in {".", ".."} or "/" in cleaned or "\\" in cleaned or has_control_char(cleaned):
        raise HTTPException(status_code=400, detail=f"Invalid {label.lower()}")
    return cleaned


def join_path(*parts: str) -> str:
    return "/".join(part.strip("/") for part in parts if part and part.strip("/"))


def parse_public_folder_path(path: str | None) -> PublicFolderPath:
    normalized = normalize_folder(path)
    if normalized == ARCHIVE_ROOT:
        return PublicFolderPath(ARCHIVE_ROOT_KEY, "")
    if normalized.startswith(f"{ARCHIVE_ROOT}/"):
        return PublicFolderPath(ARCHIVE_ROOT_KEY, normalized[len(ARCHIVE_ROOT) + 1 :])
    return PublicFolderPath(VAULT_ROOT_KEY, normalized)


def public_folder_path(root_key: str, relative_path: str) -> str:
    relative = normalize_folder(relative_path)
    if root_key == ARCHIVE_ROOT_KEY:
        return join_path(ARCHIVE_ROOT, relative) if relative else ARCHIVE_ROOT
    return relative


def is_archived_path(path: str | None) -> bool:
    return parse_public_folder_path(path).root_key == ARCHIVE_ROOT_KEY


def ensure_document_upload_folder(folder: str) -> None:
    if is_archived_path(folder):
        raise HTTPException(status_code=400, detail="Upload new documents to Vault")


def ensure_folder_creation_path(folder: str) -> None:
    if is_archived_path(folder):
        raise HTTPException(status_code=400, detail="Create folders in Vault")


def split_document_path(path: str) -> tuple[str, str]:
    cleaned = normalize_folder(path)
    if not cleaned:
        raise HTTPException(status_code=400, detail="Document path is required")
    parts = cleaned.split("/")
    return "/".join(parts[:-1]), normalize_item_name(parts[-1], "File name")


def format_size(size_bytes: int | None) -> str:
    if size_bytes is None:
        return "-"
    units = ["B", "KB", "MB", "GB", "TB"]
    size = float(size_bytes)
    for unit in units:
        if size < 1024 or unit == units[-1]:
            return f"{int(size)} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024
    return f"{size_bytes} B"


def normalize_timestamp(timestamp: dt.datetime | None) -> dt.datetime | None:
    if not timestamp:
        return None
    if timestamp.tzinfo is None:
        return timestamp.replace(tzinfo=dt.UTC)
    return timestamp.astimezone(dt.UTC)


def format_mtime(timestamp: dt.datetime | None) -> str:
    normalized = normalize_timestamp(timestamp)
    if not normalized:
        return "Not updated yet"
    hour = normalized.hour % 12 or 12
    meridiem = "am" if normalized.hour < 12 else "pm"
    return f"{normalized:%b} {normalized.day}, {normalized:%Y} at {hour}:{normalized:%M} {meridiem}"


def all_folders(db: Session) -> list[Folder]:
    return list(db.execute(select(Folder)).scalars().all())


def get_root_folder(db: Session, root_key: str) -> Folder:
    root = (
        db.execute(
            select(Folder).where(
                Folder.root_key == root_key,
                Folder.is_root == True,  # noqa: E712
            ),
        )
        .scalars()
        .first()
    )
    if root:
        return root
    root = Folder(root_key=root_key, parent_id=None, name=ROOT_NAMES[root_key], is_root=True)
    db.add(root)
    db.flush()
    default_root_folder_permissions(db, root)
    return root


def ensure_root_folders(db: Session) -> dict[str, Folder]:
    return {
        VAULT_ROOT_KEY: get_root_folder(db, VAULT_ROOT_KEY),
        ARCHIVE_ROOT_KEY: get_root_folder(db, ARCHIVE_ROOT_KEY),
    }


def user_group_names(user: UserContext) -> set[str]:
    return {group.strip().lower() for group in user.get("groups", []) if group.strip()}


def access_level(can_view: bool, can_read: bool, can_write: bool) -> int:
    if can_view and can_read and can_write:
        return 3
    if can_view and can_read:
        return 2
    if can_view:
        return 1
    return 0


def validate_permission_flags(can_view: bool, can_read: bool, can_write: bool) -> None:
    if can_write and (not can_read or not can_view):
        raise HTTPException(
            status_code=400,
            detail="Write permission requires read and view permission",
        )
    if can_read and not can_view:
        raise HTTPException(status_code=400, detail="Read permission requires view permission")


def default_root_folder_permissions(db: Session, folder: Folder) -> None:
    if folder.id is None:
        db.flush()
    groups = list(db.execute(select(VaultGroup)).scalars().all())
    for group in groups:
        db.add(
            FolderPermission(
                folder_id=folder.id,
                group_id=group.id,
                can_view=True,
                can_read=True,
                can_write=True,
            ),
        )


def folder_ancestor_ids(folder: Folder) -> list[int]:
    ids: list[int] = []
    current: Folder | None = folder
    seen: set[int] = set()
    while current and current.id not in seen:
        seen.add(current.id)
        ids.append(current.id)
        current = current.parent
    return ids


def folder_access_level(folder: Folder, user: UserContext, db: Session) -> int:
    if user["is_admin"]:
        return 3
    ancestor_ids = folder_ancestor_ids(folder)
    permissions = list(
        db.execute(
            select(FolderPermission, VaultGroup)
            .join(VaultGroup, VaultGroup.id == FolderPermission.group_id)
            .where(FolderPermission.folder_id.in_(ancestor_ids)),
        ).all(),
    )
    permissions_by_folder: dict[int, list[tuple[FolderPermission, VaultGroup]]] = defaultdict(list)
    for permission, group in permissions:
        permissions_by_folder[permission.folder_id].append((permission, group))
    groups = user_group_names(user)
    for folder_id in ancestor_ids:
        scoped_permissions = permissions_by_folder.get(folder_id, [])
        if not scoped_permissions:
            continue
        return max(
            (
                access_level(permission.can_view, permission.can_read, permission.can_write)
                for permission, group in scoped_permissions
                if group.name.strip().lower() in groups
            ),
            default=0,
        )
    return 0


def group_access_context(group: VaultGroup) -> UserContext:
    return {
        "id": f"group:{group.id}",
        "vault_user_id": 0,
        "issuer": "group",
        "subject": group.name,
        "name": group.name,
        "email": "",
        "groups": [group.name],
        "is_admin": False,
    }


def archive_access_snapshot(folder: Folder, db: Session) -> dict[str, int]:
    db.flush()
    snapshot: dict[str, int] = {}
    for group in db.execute(select(VaultGroup)).scalars().all():
        level = folder_access_level(folder, group_access_context(group), db)
        if level > 0:
            snapshot[str(group.id)] = level
    return snapshot


def archived_access_level(doc: Document, user: UserContext, db: Session) -> int:
    archive_level = folder_access_level(doc.folder, user, db)
    if archive_level <= 0:
        return 0
    snapshot = doc.archived_access or {}
    groups = user_group_names(user)
    if not groups:
        return 0
    user_groups = db.execute(select(VaultGroup).order_by(VaultGroup.name)).scalars().all()
    source_level = max(
        (
            int(snapshot.get(str(group.id), 0) or 0)
            for group in user_groups
            if group.name.strip().lower() in groups
        ),
        default=0,
    )
    return min(archive_level, source_level)


def document_access_level(doc: Document, user: UserContext, db: Session) -> int:
    if user["is_admin"]:
        return 3
    if document_is_archive(doc):
        return archived_access_level(doc, user, db)
    return folder_access_level(doc.folder, user, db)


def require_folder_access(folder: Folder, user: UserContext, db: Session, level: int) -> None:
    granted = folder_access_level(folder, user, db)
    if granted >= level:
        return
    if granted > 0:
        raise HTTPException(status_code=403, detail="Insufficient folder access")
    raise HTTPException(status_code=404, detail="Folder not found")


def require_document_access(doc: Document, user: UserContext, db: Session, level: int) -> None:
    granted = document_access_level(doc, user, db)
    if granted >= level:
        return
    if granted > 0:
        raise HTTPException(status_code=403, detail="Insufficient document access")
    raise HTTPException(status_code=404, detail="Document not found")


def nearest_existing_folder_for_path(db: Session, path: str | None) -> Folder:
    ref = parse_public_folder_path(path)
    current = get_root_folder(db, ref.root_key)
    if not ref.relative_path:
        return current
    for part in ref.relative_path.split("/"):
        child = find_child_folder(db, current.id, part)
        if not child:
            return current
        current = child
    return current


def require_write_for_folder_path(db: Session, path: str | None, user: UserContext) -> None:
    require_folder_access(nearest_existing_folder_for_path(db, path), user, db, 3)


def build_folder_path_cache(folders: list[Folder]) -> dict[int, str]:
    by_id = {folder.id: folder for folder in folders}
    cache: dict[int, str] = {}

    def compute(folder_id: int, visiting: set[int] | None = None) -> str:
        if folder_id in cache:
            return cache[folder_id]
        visiting = visiting or set()
        folder = by_id[folder_id]
        if folder_id in visiting:
            cache[folder_id] = public_folder_path(folder.root_key, folder.name)
            return cache[folder_id]
        visiting.add(folder_id)
        if folder.is_root or folder.parent_id is None:
            cache[folder_id] = public_folder_path(folder.root_key, "")
            return cache[folder_id]
        parent = by_id.get(folder.parent_id)
        if not parent:
            cache[folder_id] = public_folder_path(folder.root_key, folder.name)
            return cache[folder_id]
        parent_path = compute(parent.id, visiting)
        if folder_id in cache:
            return cache[folder_id]
        cache[folder_id] = join_path(parent_path, folder.name)
        return cache[folder_id]

    for folder in folders:
        compute(folder.id)
    return cache


def folder_relative_path(folder: Folder) -> str:
    if folder.is_root:
        return ""
    parts: list[str] = []
    current: Folder | None = folder
    seen: set[int] = set()
    while current and not current.is_root:
        if current.id in seen:
            break
        seen.add(current.id)
        parts.append(current.name)
        current = current.parent
    return "/".join(reversed(parts))


def folder_path(folder: Folder, cache: dict[int, str] | None = None) -> str:
    if cache is not None:
        fallback = public_folder_path(folder.root_key, folder_relative_path(folder))
        return cache.get(folder.id, fallback)
    return public_folder_path(folder.root_key, folder_relative_path(folder))


def document_folder_path(doc: Document, cache: dict[int, str] | None = None) -> str:
    return folder_path(doc.folder, cache)


def document_path(doc: Document, cache: dict[int, str] | None = None) -> str:
    return join_path(document_folder_path(doc, cache), doc.name)


def folder_is_archive(folder: Folder) -> bool:
    return folder.root_key == ARCHIVE_ROOT_KEY


def document_is_archive(doc: Document) -> bool:
    return folder_is_archive(doc.folder)


def refresh_document_location(doc: Document, db: Session) -> None:
    db.flush()
    db.refresh(doc)
    db.expire(doc, ["folder"])


def refresh_editable_document(doc: Document, db: Session) -> None:
    refresh_document_location(doc, db)
    if document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Restore this file before editing")


def find_child_folder(db: Session, parent_id: int, name: str) -> Folder | None:
    return (
        db.execute(select(Folder).where(Folder.parent_id == parent_id, Folder.name == name))
        .scalars()
        .first()
    )


def get_folder_by_path(db: Session, path: str | None) -> Folder | None:
    ref = parse_public_folder_path(path)
    if ref.root_key == ARCHIVE_ROOT_KEY and ref.relative_path:
        return None
    current = get_root_folder(db, ref.root_key)
    if not ref.relative_path:
        return current
    for part in ref.relative_path.split("/"):
        child = find_child_folder(db, current.id, part)
        if not child:
            return None
        current = child
    return current


def get_or_create_folder_path_with_created(
    db: Session,
    path: str | None,
) -> tuple[Folder, list[Folder]]:
    ref = parse_public_folder_path(path)
    if ref.root_key == ARCHIVE_ROOT_KEY and ref.relative_path:
        raise HTTPException(status_code=400, detail="Archive does not contain folders")
    current = get_root_folder(db, ref.root_key)
    created: list[Folder] = []
    if not ref.relative_path:
        return current, created
    for part in ref.relative_path.split("/"):
        folder = find_child_folder(db, current.id, part)
        if not folder:
            folder = Folder(
                root_key=ref.root_key,
                parent_id=current.id,
                name=part,
                is_root=False,
            )
            folder.parent = current
            db.add(folder)
            db.flush()
            created.append(folder)
        current = folder
    return current, created


def get_or_create_folder_path(db: Session, path: str | None) -> Folder:
    folder, _created = get_or_create_folder_path_with_created(db, path)
    return folder


def sanitize_folder_color(value: str | None) -> str | None:
    normalized = (value or "").strip().lower()
    if not normalized:
        return None
    if normalized not in FOLDER_COLOR_TOKENS:
        raise HTTPException(status_code=400, detail="Invalid folder color")
    return normalized


def sanitize_folder_icon(value: str | None) -> str | None:
    normalized = (value or "").strip().lower()
    if not normalized:
        return None
    if not FOLDER_ICON_PATTERN.fullmatch(normalized):
        raise HTTPException(status_code=400, detail="Invalid folder icon")
    return normalized


def sanitize_ttl_policy(days: int | None, action: str | None) -> tuple[int | None, str | None]:
    normalized_action = (action or "").strip().lower()
    if not normalized_action or normalized_action == "none":
        return None, None
    if normalized_action not in TTL_ACTIONS:
        raise HTTPException(status_code=400, detail="Invalid TTL action")
    if days is None:
        raise HTTPException(status_code=400, detail="TTL days are required")
    if days < 1 or days > 3650:
        raise HTTPException(status_code=400, detail="TTL days must be between 1 and 3650")
    return days, normalized_action


def ttl_policy_payload(folder: Folder) -> dict[str, object | None]:
    return {
        "default_ttl_days": folder.default_ttl_days,
        "default_ttl_action": folder.default_ttl_action or "none",
    }


def direct_folder_ttl_policy(folder: Folder) -> tuple[int | None, str | None]:
    days = folder.default_ttl_days
    action = (folder.default_ttl_action or "").strip().lower()
    if action not in TTL_ACTIONS or not days or days < 1:
        return None, None
    return days, action


def effective_folder_ttl_policy(folder: Folder) -> tuple[int | None, str | None, Folder | None]:
    current: Folder | None = folder
    seen: set[int] = set()
    while current and current.id not in seen:
        seen.add(current.id)
        days, action = direct_folder_ttl_policy(current)
        if days and action:
            if action == "archive" and folder_is_archive(folder):
                return None, None, None
            return days, action, current
        current = current.parent
    return None, None, None


def effective_ttl_policy_payload(folder: Folder) -> dict[str, object | None]:
    days, action, source = effective_folder_ttl_policy(folder)
    return {
        "effective_ttl_days": days,
        "effective_ttl_action": action or "none",
        "effective_ttl_source_id": source.id if source else None,
        "effective_ttl_inherited": bool(source and source.id != folder.id),
    }


def apply_folder_ttl(doc: Document, folder: Folder, timestamp: dt.datetime | None = None) -> None:
    days, action, _source = effective_folder_ttl_policy(folder)
    if action not in TTL_ACTIONS or not days:
        doc.expires_at = None
        doc.expiry_action = None
        return
    if action == "archive" and folder_is_archive(folder):
        doc.expires_at = None
        doc.expiry_action = None
        return
    base = timestamp or now_utc()
    doc.expires_at = base + dt.timedelta(days=days)
    doc.expiry_action = action


def document_expiry_payload(doc: Document) -> dict[str, object | None]:
    expires_at = normalize_timestamp(doc.expires_at)
    return {
        "expires_at": expires_at.isoformat() if expires_at else None,
        "expiry_action": doc.expiry_action,
    }


def record_folder_event(
    folder: Folder,
    user: UserContext,
    event_type: str,
    message: str,
    db: Session,
) -> None:
    db.add(
        FolderEvent(
            folder=folder,
            event_type=event_type,
            actor=user["id"],
            actor_name=user["name"],
            message=message,
        ),
    )


def ensure_unique_folder_name(
    db: Session,
    parent_id: int,
    name: str,
    exclude_folder_id: int | None = None,
) -> None:
    existing = find_child_folder(db, parent_id, name)
    if existing and existing.id != exclude_folder_id:
        raise HTTPException(status_code=400, detail="A folder already exists at that path")


def document_in_folder(
    db: Session,
    folder_id: int,
    name: str,
    exclude_doc_id: int | None = None,
) -> Document | None:
    statement = select(Document).where(Document.folder_id == folder_id, Document.name == name)
    if exclude_doc_id is not None:
        statement = statement.where(Document.id != exclude_doc_id)
    return db.execute(statement).scalars().first()


def ensure_unique_document_path(
    db: Session,
    folder_id: int,
    name: str,
    exclude_doc_id: int | None = None,
) -> None:
    if document_in_folder(db, folder_id, name, exclude_doc_id):
        raise HTTPException(status_code=400, detail="A document already exists at that path")


def folder_children_by_parent(folders: list[Folder]) -> dict[int | None, list[Folder]]:
    children: dict[int | None, list[Folder]] = defaultdict(list)
    for folder in folders:
        children[folder.parent_id].append(folder)
    return children


def subtree_folder_ids(root: Folder, folders: list[Folder]) -> set[int]:
    children = folder_children_by_parent(folders)
    pending = [root.id]
    ids: set[int] = set()
    while pending:
        folder_id = pending.pop()
        if folder_id in ids:
            continue
        ids.add(folder_id)
        pending.extend(child.id for child in children.get(folder_id, []))
    return ids


def docs_in_folder_subtree(db: Session, root: Folder) -> list[Document]:
    ids = subtree_folder_ids(root, all_folders(db))
    return list(db.execute(select(Document).where(Document.folder_id.in_(ids))).scalars().all())


def readable_docs_in_folder_subtree(
    db: Session,
    root: Folder,
    user: UserContext,
) -> list[Document]:
    return [
        doc for doc in docs_in_folder_subtree(db, root) if document_access_level(doc, user, db) >= 2
    ]


def require_folder_subtree_access(
    root: Folder,
    user: UserContext,
    db: Session,
    level: int,
) -> None:
    folder_ids = subtree_folder_ids(root, all_folders(db))
    for folder in db.execute(select(Folder).where(Folder.id.in_(folder_ids))).scalars().all():
        require_folder_access(folder, user, db, level)


def docs_in_unlocked_folder_subtree(db: Session, root: Folder, user: UserContext) -> list[Document]:
    docs = docs_in_folder_subtree(db, root)
    for doc in docs:
        ensure_not_locked_by_other(doc, user, db)
    return docs


def reapply_ttl_for_folder_subtree(folder: Folder, db: Session) -> None:
    for doc in docs_in_folder_subtree(db, folder):
        apply_folder_ttl(doc, doc.folder, doc.latest_modified_at)


def get_active_lock(doc: Document, db: Session) -> DocumentLock | None:
    return (
        db.execute(
            select(DocumentLock).where(
                DocumentLock.document_id == doc.id,
                DocumentLock.is_active == True,  # noqa: E712
            ),
        )
        .scalars()
        .first()
    )


def ensure_not_locked_by_other(
    doc: Document,
    user: UserContext,
    db: Session,
) -> DocumentLock | None:
    lock = get_active_lock(doc, db)
    if lock and lock.locked_by != user["id"] and not user["is_admin"]:
        raise HTTPException(status_code=403, detail="Document is locked by another user")
    return lock


def acquire_document_lock(
    doc: Document,
    user: UserContext,
    meta: dict[str, str | None],
    db: Session,
) -> tuple[DocumentLock, bool]:
    lock = ensure_not_locked_by_other(doc, user, db)
    if lock:
        return lock, False

    lock = DocumentLock(
        document_id=doc.id,
        locked_by=user["id"],
        locked_by_name=user["name"],
        locked_at=now_utc(),
        is_active=True,
        locked_ip=meta.get("ip"),
        locked_user_agent=meta.get("user_agent"),
        force_acquired=False,
    )
    db.add(lock)
    return lock, True


def release_lock(lock: DocumentLock | None, user: UserContext) -> None:
    if lock:
        lock.is_active = False
        lock.released_at = now_utc()
        lock.released_by = user["id"]


def record_event(
    doc: Document,
    user: UserContext,
    event_type: str,
    message: str,
    db: Session,
    meta: dict[str, str | None] | None = None,
    result: str | None = None,
    publish_state: bool = True,
) -> DocumentEvent:
    event = DocumentEvent(
        document_id=doc.id,
        event_type=event_type,
        actor=user["id"],
        actor_name=user["name"],
        message=message,
        result=result or "ok",
        ip=meta.get("ip") if meta else None,
        user_agent=meta.get("user_agent") if meta else None,
    )
    db.add(event)
    if publish_state:
        record_state_change(
            db,
            f"document.{event_type}",
            DOCUMENT_EVENT_RESOURCES.get(event_type, ("document_detail",)),
        )
    return event


def get_document_or_404(doc_id: int, db: Session) -> Document:
    doc = (
        db.execute(
            select(Document).where(Document.id == doc_id).execution_options(populate_existing=True),
        )
        .scalars()
        .first()
    )
    if not doc:
        raise HTTPException(status_code=404, detail="Document not found")
    db.expire(doc, ["folder"])
    return doc


def get_folder_by_id_or_404(folder_id: int, db: Session) -> Folder:
    folder = (
        db.execute(
            select(Folder).where(Folder.id == folder_id).execution_options(populate_existing=True),
        )
        .scalars()
        .first()
    )
    if not folder:
        raise HTTPException(status_code=404, detail="Folder not found")
    db.expire(folder, ["parent"])
    return folder


def get_version_or_404(doc: Document, version_id: str, db: Session) -> DocumentVersion:
    version = (
        db.execute(
            select(DocumentVersion).where(
                DocumentVersion.document_id == doc.id,
                DocumentVersion.id == version_id,
            ),
        )
        .scalars()
        .first()
    )
    if not version:
        raise HTTPException(status_code=404, detail="Version not found")
    return version


def next_version_number(doc: Document, db: Session) -> int:
    latest_number = (
        db.execute(
            select(DocumentVersion.version_number)
            .where(DocumentVersion.document_id == doc.id)
            .order_by(DocumentVersion.version_number.desc())
            .limit(1),
        )
        .scalars()
        .first()
    )
    return (latest_number or 0) + 1


def get_or_create_blob_for_data(
    db: Session,
    data: bytes,
    content_type: str | None,
) -> Blob:
    try:
        stored = get_storage_backend().put_bytes(data, content_type)
    except StorageConfigurationError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc
    except StorageError as exc:
        raise HTTPException(status_code=500, detail="Storage write failed") from exc

    blob = (
        db.execute(
            select(Blob).where(
                Blob.hash_algo == stored.hash_algo,
                Blob.hash == stored.digest,
                Blob.size_bytes == stored.size_bytes,
            ),
        )
        .scalars()
        .first()
    )
    if not blob:
        blob = Blob(
            hash_algo=stored.hash_algo,
            hash=stored.digest,
            size_bytes=stored.size_bytes,
        )
        db.add(blob)
        db.flush()

    location = (
        db.execute(
            select(BlobLocation).where(
                BlobLocation.backend == stored.backend,
                BlobLocation.bucket == stored.bucket,
                BlobLocation.object_key == stored.object_key,
            ),
        )
        .scalars()
        .first()
    )
    if location and location.blob_id != blob.id:
        raise HTTPException(status_code=500, detail="Storage location points at another blob")
    if not location:
        db.add(
            BlobLocation(
                blob_id=blob.id,
                backend=stored.backend,
                bucket=stored.bucket,
                object_key=stored.object_key,
            ),
        )
        db.flush()
    return blob


def create_document_version(
    db: Session,
    doc: Document,
    blob: Blob,
    user: UserContext,
    meta: dict[str, str | None],
    filename: str,
    mime_type: str | None,
    message: str,
    created_via: str,
) -> DocumentVersion:
    version_number = next_version_number(doc, db)
    timestamp = now_utc()
    version = DocumentVersion(
        id=new_version_id(),
        document_id=doc.id,
        blob_id=blob.id,
        version_number=version_number,
        committed_at=timestamp,
        committed_by=user["id"],
        committed_by_name=user["name"],
        message=message,
        mime_type=mime_type,
        original_filename=filename,
        upload_ip=meta.get("ip"),
        upload_user_agent=meta.get("user_agent"),
        created_via=created_via,
    )
    db.add(version)
    doc.current_version_id = version.id
    doc.latest_modified_at = timestamp
    doc.latest_modified_by = user["id"]
    doc.latest_version_number = version_number
    doc.version_count = max(doc.version_count or 0, version_number)
    apply_folder_ttl(doc, doc.folder, timestamp)
    record_state_change(
        db,
        f"document.{created_via}",
        VERSION_CHANGE_RESOURCES.get(created_via, ("contents", "document_detail")),
    )
    return version


def current_version(doc: Document, db: Session) -> DocumentVersion | None:
    if doc.current_version_id:
        version = (
            db.execute(
                select(DocumentVersion).where(
                    DocumentVersion.document_id == doc.id,
                    DocumentVersion.id == doc.current_version_id,
                ),
            )
            .scalars()
            .first()
        )
        if version:
            return version
        raise HTTPException(
            status_code=500,
            detail="Current document version metadata is inconsistent",
        )
    latest = (
        db.execute(
            select(DocumentVersion)
            .where(DocumentVersion.document_id == doc.id)
            .order_by(DocumentVersion.version_number.desc())
            .limit(1),
        )
        .scalars()
        .first()
    )
    if latest:
        raise HTTPException(
            status_code=500,
            detail="Current document version metadata is inconsistent",
        )
    return None


def location_for_blob(blob: Blob) -> BlobLocation:
    locations = list(blob.locations)
    if not locations:
        raise HTTPException(status_code=404, detail="Blob has no storage location")
    try:
        current_backend = get_storage_backend().name
    except StorageConfigurationError:
        current_backend = ""
    for location in locations:
        if location.backend == current_backend:
            return location
    return locations[0]


def blob_bytes_match(blob: Blob, data: bytes) -> bool:
    return (
        blob.hash_algo == "sha256"
        and len(data) == blob.size_bytes
        and hashlib.sha256(data).hexdigest() == blob.hash
    )


def read_version_bytes(version: DocumentVersion) -> bytes:
    location = location_for_blob(version.blob)
    try:
        data = get_storage_backend(location.backend).read_bytes(
            location.object_key,
            location.bucket,
        )
    except StorageNotFoundError as exc:
        raise HTTPException(status_code=404, detail="Blob missing from storage") from exc
    except StorageConfigurationError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc
    if not blob_bytes_match(version.blob, data):
        raise HTTPException(status_code=500, detail="Blob content does not match metadata")
    return data


def read_authorized_version_bytes(
    doc: Document,
    version: DocumentVersion,
    user: UserContext,
    db: Session,
) -> bytes:
    data = read_version_bytes(version)
    refresh_document_location(doc, db)
    require_document_access(doc, user, db, 2)
    return data


def download_response(data: bytes, filename: str, mime_type: str | None = None) -> Response:
    safe_name = (
        "".join(
            "_" if ord(char) < 32 or ord(char) == 127 else char
            for char in filename.replace('"', "")
        ).strip()
        or "download"
    )
    ascii_name = "".join(char if 32 <= ord(char) < 127 else "_" for char in safe_name).strip()
    ascii_name = ascii_name or "download"
    content_type = sanitize_mime_type(mime_type, safe_name)
    disposition = f"attachment; filename=\"{ascii_name}\"; filename*=UTF-8''{quote(safe_name)}"
    return Response(
        content=data,
        media_type=content_type,
        headers={"Content-Disposition": disposition, "Content-Length": str(len(data))},
    )


def sanitize_mime_type(mime_type: str | None, filename: str) -> str:
    fallback = mimetypes.guess_type(filename)[0] or "application/octet-stream"
    candidate = (mime_type or fallback).strip()
    if not candidate:
        return fallback
    if any(ord(char) < 32 or ord(char) == 127 or ord(char) > 126 for char in candidate):
        return fallback
    return candidate


def action_item_payload(item: NormalizedActionItem) -> dict[str, object]:
    payload: dict[str, object] = {"type": item.type}
    if item.id is not None:
        payload["id"] = item.id
    if item.path is not None:
        payload["path"] = normalize_folder(item.path)
    return payload


def item_label(item: NormalizedActionItem) -> str:
    if item.type == "document":
        return f"document:{item.id}"
    if item.id is not None:
        return f"folder:{item.id}"
    return f"folder:{normalize_folder(item.path)}"


def action_result(item: NormalizedActionItem, detail: str | None = None) -> dict[str, object]:
    result = {"item": action_item_payload(item)}
    if detail:
        result["detail"] = detail
    return result


def bulk_result() -> dict[str, list[dict[str, object]]]:
    return {"ok": [], "failed": [], "skipped": []}


def require_action_items(payload: ActionPayload) -> list[ActionItem]:
    if not payload.items:
        raise HTTPException(status_code=400, detail="Select at least one item")
    return payload.items


def normalize_action_items(items: list[ActionItem], db: Session) -> list[NormalizedActionItem]:
    seen: set[str] = set()
    normalized: list[NormalizedActionItem] = []
    for item in items:
        item_type = item.type.strip().lower()
        if item_type == "document":
            if item.id is None:
                raise HTTPException(status_code=400, detail="Document id is required")
            doc = get_document_or_404(item.id, db)
            normalized_item = NormalizedActionItem(type="document", id=doc.id)
            key = item_label(normalized_item)
            if key not in seen:
                seen.add(key)
                normalized.append(normalized_item)
            continue
        if item_type == "folder":
            folder: Folder | None = None
            path = normalize_folder(item.path)
            strict_path = item.id is None
            if item.id is not None:
                if item.id < 1:
                    raise HTTPException(status_code=400, detail="Folder id must be positive")
                folder = db.get(Folder, item.id)
            elif path:
                folder = get_folder_by_path(db, path)
            else:
                raise HTTPException(status_code=400, detail="Folder path is required")
            if not folder:
                detail = f"Folder not found: {path}" if path else "Folder not found"
                raise HTTPException(status_code=404, detail=detail)
            normalized_item = NormalizedActionItem(
                type="folder",
                id=folder.id,
                path=folder_path(folder),
                strict_path=strict_path,
            )
            key = item_label(normalized_item)
            if key not in seen:
                seen.add(key)
                normalized.append(normalized_item)
            continue
        raise HTTPException(status_code=400, detail="Invalid item type")

    return prune_nested_action_items(normalized, db)


def prune_nested_action_items(
    items: list[NormalizedActionItem],
    db: Session,
) -> list[NormalizedActionItem]:
    current_folder_paths: dict[str, str] = {}
    for item in items:
        if item.type != "folder" or item.id is None:
            continue
        try:
            folder = get_folder_by_id_or_404(item.id, db)
        except HTTPException:
            continue
        current_folder_paths[item_label(item)] = folder_path(folder)
    folder_paths = list(current_folder_paths.values())
    pruned: list[NormalizedActionItem] = []
    for item in items:
        if item.type == "folder":
            path = current_folder_paths.get(item_label(item), normalize_folder(item.path))
            if any(path != parent and path.startswith(f"{parent}/") for parent in folder_paths):
                continue
        if item.type == "document":
            try:
                doc = get_document_or_404(item.id or 0, db)
            except HTTPException:
                pruned.append(item)
                continue
            doc_path = document_folder_path(doc)
            if any(
                doc_path == parent or doc_path.startswith(f"{parent}/") for parent in folder_paths
            ):
                continue
        pruned.append(item)
    return pruned


def get_folder_for_action(item: NormalizedActionItem, db: Session) -> Folder:
    folder = db.get(Folder, item.id) if item.id is not None else get_folder_by_path(db, item.path)
    if not folder:
        raise HTTPException(status_code=404, detail="Folder not found")
    db.refresh(folder)
    db.expire(folder, ["parent"])
    if item.strict_path and normalize_folder(item.path) != folder_path(folder):
        raise HTTPException(status_code=404, detail="Folder not found")
    return folder


def batch_state_changed(db: Session, event_type: str) -> None:
    record_state_change(
        db,
        f"batch.{event_type}",
        ("contents", "sidebar", "document_detail", "my_edits", "preferences"),
    )


def archive_doc_item(doc: Document, request: Request, user: UserContext, db: Session) -> str:
    refresh_document_location(doc, db)
    require_document_access(doc, user, db, 3)
    if document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Document is already archived")
    source_path = document_path(doc)
    source_folder_path = document_folder_path(doc)
    source_name = doc.name
    lock = ensure_not_locked_by_other(doc, user, db)
    target_folder = get_root_folder(db, ARCHIVE_ROOT_KEY)
    require_folder_access(target_folder, user, db, 3)
    doc.archived_from_folder = source_folder_path
    doc.archived_original_name = source_name
    doc.archived_access = archive_access_snapshot(doc.folder, db)
    release_lock(lock, user)
    mutate_doc_location(
        doc,
        target_folder,
        source_name,
        user,
        db,
        client_meta(request),
        "archive",
        f"Archived from {source_path}",
        publish_state=False,
        allow_duplicate_name=True,
    )
    return document_path(doc)


def restore_doc_item(doc: Document, request: Request, user: UserContext, db: Session) -> str:
    refresh_document_location(doc, db)
    require_document_access(doc, user, db, 3)
    if not document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Document is not archived")
    source_path = document_path(doc)
    ensure_not_locked_by_other(doc, user, db)
    if doc.archived_from_folder is None or doc.archived_original_name is None:
        raise HTTPException(status_code=400, detail="Archived document is missing restore metadata")
    target_folder_path = normalize_folder(doc.archived_from_folder)
    target_name = normalize_item_name(doc.archived_original_name, "File name")
    require_write_for_folder_path(db, target_folder_path, user)
    target_folder, created_folders = get_or_create_folder_path_with_created(db, target_folder_path)
    del created_folders
    mutate_doc_location(
        doc,
        target_folder,
        target_name,
        user,
        db,
        client_meta(request),
        "unarchive",
        f"Restored to Vault from {source_path}",
        publish_state=False,
    )
    doc.archived_from_folder = None
    doc.archived_original_name = None
    doc.archived_access = None
    return document_path(doc)


def archive_folder_item(source: Folder, request: Request, user: UserContext, db: Session) -> str:
    if source.is_root:
        raise HTTPException(status_code=400, detail="Cannot archive a root folder")
    if folder_is_archive(source):
        raise HTTPException(status_code=400, detail="Folder is already archived")
    require_folder_access(source, user, db, 3)
    require_folder_subtree_access(source, user, db, 3)
    source_folder_ids = subtree_folder_ids(source, all_folders(db))
    docs = docs_in_unlocked_folder_subtree(db, source, user)
    if not docs:
        raise HTTPException(status_code=400, detail="Folder has no files to archive")
    for doc in docs:
        archive_doc_item(doc, request, user, db)
    db.flush()
    db.execute(delete(Folder).where(Folder.id.in_(source_folder_ids)))
    return ARCHIVE_ROOT


def move_doc_item(
    doc: Document,
    destination_folder: str,
    request: Request,
    user: UserContext,
    db: Session,
    name: str | None = None,
) -> str:
    refresh_document_location(doc, db)
    require_document_access(doc, user, db, 3)
    ensure_not_locked_by_other(doc, user, db)
    target_ref = parse_public_folder_path(destination_folder)
    if doc.folder.root_key != target_ref.root_key:
        raise HTTPException(status_code=400, detail="Use archive or restore for Archive moves")
    require_write_for_folder_path(db, destination_folder, user)
    target_folder = get_or_create_folder_path(db, destination_folder)
    target_name = normalize_item_name(name or doc.name, "File name")
    old_path = document_path(doc)
    mutate_doc_location(
        doc,
        target_folder,
        target_name,
        user,
        db,
        client_meta(request),
        "move",
        f"Moved from {old_path} to {join_path(folder_path(target_folder), target_name)}",
        publish_state=False,
        allow_duplicate_name=folder_is_archive(target_folder),
    )
    return document_path(doc)


def move_folder_item(
    source: Folder,
    destination_folder: str,
    user: UserContext,
    db: Session,
    name: str | None = None,
) -> str:
    if source.is_root:
        raise HTTPException(status_code=400, detail="Cannot move a root folder")
    require_folder_access(source, user, db, 3)
    target_ref = parse_public_folder_path(destination_folder)
    if source.root_key != target_ref.root_key:
        raise HTTPException(status_code=400, detail="Use archive or restore for Archive moves")
    source_path = folder_path(source)
    source_parent_path = folder_path(source.parent) if source.parent else ""
    source_name = source.name
    target_name = normalize_item_name(name or source.name, "Folder name")
    target_parent_path = public_folder_path(target_ref.root_key, target_ref.relative_path)
    target_path = join_path(target_parent_path, target_name)
    if target_path == source_path:
        return source_path
    if target_path.startswith(f"{source_path}/"):
        raise HTTPException(status_code=400, detail="Cannot move a folder into itself")
    require_folder_subtree_access(source, user, db, 3)
    docs_in_unlocked_folder_subtree(db, source, user)
    require_write_for_folder_path(db, destination_folder, user)
    target_parent = get_or_create_folder_path(db, destination_folder)
    ensure_unique_folder_name(db, target_parent.id, target_name, source.id)
    source.parent = target_parent
    source.parent_id = target_parent.id
    source.name = target_name
    reapply_ttl_for_folder_subtree(source, db)
    event_type = (
        "rename"
        if source_parent_path == folder_path(target_parent) and target_name != source_name
        else "move"
    )
    message = (
        f"Renamed from {source_name} to {target_name}"
        if event_type == "rename"
        else f"Moved from {source_path} to {target_path}"
    )
    record_folder_event(source, user, event_type, message, db)
    return target_path


def response_detail(exc: HTTPException) -> str:
    detail = exc.detail
    return detail if isinstance(detail, str) else "Action failed"


def version_signature(
    version: DocumentVersion,
) -> tuple[str | None, str | None, str | None, int | None]:
    ts = int(version.committed_at.timestamp()) if version.committed_at else None
    actor = version.committed_by_name or version.committed_by
    return (version.created_via, (version.message or "").strip(), actor, ts)


def event_signature(event: DocumentEvent) -> tuple[str | None, str | None, str | None, int | None]:
    ts = int(event.created_at.timestamp()) if event.created_at else None
    actor = event.actor_name or event.actor
    return (event.event_type, (event.message or "").strip(), actor, ts)


def dedupe_versions_by_checksum(versions: list[DocumentVersion]) -> list[DocumentVersion]:
    filtered: list[DocumentVersion] = []
    last_checksum: str | None = None
    have_last_checksum = False
    for version in sorted(versions, key=lambda item: item.version_number or 0, reverse=True):
        checksum = version.blob.hash
        if have_last_checksum and checksum == last_checksum:
            continue
        filtered.append(version)
        last_checksum = checksum
        have_last_checksum = True
    return filtered


def folder_contains_doc_folder(folder: str, doc_folder: str) -> bool:
    if not folder:
        return not is_archived_path(doc_folder)
    if folder == ARCHIVE_ROOT:
        return is_archived_path(doc_folder)
    return doc_folder == folder or doc_folder.startswith(f"{folder}/")


def active_locks_by_document(db: Session) -> dict[int, DocumentLock]:
    return {
        lock.document_id: lock
        for lock in db.execute(
            select(DocumentLock).where(DocumentLock.is_active == True),  # noqa: E712
        ).scalars()
    }


def lock_payload(lock: DocumentLock | None) -> dict[str, object | None]:
    return {
        "by": lock.locked_by if lock else None,
        "name": lock.locked_by_name if lock else None,
        "at": lock.locked_at.isoformat() if lock and lock.locked_at else None,
        "ip": lock.locked_ip if lock else None,
        "user_agent": lock.locked_user_agent if lock else None,
        "force_acquired": lock.force_acquired if lock else None,
    }


def document_row_payload(
    doc: Document,
    db: Session,
    path_cache: dict[int, str],
    locks: dict[int, DocumentLock] | None = None,
    user: UserContext | None = None,
) -> dict[str, object]:
    latest_version = current_version(doc, db)
    latest_size_bytes = latest_version.blob.size_bytes if latest_version else None
    modified_at = normalize_timestamp(latest_version.committed_at if latest_version else None)
    doc_folder = document_folder_path(doc, path_cache)
    doc_path = document_path(doc, path_cache)
    archived = document_is_archive(doc)
    archived_from_folder = normalize_folder(doc.archived_from_folder) if archived else ""
    archived_original_name = doc.archived_original_name or ""
    lock = (locks or {}).get(doc.id)
    payload: dict[str, object] = {
        "id": doc.id,
        "name": doc.name,
        "path": doc_path,
        "folder": doc_folder,
        "archived_from_folder": archived_from_folder,
        "archived_original_name": archived_original_name,
        "archived_original_path": join_path(archived_from_folder, archived_original_name)
        if archived_original_name
        else archived_from_folder,
        "modified_at": modified_at.isoformat() if modified_at else None,
        "modified_display": format_mtime(modified_at),
        "latest_by": (latest_version.committed_by_name or latest_version.committed_by)
        if latest_version
        else None,
        "latest_message": latest_version.message if latest_version else None,
        "latest_version_number": latest_version.version_number
        if latest_version
        else doc.latest_version_number,
        "version_count": doc.version_count or 0,
        "created_by": doc.created_by,
        "created_by_name": doc.created_by_name,
        "created_at": doc.created_at.isoformat() if doc.created_at else None,
        "size_bytes": latest_size_bytes,
        "size_display": format_size(latest_size_bytes),
        "lock": lock_payload(lock),
        "archived": archived,
    }
    payload.update(document_expiry_payload(doc))
    if user is not None:
        level = document_access_level(doc, user, db)
        payload["access"] = {
            "visible": level >= 1,
            "read": level >= 2,
            "write": level >= 3,
        }
    return payload


def document_detail_payload(
    doc: Document,
    user: UserContext,
    db: Session,
    path_cache: dict[int, str] | None = None,
    locks: dict[int, DocumentLock] | None = None,
) -> dict[str, object]:
    cache = path_cache or build_folder_path_cache(all_folders(db))
    payload = document_row_payload(doc, db, cache, locks or active_locks_by_document(db), user)
    versions = (
        db.execute(
            select(DocumentVersion)
            .where(DocumentVersion.document_id == doc.id)
            .order_by(DocumentVersion.committed_at.desc()),
        )
        .scalars()
        .all()
    )
    events = (
        db.execute(
            select(DocumentEvent)
            .where(DocumentEvent.document_id == doc.id)
            .order_by(DocumentEvent.created_at.desc()),
        )
        .scalars()
        .all()
    )
    filtered_versions = dedupe_versions_by_checksum(list(versions))
    version_signatures = {version_signature(version) for version in filtered_versions}
    history_items: list[dict[str, object]] = []
    for version in filtered_versions:
        history_items.append(
            {
                "id": version.id,
                "type": "version",
                "timestamp": version.committed_at.isoformat() if version.committed_at else None,
                "display": version.committed_at.strftime("%b %d, %Y %H:%M")
                if version.committed_at
                else "Version",
                "by": version.committed_by_name or version.committed_by,
                "note": version.message,
                "version_number": version.version_number,
                "created_via": version.created_via,
                "checksum": version.blob.hash,
                "hash_algo": version.blob.hash_algo,
                "size_bytes": version.blob.size_bytes,
                "mime_type": version.mime_type,
                "original_filename": version.original_filename,
                "download_url": f"/documents/{doc.id}/versions/{version.id}/download",
            },
        )
    for event in events:
        if event_signature(event) in version_signatures:
            continue
        history_items.append(
            {
                "id": f"event-{event.id}",
                "type": event.event_type,
                "timestamp": event.created_at.isoformat() if event.created_at else None,
                "display": event.created_at.strftime("%b %d, %Y %H:%M")
                if event.created_at
                else event.event_type.title(),
                "by": event.actor_name or event.actor,
                "note": event.message,
                "result": event.result,
                "download_url": None,
            },
        )
    history_items.sort(key=lambda item: str(item["timestamp"] or ""), reverse=True)
    payload["versions"] = history_items
    return payload


def docs_stats_for_folder_payloads(
    docs: list[Document],
    db: Session,
    path_cache: dict[int, str],
) -> list[DocStat]:
    stats: list[DocStat] = []
    for doc in docs:
        latest_version = current_version(doc, db)
        stats.append(
            DocStat(
                document_folder_path(doc, path_cache),
                latest_version.blob.size_bytes if latest_version else 0,
                normalize_timestamp(latest_version.committed_at if latest_version else None),
                (latest_version.committed_by_name or latest_version.committed_by)
                if latest_version
                else None,
            ),
        )
    return stats


def folder_summary_payload(folder: Folder, path: str, stats: list[DocStat]) -> dict[str, object]:
    latest: dt.datetime | None = None
    latest_by: str | None = None
    size = 0
    for stat in stats:
        if not folder_contains_doc_folder(path, stat.folder):
            continue
        size += stat.size_bytes
        if stat.mtime and (latest is None or stat.mtime > latest):
            latest = stat.mtime
            latest_by = stat.latest_by
    return {
        "id": folder.id,
        "path": path,
        "name": path.split("/")[-1] if path else "Vault",
        "color": folder.color or "",
        "icon": folder.icon or "",
        "default_ttl_days": folder.default_ttl_days,
        "default_ttl_action": folder.default_ttl_action or "none",
        **effective_ttl_policy_payload(folder),
        "latest_by": latest_by,
        "modified_at": latest.isoformat() if latest else None,
        "modified_display": format_mtime(latest),
        "size_bytes": size,
        "size_display": format_size(size),
    }


def folder_access_payload(folder: Folder, user: UserContext, db: Session) -> dict[str, bool]:
    level = folder_access_level(folder, user, db)
    return {
        "visible": level >= 1,
        "read": level >= 2,
        "write": level >= 3,
    }


def folder_counts_payload(
    folder: Folder,
    db: Session,
    user: UserContext | None = None,
) -> dict[str, int]:
    folders = all_folders(db)
    folder_ids = subtree_folder_ids(folder, folders)
    visible_folder_ids = folder_ids
    if user is not None:
        visible_folder_ids = {
            item.id
            for item in folders
            if item.id in folder_ids and folder_access_level(item, user, db) >= 1
        }
    docs = (
        list(db.execute(select(Document).where(Document.folder_id.in_(folder_ids))).scalars().all())
        if folder_ids
        else []
    )
    if user is not None:
        docs = [doc for doc in docs if document_access_level(doc, user, db) >= 1]
    return {
        "folders": max(len(visible_folder_ids) - 1, 0),
        "documents": len(docs),
    }


def folder_permissions_payload(folder: Folder, db: Session) -> list[dict[str, object]]:
    rows = db.execute(
        select(FolderPermission, VaultGroup)
        .join(VaultGroup, VaultGroup.id == FolderPermission.group_id)
        .where(FolderPermission.folder_id == folder.id)
        .order_by(VaultGroup.name),
    ).all()
    return [
        {
            "id": permission.id,
            "group_id": group.id,
            "group_name": group.name,
            "can_view": bool(permission.can_view),
            "can_read": bool(permission.can_read),
            "can_write": bool(permission.can_write),
        }
        for permission, group in rows
    ]


def folder_properties_payload(
    folder: Folder,
    db: Session,
    user: UserContext | None = None,
) -> dict[str, object]:
    folders = all_folders(db)
    path_cache = build_folder_path_cache(folders)
    path = folder_path(folder, path_cache)
    docs = list(db.execute(select(Document)).scalars().all())
    if user is not None:
        docs = [doc for doc in docs if document_access_level(doc, user, db) >= 1]
    stats = docs_stats_for_folder_payloads(docs, db, path_cache)
    summary = folder_summary_payload(folder, path, stats)
    events = (
        db.execute(
            select(FolderEvent)
            .where(FolderEvent.folder_id == folder.id)
            .order_by(FolderEvent.created_at.desc()),
        )
        .scalars()
        .all()
    )
    can_manage_permissions = user is None or folder_access_level(folder, user, db) >= 3
    groups = (
        list(db.execute(select(VaultGroup).order_by(VaultGroup.name)).scalars().all())
        if can_manage_permissions
        else []
    )
    summary.update(
        {
            "id": folder.id,
            "root": bool(folder.is_root),
            "archived": folder_is_archive(folder),
            "created_at": folder.created_at.isoformat() if folder.created_at else None,
            "created_by": folder.created_by,
            "created_by_name": folder.created_by_name or folder.created_by or "System",
            **ttl_policy_payload(folder),
            "counts": folder_counts_payload(folder, db, user),
            "history": [
                {
                    "id": event.id,
                    "type": event.event_type,
                    "by": event.actor_name or event.actor or "System",
                    "message": event.message or event.event_type,
                    "timestamp": event.created_at.isoformat() if event.created_at else None,
                }
                for event in events
            ],
            "permissions": folder_permissions_payload(folder, db) if can_manage_permissions else [],
            "available_groups": [{"id": group.id, "name": group.name} for group in groups],
        },
    )
    return summary


def matches_query(query: str, *values: str | None) -> bool:
    needle = query.strip().lower()
    if not needle:
        return True
    return any(needle in (value or "").lower() for value in values)


def folder_is_in_scope(target: str, candidate: str, recursive: bool) -> bool:
    if recursive:
        return folder_contains_doc_folder(target, candidate)
    return candidate == target


def build_contents_payload(
    db: Session,
    folder: str,
    user: UserContext,
    q: str = "",
    recursive: bool = False,
) -> dict[str, object]:
    ensure_root_folders(db)
    current_folder = get_folder_by_path(db, folder)
    if not current_folder:
        raise HTTPException(status_code=404, detail="Folder not found")
    require_folder_access(current_folder, user, db, 1)
    normalized_folder = folder_path(current_folder)
    search_query = q.strip()
    folders = all_folders(db)
    path_cache = build_folder_path_cache(folders)
    docs = [
        doc
        for doc in db.execute(select(Document)).scalars().all()
        if document_access_level(doc, user, db) >= 1
    ]
    locks = active_locks_by_document(db)
    stats = docs_stats_for_folder_payloads(docs, db, path_cache)

    if folder_is_archive(current_folder):
        folder_candidates = []
    elif search_query and recursive:
        folder_candidates = [
            item
            for item in folders
            if not item.is_root
            and item.id != current_folder.id
            and folder_is_archive(item) == folder_is_archive(current_folder)
            and folder_contains_doc_folder(normalized_folder, folder_path(item, path_cache))
        ]
    else:
        folder_candidates = list(current_folder.children)

    folder_rows = []
    for item in folder_candidates:
        if folder_access_level(item, user, db) < 1:
            continue
        path = folder_path(item, path_cache)
        if search_query and not matches_query(search_query, item.name, path):
            continue
        row = folder_summary_payload(item, path, stats)
        row["access"] = folder_access_payload(item, user, db)
        folder_rows.append(row)
    if normalized_folder == ARCHIVE_ROOT:
        for row in folder_rows:
            if row["path"] == ARCHIVE_ROOT:
                row["name"] = ARCHIVE_ROOT

    doc_rows = []
    for doc in docs:
        doc_folder = document_folder_path(doc, path_cache)
        if folder_is_archive(doc.folder) != folder_is_archive(current_folder):
            continue
        if not folder_is_in_scope(normalized_folder, doc_folder, bool(search_query and recursive)):
            continue
        doc_path = document_path(doc, path_cache)
        archived_original_path = ""
        if document_is_archive(doc):
            archived_original_path = join_path(
                normalize_folder(doc.archived_from_folder),
                doc.archived_original_name or doc.name,
            )
        if search_query and not matches_query(
            search_query,
            doc.name,
            doc_path,
            doc_folder,
            doc.archived_from_folder,
            archived_original_path,
        ):
            continue
        doc_rows.append(document_row_payload(doc, db, path_cache, locks, user))

    folder_rows.sort(key=lambda item: str(item["name"]).lower())
    doc_rows.sort(key=lambda item: str(item["name"]).lower())
    return {
        "folder": normalized_folder,
        "q": search_query,
        "recursive": bool(recursive),
        "folders": folder_rows,
        "documents": doc_rows,
    }


def build_sidebar_payload(db: Session, user: UserContext) -> dict[str, object]:
    ensure_root_folders(db)
    vault_root = get_root_folder(db, VAULT_ROOT_KEY)
    folders = all_folders(db)
    path_cache = build_folder_path_cache(folders)
    children = {
        "": sorted(
            folder_path(child, path_cache)
            for child in vault_root.children
            if folder_access_level(child, user, db) >= 1
        ),
        ARCHIVE_ROOT: [],
    }
    metadata = {
        folder_path(item, path_cache): {
            "id": item.id,
            "color": item.color or "",
            "icon": item.icon or "",
            "access": folder_access_payload(item, user, db),
            "default_ttl_days": item.default_ttl_days,
            "default_ttl_action": item.default_ttl_action or "none",
            **effective_ttl_policy_payload(item),
        }
        for item in folders
        if folder_access_level(item, user, db) >= 1
    }
    return {"folder_children": children, "folder_metadata": metadata}


def build_my_edits_payload(user: UserContext, db: Session) -> dict[str, object]:
    ensure_root_folders(db)
    folders = all_folders(db)
    path_cache = build_folder_path_cache(folders)
    locks = active_locks_by_document(db)
    docs = (
        db.execute(
            select(Document)
            .join(DocumentLock)
            .where(
                DocumentLock.locked_by == user["id"],
                DocumentLock.is_active == True,  # noqa: E712
            ),
        )
        .scalars()
        .all()
    )
    return {
        "documents": [
            document_row_payload(doc, db, path_cache, locks, user)
            for doc in sorted(docs, key=lambda item: document_path(item, path_cache).lower())
            if document_access_level(doc, user, db) >= 3
        ],
    }


def normalize_group_name(name: str | None) -> str:
    cleaned = (name or "").strip()
    if not cleaned:
        raise HTTPException(status_code=400, detail="Group name is required")
    if "/" in cleaned or "\\" in cleaned or cleaned in {".", ".."}:
        raise HTTPException(status_code=400, detail="Invalid group name")
    return " ".join(cleaned.split())


def build_admin_directory_payload(db: Session) -> dict[str, object]:
    users = list(
        db.execute(
            select(VaultUser).order_by(VaultUser.name, VaultUser.email, VaultUser.id),
        )
        .scalars()
        .all(),
    )
    groups = list(db.execute(select(VaultGroup).order_by(VaultGroup.name)).scalars().all())
    memberships = list(db.execute(select(VaultGroupMembership)).scalars().all())
    groups_by_id = {group.id: group for group in groups}
    users_by_id = {user.id: user for user in users}
    group_ids_by_user: dict[int, list[int]] = defaultdict(list)
    user_ids_by_group: dict[int, list[int]] = defaultdict(list)
    for membership in memberships:
        if membership.user_id in users_by_id and membership.group_id in groups_by_id:
            group_ids_by_user[membership.user_id].append(membership.group_id)
            user_ids_by_group[membership.group_id].append(membership.user_id)

    return {
        "users": [
            {
                "id": user.id,
                "issuer": user.issuer,
                "subject": user.subject,
                "email": user.email or "",
                "name": user.name,
                "is_admin": vault_user_is_effective_admin(
                    user,
                    db,
                    [groups_by_id[group_id].name for group_id in group_ids_by_user[user.id]],
                ),
                "is_active": bool(user.is_active),
                "created_at": user.created_at.isoformat() if user.created_at else None,
                "last_login_at": user.last_login_at.isoformat() if user.last_login_at else None,
                "last_seen_at": user.last_seen_at.isoformat() if user.last_seen_at else None,
                "groups": [
                    {"id": groups_by_id[group_id].id, "name": groups_by_id[group_id].name}
                    for group_id in sorted(
                        group_ids_by_user[user.id],
                        key=lambda item: groups_by_id[item].name.lower(),
                    )
                ],
            }
            for user in users
        ],
        "groups": [
            {
                "id": group.id,
                "name": group.name,
                "description": group.description or "",
                "members": [
                    {
                        "id": users_by_id[user_id].id,
                        "name": users_by_id[user_id].name,
                        "email": users_by_id[user_id].email or "",
                    }
                    for user_id in sorted(
                        user_ids_by_group[group.id],
                        key=lambda item: users_by_id[item].name.lower(),
                    )
                ],
            }
            for group in groups
        ],
        "dev_mode": DEV_MODE,
        "settings": site_settings_for_db(db),
    }


def ensure_not_last_active_admin(db: Session, target: VaultUser) -> None:
    if not target.is_active or not vault_user_is_effective_admin(target, db):
        return
    active_admins = [
        user
        for user in db.execute(
            select(VaultUser).where(VaultUser.is_active == True),  # noqa: E712
        )
        .scalars()
        .all()
        if vault_user_is_effective_admin(user, db)
    ]
    if len(active_admins) == 1 and active_admins[0].id == target.id:
        raise HTTPException(status_code=400, detail="At least one active admin is required")


def ensure_active_admin_for_group_names(
    db: Session,
    group_names_by_user: dict[int, list[str]],
) -> None:
    users = list(db.execute(select(VaultUser)).scalars().all())
    if any(
        user.is_active
        and vault_user_is_effective_admin(user, db, group_names_by_user.get(user.id, []))
        for user in users
    ):
        return
    raise HTTPException(status_code=400, detail="At least one active admin is required")


def group_names_by_user_after_group_change(
    db: Session,
    *,
    deleted_group_id: int | None = None,
    renamed_group_id: int | None = None,
    renamed_group_name: str | None = None,
    removed_membership_group_id: int | None = None,
    removed_membership_user_id: int | None = None,
) -> dict[int, list[str]]:
    groups = list(db.execute(select(VaultGroup)).scalars().all())
    groups_by_id = {row.id: row for row in groups if row.id != deleted_group_id}
    group_names_by_user: dict[int, list[str]] = defaultdict(list)
    memberships = list(db.execute(select(VaultGroupMembership)).scalars().all())
    for membership in memberships:
        if membership.group_id == deleted_group_id:
            continue
        if (
            membership.group_id == removed_membership_group_id
            and membership.user_id == removed_membership_user_id
        ):
            continue
        if membership.group_id == renamed_group_id and renamed_group_name is not None:
            group_names_by_user[membership.user_id].append(renamed_group_name)
            continue
        membership_group = groups_by_id.get(membership.group_id)
        if membership_group:
            group_names_by_user[membership.user_id].append(membership_group.name)
    return group_names_by_user


def ensure_group_delete_preserves_active_admin(db: Session, group: VaultGroup) -> None:
    ensure_active_admin_for_group_names(
        db,
        group_names_by_user_after_group_change(db, deleted_group_id=group.id),
    )


def ensure_group_rename_preserves_active_admin(
    db: Session,
    group: VaultGroup,
    name: str,
) -> None:
    ensure_active_admin_for_group_names(
        db,
        group_names_by_user_after_group_change(
            db,
            renamed_group_id=group.id,
            renamed_group_name=name,
        ),
    )


def ensure_group_membership_remove_preserves_active_admin(
    db: Session,
    membership: VaultGroupMembership,
) -> None:
    ensure_active_admin_for_group_names(
        db,
        group_names_by_user_after_group_change(
            db,
            removed_membership_group_id=membership.group_id,
            removed_membership_user_id=membership.user_id,
        ),
    )


def find_group_by_normalized_name(db: Session, name: str) -> VaultGroup | None:
    lowered = name.lower()
    for group in db.execute(select(VaultGroup)).scalars().all():
        if group.name.lower() == lowered:
            return group
    return None


def commit_admin_change(
    db: Session,
    event_type: str,
    resources: tuple[str, ...] = ("admin",),
) -> dict[str, object]:
    record_state_change(db, f"admin.{event_type}", resources)
    commit_state(db)
    return build_admin_directory_payload(db)


def build_bootstrap_payload(user: UserContext, folder: str, db: Session) -> dict[str, object]:
    ensure_root_folders(db)
    current = get_folder_by_path(db, folder)
    if not current:
        raise HTTPException(status_code=404, detail="Folder not found")
    require_folder_access(current, user, db, 1)
    return {
        "auth_mode": AUTH_MODE,
        "base_domain": BASE_DOMAIN,
        "dev_mode": DEV_MODE,
        "site_name": SITE_NAME,
        "user": user,
        "preferences": preferences_for_user(user, db),
        "settings": site_settings_for_db(db),
        "version": APP_VERSION,
        "current_folder": folder_path(current),
    }


def build_initial_state(
    user: UserContext,
    folder: str,
    db: Session,
    share_code: str | None = None,
) -> dict[str, object]:
    normalized = normalize_folder(folder)
    state = {
        "bootstrap": build_bootstrap_payload(user, normalized, db),
        "contents": build_contents_payload(db, normalized, user),
        "sidebar": build_sidebar_payload(db, user),
        "my_edits": build_my_edits_payload(user, db),
    }
    if share_code:
        state["share_code"] = share_code
    return state


def normalize_appearance_header(value: str | None, allowed: set[str]) -> str | None:
    normalized = (value or "").strip().lower()
    return normalized if normalized in allowed else None


def build_appearance_override(request: Request) -> dict[str, str | None]:
    return {
        "palette": normalize_appearance_header(
            request.headers.get("x-vault-palette"),
            APPEARANCE_PALETTES,
        ),
        "theme": normalize_appearance_header(
            request.headers.get("x-vault-theme"),
            APPEARANCE_THEMES,
        ),
    }


def index_template_context(request: Request, state: dict[str, object]) -> dict[str, object]:
    return {
        "appearance_override": build_appearance_override(request),
        "request": request,
        "state": state,
    }


def resolved_favorite_items(
    preferences: dict[str, object],
    user: UserContext,
    db: Session,
) -> list[dict[str, object]]:
    raw_items = preferences.get("favoriteItems")
    if not isinstance(raw_items, list) or not raw_items:
        return []
    resolved_targets: list[tuple[str, Folder | Document]] = []
    for item in raw_items:
        if not isinstance(item, dict):
            continue
        item_type = item.get("type")
        item_id = item.get("id")
        if not isinstance(item_id, int):
            continue
        if item_type == "folder":
            try:
                folder = get_folder_by_id_or_404(item_id, db)
            except HTTPException:
                continue
            if folder_access_level(folder, user, db) >= 1:
                resolved_targets.append(("folder", folder))
            continue
        if item_type == "document":
            try:
                doc = get_document_or_404(item_id, db)
            except HTTPException:
                continue
            if document_access_level(doc, user, db) >= 1:
                resolved_targets.append(("document", doc))

    folders = all_folders(db)
    path_cache = build_folder_path_cache(folders)
    visible_docs = [
        doc
        for doc in db.execute(select(Document)).scalars().all()
        if document_access_level(doc, user, db) >= 1
    ]
    stats = docs_stats_for_folder_payloads(visible_docs, db, path_cache)
    locks = active_locks_by_document(db)
    resolved: list[dict[str, object]] = []
    for item_type, target in resolved_targets:
        if item_type == "folder":
            folder = target
            row = folder_summary_payload(folder, folder_path(folder, path_cache), stats)
            row.update(
                {
                    "type": "folder",
                    "archived": folder_is_archive(folder),
                    "access": folder_access_payload(folder, user, db),
                },
            )
            resolved.append(row)
            continue
        if item_type == "document":
            doc = target
            row = document_row_payload(doc, db, path_cache, locks, user)
            row["type"] = "document"
            resolved.append(row)
    return resolved


def preferences_for_user(user: UserContext, db: Session) -> dict[str, object]:
    user_id = int(user.get("vault_user_id") or 0)
    if not user_id:
        return normalize_user_preferences({})
    vault_user = db.get(VaultUser, user_id)
    preferences = normalize_user_preferences(vault_user.preferences if vault_user else {})
    preferences["favoriteItems"] = resolved_favorite_items(preferences, user, db)
    return preferences


def require_vault_user(user: UserContext, db: Session) -> VaultUser:
    user_id = int(user.get("vault_user_id") or 0)
    if not user_id:
        raise HTTPException(status_code=400, detail="User preferences require a vault user")
    vault_user = db.get(VaultUser, user_id)
    if not vault_user:
        raise HTTPException(status_code=404, detail="Vault user not found")
    return vault_user


def generate_share_code(db: Session) -> str:
    for _ in range(20):
        code = secrets.token_urlsafe(9)
        exists = db.execute(select(ShareLink.id).where(ShareLink.code == code)).first()
        if not exists:
            return code
    raise HTTPException(status_code=500, detail="Could not create share link")


def share_url(request: Request, code: str) -> str:
    return str(request.url_for("share_entry", code=code))


def normalize_share_target_type(value: str) -> str:
    normalized = (value or "").strip().lower()
    if normalized in {"document", "file"}:
        return "document"
    if normalized == "folder":
        return "folder"
    raise HTTPException(status_code=400, detail="Invalid share target")


def create_share_target(
    payload: ShareLinkPayload,
    user: UserContext,
    db: Session,
) -> tuple[str, int | None, int | None]:
    target_type = normalize_share_target_type(payload.target_type)
    if target_type == "document":
        if payload.document_id is None:
            raise HTTPException(status_code=400, detail="Document id is required")
        doc = get_document_or_404(payload.document_id, db)
        require_document_access(doc, user, db, 1)
        return target_type, doc.id, None

    folder: Folder | None = None
    if payload.folder_id is not None:
        folder = get_folder_by_id_or_404(payload.folder_id, db)
    else:
        folder = get_folder_by_path(db, payload.path)
    if not folder:
        raise HTTPException(status_code=404, detail="Folder not found")
    require_folder_access(folder, user, db, 1)
    return target_type, None, folder.id


def resolved_share_payload(link: ShareLink, user: UserContext, db: Session) -> dict[str, object]:
    if link.disabled_at:
        raise HTTPException(status_code=404, detail="Share link not found")
    if link.expires_at and normalize_timestamp(link.expires_at) <= now_utc():
        raise HTTPException(status_code=404, detail="Share link expired")

    if link.target_type == "document" and link.document_id is not None:
        doc = get_document_or_404(link.document_id, db)
        require_document_access(doc, user, db, 1)
        path_cache = build_folder_path_cache(all_folders(db))
        return {
            "code": link.code,
            "target_type": "document",
            "document_id": doc.id,
            "folder": document_folder_path(doc, path_cache),
            "document": document_row_payload(
                doc,
                db,
                path_cache,
                active_locks_by_document(db),
                user,
            ),
        }

    if link.target_type == "folder" and link.folder_id is not None:
        folder = get_folder_by_id_or_404(link.folder_id, db)
        require_folder_access(folder, user, db, 1)
        path_cache = build_folder_path_cache(all_folders(db))
        path = folder_path(folder, path_cache)
        visible_docs = [
            doc
            for doc in db.execute(select(Document)).scalars().all()
            if document_access_level(doc, user, db) >= 1
        ]
        stats = docs_stats_for_folder_payloads(
            visible_docs,
            db,
            path_cache,
        )
        return {
            "code": link.code,
            "target_type": "folder",
            "folder_id": folder.id,
            "folder": path,
            "folder_item": folder_summary_payload(folder, path, stats),
        }

    raise HTTPException(status_code=404, detail="Share target not found")


def record_state_change(db: Session, event_type: str, resources: tuple[str, ...]) -> None:
    normalized_resources = sorted(set(resources))
    if not normalized_resources:
        return
    payload = {
        "type": event_type,
        "resources": normalized_resources,
    }
    db.add(StateEvent(event_type=event_type, payload=payload))


def record_folder_change(
    db: Session,
    event_type: str,
    include_document_updates: bool = False,
) -> None:
    resources = ["contents", "sidebar"]
    if include_document_updates:
        resources.extend(["document_detail", "my_edits"])
    record_state_change(db, f"folder.{event_type}", tuple(resources))


def record_document_deleted(db: Session) -> None:
    record_state_change(
        db,
        "document.deleted",
        ("contents", "sidebar", "document_detail", "my_edits", "preferences"),
    )


def latest_state_event_id() -> int:
    db = SessionLocal()
    try:
        return (
            db.execute(select(StateEvent.id).order_by(StateEvent.id.desc()).limit(1)).scalar() or 0
        )
    finally:
        db.close()


def state_events_after(last_id: int) -> list[StateEvent]:
    db = SessionLocal()
    try:
        return list(
            db.execute(
                select(StateEvent)
                .where(StateEvent.id > last_id)
                .order_by(StateEvent.id)
                .limit(100),
            )
            .scalars()
            .all(),
        )
    finally:
        db.close()


def debug_event_stream_generation() -> int:
    if not DEV_MODE:
        return 0
    return _debug_event_stream_generation


def debug_event_stream_retry_ms(stream_generation: int) -> int | None:
    if not DEV_MODE or stream_generation == _debug_event_stream_generation:
        return None
    return _debug_event_stream_retry_ms


def commit_state(db: Session) -> None:
    db.commit()


def mutate_doc_location(
    doc: Document,
    target_folder: Folder,
    target_name: str,
    user: UserContext,
    db: Session,
    meta: dict[str, str | None],
    event_type: str,
    message: str,
    publish_state: bool = True,
    allow_duplicate_name: bool = False,
) -> None:
    if not allow_duplicate_name:
        ensure_unique_document_path(db, target_folder.id, target_name, doc.id)
    doc.folder = target_folder
    doc.folder_id = target_folder.id
    doc.name = target_name
    doc.latest_modified_at = now_utc()
    apply_folder_ttl(doc, target_folder, doc.latest_modified_at)
    record_event(doc, user, event_type, message, db, meta=meta, publish_state=publish_state)


def archive_expired_document(doc: Document, db: Session, timestamp: dt.datetime) -> str:
    source_path = document_path(doc)
    source_folder_path = document_folder_path(doc)
    target_folder = get_root_folder(db, ARCHIVE_ROOT_KEY)
    doc.archived_from_folder = source_folder_path
    doc.archived_original_name = doc.name
    doc.archived_access = archive_access_snapshot(doc.folder, db)
    mutate_doc_location(
        doc,
        target_folder,
        doc.name,
        SYSTEM_USER,
        db,
        SYSTEM_META,
        "archive",
        f"Expired at {timestamp.strftime('%Y-%m-%d %H:%M UTC')}; archived from {source_path}",
        publish_state=False,
        allow_duplicate_name=True,
    )
    return document_path(doc)


def delete_expired_document(doc: Document, db: Session) -> str:
    deleted_path = document_path(doc)
    db.delete(doc)
    return deleted_path


def sweep_expired_documents(limit: int = 250) -> dict[str, list[str]]:
    result: dict[str, list[str]] = {"archived": [], "deleted": [], "skipped": []}
    with storage_write_lock():
        db = SessionLocal()
        try:
            ensure_root_folders(db)
            timestamp = now_utc()
            docs = list(
                db.execute(
                    select(Document)
                    .where(
                        Document.expires_at.is_not(None),
                        Document.expires_at <= timestamp,
                    )
                    .order_by(Document.expires_at)
                    .limit(limit),
                )
                .scalars()
                .all(),
            )
            if not docs:
                return result
            locks = active_locks_by_document(db)
            for doc in docs:
                if locks.get(doc.id):
                    result["skipped"].append(document_path(doc))
                    continue
                action = (doc.expiry_action or "").strip().lower()
                if action == "archive":
                    if document_is_archive(doc):
                        doc.expires_at = None
                        doc.expiry_action = None
                    else:
                        result["archived"].append(archive_expired_document(doc, db, timestamp))
                    continue
                if action == "delete":
                    result["deleted"].append(delete_expired_document(doc, db))
                    continue
                doc.expires_at = None
                doc.expiry_action = None
            if result["archived"] or result["deleted"]:
                record_state_change(
                    db,
                    "retention.expired",
                    ("contents", "sidebar", "document_detail", "my_edits"),
                )
            db.commit()
            return result
        except Exception:
            db.rollback()
            raise
        finally:
            db.close()


async def _ttl_sweeper_loop() -> None:
    while True:
        await asyncio.sleep(TTL_SWEEP_INTERVAL_SECONDS)
        try:
            sweep_expired_documents()
        except Exception:
            logger.exception("TTL sweep failed")


def start_ttl_sweeper() -> None:
    global _ttl_sweeper_task
    if _ttl_sweeper_task and not _ttl_sweeper_task.done():
        return
    _ttl_sweeper_task = asyncio.create_task(_ttl_sweeper_loop())


async def stop_ttl_sweeper() -> None:
    global _ttl_sweeper_task
    task = _ttl_sweeper_task
    _ttl_sweeper_task = None
    if not task:
        return
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        return


def storage_reconciliation_report(db: Session, apply: bool = False) -> dict[str, object]:
    referenced_blob_ids = {
        row[0] for row in db.execute(select(DocumentVersion.blob_id)).all() if row[0] is not None
    }
    referenced_blobs = (
        list(db.execute(select(Blob).where(Blob.id.in_(referenced_blob_ids))).scalars())
        if referenced_blob_ids
        else []
    )
    referenced_blobs_by_id = {blob.id: blob for blob in referenced_blobs}
    orphan_blobs = [
        blob for blob in db.execute(select(Blob)).scalars() if blob.id not in referenced_blob_ids
    ]
    orphan_blob_ids = [blob.id for blob in orphan_blobs]
    local_locations = list(
        db.execute(select(BlobLocation).where(BlobLocation.backend == "local")).scalars(),
    )
    known_local_keys = {location.object_key for location in local_locations}
    local_backend = get_storage_backend("local")
    local_bucket = local_backend.bucket
    local_keys = set(local_backend.list_object_keys())
    referenced_local_keys = {
        location.object_key
        for location in local_locations
        if location.blob_id in referenced_blob_ids
    }
    local_location_pairs = {(location.blob_id, location.object_key) for location in local_locations}
    recoverable_referenced_local_locations: set[tuple[int, str]] = set()
    corrupt_local_keys: set[str] = set()
    for blob in referenced_blobs:
        object_key = object_key_for_hash(blob.hash_algo, blob.hash)
        if object_key not in local_keys:
            continue
        try:
            data = local_backend.read_bytes(object_key, local_backend.bucket)
        except StorageError:
            corrupt_local_keys.add(object_key)
            continue
        if blob_bytes_match(blob, data):
            recoverable_referenced_local_locations.add((blob.id, object_key))
        else:
            corrupt_local_keys.add(object_key)
    for location in local_locations:
        blob = referenced_blobs_by_id.get(location.blob_id)
        if blob is None or location.object_key not in local_keys:
            continue
        try:
            data = local_backend.read_bytes(location.object_key, location.bucket)
        except StorageError:
            corrupt_local_keys.add(location.object_key)
            continue
        if not blob_bytes_match(blob, data):
            corrupt_local_keys.add(location.object_key)
    recoverable_referenced_local_keys = {
        object_key for _, object_key in recoverable_referenced_local_locations
    }
    referenced_protected_local_keys = (
        referenced_local_keys | recoverable_referenced_local_keys | corrupt_local_keys
    )
    unreferenced_local_keys = sorted(
        local_keys - known_local_keys - recoverable_referenced_local_keys - corrupt_local_keys,
    )
    missing_local_keys = sorted(referenced_local_keys - local_keys)
    missing_local_location_keys = sorted(
        object_key
        for blob_id, object_key in recoverable_referenced_local_locations
        if (blob_id, object_key) not in local_location_pairs
    )
    orphan_local_keys = sorted(
        {
            location.object_key
            for blob in orphan_blobs
            for location in blob.locations
            if location.backend == "local"
        },
    )
    orphan_local_keys_to_delete = sorted(set(orphan_local_keys) - referenced_protected_local_keys)
    if apply:
        for object_key in orphan_local_keys_to_delete:
            local_backend.delete_object(object_key)
        orphan_blob_id_set = set(orphan_blob_ids)
        remote_location_blob_ids = {
            location.blob_id
            for location in db.execute(
                select(BlobLocation).where(
                    BlobLocation.blob_id.in_(orphan_blob_id_set),
                    BlobLocation.backend != "local",
                ),
            ).scalars()
        }
        local_orphan_locations = list(
            db.execute(
                select(BlobLocation).where(
                    BlobLocation.blob_id.in_(orphan_blob_id_set),
                    BlobLocation.backend == "local",
                ),
            ).scalars(),
        )
        for location in local_orphan_locations:
            db.delete(location)
        for blob in orphan_blobs:
            if blob.id not in remote_location_blob_ids:
                db.delete(blob)
        for object_key in unreferenced_local_keys:
            local_backend.delete_object(object_key)
        for blob_id, object_key in sorted(recoverable_referenced_local_locations):
            if (blob_id, object_key) in local_location_pairs:
                continue
            existing_location = (
                db.execute(
                    select(BlobLocation).where(
                        BlobLocation.backend == "local",
                        BlobLocation.bucket == local_bucket,
                        BlobLocation.object_key == object_key,
                    ),
                )
                .scalars()
                .first()
            )
            if existing_location:
                continue
            db.add(
                BlobLocation(
                    blob_id=blob_id,
                    backend="local",
                    bucket=local_bucket,
                    object_key=object_key,
                ),
            )
        db.flush()
    return {
        "orphan_blob_ids": orphan_blob_ids,
        "unreferenced_local_keys": unreferenced_local_keys,
        "missing_local_keys": missing_local_keys,
        "missing_local_location_keys": missing_local_location_keys,
        "corrupt_local_keys": sorted(corrupt_local_keys),
        "deleted_local_keys": sorted(
            set(unreferenced_local_keys) | set(orphan_local_keys_to_delete),
        )
        if apply
        else [],
    }


@router.get("/login")
def login(request: Request) -> RedirectResponse:
    return oidc_login_response(request)


@router.get("/auth/callback")
def auth_callback(
    request: Request,
    db: Session = Depends(get_db),
) -> RedirectResponse:
    return oidc_callback_response(request, db)


@router.get("/logout")
def logout(request: Request) -> RedirectResponse:
    return logout_response(request)


@router.get("/api/admin/directory")
def api_admin_directory(
    user: UserContext = Depends(require_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    return build_admin_directory_payload(db)


def require_dev_admin(user: UserContext = Depends(require_admin)) -> UserContext:
    if not DEV_MODE:
        raise HTTPException(status_code=404, detail="Debug tools are not available")
    return user


def debug_action_result(action: str, **data: object) -> dict[str, object]:
    return {"action": action, "dev_mode": DEV_MODE, "ok": True, **data}


def debug_timestamp() -> str:
    return now_utc().strftime("%Y%m%d-%H%M%S")


@router.post("/api/admin/debug/error")
def api_admin_debug_error(
    payload: DebugErrorPayload,
    user: UserContext = Depends(require_dev_admin),
) -> dict[str, object]:
    del user
    kind = (payload.kind or "server").strip().lower()
    if kind == "bad-request":
        raise HTTPException(status_code=400, detail="Debug bad request error")
    if kind == "forbidden":
        raise HTTPException(status_code=403, detail="Debug forbidden error")
    if kind == "not-found":
        raise HTTPException(status_code=404, detail="Debug not found error")
    if kind == "unavailable":
        raise HTTPException(status_code=503, detail="Debug service unavailable")
    raise HTTPException(status_code=500, detail="Debug server error")


@router.post("/api/admin/debug/timeout")
def api_admin_debug_timeout(
    user: UserContext = Depends(require_dev_admin),
) -> dict[str, object]:
    global _debug_event_stream_generation, _debug_event_stream_retry_ms
    del user
    seconds = 10
    _debug_event_stream_generation += 1
    _debug_event_stream_retry_ms = seconds * 1000
    return debug_action_result(
        "timeout",
        seconds=seconds,
        stream_generation=_debug_event_stream_generation,
        stream_retry_ms=_debug_event_stream_retry_ms,
    )


@router.post("/api/admin/debug/emit-state")
def api_admin_debug_emit_state(
    payload: DebugStateEventPayload,
    user: UserContext = Depends(require_dev_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    allowed = {
        "admin",
        "contents",
        "document_detail",
        "my_edits",
        "preferences",
        "settings",
        "sidebar",
    }
    resources = tuple(resource for resource in payload.resources if resource in allowed)
    record_state_change(
        db,
        "debug.refresh",
        resources or ("contents", "sidebar", "my_edits"),
    )
    commit_state(db)
    return debug_action_result("emit-state", resources=list(resources))


@router.post("/api/admin/debug/sweep-ttl")
def api_admin_debug_sweep_ttl(
    user: UserContext = Depends(require_dev_admin),
) -> dict[str, object]:
    del user
    return debug_action_result("sweep-ttl", result=sweep_expired_documents())


@router.post("/api/admin/debug/storage-report")
def api_admin_debug_storage_report(
    user: UserContext = Depends(require_dev_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    return debug_action_result(
        "storage-report",
        report=storage_reconciliation_report(db, apply=False),
    )


@router.post("/api/admin/debug/seed")
def api_admin_debug_seed(
    user: UserContext = Depends(require_dev_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    folder = get_or_create_folder_path(db, "Debug Samples")
    name = f"debug-sample-{debug_timestamp()}.txt"
    blob = get_or_create_blob_for_data(
        db,
        f"Debug sample created at {now_utc().isoformat()}\n".encode(),
        "text/plain",
    )
    doc = Document(
        folder_id=folder.id,
        name=name,
        created_by=user["id"],
        created_by_name=user["name"],
        latest_modified_by=user["id"],
    )
    db.add(doc)
    db.flush()
    create_document_version(
        db,
        doc,
        blob,
        user,
        SYSTEM_META,
        name,
        "text/plain",
        "Debug seed",
        "upload",
    )
    record_folder_change(db, "debug.seeded")
    db.commit()
    return debug_action_result("seed", document_id=doc.id, folder="Debug Samples", name=name)


@router.post("/api/admin/debug/reset-database")
def api_admin_debug_reset_database(
    user: UserContext = Depends(require_dev_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    with storage_write_lock():
        db.close()
        db_runtime.Base.metadata.drop_all(bind=db_runtime.engine)
        db_runtime.init_db()
    return debug_action_result("reset-database", reload=True)


@router.patch("/api/admin/settings")
def api_admin_update_settings(
    payload: AdminSettingsPayload,
    user: UserContext = Depends(require_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    try:
        merge_site_settings(db, payload.settings)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc
    return commit_admin_change(db, "settings.updated", ("admin", "settings"))


@router.patch("/api/admin/users/{user_id}")
def api_admin_update_user(
    user_id: int,
    payload: AdminUserUpdate,
    user: UserContext = Depends(require_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    target = db.get(VaultUser, user_id)
    if not target:
        raise HTTPException(status_code=404, detail="User not found")
    if payload.is_admin is False or payload.is_active is False:
        ensure_not_last_active_admin(db, target)
    if payload.is_admin is not None:
        target.is_admin = payload.is_admin
    if payload.is_active is not None:
        target.is_active = payload.is_active
    return commit_admin_change(db, "user.updated")


@router.post("/api/admin/groups")
def api_admin_create_group(
    payload: AdminGroupPayload,
    user: UserContext = Depends(require_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    name = normalize_group_name(payload.name)
    if find_group_by_normalized_name(db, name):
        raise HTTPException(status_code=409, detail="Group already exists")
    description = (payload.description or "").strip() or None
    db.add(VaultGroup(name=name, description=description))
    return commit_admin_change(db, "group.created")


@router.patch("/api/admin/groups/{group_id}")
def api_admin_update_group(
    group_id: int,
    payload: AdminGroupPayload,
    user: UserContext = Depends(require_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    group = db.get(VaultGroup, group_id)
    if not group:
        raise HTTPException(status_code=404, detail="Group not found")
    name = normalize_group_name(payload.name)
    existing = find_group_by_normalized_name(db, name)
    if existing and existing.id != group.id:
        raise HTTPException(status_code=409, detail="Group already exists")
    ensure_group_rename_preserves_active_admin(db, group, name)
    group.name = name
    group.description = (payload.description or "").strip() or None
    return commit_admin_change(db, "group.updated")


@router.delete("/api/admin/groups/{group_id}")
def api_admin_delete_group(
    group_id: int,
    user: UserContext = Depends(require_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    group = db.get(VaultGroup, group_id)
    if not group:
        raise HTTPException(status_code=404, detail="Group not found")
    if db.execute(select(FolderPermission.id).where(FolderPermission.group_id == group.id)).first():
        raise HTTPException(status_code=400, detail="Group is used by folder permissions")
    ensure_group_delete_preserves_active_admin(db, group)
    db.delete(group)
    return commit_admin_change(db, "group.deleted")


@router.post("/api/admin/groups/{group_id}/members")
def api_admin_add_group_member(
    group_id: int,
    payload: AdminGroupMemberPayload,
    user: UserContext = Depends(require_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    group = db.get(VaultGroup, group_id)
    member = db.get(VaultUser, payload.user_id)
    if not group or not member:
        raise HTTPException(status_code=404, detail="Group or user not found")
    existing = (
        db.execute(
            select(VaultGroupMembership).where(
                VaultGroupMembership.group_id == group.id,
                VaultGroupMembership.user_id == member.id,
            ),
        )
        .scalars()
        .first()
    )
    if not existing:
        db.add(VaultGroupMembership(group_id=group.id, user_id=member.id))
    return commit_admin_change(db, "group.member.added")


@router.delete("/api/admin/groups/{group_id}/members/{user_id}")
def api_admin_remove_group_member(
    group_id: int,
    user_id: int,
    user: UserContext = Depends(require_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    membership = (
        db.execute(
            select(VaultGroupMembership).where(
                VaultGroupMembership.group_id == group_id,
                VaultGroupMembership.user_id == user_id,
            ),
        )
        .scalars()
        .first()
    )
    if not membership:
        raise HTTPException(status_code=404, detail="Membership not found")
    ensure_group_membership_remove_preserves_active_admin(db, membership)
    db.delete(membership)
    return commit_admin_change(db, "group.member.removed")


@router.get("/", response_class=HTMLResponse)
def index(
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> HTMLResponse:
    ensure_root_folders(db)
    commit_state(db)
    state = build_initial_state(user, "", db)
    return templates.TemplateResponse("index.html", index_template_context(request, state))


@router.get("/s/{code}", response_class=HTMLResponse, name="share_entry")
def share_entry(
    code: str,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> HTMLResponse:
    if not SHARE_CODE_PATTERN.fullmatch(code):
        raise HTTPException(status_code=404, detail="Share link not found")
    ensure_root_folders(db)
    commit_state(db)
    state = build_initial_state(user, "", db, share_code=code)
    return templates.TemplateResponse("index.html", index_template_context(request, state))


@router.get("/api/bootstrap")
def api_bootstrap(
    folder: str = "",
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    commit_state(db)
    return build_bootstrap_payload(user, normalize_folder(folder), db)


@router.get("/api/settings")
def api_settings(
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del user
    return {"settings": site_settings_for_db(db)}


@router.get("/api/preferences")
def api_preferences(
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    return {"preferences": preferences_for_user(user, db)}


@router.patch("/api/preferences")
def api_update_preferences(
    payload: UserPreferencesPayload,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    vault_user = require_vault_user(user, db)
    try:
        patch = clean_user_preference_patch(payload.preferences)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc
    vault_user.preferences = merge_user_preferences(vault_user.preferences, patch)
    db.commit()
    return {"preferences": preferences_for_user(user, db)}


@router.get("/api/folders/sidebar")
def api_sidebar(
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    commit_state(db)
    return build_sidebar_payload(db, user)


@router.get("/api/folders/contents")
def api_folder_contents(
    folder: str = "",
    q: str = "",
    recursive: bool = False,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    commit_state(db)
    return build_contents_payload(db, normalize_folder(folder), user, q, recursive)


@router.post("/api/share-links")
def api_create_share_link(
    payload: ShareLinkPayload,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    target_type, document_id, folder_id = create_share_target(payload, user, db)
    created_by_user_id = user["vault_user_id"] if user.get("vault_user_id") else None
    link = ShareLink(
        code=generate_share_code(db),
        target_type=target_type,
        document_id=document_id,
        folder_id=folder_id,
        access_mode="internal",
        created_by=user["id"],
        created_by_name=user["name"],
        created_by_user_id=created_by_user_id,
    )
    db.add(link)
    db.commit()
    return {
        "code": link.code,
        "url": share_url(request, link.code),
        "target_type": target_type,
        "document_id": document_id,
        "folder_id": folder_id,
        "access_mode": link.access_mode,
    }


@router.get("/api/share-links/{code}")
def api_resolve_share_link(
    code: str,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    if not SHARE_CODE_PATTERN.fullmatch(code):
        raise HTTPException(status_code=404, detail="Share link not found")
    link = db.execute(select(ShareLink).where(ShareLink.code == code)).scalars().first()
    if not link:
        raise HTTPException(status_code=404, detail="Share link not found")
    return resolved_share_payload(link, user, db)


@router.get("/api/folders/properties")
def api_folder_properties(
    path: str = "",
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    commit_state(db)
    folder = get_folder_by_path(db, path)
    if not folder:
        raise HTTPException(status_code=404, detail="Folder not found")
    require_folder_access(folder, user, db, 1)
    return folder_properties_payload(folder, db, user)


@router.patch("/api/folders/properties")
def api_update_folder_properties(
    payload: FolderPropertiesPayload,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    with storage_write_lock():
        folder = get_folder_by_path(db, payload.path)
        if not folder:
            raise HTTPException(status_code=404, detail="Folder not found")
        require_folder_access(folder, user, db, 3)
        folder.color = sanitize_folder_color(payload.color)
        folder.icon = sanitize_folder_icon(payload.icon)
        record_folder_event(folder, user, "metadata", "Updated folder appearance", db)
        record_folder_change(db, "properties")
        commit_state(db)
        return folder_properties_payload(folder, db, user)


@router.put("/api/folders/retention")
def api_update_folder_retention(
    payload: FolderRetentionPayload,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    days, action = sanitize_ttl_policy(payload.default_ttl_days, payload.default_ttl_action)
    with storage_write_lock():
        folder = get_folder_by_path(db, payload.path)
        if not folder:
            raise HTTPException(status_code=404, detail="Folder not found")
        require_folder_access(folder, user, db, 3)
        if action == "delete" and not user["is_admin"]:
            raise HTTPException(status_code=403, detail="Admin access required for delete TTL")
        require_folder_subtree_access(folder, user, db, 3)
        folder.default_ttl_days = days
        folder.default_ttl_action = action
        reapply_ttl_for_folder_subtree(folder, db)
        record_folder_event(folder, user, "retention", "Updated folder retention policy", db)
        record_folder_change(db, "retention", include_document_updates=True)
        commit_state(db)
        return folder_properties_payload(folder, db, user)


@router.put("/api/folders/permissions")
def api_update_folder_permissions(
    payload: FolderPermissionsPayload,
    user: UserContext = Depends(require_admin),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    with storage_write_lock():
        folder = get_folder_by_path(db, payload.path)
        if not folder:
            raise HTTPException(status_code=404, detail="Folder not found")
        seen: set[int] = set()
        groups_by_id = {group.id: group for group in db.execute(select(VaultGroup)).scalars().all()}
        existing = {
            permission.group_id: permission
            for permission in db.execute(
                select(FolderPermission).where(FolderPermission.folder_id == folder.id),
            )
            .scalars()
            .all()
        }
        for row in payload.permissions:
            validate_permission_flags(row.can_view, row.can_read, row.can_write)
            if row.group_id in seen:
                raise HTTPException(status_code=400, detail="Duplicate group permission")
            seen.add(row.group_id)
            if row.group_id not in groups_by_id:
                raise HTTPException(status_code=404, detail="Group not found")
            permission = existing.get(row.group_id)
            if not permission:
                permission = FolderPermission(folder_id=folder.id, group_id=row.group_id)
                db.add(permission)
            permission.can_view = row.can_view
            permission.can_read = row.can_read
            permission.can_write = row.can_write
            permission.updated_at = now_utc()
        for group_id, permission in existing.items():
            if group_id not in seen:
                db.delete(permission)
        record_folder_event(folder, user, "permissions", "Updated folder permissions", db)
        record_folder_change(db, "permissions")
        commit_state(db)
        return folder_properties_payload(folder, db, user)


@router.get("/api/documents/{doc_id}/detail")
def api_document_detail(
    doc_id: int,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    commit_state(db)
    doc = get_document_or_404(doc_id, db)
    require_document_access(doc, user, db, 2)
    return document_detail_payload(doc, user, db)


@router.get("/api/my-edits")
def api_my_edits(
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    commit_state(db)
    return build_my_edits_payload(user, db)


@router.get("/api/events/stream")
async def api_events_stream(
    request: Request,
    user: UserContext = Depends(current_user),
) -> StreamingResponse:
    del user
    stream_generation = debug_event_stream_generation()
    header_value = request.headers.get("last-event-id") or request.headers.get("Last-Event-ID")
    try:
        last_id = int(header_value) if header_value else latest_state_event_id()
    except ValueError:
        last_id = latest_state_event_id()

    async def event_generator() -> object:
        nonlocal last_id
        heartbeat_interval = 25.0
        last_heartbeat = dt.datetime.now(tz=dt.UTC)
        while not await request.is_disconnected():
            retry_ms = debug_event_stream_retry_ms(stream_generation)
            if retry_ms is not None:
                yield f"retry: {retry_ms}\n\n"
                return
            events = state_events_after(last_id)
            if events:
                for event in events:
                    last_id = event.id
                    yield (
                        f"id: {event.id}\n"
                        "event: state\n"
                        f"data: {json.dumps(event.payload)}\n\n"
                    )
                last_heartbeat = dt.datetime.now(tz=dt.UTC)
            now = dt.datetime.now(tz=dt.UTC)
            if (now - last_heartbeat).total_seconds() >= heartbeat_interval:
                yield ": heartbeat\n\n"
                last_heartbeat = now
            await asyncio.sleep(0.5)

    return StreamingResponse(event_generator(), media_type="text/event-stream")


@router.post("/folders")
def create_folder(
    folder: str = Form(...),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    normalized = normalize_folder(folder)
    if not normalized:
        raise HTTPException(status_code=400, detail="Folder path is required")
    ensure_folder_creation_path(normalized)
    with storage_write_lock():
        if get_folder_by_path(db, normalized):
            raise HTTPException(status_code=400, detail="Folder already exists")
        parent_path = "/".join(normalized.split("/")[:-1])
        require_write_for_folder_path(db, parent_path, user)
        name = normalize_item_name(normalized.split("/")[-1], "Folder name")
        parent = get_or_create_folder_path(db, parent_path)
        ensure_unique_folder_name(db, parent.id, name)
        created = Folder(
            root_key=parent.root_key,
            parent_id=parent.id,
            name=name,
            is_root=False,
            created_by=user["id"],
            created_by_name=user["name"],
        )
        db.add(created)
        db.flush()
        record_folder_event(created, user, "create", f"Created {normalized}", db)
        record_folder_change(db, "created")
        db.commit()
    return {"folder": normalized, "id": created.id}


@router.post("/api/move")
def move_items(
    payload: ActionPayload,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, list[dict[str, object]]]:
    destination = normalize_folder(payload.destination_folder)
    items = normalize_action_items(require_action_items(payload), db)
    result = bulk_result()
    changed = False
    with storage_write_lock():
        items = prune_nested_action_items(items, db)
        for item in items:
            try:
                with db.begin_nested():
                    if item.type == "document":
                        doc = get_document_or_404(item.id or 0, db)
                        detail = move_doc_item(doc, destination, request, user, db)
                    else:
                        folder_item = get_folder_for_action(item, db)
                        detail = move_folder_item(folder_item, destination, user, db)
                result["ok"].append(action_result(item, detail))
                changed = True
            except HTTPException as exc:
                result["failed"].append(action_result(item, response_detail(exc)))
        if changed:
            batch_state_changed(db, "move")
        db.commit()
    return result


@router.post("/api/rename")
def rename_item(
    payload: ActionPayload,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, list[dict[str, object]]]:
    items = normalize_action_items(require_action_items(payload), db)
    if len(items) != 1:
        raise HTTPException(status_code=400, detail="Rename exactly one item")
    if not payload.name:
        raise HTTPException(status_code=400, detail="Name is required")
    result = bulk_result()
    item = items[0]
    with storage_write_lock():
        try:
            with db.begin_nested():
                if item.type == "document":
                    doc = get_document_or_404(item.id or 0, db)
                    destination_folder = (
                        normalize_folder(payload.destination_folder)
                        if payload.destination_folder is not None
                        else document_folder_path(doc)
                    )
                    detail = move_doc_item(
                        doc,
                        destination_folder,
                        request,
                        user,
                        db,
                        name=payload.name,
                    )
                else:
                    folder_item = get_folder_for_action(item, db)
                    destination_folder = (
                        normalize_folder(payload.destination_folder)
                        if payload.destination_folder is not None
                        else (folder_path(folder_item.parent) if folder_item.parent else "")
                    )
                    detail = move_folder_item(
                        folder_item,
                        destination_folder,
                        user,
                        db,
                        name=payload.name,
                    )
                batch_state_changed(db, "rename")
            result["ok"].append(action_result(item, detail))
        except HTTPException as exc:
            result["failed"].append(action_result(item, response_detail(exc)))
        db.commit()
    return result


@router.post("/api/archive")
def archive_items(
    payload: ActionPayload,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, list[dict[str, object]]]:
    items = normalize_action_items(require_action_items(payload), db)
    result = bulk_result()
    changed = False
    with storage_write_lock():
        items = prune_nested_action_items(items, db)
        for item in items:
            try:
                with db.begin_nested():
                    if item.type == "document":
                        doc = get_document_or_404(item.id or 0, db)
                        detail = archive_doc_item(doc, request, user, db)
                    else:
                        folder_item = get_folder_for_action(item, db)
                        detail = archive_folder_item(folder_item, request, user, db)
                result["ok"].append(action_result(item, detail))
                changed = True
            except HTTPException as exc:
                result["failed"].append(action_result(item, response_detail(exc)))
        if changed:
            batch_state_changed(db, "archive")
        db.commit()
    return result


@router.post("/api/restore")
def restore_items(
    payload: ActionPayload,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, list[dict[str, object]]]:
    items = normalize_action_items(require_action_items(payload), db)
    result = bulk_result()
    changed = False
    with storage_write_lock():
        items = prune_nested_action_items(items, db)
        for item in items:
            try:
                with db.begin_nested():
                    if item.type == "document":
                        doc = get_document_or_404(item.id or 0, db)
                        detail = restore_doc_item(doc, request, user, db)
                    else:
                        raise HTTPException(
                            status_code=400,
                            detail="Restore archived files, not folders",
                        )
                result["ok"].append(action_result(item, detail))
                changed = True
            except HTTPException as exc:
                result["failed"].append(action_result(item, response_detail(exc)))
        if changed:
            batch_state_changed(db, "restore")
        db.commit()
    return result


@router.post("/api/delete-forever")
def delete_items_forever(
    payload: ActionPayload,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, list[dict[str, object]]]:
    if archive_permanent_delete_admin_only(db) and not user["is_admin"]:
        raise HTTPException(status_code=403, detail="Admin access required")
    items = normalize_action_items(require_action_items(payload), db)
    result = bulk_result()
    changed = False
    with storage_write_lock():
        items = prune_nested_action_items(items, db)
        for item in items:
            try:
                with db.begin_nested():
                    if item.type == "document":
                        doc = get_document_or_404(item.id or 0, db)
                        refresh_document_location(doc, db)
                        if not user["is_admin"]:
                            require_document_access(doc, user, db, 3)
                        if not document_is_archive(doc):
                            raise HTTPException(
                                status_code=400,
                                detail="Move the document to Archive before deleting",
                            )
                        ensure_not_locked_by_other(doc, user, db)
                        detail = document_path(doc)
                        db.delete(doc)
                    else:
                        raise HTTPException(
                            status_code=400,
                            detail="Delete forever is only available for archived files",
                        )
                result["ok"].append(action_result(item, detail))
                changed = True
            except HTTPException as exc:
                result["failed"].append(action_result(item, response_detail(exc)))
        if changed:
            record_document_deleted(db)
        db.commit()
    return result


@router.post("/api/lock")
def lock_items(
    payload: ActionPayload,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, list[dict[str, object]]]:
    items = normalize_action_items(require_action_items(payload), db)
    result = bulk_result()
    changed = False
    with storage_write_lock():
        for item in items:
            try:
                with db.begin_nested():
                    if item.type != "document":
                        raise HTTPException(status_code=400, detail="Only files can be locked")
                    doc = get_document_or_404(item.id or 0, db)
                    refresh_editable_document(doc, db)
                    require_document_access(doc, user, db, 3)
                    lock, created = acquire_document_lock(doc, user, client_meta(request), db)
                    if created:
                        record_event(
                            doc,
                            user,
                            "lock",
                            f"Locked {document_path(doc)}",
                            db,
                            meta=client_meta(request),
                            publish_state=False,
                        )
                    detail = lock.locked_by_name or lock.locked_by
                result["ok"].append(action_result(item, detail))
                changed = True
            except HTTPException as exc:
                result["failed"].append(action_result(item, response_detail(exc)))
        if changed:
            batch_state_changed(db, "lock")
        db.commit()
    return result


@router.post("/api/unlock")
def unlock_items(
    payload: ActionPayload,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, list[dict[str, object]]]:
    items = normalize_action_items(require_action_items(payload), db)
    result = bulk_result()
    changed = False
    with storage_write_lock():
        for item in items:
            try:
                with db.begin_nested():
                    if item.type != "document":
                        raise HTTPException(status_code=400, detail="Only files can be unlocked")
                    doc = get_document_or_404(item.id or 0, db)
                    require_document_access(doc, user, db, 3)
                    lock = ensure_not_locked_by_other(doc, user, db)
                    if lock is None:
                        raise HTTPException(status_code=400, detail="Document is not locked")
                    release_lock(lock, user)
                    record_event(
                        doc,
                        user,
                        "release",
                        f"Released lock for {document_path(doc)}",
                        db,
                        meta=client_meta(request),
                        publish_state=False,
                    )
                    detail = "Unlocked"
                result["ok"].append(action_result(item, detail))
                changed = True
            except HTTPException as exc:
                result["failed"].append(action_result(item, response_detail(exc)))
        if changed:
            batch_state_changed(db, "unlock")
        db.commit()
    return result


@router.post("/api/download")
def download_items(
    payload: ActionPayload,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> Response:
    with storage_write_lock():
        items = normalize_action_items(require_action_items(payload), db)
        docs_to_download: list[Document] = []
        for item in items:
            if item.type == "document":
                doc = get_document_or_404(item.id or 0, db)
                require_document_access(doc, user, db, 2)
                docs_to_download.append(doc)
            else:
                folder_item = get_folder_for_action(item, db)
                require_folder_access(folder_item, user, db, 2)
                docs_to_download.extend(readable_docs_in_folder_subtree(db, folder_item, user))
        unique_docs = list({doc.id: doc for doc in docs_to_download}.values())
        for doc in unique_docs:
            require_document_access(doc, user, db, 2)
        if len(unique_docs) == 1 and len(items) == 1 and items[0].type == "document":
            doc = unique_docs[0]
            version = current_version(doc, db)
            if not version:
                raise HTTPException(status_code=404, detail="Document has no versions")
            data = read_authorized_version_bytes(doc, version, user, db)
            record_event(
                doc,
                user,
                "download",
                f"Downloaded {document_path(doc)}",
                db,
                meta=client_meta(request),
                publish_state=False,
            )
            record_state_change(db, "document.download", ("document_detail",))
            db.commit()
            return download_response(data, doc.name, version.mime_type)

        buffer = io.BytesIO()
        errors: list[str] = []
        written: set[str] = set()
        with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
            for doc in unique_docs:
                archive_name = document_path(doc) or doc.name
                if archive_name in written:
                    archive_name = f"{doc.id}-{archive_name}"
                try:
                    version = current_version(doc, db)
                    if not version:
                        raise HTTPException(status_code=404, detail="Document has no versions")
                    archive.writestr(
                        archive_name,
                        read_authorized_version_bytes(doc, version, user, db),
                    )
                    written.add(archive_name)
                    record_event(
                        doc,
                        user,
                        "download",
                        f"Downloaded {document_path(doc)}",
                        db,
                        meta=client_meta(request),
                        publish_state=False,
                    )
                except HTTPException as exc:
                    errors.append(f"{archive_name}: {response_detail(exc)}")
            if errors:
                archive.writestr("vault-download-errors.txt", "\n".join(errors))
        if written:
            record_state_change(db, "document.download", ("document_detail",))
        db.commit()
        return download_response(buffer.getvalue(), "vault-download.zip", "application/zip")


@router.post("/documents")
async def create_document(
    request: Request,
    file: UploadFile = File(...),
    folder: str = Form(""),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    filename = normalize_item_name(file.filename, "File name")
    folder_path_value = normalize_folder(folder)
    ensure_document_upload_folder(folder_path_value)
    data = await file.read()
    mime_type = sanitize_mime_type(file.content_type, filename)
    meta = client_meta(request)
    with storage_write_lock():
        require_write_for_folder_path(db, folder_path_value, user)
        target_folder = get_or_create_folder_path(db, folder_path_value)
        ensure_unique_document_path(db, target_folder.id, filename)
        blob = get_or_create_blob_for_data(db, data, mime_type)
        doc = Document(
            folder_id=target_folder.id,
            name=filename,
            created_by=user["id"],
            created_by_name=user["name"],
            latest_modified_by=user["id"],
            latest_modified_at=now_utc(),
        )
        apply_folder_ttl(doc, target_folder, doc.latest_modified_at)
        db.add(doc)
        db.flush()
        create_document_version(
            db,
            doc,
            blob,
            user,
            meta,
            filename,
            mime_type,
            f"Uploaded {filename}",
            "upload",
        )
        db.commit()
    return {"id": doc.id, "path": join_path(folder_path_value, filename)}


@router.get("/documents/{doc_id}")
def document_detail(
    doc_id: int,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> RedirectResponse:
    doc = get_document_or_404(doc_id, db)
    require_document_access(doc, user, db, 1)
    return RedirectResponse(url="/", status_code=303)


@router.get("/documents/{doc_id}/checkout")
def checkout_document(
    doc_id: int,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> Response:
    doc = get_document_or_404(doc_id, db)
    require_document_access(doc, user, db, 3)
    if document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Restore this file before editing")
    meta = client_meta(request)
    with storage_write_lock():
        refresh_editable_document(doc, db)
        require_document_access(doc, user, db, 3)
        version = current_version(doc, db)
        if not version:
            raise HTTPException(status_code=404, detail="Document has no versions")
        acquire_document_lock(doc, user, meta, db)
        record_event(doc, user, "checkout", f"Checked out {document_path(doc)}", db, meta=meta)
        data = read_version_bytes(version)
        db.commit()
    return download_response(data, doc.name, version.mime_type)


@router.post("/documents/{doc_id}/checkin")
async def checkin_document(
    doc_id: int,
    request: Request,
    file: UploadFile = File(...),
    note: str = Form(""),
    rename_to_upload: bool = Form(False),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    doc = get_document_or_404(doc_id, db)
    require_document_access(doc, user, db, 3)
    if document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Restore this file before editing")
    lock = get_active_lock(doc, db)
    if not lock or lock.locked_by != user["id"]:
        raise HTTPException(
            status_code=403,
            detail="Check out the file before uploading a new version",
        )
    upload_name = normalize_item_name(file.filename, "File name")
    data = await file.read()
    mime_type = sanitize_mime_type(file.content_type, upload_name)
    meta = client_meta(request)
    message = note.strip() or f"Uploaded {upload_name}"
    with storage_write_lock():
        refresh_editable_document(doc, db)
        require_document_access(doc, user, db, 3)
        lock = get_active_lock(doc, db)
        if not lock or lock.locked_by != user["id"]:
            raise HTTPException(
                status_code=403,
                detail="Check out the file before uploading a new version",
            )
        if rename_to_upload and upload_name != doc.name:
            ensure_unique_document_path(db, doc.folder_id, upload_name, doc.id)
            record_event(
                doc,
                user,
                "move",
                f"Renamed {doc.name} to {upload_name}",
                db,
                meta=meta,
            )
            doc.name = upload_name
        blob = get_or_create_blob_for_data(db, data, mime_type)
        version = create_document_version(
            db,
            doc,
            blob,
            user,
            meta,
            upload_name,
            mime_type,
            message,
            "checkin",
        )
        release_lock(lock, user)
        record_event(doc, user, "release", f"Released lock for {document_path(doc)}", db, meta=meta)
        db.commit()
    return {"id": doc.id, "version": version.id, "path": document_path(doc)}


@router.get("/documents/{doc_id}/versions/{version_id}/download")
def download_version(
    doc_id: int,
    version_id: str,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> Response:
    with storage_write_lock():
        doc = get_document_or_404(doc_id, db)
        require_document_access(doc, user, db, 2)
        version = get_version_or_404(doc, version_id, db)
        data = read_authorized_version_bytes(doc, version, user, db)
        filename = version.original_filename or doc.name
        record_event(
            doc,
            user,
            "download",
            f"Downloaded version v{version.version_number} of {document_path(doc)}",
            db,
            meta=client_meta(request),
        )
        db.commit()
        return download_response(data, filename, version.mime_type)
