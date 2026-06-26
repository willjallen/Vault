import unittest

from tests.support import (
    FAKE_REQUEST,
    auth_headers,
    create_versioned_document,
    sha256_hex,
    user_context,
    vault_test_client,
)

from app.models import Document, DocumentVersion
from app.routers import archive_doc_item, get_or_create_folder_path


class CheckinStaleStateTests(unittest.TestCase):
    def test_checkin_rechecks_archived_state_after_upload_read(self) -> None:
        user = user_context("alice")

        with vault_test_client() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=user)
                db.commit()
                doc_id = doc.id

            headers = auth_headers("alice", ["vault-admin"])
            locked = ctx.client.post(
                "/api/lock",
                json={"items": [{"type": "document", "id": doc_id}]},
                headers=headers,
            )
            self.assertEqual(locked.status_code, 200, locked.text)
            self.assertEqual(locked.json()["ok"][0]["detail"], "Alice")
            data = b"v2ok"
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "document_id": doc_id,
                    "filename": "plan.txt",
                    "mime_type": "text/plain",
                    "mode": "checkin",
                    "note": "race",
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
            with ctx.db() as archive_db:
                doc = archive_db.get(Document, doc_id)
                archive_doc_item(doc, FAKE_REQUEST, user, archive_db)
                archive_db.commit()
            completed = ctx.client.post(
                f"/api/uploads/{session['id']}/complete",
                json={"sha256": sha256_hex(data)},
                headers=headers,
            )
            self.assertEqual(completed.status_code, 400)
            self.assertEqual(completed.json()["detail"], "Restore this file before editing")

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.root_key, "archive")
                versions = db.query(DocumentVersion).filter_by(document_id=doc_id).all()
                self.assertEqual(len(versions), 1)
                self.assertEqual(doc.latest_version_number, 1)


if __name__ == "__main__":
    unittest.main()
