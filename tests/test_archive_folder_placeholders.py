import unittest

from fastapi import HTTPException
from tests.support import FAKE_REQUEST, add_permission, user_context, vault_runtime

from app.models import Folder, FolderPermission, VaultGroup
from app.routers import (
    archive_folder_item,
    get_folder_by_path,
    get_or_create_folder_path,
    get_root_folder,
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

    def test_archive_folder_cannot_replace_inaccessible_empty_placeholder(self) -> None:
        writer = user_context("writer", groups=["writers"], is_admin=False)

        with vault_runtime() as ctx, ctx.db() as db:
            vault_root = get_root_folder(db, "vault")
            archive_root = get_root_folder(db, "archive")
            writers = VaultGroup(name="writers")
            confidential = VaultGroup(name="confidential")
            db.add_all([writers, confidential])
            db.flush()
            add_permission(db, vault_root, writers, write=True)
            add_permission(db, archive_root, writers, write=True)

            source = get_or_create_folder_path(db, "Project")
            placeholder = get_or_create_folder_path(db, "Archive/Project")
            add_permission(db, placeholder, confidential, write=True)
            source_id = source.id
            placeholder_id = placeholder.id
            db.commit()

            with self.assertRaises(HTTPException) as raised:
                archive_folder_item(source, FAKE_REQUEST, writer, db)

            self.assertEqual(raised.exception.status_code, 404)
            self.assertEqual(raised.exception.detail, "Folder not found")
            db.commit()

            self.assertEqual(get_folder_by_path(db, "Project").id, source_id)
            self.assertEqual(get_folder_by_path(db, "Archive/Project").id, placeholder_id)
            self.assertEqual(
                db.query(FolderPermission).filter_by(folder_id=placeholder_id).count(),
                1,
            )
            self.assertEqual(db.query(FolderPermission).filter_by(folder_id=source_id).count(), 0)

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
