import unittest

from tests.support import FAKE_REQUEST, create_versioned_document, user_context, vault_runtime

from app.db import SessionLocal
from app.models import Blob, Document, DocumentEvent, DocumentVersion
from app.routers import (
    ActionItem,
    ActionPayload,
    archive_doc_item,
    archive_folder_item,
    delete_items_forever,
    get_document_or_404,
    get_folder_by_path,
    get_or_create_folder_path,
    restore_doc_item,
    restore_folder_item,
    storage_reconciliation_report,
)
from app.storage import get_storage_backend


class DeleteStaleStateTests(unittest.TestCase):
    def test_stale_archive_delete_cannot_permanently_delete_restored_document(self) -> None:
        admin = user_context("alice", groups=["vault-admin"])

        with vault_runtime():
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=admin)
                db.commit()
                doc_id = doc.id
                archive_doc_item(doc, FAKE_REQUEST, admin, db)
                db.commit()

            stale_db = SessionLocal()
            try:
                stale_doc = get_document_or_404(doc_id, stale_db)
                self.assertEqual(stale_doc.folder.root_key, "archive")

                with SessionLocal() as restore_db:
                    doc = restore_db.get(Document, doc_id)
                    restore_doc_item(doc, FAKE_REQUEST, admin, restore_db)
                    restore_db.commit()

                result = delete_items_forever(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    admin,
                    stale_db,
                )
                self.assertEqual(result["ok"], [])
                self.assertEqual(
                    result["failed"][0]["detail"],
                    "Move the document to Archive before deleting",
                )
                stale_db.rollback()
            finally:
                stale_db.close()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.root_key, "vault")
                self.assertEqual(doc.folder.name, "Project")
                self.assertEqual(db.query(DocumentVersion).filter_by(document_id=doc_id).count(), 1)
                self.assertEqual(db.query(Blob).count(), 1)
                self.assertEqual(len(get_storage_backend("local").list_object_keys()), 1)
                report = storage_reconciliation_report(db, apply=False)
                self.assertEqual(report["orphan_blob_ids"], [])
                self.assertEqual(report["unreferenced_local_keys"], [])
                events = db.query(DocumentEvent).filter_by(document_id=doc_id).all()
                self.assertEqual([event.event_type for event in events], ["archive", "unarchive"])

    def test_stale_archive_delete_refreshes_folder_restores_with_same_folder_id(self) -> None:
        admin = user_context("alice", groups=["vault-admin"])

        with vault_runtime():
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=admin)
                db.commit()
                doc_id = doc.id
                source = get_folder_by_path(db, "Project")
                archive_folder_item(source, FAKE_REQUEST, admin, db)
                db.commit()

            stale_db = SessionLocal()
            try:
                stale_doc = get_document_or_404(doc_id, stale_db)
                self.assertEqual(stale_doc.folder.root_key, "archive")

                with SessionLocal() as restore_db:
                    source = get_folder_by_path(restore_db, "Archive/Project")
                    restore_folder_item(source, FAKE_REQUEST, admin, restore_db)
                    restore_db.commit()

                result = delete_items_forever(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    admin,
                    stale_db,
                )
                self.assertEqual(result["ok"], [])
                self.assertEqual(
                    result["failed"][0]["detail"],
                    "Move the document to Archive before deleting",
                )
                stale_db.rollback()
            finally:
                stale_db.close()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.root_key, "vault")
                self.assertEqual(doc.folder.name, "Project")
                self.assertEqual(db.query(DocumentVersion).filter_by(document_id=doc_id).count(), 1)
                self.assertEqual(db.query(Blob).count(), 1)
                self.assertEqual(len(get_storage_backend("local").list_object_keys()), 1)
                report = storage_reconciliation_report(db, apply=False)
                self.assertEqual(report["orphan_blob_ids"], [])
                self.assertEqual(report["unreferenced_local_keys"], [])
                events = db.query(DocumentEvent).filter_by(document_id=doc_id).all()
                self.assertEqual([event.event_type for event in events], ["archive", "unarchive"])


if __name__ == "__main__":
    unittest.main()
