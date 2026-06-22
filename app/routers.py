# Copyright (c) 2024 The Allen Family
"""HTTP routes for the vault service."""

import datetime as dt
import mimetypes
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import quote

from fastapi import APIRouter, Depends, File, Form, HTTPException, Request, Response, UploadFile
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse
from fastapi.templating import Jinja2Templates
from sqlalchemy import select
from sqlalchemy.orm import Session

from .auth import UserContext, current_user
from .config import BASE_DOMAIN
from .db import get_db
from .models import (
    Blob,
    BlobLocation,
    Document,
    DocumentEvent,
    DocumentLock,
    DocumentVersion,
    Folder,
)
from .storage import (
    StorageConfigurationError,
    StorageError,
    StorageNotFoundError,
    get_storage_backend,
    new_version_id,
    storage_write_lock,
)

templates = Jinja2Templates(directory=str(Path(__file__).parent / "templates"))

router = APIRouter()
ARCHIVE_ROOT = "Archive"
VAULT_ROOT_KEY = "vault"
ARCHIVE_ROOT_KEY = "archive"
ROOT_NAMES = {VAULT_ROOT_KEY: "Vault", ARCHIVE_ROOT_KEY: "Archive"}


@dataclass(frozen=True)
class DocStat:
    folder: str
    size_bytes: int
    mtime: dt.datetime | None


@dataclass(frozen=True)
class PublicFolderPath:
    root_key: str
    relative_path: str


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
    if any(part in {".", ".."} for part in parts):
        raise HTTPException(status_code=400, detail="Invalid folder path")
    return "/".join(parts)


def normalize_item_name(name: str | None, label: str = "Name") -> str:
    cleaned = (name or "").replace("\\", "/").split("/")[-1].strip()
    if not cleaned:
        raise HTTPException(status_code=400, detail=f"{label} is required")
    if cleaned in {".", ".."} or "/" in cleaned or "\\" in cleaned:
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
    return normalized.strftime("%b %d, %Y")


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
    return root


def ensure_root_folders(db: Session) -> dict[str, Folder]:
    return {
        VAULT_ROOT_KEY: get_root_folder(db, VAULT_ROOT_KEY),
        ARCHIVE_ROOT_KEY: get_root_folder(db, ARCHIVE_ROOT_KEY),
    }


def build_folder_path_cache(folders: list[Folder]) -> dict[int, str]:
    by_id = {folder.id: folder for folder in folders}
    cache: dict[int, str] = {}

    def compute(folder_id: int) -> str:
        if folder_id in cache:
            return cache[folder_id]
        folder = by_id[folder_id]
        if folder.is_root or folder.parent_id is None:
            cache[folder_id] = public_folder_path(folder.root_key, "")
            return cache[folder_id]
        parent = by_id.get(folder.parent_id)
        if not parent:
            cache[folder_id] = public_folder_path(folder.root_key, folder.name)
            return cache[folder_id]
        parent_path = compute(parent.id)
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
    while current and not current.is_root:
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
        current = folder
    return current


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
    safe_name = filename.replace('"', "").strip() or "download"
    content_type = mime_type or mimetypes.guess_type(safe_name)[0] or "application/octet-stream"
    disposition = f'attachment; filename="{safe_name}"; filename*=UTF-8\'\'{quote(safe_name)}'
    return Response(
        content=data,
        media_type=content_type,
        headers={"Content-Disposition": disposition},
    )


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


