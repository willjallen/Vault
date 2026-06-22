import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class FolderStaleStateTests(unittest.TestCase):
    def test_archive_folder_rechecks_path_after_waiting_for_write_lock(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-stale-folder-") as temp_dir:
            script = textwrap.dedent(
                """
                from contextlib import contextmanager

                from fastapi import HTTPException

                from app.db import SessionLocal, init_db
                from app.models import Document, DocumentEvent, Folder
                import app.routers as routers
                from app.routers import (
                    archive_folder,
                    create_document_version,
                    document_path,
                    folder_path,
                    get_folder_by_path,
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
                    "is_admin": False,
                }


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
                    db.commit()
                    doc_id = doc.id

                original_lock = routers.storage_write_lock
                state = {"renamed": False}

                @contextmanager
                def rename_before_archive_body():
                    if not state["renamed"]:
                        with SessionLocal() as other_db:
                            folder = get_folder_by_path(other_db, "Project")
                            assert folder is not None
                            folder.name = "Renamed"
                            other_db.commit()
                        state["renamed"] = True
                    yield

                routers.storage_write_lock = rename_before_archive_body
                with SessionLocal() as db:
                    try:
                        try:
                            archive_folder(FakeRequest(), "Project", user, db)
                        except HTTPException as exc:
                            assert exc.status_code == 404
                            assert exc.detail == "Folder not found"
                        else:
                            raise AssertionError("stale folder archive unexpectedly succeeded")
                    finally:
                        routers.storage_write_lock = original_lock
                        db.rollback()

                with SessionLocal() as db:
                    doc = db.get(Document, doc_id)
                    assert doc is not None
                    assert doc.folder.root_key == "vault"
                    assert document_path(doc) == "Renamed/plan.txt"
                    folders = db.query(Folder).filter(Folder.is_root == False).all()  # noqa: E712
                    assert [(folder.root_key, folder_path(folder)) for folder in folders] == [
                        ("vault", "Renamed")
                    ]
                    assert db.query(DocumentEvent).count() == 0
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
