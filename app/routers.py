"""HTTP routes for the vault service."""

import asyncio
import datetime as dt
import hashlib
import json
import logging
import mimetypes
import re
import secrets
import shutil
import tempfile
import threading
import time
import uuid
import zipfile
from collections import defaultdict
from collections.abc import AsyncIterator, Callable, Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal, cast
from urllib.parse import quote

from fastapi import (
    APIRouter,
    Depends,
    File,
    Form,
    Header,
    HTTPException,
    Request,
    Response,
    UploadFile,
)
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse, StreamingResponse
from fastapi.templating import Jinja2Templates
from pydantic import BaseModel, Field
from sqlalchemy import delete, select
from sqlalchemy.orm import Session

from . import db as db_runtime
from .assets import static_asset_path
from .auth import (
    UserContext,
    current_user,
    logout_response,
    oidc_callback_response,
    oidc_login_response,
    require_admin,
    vault_user_is_effective_admin,
)
from .config import (
    AUTH_MODE,
    BASE_DOMAIN,
    DEV_MODE,
    EXPORT_TTL_SECONDS,
    EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES,
    EXPORT_ZIP_COMPRESSLEVEL,
    MAX_UPLOAD_BYTES,
    PUBLIC_URL,
    SITE_NAME,
    TRANSFER_CHUNK_BYTES,
    TRANSFER_SESSION_TTL_SECONDS,
    TRANSFERS_PATH,
    TTL_SWEEP_INTERVAL_SECONDS,
)
from .db import SessionLocal, get_db
from .models import (
    Blob,
    BlobLocation,
    Document,
    DocumentEvent,
    DocumentLock,
    DocumentVersion,
    ExportArtifact,
    ExportJob,
    Folder,
    FolderEvent,
    FolderPermission,
    ShareLink,
    StateEvent,
    UploadPart,
    UploadSession,
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
    BlobReader,
    StorageChecksumMismatch,
    StorageConfigurationError,
    StorageError,
    StorageNotFoundError,
    StorageProgressCallback,
    StoredBlob,
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
SYSTEM_META: dict[str, str | None] = {"ip": None, "user_agent": None}
_ttl_sweeper_task: asyncio.Task[None] | None = None
_debug_event_stream_generation = 0
_debug_event_stream_retry_ms = 3000
STREAM_CHUNK_SIZE = 8 * 1024 * 1024
ResolvedFavoriteTarget = tuple[Literal["folder"], Folder] | tuple[Literal["document"], Document]


def configure_router_runtime(
    *,
    auth_mode: str | None = None,
    base_domain: str | None = None,
    dev_mode: bool | None = None,
    export_ttl_seconds: int | None = None,
    export_zip_compression_threshold_bytes: int | None = None,
    export_zip_compresslevel: int | None = None,
    max_upload_bytes: int | None = None,
    public_url: str | None = None,
    site_name: str | None = None,
    transfer_chunk_bytes: int | None = None,
    transfers_path: str | Path | None = None,
    transfer_session_ttl_seconds: int | None = None,
    ttl_sweep_interval_seconds: int | None = None,
) -> None:
    """Configure process-local route globals that are normally loaded from env."""
    global AUTH_MODE, BASE_DOMAIN, DEV_MODE, EXPORT_TTL_SECONDS, MAX_UPLOAD_BYTES, PUBLIC_URL
    global EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES, EXPORT_ZIP_COMPRESSLEVEL
    global SITE_NAME
    global TRANSFER_CHUNK_BYTES, TRANSFERS_PATH, TRANSFER_SESSION_TTL_SECONDS
    global TTL_SWEEP_INTERVAL_SECONDS
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
    if max_upload_bytes is not None:
        MAX_UPLOAD_BYTES = max(1, int(max_upload_bytes))
        config.MAX_UPLOAD_BYTES = MAX_UPLOAD_BYTES
    if public_url is not None:
        PUBLIC_URL = public_url.strip().rstrip("/")
        config.PUBLIC_URL = PUBLIC_URL
    if transfer_chunk_bytes is not None:
        TRANSFER_CHUNK_BYTES = max(1, int(transfer_chunk_bytes))
        config.TRANSFER_CHUNK_BYTES = TRANSFER_CHUNK_BYTES
    if transfers_path is not None:
        TRANSFERS_PATH = Path(transfers_path).resolve()
        config.TRANSFERS_PATH = TRANSFERS_PATH
    if transfer_session_ttl_seconds is not None:
        TRANSFER_SESSION_TTL_SECONDS = max(60, int(transfer_session_ttl_seconds))
        config.TRANSFER_SESSION_TTL_SECONDS = TRANSFER_SESSION_TTL_SECONDS
    if export_ttl_seconds is not None:
        EXPORT_TTL_SECONDS = max(60, int(export_ttl_seconds))
        config.EXPORT_TTL_SECONDS = EXPORT_TTL_SECONDS
    if export_zip_compression_threshold_bytes is not None:
        EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES = max(0, int(export_zip_compression_threshold_bytes))
        config.EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES = EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES
    if export_zip_compresslevel is not None:
        EXPORT_ZIP_COMPRESSLEVEL = min(9, max(1, int(export_zip_compresslevel)))
        config.EXPORT_ZIP_COMPRESSLEVEL = EXPORT_ZIP_COMPRESSLEVEL
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


@dataclass(frozen=True)
class UploadSpool:
    path: Path
    digest: str
    size_bytes: int

    def cleanup(self) -> None:
        self.path.unlink(missing_ok=True)


@dataclass(frozen=True)
class TempDownload:
    path: Path
    size_bytes: int

    def cleanup(self) -> None:
        self.path.unlink(missing_ok=True)


class ExportCancelled(Exception):
    """Raised when an export job is cancelled while writing bytes."""


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


class UploadSessionPayload(BaseModel):
    mode: Literal["create", "checkin"] = "create"
    filename: str
    size_bytes: int
    mime_type: str | None = None
    folder: str = ""
    document_id: int | None = None
    note: str | None = None
    rename_to_upload: bool = False


class CompleteUploadPayload(BaseModel):
    sha256: str | None = None


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
    folder_id = cast(int | None, folder.id)
    if folder_id is None:
        db.flush()
        folder_id = folder.id
    groups = list(db.execute(select(VaultGroup)).scalars().all())
    for group in groups:
        db.add(
            FolderPermission(
                folder_id=folder_id,
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


async def spool_upload_file(file: UploadFile) -> UploadSpool:
    temp_file = tempfile.NamedTemporaryFile(prefix="vault-upload-", delete=False)
    temp_path = Path(temp_file.name)
    digest = hashlib.sha256()
    size_bytes = 0
    try:
        with temp_file:
            while True:
                try:
                    chunk = await file.read(STREAM_CHUNK_SIZE)
                except Exception as exc:
                    raise HTTPException(
                        status_code=400,
                        detail="Upload failed while reading request body",
                    ) from exc
                if not chunk:
                    break
                size_bytes += len(chunk)
                if size_bytes > MAX_UPLOAD_BYTES:
                    raise HTTPException(
                        status_code=413,
                        detail=f"Upload exceeds limit of {MAX_UPLOAD_BYTES} bytes",
                    )
                digest.update(chunk)
                temp_file.write(chunk)
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise
    return UploadSpool(path=temp_path, digest=digest.hexdigest(), size_bytes=size_bytes)


def get_or_create_blob_for_stored_blob(db: Session, stored: StoredBlob) -> Blob:
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

    return get_or_create_blob_for_stored_blob(db, stored)


def get_or_create_blob_for_upload(
    db: Session,
    upload: UploadSpool,
    content_type: str | None,
) -> Blob:
    try:
        stored = get_storage_backend().put_file(
            upload.path,
            upload.digest,
            upload.size_bytes,
            content_type,
        )
    except StorageConfigurationError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc
    except StorageError as exc:
        raise HTTPException(status_code=500, detail="Storage write failed") from exc
    return get_or_create_blob_for_stored_blob(db, stored)


def upload_part_paths(session: UploadSession) -> list[Path]:
    paths: list[Path] = []
    for part_number in range(1, session.part_count + 1):
        part_path = upload_part_path(session.id, part_number)
        if not part_path.exists():
            raise HTTPException(status_code=400, detail="Upload session has missing parts")
        paths.append(part_path)
    return paths


def get_or_create_blob_for_upload_session(
    db: Session,
    session: UploadSession,
    content_type: str | None,
    expected_sha256: str | None,
    progress_callback: StorageProgressCallback | None = None,
) -> Blob:
    try:
        stored = get_storage_backend().put_part_files(
            upload_part_paths(session),
            content_type,
            expected_sha256,
            progress_callback,
        )
    except StorageChecksumMismatch as exc:
        raise HTTPException(status_code=400, detail="Upload checksum mismatch") from exc
    except StorageConfigurationError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc
    except StorageError as exc:
        raise HTTPException(status_code=500, detail="Storage write failed") from exc
    if stored.size_bytes != session.total_size:
        raise HTTPException(status_code=400, detail="Upload size does not match session")
    return get_or_create_blob_for_stored_blob(db, stored)


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


def copy_version_to_temp(version: DocumentVersion) -> TempDownload:
    location = location_for_blob(version.blob)
    temp_file = tempfile.NamedTemporaryFile(prefix="vault-download-", delete=False)
    temp_path = Path(temp_file.name)
    digest = hashlib.sha256()
    size_bytes = 0
    try:
        with temp_file:
            try:
                with get_storage_backend(location.backend).open_reader(
                    location.object_key,
                    location.bucket,
                ) as reader:
                    while True:
                        chunk = reader.read(STREAM_CHUNK_SIZE)
                        if not chunk:
                            break
                        size_bytes += len(chunk)
                        digest.update(chunk)
                        temp_file.write(chunk)
            except StorageNotFoundError as exc:
                raise HTTPException(status_code=404, detail="Blob missing from storage") from exc
            except StorageConfigurationError as exc:
                raise HTTPException(status_code=500, detail=str(exc)) from exc
            except StorageError as exc:
                raise HTTPException(status_code=500, detail="Storage read failed") from exc
            except OSError as exc:
                raise HTTPException(status_code=500, detail="Storage read failed") from exc
        if (
            version.blob.hash_algo != "sha256"
            or size_bytes != version.blob.size_bytes
            or digest.hexdigest() != version.blob.hash
        ):
            raise HTTPException(status_code=500, detail="Blob content does not match metadata")
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise
    return TempDownload(path=temp_path, size_bytes=size_bytes)


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


def copy_authorized_version_to_temp(
    doc: Document,
    version: DocumentVersion,
    user: UserContext,
    db: Session,
) -> TempDownload:
    temp_download = copy_version_to_temp(version)
    try:
        refresh_document_location(doc, db)
        require_document_access(doc, user, db, 2)
    except Exception:
        temp_download.cleanup()
        raise
    return temp_download


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


def safe_download_name(filename: str) -> str:
    return (
        "".join(
            "_" if ord(char) < 32 or ord(char) == 127 else char
            for char in filename.replace('"', "")
        ).strip()
        or "download"
    )


def download_headers(filename: str, content_length: int | None = None) -> dict[str, str]:
    safe_name = safe_download_name(filename)
    ascii_name = "".join(char if 32 <= ord(char) < 127 else "_" for char in safe_name).strip()
    ascii_name = ascii_name or "download"
    disposition = f"attachment; filename=\"{ascii_name}\"; filename*=UTF-8''{quote(safe_name)}"
    headers = {"Content-Disposition": disposition}
    if content_length is not None:
        headers["Content-Length"] = str(content_length)
    return headers


def iter_temp_file(path: Path) -> Iterator[bytes]:
    try:
        with path.open("rb") as source:
            while True:
                chunk = source.read(STREAM_CHUNK_SIZE)
                if not chunk:
                    break
                yield chunk
    finally:
        path.unlink(missing_ok=True)


def streaming_download_response(
    temp_download: TempDownload,
    filename: str,
    mime_type: str | None = None,
) -> StreamingResponse:
    safe_name = safe_download_name(filename)
    content_type = sanitize_mime_type(mime_type, safe_name)
    return StreamingResponse(
        iter_temp_file(temp_download.path),
        media_type=content_type,
        headers=download_headers(safe_name, temp_download.size_bytes),
    )


def write_temp_file_to_zip(
    archive: zipfile.ZipFile,
    archive_name: str,
    temp_download: TempDownload,
) -> None:
    with temp_download.path.open("rb") as source, archive.open(archive_name, "w") as target:
        while True:
            chunk = source.read(STREAM_CHUNK_SIZE)
            if not chunk:
                break
            target.write(chunk)


def download_response(data: bytes, filename: str, mime_type: str | None = None) -> Response:
    safe_name = safe_download_name(filename)
    content_type = sanitize_mime_type(mime_type, safe_name)
    return Response(
        content=data,
        media_type=content_type,
        headers=download_headers(safe_name, len(data)),
    )


@dataclass(frozen=True)
class ByteRange:
    start: int
    end: int
    status_code: int


def blob_etag(blob: Blob) -> str:
    return f'"{blob.hash_algo}-{blob.hash}-{blob.size_bytes}"'


def parse_range_header(
    range_header: str | None,
    if_range: str | None,
    size_bytes: int,
    etag: str,
) -> ByteRange:
    if size_bytes <= 0:
        return ByteRange(start=0, end=-1, status_code=200)
    if if_range and if_range.strip() != etag:
        range_header = None
    if not range_header:
        return ByteRange(start=0, end=size_bytes - 1, status_code=200)
    value = range_header.strip()
    if not value.startswith("bytes=") or "," in value:
        raise HTTPException(
            status_code=416,
            detail="Invalid byte range",
            headers={"Content-Range": f"bytes */{size_bytes}"},
        )
    spec = value.removeprefix("bytes=").strip()
    if "-" not in spec:
        raise HTTPException(
            status_code=416,
            detail="Invalid byte range",
            headers={"Content-Range": f"bytes */{size_bytes}"},
        )
    raw_start, raw_end = spec.split("-", 1)
    try:
        if raw_start == "":
            suffix_length = int(raw_end)
            if suffix_length <= 0:
                raise ValueError
            start = max(size_bytes - suffix_length, 0)
            end = size_bytes - 1
        else:
            start = int(raw_start)
            end = int(raw_end) if raw_end else size_bytes - 1
    except ValueError as exc:
        raise HTTPException(
            status_code=416,
            detail="Invalid byte range",
            headers={"Content-Range": f"bytes */{size_bytes}"},
        ) from exc
    if start < 0 or end < start or start >= size_bytes:
        raise HTTPException(
            status_code=416,
            detail="Invalid byte range",
            headers={"Content-Range": f"bytes */{size_bytes}"},
        )
    return ByteRange(start=start, end=min(end, size_bytes - 1), status_code=206)


def iter_blob_reader(reader: object, close: object, limit: int) -> Iterator[bytes]:
    remaining = limit
    try:
        while remaining > 0:
            chunk_size = min(STREAM_CHUNK_SIZE, remaining)
            chunk = cast(BlobReader, reader).read(chunk_size)
            if not chunk:
                break
            remaining -= len(chunk)
            yield chunk
    finally:
        cast(Any, close)(None, None, None)


def blob_streaming_response(
    blob: Blob,
    filename: str,
    mime_type: str | None,
    request: Request,
) -> StreamingResponse:
    size_bytes = blob.size_bytes
    etag = blob_etag(blob)
    byte_range = parse_range_header(
        request.headers.get("range"),
        request.headers.get("if-range"),
        size_bytes,
        etag,
    )
    safe_name = safe_download_name(filename)
    headers = download_headers(
        safe_name,
        0 if byte_range.end < byte_range.start else byte_range.end - byte_range.start + 1,
    )
    headers["Accept-Ranges"] = "bytes"
    headers["Content-Encoding"] = "identity"
    headers["ETag"] = etag
    if byte_range.status_code == 206:
        headers["Content-Range"] = f"bytes {byte_range.start}-{byte_range.end}/{size_bytes}"
    if size_bytes <= 0:
        return StreamingResponse(
            iter(()),
            media_type=sanitize_mime_type(mime_type, safe_name),
            headers=headers,
            status_code=200,
        )
    location = location_for_blob(blob)
    try:
        context = get_storage_backend(location.backend).open_range_reader(
            location.object_key,
            byte_range.start,
            byte_range.end,
            location.bucket,
        )
        reader = context.__enter__()
    except StorageNotFoundError as exc:
        raise HTTPException(status_code=404, detail="Blob missing from storage") from exc
    except StorageConfigurationError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc
    except StorageError as exc:
        raise HTTPException(status_code=500, detail="Storage read failed") from exc
    return StreamingResponse(
        iter_blob_reader(
            reader,
            context.__exit__,
            byte_range.end - byte_range.start + 1,
        ),
        media_type=sanitize_mime_type(mime_type, safe_name),
        headers=headers,
        status_code=byte_range.status_code,
    )


def version_streaming_response(
    version: DocumentVersion,
    filename: str,
    mime_type: str | None,
    request: Request,
) -> StreamingResponse:
    return blob_streaming_response(version.blob, filename, mime_type, request)


def transfer_user_payload(user: UserContext) -> dict[str, object]:
    return {
        "id": user["id"],
        "vault_user_id": user.get("vault_user_id", 0),
        "issuer": user.get("issuer", ""),
        "subject": user.get("subject", ""),
        "name": user.get("name", ""),
        "email": user.get("email", ""),
        "groups": list(user.get("groups", [])),
        "is_admin": bool(user.get("is_admin", False)),
    }


def transfer_user_context(session: UploadSession | ExportJob) -> UserContext:
    payload = dict(session.user_context or {})
    raw_vault_user_id = payload.get("vault_user_id", 0) or 0
    raw_groups = payload.get("groups", [])
    groups = raw_groups if isinstance(raw_groups, list) else []
    return {
        "id": str(payload.get("id", session.created_by)),
        "vault_user_id": int(cast(int | str, raw_vault_user_id)),
        "issuer": str(payload.get("issuer", "")),
        "subject": str(payload.get("subject", "")),
        "name": str(payload.get("name", session.created_by_name or session.created_by)),
        "email": str(payload.get("email", "")),
        "groups": [str(group) for group in groups],
        "is_admin": bool(payload.get("is_admin", False)),
    }


def transfer_owner_required(owner_id: str, user: UserContext) -> None:
    if str(user["id"]) != owner_id and not user["is_admin"]:
        raise HTTPException(status_code=404, detail="Transfer not found")


def transfer_expires_at(seconds: int) -> dt.datetime:
    return now_utc() + dt.timedelta(seconds=seconds)


def upload_session_dir(session_id: str) -> Path:
    return TRANSFERS_PATH / "uploads" / session_id


def upload_part_path(session_id: str, part_number: int) -> Path:
    return upload_session_dir(session_id) / f"{part_number:08d}.part"


def clear_upload_session_files(session_id: str) -> None:
    shutil.rmtree(upload_session_dir(session_id), ignore_errors=True)


def clear_upload_session_parts(session: UploadSession) -> None:
    session.parts.clear()


def uploaded_parts_by_number(session: UploadSession) -> dict[int, UploadPart]:
    return {part.part_number: part for part in session.parts}


def upload_session_payload(session: UploadSession) -> dict[str, object]:
    uploaded_parts = [
        {
            "part_number": part.part_number,
            "offset": part.offset_bytes,
            "size_bytes": part.size_bytes,
            "sha256": part.sha256,
        }
        for part in sorted(session.parts, key=lambda item: item.part_number)
    ]
    uploaded_bytes = sum(part.size_bytes for part in session.parts)
    verification_total = session.verification_total_bytes
    verification_processed = session.verification_processed_bytes
    if session.status == "complete":
        verification_total = session.total_size
        verification_processed = session.total_size
    verification = (
        {
            "processed_bytes": min(verification_processed, verification_total),
            "total_bytes": verification_total,
        }
        if verification_total and session.status in {"completing", "complete"}
        else None
    )
    return {
        "id": session.id,
        "mode": session.mode,
        "status": session.status,
        "filename": session.filename,
        "size_bytes": session.total_size,
        "chunk_size": session.chunk_size,
        "part_count": session.part_count,
        "uploaded_bytes": uploaded_bytes,
        "uploaded_parts": uploaded_parts,
        "verification": verification,
        "expires_at": session.expires_at.isoformat() if session.expires_at else None,
        "result": upload_session_result(session),
    }


def upload_session_result(session: UploadSession) -> dict[str, object] | None:
    if session.status != "complete":
        return None
    return {
        "id": session.result_document_id,
        "version": session.result_version_id,
        "path": session.result_path,
    }


def expected_part_bounds(session: UploadSession, part_number: int) -> tuple[int, int]:
    if part_number < 1 or part_number > session.part_count:
        raise HTTPException(status_code=400, detail="Invalid part number")
    offset = (part_number - 1) * session.chunk_size
    size = min(session.chunk_size, session.total_size - offset)
    return offset, size


def ensure_active_upload_session(session: UploadSession, db: Session | None = None) -> None:
    if session.status != "active":
        raise HTTPException(status_code=409, detail=f"Upload session is {session.status}")
    expires_at = normalize_timestamp(session.expires_at)
    if expires_at is not None and expires_at <= now_utc():
        session.status = "expired"
        session.updated_at = now_utc()
        clear_upload_session_parts(session)
        clear_upload_session_files(session.id)
        if db is not None:
            db.commit()
        raise HTTPException(status_code=410, detail="Upload session expired")


async def spool_upload_part_body(
    request: Request,
    expected_size: int,
    expected_sha256: str | None,
    temp_dir: Path,
) -> tuple[Path, str, int]:
    temp_dir.mkdir(parents=True, exist_ok=True)
    temp_file = tempfile.NamedTemporaryFile(
        prefix="vault-upload-part-",
        dir=temp_dir,
        delete=False,
    )
    temp_path = Path(temp_file.name)
    digest = hashlib.sha256()
    size_bytes = 0
    try:
        with temp_file:
            async for chunk in request.stream():
                if not chunk:
                    continue
                size_bytes += len(chunk)
                if size_bytes > expected_size:
                    raise HTTPException(status_code=413, detail="Upload part is too large")
                digest.update(chunk)
                temp_file.write(chunk)
    except HTTPException:
        temp_path.unlink(missing_ok=True)
        raise
    except Exception as exc:
        temp_path.unlink(missing_ok=True)
        raise HTTPException(
            status_code=400, detail="Upload failed while reading request body"
        ) from exc
    actual_sha256 = digest.hexdigest()
    if size_bytes != expected_size:
        temp_path.unlink(missing_ok=True)
        raise HTTPException(status_code=400, detail="Upload part size does not match session")
    if expected_sha256 and actual_sha256 != expected_sha256.lower():
        temp_path.unlink(missing_ok=True)
        raise HTTPException(status_code=400, detail="Upload part checksum mismatch")
    return temp_path, actual_sha256, size_bytes


def mark_upload_session_failed(session_id: str, message: str) -> None:
    with SessionLocal() as db:
        session = db.get(UploadSession, session_id)
        if session and session.status not in {"complete", "aborted"}:
            session.status = "failed"
            session.error = message
            session.updated_at = now_utc()
            clear_upload_session_parts(session)
            db.commit()
            clear_upload_session_files(session.id)


def reserve_upload_completion(session: UploadSession, db: Session) -> None:
    ensure_active_upload_session(session, db)
    parts_by_number = uploaded_parts_by_number(session)
    missing = [
        part_number
        for part_number in range(1, session.part_count + 1)
        if part_number not in parts_by_number
    ]
    if missing:
        raise HTTPException(status_code=400, detail="Upload session has missing parts")
    session.status = "completing"
    session.verification_total_bytes = session.total_size
    session.verification_processed_bytes = 0
    session.updated_at = now_utc()
    db.commit()


def upload_verification_progress_callback(
    db: Session,
    session_id: str,
    total_size: int,
) -> StorageProgressCallback:
    last_processed = -1
    last_reported_at = 0.0
    minimum_step = max(STREAM_CHUNK_SIZE * 8, total_size // 100 if total_size else 0)

    def report(processed_bytes: int) -> None:
        nonlocal last_processed, last_reported_at
        processed = min(max(processed_bytes, 0), total_size)
        monotonic_now = time.monotonic()
        if (
            processed < total_size
            and processed - last_processed < minimum_step
            and monotonic_now - last_reported_at < 0.25
        ):
            return
        session = db.get(UploadSession, session_id)
        if not session or session.status != "completing":
            return
        session.verification_total_bytes = total_size
        session.verification_processed_bytes = processed
        session.updated_at = now_utc()
        db.commit()
        last_processed = processed
        last_reported_at = monotonic_now

    return report


def upload_completion_verification_multiplier() -> int:
    backend = get_storage_backend()
    return 2 if backend.name in {"s3", "r2"} else 1


def complete_upload_session_document(
    session: UploadSession,
    expected_sha256: str | None,
    user: UserContext,
    db: Session,
) -> dict[str, object]:
    actor = transfer_user_context(session)
    meta = {"ip": session.upload_ip, "user_agent": session.upload_user_agent}
    mime_type = sanitize_mime_type(session.mime_type, session.filename)
    verification_total = session.total_size * upload_completion_verification_multiplier()
    if session.mode == "create":
        folder_path_value = normalize_folder(session.folder_path or "")
        require_write_for_folder_path(db, folder_path_value, user)
        target_folder = get_or_create_folder_path(db, folder_path_value)
        ensure_unique_document_path(db, target_folder.id, session.filename)
        blob = get_or_create_blob_for_upload_session(
            db,
            session,
            mime_type,
            expected_sha256,
            upload_verification_progress_callback(db, session.id, verification_total),
        )
        doc = Document(
            folder_id=target_folder.id,
            name=session.filename,
            created_by=actor["id"],
            created_by_name=actor["name"],
            latest_modified_by=actor["id"],
            latest_modified_at=now_utc(),
        )
        apply_folder_ttl(doc, target_folder, doc.latest_modified_at)
        db.add(doc)
        db.flush()
        version = create_document_version(
            db,
            doc,
            blob,
            actor,
            meta,
            session.filename,
            mime_type,
            f"Uploaded {session.filename}",
            "upload",
        )
        result_path = join_path(folder_path_value, session.filename)
    elif session.mode == "checkin":
        doc = get_document_or_404(session.document_id or 0, db)
        refresh_editable_document(doc, db)
        require_document_access(doc, user, db, 3)
        lock = get_active_lock(doc, db)
        if not lock or lock.locked_by != user["id"]:
            raise HTTPException(
                status_code=403,
                detail="Check out the file before uploading a new version",
            )
        if session.rename_to_upload and session.filename != doc.name:
            ensure_unique_document_path(db, doc.folder_id, session.filename, doc.id)
            record_event(
                doc,
                actor,
                "move",
                f"Renamed {doc.name} to {session.filename}",
                db,
                meta=meta,
            )
            doc.name = session.filename
        blob = get_or_create_blob_for_upload_session(
            db,
            session,
            mime_type,
            expected_sha256,
            upload_verification_progress_callback(db, session.id, verification_total),
        )
        version = create_document_version(
            db,
            doc,
            blob,
            actor,
            meta,
            session.filename,
            mime_type,
            (session.note or "").strip() or f"Uploaded {session.filename}",
            "checkin",
        )
        release_lock(lock, actor)
        record_event(
            doc,
            actor,
            "release",
            f"Released lock for {document_path(doc)}",
            db,
            meta=meta,
        )
        result_path = document_path(doc)
    else:
        raise HTTPException(status_code=400, detail="Unsupported upload session mode")
    session.status = "complete"
    session.verification_total_bytes = session.total_size
    session.verification_processed_bytes = session.total_size
    session.completed_at = now_utc()
    session.updated_at = session.completed_at
    session.result_document_id = doc.id
    session.result_version_id = version.id
    session.result_path = result_path
    record_state_change(db, "document.upload.complete", ("contents", "sidebar", "document_detail"))
    db.commit()
    clear_upload_session_files(session.id)
    return {"id": doc.id, "version": version.id, "path": result_path}


def export_job_payload(job: ExportJob) -> dict[str, object]:
    artifact = job.artifacts[0] if job.artifacts else None
    return {
        "id": job.id,
        "status": job.status,
        "filename": job.filename,
        "total_items": job.total_items,
        "processed_items": job.processed_items,
        "total_bytes": job.total_bytes,
        "processed_bytes": job.processed_bytes,
        "error": job.error,
        "expires_at": job.expires_at.isoformat() if job.expires_at else None,
        "download_url": f"/api/exports/{job.id}/download" if artifact else None,
        "size_bytes": artifact.size_bytes if artifact else None,
    }


def hash_file(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    size_bytes = 0
    with path.open("rb") as source:
        while True:
            chunk = source.read(STREAM_CHUNK_SIZE)
            if not chunk:
                break
            size_bytes += len(chunk)
            digest.update(chunk)
    return digest.hexdigest(), size_bytes


def write_version_to_zip(
    archive: zipfile.ZipFile,
    archive_name: str,
    version: DocumentVersion,
    *,
    progress_callback: Callable[[int], None] | None = None,
    should_cancel: Callable[[], bool] | None = None,
) -> int:
    location = location_for_blob(version.blob)
    digest = hashlib.sha256()
    size_bytes = 0
    try:
        with get_storage_backend(location.backend).open_reader(
            location.object_key,
            location.bucket,
        ) as reader:
            with archive.open(archive_name, "w", force_zip64=True) as target:
                while True:
                    if should_cancel and should_cancel():
                        raise ExportCancelled
                    chunk = reader.read(STREAM_CHUNK_SIZE)
                    if not chunk:
                        break
                    size_bytes += len(chunk)
                    digest.update(chunk)
                    target.write(chunk)
                    if progress_callback:
                        progress_callback(len(chunk))
    except StorageNotFoundError as exc:
        raise HTTPException(status_code=404, detail="Blob missing from storage") from exc
    except StorageConfigurationError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc
    except StorageError as exc:
        raise HTTPException(status_code=500, detail="Storage read failed") from exc
    if (
        version.blob.hash_algo != "sha256"
        or size_bytes != version.blob.size_bytes
        or digest.hexdigest() != version.blob.hash
    ):
        raise HTTPException(status_code=500, detail="Blob content does not match metadata")
    return size_bytes


def export_zip_compression(total_bytes: int) -> int:
    if (
        EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES > 0
        and total_bytes >= EXPORT_ZIP_COMPRESSION_THRESHOLD_BYTES
    ):
        return zipfile.ZIP_DEFLATED
    return zipfile.ZIP_STORED


def export_docs_for_job(job: ExportJob, db: Session) -> list[Document]:
    raw_items_value = (job.request_payload or {}).get("items") or []
    raw_items = raw_items_value if isinstance(raw_items_value, list) else []
    action_items = [ActionItem(**item) for item in raw_items if isinstance(item, dict)]
    items = normalize_action_items(require_action_items(ActionPayload(items=action_items)), db)
    user = transfer_user_context(job)
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
    return unique_docs


def run_export_job(job_id: str) -> None:
    zip_temp = tempfile.NamedTemporaryFile(prefix="vault-export-", suffix=".zip", delete=False)
    zip_path = Path(zip_temp.name)
    zip_temp.close()
    try:
        with SessionLocal() as db:
            job = db.get(ExportJob, job_id)
            if not job or job.status != "queued":
                return
            export_job = job
            export_job.status = "running"
            export_job.updated_at = now_utc()
            db.commit()

            user = transfer_user_context(export_job)
            docs = export_docs_for_job(export_job, db)
            versions: list[tuple[Document, DocumentVersion]] = []
            total_bytes = 0
            for doc in docs:
                version = current_version(doc, db)
                if not version:
                    continue
                versions.append((doc, version))
                total_bytes += version.blob.size_bytes
            export_job.total_items = len(versions)
            export_job.total_bytes = total_bytes
            export_job.updated_at = now_utc()
            db.commit()

            errors: list[str] = []
            written: set[str] = set()
            pending_processed_bytes = 0
            last_cancel_check = 0.0
            last_progress_commit = 0.0

            def export_is_cancelled() -> bool:
                nonlocal last_cancel_check
                monotonic_now = time.monotonic()
                if monotonic_now - last_cancel_check < 0.25:
                    return export_job.status == "cancelled"
                db.refresh(export_job)
                last_cancel_check = monotonic_now
                return export_job.status == "cancelled"

            def flush_export_progress(force: bool = False) -> None:
                nonlocal last_progress_commit, pending_processed_bytes
                if pending_processed_bytes <= 0:
                    return
                monotonic_now = time.monotonic()
                if not force and monotonic_now - last_progress_commit < 0.25:
                    return
                export_job.processed_bytes += pending_processed_bytes
                pending_processed_bytes = 0
                export_job.updated_at = now_utc()
                db.commit()
                last_progress_commit = monotonic_now

            def record_export_progress(delta_bytes: int) -> None:
                nonlocal pending_processed_bytes
                pending_processed_bytes += delta_bytes
                flush_export_progress()

            compression = export_zip_compression(total_bytes)
            compresslevel = (
                EXPORT_ZIP_COMPRESSLEVEL if compression == zipfile.ZIP_DEFLATED else None
            )
            with zipfile.ZipFile(
                zip_path,
                "w",
                compression,
                allowZip64=True,
                compresslevel=compresslevel,
            ) as archive:
                for doc, version in versions:
                    if export_is_cancelled():
                        return
                    archive_name = document_path(doc) or doc.name
                    if archive_name in written:
                        archive_name = f"{doc.id}-{archive_name}"
                    try:
                        write_version_to_zip(
                            archive,
                            archive_name,
                            version,
                            progress_callback=record_export_progress,
                            should_cancel=export_is_cancelled,
                        )
                        flush_export_progress(force=True)
                        written.add(archive_name)
                        export_job.processed_items += 1
                        record_event(
                            doc,
                            user,
                            "download",
                            f"Exported {document_path(doc)}",
                            db,
                            meta=SYSTEM_META,
                            publish_state=False,
                        )
                    except HTTPException as exc:
                        errors.append(f"{archive_name}: {response_detail(exc)}")
                    export_job.updated_at = now_utc()
                    db.commit()
                if errors:
                    archive.writestr("vault-download-errors.txt", "\n".join(errors))
                export_job.status = "finalizing"
                export_job.updated_at = now_utc()
                db.commit()

            db.refresh(export_job)
            if export_job.status == "cancelled":
                return
            digest, size_bytes = hash_file(zip_path)
            db.refresh(export_job)
            if export_job.status == "cancelled":
                return
            blob = get_or_create_blob_for_upload(
                db,
                UploadSpool(path=zip_path, digest=digest, size_bytes=size_bytes),
                "application/zip",
            )
            artifact = ExportArtifact(
                job_id=export_job.id,
                blob_id=blob.id,
                filename=export_job.filename,
                mime_type="application/zip",
                size_bytes=size_bytes,
                hash_algo="sha256",
                hash=digest,
                expires_at=export_job.expires_at,
            )
            db.add(artifact)
            export_job.status = "complete"
            export_job.completed_at = now_utc()
            export_job.updated_at = export_job.completed_at
            db.commit()
    except ExportCancelled:
        return
    except Exception as exc:
        with SessionLocal() as db:
            job = db.get(ExportJob, job_id)
            if job:
                job.status = "failed"
                job.error = response_detail(exc) if isinstance(exc, HTTPException) else str(exc)
                job.updated_at = now_utc()
                db.commit()
    finally:
        zip_path.unlink(missing_ok=True)


def start_export_job(job_id: str) -> None:
    thread = threading.Thread(target=run_export_job, args=(job_id,), daemon=True)
    thread.start()


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
    result: dict[str, object] = {"item": action_item_payload(item)}
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
        "download_url": f"/documents/{doc.id}/versions/{latest_version.id}/download"
        if latest_version
        else None,
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
    state: dict[str, object] = {
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
        "asset_url": static_asset_path,
        "csp_nonce": getattr(request.state, "csp_nonce", ""),
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
    resolved_targets: list[ResolvedFavoriteTarget] = []
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
            folder = cast(Folder, target)
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
            doc = cast(Document, target)
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
    if PUBLIC_URL:
        return f"{PUBLIC_URL}/s/{quote(code, safe='')}"
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
    if link.expires_at:
        expires_at = normalize_timestamp(link.expires_at)
        if expires_at and expires_at <= now_utc():
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


def blob_is_referenced(db: Session, blob_id: int) -> bool:
    document_reference = db.execute(
        select(DocumentVersion.id).where(DocumentVersion.blob_id == blob_id).limit(1),
    ).first()
    if document_reference:
        return True
    artifact_reference = db.execute(
        select(ExportArtifact.id).where(ExportArtifact.blob_id == blob_id).limit(1),
    ).first()
    return bool(artifact_reference)


def delete_unreferenced_blob(db: Session, blob: Blob) -> list[str]:
    if blob_is_referenced(db, blob.id):
        return []
    deleted_keys: list[str] = []
    for location in list(blob.locations):
        backend = get_storage_backend(location.backend)
        backend.delete_object(location.object_key, location.bucket)
        deleted_keys.append(location.object_key)
        db.delete(location)
    db.delete(blob)
    return deleted_keys


def sweep_expired_transfers(limit: int = 250) -> dict[str, list[str]]:
    result: dict[str, list[str]] = {
        "expired_uploads": [],
        "deleted_uploads": [],
        "cancelled_exports": [],
        "deleted_exports": [],
        "deleted_export_objects": [],
    }
    with storage_write_lock():
        db = SessionLocal()
        try:
            timestamp = now_utc()
            upload_sessions = list(
                db.execute(
                    select(UploadSession)
                    .where(UploadSession.expires_at <= timestamp)
                    .order_by(UploadSession.expires_at)
                    .limit(limit),
                )
                .scalars()
                .all(),
            )
            for session in upload_sessions:
                clear_upload_session_parts(session)
                clear_upload_session_files(session.id)
                if session.status in {"active", "completing"}:
                    session.status = "expired"
                    session.updated_at = timestamp
                    result["expired_uploads"].append(session.id)
                    continue
                result["deleted_uploads"].append(session.id)
                db.delete(session)

            export_jobs = list(
                db.execute(
                    select(ExportJob)
                    .where(ExportJob.expires_at <= timestamp)
                    .order_by(ExportJob.expires_at)
                    .limit(limit),
                )
                .scalars()
                .all(),
            )
            expired_artifact_blobs: dict[int, Blob] = {}
            for job in export_jobs:
                if job.status in {"queued", "running", "finalizing"}:
                    job.status = "cancelled"
                    job.cancelled_at = timestamp
                    job.updated_at = timestamp
                    result["cancelled_exports"].append(job.id)
                    continue
                for artifact in job.artifacts:
                    expired_artifact_blobs[artifact.blob_id] = artifact.blob
                result["deleted_exports"].append(job.id)
                db.delete(job)
            db.flush()
            for blob in expired_artifact_blobs.values():
                result["deleted_export_objects"].extend(delete_unreferenced_blob(db, blob))
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
            sweep_expired_transfers()
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
    document_blob_ids = {
        row[0] for row in db.execute(select(DocumentVersion.blob_id)).all() if row[0] is not None
    }
    export_artifact_blob_ids = {
        row[0] for row in db.execute(select(ExportArtifact.blob_id)).all() if row[0] is not None
    }
    referenced_blob_ids = document_blob_ids | export_artifact_blob_ids
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
        referenced_blob = referenced_blobs_by_id.get(location.blob_id)
        if referenced_blob is None or location.object_key not in local_keys:
            continue
        try:
            data = local_backend.read_bytes(location.object_key, location.bucket)
        except StorageError:
            corrupt_local_keys.add(location.object_key)
            continue
        if not blob_bytes_match(referenced_blob, data):
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
    return debug_action_result(
        "sweep-ttl",
        result={
            "documents": sweep_expired_documents(),
            "transfers": sweep_expired_transfers(),
        },
    )


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
    return templates.TemplateResponse(request, "index.html", index_template_context(request, state))


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
    return templates.TemplateResponse(request, "index.html", index_template_context(request, state))


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

    async def event_generator() -> AsyncIterator[str]:
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
                    yield (f"id: {event.id}\nevent: state\ndata: {json.dumps(event.payload)}\n\n")
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


@router.post("/api/uploads")
def create_upload_session(
    payload: UploadSessionPayload,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    filename = normalize_item_name(payload.filename, "File name")
    if payload.size_bytes < 0:
        raise HTTPException(status_code=400, detail="Upload size must be non-negative")
    if payload.size_bytes > MAX_UPLOAD_BYTES:
        raise HTTPException(
            status_code=413, detail=f"Upload exceeds limit of {MAX_UPLOAD_BYTES} bytes"
        )
    mime_type = sanitize_mime_type(payload.mime_type, filename)
    part_count = (payload.size_bytes + TRANSFER_CHUNK_BYTES - 1) // TRANSFER_CHUNK_BYTES
    meta = client_meta(request)
    with storage_write_lock():
        if payload.mode == "create":
            folder_path_value = normalize_folder(payload.folder)
            ensure_document_upload_folder(folder_path_value)
            require_write_for_folder_path(db, folder_path_value, user)
            target_folder = get_or_create_folder_path(db, folder_path_value)
            ensure_unique_document_path(db, target_folder.id, filename)
            document_id = None
        else:
            doc = get_document_or_404(payload.document_id or 0, db)
            require_document_access(doc, user, db, 3)
            if document_is_archive(doc):
                raise HTTPException(status_code=400, detail="Restore this file before editing")
            lock = get_active_lock(doc, db)
            if not lock or lock.locked_by != user["id"]:
                raise HTTPException(
                    status_code=403,
                    detail="Check out the file before uploading a new version",
                )
            if payload.rename_to_upload and filename != doc.name:
                ensure_unique_document_path(db, doc.folder_id, filename, doc.id)
            folder_path_value = None
            document_id = doc.id
        session = UploadSession(
            id=uuid.uuid4().hex,
            mode=payload.mode,
            status="active",
            folder_path=folder_path_value,
            document_id=document_id,
            filename=filename,
            total_size=payload.size_bytes,
            chunk_size=TRANSFER_CHUNK_BYTES,
            part_count=part_count,
            mime_type=mime_type,
            note=(payload.note or "").strip() or None,
            rename_to_upload=payload.rename_to_upload,
            created_by=str(user["id"]),
            created_by_name=str(user["name"]),
            user_context=transfer_user_payload(user),
            upload_ip=meta.get("ip"),
            upload_user_agent=meta.get("user_agent"),
            expires_at=transfer_expires_at(TRANSFER_SESSION_TTL_SECONDS),
        )
        db.add(session)
        db.commit()
        return upload_session_payload(session)


@router.get("/api/uploads/{session_id}")
def get_upload_session_status(
    session_id: str,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    session = db.get(UploadSession, session_id)
    if not session:
        raise HTTPException(status_code=404, detail="Upload session not found")
    transfer_owner_required(session.created_by, user)
    if session.status == "active":
        ensure_active_upload_session(session, db)
    return upload_session_payload(session)


@router.put("/api/uploads/{session_id}/parts/{part_number}")
async def upload_session_part(
    session_id: str,
    part_number: int,
    request: Request,
    x_upload_offset: int = Header(..., alias="X-Upload-Offset"),
    x_upload_size: int = Header(..., alias="X-Upload-Size"),
    x_upload_sha256: str | None = Header(None, alias="X-Upload-Sha256"),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    session = db.get(UploadSession, session_id)
    if not session:
        raise HTTPException(status_code=404, detail="Upload session not found")
    transfer_owner_required(session.created_by, user)
    ensure_active_upload_session(session, db)
    expected_offset, expected_size = expected_part_bounds(session, part_number)
    if x_upload_offset != expected_offset or x_upload_size != expected_size:
        raise HTTPException(status_code=400, detail="Upload part range does not match session")
    existing = uploaded_parts_by_number(session).get(part_number)
    if existing:
        if x_upload_sha256 and existing.sha256 != x_upload_sha256.lower():
            raise HTTPException(
                status_code=409,
                detail="Upload part already exists with different content",
            )
        if existing.offset_bytes == expected_offset and existing.size_bytes == expected_size:
            return upload_session_payload(session)
        raise HTTPException(
            status_code=409, detail="Upload part already exists with different content"
        )
    final_path = upload_part_path(session.id, part_number)
    temp_path, actual_sha256, size_bytes = await spool_upload_part_body(
        request,
        expected_size,
        x_upload_sha256,
        final_path.parent,
    )
    try:
        with storage_write_lock():
            session = db.get(UploadSession, session_id)
            if not session:
                raise HTTPException(status_code=404, detail="Upload session not found")
            transfer_owner_required(session.created_by, user)
            ensure_active_upload_session(session, db)
            if uploaded_parts_by_number(session).get(part_number):
                raise HTTPException(status_code=409, detail="Upload part already exists")
            temp_path.replace(final_path)
            db.add(
                UploadPart(
                    session_id=session.id,
                    part_number=part_number,
                    offset_bytes=expected_offset,
                    size_bytes=size_bytes,
                    sha256=actual_sha256,
                    storage_path=str(final_path),
                ),
            )
            session.updated_at = now_utc()
            db.commit()
            return upload_session_payload(session)
    finally:
        temp_path.unlink(missing_ok=True)


@router.post("/api/uploads/{session_id}/complete")
def complete_upload_session(
    session_id: str,
    payload: CompleteUploadPayload,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    session = db.get(UploadSession, session_id)
    if not session:
        raise HTTPException(status_code=404, detail="Upload session not found")
    transfer_owner_required(session.created_by, user)
    if session.status == "complete":
        result = upload_session_result(session)
        if result is None:
            raise HTTPException(
                status_code=500, detail="Completed upload session is missing result"
            )
        return result
    with storage_write_lock():
        session = db.get(UploadSession, session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Upload session not found")
        transfer_owner_required(session.created_by, user)
        reserve_upload_completion(session, db)
    try:
        db.refresh(session)
        with storage_write_lock():
            session = db.get(UploadSession, session_id)
            if not session:
                raise HTTPException(status_code=404, detail="Upload session not found")
            if session.status != "completing":
                raise HTTPException(status_code=409, detail=f"Upload session is {session.status}")
            return complete_upload_session_document(session, payload.sha256, user, db)
    except Exception as exc:
        db.rollback()
        mark_upload_session_failed(
            session_id, response_detail(exc) if isinstance(exc, HTTPException) else str(exc)
        )
        raise


@router.delete("/api/uploads/{session_id}")
def abort_upload_session(
    session_id: str,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    with storage_write_lock():
        session = db.get(UploadSession, session_id)
        if not session:
            raise HTTPException(status_code=404, detail="Upload session not found")
        transfer_owner_required(session.created_by, user)
        if session.status != "complete":
            session.status = "aborted"
            session.aborted_at = now_utc()
            session.updated_at = session.aborted_at
            clear_upload_session_parts(session)
        db.commit()
        clear_upload_session_files(session.id)
        return upload_session_payload(session)


@router.post("/api/exports")
def create_export_job(
    payload: ActionPayload,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
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
        if not unique_docs:
            raise HTTPException(status_code=400, detail="Export has no downloadable files")
        filename = "vault-download.zip"
        current_versions = [current_version(doc, db) for doc in unique_docs]
        job = ExportJob(
            id=uuid.uuid4().hex,
            status="queued",
            created_by=str(user["id"]),
            created_by_name=str(user["name"]),
            user_context=transfer_user_payload(user),
            request_payload={"items": [action_item_payload(item) for item in items]},
            filename=filename,
            total_items=len(unique_docs),
            total_bytes=sum(
                version.blob.size_bytes if version else 0 for version in current_versions
            ),
            expires_at=transfer_expires_at(EXPORT_TTL_SECONDS),
        )
        db.add(job)
        db.commit()
        response = export_job_payload(job)
    start_export_job(job.id)
    return response


@router.get("/api/exports/{job_id}")
def get_export_job(
    job_id: str,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    job = db.get(ExportJob, job_id)
    if not job:
        raise HTTPException(status_code=404, detail="Export not found")
    transfer_owner_required(job.created_by, user)
    return export_job_payload(job)


@router.delete("/api/exports/{job_id}")
def cancel_export_job(
    job_id: str,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    with storage_write_lock():
        job = db.get(ExportJob, job_id)
        if not job:
            raise HTTPException(status_code=404, detail="Export not found")
        transfer_owner_required(job.created_by, user)
        if job.status in {"queued", "running", "finalizing"}:
            job.status = "cancelled"
            job.cancelled_at = now_utc()
            job.updated_at = job.cancelled_at
            db.commit()
        return export_job_payload(job)


@router.get("/api/exports/{job_id}/download")
def download_export_artifact(
    job_id: str,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> StreamingResponse:
    job = db.get(ExportJob, job_id)
    if not job:
        raise HTTPException(status_code=404, detail="Export not found")
    transfer_owner_required(job.created_by, user)
    expires_at = normalize_timestamp(job.expires_at)
    if expires_at and expires_at <= now_utc():
        raise HTTPException(status_code=410, detail="Export expired")
    if job.status != "complete" or not job.artifacts:
        raise HTTPException(status_code=409, detail="Export is not complete")
    artifact = job.artifacts[0]
    return blob_streaming_response(artifact.blob, artifact.filename, artifact.mime_type, request)


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
            refresh_document_location(doc, db)
            require_document_access(doc, user, db, 2)
            response = version_streaming_response(version, doc.name, version.mime_type, request)
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
            return response
        job = ExportJob(
            id=uuid.uuid4().hex,
            status="queued",
            created_by=str(user["id"]),
            created_by_name=str(user["name"]),
            user_context=transfer_user_payload(user),
            request_payload={"items": [action_item_payload(item) for item in items]},
            filename="vault-download.zip",
            expires_at=transfer_expires_at(EXPORT_TTL_SECONDS),
        )
        db.add(job)
        db.commit()
        export_response = JSONResponse(export_job_payload(job), status_code=202)
    start_export_job(job.id)
    return export_response


@router.post("/documents")
async def create_document(
    request: Request,
    file: UploadFile = File(...),
    folder: str = Form(""),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    del request, file, folder, user, db
    raise HTTPException(status_code=410, detail="Use resumable upload sessions")


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
        response = version_streaming_response(version, doc.name, version.mime_type, request)
        db.commit()
        return response


@router.get("/documents/{doc_id}/download")
def download_current_document_version(
    doc_id: int,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> StreamingResponse:
    with storage_write_lock():
        doc = get_document_or_404(doc_id, db)
        require_document_access(doc, user, db, 2)
        version = current_version(doc, db)
        if not version:
            raise HTTPException(status_code=404, detail="Document has no versions")
        refresh_document_location(doc, db)
        require_document_access(doc, user, db, 2)
        response = version_streaming_response(version, doc.name, version.mime_type, request)
        record_event(
            doc,
            user,
            "download",
            f"Downloaded {document_path(doc)}",
            db,
            meta=client_meta(request),
        )
        db.commit()
        return response


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
    del doc_id, request, file, note, rename_to_upload, user, db
    raise HTTPException(status_code=410, detail="Use resumable upload sessions")


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
        refresh_document_location(doc, db)
        require_document_access(doc, user, db, 2)
        filename = version.original_filename or doc.name
        response = version_streaming_response(version, filename, version.mime_type, request)
        record_event(
            doc,
            user,
            "download",
            f"Downloaded version v{version.version_number} of {document_path(doc)}",
            db,
            meta=client_meta(request),
        )
        db.commit()
        return response