def build_state(user: UserContext, current_folder: str, db: Session) -> dict[str, object]:
    ensure_root_folders(db)
    folders = all_folders(db)
    path_cache = build_folder_path_cache(folders)
    docs = db.execute(select(Document)).scalars().all()
    locks = {
        lock.document_id: lock
        for lock in db.execute(select(DocumentLock).where(DocumentLock.is_active == True)).scalars()  # noqa: E712
    }
    versions = (
        db.execute(
            select(DocumentVersion).order_by(
                DocumentVersion.document_id,
                DocumentVersion.committed_at.desc(),
            ),
        )
        .scalars()
        .all()
    )
    events = (
        db.execute(
            select(DocumentEvent).order_by(
                DocumentEvent.document_id,
                DocumentEvent.created_at.desc(),
            ),
        )
        .scalars()
        .all()
    )

    versions_by_document: dict[int, list[DocumentVersion]] = defaultdict(list)
    events_by_document: dict[int, list[DocumentEvent]] = defaultdict(list)
    for version in versions:
        versions_by_document[version.document_id].append(version)
    for event in events:
        events_by_document[event.document_id].append(event)

    doc_payloads: list[dict[str, object]] = []
    doc_stats: list[DocStat] = []
    for doc in docs:
        doc_folder = document_folder_path(doc, path_cache)
        doc_path = document_path(doc, path_cache)
        doc_versions = versions_by_document.get(doc.id, [])
        filtered_versions = dedupe_versions_by_checksum(doc_versions)
        latest_version = current_version(doc, db)
        latest_size_bytes = latest_version.blob.size_bytes if latest_version else None
        latest_updated_at = normalize_timestamp(doc.latest_modified_at)
        lock = locks.get(doc.id)
        history_items: list[dict[str, object]] = []
        version_signatures = {version_signature(version) for version in filtered_versions}
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
        for event in events_by_document.get(doc.id, []):
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
        doc_payloads.append(
            {
                "id": doc.id,
                "name": doc.name,
                "path": doc_path,
                "folder": doc_folder,
                "latest_updated_at": latest_updated_at.isoformat() if latest_updated_at else None,
                "latest_updated_display": format_mtime(latest_updated_at),
                "latest_by": (latest_version.committed_by_name or latest_version.committed_by)
                if latest_version
                else None,
                "latest_message": latest_version.message if latest_version else None,
                "latest_version_number": latest_version.version_number
                if latest_version
                else doc.latest_version_number,
                "version_count": doc.version_count or len(filtered_versions),
                "created_by": doc.created_by,
                "created_by_name": doc.created_by_name,
                "created_at": doc.created_at.isoformat() if doc.created_at else None,
                "size_bytes": latest_size_bytes,
                "size_display": format_size(latest_size_bytes),
                "lock": {
                    "by": lock.locked_by if lock else None,
                    "name": lock.locked_by_name if lock else None,
                    "at": lock.locked_at.isoformat() if lock and lock.locked_at else None,
                    "ip": lock.locked_ip if lock else None,
                    "user_agent": lock.locked_user_agent if lock else None,
                    "force_acquired": lock.force_acquired if lock else None,
                },
                "archived": document_is_archive(doc),
                "versions": history_items,
            },
        )
        doc_stats.append(DocStat(doc_folder, latest_size_bytes or 0, latest_updated_at))

    folder_children: dict[str, list[str]] = defaultdict(list)
    folder_payloads: dict[str, dict[str, object]] = {}
    folder_children.setdefault("", [])
    folder_children.setdefault(ARCHIVE_ROOT, [])
    for folder in folders:
        path = path_cache.get(folder.id, "")
        folder_children.setdefault(path, [])
        if folder.is_root or folder.parent_id is None:
            continue
        parent_path = path_cache.get(folder.parent_id, "")
        folder_children[parent_path].append(path)

    all_folder_paths = set(folder_children.keys())
    for child_paths in folder_children.values():
        all_folder_paths.update(child_paths)
    for path in all_folder_paths:
        latest: dt.datetime | None = None
        size = 0
        for stat in doc_stats:
            if not folder_contains_doc_folder(path, stat.folder):
                continue
            size += stat.size_bytes
            if stat.mtime and (latest is None or stat.mtime > latest):
                latest = stat.mtime
        folder_payloads[path] = {
            "name": path.split("/")[-1] if path else "Vault",
            "latest_updated_at": latest.isoformat() if latest else None,
            "latest_updated_display": format_mtime(latest),
            "size_bytes": size,
            "size_display": format_size(size),
        }
    folder_payloads[ARCHIVE_ROOT]["name"] = ARCHIVE_ROOT

    return {
        "base_domain": BASE_DOMAIN,
        "user": user,
        "current_folder": normalize_folder(current_folder),
        "doc_payloads": doc_payloads,
        "folder_children": {key: sorted(value) for key, value in folder_children.items()},
        "folder_payloads": folder_payloads,
    }


def commit_state(db: Session) -> None:
    db.commit()


def folder_for_new_path(db: Session, path: str) -> tuple[Folder, str]:
    folder_path_value, name = split_document_path(path)
    folder = get_or_create_folder_path(db, folder_path_value)
    return folder, name


