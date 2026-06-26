"""SQLAlchemy models for the vault service."""

import datetime

from sqlalchemy import (
    JSON,
    Boolean,
    DateTime,
    ForeignKey,
    Index,
    Integer,
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
    default_ttl_days: Mapped[int | None] = mapped_column(Integer, nullable=True)
    default_ttl_action: Mapped[str | None] = mapped_column(String, nullable=True)

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
    preferences: Mapped[dict[str, object]] = mapped_column(
        JSON,
        default=dict,
        server_default=text("'{}'"),
        nullable=False,
    )
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


class VaultSetting(Base):
    __tablename__ = "vault_settings"

    key: Mapped[str] = mapped_column(String, primary_key=True)
    value: Mapped[object] = mapped_column(JSON, nullable=False)
    updated_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)


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
    __table_args__ = (UniqueConstraint("backend", "bucket", "object_key", name="uq_blob_location"),)

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


class UploadSession(Base):
    __tablename__ = "upload_sessions"
    __table_args__ = (
        Index("ix_upload_sessions_owner_status", "created_by", "status"),
        Index("ix_upload_sessions_expires_at", "expires_at"),
    )

    id: Mapped[str] = mapped_column(String, primary_key=True)
    mode: Mapped[str] = mapped_column(String, nullable=False)
    status: Mapped[str] = mapped_column(String, default="active", nullable=False)
    folder_path: Mapped[str | None] = mapped_column(String, nullable=True)
    document_id: Mapped[int | None] = mapped_column(
        Integer,
        ForeignKey("documents.id", ondelete="CASCADE"),
        nullable=True,
        index=True,
    )
    filename: Mapped[str] = mapped_column(String, nullable=False)
    total_size: Mapped[int] = mapped_column(Integer, nullable=False)
    chunk_size: Mapped[int] = mapped_column(Integer, nullable=False)
    part_count: Mapped[int] = mapped_column(Integer, nullable=False)
    verification_total_bytes: Mapped[int] = mapped_column(Integer, default=0, nullable=False)
    verification_processed_bytes: Mapped[int] = mapped_column(Integer, default=0, nullable=False)
    mime_type: Mapped[str | None] = mapped_column(String, nullable=True)
    note: Mapped[str | None] = mapped_column(Text, nullable=True)
    rename_to_upload: Mapped[bool] = mapped_column(Boolean, default=False, nullable=False)
    created_by: Mapped[str] = mapped_column(String, nullable=False, index=True)
    created_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    user_context: Mapped[dict[str, object]] = mapped_column(JSON, nullable=False)
    upload_ip: Mapped[str | None] = mapped_column(String, nullable=True)
    upload_user_agent: Mapped[str | None] = mapped_column(String, nullable=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    updated_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    expires_at: Mapped[datetime.datetime] = mapped_column(DateTime, nullable=False)
    completed_at: Mapped[datetime.datetime | None] = mapped_column(DateTime, nullable=True)
    aborted_at: Mapped[datetime.datetime | None] = mapped_column(DateTime, nullable=True)
    error: Mapped[str | None] = mapped_column(Text, nullable=True)
    result_document_id: Mapped[int | None] = mapped_column(Integer, nullable=True)
    result_version_id: Mapped[str | None] = mapped_column(String, nullable=True)
    result_path: Mapped[str | None] = mapped_column(String, nullable=True)

    document: Mapped["Document | None"] = relationship("Document")
    parts: Mapped[list["UploadPart"]] = relationship(
        "UploadPart",
        back_populates="session",
        cascade="all, delete-orphan",
    )


class UploadPart(Base):
    __tablename__ = "upload_parts"
    __table_args__ = (
        UniqueConstraint("session_id", "part_number", name="uq_upload_part_number"),
        Index("ix_upload_parts_session_offset", "session_id", "offset_bytes"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    session_id: Mapped[str] = mapped_column(
        String,
        ForeignKey("upload_sessions.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    part_number: Mapped[int] = mapped_column(Integer, nullable=False)
    offset_bytes: Mapped[int] = mapped_column(Integer, nullable=False)
    size_bytes: Mapped[int] = mapped_column(Integer, nullable=False)
    sha256: Mapped[str] = mapped_column(String, nullable=False)
    storage_path: Mapped[str] = mapped_column(String, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)

    session: Mapped[UploadSession] = relationship("UploadSession", back_populates="parts")


class Document(Base):
    __tablename__ = "documents"
    __table_args__ = (
        Index(
            "uq_documents_active_folder_name",
            "folder_id",
            "name",
            unique=True,
            sqlite_where=text("archived_from_folder IS NULL"),
        ),
    )

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
    expires_at: Mapped[datetime.datetime | None] = mapped_column(
        DateTime,
        nullable=True,
        index=True,
    )
    expiry_action: Mapped[str | None] = mapped_column(String, nullable=True)
    archived_from_folder: Mapped[str | None] = mapped_column(String, nullable=True)
    archived_original_name: Mapped[str | None] = mapped_column(String, nullable=True)
    archived_access: Mapped[dict[str, int] | None] = mapped_column(JSON, nullable=True)

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


class ExportJob(Base):
    __tablename__ = "export_jobs"
    __table_args__ = (
        Index("ix_export_jobs_owner_status", "created_by", "status"),
        Index("ix_export_jobs_expires_at", "expires_at"),
    )

    id: Mapped[str] = mapped_column(String, primary_key=True)
    status: Mapped[str] = mapped_column(String, default="queued", nullable=False)
    created_by: Mapped[str] = mapped_column(String, nullable=False, index=True)
    created_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    user_context: Mapped[dict[str, object]] = mapped_column(JSON, nullable=False)
    request_payload: Mapped[dict[str, object]] = mapped_column(JSON, nullable=False)
    filename: Mapped[str] = mapped_column(String, nullable=False)
    total_items: Mapped[int] = mapped_column(Integer, default=0, nullable=False)
    processed_items: Mapped[int] = mapped_column(Integer, default=0, nullable=False)
    total_bytes: Mapped[int] = mapped_column(Integer, default=0, nullable=False)
    processed_bytes: Mapped[int] = mapped_column(Integer, default=0, nullable=False)
    error: Mapped[str | None] = mapped_column(Text, nullable=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    updated_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    expires_at: Mapped[datetime.datetime] = mapped_column(DateTime, nullable=False)
    completed_at: Mapped[datetime.datetime | None] = mapped_column(DateTime, nullable=True)
    cancelled_at: Mapped[datetime.datetime | None] = mapped_column(DateTime, nullable=True)

    artifacts: Mapped[list["ExportArtifact"]] = relationship(
        "ExportArtifact",
        back_populates="job",
        cascade="all, delete-orphan",
    )


class ExportArtifact(Base):
    __tablename__ = "export_artifacts"
    __table_args__ = (UniqueConstraint("job_id", name="uq_export_artifact_job"),)

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    job_id: Mapped[str] = mapped_column(
        String,
        ForeignKey("export_jobs.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    blob_id: Mapped[int] = mapped_column(
        Integer,
        ForeignKey("blobs.id", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    filename: Mapped[str] = mapped_column(String, nullable=False)
    mime_type: Mapped[str] = mapped_column(String, default="application/zip", nullable=False)
    size_bytes: Mapped[int] = mapped_column(Integer, nullable=False)
    hash_algo: Mapped[str] = mapped_column(String, default="sha256", nullable=False)
    hash: Mapped[str] = mapped_column(String, nullable=False)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    expires_at: Mapped[datetime.datetime] = mapped_column(DateTime, nullable=False, index=True)

    job: Mapped[ExportJob] = relationship("ExportJob", back_populates="artifacts")
    blob: Mapped[Blob] = relationship("Blob")


class StateEvent(Base):
    __tablename__ = "state_events"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    event_type: Mapped[str] = mapped_column(String, nullable=False, index=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow, index=True)
    payload: Mapped[dict[str, object]] = mapped_column(JSON, nullable=False)


class ShareLink(Base):
    __tablename__ = "share_links"
    __table_args__ = (
        Index("ix_share_links_code", "code", unique=True),
        Index("ix_share_links_document", "document_id"),
        Index("ix_share_links_folder", "folder_id"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, index=True)
    code: Mapped[str] = mapped_column(String, nullable=False, unique=True)
    target_type: Mapped[str] = mapped_column(String, nullable=False)
    document_id: Mapped[int | None] = mapped_column(
        Integer,
        ForeignKey("documents.id", ondelete="CASCADE"),
        nullable=True,
    )
    folder_id: Mapped[int | None] = mapped_column(
        Integer,
        ForeignKey("folders.id", ondelete="CASCADE"),
        nullable=True,
    )
    access_mode: Mapped[str] = mapped_column(String, default="internal", nullable=False)
    created_by: Mapped[str | None] = mapped_column(String, nullable=True)
    created_by_name: Mapped[str | None] = mapped_column(String, nullable=True)
    created_by_user_id: Mapped[int | None] = mapped_column(
        Integer,
        ForeignKey("vault_users.id", ondelete="SET NULL"),
        nullable=True,
    )
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, default=utcnow)
    expires_at: Mapped[datetime.datetime | None] = mapped_column(DateTime, nullable=True)
    disabled_at: Mapped[datetime.datetime | None] = mapped_column(DateTime, nullable=True)

    document: Mapped[Document | None] = relationship("Document")
    folder: Mapped[Folder | None] = relationship("Folder")
