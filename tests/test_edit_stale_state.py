import unittest

from fastapi import HTTPException
from tests.support import (
    FAKE_REQUEST,
    add_permission,
    create_versioned_document,
    user_context,
    vault_runtime,
)

from app.db import SessionLocal
from app.models import Document, DocumentLock, FolderPermission, VaultGroup
from app.routers import (
    ActionItem,
    ActionPayload,
    archive_doc_item,
    checkout_document,
    get_document_or_404,
    get_or_create_folder_path,
    get_root_folder,
    lock_items,
    move_doc_item,
    unlock_items,
)


class EditStaleStateTests(unittest.TestCase):
    def test_lock_rechecks_archived_state_inside_write_lock(self) -> None:
        user = user_context("alice")

        with vault_runtime():
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=user)
                db.commit()
                doc_id = doc.id

            stale_db = SessionLocal()
            try:
                get_document_or_404(doc_id, stale_db)
                with SessionLocal() as archive_db:
                    doc = archive_db.get(Document, doc_id)
                    archive_doc_item(doc, FAKE_REQUEST, user, archive_db)
                    archive_db.commit()

                result = lock_items(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    FAKE_REQUEST,
                    user,
                    stale_db,
                )
                self.assertEqual(result["ok"], [])
                self.assertEqual(
                    result["failed"][0]["detail"],
                    "Restore this file before editing",
                )
            finally:
                stale_db.close()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.root_key, "archive")
                active_locks = db.query(DocumentLock).filter_by(
                    document_id=doc_id,
                    is_active=True,
                )
                self.assertEqual(active_locks.count(), 0)

    def test_checkout_rechecks_archived_state_inside_write_lock(self) -> None:
        user = user_context("alice")

        with vault_runtime():
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=user)
                db.commit()
                doc_id = doc.id

            stale_db = SessionLocal()
            try:
                get_document_or_404(doc_id, stale_db)
                with SessionLocal() as archive_db:
                    doc = archive_db.get(Document, doc_id)
                    archive_doc_item(doc, FAKE_REQUEST, user, archive_db)
                    archive_db.commit()

                with self.assertRaises(HTTPException) as raised:
                    checkout_document(doc_id, FAKE_REQUEST, user, stale_db)

                self.assertEqual(raised.exception.status_code, 400)
                self.assertEqual(raised.exception.detail, "Restore this file before editing")
            finally:
                stale_db.close()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.root_key, "archive")
                active_locks = db.query(DocumentLock).filter_by(
                    document_id=doc_id,
                    is_active=True,
                )
                self.assertEqual(active_locks.count(), 0)

    def test_unlock_rechecks_current_folder_access_from_stale_session(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        writer = user_context("writer", groups=["writers"], is_admin=False)

        with vault_runtime():
            with SessionLocal() as db:
                root = get_root_folder(db, "vault")
                writers = VaultGroup(name="writers")
                db.add(writers)
                db.flush()
                add_permission(db, root, writers, write=True)

                project = get_or_create_folder_path(db, "Project")
                private = get_or_create_folder_path(db, "Private")
                db.flush()
                private_permission = (
                    db.query(FolderPermission)
                    .filter_by(folder_id=private.id, group_id=writers.id)
                    .one_or_none()
                )
                if private_permission is None:
                    private_permission = FolderPermission(
                        folder_id=private.id,
                        group_id=writers.id,
                    )
                    db.add(private_permission)
                private_permission.can_view = False
                private_permission.can_read = False
                private_permission.can_write = False

                doc = create_versioned_document(db, project, actor=admin)
                db.add(
                    DocumentLock(
                        document_id=doc.id,
                        locked_by="artist",
                        locked_by_name="Artist",
                    ),
                )
                db.commit()
                doc_id = doc.id

            stale_db = SessionLocal()
            try:
                stale_doc = get_document_or_404(doc_id, stale_db)
                self.assertEqual(stale_doc.folder.name, "Project")

                with SessionLocal() as move_db:
                    doc = move_db.get(Document, doc_id)
                    move_doc_item(doc, "Private", FAKE_REQUEST, admin, move_db)
                    move_db.commit()

                result = unlock_items(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    FAKE_REQUEST,
                    writer,
                    stale_db,
                )
                self.assertEqual(result["ok"], [])
                self.assertEqual(result["failed"][0]["detail"], "Document not found")
            finally:
                stale_db.close()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.name, "Private")
                active_locks = db.query(DocumentLock).filter_by(
                    document_id=doc_id,
                    is_active=True,
                )
                self.assertEqual(active_locks.count(), 1)


if __name__ == "__main__":
    unittest.main()
