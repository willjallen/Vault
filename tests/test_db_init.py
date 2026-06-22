import os
import sqlite3
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class DatabaseInitTests(unittest.TestCase):
    def test_incompatible_schema_is_not_dropped_without_explicit_reset(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            with sqlite3.connect(db_path) as conn:
                conn.execute("CREATE TABLE documents (id INTEGER PRIMARY KEY, path TEXT)")
                conn.execute("INSERT INTO documents (path) VALUES ('keep-me')")

            script = textwrap.dedent(
                """
                import sqlite3

                from app.db import init_db

                try:
                    init_db()
                except RuntimeError as exc:
                    assert "Refusing to reset metadata automatically" in str(exc)
                else:
                    raise AssertionError("init_db unexpectedly accepted an incompatible schema")

                with sqlite3.connect(r"{db_path}") as conn:
                    row = conn.execute("SELECT path FROM documents").fetchone()
                assert row == ("keep-me",)
                """,
            ).format(db_path=db_path)
            env = os.environ.copy()
            env["VAULT_DB_PATH"] = str(db_path)
            env.pop("VAULT_RESET_DB_ON_START", None)

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

    def test_partial_current_schema_missing_model_column_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            script = textwrap.dedent(
                """
                import sqlite3

                from app import models
                from app.db import Base, engine, init_db

                Base.metadata.create_all(bind=engine)
                with sqlite3.connect(r"{db_path}") as conn:
                    conn.execute("ALTER TABLE documents DROP COLUMN current_version_id")

                try:
                    init_db()
                except RuntimeError as exc:
                    assert "Refusing to reset metadata automatically" in str(exc)
                else:
                    raise AssertionError("init_db unexpectedly accepted a partial schema")

                with sqlite3.connect(r"{db_path}") as conn:
                    columns = {{
                        row[1]
                        for row in conn.execute("PRAGMA table_info(documents)").fetchall()
                    }}
                assert "current_version_id" not in columns
                """,
            ).format(db_path=db_path)
            env = os.environ.copy()
            env["VAULT_DB_PATH"] = str(db_path)
            env.pop("VAULT_RESET_DB_ON_START", None)

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
