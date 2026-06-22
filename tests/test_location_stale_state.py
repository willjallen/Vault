import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class LocationStaleStateTests(unittest.TestCase):
    def run_location_script(self, script: str) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-stale-location-") as temp_dir:
            env = os.environ.copy()
            env["VAULT_DB_PATH"] = str(Path(temp_dir) / "vault.db")
            env["VAULT_OBJECTS_PATH"] = str(Path(temp_dir) / "objects")

            completed = subprocess.run(
                [sys.executable, "-c", textwrap.dedent(script)],
                check=False,
                cwd=Path(__file__).resolve().parents[1],
                env=env,
                stderr=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout)

    def test_stale_move_cannot_restore_archived_document_as_plain_move(self) -> None:
        self.run_location_script(
            """
            from fastapi import HTTPException

            from app.db import SessionLocal, init_db
            from app.models import Document, DocumentEvent
            from app.routers import (
                archive_doc_item,
                create_document_version,
                get_folder_by_path,
                get_document_or_404,
                get_or_create_blob_for_data,
                get_or_create_folder_path,
                move_doc_item,
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

            stale_db = SessionLocal()
            try:
                stale_doc = get_document_or_404(doc_id, stale_db)
                with SessionLocal() as archive_db:
                    doc = archive_db.get(Document, doc_id)
                    archive_doc_item(doc, FakeRequest(), user, archive_db)
                    archive_db.commit()

                try:
                    move_doc_item(stale_doc, "Other", FakeRequest(), user, stale_db, name="plan.txt")
                except HTTPException as exc:
                    assert exc.status_code == 400
                    assert exc.detail == "Use archive or restore for Archive moves"
                else:
                    raise AssertionError("stale move unexpectedly restored the archived document")
            finally:
                stale_db.close()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                assert doc is not None
                assert doc.folder.root_key == "archive"
                assert doc.folder.name == "Project"
                assert get_folder_by_path(db, "Other") is None
                events = db.query(DocumentEvent).filter_by(document_id=doc_id).all()
                assert [event.event_type for event in events] == ["archive"]
            """,
        )

    def test_stale_archive_does_not_record_duplicate_archive_transition(self) -> None:
        self.run_location_script(
            """
            from fastapi import HTTPException

            from app.db import SessionLocal, init_db
            from app.models import Document, DocumentEvent
            from app.routers import (
                archive_doc_item,
                create_document_version,
                get_document_or_404,
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

            stale_db = SessionLocal()
            try:
                stale_doc = get_document_or_404(doc_id, stale_db)
                with SessionLocal() as archive_db:
                    doc = archive_db.get(Document, doc_id)
                    archive_doc_item(doc, FakeRequest(), user, archive_db)
                    archive_db.commit()

                try:
                    archive_doc_item(stale_doc, FakeRequest(), user, stale_db)
                except HTTPException as exc:
                    assert exc.status_code == 400
                    assert exc.detail == "Document is already archived"
                else:
                    raise AssertionError("stale archive unexpectedly succeeded twice")
            finally:
                stale_db.close()

            with SessionLocal() as db:
                events = db.query(DocumentEvent).filter_by(document_id=doc_id).all()
                assert [event.event_type for event in events] == ["archive"]
            """,
        )


if __name__ == "__main__":
    unittest.main()
