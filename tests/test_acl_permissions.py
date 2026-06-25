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
    FolderPermissionPayload,
    FolderPermissionsPayload,
    api_update_folder_permissions,
    archive_items,
    archive_doc_item,
    build_contents_payload,
    create_document,
    create_folder,
    download_items,
    get_or_create_folder_path,
    get_root_folder,
    restore_doc_item,
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

    def test_created_folders_inherit_parent_acl_and_missing_acl_denies(
        self,
    ) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        writer = user_context("writer", groups=["writers"], is_admin=False)
        outsider = user_context("outsider", groups=["outsiders"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                writers = VaultGroup(name="writers")
                outsiders = VaultGroup(name="outsiders")
                db.add_all([writers, outsiders])
                db.flush()
                add_permission(db, root, writers, write=True)

                create_folder("Open", admin, db)
                open_folder = db.query(Folder).filter_by(parent_id=root.id, name="Open").one()
                rule_count = (
                    db.query(FolderPermission)
                    .filter_by(folder_id=open_folder.id, group_id=writers.id)
                    .count()
                )
                self.assertEqual(rule_count, 0)

                locked = Folder(root_key="vault", parent_id=root.id, name="Locked", is_root=False)
                db.add(locked)
                db.commit()

            with ctx.db() as db:
                open_payload = build_contents_payload(db, "Open", writer)
                self.assertEqual(open_payload["folder"], "Open")
                with self.assertRaises(HTTPException) as raised:
                    build_contents_payload(db, "Locked", outsider)
                self.assertEqual(raised.exception.status_code, 404)

    def test_new_child_folder_inherits_restricted_parent_acl(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        writer = user_context("writer", groups=["writers"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                writers = VaultGroup(name="writers")
                db.add(writers)
                db.flush()
                add_permission(db, root, writers, write=True)

                secret = get_or_create_folder_path(db, "Secret")
                db.flush()
                db.query(FolderPermission).filter_by(folder_id=secret.id).delete()
                db.flush()
                add_permission(db, secret, writers, view=False, read=False, write=False)

                create_folder("Secret/Plans", admin, db)
                plans = db.query(Folder).filter_by(parent_id=secret.id, name="Plans").one()
                create_versioned_document(
                    db,
                    plans,
                    name="roadmap.txt",
                    data=b"secret roadmap",
                    actor=admin,
                )
                db.commit()

            with ctx.db() as db:
                with self.assertRaises(HTTPException) as raised:
                    build_contents_payload(db, "Secret/Plans", writer)
                self.assertEqual(raised.exception.status_code, 404)

    def test_failed_folder_archive_does_not_stamp_stale_source_acl(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        outsider = user_context("outsider", groups=["outsiders"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                vault_root = get_root_folder(db, "vault")
                archive_root = get_root_folder(db, "archive")
                outsiders = VaultGroup(name="outsiders")
                db.add(outsiders)
                db.flush()
                add_permission(db, vault_root, outsiders, write=True)
                add_permission(db, archive_root, outsiders, write=True)

                secret = get_or_create_folder_path(db, "Secret")
                plans = get_or_create_folder_path(db, "Secret/Plans")
                create_versioned_document(
                    db,
                    plans,
                    name="roadmap.txt",
                    data=b"secret roadmap",
                    actor=admin,
                )

                archive_conflict = get_or_create_folder_path(db, "Archive/Secret/Plans")
                create_versioned_document(
                    db,
                    archive_conflict,
                    name="existing.txt",
                    data=b"existing archive content",
                    actor=admin,
                )
                secret_id = secret.id
                plans_id = plans.id
                outsiders_id = outsiders.id
                db.commit()

                result = archive_items(
                    ActionPayload(items=[ActionItem(type="folder", id=plans_id)]),
                    FAKE_REQUEST,
                    admin,
                    db,
                )
                self.assertEqual(result["ok"], [])
                self.assertEqual(len(result["failed"]), 1)

                secret = db.get(Folder, secret_id)
                outsiders = db.get(VaultGroup, outsiders_id)
                db.query(FolderPermission).filter_by(folder_id=secret.id).delete()
                db.flush()
                add_permission(db, secret, outsiders, view=False, read=False, write=False)
                db.commit()

            with ctx.db() as db:
                with self.assertRaises(HTTPException) as raised:
                    build_contents_payload(db, "Secret/Plans", outsider)
                self.assertEqual(raised.exception.status_code, 404)

    def test_permission_update_rejects_write_without_read_and_view(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_runtime() as ctx, ctx.db() as db:
            root = get_root_folder(db, "vault")
            project = get_or_create_folder_path(db, "Project")
            outsiders = VaultGroup(name="outsiders")
            db.add(outsiders)
            db.flush()
            add_permission(db, root, outsiders, view=False, read=False, write=False)
            db.commit()

            with self.assertRaises(HTTPException) as raised:
                api_update_folder_permissions(
                    FolderPermissionsPayload(
                        path="Project",
                        permissions=[
                            FolderPermissionPayload(
                                group_id=outsiders.id,
                                can_view=False,
                                can_read=False,
                                can_write=True,
                            ),
                        ],
                    ),
                    admin,
                    db,
                )

            self.assertEqual(raised.exception.status_code, 400)
            self.assertEqual(
                raised.exception.detail,
                "Write permission requires read and view permission",
            )
            self.assertEqual(
                db.query(FolderPermission).filter_by(folder_id=project.id).count(),
                0,
            )

    def test_archiving_restricted_document_preserves_folder_acl_in_archive(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        outsider = user_context("outsider", groups=["outsiders"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                vault_root = get_root_folder(db, "vault")
                archive_root = get_root_folder(db, "archive")
                outsiders = VaultGroup(name="outsiders")
                db.add(outsiders)
                db.flush()
                add_permission(db, vault_root, outsiders, write=True)
                add_permission(db, archive_root, outsiders, write=True)

                secret = get_or_create_folder_path(db, "Secret")
                db.flush()
                db.query(FolderPermission).filter_by(folder_id=secret.id).delete()
                db.flush()
                add_permission(db, secret, outsiders, view=False, read=False, write=False)

                doc = create_versioned_document(
                    db,
                    secret,
                    name="roadmap.txt",
                    data=b"secret roadmap",
                    actor=admin,
                )
                archive_doc_item(doc, FAKE_REQUEST, admin, db)
                db.commit()

            with ctx.db() as db:
                with self.assertRaises(HTTPException) as raised:
                    build_contents_payload(db, "Archive/Secret", outsider)
                self.assertEqual(raised.exception.status_code, 404)

    def test_restoring_document_does_not_overwrite_current_vault_folder_acl(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        outsider = user_context("outsider", groups=["outsiders"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                vault_root = get_root_folder(db, "vault")
                archive_root = get_root_folder(db, "archive")
                outsiders = VaultGroup(name="outsiders")
                db.add(outsiders)
                db.flush()
                add_permission(db, vault_root, outsiders, write=True)
                add_permission(db, archive_root, outsiders, write=True)

                secret = get_or_create_folder_path(db, "Secret")
                doc = create_versioned_document(
                    db,
                    secret,
                    name="roadmap.txt",
                    data=b"secret roadmap",
                    actor=admin,
                )
                archive_doc_item(doc, FAKE_REQUEST, admin, db)

                db.query(FolderPermission).filter_by(folder_id=secret.id).delete()
                db.flush()
                add_permission(db, secret, outsiders, view=False, read=False, write=False)

                restore_doc_item(doc, FAKE_REQUEST, admin, db)
                db.commit()

            with ctx.db() as db:
                with self.assertRaises(HTTPException) as raised:
                    build_contents_payload(db, "Secret", outsider)
                self.assertEqual(raised.exception.status_code, 404)


if __name__ == "__main__":
    unittest.main()
