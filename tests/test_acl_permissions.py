import io
import unittest
import zipfile

from fastapi import HTTPException
from tests.support import (
    FAKE_REQUEST,
    add_permission,
    auth_headers,
    collect_response_body,
    create_versioned_document,
    upload_file_via_session,
    user_context,
    vault_runtime,
    vault_test_client,
    wait_for_export,
)

from app.models import Document, DocumentLock, Folder, FolderPermission, VaultGroup
from app.routers import (
    ActionItem,
    ActionPayload,
    FolderPermissionPayload,
    FolderPermissionsPayload,
    api_update_folder_permissions,
    archive_doc_item,
    archive_items,
    build_contents_payload,
    create_folder,
    document_path,
    download_items,
    get_or_create_folder_path,
    get_root_folder,
    restore_doc_item,
    unlock_items,
)


class AclPermissionTests(unittest.TestCase):
    def test_folder_acl_enforces_visible_read_and_write_paths(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        viewer = user_context("viewer", groups=["viewers"], is_admin=False)
        reader = user_context("reader", groups=["readers"], is_admin=False)
        writer = user_context("writer", groups=["writers"], is_admin=False)
        outsider = user_context("outsider", groups=["outsiders"], is_admin=False)

        with vault_test_client() as ctx:
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
                        locked_by=str(writer["id"]),
                        locked_by_name=str(writer["name"]),
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
                self.assertEqual(collect_response_body(response), b"secret")

                result = unlock_items(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    FAKE_REQUEST,
                    writer,
                    db,
                )
                self.assertEqual(result["ok"][0]["detail"], "Unlocked")
                active_locks = db.query(DocumentLock).filter_by(document_id=doc_id, is_active=True)
                self.assertEqual(active_locks.count(), 0)

                db.rollback()

            reader_upload = upload_file_via_session(
                ctx.client,
                headers=auth_headers("reader", ["readers"]),
                filename="writer.txt",
                data=b"writer",
                folder="Project",
            )
            self.assertEqual(reader_upload.status_code, 403)

            writer_upload = upload_file_via_session(
                ctx.client,
                headers=auth_headers("writer", ["writers"]),
                filename="writer.txt",
                data=b"writer",
                folder="Project",
            )
            self.assertEqual(writer_upload.status_code, 200, writer_upload.text)
            self.assertEqual(writer_upload.json()["path"], "Project/writer.txt")

    def test_folder_download_excludes_inaccessible_descendants(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_test_client() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                readers = VaultGroup(name="readers")
                confidential = VaultGroup(name="confidential")
                db.add_all([readers, confidential])
                db.flush()
                add_permission(db, root, readers)

                project = get_or_create_folder_path(db, "Project")
                private = get_or_create_folder_path(db, "Project/Private")
                db.flush()
                add_permission(db, project, readers)
                add_permission(db, private, confidential)

                create_versioned_document(
                    db,
                    project,
                    name="visible.txt",
                    data=b"visible",
                    actor=admin,
                )
                create_versioned_document(
                    db,
                    private,
                    name="secret.txt",
                    data=b"secret",
                    actor=admin,
                )
                project_id = project.id
                db.commit()

            export_response = ctx.client.post(
                "/api/exports",
                json={"items": [{"type": "folder", "id": project_id}]},
                headers=auth_headers("reader", ["readers"]),
            )
            self.assertEqual(export_response.status_code, 200, export_response.text)
            export = wait_for_export(
                ctx.client,
                export_response.json()["id"],
                headers=auth_headers("reader", ["readers"]),
            )
            self.assertEqual(export["status"], "complete")
            response = ctx.client.get(
                str(export["download_url"]), headers=auth_headers("reader", ["readers"])
            )

            with zipfile.ZipFile(io.BytesIO(response.content)) as archive:
                self.assertEqual(archive.namelist(), ["Project/visible.txt"])
                self.assertEqual(archive.read("Project/visible.txt"), b"visible")

    def test_folder_archive_rejects_inaccessible_descendants(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        writer = user_context("writer", groups=["writers"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                vault_root = get_root_folder(db, "vault")
                archive_root = get_root_folder(db, "archive")
                writers = VaultGroup(name="writers")
                confidential = VaultGroup(name="confidential")
                db.add_all([writers, confidential])
                db.flush()
                add_permission(db, vault_root, writers, write=True)
                add_permission(db, archive_root, writers, write=True)

                project = get_or_create_folder_path(db, "Project")
                private = get_or_create_folder_path(db, "Project/Private")
                db.flush()
                add_permission(db, project, writers, write=True)
                add_permission(db, private, confidential, write=True)

                create_versioned_document(
                    db,
                    project,
                    name="visible.txt",
                    data=b"visible",
                    actor=admin,
                )
                secret = create_versioned_document(
                    db,
                    private,
                    name="secret.txt",
                    data=b"secret",
                    actor=admin,
                )
                project_id = project.id
                secret_id = secret.id
                db.commit()

                result = archive_items(
                    ActionPayload(items=[ActionItem(type="folder", id=project_id)]),
                    FAKE_REQUEST,
                    writer,
                    db,
                )
                self.assertEqual(result["ok"], [])
                self.assertEqual(len(result["failed"]), 1)

                db.expire_all()
                secret = db.get(Document, secret_id)
                self.assertEqual(document_path(secret), "Project/Private/secret.txt")

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

    def test_failed_folder_archive_does_not_change_source_acl(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        outsider = user_context("outsider", groups=["outsiders"], is_admin=False)

        with vault_runtime() as ctx:
            with ctx.db() as db:
                vault_root = get_root_folder(db, "vault")
                archive_root = get_root_folder(db, "archive")
                outsiders = VaultGroup(name="outsiders")
                confidential = VaultGroup(name="confidential")
                db.add_all([outsiders, confidential])
                db.flush()
                add_permission(db, vault_root, outsiders, write=True)
                add_permission(db, archive_root, outsiders, write=True)

                secret = get_or_create_folder_path(db, "Secret")
                plans = get_or_create_folder_path(db, "Secret/Plans")
                db.flush()
                add_permission(db, secret, outsiders, write=True)
                add_permission(db, plans, confidential, write=True)
                create_versioned_document(
                    db,
                    plans,
                    name="roadmap.txt",
                    data=b"secret roadmap",
                    actor=admin,
                )
                secret_id = secret.id
                plans_id = plans.id
                outsiders_id = outsiders.id
                db.commit()

                result = archive_items(
                    ActionPayload(items=[ActionItem(type="folder", id=plans_id)]),
                    FAKE_REQUEST,
                    outsider,
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
                payload = build_contents_payload(db, "Archive", outsider)
                self.assertEqual(payload["folders"], [])
                self.assertEqual(payload["documents"], [])

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