def mutate_doc_location(
    doc: Document,
    target_folder: Folder,
    target_name: str,
    user: UserContext,
    db: Session,
    meta: dict[str, str | None],
    event_type: str,
    message: str,
) -> None:
    ensure_unique_document_path(db, target_folder.id, target_name, doc.id)
    doc.folder = target_folder
    doc.folder_id = target_folder.id
    doc.name = target_name
    doc.latest_modified_at = now_utc()
    record_event(doc, user, event_type, message, db, meta=meta)


def storage_reconciliation_report(db: Session, apply: bool = False) -> dict[str, object]:
    referenced_blob_ids = {
        row[0] for row in db.execute(select(DocumentVersion.blob_id)).all() if row[0] is not None
    }
    orphan_blob_ids = [
        blob.id for blob in db.execute(select(Blob)).scalars() if blob.id not in referenced_blob_ids
    ]
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
    if apply:
        local_backend = get_storage_backend("local")
        for object_key in unreferenced_local_keys:
            local_backend.delete_object(object_key)
    return {
        "orphan_blob_ids": orphan_blob_ids,
        "unreferenced_local_keys": unreferenced_local_keys,
        "deleted_local_keys": unreferenced_local_keys if apply else [],
    }


@router.get("/", response_class=HTMLResponse)
def index(
    request: Request,
    folder: str = "",
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> HTMLResponse:
    ensure_root_folders(db)
    commit_state(db)
    state = build_state(user, normalize_folder(folder), db)
    return templates.TemplateResponse("index.html", {"request": request, "state": state})


@router.get("/api/state")
def api_state(
    folder: str = "",
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    ensure_root_folders(db)
    commit_state(db)
    return build_state(user, normalize_folder(folder), db)


@router.post("/folders")
def create_folder(
    folder: str = Form(...),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    del user
    normalized = normalize_folder(folder)
    if not normalized:
        raise HTTPException(status_code=400, detail="Folder path is required")
    if get_folder_by_path(db, normalized):
        raise HTTPException(status_code=400, detail="Folder already exists")
    parent_path = "/".join(normalized.split("/")[:-1])
    name = normalize_item_name(normalized.split("/")[-1], "Folder name")
    parent = get_or_create_folder_path(db, parent_path)
    ensure_unique_folder_name(db, parent.id, name)
    created = Folder(root_key=parent.root_key, parent_id=parent.id, name=name, is_root=False)
    db.add(created)
    db.commit()
    return {"folder": normalized}


@router.post("/folders/rename")
def rename_folder(
    folder: str = Form(...),
    new_path: str = Form(...),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    del user
    source_path = normalize_folder(folder)
    target_path = normalize_folder(new_path)
    source = get_folder_by_path(db, source_path)
    if not source:
        raise HTTPException(status_code=404, detail="Folder not found")
    if source.is_root:
        raise HTTPException(status_code=400, detail="Cannot rename that folder")
    target_ref = parse_public_folder_path(target_path)
    if not target_ref.relative_path:
        raise HTTPException(status_code=400, detail="Invalid target folder")
    if target_path == source_path:
        return {"folder": source_path}
    if target_ref.root_key != source.root_key:
        raise HTTPException(status_code=400, detail="Use archive or restore for Archive moves")
    if target_path.startswith(f"{source_path}/"):
        raise HTTPException(status_code=400, detail="Cannot move a folder into itself")

    target_parent_path = "/".join(target_path.split("/")[:-1])
    target_name = normalize_item_name(target_path.split("/")[-1], "Folder name")
    target_parent = get_or_create_folder_path(db, target_parent_path)
    ensure_unique_folder_name(db, target_parent.id, target_name, source.id)
    source.parent = target_parent
    source.parent_id = target_parent.id
    source.name = target_name
    db.commit()
    return {"folder": target_path}


@router.post("/folders/archive")
def archive_folder(
    request: Request,
    folder: str = Form(...),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    source_path = normalize_folder(folder)
    source = get_folder_by_path(db, source_path)
    if not source or source.is_root:
        raise HTTPException(status_code=404, detail="Folder not found")
    if folder_is_archive(source):
        raise HTTPException(status_code=400, detail="Pick a Vault folder to archive")
    target_path = public_folder_path(ARCHIVE_ROOT_KEY, folder_relative_path(source))
    target_parent = get_or_create_folder_path(db, "/".join(target_path.split("/")[:-1]))
    target_name = normalize_item_name(target_path.split("/")[-1], "Folder name")
    ensure_unique_folder_name(db, target_parent.id, target_name, source.id)
    meta = client_meta(request)
    for doc in docs_in_folder_subtree(db, source):
        release_lock(get_active_lock(doc, db), user)
        record_event(doc, user, "archive", f"Archived from {document_path(doc)}", db, meta=meta)
        doc.latest_modified_at = now_utc()
    source.parent = target_parent
    source.parent_id = target_parent.id
    source.name = target_name
    set_subtree_root_key(db, source, ARCHIVE_ROOT_KEY)
    db.commit()
    return {"archive_folder": target_path}


@router.post("/folders/unarchive")
def unarchive_folder(
    request: Request,
    folder: str = Form(...),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    source_path = normalize_folder(folder)
    source = get_folder_by_path(db, source_path)
    if not source or source.is_root or not folder_is_archive(source):
        raise HTTPException(status_code=400, detail="Choose an archived folder to restore")
    target_path = folder_relative_path(source)
    target_parent = get_or_create_folder_path(db, "/".join(target_path.split("/")[:-1]))
    target_name = normalize_item_name(target_path.split("/")[-1], "Folder name")
    ensure_unique_folder_name(db, target_parent.id, target_name, source.id)
    meta = client_meta(request)
    for doc in docs_in_folder_subtree(db, source):
        record_event(
            doc,
            user,
            "unarchive",
            f"Restored to Vault from {document_path(doc)}",
            db,
            meta=meta,
        )
        doc.latest_modified_at = now_utc()
    source.parent = target_parent
    source.parent_id = target_parent.id
    source.name = target_name
    set_subtree_root_key(db, source, VAULT_ROOT_KEY)
    db.commit()
    return {"folder": target_path}


@router.post("/folders/permanent_delete")
def permanent_delete_folder(
    folder: str = Form(...),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    if not user["is_admin"]:
        raise HTTPException(status_code=403, detail="Admin access required")
    normalized = normalize_folder(folder)
    target = get_folder_by_path(db, normalized)
    if not target or target.is_root or not folder_is_archive(target):
        raise HTTPException(status_code=400, detail="Delete forever is only available in Archive")
    for doc in docs_in_folder_subtree(db, target):
        db.delete(doc)
    db.delete(target)
    db.commit()
    return {"folder": normalized}


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
    target_folder = get_or_create_folder_path(db, folder_path_value)
    ensure_unique_document_path(db, target_folder.id, filename)
    data = await file.read()
    mime_type = file.content_type or mimetypes.guess_type(filename)[0]
    meta = client_meta(request)
    with storage_write_lock():
        blob = get_or_create_blob_for_data(db, data, mime_type)
        doc = Document(
            folder_id=target_folder.id,
            name=filename,
            created_by=user["id"],
            created_by_name=user["name"],
            latest_modified_by=user["id"],
            latest_modified_at=now_utc(),
        )
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
def document_detail(doc_id: int, db: Session = Depends(get_db)) -> RedirectResponse:
    doc = get_document_or_404(doc_id, db)
    folder_value = document_folder_path(doc)
    return RedirectResponse(url=f"/?folder={quote(folder_value)}", status_code=303)


@router.get("/documents/{doc_id}/download")
def download_document(
    doc_id: int,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> Response:
    doc = get_document_or_404(doc_id, db)
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
    )
    db.commit()
    return download_response(data, doc.name, version.mime_type)


@router.get("/documents/{doc_id}/checkout")
def checkout_document(
    doc_id: int,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> Response:
    doc = get_document_or_404(doc_id, db)
    if document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Restore this file before editing")
    version = current_version(doc, db)
    if not version:
        raise HTTPException(status_code=404, detail="Document has no versions")
    meta = client_meta(request)
    with storage_write_lock():
        acquire_document_lock(doc, user, meta, db)
        record_event(doc, user, "checkout", f"Checked out {document_path(doc)}", db, meta=meta)
        data = read_version_bytes(version)
        db.commit()
    return download_response(data, doc.name, version.mime_type)


@router.post("/documents/{doc_id}/lock")
def lock_document(
    doc_id: int,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, object]:
    doc = get_document_or_404(doc_id, db)
    if document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Restore this file before editing")
    meta = client_meta(request)
    with storage_write_lock():
        lock, created = acquire_document_lock(doc, user, meta, db)
        if created:
            record_event(doc, user, "lock", f"Locked {document_path(doc)}", db, meta=meta)
        db.commit()
    return {
        "locked": True,
        "lock": {
            "by": lock.locked_by,
            "name": lock.locked_by_name,
            "at": lock.locked_at.isoformat() if lock.locked_at else None,
        },
    }


@router.post("/documents/{doc_id}/release")
def release_document(
    doc_id: int,
    request: Request,
    mode: str = "redirect",
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> Response:
    doc = get_document_or_404(doc_id, db)
    lock = get_active_lock(doc, db)
    if lock and lock.locked_by != user["id"] and not user["is_admin"]:
        raise HTTPException(status_code=403, detail="Document is locked by another user")
    release_lock(lock, user)
    record_event(
        doc,
        user,
        "release",
        f"Released lock for {document_path(doc)}",
        db,
        meta=client_meta(request),
    )
    db.commit()
    if mode == "json":
        return JSONResponse({"released": True})
    return RedirectResponse(url=f"/?folder={quote(document_folder_path(doc))}", status_code=303)


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
    mime_type = file.content_type or mimetypes.guess_type(upload_name)[0]
    meta = client_meta(request)
    message = note.strip() or f"Uploaded {upload_name}"
    with storage_write_lock():
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


@router.post("/documents/{doc_id}/move")
def move_document(
    doc_id: int,
    request: Request,
    new_path: str = Form(...),
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    doc = get_document_or_404(doc_id, db)
    ensure_not_locked_by_other(doc, user, db)
    old_path = document_path(doc)
    target_folder, target_name = folder_for_new_path(db, new_path)
    if doc.folder.root_key != target_folder.root_key:
        raise HTTPException(status_code=400, detail="Use archive or restore for Archive moves")
    with storage_write_lock():
        mutate_doc_location(
            doc,
            target_folder,
            target_name,
            user,
            db,
            client_meta(request),
            "move",
            f"Moved from {old_path} to {join_path(folder_path(target_folder), target_name)}",
        )
        db.commit()
    return {"path": document_path(doc)}


@router.post("/documents/{doc_id}/archive")
def archive_document(
    doc_id: int,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    doc = get_document_or_404(doc_id, db)
    source_path = document_path(doc)
    if document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Document is already archived")
    lock = ensure_not_locked_by_other(doc, user, db)
    target_folder_path = public_folder_path(ARCHIVE_ROOT_KEY, folder_relative_path(doc.folder))
    target_folder = get_or_create_folder_path(db, target_folder_path)
    with storage_write_lock():
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
        )
        db.commit()
    return {"path": document_path(doc)}


@router.post("/documents/{doc_id}/unarchive")
def unarchive_document(
    doc_id: int,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    doc = get_document_or_404(doc_id, db)
    source_path = document_path(doc)
    if not document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Document is not archived")
    ensure_not_locked_by_other(doc, user, db)
    target_folder_path = folder_relative_path(doc.folder)
    target_folder = get_or_create_folder_path(db, target_folder_path)
    with storage_write_lock():
        mutate_doc_location(
            doc,
            target_folder,
            doc.name,
            user,
            db,
            client_meta(request),
            "unarchive",
            f"Restored to Vault from {source_path}",
        )
        db.commit()
    return {"path": document_path(doc)}


@router.post("/documents/{doc_id}/delete")
def delete_document(
    doc_id: int,
    request: Request,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    doc = get_document_or_404(doc_id, db)
    if not document_is_archive(doc):
        return archive_document(doc_id, request, user, db)
    return permanent_delete_document(doc_id, user, db)


@router.post("/documents/{doc_id}/permanent_delete")
def permanent_delete_document(
    doc_id: int,
    user: UserContext = Depends(current_user),
    db: Session = Depends(get_db),
) -> dict[str, str]:
    if not user["is_admin"]:
        raise HTTPException(status_code=403, detail="Admin access required")
    doc = get_document_or_404(doc_id, db)
    if not document_is_archive(doc):
        raise HTTPException(status_code=400, detail="Move the document to Archive before deleting")
    deleted_path = document_path(doc)
    db.delete(doc)
    db.commit()
    return {"deleted": deleted_path}
