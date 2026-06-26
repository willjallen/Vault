import unittest

from tests.support import (
    auth_headers,
    create_versioned_document,
    sha256_hex,
    user_context,
    vault_test_client,
)

from app.db import SessionLocal
from app.models import Blob, BlobLocation, Document, DocumentVersion
from app.routers import get_or_create_folder_path, storage_reconciliation_report
from app.storage import get_storage_backend


class CreateDocumentStaleStateTests(unittest.TestCase):
    def test_create_document_rechecks_duplicate_path_after_upload_read(self) -> None:
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

        with vault_test_client() as ctx:
            with ctx.db() as db:
                get_or_create_folder_path(db, "Race")
                db.commit()

            headers = auth_headers("alice", ["vault-admin"])
            data = b"lose"
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "race.txt",
                    "folder": "Race",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            part_response = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=data,
                headers={
                    **headers,
                    "Content-Type": "application/octet-stream",
                    "X-Upload-Offset": "0",
                    "X-Upload-Sha256": sha256_hex(data),
                    "X-Upload-Size": str(len(data)),
                },
            )
            self.assertEqual(part_response.status_code, 200, part_response.text)
            create_competing_document()
            completed = ctx.client.post(
                f"/api/uploads/{session['id']}/complete",
                json={"sha256": sha256_hex(data)},
                headers=headers,
            )
            self.assertEqual(completed.status_code, 400)
            self.assertEqual(completed.json()["detail"], "A document already exists at that path")

            with ctx.db() as db:
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
