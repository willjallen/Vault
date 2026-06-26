import unittest

from tests.support import (
    FAKE_REQUEST,
    add_permission,
    create_versioned_document,
    user_context,
    vault_runtime,
)

from app.db import SessionLocal
from app.models import Blob, Document, DocumentEvent, DocumentLock, DocumentVersion, VaultGroup
from app.routers import (
    ActionItem,
    ActionPayload,
    archive_doc_item,
    archive_folder_item,
    delete_items_forever,
    get_document_or_404,
    get_folder_by_path,
    get_or_create_folder_path,
    get_root_folder,
    restore_doc_item,
    storage_reconciliation_report,
)
from app.site_settings import merge_site_settings
from app.storage import get_storage_backend


class DeleteStaleStateTests(unittest.TestCase):
    def test_delete_forever_rejects_document_locked_by_other_user(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        writer = user_context("writer", groups=["writers"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                vault_root = get_root_folder(db, "vault")
                archive_root = get_root_folder(db, "archive")
                writers = VaultGroup(name="writers")
                db.add(writers)
                db.flush()
                add_permission(db, vault_root, writers, write=True)
                add_permission(db, archive_root, writers, write=True)
                merge_site_settings(db, {"archivePermanentDeleteAdminOnly": False})

                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=admin)
                archive_doc_item(doc, FAKE_REQUEST, admin, db)
                db.add(
                    DocumentLock(
                        document_id=doc.id,
                        locked_by="editor",
                        locked_by_name="Editor",
                    ),
                )
                db.commit()
                doc_id = doc.id

                result = delete_items_forever(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    writer,
                    db,
                )

                self.assertEqual(result["ok"], [])
                self.assertEqual(
                    result["failed"][0]["detail"],
                    "Document is locked by another user",
                )

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.root_key, "archive")
                active_locks = db.query(DocumentLock).filter_by(
                    document_id=doc_id,
                    is_active=True,
                )
                self.assertEqual(active_locks.count(), 1)
                self.assertEqual(db.query(DocumentVersion).filter_by(document_id=doc_id).count(), 1)

    def test_delete_forever_rejects_file_archived_from_folder_locked_by_other_user(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        writer = user_context("writer", groups=["writers"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                vault_root = get_root_folder(db, "vault")
                archive_root = get_root_folder(db, "archive")
                writers = VaultGroup(name="writers")
                db.add(writers)
                db.flush()
                add_permission(db, vault_root, writers, write=True)
                add_permission(db, archive_root, writers, write=True)
                merge_site_settings(db, {"archivePermanentDeleteAdminOnly": False})

                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=admin)
                archive_folder_item(folder, FAKE_REQUEST, admin, db)
                db.add(
                    DocumentLock(
                        document_id=doc.id,
                        locked_by="editor",
                        locked_by_name="Editor",
                    ),
                )
                db.commit()
                doc_id = doc.id

                result = delete_items_forever(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    writer,
                    db,
                )

                self.assertEqual(result["ok"], [])
                self.assertEqual(
                    result["failed"][0]["detail"],
                    "Document is locked by another user",
                )

            with ctx.db() as db:
                self.assertIsNotNone(db.get(Document, doc_id))
                self.assertIsNone(get_folder_by_path(db, "Project"))
                self.assertIsNone(get_folder_by_path(db, "Archive/Project"))
                self.assertEqual(db.get(Document, doc_id).folder.root_key, "archive")
                active_locks = db.query(DocumentLock).filter_by(
                    document_id=doc_id,
                    is_active=True,
                )
                self.assertEqual(active_locks.count(), 1)

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

    def test_stale_archive_delete_refreshes_document_restored_after_folder_archive(self) -> None:
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


if __name__ == "__main__":
    unittest.main()
