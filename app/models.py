# Copyright (c) 2024 The Allen Family
"""SQLAlchemy models for the vault service."""

import datetime

from sqlalchemy import Boolean, DateTime, ForeignKey, Integer, String, Text
from sqlalchemy.orm import Mapped, mapped_column, relationship

from .db import Base


class Document(Base):
    __tablename__ = "documents"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    path: Mapped[str] = mapped_column(String, unique=True, nullable=False)
    display_name: Mapped[str | None] = mapped_column(String, nullable=True)
    description: Mapped[str | None] = mapped_column(Text, nullable=True)
    created_at: Mapped[datetime.datetime] = mapped_column(
        DateTime,
        default=lambda: datetime.datetime.now(tz=datetime.UTC),
    )
    created_by: Mapped[str | None] = mapped_column(String, nullable=True)
    created_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    latest_commit: Mapped[str | None] = mapped_column(String, nullable=True)
    latest_modified_at: Mapped[datetime.datetime] = mapped_column(
        DateTime,
        default=lambda: datetime.datetime.now(tz=datetime.UTC),
    )
    latest_modified_by: Mapped[str | None] = mapped_column(String, nullable=True)
    latest_version_number: Mapped[int | None] = mapped_column(Integer, nullable=True)
    version_count: Mapped[int] = mapped_column(Integer, default=0)

    locks: Mapped[list["DocumentLock"]] = relationship("DocumentLock", back_populates="document")
    versions: Mapped[list["DocumentVersion"]] = relationship(
        "DocumentVersion",
        back_populates="document",
    )
    events: Mapped[list["DocumentEvent"]] = relationship(
        "DocumentEvent",
        back_populates="document",
        cascade="all, delete-orphan",
    )


class DocumentLock(Base):
    __tablename__ = "document_locks"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    document_id: Mapped[int] = mapped_column(Integer, ForeignKey("documents.id"), nullable=False)
    locked_by: Mapped[str] = mapped_column(String, nullable=False)
    locked_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    locked_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=datetime.datetime.utcnow)
    is_active: Mapped[bool] = mapped_column(Boolean, default=True)
    locked_ip: Mapped[str | None] = mapped_column(String, nullable=True)
    locked_user_agent: Mapped[str | None] = mapped_column(String, nullable=True)
    force_acquired: Mapped[bool] = mapped_column(Boolean, default=False)

    document: Mapped[Document] = relationship("Document", back_populates="locks")


class DocumentVersion(Base):
    __tablename__ = "document_versions"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    document_id: Mapped[int] = mapped_column(Integer, ForeignKey("documents.id"), nullable=False)
    commit_hash: Mapped[str] = mapped_column(String, nullable=False)
    version_number: Mapped[int] = mapped_column(Integer, nullable=False)
    committed_at: Mapped[datetime.datetime] = mapped_column(
        DateTime,
        default=datetime.datetime.utcnow,
    )
    committed_by: Mapped[str] = mapped_column(String, nullable=False)
    committed_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    message: Mapped[str | None] = mapped_column(Text, nullable=True)
    checksum: Mapped[str | None] = mapped_column(String, nullable=True)
    hash_algo: Mapped[str] = mapped_column(String, default="sha256")
    size_bytes: Mapped[int | None] = mapped_column(Integer, nullable=True)
    mime_type: Mapped[str | None] = mapped_column(String, nullable=True)
    original_filename: Mapped[str | None] = mapped_column(String, nullable=True)
    upload_ip: Mapped[str | None] = mapped_column(String, nullable=True)
    upload_user_agent: Mapped[str | None] = mapped_column(String, nullable=True)
    created_via: Mapped[str | None] = mapped_column(String, nullable=True)

    document: Mapped[Document] = relationship("Document", back_populates="versions")


class DocumentEvent(Base):
    __tablename__ = "document_events"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    document_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("documents.id", ondelete="CASCADE"),
        nullable=False,
    )
    event_type: Mapped[str] = mapped_column(String, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(
        DateTime,
        default=datetime.datetime.utcnow,
    )
    actor: Mapped[str] = mapped_column(String, nullable=False)
    actor_name: Mapped[str | None] = mapped_column(String, nullable=True)
    message: Mapped[str | None] = mapped_column(Text, nullable=True)
    result: Mapped[str | None] = mapped_column(String, nullable=True)
    ip: Mapped[str | None] = mapped_column(String, nullable=True)
    user_agent: Mapped[str | None] = mapped_column(String, nullable=True)

    document: Mapped[Document] = relationship("Document", back_populates="events")
