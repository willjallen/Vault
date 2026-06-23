"""HTTP routes for the vault service."""

import asyncio
import datetime as dt
import io
import json
import logging
import mimetypes
import re
import zipfile
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import quote

from fastapi import APIRouter, Depends, File, Form, HTTPException, Request, Response, UploadFile
from fastapi.responses import HTMLResponse, RedirectResponse, StreamingResponse
from fastapi.templating import Jinja2Templates
from pydantic import BaseModel, Field
from sqlalchemy import select
from sqlalchemy.orm import Session

from .auth import (
    UserContext,
    current_user,
    logout_response,
    oidc_callback_response,
    oidc_login_response,
    require_admin,
)
from .config import AUTH_MODE, BASE_DOMAIN, SITE_NAME, TTL_SWEEP_INTERVAL_SECONDS
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
    StateEvent,
    VaultGroup,
    VaultGroupMembership,
    VaultUser,
)
from .storage import (
    StorageConfigurationError,
    StorageError,
    StorageNotFoundError,
    get_storage_backend,
    new_version_id,
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
TTL_ACTIONS = {"archive", "delete"}
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
    default_folder_permissions(db, root)
    return root


def ensure_root_folders(db: Session) -> dict[str, Folder]:
    return {
        VAULT_ROOT_KEY: get_root_folder(db, VAULT_ROOT_KEY),
        ARCHIVE_ROOT_KEY: get_root_folder(db, ARCHIVE_ROOT_KEY),
    }


def user_group_names(user: UserContext) -> set[str]:
    return {group.strip().lower() for group in user.get("groups", []) if group.strip()}


def access_level(can_view: bool, can_read: bool, can_write: bool) -> int:
    if can_write:
        return 3
    if can_read:
        return 2
    if can_view:
        return 1
    return 0


def default_folder_permissions(db: Session, folder: Folder) -> None:
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


def document_access_level(doc: Document, user: UserContext, db: Session) -> int:
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
    current = get_root_folder(db, ref.root_key)
    if not ref.relative_path:
        return current
    for part in ref.relative_path.split("/"):
        child = find_child_folder(db, current.id, part)
        if not child:
            return None
        current = child
    return current


def get_or_create_folder_path(db: Session, path: str | None) -> Folder:
    ref = parse_public_folder_path(path)
    current = get_root_folder(db, ref.root_key)
    if not ref.relative_path:
        return current
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
            default_folder_permissions(db, folder)
        current = folder
    return current


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


def apply_folder_ttl(doc: Document, folder: Folder, timestamp: dt.datetime | None = None) -> None:
    days = folder.default_ttl_days
    action = (folder.default_ttl_action or "").strip().lower()
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


def remove_empty_folder_conflict(
    db: Session,
    parent_id: int,
    name: str,
    exclude_folder_id: int | None = None,
) -> None:
    existing = find_child_folder(db, parent_id, name)
    if existing and existing.id != exclude_folder_id and not folder_has_items(db, existing):
        db.delete(existing)
        db.flush()


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


def set_subtree_root_key(db: Session, root: Folder, root_key: str) -> None:
    ids = subtree_folder_ids(root, all_folders(db))
    for folder in db.execute(select(Folder).where(Folder.id.in_(ids))).scalars():
        folder.root_key = root_key


def folder_has_items(db: Session, folder: Folder) -> bool:
    child = db.execute(select(Folder.id).where(Folder.parent_id == folder.id).limit(1)).first()
    if child is not None:
        return True
    doc = db.execute(select(Document.id).where(Document.folder_id == folder.id).limit(1)).first()
    return doc is not None


def prune_empty_archive_folders(db: Session, folder: Folder | None) -> None:
    current = folder
    while current and folder_is_archive(current) and not current.is_root:
        db.flush()
        if folder_has_items(db, current):
            return
        parent = current.parent
        db.delete(current)
        db.flush()
        current = parent


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
    doc = db.execute(select(Document).where(Document.id == doc_id)).scalars().first()
    if not doc:
        raise HTTPException(status_code=404, detail="Document not found")
    return doc


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
    return (
        db.execute(
            select(DocumentVersion)
            .where(DocumentVersion.document_id == doc.id)
            .order_by(DocumentVersion.version_number.desc())
            .limit(1),
        )
        .scalars()
        .first()
    )


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


def read_version_bytes(version: DocumentVersion) -> bytes:
    location = location_for_blob(version.blob)
    try:
        return get_storage_backend(location.backend).read_bytes(
            location.object_key,
            location.bucket,
        )
    except StorageNotFoundError as exc:
        raise HTTPException(status_code=404, detail="Blob missing from storage") from exc
    except StorageConfigurationError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc


def download_response(data: bytes, filename: str, mime_type: str | None = None) -> Response:
    safe_name = "".join(
        "_" if ord(char) < 32 or ord(char) == 127 else char
        for char in filename.replace('"', "")
    ).strip() or "download"
    ascii_name = "".join(char if 32 <= ord(char) < 127 else "_" for char in safe_name).strip()
    ascii_name = ascii_name or "download"
    content_type = sanitize_mime_type(mime_type, safe_name)
    disposition = f'attachment; filename="{ascii_name}"; filename*=UTF-8\'\'{quote(safe_name)}'
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


def action_item_payload(item: ActionItem) -> dict[str, object]:
    payload: dict[str, object] = {"type": item.type}
    if item.id is not None:
        payload["id"] = item.id
    if item.path is not None:
        payload["path"] = normalize_folder(item.path)
    return payload


def item_label(item: ActionItem) -> str:
    if item.type == "document":
        return f"document:{item.id}"
    return f"folder:{normalize_folder(item.path)}"


def action_result(item: ActionItem, detail: str | None = None) -> dict[str, object]:
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


def normalize_action_items(items: list[ActionItem], db: Session) -> list[ActionItem]:
    folders: list[tuple[ActionItem, Folder]] = []
    documents: list[tuple[ActionItem, Document]] = []
    seen: set[str] = set()
    normalized: list[ActionItem] = []
    for item in items:
        item_type = item.type.strip().lower()
        if item_type == "document":
            if item.id is None:
                raise HTTPException(status_code=400, detail="Document id is required")
            doc = get_document_or_404(item.id, db)
            normalized_item = ActionItem(type="document", id=doc.id)
            key = item_label(normalized_item)
            if key not in seen:
                seen.add(key)
                normalized.append(normalized_item)
                documents.append((normalized_item, doc))
            continue
        if item_type == "folder":
            path = normalize_folder(item.path)
            if not path:
                raise HTTPException(status_code=400, detail="Folder path is required")
            folder = get_folder_by_path(db, path)
            if not folder:
                raise HTTPException(status_code=404, detail=f"Folder not found: {path}")
            normalized_item = ActionItem(type="folder", path=folder_path(folder))
            key = item_label(normalized_item)
            if key not in seen:
                seen.add(key)
                normalized.append(normalized_item)
                folders.append((normalized_item, folder))
            continue
        raise HTTPException(status_code=400, detail="Invalid item type")

    folder_paths = [folder_path(folder) for _, folder in folders]
    pruned: list[ActionItem] = []
    for item in normalized:
        if item.type == "folder":
            path = normalize_folder(item.path)
            if any(path != parent and path.startswith(f"{parent}/") for parent in folder_paths):
                continue
        if item.type == "document":
            doc = get_document_or_404(item.id or 0, db)
            doc_path = document_folder_path(doc)
            if any(doc_path == parent or doc_path.startswith(f"{parent}/") for parent in folder_paths):
                continue
        pruned.append(item)
    return pruned


def batch_state_changed(db: Session, event_type: str) -> None:
    record_state_change(
        db,
        f"batch.{event_type}",
        ("contents", "sidebar", "document_detail", "my_edits"),
    )


def archive_doc_item(doc: Document, request: Request, user: UserContext, db: Session) -> str:
    refresh_document_location(doc, db)
    require_document_access(doc, user, db, 3)
    if document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Document is already archived")
    source_path = document_path(doc)
    lock = ensure_not_locked_by_other(doc, user, db)
    target_folder_path = public_folder_path(ARCHIVE_ROOT_KEY, folder_relative_path(doc.folder))
    require_write_for_folder_path(db, target_folder_path, user)
    target_folder = get_or_create_folder_path(db, target_folder_path)
    release_lock(lock, user)
    mutate_doc_location(
        doc,
        target_folder,
        doc.name,
        user,
        db,
        client_meta(request),
        "archive",
        f"Archived from {source_path}",
        publish_state=False,
    )
    return document_path(doc)


def restore_doc_item(doc: Document, request: Request, user: UserContext, db: Session) -> str:
    refresh_document_location(doc, db)
    require_document_access(doc, user, db, 3)
    if not document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Document is not archived")
    source_path = document_path(doc)
    ensure_not_locked_by_other(doc, user, db)
    archive_folder = doc.folder
    target_folder_path = folder_relative_path(doc.folder)
    require_write_for_folder_path(db, target_folder_path, user)
    target_folder = get_or_create_folder_path(db, target_folder_path)
    mutate_doc_location(
        doc,
        target_folder,
        doc.name,
        user,
        db,
        client_meta(request),
        "unarchive",
        f"Restored to Vault from {source_path}",
        publish_state=False,
    )
    prune_empty_archive_folders(db, archive_folder)
    return document_path(doc)


def archive_folder_item(source: Folder, request: Request, user: UserContext, db: Session) -> str:
    if source.is_root:
        raise HTTPException(status_code=400, detail="Cannot archive a root folder")
    if folder_is_archive(source):
        raise HTTPException(status_code=400, detail="Folder is already archived")
    require_folder_access(source, user, db, 3)
    source_path = folder_path(source)
    target_path = public_folder_path(ARCHIVE_ROOT_KEY, folder_relative_path(source))
    require_write_for_folder_path(db, "/".join(target_path.split("/")[:-1]), user)
    target_parent = get_or_create_folder_path(db, "/".join(target_path.split("/")[:-1]))
    target_name = normalize_item_name(target_path.split("/")[-1], "Folder name")
    remove_empty_folder_conflict(db, target_parent.id, target_name, source.id)
    ensure_unique_folder_name(db, target_parent.id, target_name, source.id)
    meta = client_meta(request)
    docs = docs_in_folder_subtree(db, source)
    for doc in docs:
        release_lock(get_active_lock(doc, db), user)
        record_event(
            doc,
            user,
            "archive",
            f"Archived from {document_path(doc)}",
            db,
            meta=meta,
            publish_state=False,
        )
        doc.latest_modified_at = now_utc()
    source.parent = target_parent
    source.parent_id = target_parent.id
    source.name = target_name
    set_subtree_root_key(db, source, ARCHIVE_ROOT_KEY)
    for doc in docs:
        apply_folder_ttl(doc, doc.folder, doc.latest_modified_at)
    record_folder_event(source, user, "archive", f"Moved to Archive from {source_path}", db)
    return target_path


def restore_folder_item(source: Folder, request: Request, user: UserContext, db: Session) -> str:
    if source.is_root or not folder_is_archive(source):
        raise HTTPException(status_code=400, detail="Choose an archived folder to restore")
    require_folder_access(source, user, db, 3)
    source_path = folder_path(source)
    target_path = folder_relative_path(source)
    require_write_for_folder_path(db, "/".join(target_path.split("/")[:-1]), user)
    target_parent = get_or_create_folder_path(db, "/".join(target_path.split("/")[:-1]))
    target_name = normalize_item_name(target_path.split("/")[-1], "Folder name")
    remove_empty_folder_conflict(db, target_parent.id, target_name, source.id)
    ensure_unique_folder_name(db, target_parent.id, target_name, source.id)
    archive_parent = source.parent
    meta = client_meta(request)
    docs = docs_in_folder_subtree(db, source)
    for doc in docs:
        record_event(
            doc,
            user,
            "unarchive",
            f"Restored to Vault from {document_path(doc)}",
            db,
            meta=meta,
            publish_state=False,
        )
        doc.latest_modified_at = now_utc()
    source.parent = target_parent
    source.parent_id = target_parent.id
    source.name = target_name
    set_subtree_root_key(db, source, VAULT_ROOT_KEY)
    for doc in docs:
        apply_folder_ttl(doc, doc.folder, doc.latest_modified_at)
    prune_empty_archive_folders(db, archive_parent)
    record_folder_event(source, user, "restore", f"Restored to Vault from {source_path}", db)
    return target_path


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
    require_write_for_folder_path(db, destination_folder, user)
    target_parent = get_or_create_folder_path(db, destination_folder)
    source_path = folder_path(source)
    source_parent_path = folder_path(source.parent) if source.parent else ""
    source_name = source.name
    target_name = normalize_item_name(name or source.name, "Folder name")
    target_path = join_path(folder_path(target_parent), target_name)
    if target_path == source_path:
        return source_path
    if target_path.startswith(f"{source_path}/"):
        raise HTTPException(status_code=400, detail="Cannot move a folder into itself")
    ensure_unique_folder_name(db, target_parent.id, target_name, source.id)
    source.parent = target_parent
    source.parent_id = target_parent.id
    source.name = target_name
    event_type = "rename" if source_parent_path == folder_path(target_parent) and target_name != source_name else "move"
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
        for lock in db.execute(select(DocumentLock).where(DocumentLock.is_active == True)).scalars()  # noqa: E712
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
    lock = (locks or {}).get(doc.id)
    payload: dict[str, object] = {
        "id": doc.id,
        "name": doc.name,
        "path": doc_path,
        "folder": doc_folder,
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
        "archived": document_is_archive(doc),
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
        "path": path,
        "name": path.split("/")[-1] if path else "Vault",
        "color": folder.color or "",
        "icon": folder.icon or "",
        "default_ttl_days": folder.default_ttl_days,
        "default_ttl_action": folder.default_ttl_action or "none",
        "latest_by": latest_by,
        "modified_at": latest.isoformat() if latest else None,
        "modified_display": format_mtime(latest),
        "size_bytes": size,
        "size_display": format_size(size),
    }


def folder_counts_payload(folder: Folder, db: Session) -> dict[str, int]:
    folder_ids = subtree_folder_ids(folder, all_folders(db))
    document_count = (
        db.execute(select(Document.id).where(Document.folder_id.in_(folder_ids))).all()
        if folder_ids
        else []
    )
    return {
        "folders": max(len(folder_ids) - 1, 0),
        "documents": len(document_count),
    }


def folder_permissions_payload(folder: Folder, db: Session) -> list[dict[str, object]]:
    rows = (
        db.execute(
            select(FolderPermission, VaultGroup)
            .join(VaultGroup, VaultGroup.id == FolderPermission.group_id)
            .where(FolderPermission.folder_id == folder.id)
            .order_by(VaultGroup.name),
        )
        .all()
    )
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


def folder_properties_payload(folder: Folder, db: Session) -> dict[str, object]:
    folders = all_folders(db)
    path_cache = build_folder_path_cache(folders)
    path = folder_path(folder, path_cache)
    docs = list(db.execute(select(Document)).scalars().all())
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
    groups = list(db.execute(select(VaultGroup).order_by(VaultGroup.name)).scalars().all())
    summary.update(
        {
            "id": folder.id,
            "root": bool(folder.is_root),
            "archived": folder_is_archive(folder),
            "created_at": folder.created_at.isoformat() if folder.created_at else None,
            "created_by": folder.created_by,
            "created_by_name": folder.created_by_name or folder.created_by or "System",
            **ttl_policy_payload(folder),
            "counts": folder_counts_payload(folder, db),
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
            "permissions": folder_permissions_payload(folder, db),
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

    if search_query and recursive:
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
        folder_rows.append(folder_summary_payload(item, path, stats))
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
        if search_query and not matches_query(search_query, doc.name, doc_path, doc_folder):
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
    archive_root = get_root_folder(db, ARCHIVE_ROOT_KEY)
    folders = all_folders(db)
    path_cache = build_folder_path_cache(folders)
    children = {
        "": sorted(
            folder_path(child, path_cache)
            for child in vault_root.children
            if folder_access_level(child, user, db) >= 1
        ),
        ARCHIVE_ROOT: sorted(
            folder_path(child, path_cache)
            for child in archive_root.children
            if folder_access_level(child, user, db) >= 1
        ),
    }
    metadata = {
        folder_path(item, path_cache): {
            "color": item.color or "",
            "icon": item.icon or "",
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
                "is_admin": bool(user.is_admin),
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
    }


def ensure_not_last_active_admin(db: Session, target: VaultUser) -> None:
    if not target.is_admin or not target.is_active:
        return
    active_admins = list(
        db.execute(
            select(VaultUser).where(
                VaultUser.is_admin == True,  # noqa: E712
                VaultUser.is_active == True,  # noqa: E712
            ),
        )
        .scalars()
        .all(),
    )
    if len(active_admins) == 1 and active_admins[0].id == target.id:
        raise HTTPException(status_code=400, detail="At least one active admin is required")


def find_group_by_normalized_name(db: Session, name: str) -> VaultGroup | None:
    lowered = name.lower()
    for group in db.execute(select(VaultGroup)).scalars().all():
        if group.name.lower() == lowered:
            return group
    return None


def commit_admin_change(db: Session, event_type: str) -> dict[str, object]:
    record_state_change(db, f"admin.{event_type}", ("admin",))
    commit_state(db)
    return build_admin_directory_payload(db)


def build_bootstrap_payload(user: UserContext, folder: str, db: Session) -> dict[str, object]:
    ensure_root_folders(db)
    current = get_folder_by_path(db, folder) or get_root_folder(db, VAULT_ROOT_KEY)
    require_folder_access(current, user, db, 1)
    return {
        "auth_mode": AUTH_MODE,
        "base_domain": BASE_DOMAIN,
        "site_name": SITE_NAME,
        "user": user,
        "version": APP_VERSION,
        "current_folder": folder_path(current),
    }


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
        ("contents", "sidebar", "document_detail", "my_edits"),
    )


def latest_state_event_id() -> int:
    db = SessionLocal()
    try:
        return db.execute(select(StateEvent.id).order_by(StateEvent.id.desc()).limit(1)).scalar() or 0
    finally:
        db.close()


def state_events_after(last_id: int) -> list[StateEvent]:
    db = SessionLocal()
    try:
        return list(
            db.execute(
                select(StateEvent).where(StateEvent.id > last_id).order_by(StateEvent.id).limit(100),
            )
            .scalars()
            .all(),
        )
    finally:
        db.close()


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
) -> None:
    ensure_unique_document_path(db, target_folder.id, target_name, doc.id)
    folder_changed = doc.folder_id != target_folder.id
    doc.folder = target_folder
    doc.folder_id = target_folder.id
    doc.name = target_name
    doc.latest_modified_at = now_utc()
    if folder_changed:
        apply_folder_ttl(doc, target_folder, doc.latest_modified_at)
    record_event(doc, user, event_type, message, db, meta=meta, publish_state=publish_state)


def unique_document_name(
    db: Session,
    folder_id: int,
    desired_name: str,
    exclude_doc_id: int | None = None,
) -> str:
    if not document_in_folder(db, folder_id, desired_name, exclude_doc_id):
        return desired_name
    stem, dot, suffix = desired_name.rpartition(".")
    base = stem if dot else desired_name
    extension = f".{suffix}" if dot else ""
    for index in range(1, 1000):
        candidate = f"{base} (expired {index}){extension}"
        if not document_in_folder(db, folder_id, candidate, exclude_doc_id):
            return candidate
    raise HTTPException(status_code=400, detail="Could not choose an archive name")


def archive_expired_document(doc: Document, db: Session, timestamp: dt.datetime) -> str:
    source_path = document_path(doc)
    target_folder_path = public_folder_path(ARCHIVE_ROOT_KEY, folder_relative_path(doc.folder))
    target_folder = get_or_create_folder_path(db, target_folder_path)
    target_name = unique_document_name(db, target_folder.id, doc.name, doc.id)
    mutate_doc_location(
        doc,
        target_folder,
        target_name,
        SYSTEM_USER,
        db,
        SYSTEM_META,
        "archive",
        f"Expired at {timestamp.strftime('%Y-%m-%d %H:%M UTC')}; archived from {source_path}",
        publish_state=False,
    )
    return document_path(doc)


def delete_expired_document(doc: Document, db: Session) -> str:
    deleted_path = document_path(doc)
    archive_folder = doc.folder if document_is_archive(doc) else None
    db.delete(doc)
    if archive_folder is not None:
        prune_empty_archive_folders(db, archive_folder)
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
    orphan_blobs = [
        blob for blob in db.execute(select(Blob)).scalars() if blob.id not in referenced_blob_ids
    ]
    orphan_blob_ids = [blob.id for blob in orphan_blobs]
    local_locations = list(
        db.execute(select(BlobLocation).where(BlobLocation.backend == "local")).scalars(),
    )
    known_local_keys = {location.object_key for location in local_locations}
    try:
        local_backend = get_storage_backend("local")
        local_keys = set(local_backend.list_object_keys())
    except StorageError:
        local_keys = set()
    unreferenced_local_keys = sorted(local_keys - known_local_keys)
    orphan_local_keys = sorted(
        {
            location.object_key
            for blob in orphan_blobs
            for location in blob.locations
            if location.backend == "local"
        },
    )
    if apply:
        local_backend = get_storage_backend("local")
        for object_key in orphan_local_keys:
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
        db.flush()
    return {
        "orphan_blob_ids": orphan_blob_ids,
        "unreferenced_local_keys": unreferenced_local_keys,
        "deleted_local_keys": sorted(
            set(unreferenced_local_keys) | set(orphan_local_keys),
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
    db.delete(membership)
    return commit_admin_change(db, "group.member.removed")


@router.get("/", response_class=HTMLResponse)
def index(
    request: Request,
    folder: str = "",
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> HTMLResponse:
    ensure_root_folders(db)
    commit_state(db)
    normalized = normalize_folder(folder)
    state = {
        "bootstrap": build_bootstrap_payload(user, normalized, db),
        "contents": build_contents_payload(db, normalized, user),
        "sidebar": build_sidebar_payload(db, user),
        "my_edits": build_my_edits_payload(user, db),
    }
    return templates.TemplateResponse("index.html", {"request": request, "state": state})


@router.get("/api/bootstrap")
def api_bootstrap(
    folder: str = "",
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    commit_state(db)
    return build_bootstrap_payload(user, normalize_folder(folder), db)


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
    return folder_properties_payload(folder, db)


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
        return folder_properties_payload(folder, db)


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
        folder.default_ttl_days = days
        folder.default_ttl_action = action
        record_folder_event(folder, user, "retention", "Updated folder retention policy", db)
        record_folder_change(db, "retention")
        commit_state(db)
        return folder_properties_payload(folder, db)


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
        return folder_properties_payload(folder, db)


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
) -> dict[str, str]:
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
        default_folder_permissions(db, created)
        record_folder_event(created, user, "create", f"Created {normalized}", db)
        record_folder_change(db, "created")
        db.commit()
    return {"folder": normalized}


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
        for item in items:
            try:
                if item.type == "document":
                    doc = get_document_or_404(item.id or 0, db)
                    moved = move_doc_item(doc, destination, request, user, db)
                    result["ok"].append(action_result(item, moved))
                else:
                    folder_item = get_folder_by_path(db, item.path)
                    if not folder_item:
                        raise HTTPException(status_code=404, detail="Folder not found")
                    moved = move_folder_item(folder_item, destination, user, db)
                    result["ok"].append(action_result(item, moved))
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
            if item.type == "document":
                doc = get_document_or_404(item.id or 0, db)
                destination_folder = (
                    normalize_folder(payload.destination_folder)
                    if payload.destination_folder is not None
                    else document_folder_path(doc)
                )
                renamed = move_doc_item(
                    doc,
                    destination_folder,
                    request,
                    user,
                    db,
                    name=payload.name,
                )
                result["ok"].append(action_result(item, renamed))
            else:
                folder_item = get_folder_by_path(db, item.path)
                if not folder_item:
                    raise HTTPException(status_code=404, detail="Folder not found")
                destination_folder = (
                    normalize_folder(payload.destination_folder)
                    if payload.destination_folder is not None
                    else (folder_path(folder_item.parent) if folder_item.parent else "")
                )
                renamed = move_folder_item(
                    folder_item,
                    destination_folder,
                    user,
                    db,
                    name=payload.name,
                )
                result["ok"].append(action_result(item, renamed))
            batch_state_changed(db, "rename")
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
        for item in items:
            try:
                if item.type == "document":
                    doc = get_document_or_404(item.id or 0, db)
                    archived = archive_doc_item(doc, request, user, db)
                else:
                    folder_item = get_folder_by_path(db, item.path)
                    if not folder_item:
                        raise HTTPException(status_code=404, detail="Folder not found")
                    archived = archive_folder_item(folder_item, request, user, db)
                result["ok"].append(action_result(item, archived))
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
        for item in items:
            try:
                if item.type == "document":
                    doc = get_document_or_404(item.id or 0, db)
                    restored = restore_doc_item(doc, request, user, db)
                else:
                    folder_item = get_folder_by_path(db, item.path)
                    if not folder_item:
                        raise HTTPException(status_code=404, detail="Folder not found")
                    restored = restore_folder_item(folder_item, request, user, db)
                result["ok"].append(action_result(item, restored))
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
    if not user["is_admin"]:
        raise HTTPException(status_code=403, detail="Admin access required")
    items = normalize_action_items(require_action_items(payload), db)
    result = bulk_result()
    changed = False
    with storage_write_lock():
        for item in items:
            try:
                if item.type == "document":
                    doc = get_document_or_404(item.id or 0, db)
                    refresh_document_location(doc, db)
                    if not document_is_archive(doc):
                        raise HTTPException(
                            status_code=400,
                            detail="Move the document to Archive before deleting",
                        )
                    deleted = document_path(doc)
                    archive_folder = doc.folder
                    db.delete(doc)
                    prune_empty_archive_folders(db, archive_folder)
                else:
                    folder_item = get_folder_by_path(db, item.path)
                    if not folder_item or folder_item.is_root or not folder_is_archive(folder_item):
                        raise HTTPException(
                            status_code=400,
                            detail="Delete forever is only available in Archive",
                        )
                    deleted = folder_path(folder_item)
                    archive_parent = folder_item.parent
                    db.delete(folder_item)
                    prune_empty_archive_folders(db, archive_parent)
                result["ok"].append(action_result(item, deleted))
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
                result["ok"].append(action_result(item, lock.locked_by_name or lock.locked_by))
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
                if item.type != "document":
                    raise HTTPException(status_code=400, detail="Only files can be unlocked")
                doc = get_document_or_404(item.id or 0, db)
                require_document_access(doc, user, db, 3)
                lock = get_active_lock(doc, db)
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
                result["ok"].append(action_result(item, "Unlocked"))
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
    items = normalize_action_items(require_action_items(payload), db)
    docs_to_download: list[Document] = []
    for item in items:
        if item.type == "document":
            doc = get_document_or_404(item.id or 0, db)
            require_document_access(doc, user, db, 2)
            docs_to_download.append(doc)
        else:
            folder_item = get_folder_by_path(db, item.path)
            if not folder_item:
                raise HTTPException(status_code=404, detail="Folder not found")
            require_folder_access(folder_item, user, db, 2)
            docs_to_download.extend(docs_in_folder_subtree(db, folder_item))
    unique_docs = list({doc.id: doc for doc in docs_to_download}.values())
    for doc in unique_docs:
        require_document_access(doc, user, db, 2)
    if len(unique_docs) == 1 and len(items) == 1 and items[0].type == "document":
        doc = unique_docs[0]
        version = current_version(doc, db)
        if not version:
            raise HTTPException(status_code=404, detail="Document has no versions")
        data = read_version_bytes(version)
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
                archive.writestr(archive_name, read_version_bytes(version))
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
    folder_value = document_folder_path(doc)
    return RedirectResponse(url=f"/?folder={quote(folder_value)}", status_code=303)


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
    doc = get_document_or_404(doc_id, db)
    require_document_access(doc, user, db, 2)
    version = get_version_or_404(doc, version_id, db)
    data = read_version_bytes(version)
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
