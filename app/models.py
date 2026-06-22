# Copyright (c) 2024 The Allen Family
"""SQLAlchemy models for the vault service."""

import datetime

from sqlalchemy import Boolean, DateTime, ForeignKey, Integer, String, Text, UniqueConstraint
from sqlalchemy.orm import Mapped, mapped_column, relationship

from .db import Base


def utcnow() -> datetime.datetime:
    return datetime.datetime.now(tz=datetime.UTC)


class Folder(Base):
    __tablename__ = "folders"
    __table_args__ = (UniqueConstraint("parent_id", "name", name="uq_folders_parent_name"),)

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    parent_id: Mapped[int | None] = mapped_column(
        Integer,
        ForeignKey("folders.id", ondelete="CASCADE"),
        nullable=True,
        index=True,
    )
    name: Mapped[str] = mapped_column(String, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)

    parent: Mapped["Folder | None"] = relationship(
        "Folder",
        remote_side=[id],
        back_populates="children",
    )
    children: Mapped[list["Folder"]] = relationship(
        "Folder",
        back_populates="parent",
        cascade="all, delete-orphan",
    )
    documents: Mapped[list["Document"]] = relationship("Document", back_populates="folder")


class StorageObject(Base):
    __tablename__ = "storage_objects"
    __table_args__ = (
        UniqueConstraint("backend", "bucket", "object_key", name="uq_storage_object_location"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    hash_algo: Mapped[str] = mapped_column(String, default="sha256", nullable=False, index=True)
    hash: Mapped[str] = mapped_column(String, nullable=False, index=True)
    size_bytes: Mapped[int] = mapped_column(Integer, nullable=False)
    backend: Mapped[str] = mapped_column(String, nullable=False)
    bucket: Mapped[str] = mapped_column(String, nullable=False, default="")
    object_key: Mapped[str] = mapped_column(String, nullable=False)
    mime_type: Mapped[str | None] = mapped_column(String, nullable=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)

    versions: Mapped[list["DocumentVersion"]] = relationship(
        "DocumentVersion",
        back_populates="storage_object",
    )


class Document(Base):
    __tablename__ = "documents"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    folder_id: Mapped[int | None] = mapped_column(
        Integer,
        ForeignKey("folders.id", ondelete="SET NULL"),
        nullable=True,
        index=True,
    )
    name: Mapped[str] = mapped_column(String, nullable=False)
    description: Mapped[str | None] = mapped_column(Text, nullable=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    created_by: Mapped[str | None] = mapped_column(String, nullable=True)
    created_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    latest_modified_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    latest_modified_by: Mapped[str | None] = mapped_column(String, nullable=True)
    latest_version_number: Mapped[int | None] = mapped_column(Integer, nullable=True)
    version_count: Mapped[int] = mapped_column(Integer, default=0)
    current_version_id: Mapped[str | None] = mapped_column(String, nullable=True)

    folder: Mapped[Folder | None] = relationship("Folder", back_populates="documents")
    locks: Mapped[list["DocumentLock"]] = relationship(
        "DocumentLock",
        back_populates="document",
        cascade="all, delete-orphan",
    )
    versions: Mapped[list["DocumentVersion"]] = relationship(
        "DocumentVersion",
        back_populates="document",
        cascade="all, delete-orphan",
    )
    events: Mapped[list["DocumentEvent"]] = relationship(
        "DocumentEvent",
        back_populates="document",
        cascade="all, delete-orphan",
    )


class DocumentLock(Base):
    __tablename__ = "document_locks"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    document_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("documents.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    locked_by: Mapped[str] = mapped_column(String, nullable=False)
    locked_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    locked_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    is_active: Mapped[bool] = mapped_column(Boolean, default=True)
    locked_ip: Mapped[str | None] = mapped_column(String, nullable=True)
    locked_user_agent: Mapped[str | None] = mapped_column(String, nullable=True)
    force_acquired: Mapped[bool] = mapped_column(Boolean, default=False)

    document: Mapped[Document] = relationship("Document", back_populates="locks")


class DocumentVersion(Base):
    __tablename__ = "document_versions"

    id: Mapped[str] = mapped_column(String, primary_key=True, index=True)
    document_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("documents.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    storage_object_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("storage_objects.id"),
        nullable=False,
        index=True,
    )
    version_number: Mapped[int] = mapped_column(Integer, nullable=False)
    committed_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    committed_by: Mapped[str] = mapped_column(String, nullable=False)
    committed_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    message: Mapped[str | None] = mapped_column(Text, nullable=True)
    checksum: Mapped[str | None] = mapped_column(String, nullable=True)
    hash_algo: Mapped[str] = mapped_column(String, default="sha256", nullable=False)
    size_bytes: Mapped[int | None] = mapped_column(Integer, nullable=True)
    mime_type: Mapped[str | None] = mapped_column(String, nullable=True)
    original_filename: Mapped[str | None] = mapped_column(String, nullable=True)
    upload_ip: Mapped[str | None] = mapped_column(String, nullable=True)
    upload_user_agent: Mapped[str | None] = mapped_column(String, nullable=True)
    created_via: Mapped[str | None] = mapped_column(String, nullable=True)

    document: Mapped[Document] = relationship("Document", back_populates="versions")
    storage_object: Mapped[StorageObject] = relationship(
        "StorageObject",
        back_populates="versions",
    )


class DocumentEvent(Base):
    __tablename__ = "document_events"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    document_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("documents.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    event_type: Mapped[str] = mapped_column(String, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    actor: Mapped[str] = mapped_column(String, nullable=False)
    actor_name: Mapped[str | None] = mapped_column(String, nullable=True)
    message: Mapped[str | None] = mapped_column(Text, nullable=True)
    result: Mapped[str | None] = mapped_column(String, nullable=True)
    ip: Mapped[str | None] = mapped_column(String, nullable=True)
    user_agent: Mapped[str | None] = mapped_column(String, nullable=True)

    document: Mapped[Document] = relationship("Document", back_populates="events")
