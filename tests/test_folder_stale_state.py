import unittest
from contextlib import contextmanager

from tests.support import FAKE_REQUEST, create_versioned_document, user_context, vault_runtime

import app.routers as routers
from app.db import SessionLocal
from app.models import Document, DocumentEvent, Folder
from app.routers import (
    ActionItem,
    ActionPayload,
    archive_items,
    document_path,
    folder_path,
    get_folder_by_path,
    get_or_create_folder_path,
)


class FolderStaleStateTests(unittest.TestCase):
    def test_archive_folder_rechecks_path_after_waiting_for_write_lock(self) -> None:
        user = user_context("alice")

        with vault_runtime():
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=user)
                db.commit()
                doc_id = doc.id

            original_lock = routers.storage_write_lock
            state = {"renamed": False}

            @contextmanager
            def rename_before_archive_body():
                if not state["renamed"]:
                    with SessionLocal() as other_db:
                        folder = get_folder_by_path(other_db, "Project")
                        self.assertIsNotNone(folder)
                        folder.name = "Renamed"
                        other_db.commit()
                    state["renamed"] = True
                yield

            routers.storage_write_lock = rename_before_archive_body
            with SessionLocal() as db:
                try:
                    result = archive_items(
                        ActionPayload(items=[ActionItem(type="folder", path="Project")]),
                        FAKE_REQUEST,
                        user,
                        db,
                    )
                    self.assertEqual(result["ok"], [])
                    self.assertEqual(result["failed"][0]["detail"], "Folder not found")
                finally:
                    routers.storage_write_lock = original_lock
                    db.rollback()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.folder.root_key, "vault")
                self.assertEqual(document_path(doc), "Renamed/plan.txt")
                folders = db.query(Folder).filter(Folder.is_root == False).all()  # noqa: E712
                self.assertEqual(
                    [(folder.root_key, folder_path(folder)) for folder in folders],
                    [("vault", "Renamed")],
                )
                self.assertEqual(db.query(DocumentEvent).count(), 0)


if __name__ == "__main__":
    unittest.main()
