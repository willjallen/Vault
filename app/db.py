"""Database setup for the vault service."""

from collections.abc import Generator
from pathlib import Path
from typing import Any

from sqlalchemy import MetaData, create_engine, event, inspect, select
from sqlalchemy.engine import Engine
from sqlalchemy.orm import DeclarativeBase, Session, sessionmaker

from .config import DB_PATH, RESET_DB_ON_START


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


def configure_database(db_path: str | Path, *, reset_on_start: bool = False) -> None:
    """Point the process-local database globals at a new SQLite database."""
    global DB_PATH, RESET_DB_ON_START, engine

    from . import config

    old_engine = engine
    DB_PATH = Path(db_path).resolve()
    RESET_DB_ON_START = reset_on_start
    config.DB_PATH = DB_PATH
    config.RESET_DB_ON_START = RESET_DB_ON_START

    engine = create_database_engine(DB_PATH)
    SessionLocal.configure(bind=engine)
    old_engine.dispose()


def init_db() -> None:
    Path(DB_PATH).parent.mkdir(parents=True, exist_ok=True)
    # Import models so SQLAlchemy is aware of them before creating tables
    from . import models

    if RESET_DB_ON_START:
        _drop_existing_schema()
    elif _schema_needs_reset():
        raise RuntimeError(
            "Database schema is incompatible with this app version. "
            "Refusing to reset metadata automatically; migrate or back up the database before "
            "setting VAULT_RESET_DB_ON_START=1."
        )
    Base.metadata.create_all(bind=engine)
    _bootstrap_root_folders(models.Folder)


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
