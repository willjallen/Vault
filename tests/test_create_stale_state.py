import asyncio
import unittest

from fastapi import HTTPException
from tests.support import (
    FAKE_REQUEST,
    create_versioned_document,
    user_context,
    vault_runtime,
)

from app.db import SessionLocal
from app.models import Blob, BlobLocation, Document, DocumentVersion
from app.routers import (
    create_document,
    get_or_create_folder_path,
    storage_reconciliation_report,
)
from app.storage import get_storage_backend


class CreateDocumentStaleStateTests(unittest.TestCase):
    def test_create_document_rechecks_duplicate_path_after_upload_read(self) -> None:
        alice = user_context("alice", groups=[], is_admin=True)
        bob = user_context("bob", groups=[], is_admin=False)

        def create_competing_document() -> None:
            with SessionLocal() as other_db:
                folder = get_or_create_folder_path(other_db, "Race")
                create_versioned_document(
                    other_db,
                    folder,
                    name="race.txt",
                    data=b"winner",
                    actor=bob,
                )
                other_db.commit()

        class RacingUpload:
            filename = "race.txt"
            content_type = "text/plain"

            def __init__(self) -> None:
                self._sent = False

            async def read(self, size: int = -1) -> bytes:
                del size
                if self._sent:
                    return b""
                self._sent = True
                create_competing_document()
                return b"loser"

        with vault_runtime():
            with SessionLocal() as db:
                get_or_create_folder_path(db, "Race")
                db.commit()

            with SessionLocal() as db:
                try:
                    with self.assertRaises(HTTPException) as raised:
                        asyncio.run(
                            create_document(FAKE_REQUEST, RacingUpload(), "Race", alice, db),
                        )

                    self.assertEqual(raised.exception.status_code, 400)
                    self.assertEqual(
                        raised.exception.detail,
                        "A document already exists at that path",
                    )
                finally:
                    db.rollback()

            with SessionLocal() as db:
                documents = db.query(Document).all()
                self.assertEqual(
                    [(doc.name, doc.created_by) for doc in documents],
                    [("race.txt", "bob")],
                )
                self.assertEqual(db.query(DocumentVersion).count(), 1)
                self.assertEqual(db.query(Blob).count(), 1)
                self.assertEqual(db.query(BlobLocation).count(), 1)
                self.assertEqual(len(get_storage_backend("local").list_object_keys()), 1)
                report = storage_reconciliation_report(db, apply=False)
                self.assertEqual(report["orphan_blob_ids"], [])
                self.assertEqual(report["unreferenced_local_keys"], [])


if __name__ == "__main__":
    unittest.main()
