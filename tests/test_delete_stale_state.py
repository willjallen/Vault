import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class DeleteStaleStateTests(unittest.TestCase):
    def run_delete_script(self, script: str) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-stale-delete-") as temp_dir:
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

    def test_stale_archive_delete_cannot_permanently_delete_restored_document(self) -> None:
        self.run_delete_script(
            """
            from fastapi import HTTPException

            from app.db import SessionLocal, init_db
            from app.models import Blob, Document, DocumentEvent, DocumentVersion
            from app.routers import (
                ActionItem,
                ActionPayload,
                archive_doc_item,
                create_document_version,
                delete_items_forever,
                get_document_or_404,
                get_or_create_blob_for_data,
                get_or_create_folder_path,
                now_utc,
                restore_doc_item,
                storage_reconciliation_report,
            )
            from app.storage import ensure_storage, get_storage_backend


            class FakeClient:
                host = "testclient"


            class FakeRequest:
                headers = {}
                client = FakeClient()


            admin = {
                "id": "alice",
                "name": "Alice",
                "email": "alice@example.com",
                "groups": ["vault-admin"],
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
                    created_by=admin["id"],
                    created_by_name=admin["name"],
                    latest_modified_by=admin["id"],
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                create_document_version(
                    db,
                    doc,
                    blob,
                    admin,
                    {"ip": None, "user_agent": None},
                    "plan.txt",
                    "text/plain",
                    "Uploaded plan.txt",
                    "upload",
                )
                db.commit()
                doc_id = doc.id
                archive_doc_item(doc, FakeRequest(), admin, db)
                db.commit()

            stale_db = SessionLocal()
            try:
                stale_doc = get_document_or_404(doc_id, stale_db)
                assert stale_doc.folder.root_key == "archive"

                with SessionLocal() as restore_db:
                    doc = restore_db.get(Document, doc_id)
                    restore_doc_item(doc, FakeRequest(), admin, restore_db)
                    restore_db.commit()

                result = delete_items_forever(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    admin,
                    stale_db,
                )
                assert result["ok"] == []
                assert result["failed"][0]["detail"] == "Move the document to Archive before deleting"
                stale_db.rollback()
            finally:
                stale_db.close()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                assert doc is not None
                assert doc.folder.root_key == "vault"
                assert doc.folder.name == "Project"
                assert db.query(DocumentVersion).filter_by(document_id=doc_id).count() == 1
                assert db.query(Blob).count() == 1
                assert len(get_storage_backend("local").list_object_keys()) == 1
                report = storage_reconciliation_report(db, apply=False)
                assert report["orphan_blob_ids"] == []
                assert report["unreferenced_local_keys"] == []
                events = db.query(DocumentEvent).filter_by(document_id=doc_id).all()
                assert [event.event_type for event in events] == ["archive", "unarchive"]
            """,
        )

    def test_stale_archive_delete_refreshes_folder_restores_with_same_folder_id(self) -> None:
        self.run_delete_script(
            """
            from fastapi import HTTPException

            from app.db import SessionLocal, init_db
            from app.models import Blob, Document, DocumentEvent, DocumentVersion
            from app.routers import (
                ActionItem,
                ActionPayload,
                archive_folder_item,
                create_document_version,
                delete_items_forever,
                get_folder_by_path,
                get_document_or_404,
                get_or_create_blob_for_data,
                get_or_create_folder_path,
                now_utc,
                restore_folder_item,
                storage_reconciliation_report,
            )
            from app.storage import ensure_storage, get_storage_backend


            class FakeClient:
                host = "testclient"


            class FakeRequest:
                headers = {}
                client = FakeClient()


            admin = {
                "id": "alice",
                "name": "Alice",
                "email": "alice@example.com",
                "groups": ["vault-admin"],
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
                    created_by=admin["id"],
                    created_by_name=admin["name"],
                    latest_modified_by=admin["id"],
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                create_document_version(
                    db,
                    doc,
                    blob,
                    admin,
                    {"ip": None, "user_agent": None},
                    "plan.txt",
                    "text/plain",
                    "Uploaded plan.txt",
                    "upload",
                )
                db.commit()
                doc_id = doc.id
                source = get_folder_by_path(db, "Project")
                archive_folder_item(source, FakeRequest(), admin, db)
                db.commit()

            stale_db = SessionLocal()
            try:
                stale_doc = get_document_or_404(doc_id, stale_db)
                assert stale_doc.folder.root_key == "archive"

                with SessionLocal() as restore_db:
                    source = get_folder_by_path(restore_db, "Archive/Project")
                    restore_folder_item(source, FakeRequest(), admin, restore_db)
                    restore_db.commit()

                result = delete_items_forever(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    admin,
                    stale_db,
                )
                assert result["ok"] == []
                assert result["failed"][0]["detail"] == "Move the document to Archive before deleting"
                stale_db.rollback()
            finally:
                stale_db.close()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                assert doc is not None
                assert doc.folder.root_key == "vault"
                assert doc.folder.name == "Project"
                assert db.query(DocumentVersion).filter_by(document_id=doc_id).count() == 1
                assert db.query(Blob).count() == 1
                assert len(get_storage_backend("local").list_object_keys()) == 1
                report = storage_reconciliation_report(db, apply=False)
                assert report["orphan_blob_ids"] == []
                assert report["unreferenced_local_keys"] == []
                events = db.query(DocumentEvent).filter_by(document_id=doc_id).all()
                assert [event.event_type for event in events] == ["archive", "unarchive"]
            """,
        )


if __name__ == "__main__":
    unittest.main()
