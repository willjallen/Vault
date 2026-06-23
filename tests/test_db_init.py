import sqlite3
import tempfile
import unittest
from pathlib import Path

from tests.support import restore_runtime, snapshot_runtime

from app import db as db_module, models


class DatabaseInitTests(unittest.TestCase):
    def test_incompatible_schema_is_not_dropped_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            with sqlite3.connect(db_path) as conn:
                conn.execute("CREATE TABLE documents (id INTEGER PRIMARY KEY, path TEXT)")
                conn.execute("INSERT INTO documents (path) VALUES ('keep-me')")

            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    row = conn.execute("SELECT path FROM documents").fetchone()
                self.assertEqual(row, ("keep-me",))
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_partial_current_schema_missing_model_column_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                self.assertEqual(models.Document.__tablename__, "documents")
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("ALTER TABLE documents DROP COLUMN current_version_id")

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    columns = {
                        row[1] for row in conn.execute("PRAGMA table_info(documents)").fetchall()
                    }
                self.assertNotIn("current_version_id", columns)
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)


if __name__ == "__main__":
    unittest.main()
