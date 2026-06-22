# Copyright (c) 2024 The Allen Family
"""SQLAlchemy models for the vault service."""

import datetime

from sqlalchemy import (
    Boolean,
    DateTime,
    ForeignKey,
    Index,
    Integer,
    JSON,
    String,
    Text,
    UniqueConstraint,
    text,
)
from sqlalchemy.orm import Mapped, mapped_column, relationship

from .db import Base


def utcnow() -> datetime.datetime:
    return datetime.datetime.now(tz=datetime.UTC)


class Folder(Base):
    __tablename__ = "folders"
    __table_args__ = (
        Index(
            "uq_folders_root_key",
            "root_key",
            unique=True,
            sqlite_where=text("is_root = 1"),
        ),
        Index(
            "uq_folders_parent_name",
            "parent_id",
            "name",
            unique=True,
            sqlite_where=text("is_root = 0"),
        ),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    root_key: Mapped[str] = mapped_column(String, nullable=False, index=True)
    parent_id: Mapped[int | None] = mapped_column(
        Integer,
        ForeignKey("folders.id", ondelete="CASCADE"),
        nullable=True,
        index=True,
    )
    name: Mapped[str] = mapped_column(String, nullable=False)
    is_root: Mapped[bool] = mapped_column(Boolean, default=False, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    created_by: Mapped[str | None] = mapped_column(String, nullable=True)
    created_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    color: Mapped[str | None] = mapped_column(String, nullable=True)
    icon: Mapped[str | None] = mapped_column(String, nullable=True)

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
    documents: Mapped[list["Document"]] = relationship(
        "Document",
        back_populates="folder",
        cascade="all, delete-orphan",
    )
    events: Mapped[list["FolderEvent"]] = relationship(
        "FolderEvent",
        back_populates="folder",
        cascade="all, delete-orphan",
    )
    permissions: Mapped[list["FolderPermission"]] = relationship(
        "FolderPermission",
        back_populates="folder",
        cascade="all, delete-orphan",
    )


class FolderEvent(Base):
    __tablename__ = "folder_events"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    folder_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("folders.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    event_type: Mapped[str] = mapped_column(String, nullable=False)
    actor: Mapped[str | None] = mapped_column(String, nullable=True)
    actor_name: Mapped[str | None] = mapped_column(String, nullable=True)
    message: Mapped[str | None] = mapped_column(Text, nullable=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)

    folder: Mapped[Folder] = relationship("Folder", back_populates="events")


class FolderPermission(Base):
    __tablename__ = "folder_permissions"
    __table_args__ = (UniqueConstraint("folder_id", "group_id", name="uq_folder_permission_group"),)

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    folder_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("folders.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    group_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("vault_groups.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    can_view: Mapped[bool] = mapped_column(Boolean, default=True, nullable=False)
    can_read: Mapped[bool] = mapped_column(Boolean, default=True, nullable=False)
    can_write: Mapped[bool] = mapped_column(Boolean, default=False, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    updated_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)

    folder: Mapped[Folder] = relationship("Folder", back_populates="permissions")
    group: Mapped["VaultGroup"] = relationship("VaultGroup")


class VaultUser(Base):
    __tablename__ = "vault_users"
    __table_args__ = (
        UniqueConstraint("issuer", "subject", name="uq_vault_users_identity"),
        Index("ix_vault_users_email", "email"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    issuer: Mapped[str] = mapped_column(String, nullable=False)
    subject: Mapped[str] = mapped_column(String, nullable=False)
    email: Mapped[str | None] = mapped_column(String, nullable=True)
    name: Mapped[str] = mapped_column(String, nullable=False)
    is_admin: Mapped[bool] = mapped_column(Boolean, default=False, nullable=False)
    is_active: Mapped[bool] = mapped_column(Boolean, default=True, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    last_login_at: Mapped[datetime.datetime | None] = mapped_column(DateTime, nullable=True)
    last_seen_at: Mapped[datetime.datetime | None] = mapped_column(DateTime, nullable=True)

    memberships: Mapped[list["VaultGroupMembership"]] = relationship(
        "VaultGroupMembership",
        back_populates="user",
        cascade="all, delete-orphan",
    )


class VaultGroup(Base):
    __tablename__ = "vault_groups"
    __table_args__ = (UniqueConstraint("name", name="uq_vault_groups_name"),)

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    name: Mapped[str] = mapped_column(String, nullable=False)
    description: Mapped[str | None] = mapped_column(Text, nullable=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)

    memberships: Mapped[list["VaultGroupMembership"]] = relationship(
        "VaultGroupMembership",
        back_populates="group",
        cascade="all, delete-orphan",
    )


class VaultGroupMembership(Base):
    __tablename__ = "vault_group_memberships"
    __table_args__ = (UniqueConstraint("user_id", "group_id", name="uq_vault_group_membership"),)

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    user_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("vault_users.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    group_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("vault_groups.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)

    user: Mapped[VaultUser] = relationship("VaultUser", back_populates="memberships")
    group: Mapped[VaultGroup] = relationship("VaultGroup", back_populates="memberships")


class Blob(Base):
    __tablename__ = "blobs"
    __table_args__ = (UniqueConstraint("hash_algo", "hash", "size_bytes", name="uq_blob_identity"),)

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    hash_algo: Mapped[str] = mapped_column(String, default="sha256", nullable=False, index=True)
    hash: Mapped[str] = mapped_column(String, nullable=False, index=True)
    size_bytes: Mapped[int] = mapped_column(Integer, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)

    locations: Mapped[list["BlobLocation"]] = relationship(
        "BlobLocation",
        back_populates="blob",
        cascade="all, delete-orphan",
    )
    versions: Mapped[list["DocumentVersion"]] = relationship(
        "DocumentVersion",
        back_populates="blob",
    )


class BlobLocation(Base):
    __tablename__ = "blob_locations"
    __table_args__ = (
        UniqueConstraint("backend", "bucket", "object_key", name="uq_blob_location"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    blob_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("blobs.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    backend: Mapped[str] = mapped_column(String, nullable=False)
    bucket: Mapped[str] = mapped_column(String, nullable=False, default="")
    object_key: Mapped[str] = mapped_column(String, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)

    blob: Mapped[Blob] = relationship("Blob", back_populates="locations")


class Document(Base):
    __tablename__ = "documents"
    __table_args__ = (UniqueConstraint("folder_id", "name", name="uq_documents_folder_name"),)

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    folder_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("folders.id", ondelete="CASCADE"),
        nullable=False,
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

    folder: Mapped[Folder] = relationship("Folder", back_populates="documents")
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
    __table_args__ = (
        Index(
            "uq_document_locks_active_document",
            "document_id",
            unique=True,
            sqlite_where=text("is_active = 1"),
        ),
    )

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
    is_active: Mapped[bool] = mapped_column(Boolean, default=True, nullable=False)
    locked_ip: Mapped[str | None] = mapped_column(String, nullable=True)
    locked_user_agent: Mapped[str | None] = mapped_column(String, nullable=True)
    force_acquired: Mapped[bool] = mapped_column(Boolean, default=False, nullable=False)
    released_at: Mapped[datetime.datetime | None] = mapped_column(DateTime, nullable=True)
    released_by: Mapped[str | None] = mapped_column(String, nullable=True)

    document: Mapped[Document] = relationship("Document", back_populates="locks")


class DocumentVersion(Base):
    __tablename__ = "document_versions"
    __table_args__ = (
        UniqueConstraint("document_id", "version_number", name="uq_versions_document_number"),
    )

    id: Mapped[str] = mapped_column(String, primary_key=True, index=True)
    document_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("documents.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    blob_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("blobs.id"),
        nullable=False,
        index=True,
    )
    version_number: Mapped[int] = mapped_column(Integer, nullable=False)
    committed_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    committed_by: Mapped[str] = mapped_column(String, nullable=False)
    committed_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    message: Mapped[str | None] = mapped_column(Text, nullable=True)
    mime_type: Mapped[str | None] = mapped_column(String, nullable=True)
    original_filename: Mapped[str | None] = mapped_column(String, nullable=True)
    upload_ip: Mapped[str | None] = mapped_column(String, nullable=True)
    upload_user_agent: Mapped[str | None] = mapped_column(String, nullable=True)
    created_via: Mapped[str | None] = mapped_column(String, nullable=True)

    document: Mapped[Document] = relationship("Document", back_populates="versions")
    blob: Mapped[Blob] = relationship("Blob", back_populates="versions")


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


class StateEvent(Base):
    __tablename__ = "state_events"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    event_type: Mapped[str] = mapped_column(String, nullable=False, index=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow, index=True)
    payload: Mapped[dict[str, object]] = mapped_column(JSON, nullable=False)
