import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class RetentionTtlTests(unittest.TestCase):
    def run_retention_script(self, script: str) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-retention-") as temp_dir:
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

    def test_expired_document_is_archived_to_matching_archive_folder(self) -> None:
        self.run_retention_script(
            """
            import datetime as dt

            from app.db import SessionLocal, init_db
            from app.models import Document, StateEvent
            from app.routers import (
                apply_folder_ttl,
                folder_path,
                get_or_create_folder_path,
                normalize_timestamp,
                now_utc,
                restore_doc_item,
                sweep_expired_documents,
            )


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
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                folder.default_ttl_days = 30
                folder.default_ttl_action = "archive"
                doc = Document(folder_id=folder.id, name="plan.txt", latest_modified_at=now_utc())
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, now_utc() - dt.timedelta(days=31))
                assert doc.expires_at <= now_utc()
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            assert result["archived"] == ["Archive/Project/plan.txt"]
            assert result["deleted"] == []

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                assert doc is not None
                assert folder_path(doc.folder) == "Archive/Project"
                assert doc.expires_at is None
                assert doc.expiry_action is None
                event = db.query(StateEvent).filter_by(event_type="retention.expired").one()
                assert event.payload["resources"] == [
                    "contents",
                    "document_detail",
                    "my_edits",
                    "sidebar",
                ]

                restore_doc_item(doc, FakeRequest(), admin, db)
                db.commit()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                assert doc is not None
                assert folder_path(doc.folder) == "Project"
                assert doc.expiry_action == "archive"
                assert doc.expires_at is not None
                assert normalize_timestamp(doc.expires_at) > now_utc() + dt.timedelta(days=29)
            """,
        )

    def test_expired_document_can_be_deleted_without_archive_first(self) -> None:
        self.run_retention_script(
            """
            import datetime as dt

            from app.db import SessionLocal, init_db
            from app.models import Document
            from app.routers import (
                apply_folder_ttl,
                get_or_create_folder_path,
                now_utc,
                sweep_expired_documents,
            )


            init_db()
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Temp")
                folder.default_ttl_days = 1
                folder.default_ttl_action = "delete"
                doc = Document(
                    folder_id=folder.id,
                    name="scratch.txt",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, now_utc() - dt.timedelta(days=2))
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            assert result["archived"] == []
            assert result["deleted"] == ["Temp/scratch.txt"]

            with SessionLocal() as db:
                assert db.get(Document, doc_id) is None
            """,
        )

    def test_locked_expired_document_is_skipped(self) -> None:
        self.run_retention_script(
            """
            import datetime as dt

            from app.db import SessionLocal, init_db
            from app.models import Document, DocumentLock
            from app.routers import (
                apply_folder_ttl,
                get_or_create_folder_path,
                now_utc,
                sweep_expired_documents,
            )


            init_db()
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Working")
                folder.default_ttl_days = 1
                folder.default_ttl_action = "delete"
                doc = Document(
                    folder_id=folder.id,
                    name="locked.txt",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, now_utc() - dt.timedelta(days=2))
                db.add(DocumentLock(document_id=doc.id, locked_by="user", is_active=True))
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            assert result["skipped"] == ["Working/locked.txt"]
            assert result["deleted"] == []

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                assert doc is not None
                assert doc.expires_at is not None
                assert doc.expiry_action == "delete"
            """,
        )

    def test_plain_folders_do_not_compute_delete_ttl_for_old_documents(self) -> None:
        self.run_retention_script(
            """
            import datetime as dt

            from app.db import SessionLocal, init_db
            from app.models import Document, StateEvent
            from app.routers import get_or_create_folder_path, now_utc, sweep_expired_documents


            init_db()
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Safe")
                doc = Document(
                    folder_id=folder.id,
                    name="old-but-safe.txt",
                    latest_modified_at=now_utc() - dt.timedelta(days=365),
                )
                db.add(doc)
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            assert result == {"archived": [], "deleted": [], "skipped": []}

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                assert doc is not None
                assert doc.expires_at is None
                assert doc.expiry_action is None
                assert db.query(StateEvent).filter_by(event_type="retention.expired").count() == 0
            """,
        )

    def test_child_folder_without_ttl_does_not_inherit_parent_delete_ttl(self) -> None:
        self.run_retention_script(
            """
            import datetime as dt

            from app.db import SessionLocal, init_db
            from app.models import Document
            from app.routers import (
                apply_folder_ttl,
                get_or_create_folder_path,
                now_utc,
                sweep_expired_documents,
            )


            init_db()
            with SessionLocal() as db:
                parent = get_or_create_folder_path(db, "Temp")
                parent.default_ttl_days = 1
                parent.default_ttl_action = "delete"
                child = get_or_create_folder_path(db, "Temp/Keep")
                doc = Document(
                    folder_id=child.id,
                    name="child-safe.txt",
                    latest_modified_at=now_utc() - dt.timedelta(days=30),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, child, now_utc() - dt.timedelta(days=30))
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            assert result["deleted"] == []

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                assert doc is not None
                assert doc.expires_at is None
                assert doc.expiry_action is None
            """,
        )

    def test_moving_from_delete_ttl_folder_to_plain_folder_clears_delete_expiry(self) -> None:
        self.run_retention_script(
            """
            import datetime as dt

            from app.db import SessionLocal, init_db
            from app.models import Document
            from app.routers import (
                apply_folder_ttl,
                get_or_create_folder_path,
                move_doc_item,
                now_utc,
                sweep_expired_documents,
            )


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
            with SessionLocal() as db:
                source = get_or_create_folder_path(db, "Temp")
                source.default_ttl_days = 1
                source.default_ttl_action = "delete"
                get_or_create_folder_path(db, "Safe")
                doc = Document(folder_id=source.id, name="rescue.txt", latest_modified_at=now_utc())
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, source, now_utc() - dt.timedelta(days=2))
                assert doc.expiry_action == "delete"
                move_doc_item(doc, "Safe", FakeRequest(), admin, db)
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            assert result["deleted"] == []

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                assert doc is not None
                assert doc.expires_at is None
                assert doc.expiry_action is None
                assert doc.folder.name == "Safe"
            """,
        )


if __name__ == "__main__":
    unittest.main()
