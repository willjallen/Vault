import unittest

from fastapi import HTTPException
from tests.support import FAKE_REQUEST, user_context, vault_runtime

from app.models import Folder
from app.routers import (
    archive_folder_item,
    get_folder_by_path,
    get_or_create_folder_path,
    restore_folder_item,
)


class ArchiveFolderPlaceholderTests(unittest.TestCase):
    def test_archive_folder_reuses_empty_archive_placeholder(self) -> None:
        user = user_context("user")

        with vault_runtime() as ctx, ctx.db() as db:
            get_or_create_folder_path(db, "Project")
            get_or_create_folder_path(db, "Archive/Project")
            db.commit()

            source = get_folder_by_path(db, "Project")
            result = archive_folder_item(source, FAKE_REQUEST, user, db)
            db.commit()
            self.assertEqual(result, "Archive/Project")

            rows = db.query(Folder).filter_by(name="Project").all()
            self.assertEqual(len(rows), 1)
            self.assertEqual(rows[0].root_key, "archive")
            self.assertIsNotNone(rows[0].parent)
            self.assertTrue(rows[0].parent.is_root)

    def test_archive_folder_keeps_nonempty_archive_target_as_conflict(self) -> None:
        user = user_context("user")

        with vault_runtime() as ctx, ctx.db() as db:
            get_or_create_folder_path(db, "Project")
            get_or_create_folder_path(db, "Archive/Project/Existing")
            db.commit()

            with self.assertRaises(HTTPException) as raised:
                source = get_folder_by_path(db, "Project")
                archive_folder_item(source, FAKE_REQUEST, user, db)

            self.assertEqual(raised.exception.status_code, 400)
            self.assertEqual(raised.exception.detail, "A folder already exists at that path")

            rows = db.query(Folder).filter_by(name="Project").all()
            self.assertEqual(sorted(row.root_key for row in rows), ["archive", "vault"])

    def test_unarchive_folder_reuses_empty_vault_placeholder(self) -> None:
        user = user_context("user")

        with vault_runtime() as ctx, ctx.db() as db:
            get_or_create_folder_path(db, "Project")
            db.commit()

            source = get_folder_by_path(db, "Project")
            archive_folder_item(source, FAKE_REQUEST, user, db)
            db.commit()
            get_or_create_folder_path(db, "Project")
            db.commit()

            source = get_folder_by_path(db, "Archive/Project")
            result = restore_folder_item(source, FAKE_REQUEST, user, db)
            db.commit()
            self.assertEqual(result, "Project")

            rows = db.query(Folder).filter_by(name="Project").all()
            self.assertEqual(len(rows), 1)
            self.assertEqual(rows[0].root_key, "vault")
            self.assertIsNotNone(rows[0].parent)
            self.assertTrue(rows[0].parent.is_root)

    def test_unarchive_folder_keeps_nonempty_vault_target_as_conflict(self) -> None:
        user = user_context("user")

        with vault_runtime() as ctx, ctx.db() as db:
            get_or_create_folder_path(db, "Project")
            db.commit()

            source = get_folder_by_path(db, "Project")
            archive_folder_item(source, FAKE_REQUEST, user, db)
            db.commit()
            get_or_create_folder_path(db, "Project/Existing")
            db.commit()

            with self.assertRaises(HTTPException) as raised:
                source = get_folder_by_path(db, "Archive/Project")
                restore_folder_item(source, FAKE_REQUEST, user, db)

            self.assertEqual(raised.exception.status_code, 400)
            self.assertEqual(raised.exception.detail, "A folder already exists at that path")

            rows = db.query(Folder).filter_by(name="Project").all()
            self.assertEqual(sorted(row.root_key for row in rows), ["archive", "vault"])


if __name__ == "__main__":
    unittest.main()
