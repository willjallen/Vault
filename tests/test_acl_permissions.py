import asyncio
import unittest

from fastapi import HTTPException
from tests.support import (
    FAKE_REQUEST,
    add_permission,
    create_versioned_document,
    user_context,
    vault_runtime,
)

from app.models import DocumentLock, Folder, FolderPermission, VaultGroup
from app.routers import (
    ActionItem,
    ActionPayload,
    build_contents_payload,
    create_document,
    create_folder,
    download_items,
    get_or_create_folder_path,
    get_root_folder,
    unlock_items,
)


class Upload:
    filename = "writer.txt"
    content_type = "text/plain"

    async def read(self):
        return b"writer"


class AclPermissionTests(unittest.TestCase):
    def test_folder_acl_enforces_visible_read_and_write_paths(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        viewer = user_context("viewer", groups=["viewers"], is_admin=False)
        reader = user_context("reader", groups=["readers"], is_admin=False)
        writer = user_context("writer", groups=["writers"], is_admin=False)
        outsider = user_context("outsider", groups=["outsiders"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                viewers = VaultGroup(name="viewers")
                readers = VaultGroup(name="readers")
                writers = VaultGroup(name="writers")
                outsiders = VaultGroup(name="outsiders")
                db.add_all([viewers, readers, writers, outsiders])
                db.flush()

                for group in (viewers, readers, writers, outsiders):
                    add_permission(db, root, group)

                project = get_or_create_folder_path(db, "Project")
                project_id = project.id
                db.commit()

                project = db.get(Folder, project_id)
                db.query(FolderPermission).filter_by(folder_id=project.id).delete()
                db.flush()
                add_permission(db, project, viewers, read=False)
                add_permission(db, project, readers)
                add_permission(db, project, writers, write=True)
                doc = create_versioned_document(
                    db,
                    project,
                    name="plan.txt",
                    data=b"secret",
                    actor=admin,
                )
                db.add(
                    DocumentLock(
                        document_id=doc.id,
                        locked_by=str(reader["id"]),
                        locked_by_name=str(reader["name"]),
                    ),
                )
                doc_id = doc.id
                db.commit()

            with ctx.db() as db:
                viewer_root = build_contents_payload(db, "", viewer)
                self.assertEqual([folder["path"] for folder in viewer_root["folders"]], ["Project"])

                outsider_root = build_contents_payload(db, "", outsider)
                self.assertEqual(outsider_root["folders"], [])

                viewer_project = build_contents_payload(db, "Project", viewer)
                self.assertEqual(
                    viewer_project["documents"][0]["access"],
                    {
                        "visible": True,
                        "read": False,
                        "write": False,
                    },
                )

                with self.assertRaises(HTTPException) as raised:
                    download_items(
                        ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                        FAKE_REQUEST,
                        viewer,
                        db,
                    )
                self.assertEqual(raised.exception.status_code, 403)

                response = download_items(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    FAKE_REQUEST,
                    reader,
                    db,
                )
                self.assertEqual(response.body, b"secret")

                result = unlock_items(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    FAKE_REQUEST,
                    writer,
                    db,
                )
                self.assertEqual(result["ok"][0]["detail"], "Unlocked")
                active_locks = db.query(DocumentLock).filter_by(document_id=doc_id, is_active=True)
                self.assertEqual(active_locks.count(), 0)

                try:
                    with self.assertRaises(HTTPException) as upload_raised:
                        asyncio.run(create_document(FAKE_REQUEST, Upload(), "Project", reader, db))
                    self.assertEqual(upload_raised.exception.status_code, 403)
                finally:
                    db.rollback()

                result = asyncio.run(create_document(FAKE_REQUEST, Upload(), "Project", writer, db))
                self.assertEqual(result["path"], "Project/writer.txt")

    def test_created_folders_default_to_write_for_existing_groups_and_missing_acl_denies(
        self,
    ) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        writer = user_context("writer", groups=["writers"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                writers = VaultGroup(name="writers")
                db.add(writers)
                db.flush()

                create_folder("Open", admin, db)
                open_folder = db.query(Folder).filter_by(parent_id=root.id, name="Open").one()
                rule = (
                    db.query(FolderPermission)
                    .filter_by(folder_id=open_folder.id, group_id=writers.id)
                    .one()
                )
                self.assertTrue(rule.can_view)
                self.assertTrue(rule.can_read)
                self.assertTrue(rule.can_write)

                locked = Folder(root_key="vault", parent_id=root.id, name="Locked", is_root=False)
                db.add(locked)
                db.commit()

            with ctx.db() as db:
                with self.assertRaises(HTTPException) as raised:
                    build_contents_payload(db, "Locked", writer)
                self.assertEqual(raised.exception.status_code, 404)


if __name__ == "__main__":
    unittest.main()
