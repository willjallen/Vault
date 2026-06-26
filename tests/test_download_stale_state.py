import unittest
from unittest.mock import patch

from fastapi import HTTPException
from tests.support import (
    FAKE_REQUEST,
    add_permission,
    create_versioned_document,
    user_context,
    vault_runtime,
)

import app.routers as routers
from app.db import SessionLocal
from app.models import Document, VaultGroup
from app.routers import (
    ActionItem,
    ActionPayload,
    download_items,
    download_version,
    get_folder_by_path,
    get_or_create_folder_path,
    get_root_folder,
)


class DownloadStaleStateTests(unittest.TestCase):
    def _create_hidden_move_fixture(self) -> tuple[int, str, dict[str, object]]:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        reader = user_context("reader", groups=["readers"], is_admin=False)

        with SessionLocal() as db:
            root = get_root_folder(db, "vault")
            readers = VaultGroup(name="readers")
            confidential = VaultGroup(name="confidential")
            db.add_all([readers, confidential])
            db.flush()
            add_permission(db, root, readers)

            project = get_or_create_folder_path(db, "Project")
            secret = get_or_create_folder_path(db, "Secret")
            add_permission(db, secret, confidential, write=True)
            doc = create_versioned_document(
                db,
                project,
                actor=admin,
                name="plan.txt",
                data=b"secret",
            )
            db.commit()
            return doc.id, doc.current_version_id or "", reader

    def _move_document_under_hidden_acl(self, doc_id: int) -> None:
        with SessionLocal() as db:
            doc = db.get(Document, doc_id)
            secret = get_folder_by_path(db, "Secret")
            self.assertIsNotNone(doc)
            self.assertIsNotNone(secret)
            doc.folder = secret
            doc.folder_id = secret.id
            db.commit()

    def test_direct_download_rechecks_access_after_blob_read(self) -> None:
        with vault_runtime():
            doc_id, _version_id, reader = self._create_hidden_move_fixture()
            original_copy = routers.copy_version_to_temp
            moved = False

            def move_before_copy(version):
                nonlocal moved
                if not moved:
                    moved = True
                    self._move_document_under_hidden_acl(doc_id)
                return original_copy(version)

            with SessionLocal() as db:
                with patch.object(routers, "copy_version_to_temp", side_effect=move_before_copy):
                    with self.assertRaises(HTTPException) as raised:
                        download_items(
                            ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                            FAKE_REQUEST,
                            reader,
                            db,
                        )

            self.assertTrue(moved)
            self.assertEqual(raised.exception.status_code, 404)
            self.assertEqual(raised.exception.detail, "Document not found")

    def test_version_download_rechecks_access_after_blob_read(self) -> None:
        with vault_runtime():
            doc_id, version_id, reader = self._create_hidden_move_fixture()
            original_copy = routers.copy_version_to_temp
            moved = False

            def move_before_copy(version):
                nonlocal moved
                if not moved:
                    moved = True
                    self._move_document_under_hidden_acl(doc_id)
                return original_copy(version)

            with SessionLocal() as db:
                with patch.object(routers, "copy_version_to_temp", side_effect=move_before_copy):
                    with self.assertRaises(HTTPException) as raised:
                        download_version(doc_id, version_id, FAKE_REQUEST, reader, db)

            self.assertTrue(moved)
            self.assertEqual(raised.exception.status_code, 404)
            self.assertEqual(raised.exception.detail, "Document not found")


if __name__ == "__main__":
    unittest.main()
