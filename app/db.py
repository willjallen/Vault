# Copyright (c) 2024 The Allen Family
"""Database setup for the vault service."""

from collections.abc import Generator
from pathlib import Path
from typing import Any

from sqlalchemy import MetaData, create_engine, event, inspect, select
from sqlalchemy.orm import DeclarativeBase, Session, sessionmaker

from .config import DB_PATH, RESET_DB_ON_START


class Base(DeclarativeBase):
    """SQLAlchemy declarative base with typing support."""


engine = create_engine(
    f"sqlite:///{DB_PATH}",
    connect_args={"check_same_thread": False, "timeout": 30},
    pool_pre_ping=True,
)
SessionLocal = sessionmaker(bind=engine, autoflush=False, autocommit=False)


@event.listens_for(engine, "connect")
def set_sqlite_pragma(dbapi_connection: Any, _: object) -> None:
    cursor = dbapi_connection.cursor()
    cursor.execute("PRAGMA journal_mode=WAL;")
    cursor.execute("PRAGMA synchronous=NORMAL;")
    cursor.execute("PRAGMA foreign_keys=ON;")
    cursor.execute("PRAGMA busy_timeout=5000;")
    cursor.close()


def init_db() -> None:
    Path(DB_PATH).parent.mkdir(parents=True, exist_ok=True)
    # Import models so SQLAlchemy is aware of them before creating tables
    from . import models

    if RESET_DB_ON_START or _schema_needs_reset():
        _drop_existing_schema()
    Base.metadata.create_all(bind=engine)
    _bootstrap_root_folders(models.Folder)


def _schema_needs_reset() -> bool:
    inspector = inspect(engine)
    tables = set(inspector.get_table_names())
    if not tables:
        return False
    required_tables = {
        "folders",
        "documents",
        "document_versions",
        "document_locks",
        "document_events",
        "state_events",
        "blobs",
        "blob_locations",
        "vault_users",
        "vault_groups",
        "vault_group_memberships",
    }
    if not required_tables.issubset(tables):
        return True
    folder_columns = {column["name"] for column in inspector.get_columns("folders")}
    document_columns = {column["name"] for column in inspector.get_columns("documents")}
    version_columns = {column["name"] for column in inspector.get_columns("document_versions")}
    return (
        "root_key" not in folder_columns
        or "is_root" not in folder_columns
        or "folder_id" not in document_columns
        or "blob_id" not in version_columns
    )


def _drop_existing_schema() -> None:
    metadata = MetaData()
    metadata.reflect(bind=engine)
    metadata.drop_all(bind=engine)


def _bootstrap_root_folders(folder_model: type[Any]) -> None:
    db = SessionLocal()
    try:
        for root_key, name in (("vault", "Vault"), ("archive", "Archive")):
            existing = (
                db.execute(
                    select(folder_model).where(
                        folder_model.root_key == root_key,
                        folder_model.is_root == True,  # noqa: E712
                    ),
                )
                .scalars()
                .first()
            )
            if existing:
                continue
            db.add(
                folder_model(
                    root_key=root_key,
                    parent_id=None,
                    name=name,
                    is_root=True,
                ),
            )
        db.commit()
    finally:
        db.close()


def get_db() -> Generator[Session, None, None]:
    db: Session = SessionLocal()
    try:
        yield db
    finally:
        db.close()
