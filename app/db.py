"""Database setup for the vault service."""

from collections.abc import Generator
from pathlib import Path
from typing import Any

from sqlalchemy import (
    ForeignKeyConstraint,
    UniqueConstraint,
    create_engine,
    event,
    inspect,
    select,
)
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
        existing_columns = {
            column["name"]: column for column in inspector.get_columns(table.name)
        }
        expected_columns = {column.name: column for column in table.columns}
        if not set(expected_columns).issubset(existing_columns):
            return True
        for column_name, expected_column in expected_columns.items():
            existing_column = existing_columns[column_name]
            if bool(existing_column.get("nullable")) != bool(expected_column.nullable):
                return True
        existing_indexes = {index["name"] for index in inspector.get_indexes(table.name)}
        expected_indexes = {index.name for index in table.indexes if index.name}
        if not expected_indexes.issubset(existing_indexes):
            return True
        existing_unique_constraints = {
            constraint["name"]
            for constraint in inspector.get_unique_constraints(table.name)
            if constraint.get("name")
        }
        expected_unique_constraints = {
            constraint.name
            for constraint in table.constraints
            if isinstance(constraint, UniqueConstraint) and constraint.name
        }
        if not expected_unique_constraints.issubset(existing_unique_constraints):
            return True
        existing_foreign_keys = set()
        for foreign_key in inspector.get_foreign_keys(table.name):
            referred_table = foreign_key.get("referred_table")
            constrained_columns = foreign_key.get("constrained_columns") or []
            referred_columns = foreign_key.get("referred_columns") or []
            ondelete = ((foreign_key.get("options") or {}).get("ondelete") or "").upper()
            existing_foreign_keys.add(
                tuple(
                    (local_column, referred_table, remote_column, ondelete)
                    for local_column, remote_column in zip(
                        constrained_columns,
                        referred_columns,
                        strict=False,
                    )
                ),
            )
        expected_foreign_keys = {
            tuple(
                (
                    element.parent.name,
                    element.column.table.name,
                    element.column.name,
                    (element.ondelete or "").upper(),
                )
                for element in constraint.elements
            )
            for constraint in table.constraints
            if isinstance(constraint, ForeignKeyConstraint)
        }
        if not expected_foreign_keys.issubset(existing_foreign_keys):
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
