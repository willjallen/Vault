import datetime as dt
import unittest

from tests.support import FAKE_REQUEST, user_context, vault_runtime

from app.models import Document, DocumentLock, StateEvent
from app.routers import (
    apply_folder_ttl,
    folder_path,
    get_or_create_folder_path,
    move_doc_item,
    normalize_timestamp,
    now_utc,
    restore_doc_item,
    sweep_expired_documents,
)


class RetentionTtlTests(unittest.TestCase):
    def test_expired_document_is_archived_to_matching_archive_folder(self) -> None:
        admin = user_context("alice", groups=["vault-admin"])

        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                folder.default_ttl_days = 30
                folder.default_ttl_action = "archive"
                doc = Document(folder_id=folder.id, name="plan.txt", latest_modified_at=now_utc())
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, now_utc() - dt.timedelta(days=31))
                self.assertLessEqual(doc.expires_at, now_utc())
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["archived"], ["Archive/Project/plan.txt"])
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(folder_path(doc.folder), "Archive/Project")
                self.assertIsNone(doc.expires_at)
                self.assertIsNone(doc.expiry_action)
                event = db.query(StateEvent).filter_by(event_type="retention.expired").one()
                self.assertEqual(
                    event.payload["resources"],
                    ["contents", "document_detail", "my_edits", "sidebar"],
                )

                restore_doc_item(doc, FAKE_REQUEST, admin, db)
                db.commit()

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(folder_path(doc.folder), "Project")
                self.assertEqual(doc.expiry_action, "archive")
                self.assertIsNotNone(doc.expires_at)
                threshold = now_utc() + dt.timedelta(days=29)
                self.assertGreater(normalize_timestamp(doc.expires_at), threshold)

    def test_expired_document_can_be_deleted_without_archive_first(self) -> None:
        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Temp")
                folder.default_ttl_days = 1
                folder.default_ttl_action = "delete"
                doc = Document(
                    folder_id=folder.id,
                    name="scratch.txt",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, now_utc() - dt.timedelta(days=2))
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["archived"], [])
            self.assertEqual(result["deleted"], ["Temp/scratch.txt"])

            with ctx.db() as db:
                self.assertIsNone(db.get(Document, doc_id))

    def test_locked_expired_document_is_skipped(self) -> None:
        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Working")
                folder.default_ttl_days = 1
                folder.default_ttl_action = "delete"
                doc = Document(
                    folder_id=folder.id,
                    name="locked.txt",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, now_utc() - dt.timedelta(days=2))
                db.add(DocumentLock(document_id=doc.id, locked_by="user", is_active=True))
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["skipped"], ["Working/locked.txt"])
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertIsNotNone(doc.expires_at)
                self.assertEqual(doc.expiry_action, "delete")

    def test_plain_folders_do_not_compute_delete_ttl_for_old_documents(self) -> None:
        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Safe")
                doc = Document(
                    folder_id=folder.id,
                    name="old-but-safe.txt",
                    latest_modified_at=now_utc() - dt.timedelta(days=365),
                )
                db.add(doc)
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result, {"archived": [], "deleted": [], "skipped": []})

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertIsNone(doc.expires_at)
                self.assertIsNone(doc.expiry_action)
                event_count = db.query(StateEvent).filter_by(event_type="retention.expired").count()
                self.assertEqual(event_count, 0)

    def test_child_folder_without_ttl_does_not_inherit_parent_delete_ttl(self) -> None:
        with vault_runtime() as ctx:
            with ctx.db() as db:
                parent = get_or_create_folder_path(db, "Temp")
                parent.default_ttl_days = 1
                parent.default_ttl_action = "delete"
                child = get_or_create_folder_path(db, "Temp/Keep")
                doc = Document(
                    folder_id=child.id,
                    name="child-safe.txt",
                    latest_modified_at=now_utc() - dt.timedelta(days=30),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, child, now_utc() - dt.timedelta(days=30))
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertIsNone(doc.expires_at)
                self.assertIsNone(doc.expiry_action)

    def test_moving_from_delete_ttl_folder_to_plain_folder_clears_delete_expiry(self) -> None:
        admin = user_context("alice", groups=["vault-admin"])

        with vault_runtime() as ctx:
            with ctx.db() as db:
                source = get_or_create_folder_path(db, "Temp")
                source.default_ttl_days = 1
                source.default_ttl_action = "delete"
                get_or_create_folder_path(db, "Safe")
                doc = Document(folder_id=source.id, name="rescue.txt", latest_modified_at=now_utc())
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, source, now_utc() - dt.timedelta(days=2))
                self.assertEqual(doc.expiry_action, "delete")
                move_doc_item(doc, "Safe", FAKE_REQUEST, admin, db)
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertIsNone(doc.expires_at)
                self.assertIsNone(doc.expiry_action)
                self.assertEqual(doc.folder.name, "Safe")


if __name__ == "__main__":
    unittest.main()
