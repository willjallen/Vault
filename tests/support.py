import asyncio
import hashlib
import os
import tempfile
import time
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

from fastapi import Response
from fastapi.testclient import TestClient
from sqlalchemy.orm import Session
from starlette.testclient import TestClient as StarletteTestClient

import app.auth as auth_module
import app.db as db_module
import app.routers as routers_module
import app.storage as storage_module
from app.main import create_app
from app.models import Document, Folder, FolderPermission, VaultGroup
from app.routers import create_document_version, get_or_create_blob_for_data, now_utc
from app.storage import ensure_storage

ENV_KEYS = (
    "BASE_DOMAIN",
    "VAULT_AUTH_MODE",
    "VAULT_DEV_AUTH",
    "VAULT_DEV_MODE",
    "VAULT_DB_PATH",
    "VAULT_OBJECTS_PATH",
    "VAULT_MAX_UPLOAD_BYTES",
    "VAULT_TRANSFER_CHUNK_BYTES",
    "VAULT_TRANSFER_SESSION_TTL_SECONDS",
    "VAULT_EXPORT_TTL_SECONDS",
    "VAULT_TRANSFERS_PATH",
    "VAULT_STORAGE_BACKEND",
)


@dataclass(frozen=True)
class RuntimeSnapshot:
    db_path: Path
    storage_backend: str
    storage_prefix: str
    objects_path: Path
    s3_bucket: str
    s3_region: str
    s3_endpoint_url: str | None
    s3_access_key_id: str
    s3_secret_access_key: str
    s3_session_token: str
    r2_bucket: str
    r2_endpoint_url: str | None
    r2_access_key_id: str
    r2_secret_access_key: str
    auth_mode: str
    admin_groups: set[str]
    bootstrap_admin_emails: set[str]
    session_secret: str
    session_cookie_name: str
    session_max_age_seconds: int
    base_domain: str
    dev_mode: bool
    export_ttl_seconds: int
    max_upload_bytes: int
    site_name: str
    transfer_chunk_bytes: int
    transfer_session_ttl_seconds: int
    transfers_path: Path
    ttl_sweep_interval_seconds: int
    env: dict[str, str | None]


@dataclass(frozen=True)
class VaultTestContext:
    client: TestClient
    temp_dir: Path

    @contextmanager
    def db(self) -> Iterator[Session]:
        session = db_module.SessionLocal()
        try:
            yield session
        finally:
            session.close()


@dataclass(frozen=True)
class VaultRuntimeContext:
    temp_dir: Path

    @contextmanager
    def db(self) -> Iterator[Session]:
        session = db_module.SessionLocal()
        try:
            yield session
        finally:
            session.close()


class FakeClient:
    host = "testclient"


class FakeRequest:
    headers: dict[str, str] = {}
    client = FakeClient()


FAKE_REQUEST = FakeRequest()
SYSTEM_META = {"ip": None, "user_agent": None}


def auth_headers(user: str, groups: list[str] | tuple[str, ...]) -> dict[str, str]:
    return {
        "Remote-User": user,
        "Remote-Name": user.title(),
        "Remote-Email": f"{user}@example.com",
        "Remote-Groups": ",".join(groups),
    }


def user_context(
    user_id: str = "alice",
    *,
    name: str | None = None,
    email: str | None = None,
    groups: list[str] | None = None,
    is_admin: bool = True,
) -> dict[str, object]:
    return {
        "id": user_id,
        "vault_user_id": 0,
        "issuer": "test",
        "subject": user_id,
        "name": name or user_id.title(),
        "email": email or f"{user_id}@example.com",
        "groups": groups or ["vault-users"],
        "is_admin": is_admin,
    }


def add_permission(
    db: Session,
    folder: Folder,
    group: VaultGroup,
    *,
    view: bool = True,
    read: bool = True,
    write: bool = False,
) -> None:
    db.add(
        FolderPermission(
            folder_id=folder.id,
            group_id=group.id,
            can_view=view,
            can_read=read,
            can_write=write,
        ),
    )


def create_versioned_document(
    db: Session,
    folder: Folder,
    *,
    name: str = "plan.txt",
    data: bytes = b"v1",
    actor: dict[str, object] | None = None,
    content_type: str = "text/plain",
    committed_at=None,
) -> Document:
    user = actor or user_context()
    blob = get_or_create_blob_for_data(db, data, content_type)
    doc = Document(
        folder_id=folder.id,
        name=name,
        created_by=str(user["id"]),
        created_by_name=str(user["name"]),
        latest_modified_by=str(user["id"]),
        latest_modified_at=committed_at or now_utc(),
    )
    db.add(doc)
    db.flush()
    version = create_document_version(
        db,
        doc,
        blob,
        user,
        SYSTEM_META,
        name,
        content_type,
        f"Uploaded {name}",
        "upload",
    )
    if committed_at is not None:
        version.committed_at = committed_at
        doc.latest_modified_at = committed_at
    return doc


