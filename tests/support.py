import os
import tempfile
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

from fastapi.testclient import TestClient
from sqlalchemy.orm import Session

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
    "VAULT_DB_PATH",
    "VAULT_OBJECTS_PATH",
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
    site_name: str
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
        site_name=routers_module.SITE_NAME,
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
        site_name=snapshot.site_name,
        ttl_sweep_interval_seconds=snapshot.ttl_sweep_interval_seconds,
    )
    for key, value in snapshot.env.items():
        if value is None:
            os.environ.pop(key, None)
        else:
            os.environ[key] = value


@contextmanager
def vault_runtime(*, auth_mode: str = "headers") -> Iterator[VaultRuntimeContext]:
    snapshot = snapshot_runtime()
    with tempfile.TemporaryDirectory(prefix="vault-test-client-") as temp_dir_name:
        temp_dir = Path(temp_dir_name)
        db_path = temp_dir / "vault.db"
        objects_path = temp_dir / "objects"
        os.environ.update(
            {
                "BASE_DOMAIN": "localhost",
                "VAULT_AUTH_MODE": auth_mode,
                "VAULT_DEV_AUTH": "1" if auth_mode == "dev" else "0",
                "VAULT_DB_PATH": str(db_path),
                "VAULT_OBJECTS_PATH": str(objects_path),
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


@contextmanager
def vault_test_client(*, auth_mode: str = "headers") -> Iterator[VaultTestContext]:
    with vault_runtime(auth_mode=auth_mode) as runtime:
        try:
            app = create_app(enable_ttl_sweeper=False)
            with TestClient(app) as client:
                yield VaultTestContext(client=client, temp_dir=runtime.temp_dir)
        finally:
            pass
