import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class CheckinStaleStateTests(unittest.TestCase):
    def test_checkin_rechecks_archived_state_after_upload_read(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-stale-checkin-") as temp_dir:
            script = textwrap.dedent(
                """
                import asyncio

                from fastapi import HTTPException

                from app.db import SessionLocal, init_db
                from app.models import Document, DocumentLock, DocumentVersion
                from app.routers import (
                    archive_doc_item,
                    checkin_document,
                    create_document_version,
                    get_or_create_blob_for_data,
                    get_or_create_folder_path,
                    now_utc,
                )
                from app.storage import ensure_storage


                class FakeClient:
                    host = "testclient"


                class FakeRequest:
                    headers = {}
                    client = FakeClient()


                user = {
                    "id": "alice",
                    "name": "Alice",
                    "email": "alice@example.com",
                    "groups": ["vault-users"],
                    "is_admin": True,
                }


                class ArchivingUpload:
                    filename = "plan.txt"
                    content_type = "text/plain"

                    def __init__(self, doc_id: int) -> None:
                        self.doc_id = doc_id

                    async def read(self) -> bytes:
                        with SessionLocal() as archive_db:
                            doc = archive_db.get(Document, self.doc_id)
                            archive_doc_item(doc, FakeRequest(), user, archive_db)
                            archive_db.commit()
                        return b"v2 after archive"


                init_db()
                ensure_storage()
                with SessionLocal() as db:
                    folder = get_or_create_folder_path(db, "Project")
                    blob = get_or_create_blob_for_data(db, b"v1", "text/plain")
                    doc = Document(
                        folder_id=folder.id,
                        name="plan.txt",
                        created_by=user["id"],
                        created_by_name=user["name"],
                        latest_modified_by=user["id"],
                        latest_modified_at=now_utc(),
                    )
                    db.add(doc)
                    db.flush()
                    create_document_version(
                        db,
                        doc,
                        blob,
                        user,
                        {"ip": None, "user_agent": None},
                        "plan.txt",
                        "text/plain",
                        "Uploaded plan.txt",
                        "upload",
                    )
                    db.add(
                        DocumentLock(
                            document_id=doc.id,
                            locked_by=user["id"],
                            locked_by_name=user["name"],
                        ),
                    )
                    db.commit()
                    doc_id = doc.id

                with SessionLocal() as db:
                    try:
                        asyncio.run(
                            checkin_document(
                                doc_id,
                                FakeRequest(),
                                ArchivingUpload(doc_id),
                                "race",
                                False,
                                user,
                                db,
                            ),
                        )
                    except HTTPException as exc:
                        assert exc.status_code == 400
                        assert exc.detail == "Restore this file before editing"
                    else:
                        raise AssertionError("check-in unexpectedly wrote to an archived document")

                with SessionLocal() as db:
                    doc = db.get(Document, doc_id)
                    assert doc is not None
                    assert doc.folder.root_key == "archive"
                    versions = db.query(DocumentVersion).filter_by(document_id=doc_id).all()
                    assert len(versions) == 1
                    assert doc.latest_version_number == 1
                """,
            )
            env = os.environ.copy()
            env["VAULT_DB_PATH"] = str(Path(temp_dir) / "vault.db")
            env["VAULT_OBJECTS_PATH"] = str(Path(temp_dir) / "objects")

            completed = subprocess.run(
                [sys.executable, "-c", script],
                check=False,
                cwd=Path(__file__).resolve().parents[1],
                env=env,
                stderr=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout)


if __name__ == "__main__":
    unittest.main()
