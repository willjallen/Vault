import unittest

from fastapi import HTTPException
from tests.support import FAKE_REQUEST, create_versioned_document, user_context, vault_runtime

from app.db import SessionLocal
from app.models import Document, DocumentLock
from app.routers import (
    ActionItem,
    ActionPayload,
    archive_doc_item,
    checkout_document,
    get_document_or_404,
    get_or_create_folder_path,
    lock_items,
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


if __name__ == "__main__":
    unittest.main()
