"""Database setup for the vault service."""

from collections.abc import Generator
from pathlib import Path
from typing import Any

from sqlalchemy import create_engine, event, inspect, select
from sqlalchemy.engine import Engine
from sqlalchemy.orm import DeclarativeBase, Session, sessionmaker

from .config import DB_PATH


class Base(DeclarativeBase):
    """SQLAlchemy declarative base with typing support."""


def set_sqlite_pragma(dbapi_connection: Any, _: object) -> None:
    cursor = dbapi_connection.cursor()
    cursor.execute("PRAGMA journal_mode=WAL;")
    cursor.execute("PRAGMA synchronous=NORMAL;")
    cursor.execute("PRAGMA foreign_keys=ON;")
    cursor.execute("PRAGMA busy_timeout=5000;")
    cursor.close()


def create_database_engine(db_path: Path) -> Engine:
    configured_engine = create_engine(
        f"sqlite:///{db_path}",
        connect_args={"check_same_thread": False, "timeout": 30},
        pool_pre_ping=True,
    )
    event.listen(configured_engine, "connect", set_sqlite_pragma)
    return configured_engine


engine = create_database_engine(DB_PATH)
SessionLocal = sessionmaker(bind=engine, autoflush=False, autocommit=False)


def configure_database(db_path: str | Path) -> None:
    """Point the process-local database globals at a new SQLite database."""
    global DB_PATH, engine

    from . import config

    old_engine = engine
    DB_PATH = Path(db_path).resolve()
    config.DB_PATH = DB_PATH

    engine = create_database_engine(DB_PATH)
    SessionLocal.configure(bind=engine)
    old_engine.dispose()


def init_db() -> None:
    Path(DB_PATH).parent.mkdir(parents=True, exist_ok=True)
    # Import models so SQLAlchemy is aware of them before creating tables
    from . import models

    _apply_known_additive_migrations()
    if _schema_needs_reset():
        raise RuntimeError(
            "Database schema is incompatible with this app version. "
            "Startup refused to alter or drop existing metadata automatically."
        )
    Base.metadata.create_all(bind=engine)
    _bootstrap_root_folders(models.Folder)


def _apply_known_additive_migrations() -> None:
    from . import models

    inspector = inspect(engine)
    tables = set(inspector.get_table_names())
    if "vault_users" in tables and "vault_settings" not in tables:
        models.VaultSetting.__table__.create(bind=engine, checkfirst=True)
    if "vault_users" not in tables:
        return
    vault_user_columns = {column["name"] for column in inspector.get_columns("vault_users")}
    if "preferences" not in vault_user_columns:
        with engine.begin() as connection:
            connection.exec_driver_sql(
                "ALTER TABLE vault_users "
                "ADD COLUMN preferences JSON NOT NULL DEFAULT '{}'",
            )


def _schema_needs_reset() -> bool:
    inspector = inspect(engine)
    tables = set(inspector.get_table_names())
    if not tables:
        return False
    for table in Base.metadata.sorted_tables:
        if table.name not in tables:
            return True
        existing_columns = {column["name"] for column in inspector.get_columns(table.name)}
        expected_columns = {column.name for column in table.columns}
        if not expected_columns.issubset(existing_columns):
            return True
    return False


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
