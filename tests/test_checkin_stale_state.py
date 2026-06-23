import asyncio
import unittest

from fastapi import HTTPException
from tests.support import FAKE_REQUEST, create_versioned_document, user_context, vault_runtime

from app.db import SessionLocal
from app.models import Document, DocumentLock, DocumentVersion
from app.routers import archive_doc_item, checkin_document, get_or_create_folder_path


class CheckinStaleStateTests(unittest.TestCase):
    def test_checkin_rechecks_archived_state_after_upload_read(self) -> None:
        user = user_context("alice")

        class ArchivingUpload:
            filename = "plan.txt"
            content_type = "text/plain"

            def __init__(self, doc_id: int) -> None:
                self.doc_id = doc_id

            async def read(self) -> bytes:
                with SessionLocal() as archive_db:
                    doc = archive_db.get(Document, self.doc_id)
                    archive_doc_item(doc, FAKE_REQUEST, user, archive_db)
                    archive_db.commit()
                return b"v2 after archive"

        with vault_runtime():
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=user)
                db.add(
                    DocumentLock(
                        document_id=doc.id,
                        locked_by=str(user["id"]),
                        locked_by_name=str(user["name"]),
                    ),
                )
                db.commit()
                doc_id = doc.id

            with SessionLocal() as db:
                with self.assertRaises(HTTPException) as raised:
                    asyncio.run(
                        checkin_document(
                            doc_id,
                            FAKE_REQUEST,
                            ArchivingUpload(doc_id),
                            "race",
                            False,
                            user,
                            db,
                        ),
                    )

                self.assertEqual(raised.exception.status_code, 400)
                self.assertEqual(raised.exception.detail, "Restore this file before editing")

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.root_key, "archive")
                versions = db.query(DocumentVersion).filter_by(document_id=doc_id).all()
                self.assertEqual(len(versions), 1)
                self.assertEqual(doc.latest_version_number, 1)


if __name__ == "__main__":
    unittest.main()
