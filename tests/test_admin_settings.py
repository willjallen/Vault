import unittest

from tests.support import (
    FAKE_REQUEST,
    auth_headers,
    create_versioned_document,
    user_context,
    vault_test_client,
)

from app.models import Document, Folder, FolderPermission, StateEvent, VaultGroup
from app.routers import (
    archive_doc_item,
    archive_folder_item,
    get_folder_by_path,
    get_or_create_folder_path,
)


def set_group_write_access(db, group_name: str, folder: Folder, *, write: bool) -> None:
    db.flush()
    group = db.query(VaultGroup).filter_by(name=group_name).one()
    permission = (
        db.query(FolderPermission)
        .filter_by(folder_id=folder.id, group_id=group.id)
        .one_or_none()
    )
    if not permission:
        permission = FolderPermission(folder_id=folder.id, group_id=group.id)
        db.add(permission)
    permission.can_view = True
    permission.can_read = True
    permission.can_write = write


class AdminSettingsTests(unittest.TestCase):
    def test_archive_permanent_delete_policy_defaults_to_admin_only(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])
        writer_headers = auth_headers("writer", ["writers"])
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_test_client() as ctx:
            admin_bootstrap = ctx.client.get("/api/bootstrap", headers=admin_headers)
            self.assertEqual(admin_bootstrap.status_code, 200, admin_bootstrap.text)
            self.assertTrue(
                admin_bootstrap.json()["settings"]["archivePermanentDeleteAdminOnly"],
            )
            writer_bootstrap = ctx.client.get("/api/bootstrap", headers=writer_headers)
            self.assertEqual(writer_bootstrap.status_code, 200, writer_bootstrap.text)

            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=admin)
                archive_doc_item(doc, FAKE_REQUEST, admin, db)
                db.commit()
                doc_id = doc.id

            denied = ctx.client.post(
                "/api/delete-forever",
                json={"items": [{"type": "document", "id": doc_id}]},
                headers=writer_headers,
            )
            self.assertEqual(denied.status_code, 403)
            self.assertEqual(denied.json()["detail"], "Admin access required")

    def test_admin_can_allow_writers_to_delete_archived_items_forever(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])
        writer_headers = auth_headers("writer", ["writers"])
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_test_client() as ctx:
            admin_bootstrap = ctx.client.get("/api/bootstrap", headers=admin_headers)
            writer_bootstrap = ctx.client.get("/api/bootstrap", headers=writer_headers)
            self.assertEqual(admin_bootstrap.status_code, 200, admin_bootstrap.text)
            self.assertEqual(writer_bootstrap.status_code, 200, writer_bootstrap.text)

            updated = ctx.client.patch(
                "/api/admin/settings",
                json={"settings": {"archivePermanentDeleteAdminOnly": False}},
                headers=admin_headers,
            )
            self.assertEqual(updated.status_code, 200, updated.text)
            self.assertFalse(updated.json()["settings"]["archivePermanentDeleteAdminOnly"])

            synced = ctx.client.get("/api/settings", headers=writer_headers)
            self.assertEqual(synced.status_code, 200, synced.text)
            self.assertFalse(synced.json()["settings"]["archivePermanentDeleteAdminOnly"])

            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=admin)
                archive_doc_item(doc, FAKE_REQUEST, admin, db)
                db.commit()
                doc_id = doc.id

            deleted = ctx.client.post(
                "/api/delete-forever",
                json={"items": [{"type": "document", "id": doc_id}]},
                headers=writer_headers,
            )
            self.assertEqual(deleted.status_code, 200, deleted.text)
            self.assertEqual(deleted.json()["failed"], [])
            self.assertEqual(deleted.json()["ok"][0]["item"], {"type": "document", "id": doc_id})

    def test_relaxed_policy_still_requires_write_access_per_archived_item(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])
        reader_headers = auth_headers("reader", ["readers"])
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_test_client() as ctx:
            admin_bootstrap = ctx.client.get("/api/bootstrap", headers=admin_headers)
            reader_bootstrap = ctx.client.get("/api/bootstrap", headers=reader_headers)
            self.assertEqual(admin_bootstrap.status_code, 200, admin_bootstrap.text)
            self.assertEqual(reader_bootstrap.status_code, 200, reader_bootstrap.text)
            updated = ctx.client.patch(
                "/api/admin/settings",
                json={"settings": {"archivePermanentDeleteAdminOnly": False}},
                headers=admin_headers,
            )
            self.assertEqual(updated.status_code, 200, updated.text)

            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, actor=admin)
                archive_doc_item(doc, FAKE_REQUEST, admin, db)
                set_group_write_access(db, "readers", doc.folder, write=False)
                db.commit()
                doc_id = doc.id

            denied = ctx.client.post(
                "/api/delete-forever",
                json={"items": [{"type": "document", "id": doc_id}]},
                headers=reader_headers,
            )
            self.assertEqual(denied.status_code, 200, denied.text)
            self.assertEqual(denied.json()["ok"], [])
            self.assertEqual(denied.json()["failed"][0]["detail"], "Insufficient document access")

            with ctx.db() as db:
                self.assertIsNotNone(db.get(Document, doc_id))

    def test_relaxed_policy_allows_writer_to_delete_archived_folder_forever(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])
        writer_headers = auth_headers("writer", ["writers"])
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_test_client() as ctx:
            admin_bootstrap = ctx.client.get("/api/bootstrap", headers=admin_headers)
            writer_bootstrap = ctx.client.get("/api/bootstrap", headers=writer_headers)
            self.assertEqual(admin_bootstrap.status_code, 200, admin_bootstrap.text)
            self.assertEqual(writer_bootstrap.status_code, 200, writer_bootstrap.text)
            updated = ctx.client.patch(
                "/api/admin/settings",
                json={"settings": {"archivePermanentDeleteAdminOnly": False}},
                headers=admin_headers,
            )
            self.assertEqual(updated.status_code, 200, updated.text)

            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                create_versioned_document(db, folder, actor=admin)
                archived_path = archive_folder_item(folder, FAKE_REQUEST, admin, db)
                db.commit()

            archive_contents = ctx.client.get(
                "/api/folders/contents",
                params={"folder": "Archive"},
                headers=writer_headers,
            )
            self.assertEqual(archive_contents.status_code, 200, archive_contents.text)
            [folder_row] = archive_contents.json()["folders"]
            self.assertEqual(folder_row["path"], "Archive/Project")
            self.assertTrue(folder_row["access"]["write"])

            deleted = ctx.client.post(
                "/api/delete-forever",
                json={"items": [{"type": "folder", "path": archived_path}]},
                headers=writer_headers,
            )
            self.assertEqual(deleted.status_code, 200, deleted.text)
            self.assertEqual(deleted.json()["failed"], [])
            self.assertEqual(
                deleted.json()["ok"][0]["item"],
                {"type": "folder", "path": "Archive/Project"},
            )

            with ctx.db() as db:
                self.assertIsNone(get_folder_by_path(db, "Archive/Project"))

    def test_non_admin_cannot_change_archive_delete_policy(self) -> None:
        writer_headers = auth_headers("writer", ["writers"])

        with vault_test_client() as ctx:
            denied = ctx.client.patch(
                "/api/admin/settings",
                json={"settings": {"archivePermanentDeleteAdminOnly": False}},
                headers=writer_headers,
            )
            self.assertEqual(denied.status_code, 403)
            self.assertEqual(denied.json()["detail"], "Admin access required")

    def test_archive_delete_policy_change_emits_settings_refresh_event(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])

        with vault_test_client() as ctx:
            updated = ctx.client.patch(
                "/api/admin/settings",
                json={"settings": {"archivePermanentDeleteAdminOnly": False}},
                headers=admin_headers,
            )
            self.assertEqual(updated.status_code, 200, updated.text)

            with ctx.db() as db:
                events = db.query(StateEvent).order_by(StateEvent.id.desc()).all()
                settings_events = [
                    event
                    for event in events
                    if event.event_type == "admin.settings.updated"
                ]
                self.assertTrue(settings_events)
                self.assertEqual(
                    settings_events[0].payload["resources"],
                    ["admin", "settings"],
                )

    def test_admin_settings_reject_unknown_keys(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])

        with vault_test_client() as ctx:
            invalid = ctx.client.patch(
                "/api/admin/settings",
                json={"settings": {"deleteAnything": True}},
                headers=admin_headers,
            )
            self.assertEqual(invalid.status_code, 400)
            self.assertEqual(invalid.json()["detail"], "Unknown setting: deleteAnything")


if __name__ == "__main__":
    unittest.main()