def snapshot_runtime() -> RuntimeSnapshot:
    return RuntimeSnapshot(
        db_path=db_module.DB_PATH,
        storage_backend=storage_module.STORAGE_BACKEND,
        storage_prefix=storage_module.STORAGE_PREFIX,
        objects_path=storage_module.OBJECTS_PATH,
        s3_bucket=storage_module.S3_BUCKET,
        s3_region=storage_module.S3_REGION,
        s3_endpoint_url=storage_module.S3_ENDPOINT_URL,
        s3_access_key_id=storage_module.S3_ACCESS_KEY_ID,
        s3_secret_access_key=storage_module.S3_SECRET_ACCESS_KEY,
        s3_session_token=storage_module.S3_SESSION_TOKEN,
        r2_bucket=storage_module.R2_BUCKET,
        r2_endpoint_url=storage_module.R2_ENDPOINT_URL,
        r2_access_key_id=storage_module.R2_ACCESS_KEY_ID,
        r2_secret_access_key=storage_module.R2_SECRET_ACCESS_KEY,
        auth_mode=auth_module.AUTH_MODE,
        admin_groups=set(auth_module.ADMIN_GROUPS),
        bootstrap_admin_emails=set(auth_module.BOOTSTRAP_ADMIN_EMAILS),
        session_secret=auth_module.SESSION_SECRET,
        session_cookie_name=auth_module.SESSION_COOKIE_NAME,
        session_max_age_seconds=auth_module.SESSION_MAX_AGE_SECONDS,
        base_domain=routers_module.BASE_DOMAIN,
        dev_mode=routers_module.DEV_MODE,
        export_ttl_seconds=routers_module.EXPORT_TTL_SECONDS,
        max_upload_bytes=routers_module.MAX_UPLOAD_BYTES,
        site_name=routers_module.SITE_NAME,
        transfer_chunk_bytes=routers_module.TRANSFER_CHUNK_BYTES,
        transfer_session_ttl_seconds=routers_module.TRANSFER_SESSION_TTL_SECONDS,
        transfers_path=routers_module.TRANSFERS_PATH,
        ttl_sweep_interval_seconds=routers_module.TTL_SWEEP_INTERVAL_SECONDS,
        env={key: os.environ.get(key) for key in ENV_KEYS},
    )


def restore_runtime(snapshot: RuntimeSnapshot) -> None:
    db_module.configure_database(snapshot.db_path)
    storage_module.configure_storage(
        backend=snapshot.storage_backend,
        objects_path=snapshot.objects_path,
        prefix=snapshot.storage_prefix,
        s3_bucket=snapshot.s3_bucket,
        s3_region=snapshot.s3_region,
        s3_endpoint_url=snapshot.s3_endpoint_url,
        s3_access_key_id=snapshot.s3_access_key_id,
        s3_secret_access_key=snapshot.s3_secret_access_key,
        s3_session_token=snapshot.s3_session_token,
        r2_bucket=snapshot.r2_bucket,
        r2_endpoint_url=snapshot.r2_endpoint_url,
        r2_access_key_id=snapshot.r2_access_key_id,
        r2_secret_access_key=snapshot.r2_secret_access_key,
    )
    auth_module.configure_auth(
        auth_mode=snapshot.auth_mode,
        admin_groups=snapshot.admin_groups,
        bootstrap_admin_emails=snapshot.bootstrap_admin_emails,
        session_secret=snapshot.session_secret,
        session_cookie_name=snapshot.session_cookie_name,
        session_max_age_seconds=snapshot.session_max_age_seconds,
    )
    routers_module.configure_router_runtime(
        auth_mode=snapshot.auth_mode,
        base_domain=snapshot.base_domain,
        dev_mode=snapshot.dev_mode,
        export_ttl_seconds=snapshot.export_ttl_seconds,
        max_upload_bytes=snapshot.max_upload_bytes,
        site_name=snapshot.site_name,
        transfer_chunk_bytes=snapshot.transfer_chunk_bytes,
        transfer_session_ttl_seconds=snapshot.transfer_session_ttl_seconds,
        transfers_path=snapshot.transfers_path,
        ttl_sweep_interval_seconds=snapshot.ttl_sweep_interval_seconds,
    )
    for key, value in snapshot.env.items():
        if value is None:
            os.environ.pop(key, None)
        else:
            os.environ[key] = value


