import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class CreateDocumentStaleStateTests(unittest.TestCase):
    def test_create_document_rechecks_duplicate_path_after_upload_read(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-stale-create-") as temp_dir:
            script = textwrap.dedent(
                """
                import asyncio

                from fastapi import HTTPException

                from app.db import SessionLocal, init_db
                from app.models import Blob, BlobLocation, Document, DocumentVersion
                from app.routers import (
                    create_document,
                    create_document_version,
                    get_or_create_blob_for_data,
                    get_or_create_folder_path,
                    now_utc,
                    storage_reconciliation_report,
                )
                from app.storage import ensure_storage, get_storage_backend


                class FakeClient:
                    host = "testclient"


                class FakeRequest:
                    headers = {}
                    client = FakeClient()


                alice = {
                    "id": "alice",
                    "name": "Alice",
                    "email": "alice@example.com",
                    "groups": [],
                    "is_admin": True,
                }
                bob = {
                    "id": "bob",
                    "name": "Bob",
                    "email": "bob@example.com",
                    "groups": [],
                    "is_admin": False,
                }


                def create_competing_document() -> None:
                    with SessionLocal() as other_db:
                        folder = get_or_create_folder_path(other_db, "Race")
                        blob = get_or_create_blob_for_data(other_db, b"winner", "text/plain")
                        doc = Document(
                            folder_id=folder.id,
                            name="race.txt",
                            created_by=bob["id"],
                            created_by_name=bob["name"],
                            latest_modified_by=bob["id"],
                            latest_modified_at=now_utc(),
                        )
                        other_db.add(doc)
                        other_db.flush()
                        create_document_version(
                            other_db,
                            doc,
                            blob,
                            bob,
                            {"ip": None, "user_agent": None},
                            "race.txt",
                            "text/plain",
                            "Uploaded race.txt",
                            "upload",
                        )
                        other_db.commit()


                class RacingUpload:
                    filename = "race.txt"
                    content_type = "text/plain"

                    async def read(self) -> bytes:
                        create_competing_document()
                        return b"loser"


                init_db()
                ensure_storage()
                with SessionLocal() as db:
                    get_or_create_folder_path(db, "Race")
                    db.commit()

                with SessionLocal() as db:
                    try:
                        asyncio.run(
                            create_document(FakeRequest(), RacingUpload(), "Race", alice, db),
                        )
                    except HTTPException as exc:
                        assert exc.status_code == 400
                        assert exc.detail == "A document already exists at that path"
                    else:
                        raise AssertionError("duplicate upload unexpectedly succeeded")
                    finally:
                        db.rollback()

                with SessionLocal() as db:
                    documents = db.query(Document).all()
                    assert [(doc.name, doc.created_by) for doc in documents] == [
                        ("race.txt", "bob")
                    ]
                    assert db.query(DocumentVersion).count() == 1
                    assert db.query(Blob).count() == 1
                    assert db.query(BlobLocation).count() == 1
                    assert len(get_storage_backend("local").list_object_keys()) == 1
                    report = storage_reconciliation_report(db, apply=False)
                    assert report["orphan_blob_ids"] == []
                    assert report["unreferenced_local_keys"] == []
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
