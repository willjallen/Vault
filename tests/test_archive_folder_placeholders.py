import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class ArchiveFolderPlaceholderTests(unittest.TestCase):
    def run_archive_script(self, script: str) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-archive-placeholder-") as temp_dir:
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

    def test_archive_folder_reuses_empty_archive_placeholder(self) -> None:
        self.run_archive_script(
            """
            from app.db import SessionLocal, init_db
            from app.models import Folder
            from app.routers import archive_folder_item, get_folder_by_path, get_or_create_folder_path


            class FakeClient:
                host = "testclient"


            class FakeRequest:
                headers = {}
                client = FakeClient()


            init_db()
            user = {
                "id": "user",
                "name": "User",
                "email": "user@example.com",
                "groups": ["vault-users"],
                "is_admin": True,
            }

            with SessionLocal() as db:
                get_or_create_folder_path(db, "Project")
                get_or_create_folder_path(db, "Archive/Project")
                db.commit()

                source = get_folder_by_path(db, "Project")
                result = archive_folder_item(source, FakeRequest(), user, db)
                db.commit()
                assert result == "Archive/Project"

                rows = db.query(Folder).filter_by(name="Project").all()
                assert len(rows) == 1
                assert rows[0].root_key == "archive"
                assert rows[0].parent is not None
                assert rows[0].parent.is_root
            """,
        )

    def test_archive_folder_keeps_nonempty_archive_target_as_conflict(self) -> None:
        self.run_archive_script(
            """
            from fastapi import HTTPException

            from app.db import SessionLocal, init_db
            from app.models import Folder
            from app.routers import archive_folder_item, get_folder_by_path, get_or_create_folder_path


            class FakeClient:
                host = "testclient"


            class FakeRequest:
                headers = {}
                client = FakeClient()


            init_db()
            user = {
                "id": "user",
                "name": "User",
                "email": "user@example.com",
                "groups": ["vault-users"],
                "is_admin": True,
            }

            with SessionLocal() as db:
                get_or_create_folder_path(db, "Project")
                get_or_create_folder_path(db, "Archive/Project/Existing")
                db.commit()

                try:
                    source = get_folder_by_path(db, "Project")
                    archive_folder_item(source, FakeRequest(), user, db)
                except HTTPException as exc:
                    assert exc.status_code == 400
                    assert exc.detail == "A folder already exists at that path"
                else:
                    raise AssertionError("archive unexpectedly replaced a non-empty target")

                rows = db.query(Folder).filter_by(name="Project").all()
                assert sorted(row.root_key for row in rows) == ["archive", "vault"]
            """,
        )

    def test_unarchive_folder_reuses_empty_vault_placeholder(self) -> None:
        self.run_archive_script(
            """
            from app.db import SessionLocal, init_db
            from app.models import Folder
            from app.routers import archive_folder_item, get_folder_by_path, get_or_create_folder_path, restore_folder_item


            class FakeClient:
                host = "testclient"


            class FakeRequest:
                headers = {}
                client = FakeClient()


            init_db()
            user = {
                "id": "user",
                "name": "User",
                "email": "user@example.com",
                "groups": ["vault-users"],
                "is_admin": True,
            }

            with SessionLocal() as db:
                get_or_create_folder_path(db, "Project")
                db.commit()

                source = get_folder_by_path(db, "Project")
                archive_folder_item(source, FakeRequest(), user, db)
                db.commit()
                get_or_create_folder_path(db, "Project")
                db.commit()

                source = get_folder_by_path(db, "Archive/Project")
                result = restore_folder_item(source, FakeRequest(), user, db)
                db.commit()
                assert result == "Project"

                rows = db.query(Folder).filter_by(name="Project").all()
                assert len(rows) == 1
                assert rows[0].root_key == "vault"
                assert rows[0].parent is not None
                assert rows[0].parent.is_root
            """,
        )

    def test_unarchive_folder_keeps_nonempty_vault_target_as_conflict(self) -> None:
        self.run_archive_script(
            """
            from fastapi import HTTPException

            from app.db import SessionLocal, init_db
            from app.models import Folder
            from app.routers import archive_folder_item, get_folder_by_path, get_or_create_folder_path, restore_folder_item


            class FakeClient:
                host = "testclient"


            class FakeRequest:
                headers = {}
                client = FakeClient()


            init_db()
            user = {
                "id": "user",
                "name": "User",
                "email": "user@example.com",
                "groups": ["vault-users"],
                "is_admin": True,
            }

            with SessionLocal() as db:
                get_or_create_folder_path(db, "Project")
                db.commit()

                source = get_folder_by_path(db, "Project")
                archive_folder_item(source, FakeRequest(), user, db)
                db.commit()
                get_or_create_folder_path(db, "Project/Existing")
                db.commit()

                try:
                    source = get_folder_by_path(db, "Archive/Project")
                    restore_folder_item(source, FakeRequest(), user, db)
                except HTTPException as exc:
                    assert exc.status_code == 400
                    assert exc.detail == "A folder already exists at that path"
                else:
                    raise AssertionError("restore unexpectedly replaced a non-empty target")

                rows = db.query(Folder).filter_by(name="Project").all()
                assert sorted(row.root_key for row in rows) == ["archive", "vault"]
            """,
        )


if __name__ == "__main__":
    unittest.main()
