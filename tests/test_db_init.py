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

    def test_unexpected_model_column_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("PRAGMA foreign_keys=OFF")
                    conn.execute("DROP TABLE vault_groups")
                    conn.execute(
                        """
                        CREATE TABLE vault_groups (
                            id INTEGER NOT NULL,
                            name VARCHAR NOT NULL,
                            description TEXT,
                            created_at DATETIME NOT NULL,
                            legacy_required TEXT NOT NULL,
                            PRIMARY KEY (id),
                            CONSTRAINT uq_vault_groups_name UNIQUE (name)
                        )
                        """,
                    )
                    conn.execute("CREATE INDEX ix_vault_groups_id ON vault_groups (id)")

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    columns = {
                        row[1] for row in conn.execute("PRAGMA table_info(vault_groups)").fetchall()
                    }
                self.assertIn("legacy_required", columns)
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_missing_user_preferences_column_is_added_without_dropping_data(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("ALTER TABLE vault_users DROP COLUMN preferences")
                    conn.execute(
                        """
                        INSERT INTO vault_users
                            (issuer, subject, email, name, is_admin, is_active, created_at)
                        VALUES
                            ('test', 'alice', 'alice@example.com', 'Alice', 0, 1, CURRENT_TIMESTAMP)
                        """,
                    )

                db_module.init_db()

                with sqlite3.connect(db_path) as conn:
                    columns = {
                        row[1] for row in conn.execute("PRAGMA table_info(vault_users)").fetchall()
                    }
                    row = conn.execute(
                        "SELECT subject, preferences FROM vault_users WHERE subject = 'alice'",
                    ).fetchone()
                self.assertIn("preferences", columns)
                self.assertEqual(row, ("alice", "{}"))
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_missing_vault_settings_table_is_added_without_dropping_data(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("DROP TABLE vault_settings")
                    conn.execute(
                        """
                        INSERT INTO vault_users
                            (
                                issuer,
                                subject,
                                email,
                                name,
                                is_admin,
                                is_active,
                                preferences,
                                created_at
                            )
                        VALUES
                            (
                                'test',
                                'alice',
                                'alice@example.com',
                                'Alice',
                                0,
                                1,
                                '{}',
                                CURRENT_TIMESTAMP
                            )
                        """,
                    )

                db_module.init_db()

                with sqlite3.connect(db_path) as conn:
                    tables = {
                        row[0]
                        for row in conn.execute(
                            "SELECT name FROM sqlite_master WHERE type = 'table'",
                        ).fetchall()
                    }
                    row = conn.execute(
                        "SELECT subject FROM vault_users WHERE subject = 'alice'",
                    ).fetchone()
                self.assertIn("vault_settings", tables)
                self.assertEqual(row, ("alice",))
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_missing_upload_verification_columns_are_added_without_dropping_data(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("ALTER TABLE upload_sessions DROP COLUMN verification_total_bytes")
                    conn.execute(
                        "ALTER TABLE upload_sessions DROP COLUMN verification_processed_bytes",
                    )

                db_module.init_db()

                with sqlite3.connect(db_path) as conn:
                    columns = {
                        row[1]
                        for row in conn.execute("PRAGMA table_info(upload_sessions)").fetchall()
                    }
                self.assertIn("verification_total_bytes", columns)
                self.assertIn("verification_processed_bytes", columns)
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_missing_model_index_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("DROP INDEX uq_document_locks_active_document")

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    indexes = {
                        row[0]
                        for row in conn.execute(
                            "SELECT name FROM sqlite_master WHERE type = 'index'",
                        ).fetchall()
                    }
                self.assertNotIn("uq_document_locks_active_document", indexes)
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_wrong_model_index_definition_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("DROP INDEX uq_document_locks_active_document")
                    conn.execute(
                        """
                        CREATE INDEX uq_document_locks_active_document
                        ON document_locks (document_id)
                        """,
                    )

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    indexes = {
                        row[1]: row[2]
                        for row in conn.execute("PRAGMA index_list(document_locks)").fetchall()
                    }
                self.assertEqual(indexes["uq_document_locks_active_document"], 0)
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_unexpected_unique_index_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute(
                        """
                        CREATE UNIQUE INDEX uq_documents_global_name
                        ON documents (name)
                        """,
                    )

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    indexes = {
                        row[1]: row[2]
                        for row in conn.execute("PRAGMA index_list(documents)").fetchall()
                    }
                self.assertEqual(indexes["uq_documents_global_name"], 1)
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_missing_unique_constraint_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("PRAGMA foreign_keys=OFF")
                    conn.execute("DROP TABLE vault_groups")
                    conn.execute(
                        """
                        CREATE TABLE vault_groups (
                            id INTEGER NOT NULL,
                            name VARCHAR NOT NULL,
                            description TEXT,
                            created_at DATETIME NOT NULL,
                            PRIMARY KEY (id)
                        )
                        """,
                    )
                    conn.execute("CREATE INDEX ix_vault_groups_id ON vault_groups (id)")

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    constraints = conn.execute(
                        "PRAGMA index_list(vault_groups)",
                    ).fetchall()
                self.assertFalse(
                    any(row[1].startswith("sqlite_autoindex_vault_groups") for row in constraints),
                )
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_wrong_unique_constraint_definition_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("PRAGMA foreign_keys=OFF")
                    conn.execute("DROP TABLE vault_groups")
                    conn.execute(
                        """
                        CREATE TABLE vault_groups (
                            id INTEGER NOT NULL,
                            name VARCHAR NOT NULL,
                            description TEXT,
                            created_at DATETIME NOT NULL,
                            PRIMARY KEY (id),
                            CONSTRAINT uq_vault_groups_name UNIQUE (id)
                        )
                        """,
                    )
                    conn.execute("CREATE INDEX ix_vault_groups_id ON vault_groups (id)")

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    constraints = conn.execute("PRAGMA index_list(vault_groups)").fetchall()
                    wrong_unique = [
                        row for row in constraints if row[1].startswith("sqlite_autoindex")
                    ]
                    unique_columns = [
                        column[2]
                        for column in conn.execute(
                            f"PRAGMA index_info({wrong_unique[0][1]})",
                        ).fetchall()
                    ]
                self.assertEqual(unique_columns, ["id"])
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_missing_primary_key_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("PRAGMA foreign_keys=OFF")
                    conn.execute("DROP TABLE vault_groups")
                    conn.execute(
                        """
                        CREATE TABLE vault_groups (
                            id INTEGER NOT NULL,
                            name VARCHAR NOT NULL,
                            description TEXT,
                            created_at DATETIME NOT NULL,
                            CONSTRAINT uq_vault_groups_name UNIQUE (name)
                        )
                        """,
                    )
                    conn.execute("CREATE INDEX ix_vault_groups_id ON vault_groups (id)")

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    primary_key = conn.execute("PRAGMA table_info(vault_groups)").fetchall()
                self.assertFalse(any(row[5] for row in primary_key))
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_missing_foreign_key_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("PRAGMA foreign_keys=OFF")
                    conn.execute("DROP TABLE folder_events")
                    conn.execute(
                        """
                        CREATE TABLE folder_events (
                            id INTEGER NOT NULL,
                            folder_id INTEGER NOT NULL,
                            event_type VARCHAR NOT NULL,
                            actor VARCHAR,
                            actor_name VARCHAR,
                            message TEXT,
                            created_at DATETIME NOT NULL,
                            PRIMARY KEY (id)
                        )
                        """,
                    )
                    conn.execute("CREATE INDEX ix_folder_events_id ON folder_events (id)")
                    conn.execute(
                        "CREATE INDEX ix_folder_events_folder_id ON folder_events (folder_id)",
                    )

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    foreign_keys = conn.execute("PRAGMA foreign_key_list(folder_events)").fetchall()
                self.assertEqual(foreign_keys, [])
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_unexpected_foreign_key_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("PRAGMA foreign_keys=OFF")
                    conn.execute("DROP TABLE vault_groups")
                    conn.execute(
                        """
                        CREATE TABLE vault_groups (
                            id INTEGER NOT NULL,
                            name VARCHAR NOT NULL,
                            description TEXT,
                            created_at DATETIME NOT NULL,
                            PRIMARY KEY (id),
                            CONSTRAINT uq_vault_groups_name UNIQUE (name),
                            FOREIGN KEY(id) REFERENCES folders(id) ON DELETE CASCADE
                        )
                        """,
                    )
                    conn.execute("CREATE INDEX ix_vault_groups_id ON vault_groups (id)")

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    foreign_keys = conn.execute("PRAGMA foreign_key_list(vault_groups)").fetchall()
                self.assertEqual(len(foreign_keys), 1)
                self.assertEqual(foreign_keys[0][2], "folders")
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_nullable_required_column_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("PRAGMA foreign_keys=OFF")
                    conn.execute("DROP TABLE folder_events")
                    conn.execute(
                        """
                        CREATE TABLE folder_events (
                            id INTEGER NOT NULL,
                            folder_id INTEGER,
                            event_type VARCHAR NOT NULL,
                            actor VARCHAR,
                            actor_name VARCHAR,
                            message TEXT,
                            created_at DATETIME NOT NULL,
                            PRIMARY KEY (id),
                            FOREIGN KEY(folder_id) REFERENCES folders(id) ON DELETE CASCADE
                        )
                        """,
                    )
                    conn.execute("CREATE INDEX ix_folder_events_id ON folder_events (id)")
                    conn.execute(
                        "CREATE INDEX ix_folder_events_folder_id ON folder_events (folder_id)",
                    )

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    columns = {
                        row[1]: row[3]
                        for row in conn.execute("PRAGMA table_info(folder_events)").fetchall()
                    }
                self.assertEqual(columns["folder_id"], 0)
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_wrong_column_type_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("PRAGMA foreign_keys=OFF")
                    conn.execute("DROP TABLE folder_events")
                    conn.execute(
                        """
                        CREATE TABLE folder_events (
                            id INTEGER NOT NULL,
                            folder_id TEXT NOT NULL,
                            event_type VARCHAR NOT NULL,
                            actor VARCHAR,
                            actor_name VARCHAR,
                            message TEXT,
                            created_at DATETIME NOT NULL,
                            PRIMARY KEY (id),
                            FOREIGN KEY(folder_id) REFERENCES folders(id) ON DELETE CASCADE
                        )
                        """,
                    )
                    conn.execute("CREATE INDEX ix_folder_events_id ON folder_events (id)")
                    conn.execute(
                        "CREATE INDEX ix_folder_events_folder_id ON folder_events (folder_id)",
                    )

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    columns = {
                        row[1]: row[2]
                        for row in conn.execute("PRAGMA table_info(folder_events)").fetchall()
                    }
                self.assertEqual(columns["folder_id"], "TEXT")
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_unexpected_check_constraint_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute("PRAGMA foreign_keys=OFF")
                    conn.execute("DROP TABLE vault_groups")
                    conn.execute(
                        """
                        CREATE TABLE vault_groups (
                            id INTEGER NOT NULL,
                            name VARCHAR NOT NULL,
                            description TEXT,
                            created_at DATETIME NOT NULL,
                            PRIMARY KEY (id),
                            CONSTRAINT uq_vault_groups_name UNIQUE (name),
                            CONSTRAINT ck_vault_groups_not_blocked CHECK (name != 'blocked')
                        )
                        """,
                    )
                    conn.execute("CREATE INDEX ix_vault_groups_id ON vault_groups (id)")

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    create_sql = conn.execute(
                        """
                        SELECT sql FROM sqlite_master
                        WHERE type = 'table' AND name = 'vault_groups'
                        """,
                    ).fetchone()[0]
                self.assertIn("ck_vault_groups_not_blocked", create_sql)
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)

    def test_unexpected_trigger_is_rejected_on_startup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-db-init-") as temp_dir:
            db_path = Path(temp_dir) / "vault.db"
            snapshot = snapshot_runtime()
            try:
                db_module.configure_database(db_path)
                db_module.Base.metadata.create_all(bind=db_module.engine)
                with sqlite3.connect(db_path) as conn:
                    conn.execute(
                        """
                        CREATE TRIGGER vault_groups_delete_documents
                        AFTER INSERT ON vault_groups
                        BEGIN
                            DELETE FROM documents;
                        END
                        """,
                    )

                with self.assertRaises(RuntimeError) as raised:
                    db_module.init_db()

                self.assertIn("Startup refused to alter or drop", str(raised.exception))
                with sqlite3.connect(db_path) as conn:
                    trigger = conn.execute(
                        """
                        SELECT name FROM sqlite_master
                        WHERE type = 'trigger' AND name = 'vault_groups_delete_documents'
                        """,
                    ).fetchone()
                self.assertEqual(trigger, ("vault_groups_delete_documents",))
            finally:
                db_module.engine.dispose()
                restore_runtime(snapshot)


if __name__ == "__main__":
    unittest.main()