@contextmanager
def vault_runtime(
    *,
    auth_mode: str = "headers",
    dev_mode: bool | None = None,
) -> Iterator[VaultRuntimeContext]:
    snapshot = snapshot_runtime()
    runtime_dev_mode = auth_mode == "dev" if dev_mode is None else dev_mode
    with tempfile.TemporaryDirectory(prefix="vault-test-client-") as temp_dir_name:
        temp_dir = Path(temp_dir_name)
        db_path = temp_dir / "vault.db"
        objects_path = temp_dir / "objects"
        transfers_path = temp_dir / "transfers"
        os.environ.update(
            {
                "BASE_DOMAIN": "localhost",
                "VAULT_AUTH_MODE": auth_mode,
                "VAULT_DEV_AUTH": "1" if auth_mode == "dev" else "0",
                "VAULT_DEV_MODE": "1" if runtime_dev_mode else "0",
                "VAULT_DB_PATH": str(db_path),
                "VAULT_OBJECTS_PATH": str(objects_path),
                "VAULT_TRANSFERS_PATH": str(transfers_path),
                "VAULT_STORAGE_BACKEND": "local",
            },
        )
        db_module.configure_database(db_path)
        storage_module.configure_storage(backend="local", objects_path=objects_path)
        auth_module.configure_auth(
            auth_mode=auth_mode,
            admin_groups={"admin", "vault-admin"},
            bootstrap_admin_emails=set(),
            session_secret="test-session-secret",  # noqa: S106 - fixed test-only signing key
        )
        routers_module.configure_router_runtime(
            auth_mode=auth_mode,
            base_domain="localhost",
            dev_mode=runtime_dev_mode,
            export_ttl_seconds=86400,
            max_upload_bytes=5 * 1024 * 1024 * 1024,
            transfer_chunk_bytes=4,
            transfer_session_ttl_seconds=86400,
            transfers_path=transfers_path,
            site_name="Vault",
            ttl_sweep_interval_seconds=10,
        )
        try:
            db_module.init_db()
            ensure_storage()
            yield VaultRuntimeContext(temp_dir=temp_dir)
        finally:
            db_module.engine.dispose()
            restore_runtime(snapshot)


async def read_response_body(response: Response) -> bytes:
    body = getattr(response, "body", None)
    if body is not None:
        return body
    chunks: list[bytes] = []
    async for chunk in response.body_iterator:
        chunks.append(chunk.encode() if isinstance(chunk, str) else chunk)
    return b"".join(chunks)


def collect_response_body(response: Response) -> bytes:
    return asyncio.run(read_response_body(response))


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def upload_file_via_session(
    client: StarletteTestClient,
    *,
    headers: dict[str, str] | None = None,
    filename: str = "upload.txt",
    data: bytes = b"upload",
    content_type: str = "text/plain",
    folder: str = "",
    mode: str = "create",
    document_id: int | None = None,
    note: str = "",
    rename_to_upload: bool = False,
) -> object:
    session_response = client.post(
        "/api/uploads",
        json={
            "document_id": document_id,
            "filename": filename,
            "folder": folder,
            "mime_type": content_type,
            "mode": mode,
            "note": note,
            "rename_to_upload": rename_to_upload,
            "size_bytes": len(data),
        },
        headers=headers or {},
    )
    if session_response.status_code >= 400:
        return session_response
    session = session_response.json()
    chunk_size = int(session["chunk_size"])
    for index, offset in enumerate(range(0, len(data), chunk_size), start=1):
        chunk = data[offset : offset + chunk_size]
        part_response = client.put(
            f"/api/uploads/{session['id']}/parts/{index}",
            content=chunk,
            headers={
                **(headers or {}),
                "Content-Type": "application/octet-stream",
                "X-Upload-Offset": str(offset),
                "X-Upload-Sha256": sha256_hex(chunk),
                "X-Upload-Size": str(len(chunk)),
            },
        )
        if part_response.status_code >= 400:
            return part_response
    return client.post(
        f"/api/uploads/{session['id']}/complete",
        json={"sha256": sha256_hex(data)},
        headers=headers or {},
    )


def wait_for_export(
    client: StarletteTestClient,
    job_id: str,
    *,
    headers: dict[str, str] | None = None,
    timeout_seconds: float = 5.0,
) -> dict[str, object]:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        response = client.get(f"/api/exports/{job_id}", headers=headers or {})
        response.raise_for_status()
        payload = response.json()
        if payload["status"] in {"complete", "failed", "cancelled"}:
            return payload
        time.sleep(0.05)
    raise AssertionError("Export did not finish")


@contextmanager
def vault_test_client(
    *,
    auth_mode: str = "headers",
    dev_mode: bool | None = None,
) -> Iterator[VaultTestContext]:
    with vault_runtime(auth_mode=auth_mode, dev_mode=dev_mode) as runtime:
        try:
            app = create_app(enable_ttl_sweeper=False)
            with TestClient(app) as client:
                yield VaultTestContext(client=client, temp_dir=runtime.temp_dir)
        finally:
            pass
