import asyncio
import unittest

from fastapi import HTTPException
from tests.support import (
    FAKE_REQUEST,
    add_permission,
    auth_headers,
    create_versioned_document,
    user_context,
    vault_test_client,
)

from app.models import (
    Document,
    Folder,
    FolderPermission,
    StateEvent,
    VaultGroup,
    VaultGroupMembership,
    VaultUser,
)
from app.routers import (
    AdminGroupPayload,
    AdminUserUpdate,
    api_admin_delete_group,
    api_admin_remove_group_member,
    api_admin_update_group,
    api_admin_update_user,
    api_events_stream,
    archive_doc_item,
    archive_folder_item,
    get_folder_by_path,
    get_or_create_folder_path,
    get_root_folder,
)


class FakeEventStreamRequest:
    headers: dict[str, str] = {}

    async def is_disconnected(self) -> bool:
        return False


def set_group_write_access(db, group_name: str, folder: Folder, *, write: bool) -> None:
    db.flush()
    group = db.query(VaultGroup).filter_by(name=group_name).one()
    permission = (
        db.query(FolderPermission).filter_by(folder_id=folder.id, group_id=group.id).one_or_none()
    )
    if not permission:
        permission = FolderPermission(folder_id=folder.id, group_id=group.id)
        db.add(permission)
    permission.can_view = True
    permission.can_read = True
    permission.can_write = write


