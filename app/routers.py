# Copyright (c) 2024 The Allen Family
"""HTTP routes for the vault service."""

import datetime as dt
import hashlib
import mimetypes
import shutil
import uuid
from collections import defaultdict
from collections.abc import Iterable
from pathlib import Path

from fastapi import APIRouter, Depends, File, Form, HTTPException, Request, Response, UploadFile
from fastapi.responses import FileResponse, HTMLResponse, JSONResponse, RedirectResponse
from fastapi.templating import Jinja2Templates
from sqlalchemy import select
from sqlalchemy.exc import NoResultFound
from sqlalchemy.orm import Session

from .auth import UserContext, current_user
from .config import BASE_DOMAIN, FILES_PATH
from .db import get_db
from .models import Document, DocumentEvent, DocumentLock, DocumentVersion
from .storage import (
    StagedChange,
    ensure_storage,
    new_version_id,
    safe_path,
    stage_move,
    stage_write,
    storage_write_lock,
    version_dir,
    version_file_path,
)

templates = Jinja2Templates(directory=str(Path(__file__).parent / "templates"))

router = APIRouter()
ARCHIVE_ROOT = "Archive"


def safe_redirect(target: str | None) -> str:
    if not target or not target.startswith("/"):
        return "/"
    return target


def client_meta(request: Request) -> dict[str, str | None]:
    """Extract IP and user agent for auditing."""
    xff = request.headers.get("x-forwarded-for")
    ip = (xff.split(",")[0].strip() if xff else None) or (
        request.client.host if request.client else None
    )
    ua = request.headers.get("user-agent")
    return {"ip": ip, "user_agent": ua}


def normalize_folder(folder: str) -> str:
    cleaned = folder.strip().replace("\\", "/").strip("/")
    if not cleaned:
        return ""
    parts = [part for part in cleaned.split("/") if part]
    if any(part in (".", "..") for part in parts):
        raise HTTPException(status_code=400, detail="Invalid folder path")
    return "/".join(parts)


def is_archived_path(path: str) -> bool:
    normalized = path.replace("\\", "/").lstrip("/")
    return normalized == ARCHIVE_ROOT or normalized.startswith(f"{ARCHIVE_ROOT}/")


def archive_path_for(path: str) -> str:
    normalized = path.replace("\\", "/").lstrip("/")
    if is_archived_path(normalized):
        return normalized
    return f"{ARCHIVE_ROOT}/{normalized}"


def prune_empty_archived_parents(path: Path) -> None:
    """Remove empty archive folders up to the Archive root."""
    archive_root = safe_path(ARCHIVE_ROOT)
    if archive_root not in path.parents:
        return

    current = path
    while archive_root in current.parents:
        if current.exists():
            if any(current.iterdir()):
                break
            current.rmdir()
        current = current.parent


def ensure_not_archived(doc: Document) -> None:
    if is_archived_path(doc.path):
        raise HTTPException(
            status_code=400,
            detail="Document is archived. Unarchive it before editing.",
        )


def doc_in_folder(doc_path: str, folder: str) -> bool:
    folder = folder.strip().strip("/")
    doc_folder = "/".join(Path(doc_path).parent.parts)
    if not folder:
        return doc_folder == ""
    return doc_folder == folder or doc_folder.startswith(f"{folder}/")


def discover_folders() -> set[str]:
    ensure_storage()
    folders: set[str] = {""}
    for path in FILES_PATH.rglob("*"):
        if not path.is_dir():
            continue
        try:
            rel = path.relative_to(FILES_PATH)
        except ValueError:
            continue
        if not rel.parts:
            continue
        if rel.parts[0] == ".versions":
            continue
        folders.add(str(rel).replace("\\", "/"))
    return folders


def build_folder_maps(
    documents: Iterable[Document],
    existing_dirs: set[str] | None = None,
) -> tuple[dict[str, list[Document]], dict[str, set[str]]]:
    from collections import defaultdict as dd  # Inline import to limit surface area.
    from pathlib import Path

    folder_docs = dd(list)
    folder_children = dd(set)
    for doc in documents:
        parts = Path(doc.path).parts
        parent_parts = parts[:-1]
        parent_folder = "/".join(parent_parts)
        folder_docs[parent_folder].append(doc)
        for idx in range(len(parent_parts)):
            parent = "/".join(parent_parts[:idx])
            child_path = "/".join(parent_parts[: idx + 1])
            folder_children[parent].add(child_path)
    existing_dirs = existing_dirs or set()
    for dir_path in existing_dirs:
        if dir_path == "":
            continue
        dir_parts: list[str] = list(Path(dir_path).parts) if dir_path else []
        parent_folder = "/".join(dir_parts[:-1]) if dir_parts else ""
        folder_children[parent_folder].add(dir_path)
        folder_children.setdefault(dir_path, set())
    folder_children.setdefault("", set())
    return folder_docs, folder_children


def ensure_not_locked_by_other(
    doc: Document,
    user: UserContext,
    db: Session,
) -> DocumentLock | None:
    lock = (
        db.execute(
            select(DocumentLock).where(
                DocumentLock.document_id == doc.id,
                DocumentLock.is_active == True,  # noqa: E712
            ),
        )
        .scalars()
        .first()
    )
    if lock and lock.locked_by != user["id"]:
        raise HTTPException(status_code=403, detail="Document is locked by another user")
    return lock


def next_version_number(doc_id: int, db: Session) -> int:
    latest_number = (
        db.execute(
            select(DocumentVersion.version_number)
            .where(DocumentVersion.document_id == doc_id)
            .order_by(DocumentVersion.version_number.desc())
            .limit(1)
        )
        .scalars()
        .first()
    )
    return (latest_number or 0) + 1


def latest_version_checksum(doc: Document, db: Session) -> str | None:
    return (
        db.execute(
            select(DocumentVersion.checksum)
            .where(DocumentVersion.document_id == doc.id)
            .order_by(DocumentVersion.version_number.desc())
            .limit(1),
        )
        .scalars()
        .first()
    )


