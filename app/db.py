# Copyright (c) 2024 The Allen Family
"""Database setup for the vault service."""

from collections.abc import Generator
from pathlib import Path
from typing import Any

from sqlalchemy import Connection, create_engine, event, inspect
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


@event.listens_for(engine, "begin")
def set_begin_immediate(conn: Connection) -> None:
    # Ensure write transactions grab the lock up front to reduce mid-flight SQLITE_BUSY.
    conn.exec_driver_sql("BEGIN IMMEDIATE")


def init_db() -> None:
    Path(DB_PATH).parent.mkdir(parents=True, exist_ok=True)
    # Import models so SQLAlchemy is aware of them before creating tables
    from . import models  # noqa: F401

    if RESET_DB_ON_START or _schema_needs_reset():
        Base.metadata.drop_all(bind=engine)
    Base.metadata.create_all(bind=engine)


def _schema_needs_reset() -> bool:
    inspector = inspect(engine)
    tables = set(inspector.get_table_names())
    if not tables:
        return False
    if "documents" not in tables:
        return False
    document_columns = {column["name"] for column in inspector.get_columns("documents")}
    return "folder_id" not in document_columns or "current_version_id" not in document_columns


def get_db() -> Generator[Session, None, None]:
    db: Session = SessionLocal()
    try:
        yield db
    finally:
        db.close()
