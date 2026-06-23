import datetime as dt
import unittest

from fastapi import HTTPException
from tests.support import FAKE_REQUEST, create_versioned_document, user_context, vault_runtime

from app.db import SessionLocal
from app.models import Document, DocumentEvent
from app.routers import (
    all_folders,
    archive_doc_item,
    build_folder_path_cache,
    docs_stats_for_folder_payloads,
    document_row_payload,
    folder_path,
    folder_summary_payload,
    get_document_or_404,
    get_folder_by_path,
    get_or_create_folder_path,
    move_doc_item,
    normalize_timestamp,
    now_utc,
    restore_doc_item,
)


class LocationStaleStateTests(unittest.TestCase):
    def test_row_modified_time_uses_version_commit_not_location_changes(self) -> None:
        user = user_context("alice")

        with vault_runtime():
            content_modified_at = now_utc() - dt.timedelta(days=7)
            expected_modified_at = normalize_timestamp(content_modified_at).isoformat()
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(
                    db,
                    folder,
                    actor=user,
                    committed_at=content_modified_at,
                )
                db.commit()
                doc_id = doc.id

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                archive_doc_item(doc, FAKE_REQUEST, user, db)
                db.flush()
                self.assertGreater(normalize_timestamp(doc.latest_modified_at), content_modified_at)
                cache = build_folder_path_cache(all_folders(db))
                row = document_row_payload(doc, db, cache)
                self.assertEqual(row["modified_at"], expected_modified_at)
                stats = docs_stats_for_folder_payloads([doc], db, cache)
                summary = folder_summary_payload(doc.folder, folder_path(doc.folder, cache), stats)
                self.assertEqual(summary["modified_at"], expected_modified_at)

                restore_doc_item(doc, FAKE_REQUEST, user, db)
                db.flush()
                cache = build_folder_path_cache(all_folders(db))
                row = document_row_payload(doc, db, cache)
                self.assertEqual(row["modified_at"], expected_modified_at)

    def test_stale_move_cannot_restore_archived_document_as_plain_move(self) -> None:
        user = user_context("alice")

        with vault_runtime():
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=user)
                db.commit()
                doc_id = doc.id

            stale_db = SessionLocal()
            try:
                stale_doc = get_document_or_404(doc_id, stale_db)
                with SessionLocal() as archive_db:
                    doc = archive_db.get(Document, doc_id)
                    archive_doc_item(doc, FAKE_REQUEST, user, archive_db)
                    archive_db.commit()

                with self.assertRaises(HTTPException) as raised:
                    move_doc_item(stale_doc, "Other", FAKE_REQUEST, user, stale_db, name="plan.txt")

                self.assertEqual(raised.exception.status_code, 400)
                self.assertEqual(
                    raised.exception.detail,
                    "Use archive or restore for Archive moves",
                )
            finally:
                stale_db.close()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.root_key, "archive")
                self.assertEqual(doc.folder.name, "Project")
                self.assertIsNone(get_folder_by_path(db, "Other"))
                events = db.query(DocumentEvent).filter_by(document_id=doc_id).all()
                self.assertEqual([event.event_type for event in events], ["archive"])

    def test_stale_archive_does_not_record_duplicate_archive_transition(self) -> None:
        user = user_context("alice")

        with vault_runtime():
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=user)
                db.commit()
                doc_id = doc.id

            stale_db = SessionLocal()
            try:
                stale_doc = get_document_or_404(doc_id, stale_db)
                with SessionLocal() as archive_db:
                    doc = archive_db.get(Document, doc_id)
                    archive_doc_item(doc, FAKE_REQUEST, user, archive_db)
                    archive_db.commit()

                with self.assertRaises(HTTPException) as raised:
                    archive_doc_item(stale_doc, FAKE_REQUEST, user, stale_db)

                self.assertEqual(raised.exception.status_code, 400)
                self.assertEqual(raised.exception.detail, "Document is already archived")
            finally:
                stale_db.close()

            with SessionLocal() as db:
                events = db.query(DocumentEvent).filter_by(document_id=doc_id).all()
                self.assertEqual([event.event_type for event in events], ["archive"])


if __name__ == "__main__":
    unittest.main()