class AdminSettingsTests(unittest.TestCase):
    def test_debug_tools_are_hidden_outside_dev_mode(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])

        with vault_test_client() as ctx:
            bootstrap = ctx.client.get("/api/bootstrap", headers=admin_headers)
            self.assertEqual(bootstrap.status_code, 200, bootstrap.text)
            self.assertFalse(bootstrap.json()["dev_mode"])

            directory = ctx.client.get("/api/admin/directory", headers=admin_headers)
            self.assertEqual(directory.status_code, 200, directory.text)
            self.assertFalse(directory.json()["dev_mode"])

            denied = ctx.client.post(
                "/api/admin/debug/error",
                json={"kind": "server"},
                headers=admin_headers,
            )
            self.assertEqual(denied.status_code, 404)

            timeout_denied = ctx.client.post(
                "/api/admin/debug/timeout",
                headers=admin_headers,
            )
            self.assertEqual(timeout_denied.status_code, 404)

    def test_dev_mode_exposes_admin_debug_tools(self) -> None:
        with vault_test_client(auth_mode="dev") as ctx:
            bootstrap = ctx.client.get("/api/bootstrap")
            self.assertEqual(bootstrap.status_code, 200, bootstrap.text)
            self.assertTrue(bootstrap.json()["dev_mode"])

            directory = ctx.client.get("/api/admin/directory")
            self.assertEqual(directory.status_code, 200, directory.text)
            self.assertTrue(directory.json()["dev_mode"])

            server_error = ctx.client.post("/api/admin/debug/error", json={"kind": "server"})
            self.assertEqual(server_error.status_code, 500)
            self.assertEqual(server_error.json()["detail"], "Debug server error")

            stream = asyncio.run(api_events_stream(FakeEventStreamRequest(), user_context()))
            timeout = ctx.client.post("/api/admin/debug/timeout")
            self.assertEqual(timeout.status_code, 200, timeout.text)
            self.assertEqual(timeout.json()["action"], "timeout")
            self.assertEqual(timeout.json()["seconds"], 10)
            self.assertEqual(timeout.json()["stream_retry_ms"], 10000)

            stream_chunk = asyncio.run(anext(stream.body_iterator))
            if isinstance(stream_chunk, bytes):
                stream_chunk = stream_chunk.decode()
            self.assertEqual(stream_chunk, "retry: 10000\n\n")
            with self.assertRaises(StopAsyncIteration):
                asyncio.run(anext(stream.body_iterator))

            seeded = ctx.client.post("/api/admin/debug/seed")
            self.assertEqual(seeded.status_code, 200, seeded.text)
            self.assertEqual(seeded.json()["action"], "seed")
            self.assertEqual(seeded.json()["folder"], "Debug Samples")

            emitted = ctx.client.post(
                "/api/admin/debug/emit-state",
                json={"resources": ["contents", "sidebar", "not-real"]},
            )
            self.assertEqual(emitted.status_code, 200, emitted.text)
            self.assertEqual(emitted.json()["resources"], ["contents", "sidebar"])

            storage_report = ctx.client.post("/api/admin/debug/storage-report")
            self.assertEqual(storage_report.status_code, 200, storage_report.text)
            self.assertIn("report", storage_report.json())

            swept = ctx.client.post("/api/admin/debug/sweep-ttl")
            self.assertEqual(swept.status_code, 200, swept.text)
            self.assertIn("result", swept.json())

            reset = ctx.client.post("/api/admin/debug/reset-database")
            self.assertEqual(reset.status_code, 200, reset.text)
            self.assertTrue(reset.json()["reload"])

            refreshed = ctx.client.get("/api/bootstrap")
            self.assertEqual(refreshed.status_code, 200, refreshed.text)
            self.assertTrue(refreshed.json()["dev_mode"])

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
                {"type": "folder", "id": folder_row["id"], "path": "Archive/Project"},
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
                    event for event in events if event.event_type == "admin.settings.updated"
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

    def test_admin_cannot_delete_group_used_by_folder_permissions(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])
        writer_headers = auth_headers("writer", ["writers"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                writers = VaultGroup(name="writers")
                confidential = VaultGroup(name="confidential")
                db.add_all([writers, confidential])
                db.flush()
                add_permission(db, root, writers, write=True)

                secret = get_or_create_folder_path(db, "Secret")
                add_permission(db, secret, confidential, write=True)
                confidential_id = confidential.id
                db.commit()

            hidden_before = ctx.client.get(
                "/api/folders/contents",
                params={"folder": "Secret"},
                headers=writer_headers,
            )
            self.assertEqual(hidden_before.status_code, 404, hidden_before.text)

            deleted = ctx.client.delete(
                f"/api/admin/groups/{confidential_id}",
                headers=admin_headers,
            )
            self.assertEqual(deleted.status_code, 400, deleted.text)

            hidden_after = ctx.client.get(
                "/api/folders/contents",
                params={"folder": "Secret"},
                headers=writer_headers,
            )
            self.assertEqual(hidden_after.status_code, 404, hidden_after.text)

            with ctx.db() as db:
                self.assertIsNotNone(db.get(VaultGroup, confidential_id))
                self.assertEqual(
                    db.query(FolderPermission).filter_by(group_id=confidential_id).count(),
                    1,
                )

    def test_admin_cannot_delete_only_effective_admin_group(self) -> None:
        acting_admin = user_context("bob", groups=["vault-admin"], is_admin=True)

        with vault_test_client() as ctx, ctx.db() as db:
            admin_group = VaultGroup(name="vault-admin")
            bob = VaultUser(
                issuer="oidc",
                subject="bob",
                email="bob@example.com",
                name="Bob",
                is_admin=False,
                is_active=True,
            )
            db.add_all([admin_group, bob])
            db.flush()
            db.add(VaultGroupMembership(user_id=bob.id, group_id=admin_group.id))
            db.commit()
            group_id = admin_group.id

            with self.assertRaises(HTTPException) as raised:
                api_admin_delete_group(group_id, acting_admin, db)

            self.assertEqual(raised.exception.status_code, 400)
            self.assertEqual(raised.exception.detail, "At least one active admin is required")
            db.rollback()
            self.assertIsNotNone(db.get(VaultGroup, group_id))
            self.assertEqual(db.query(VaultGroupMembership).filter_by(group_id=group_id).count(), 1)

    def test_admin_cannot_rename_only_effective_admin_group(self) -> None:
        acting_admin = user_context("bob", groups=["vault-admin"], is_admin=True)

        with vault_test_client() as ctx, ctx.db() as db:
            admin_group = VaultGroup(name="vault-admin")
            bob = VaultUser(
                issuer="oidc",
                subject="bob",
                email="bob@example.com",
                name="Bob",
                is_admin=False,
                is_active=True,
            )
            db.add_all([admin_group, bob])
            db.flush()
            db.add(VaultGroupMembership(user_id=bob.id, group_id=admin_group.id))
            db.commit()
            group_id = admin_group.id

            with self.assertRaises(HTTPException) as raised:
                api_admin_update_group(
                    group_id,
                    AdminGroupPayload(name="staff", description="Staff"),
                    acting_admin,
                    db,
                )

            self.assertEqual(raised.exception.status_code, 400)
            self.assertEqual(raised.exception.detail, "At least one active admin is required")
            db.rollback()
            self.assertEqual(db.get(VaultGroup, group_id).name, "vault-admin")

    def test_admin_cannot_remove_only_effective_admin_group_membership(self) -> None:
        acting_admin = user_context("bob", groups=["vault-admin"], is_admin=True)

        with vault_test_client() as ctx, ctx.db() as db:
            admin_group = VaultGroup(name="vault-admin")
            bob = VaultUser(
                issuer="oidc",
                subject="bob",
                email="bob@example.com",
                name="Bob",
                is_admin=False,
                is_active=True,
            )
            db.add_all([admin_group, bob])
            db.flush()
            db.add(VaultGroupMembership(user_id=bob.id, group_id=admin_group.id))
            db.commit()
            group_id = admin_group.id
            user_id = bob.id

            with self.assertRaises(HTTPException) as raised:
                api_admin_remove_group_member(group_id, user_id, acting_admin, db)

            self.assertEqual(raised.exception.status_code, 400)
            self.assertEqual(raised.exception.detail, "At least one active admin is required")
            db.rollback()
            self.assertEqual(
                db.query(VaultGroupMembership)
                .filter_by(group_id=group_id, user_id=user_id)
                .count(),
                1,
            )

    def test_last_admin_guard_normalizes_persisted_group_names(self) -> None:
        acting_admin = user_context("bob", groups=["Vault-Admin"], is_admin=True)

        with vault_test_client() as ctx, ctx.db() as db:
            admin_group = VaultGroup(name="Vault-Admin")
            bob = VaultUser(
                issuer="oidc",
                subject="bob",
                email="bob@example.com",
                name="Bob",
                is_admin=False,
                is_active=True,
            )
            db.add_all([admin_group, bob])
            db.flush()
            db.add(VaultGroupMembership(user_id=bob.id, group_id=admin_group.id))
            db.commit()
            user_id = bob.id

            with self.assertRaises(HTTPException) as raised:
                api_admin_update_user(
                    user_id,
                    AdminUserUpdate(is_active=False),
                    acting_admin,
                    db,
                )

            self.assertEqual(raised.exception.status_code, 400)
            self.assertEqual(raised.exception.detail, "At least one active admin is required")
            db.rollback()
            self.assertTrue(db.get(VaultUser, user_id).is_active)


if __name__ == "__main__":
    unittest.main()