def dedupe_versions_by_checksum(versions: Iterable[DocumentVersion]) -> list[DocumentVersion]:
    """Return versions with consecutive duplicate hashes removed (latest-first)."""
    if not versions:
        return []
    filtered: list[DocumentVersion] = []
    last_checksum: str | None = None
    have_last_checksum = False
    for version in sorted(versions, key=lambda v: v.version_number or 0, reverse=True):
        checksum = version.checksum
        if have_last_checksum and checksum and checksum == last_checksum:
            continue
        filtered.append(version)
        if checksum:
            last_checksum = checksum
            have_last_checksum = True
        else:
            last_checksum = None
            have_last_checksum = False
    return filtered


def version_event_type(created_via: str | None) -> str | None:
    if not created_via:
        return None
    return created_via


VersionSignature = tuple[str | None, str | None, str | None, int | None]


def version_signature(version: DocumentVersion) -> VersionSignature:
    event_type = version_event_type(version.created_via)
    actor = version.committed_by_name or version.committed_by
    ts = int(version.committed_at.timestamp()) if version.committed_at else None
    message = (version.message or "").strip()
    return (event_type, message, actor, ts)


def event_signature(event: DocumentEvent) -> VersionSignature:
    ts = int(event.created_at.timestamp()) if event.created_at else None
    message = (event.message or "").strip()
    actor = event.actor_name or event.actor
    return (event.event_type, message, actor, ts)


def snapshot_version(
    doc: Document,
    data: bytes,
    user: UserContext,
    message: str,
    db: Session,
    update_latest: bool = True,
    locked: bool = False,
    meta: dict[str, str | None] | None = None,
    filename: str | None = None,
    created_via: str | None = None,
    checksum: str | None = None,
) -> str:
    if not locked:
        with storage_write_lock():
            return snapshot_version(
                doc,
                data,
                user,
                message,
                db,
                update_latest=update_latest,
                locked=True,
                meta=meta,
                filename=filename,
                created_via=created_via,
                checksum=checksum,
            )
    version_id = new_version_id()
    version_path = version_file_path(doc, version_id)
    staged_version = stage_write(version_path, data)
    # Remove any temporary backups immediately; callers will clean up the main file on rollback.
    staged_version.finalize()
    version_number = next_version_number(doc.id, db)
    checksum_value = checksum or hashlib.sha256(data).hexdigest()
    mime_type = None
    if meta and meta.get("mime_type"):
        mime_type = meta.get("mime_type")
    else:
        guess = mimetypes.guess_type(filename or doc.path)[0]
        mime_type = guess
    size_bytes = len(data) if data is not None else None
    upload_ip = meta.get("ip") if meta else None
    upload_user_agent = meta.get("user_agent") if meta else None
    original_filename = filename or Path(doc.path).name
    if update_latest:
        doc.latest_commit = version_id
        doc.latest_modified_by = user["id"]
        doc.latest_modified_at = dt.datetime.now(tz=dt.UTC)
        doc.latest_version_number = version_number
    doc.version_count = max(doc.version_count or 0, version_number)
    version = DocumentVersion(
        document_id=doc.id,
        commit_hash=version_id,
        version_number=version_number,
        committed_by=user["id"],
        committed_by_name=user["name"],
        message=message,
        checksum=checksum_value,
        hash_algo="sha256",
        size_bytes=size_bytes,
        mime_type=mime_type,
        original_filename=original_filename,
        upload_ip=upload_ip,
        upload_user_agent=upload_user_agent,
        created_via=created_via,
    )
    db.add(version)
    return version_id


def cleanup_version_file(doc: Document, version_id: str) -> None:
    with storage_write_lock():
        path = version_file_path(doc, version_id)
        if path.exists():
            path.unlink()


def record_snapshot_from_disk(
    doc: Document,
    user: UserContext,
    message: str,
    db: Session,
    update_latest: bool = False,
    locked: bool = False,
    meta: dict[str, str | None] | None = None,
) -> str | None:
    if not locked:
        with storage_write_lock():
            return record_snapshot_from_disk(
                doc,
                user,
                message,
                db,
                update_latest=update_latest,
                locked=True,
                meta=meta,
            )
    ensure_storage()
    source = safe_path(doc.path)
    if not source.exists():
        raise HTTPException(status_code=404, detail="File missing from repository")
    data = source.read_bytes()
    latest_checksum = latest_version_checksum(doc, db)
    checksum_value = hashlib.sha256(data).hexdigest()
    if latest_checksum and latest_checksum == checksum_value:
        return None
    return snapshot_version(
        doc,
        data,
        user,
        message,
        db,
        update_latest=update_latest,
        locked=True,
        meta=meta,
        filename=Path(doc.path).name,
        created_via="system",
        checksum=checksum_value,
    )


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


def archive_document(
    doc: Document,
    user: UserContext,
    db: Session,
    meta: dict[str, str | None] | None = None,
) -> tuple[StagedChange, str | None]:
    if is_archived_path(doc.path):
        raise HTTPException(status_code=400, detail="Document is already archived")
    lock = ensure_not_locked_by_other(doc, user, db)
    if lock:
        lock.is_active = False

    source_rel = doc.path
    target_rel = archive_path_for(doc.path)
    existing = db.execute(select(Document).where(Document.path == target_rel)).scalars().first()
    if existing:
        raise HTTPException(
            status_code=400,
            detail="A document already exists in the archive at that path",
        )
    with storage_write_lock():
        ensure_storage()
        source = safe_path(source_rel)
        target = safe_path(target_rel)
        if not source.exists():
            raise HTTPException(status_code=404, detail="Source file missing")
        if target.exists():
            raise HTTPException(
                status_code=400,
                detail="A file already exists in the archive at that path",
            )
        data = source.read_bytes()
        checksum_value = hashlib.sha256(data).hexdigest()
        latest_checksum = latest_version_checksum(doc, db)
        move_stage = stage_move(source, target)
        version_id = None
        try:
            doc.path = str(target.relative_to(FILES_PATH))
            if not latest_checksum or latest_checksum != checksum_value:
                version_id = snapshot_version(
                    doc,
                    data,
                    user,
                    f"Archived from {source_rel}",
                    db,
                    locked=True,
                    meta=meta,
                    filename=Path(source_rel).name,
                    created_via="archive",
                    checksum=checksum_value,
                )
            record_event(doc, user, "archive", f"Archived from {source_rel}", db, meta=meta)
        except Exception:
            move_stage.rollback()
            raise
    return move_stage, version_id


