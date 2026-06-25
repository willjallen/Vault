import unittest
from contextlib import contextmanager

from fastapi import HTTPException
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
    move_doc_item,
    move_folder_item,
    move_items,
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

    def test_failed_move_into_descendant_does_not_create_destination_folders(self) -> None:
        user = user_context("alice")

        with vault_runtime() as ctx:
            with ctx.db() as db:
                source = get_or_create_folder_path(db, "Project")
                db.commit()

                with self.assertRaises(HTTPException) as raised:
                    move_folder_item(source, "Project/NewParent", user, db)

                self.assertEqual(raised.exception.status_code, 400)
                self.assertEqual(raised.exception.detail, "Cannot move a folder into itself")
                db.commit()

            with ctx.db() as db:
                self.assertIsNone(get_folder_by_path(db, "Project/NewParent"))
                folders = db.query(Folder).filter(Folder.is_root == False).all()  # noqa: E712
                self.assertEqual(
                    [(folder.root_key, folder_path(folder)) for folder in folders],
                    [("vault", "Project")],
                )

    def test_batch_move_rechecks_document_pruning_after_waiting_for_write_lock(self) -> None:
        user = user_context("alice")

        with vault_runtime():
            with SessionLocal() as db:
                project = get_or_create_folder_path(db, "Project")
                other = get_or_create_folder_path(db, "Other")
                doc = create_versioned_document(db, other, actor=user)
                db.commit()
                project_id = project.id
                doc_id = doc.id

            original_lock = routers.storage_write_lock
            state = {"moved": False}

            @contextmanager
            def move_doc_before_batch_body():
                if not state["moved"]:
                    with SessionLocal() as other_db:
                        doc = other_db.get(Document, doc_id)
                        self.assertIsNotNone(doc)
                        move_doc_item(doc, "Project", FAKE_REQUEST, user, other_db)
                        other_db.commit()
                    state["moved"] = True
                yield

            routers.storage_write_lock = move_doc_before_batch_body
            with SessionLocal() as db:
                try:
                    result = move_items(
                        ActionPayload(
                            items=[
                                ActionItem(type="folder", id=project_id),
                                ActionItem(type="document", id=doc_id),
                            ],
                            destination_folder="Dest",
                        ),
                        FAKE_REQUEST,
                        user,
                        db,
                    )
                    self.assertEqual(result["failed"], [])
                    self.assertEqual(len(result["ok"]), 1)
                    self.assertEqual(result["ok"][0]["item"]["type"], "folder")
                finally:
                    routers.storage_write_lock = original_lock
                    db.rollback()

            with SessionLocal() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(document_path(doc), "Dest/Project/plan.txt")


if __name__ == "__main__":
    unittest.main()
