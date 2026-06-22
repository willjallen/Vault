import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class StorageReconciliationTests(unittest.TestCase):
    def test_apply_removes_orphan_blob_metadata_and_local_object(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-reconcile-") as temp_dir:
            script = textwrap.dedent(
                """
                from app.db import SessionLocal, init_db
                from app.models import Blob, BlobLocation, Document, DocumentVersion
                from app.routers import (
                    create_document_version,
                    get_or_create_blob_for_data,
                    get_or_create_folder_path,
                    now_utc,
                    storage_reconciliation_report,
                )
                from app.storage import ensure_storage, get_storage_backend


                user = {
                    "id": "user",
                    "name": "User",
                    "email": "user@example.com",
                    "groups": [],
                    "is_admin": True,
                }

                init_db()
                ensure_storage()
                with SessionLocal() as db:
                    folder = get_or_create_folder_path(db, "")
                    blob = get_or_create_blob_for_data(db, b"orphan me", "text/plain")
                    doc = Document(
                        folder_id=folder.id,
                        name="dead.txt",
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
                        "dead.txt",
                        "text/plain",
                        "Uploaded dead.txt",
                        "upload",
                    )
                    db.commit()

                    assert get_storage_backend("local").list_object_keys()

                    db.delete(doc)
                    db.commit()

                    before = storage_reconciliation_report(db, apply=False)
                    assert before["orphan_blob_ids"] == [blob.id]
                    assert before["unreferenced_local_keys"] == []

                    applied = storage_reconciliation_report(db, apply=True)
                    assert applied["orphan_blob_ids"] == [blob.id]
                    assert applied["deleted_local_keys"]
                    db.commit()

                    after = storage_reconciliation_report(db, apply=False)
                    assert after["orphan_blob_ids"] == []
                    assert after["unreferenced_local_keys"] == []
                    assert db.query(Blob).count() == 0
                    assert db.query(BlobLocation).count() == 0
                    assert db.query(DocumentVersion).count() == 0
                    assert get_storage_backend("local").list_object_keys() == []
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