def unarchive_document(
    doc: Document,
    user: UserContext,
    db: Session,
    meta: dict[str, str | None] | None = None,
) -> tuple[StagedChange, str | None]:
    if not is_archived_path(doc.path):
        raise HTTPException(status_code=400, detail="Document is not archived")
    lock = ensure_not_locked_by_other(doc, user, db)
    if lock:
        lock.is_active = False

    archived_rel = doc.path
    restored_rel = archived_rel.split("/", 1)[1] if "/" in archived_rel else ""
    if not restored_rel:
        raise HTTPException(status_code=400, detail="Cannot determine original path to unarchive")

    existing = db.execute(select(Document).where(Document.path == restored_rel)).scalars().first()
    if existing:
        raise HTTPException(
            status_code=400,
            detail="Another document already exists at the original path",
        )

    with storage_write_lock():
        ensure_storage()
        source = safe_path(archived_rel)
        target = safe_path(restored_rel)
        if not source.exists():
            raise HTTPException(status_code=404, detail="Archived file missing")
        if target.exists():
            raise HTTPException(status_code=400, detail="A file already exists at the restore path")
        data = source.read_bytes()
        checksum_value = hashlib.sha256(data).hexdigest()
        latest_checksum = latest_version_checksum(doc, db)
        move_stage = stage_move(source, target)
        source_parent = source.parent

        def finalize_with_cleanup() -> None:
            move_stage.finalize()
            prune_empty_archived_parents(source_parent)

        cleanup_stage = StagedChange(move_stage.rollback, finalize_with_cleanup)
        version_id = None
        try:
            doc.path = str(target.relative_to(FILES_PATH))
            if not latest_checksum or latest_checksum != checksum_value:
                version_id = snapshot_version(
                    doc,
                    data,
                    user,
                    f"Unarchived to {restored_rel}",
                    db,
                    locked=True,
                    meta=meta,
                    filename=Path(restored_rel).name,
                    created_via="unarchive",
                    checksum=checksum_value,
                )
            record_event(doc, user, "unarchive", f"Unarchived to {restored_rel}", db, meta=meta)
        except Exception:
            cleanup_stage.rollback()
            raise
    return cleanup_stage, version_id


def build_folder_tree(
    folder_children: dict[str, set[str]],
    node_path: str = "",
) -> list[dict[str, object]]:
    children: list[dict[str, object]] = []
    for child_path in sorted(folder_children.get(node_path, [])):
        child_payload: dict[str, object] = {
            "name": child_path.split("/")[-1] if child_path else "Vault",
            "path": child_path,
            "children": build_folder_tree(folder_children, child_path),
        }
        children.append(child_payload)
    return children


def breadcrumbs_for(folder: str) -> list[dict[str, str]]:
    crumbs = [{"name": "Vault", "path": ""}]
    if not folder:
        return crumbs
    parts = [part for part in folder.split("/") if part]
    for idx, part in enumerate(parts):
        crumbs.append({"name": part, "path": "/".join(parts[: idx + 1])})
    return crumbs


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


def folder_stat_mtime(folder: str) -> dt.datetime | None:
    target = FILES_PATH if not folder else FILES_PATH / folder
    try:
        stat = target.stat()
    except OSError:
        return None
    return dt.datetime.fromtimestamp(stat.st_mtime, tz=dt.UTC)


def get_document_or_404(doc_id: int, db: Session) -> Document:
    try:
        return db.execute(select(Document).where(Document.id == doc_id)).scalar_one()
    except NoResultFound as exc:
        raise HTTPException(status_code=404, detail="Document not found") from exc


def build_state(user: UserContext, current_folder: str, db: Session) -> dict[str, object]:
    docs = db.execute(select(Document)).scalars().all()
    existing_dirs = discover_folders()
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

    versions_by_document = defaultdict(list)
    events_by_document = defaultdict(list)
    for version in versions:
        versions_by_document[version.document_id].append(version)
    for event in events:
        events_by_document[event.document_id].append(event)

    doc_payloads = []
    doc_stats = []
    for doc in docs:
        doc_versions = versions_by_document.get(doc.id, [])
        filtered_versions = dedupe_versions_by_checksum(doc_versions)
        version_signatures = {version_signature(v) for v in filtered_versions}
        latest_version = filtered_versions[0] if filtered_versions else None
        latest_size_bytes = latest_version.size_bytes if latest_version else None
        if latest_size_bytes is None:
            try:
                latest_size_bytes = safe_path(doc.path).stat().st_size
            except (OSError, HTTPException):
                latest_size_bytes = None
        latest_updated_at = normalize_timestamp(doc.latest_modified_at)
        lock = locks.get(doc.id)
        archived = is_archived_path(doc.path)
        history_items = []
        for version in filtered_versions:
            history_items.append(
                {
                    "id": version.commit_hash,
                    "type": "version",
                    "timestamp": version.committed_at.isoformat() if version.committed_at else None,
                    "display": version.committed_at.strftime("%b %d, %Y %H:%M")
                    if version.committed_at
                    else "Version",
                    "by": version.committed_by_name or version.committed_by,
                    "note": version.message,
                    "version_number": version.version_number,
                    "created_via": version.created_via,
                    "checksum": version.checksum,
                    "hash_algo": version.hash_algo,
                    "size_bytes": version.size_bytes,
                    "mime_type": version.mime_type,
                    "original_filename": version.original_filename,
                    "download_url": f"/documents/{doc.id}/versions/{version.commit_hash}/download",
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
        history_items.sort(key=lambda item: item["timestamp"] or "", reverse=True)
        doc_payloads.append(
            {
                "id": doc.id,
                "name": Path(doc.path).name,
                "path": doc.path,
                "folder": "/".join(Path(doc.path).parent.parts),
                "latest_updated_at": latest_updated_at.isoformat() if latest_updated_at else None,
                "latest_updated_display": format_mtime(latest_updated_at),
                "latest_by": (latest_version.committed_by_name or latest_version.committed_by)
                if latest_version
                else None,
                "latest_message": latest_version.message if latest_version else None,
                "latest_version_number": latest_version.version_number
                if latest_version
                else doc.latest_version_number,
                "version_count": len(filtered_versions) or (doc.version_count or 0),
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
                "archived": archived,
                "versions": history_items,
            },
        )
        doc_stats.append(
            {
                "path": doc.path,
                "size_bytes": latest_size_bytes or 0,
                "mtime": latest_updated_at,
            },
        )

    _folder_docs, folder_children = build_folder_maps(docs, existing_dirs)
    folder_children_serialized = {
        folder: sorted(children) for folder, children in folder_children.items()
    }
    all_folders = set(folder_children)
    for children in folder_children.values():
        all_folders.update(children)
    folder_payloads = {}
    for folder_path in all_folders:
        folder_size = 0
        latest_folder_mtime = folder_stat_mtime(folder_path)
        for doc_stat in doc_stats:
            if not doc_in_folder(str(doc_stat["path"]), folder_path):
                continue
            folder_size += int(doc_stat["size_bytes"])
            doc_mtime = doc_stat["mtime"]
            if doc_mtime and (
                not latest_folder_mtime or doc_mtime > latest_folder_mtime
            ):
                latest_folder_mtime = doc_mtime
        folder_payloads[folder_path] = {
            "name": folder_path.split("/")[-1] if folder_path else "Vault",
            "path": folder_path,
            "latest_updated_at": latest_folder_mtime.isoformat()
            if latest_folder_mtime
            else None,
            "latest_updated_display": format_mtime(latest_folder_mtime),
            "size_bytes": folder_size,
            "size_display": format_size(folder_size),
        }
    breadcrumbs = breadcrumbs_for(current_folder)
    subfolders = sorted(folder_children.get(current_folder, []))

    return {
        "current_folder": current_folder,
        "subfolders": subfolders,
        "breadcrumbs": breadcrumbs,
        "user": user,
        "base_domain": BASE_DOMAIN,
        "doc_payloads": doc_payloads,
        "folder_children": folder_children_serialized,
        "folder_payloads": folder_payloads,
    }


@router.get("/", response_class=HTMLResponse)
def index(request: Request, db: Session = Depends(get_db)) -> HTMLResponse:
    user = current_user(request)
    current_folder = normalize_folder(request.query_params.get("folder", ""))
    state = build_state(user, current_folder, db)
    return templates.TemplateResponse(
        "index.html",
        {
            "request": request,
            "state": state,
        },
    )


@router.get("/api/state", response_class=JSONResponse)
def state_api(request: Request, db: Session = Depends(get_db)) -> JSONResponse:
    user = current_user(request)
    current_folder = normalize_folder(request.query_params.get("folder", ""))
    state = build_state(user, current_folder, db)
    return JSONResponse(state)


@router.post("/folders", response_class=JSONResponse)
def create_folder(request: Request, folder: str = Form(...)) -> JSONResponse:
    current_user(request)
    normalized = normalize_folder(folder)
    if not normalized:
        raise HTTPException(status_code=400, detail="Folder name is required")
    target = safe_path(normalized)
    with storage_write_lock():
        ensure_storage()
        if target.exists():
            raise HTTPException(status_code=400, detail="A folder or file already exists there")
        target.mkdir(parents=True, exist_ok=False)
    return JSONResponse({"ok": True, "path": normalized})


@router.post("/folders/archive", response_class=JSONResponse)
def archive_folder(
    request: Request,
    folder: str = Form(...),
    db: Session = Depends(get_db),
) -> JSONResponse:
    user = current_user(request)
    normalized = normalize_folder(folder)
    if not normalized:
        raise HTTPException(status_code=400, detail="Folder is required")
    if is_archived_path(normalized):
        raise HTTPException(status_code=400, detail="Folder is already in the archive")

    docs = db.execute(select(Document)).scalars().all()
    to_archive = [doc for doc in docs if doc_in_folder(doc.path, normalized)]
    original_paths = {doc.id: doc.path for doc in to_archive}
    for doc in to_archive:
        ensure_not_locked_by_other(doc, user, db)
    stages = []
    meta = client_meta(request)
    for doc in to_archive:
        stage, version_id = archive_document(doc, user, db, meta=meta)
        stages.append((doc, stage, version_id))
    try:
        db.commit()
    except Exception:
        for doc in to_archive:
            doc.path = original_paths.get(doc.id, doc.path)
        for doc, stage, version_id in reversed(stages):
            stage.rollback()
            if version_id:
                cleanup_version_file(doc, version_id)
        db.rollback()
        raise
    else:
        with storage_write_lock():
            for _, stage, _ in stages:
                stage.finalize()
            folder_path = safe_path(normalized)
            if folder_path.exists():
                if any(folder_path.iterdir()):
                    # Leave the folder intact if anything remains that was not tracked.
                    pass
                else:
                    folder_path.rmdir()
    return JSONResponse(
        {"ok": True, "archived": len(to_archive), "archive_folder": archive_path_for(normalized)},
    )


@router.post("/folders/unarchive", response_class=JSONResponse)
def unarchive_folder(
    request: Request,
    folder: str = Form(...),
    db: Session = Depends(get_db),
) -> JSONResponse:
    user = current_user(request)
    normalized = normalize_folder(folder)
    if not normalized:
        raise HTTPException(status_code=400, detail="Folder is required")
    if not is_archived_path(normalized):
        raise HTTPException(status_code=400, detail="Folder is not in the archive")

    docs = db.execute(select(Document)).scalars().all()
    to_unarchive = [doc for doc in docs if doc_in_folder(doc.path, normalized)]
    original_paths = {doc.id: doc.path for doc in to_unarchive}
    for doc in to_unarchive:
        ensure_not_locked_by_other(doc, user, db)
    stages = []
    meta = client_meta(request)
    for doc in to_unarchive:
        stage, version_id = unarchive_document(doc, user, db, meta=meta)
        stages.append((doc, stage, version_id))
    dest_folder = normalized.split("/", 1)[1] if "/" in normalized else ""
    try:
        db.commit()
    except Exception:
        for doc in to_unarchive:
            doc.path = original_paths.get(doc.id, doc.path)
        for doc, stage, version_id in reversed(stages):
            stage.rollback()
            if version_id:
                cleanup_version_file(doc, version_id)
        db.rollback()
        raise
    else:
        with storage_write_lock():
            for _, stage, _ in stages:
                stage.finalize()
            archived_folder = safe_path(normalized)
            if archived_folder.exists() and not any(archived_folder.iterdir()):
                archived_folder.rmdir()
    return JSONResponse({"ok": True, "unarchived": len(to_unarchive), "folder": dest_folder})


@router.post("/folders/rename", response_class=JSONResponse)
def rename_folder(
    request: Request,
    folder: str = Form(...),
    new_path: str = Form(...),
    db: Session = Depends(get_db),
) -> JSONResponse:
    user = current_user(request)
    meta = client_meta(request)
    original = normalize_folder(folder)
    target = normalize_folder(new_path)
    if not original:
        raise HTTPException(status_code=400, detail="Folder is required")
    if not target:
        raise HTTPException(status_code=400, detail="New name is required")
    if original == target:
        return JSONResponse({"ok": True, "folder": target, "renamed": 0})
    if is_archived_path(original) != is_archived_path(target):
        raise HTTPException(
            status_code=400,
            detail="Use archive/unarchive to move folders in or out of Archive",
        )
    if target.startswith(f"{original}/"):
        raise HTTPException(status_code=400, detail="Cannot move a folder into its own subfolder")

    docs = db.execute(select(Document)).scalars().all()
    to_rename = [doc for doc in docs if doc_in_folder(doc.path, original)]

    for doc in to_rename:
        ensure_not_locked_by_other(doc, user, db)

    prefix = f"{original}/"
    path_map: dict[int, str] = {}
    for doc in to_rename:
        if not doc.path.startswith(prefix):
            raise HTTPException(
                status_code=400,
                detail="Unable to rename folder due to inconsistent document paths",
            )
        suffix = doc.path[len(original) :].lstrip("/")
        new_doc_path = f"{target}/{suffix}" if suffix else target
        path_map[doc.id] = new_doc_path

    existing_paths = {d.path: d.id for d in docs}
    for doc_id, new_doc_path in path_map.items():
        other_id = existing_paths.get(new_doc_path)
        if other_id and other_id != doc_id:
            raise HTTPException(
                status_code=400,
                detail=f"A document already exists at {new_doc_path}",
            )
        if safe_path(new_doc_path).exists():
            raise HTTPException(status_code=400, detail=f"A file already exists at {new_doc_path}")

    with storage_write_lock():
        ensure_storage()
        source_dir = safe_path(original)
        target_dir = safe_path(target)
        if not source_dir.exists():
            raise HTTPException(status_code=404, detail="Source folder missing")
        if target_dir.exists():
            raise HTTPException(status_code=400, detail="Target folder already exists")
        target_dir.parent.mkdir(parents=True, exist_ok=True)
        source_dir.rename(target_dir)

        original_paths = {doc.id: doc.path for doc in to_rename}
        version_ids = {}
        try:
            for doc in to_rename:
                new_doc_path = path_map[doc.id]
                doc.path = new_doc_path
                version_ids[doc.id] = record_snapshot_from_disk(
                    doc,
                    user,
                    f"Folder renamed from {original} to {target} "
                    f"(was {original_paths.get(doc.id)})",
                    db,
                    update_latest=True,
                    locked=True,
                    meta=meta,
                )
                record_event(
                    doc,
                    user,
                    "move",
                    f"Folder renamed from {original} to {target} "
                    f"(was {original_paths.get(doc.id)})",
                    db,
                    meta=meta,
                )
            db.commit()
        except Exception:
            # Restore folder and metadata on failure.
            if target_dir.exists():
                target_dir.rename(source_dir)
            for doc in to_rename:
                doc.path = original_paths.get(doc.id, doc.path)
                vid = version_ids.get(doc.id)
                if vid:
                    cleanup_version_file(doc, vid)
            db.rollback()
            raise

    return JSONResponse({"ok": True, "folder": target, "renamed": len(to_rename)})


@router.post("/folders/permanent_delete", response_class=JSONResponse)
def permanent_delete_folder(
    request: Request,
    folder: str = Form(...),
    db: Session = Depends(get_db),
) -> JSONResponse:
    user = current_user(request)
    if not user.get("is_admin"):
        raise HTTPException(status_code=403, detail="Admin role required to permanently delete")
    normalized = normalize_folder(folder)
    if not normalized:
        raise HTTPException(status_code=400, detail="Folder is required")
    if not is_archived_path(normalized):
        raise HTTPException(status_code=400, detail="Archive the folder before permanent deletion")
    if normalized == ARCHIVE_ROOT:
        raise HTTPException(status_code=400, detail="Cannot permanently delete the entire archive")

    docs = db.execute(select(Document)).scalars().all()
    to_delete = [doc for doc in docs if doc_in_folder(doc.path, normalized)]
    for doc in to_delete:
        if not is_archived_path(doc.path):
            raise HTTPException(
                status_code=400,
                detail="Archive the documents in this folder before permanent deletion",
            )
        lock = ensure_not_locked_by_other(doc, user, db)
        if lock:
            lock.is_active = False

    backups = []
    with storage_write_lock():
        ensure_storage()
        for doc in to_delete:
            path = safe_path(doc.path)
            versions_path = version_dir(doc.id)
            path_backup = None
            versions_backup = None
            if path.exists():
                path_backup = path.with_name(f"{path.name}.bak-{uuid.uuid4().hex}")
                path.replace(path_backup)
            if versions_path.exists():
                versions_backup = versions_path.with_name(
                    f"{versions_path.name}.bak-{uuid.uuid4().hex}",
                )
                versions_path.replace(versions_backup)
            backups.append((doc, path, path_backup, versions_path, versions_backup))

    try:
        doc_ids = [doc.id for doc in to_delete]
        if doc_ids:
            db.query(DocumentLock).filter(DocumentLock.document_id.in_(doc_ids)).delete(
                synchronize_session=False,
            )
            db.query(DocumentVersion).filter(DocumentVersion.document_id.in_(doc_ids)).delete(
                synchronize_session=False,
            )
            for doc in to_delete:
                db.delete(doc)
        db.commit()
    except Exception:
        with storage_write_lock():
            for _doc, path, path_backup, versions_path, versions_backup in reversed(backups):
                if path_backup and Path(path_backup).exists():
                    if path.exists():
                        path.unlink()
                    Path(path_backup).replace(path)
                if versions_backup and Path(versions_backup).exists():
                    if versions_path.exists():
                        shutil.rmtree(versions_path, ignore_errors=True)
                    Path(versions_backup).replace(versions_path)
        db.rollback()
        raise
    else:
        with storage_write_lock():
            for _, _, path_backup, _, versions_backup in backups:
                if path_backup and Path(path_backup).exists():
                    Path(path_backup).unlink()
                if versions_backup and Path(versions_backup).exists():
                    shutil.rmtree(versions_backup, ignore_errors=True)
            folder_path = safe_path(normalized)
            if folder_path.exists():
                shutil.rmtree(folder_path, ignore_errors=True)
    return JSONResponse({"ok": True, "deleted": len(to_delete), "folder": normalized})


@router.post("/documents", response_class=RedirectResponse)
async def upload_document(
    request: Request,
    file: UploadFile = File(...),
    folder: str = Form(""),
    redirect_to: str | None = Form(None),
    db: Session = Depends(get_db),
) -> RedirectResponse:
    user = current_user(request)
    meta = client_meta(request)
    current_folder = normalize_folder(folder)
    filename = Path(file.filename or "").name
    if not filename:
        raise HTTPException(status_code=400, detail="Filename missing")
    rel_path = "/".join([part for part in [current_folder, filename] if part])
    data = await file.read()
    version_id = new_version_id()
    now = dt.datetime.now(tz=dt.UTC)

    with storage_write_lock():
        ensure_storage()
        target = safe_path(rel_path)
        if target.exists():
            raise HTTPException(status_code=400, detail="File already exists")

        doc = Document(
            path=str(target.relative_to(FILES_PATH)),
            created_by=user["id"],
            created_by_name=user["name"],
            latest_commit=version_id,
            latest_modified_by=user["id"],
            latest_modified_at=now,
            latest_version_number=1,
            version_count=1,
        )
        db.add(doc)
        db.flush()

        working_stage = stage_write(target, data)
        version_path = version_file_path(doc, version_id)
        version_stage = stage_write(version_path, data)

        version = DocumentVersion(
            document_id=doc.id,
            commit_hash=version_id,
            version_number=1,
            committed_by=user["id"],
            committed_by_name=user["name"],
            message=f"Initial upload of {filename}",
            checksum=hashlib.sha256(data).hexdigest(),
            hash_algo="sha256",
            size_bytes=len(data),
            mime_type=file.content_type or mimetypes.guess_type(filename)[0],
            original_filename=filename,
            upload_ip=meta.get("ip"),
            upload_user_agent=meta.get("user_agent"),
            created_via="upload",
        )
        db.add(version)
        record_event(doc, user, "upload", f"Uploaded {filename}", db, meta=meta)

        try:
            db.commit()
        except Exception:
            working_stage.rollback()
            version_stage.rollback()
            db.rollback()
            raise
        else:
            working_stage.finalize()
            version_stage.finalize()
    return RedirectResponse(url=safe_redirect(redirect_to), status_code=303)


@router.get("/documents/{doc_id}/download")
def download(doc_id: int, request: Request, db: Session = Depends(get_db)) -> FileResponse:
    user = current_user(request)
    meta = client_meta(request)
    doc = get_document_or_404(doc_id, db)
    latest_version = (
        db.execute(
            select(DocumentVersion)
            .where(DocumentVersion.document_id == doc.id)
            .order_by(DocumentVersion.version_number.desc())
            .limit(1),
        )
        .scalars()
        .first()
    )
    version_commit = latest_version.commit_hash if latest_version else doc.latest_commit
    version_label = f"v{latest_version.version_number}" if latest_version else None
    if not version_label and version_commit:
        version_label = f"commit {version_commit}"
    with storage_write_lock():
        target = FILES_PATH / doc.path
        if not target.exists():
            raise HTTPException(status_code=404, detail="File missing from repository")
    note = f"Downloaded {version_label}" if version_label else "Downloaded current version"
    record_event(
        doc,
        user,
        "download",
        note,
        db,
        meta=meta,
        result=version_commit,
    )
    db.commit()
    return FileResponse(target, filename=target.name, media_type="application/octet-stream")


@router.get("/documents/{doc_id}/checkout")
def checkout(doc_id: int, request: Request, db: Session = Depends(get_db)) -> FileResponse:
    user = current_user(request)
    meta = client_meta(request)
    doc = get_document_or_404(doc_id, db)
    ensure_not_archived(doc)
    lock = (
        db.execute(
            select(DocumentLock).where(
                DocumentLock.document_id == doc.id,
                DocumentLock.is_active == True,  # noqa: E712
            ),
        )
        .scalars()
        .first()
    )
    if lock and lock.locked_by != user["id"]:
        raise HTTPException(
            status_code=409,
            detail=f"Document is checked out by {lock.locked_by_name or lock.locked_by}",
        )
    if not lock:
        new_lock = DocumentLock(
            document_id=doc.id,
            locked_by=user["id"],
            locked_by_name=user["name"],
            locked_at=dt.datetime.now(tz=dt.UTC),
            is_active=True,
            locked_ip=meta.get("ip"),
            locked_user_agent=meta.get("user_agent"),
            force_acquired=False,
        )
        db.add(new_lock)
        db.commit()
    with storage_write_lock():
        target = FILES_PATH / doc.path
        if not target.exists():
            raise HTTPException(status_code=404, detail="File missing from repository")
    record_event(doc, user, "checkout", f"Checked out by {user['name']}", db, meta=meta)
    db.commit()
    return FileResponse(target, filename=target.name, media_type="application/octet-stream")


@router.post("/documents/{doc_id}/release")
async def release_lock(
    doc_id: int,
    request: Request,
    redirect_to: str | None = None,
    db: Session = Depends(get_db),
) -> Response:
    meta = client_meta(request)
    # Manually parse optional form data to avoid multipart boundary errors when body is empty.
    if redirect_to is None:
        try:
            form = await request.form()
            raw_redirect = form.get("redirect_to")
            redirect_to = raw_redirect if isinstance(raw_redirect, str) else None
        except Exception:
            redirect_to = None

    doc = get_document_or_404(doc_id, db)
    lock = (
        db.execute(
            select(DocumentLock).where(
                DocumentLock.document_id == doc_id,
                DocumentLock.is_active == True,  # noqa: E712
            ),
        )
        .scalars()
        .first()
    )
    user = current_user(request)
    if not lock:
        if request.query_params.get("mode") == "json":
            return JSONResponse({"ok": True, "message": "No active lock"})
        return RedirectResponse(url=safe_redirect(redirect_to), status_code=303)
    if lock.locked_by != user["id"]:
        raise HTTPException(status_code=403, detail="Lock held by another user")
    lock.is_active = False
    record_event(doc, user, "release", f"Lock released by {user['name']}", db, meta=meta)
    db.commit()
    if request.query_params.get("mode") == "json":
        return JSONResponse({"ok": True})
    return RedirectResponse(url=safe_redirect(redirect_to), status_code=303)


@router.post("/documents/{doc_id}/checkin", response_class=RedirectResponse)
async def checkin(
    doc_id: int,
    request: Request,
    file: UploadFile = File(...),
    note: str | None = Form(None),
    redirect_to: str | None = Form(None),
    db: Session = Depends(get_db),
) -> RedirectResponse:
    user = current_user(request)
    meta = client_meta(request)
    doc = get_document_or_404(doc_id, db)
    ensure_not_archived(doc)
    lock = (
        db.execute(
            select(DocumentLock).where(
                DocumentLock.document_id == doc.id,
                DocumentLock.is_active == True,  # noqa: E712
            ),
        )
        .scalars()
        .first()
    )
    if not lock or lock.locked_by != user["id"]:
        raise HTTPException(status_code=403, detail="You do not hold the lock for this document")

    data = await file.read()
    version_id = new_version_id()
    now = dt.datetime.now(tz=dt.UTC)
    with storage_write_lock():
        ensure_storage()
        target = safe_path(doc.path)
        working_stage = stage_write(target, data)
        version_path = version_file_path(doc, version_id)
        version_stage = stage_write(version_path, data)

        checksum = hashlib.sha256(data).hexdigest()
        version_number = next_version_number(doc.id, db)
        mime_type = file.content_type or mimetypes.guess_type(file.filename or doc.path)[0]
        message = f"{user['name']} uploaded a new version"
        if note:
            message = f"{message}: {note}"

        doc.latest_commit = version_id
        doc.latest_modified_by = user["id"]
        doc.latest_modified_at = now
        doc.latest_version_number = version_number
        doc.version_count = max(doc.version_count or 0, version_number)
        version = DocumentVersion(
            document_id=doc.id,
            commit_hash=version_id,
            version_number=version_number,
            committed_by=user["id"],
            committed_by_name=user["name"],
            message=message,
            checksum=checksum,
            hash_algo="sha256",
            size_bytes=len(data),
            mime_type=mime_type,
            original_filename=Path(file.filename or doc.path).name,
            upload_ip=meta.get("ip"),
            upload_user_agent=meta.get("user_agent"),
            created_via="checkin",
        )
        db.add(version)
        lock.is_active = False
        record_event(doc, user, "checkin", message, db, meta=meta)
        try:
            db.commit()
        except Exception:
            working_stage.rollback()
            version_stage.rollback()
            db.rollback()
            raise
        else:
            working_stage.finalize()
            version_stage.finalize()
    return RedirectResponse(url=safe_redirect(redirect_to), status_code=303)


@router.get("/documents/{doc_id}/versions/{version_id}/download")
def download_version(
    doc_id: int,
    version_id: str,
    request: Request,
    db: Session = Depends(get_db),
) -> FileResponse:
    from pathlib import Path

    user = current_user(request)
    meta = client_meta(request)
    doc = get_document_or_404(doc_id, db)
    version = (
        db.execute(
            select(DocumentVersion).where(
                DocumentVersion.document_id == doc.id,
                DocumentVersion.commit_hash == version_id,
            ),
        )
        .scalars()
        .first()
    )
    if not version:
        raise HTTPException(status_code=404, detail="Version not found")

    with storage_write_lock():
        target = version_file_path(doc, version_id)
        if not target.exists():
            raise HTTPException(status_code=404, detail="Version file missing from repository")

    download_name = Path(doc.path).name
    suffix = target.suffix
    if suffix and not download_name.endswith(suffix):
        download_name = f"{Path(download_name).stem}{suffix}"
    version_label = (
        f"v{version.version_number}"
        if version.version_number is not None
        else f"commit {version.commit_hash}"
    )
    note = f"Downloaded {version_label}"
    record_event(
        doc,
        user,
        "download",
        note,
        db,
        meta=meta,
        result=version.commit_hash,
    )
    db.commit()
    return FileResponse(target, filename=download_name, media_type="application/octet-stream")


@router.post("/documents/{doc_id}/move", response_class=JSONResponse)
def move_document(
    doc_id: int,
    request: Request,
    new_path: str = Form(...),
    db: Session = Depends(get_db),
) -> JSONResponse:
    user = current_user(request)
    meta = client_meta(request)
    doc = get_document_or_404(doc_id, db)
    ensure_not_locked_by_other(doc, user, db)

    cleaned = new_path.strip().lstrip("/").replace("\\", "/")
    if not cleaned:
        raise HTTPException(status_code=400, detail="New path is required")
    if is_archived_path(doc.path) and not is_archived_path(cleaned):
        raise HTTPException(
            status_code=400,
            detail="Unarchive the document before moving it out of the archive",
        )
    if not is_archived_path(doc.path) and is_archived_path(cleaned):
        raise HTTPException(
            status_code=400,
            detail="Use the archive action instead of moving into the archive",
        )

    target = safe_path(cleaned)

    original_path = doc.path
    with storage_write_lock():
        source = safe_path(doc.path)
        if not source.exists():
            raise HTTPException(status_code=404, detail="Source file missing")
        if target.exists():
            raise HTTPException(status_code=400, detail="A document already exists at that path")
        data = source.read_bytes()
        checksum_value = hashlib.sha256(data).hexdigest()
        latest_checksum = latest_version_checksum(doc, db)
        move_stage = stage_move(source, target)
        version_id = None
        try:
            doc.path = str(target.relative_to(FILES_PATH))
            if not latest_checksum or latest_checksum != checksum_value:
                version_id = snapshot_version(
                    doc,
                    data,
                    user,
                    f"Moved from {original_path} to {doc.path}",
                    db,
                    update_latest=True,
                    locked=True,
                    meta=meta,
                    filename=Path(doc.path).name,
                    created_via="move",
                    checksum=checksum_value,
                )
            record_event(
                doc,
                user,
                "move",
                f"Moved from {original_path} to {doc.path}",
                db,
                meta=meta,
            )
            db.commit()
        except Exception:
            doc.path = original_path
            move_stage.rollback()
            if version_id:
                cleanup_version_file(doc, version_id)
            db.rollback()
            raise
        else:
            move_stage.finalize()

    return JSONResponse({"ok": True, "path": doc.path})


@router.post("/documents/{doc_id}/archive", response_class=JSONResponse)
def archive_endpoint(doc_id: int, request: Request, db: Session = Depends(get_db)) -> JSONResponse:
    user = current_user(request)
    doc = get_document_or_404(doc_id, db)
    meta = client_meta(request)
    stage, version_id = archive_document(doc, user, db, meta=meta)
    try:
        db.commit()
    except Exception:
        stage.rollback()
        if version_id:
            cleanup_version_file(doc, version_id)
        db.rollback()
        raise
    else:
        with storage_write_lock():
            stage.finalize()
    return JSONResponse({"ok": True, "path": doc.path})


@router.post("/documents/{doc_id}/unarchive", response_class=JSONResponse)
def unarchive_endpoint(
    doc_id: int,
    request: Request,
    db: Session = Depends(get_db),
) -> JSONResponse:
    user = current_user(request)
    doc = get_document_or_404(doc_id, db)
    meta = client_meta(request)
    stage, version_id = unarchive_document(doc, user, db, meta=meta)
    try:
        db.commit()
    except Exception:
        stage.rollback()
        if version_id:
            cleanup_version_file(doc, version_id)
        db.rollback()
        raise
    else:
        with storage_write_lock():
            stage.finalize()
    return JSONResponse({"ok": True, "path": doc.path})


@router.post("/documents/{doc_id}/delete", response_class=JSONResponse)
def delete_document_alias(
    doc_id: int,
    request: Request,
    db: Session = Depends(get_db),
) -> JSONResponse:
    # Backwards-compatible alias that now archives instead of deleting.
    return archive_endpoint(doc_id, request, db)


@router.post("/documents/{doc_id}/permanent_delete", response_class=JSONResponse)
def permanent_delete(doc_id: int, request: Request, db: Session = Depends(get_db)) -> JSONResponse:
    user = current_user(request)
    if not user.get("is_admin"):
        raise HTTPException(status_code=403, detail="Admin role required to permanently delete")
    doc = get_document_or_404(doc_id, db)
    if not is_archived_path(doc.path):
        raise HTTPException(
            status_code=400,
            detail="Archive the document before permanent deletion",
        )
    lock = ensure_not_locked_by_other(doc, user, db)
    if lock:
        lock.is_active = False

    path_backup = None
    versions_backup = None
    path = safe_path(doc.path)
    versions_path = version_dir(doc.id)
    with storage_write_lock():
        if path.exists():
            path_backup = path.with_name(f"{path.name}.bak-{uuid.uuid4().hex}")
            path.replace(path_backup)
        if versions_path.exists():
            versions_backup = versions_path.with_name(
                f"{versions_path.name}.bak-{uuid.uuid4().hex}",
            )
            versions_path.replace(versions_backup)

    try:
        db.query(DocumentLock).filter(DocumentLock.document_id == doc.id).delete()
        db.query(DocumentVersion).filter(DocumentVersion.document_id == doc.id).delete()
        db.delete(doc)
        db.commit()
    except Exception:
        with storage_write_lock():
            if path_backup and Path(path_backup).exists():
                if path.exists():
                    path.unlink()
                Path(path_backup).replace(path)
            if versions_backup and Path(versions_backup).exists():
                if versions_path.exists():
                    shutil.rmtree(versions_path, ignore_errors=True)
                Path(versions_backup).replace(versions_path)
        db.rollback()
        raise
    else:
        with storage_write_lock():
            if path_backup and Path(path_backup).exists():
                Path(path_backup).unlink()
            if versions_backup and Path(versions_backup).exists():
                shutil.rmtree(versions_backup, ignore_errors=True)
            prune_empty_archived_parents(path.parent)
    return JSONResponse({"ok": True, "deleted": True})


@router.get("/documents/{doc_id}", response_class=HTMLResponse)
def document_detail(doc_id: int, request: Request, db: Session = Depends(get_db)) -> HTMLResponse:
    doc = get_document_or_404(doc_id, db)
    lock = (
        db.execute(
            select(DocumentLock).where(
                DocumentLock.document_id == doc.id,
                DocumentLock.is_active == True,  # noqa: E712
            ),
        )
        .scalars()
        .first()
    )
    versions = (
        db.execute(
            select(DocumentVersion)
            .where(DocumentVersion.document_id == doc.id)
            .order_by(DocumentVersion.committed_at.desc()),
        )
        .scalars()
        .all()
    )
    filtered_versions = dedupe_versions_by_checksum(versions)
    events = (
        db.execute(
            select(DocumentEvent)
            .where(DocumentEvent.document_id == doc.id)
            .order_by(DocumentEvent.created_at.desc()),
        )
        .scalars()
        .all()
    )

    history_items = []
    version_signatures = {version_signature(v) for v in filtered_versions}
    for version in filtered_versions:
        history_items.append(
            {
                "id": version.commit_hash,
                "type": "version",
                "timestamp": version.committed_at.isoformat() if version.committed_at else None,
                "display": version.committed_at.strftime("%Y-%m-%d %H:%M")
                if version.committed_at
                else "Version",
                "by": version.committed_by_name or version.committed_by,
                "note": version.message,
                "version_number": version.version_number,
                "created_via": version.created_via,
                "original_filename": version.original_filename,
                "download_url": f"/documents/{doc.id}/versions/{version.commit_hash}/download",
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
                "display": event.created_at.strftime("%Y-%m-%d %H:%M")
                if event.created_at
                else event.event_type.title(),
                "by": event.actor_name or event.actor,
                "note": event.message,
                "download_url": None,
            },
        )
    history_items.sort(key=lambda item: item["timestamp"] or "", reverse=True)

    folder_path = "/".join(Path(doc.path).parent.parts)

    return templates.TemplateResponse(
        "document.html",
        {
            "request": request,
            "doc": doc,
            "lock": lock,
            "history": history_items,
            "folder_path": folder_path,
            "user": current_user(request),
            "base_domain": BASE_DOMAIN,
        },
    )
