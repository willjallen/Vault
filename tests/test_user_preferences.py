import unittest

from tests.support import (
    add_permission,
    auth_headers,
    create_versioned_document,
    user_context,
    vault_runtime,
    vault_test_client,
)

from app.db import SessionLocal
from app.models import Document, Folder, VaultGroup, VaultUser
from app.routers import get_or_create_folder_path, get_root_folder, preferences_for_user


class UserPreferenceTests(unittest.TestCase):
    def test_preferences_sync_through_bootstrap_and_api(self) -> None:
        headers = auth_headers("artist", ["vault-admin"])

        with vault_test_client() as ctx:
            created_folder = ctx.client.post("/folders", data={"folder": "Art"}, headers=headers)
            self.assertEqual(created_folder.status_code, 200, created_folder.text)
            folder_id = created_folder.json()["id"]
            uploaded = ctx.client.post(
                "/documents",
                data={"folder": "Art"},
                files={"file": ("chest.fbx", b"chest", "model/fbx")},
                headers=headers,
            )
            self.assertEqual(uploaded.status_code, 200, uploaded.text)
            document_id = uploaded.json()["id"]

            initial = ctx.client.get("/api/bootstrap", headers=headers)
            self.assertEqual(initial.status_code, 200, initial.text)
            self.assertEqual(
                initial.json()["preferences"],
                {
                    "themePreference": "system",
                    "palettePreference": "cozy",
                    "openFoldersOnClick": True,
                    "alternateRows": False,
                    "doubleClickDownload": False,
                    "favoriteItems": [],
                    "sidebarSectionSizes": {
                        "folders": 180,
                        "favorites": 95,
                        "editing": 90,
                        "archive": 115,
                    },
                    "sidebarSectionCollapsed": {
                        "folders": False,
                        "favorites": False,
                        "editing": False,
                        "archive": True,
                    },
                },
            )

            updated = ctx.client.patch(
                "/api/preferences",
                json={
                    "preferences": {
                        "themePreference": "dark",
                        "palettePreference": "winui",
                        "openFoldersOnClick": False,
                        "alternateRows": True,
                        "doubleClickDownload": True,
                        "favoriteItems": [
                            {"type": "folder", "id": folder_id},
                            {"type": "document", "id": document_id},
                        ],
                        "sidebarSectionSizes": {
                            "folders": 240,
                            "favorites": 150,
                            "archive": 130,
                            "editing": 90,
                        },
                        "sidebarSectionCollapsed": {
                            "folders": False,
                            "favorites": True,
                            "archive": False,
                            "editing": True,
                        },
                    },
                },
                headers=headers,
            )
            self.assertEqual(updated.status_code, 200, updated.text)
            self.assertEqual(updated.json()["preferences"]["themePreference"], "dark")
            self.assertEqual(updated.json()["preferences"]["palettePreference"], "winui")
            self.assertFalse(updated.json()["preferences"]["openFoldersOnClick"])
            self.assertTrue(updated.json()["preferences"]["alternateRows"])
            self.assertTrue(updated.json()["preferences"]["doubleClickDownload"])
            favorites = updated.json()["preferences"]["favoriteItems"]
            self.assertEqual(
                [(item["type"], item["id"]) for item in favorites],
                [
                    ("folder", folder_id),
                    ("document", document_id),
                ],
            )
            self.assertEqual(favorites[0]["path"], "Art")
            self.assertEqual(favorites[1]["path"], "Art/chest.fbx")
            with ctx.db() as db:
                stored_user = db.query(VaultUser).filter(VaultUser.subject == "artist").one()
                self.assertEqual(
                    stored_user.preferences["favoriteItems"],
                    [
                        {"type": "folder", "id": folder_id},
                        {"type": "document", "id": document_id},
                    ],
                )
            self.assertEqual(
                updated.json()["preferences"]["sidebarSectionSizes"],
                {
                    "folders": 240,
                    "favorites": 150,
                    "archive": 130,
                    "editing": 90,
                },
            )
            self.assertEqual(
                updated.json()["preferences"]["sidebarSectionCollapsed"],
                {
                    "folders": False,
                    "favorites": True,
                    "archive": False,
                    "editing": True,
                },
            )

            synced = ctx.client.get("/api/bootstrap", headers=headers)
            self.assertEqual(synced.status_code, 200, synced.text)
            self.assertEqual(synced.json()["preferences"], updated.json()["preferences"])

            html = ctx.client.get("/", headers=headers)
            self.assertEqual(html.status_code, 200, html.text)
            self.assertIn('"themePreference": "dark"', html.text)
            self.assertIn('"palettePreference": "winui"', html.text)

    def test_invalid_preferences_are_rejected_without_changing_existing_values(self) -> None:
        headers = auth_headers("artist", ["vault-admin"])

        with vault_test_client() as ctx:
            updated = ctx.client.patch(
                "/api/preferences",
                json={"preferences": {"themePreference": "light"}},
                headers=headers,
            )
            self.assertEqual(updated.status_code, 200, updated.text)

            invalid = ctx.client.patch(
                "/api/preferences",
                json={"preferences": {"themePreference": "solarized"}},
                headers=headers,
            )
            self.assertEqual(invalid.status_code, 400, invalid.text)

            unknown = ctx.client.patch(
                "/api/preferences",
                json={"preferences": {"sidebarWidth": 320}},
                headers=headers,
            )
            self.assertEqual(unknown.status_code, 400, unknown.text)

            invalid_favorites = ctx.client.patch(
                "/api/preferences",
                json={"preferences": {"favoriteItems": "Art"}},
                headers=headers,
            )
            self.assertEqual(invalid_favorites.status_code, 400, invalid_favorites.text)

            invalid_favorite_item = ctx.client.patch(
                "/api/preferences",
                json={"preferences": {"favoriteItems": [{"type": "document", "id": 0}]}},
                headers=headers,
            )
            self.assertEqual(invalid_favorite_item.status_code, 400, invalid_favorite_item.text)

            invalid_folder_favorite = ctx.client.patch(
                "/api/preferences",
                json={"preferences": {"favoriteItems": [{"type": "folder", "path": "Art"}]}},
                headers=headers,
            )
            self.assertEqual(invalid_folder_favorite.status_code, 400, invalid_folder_favorite.text)

            invalid_sidebar_size = ctx.client.patch(
                "/api/preferences",
                json={"preferences": {"sidebarSectionSizes": {"folders": "wide"}}},
                headers=headers,
            )
            self.assertEqual(invalid_sidebar_size.status_code, 400, invalid_sidebar_size.text)

            invalid_sidebar_collapse = ctx.client.patch(
                "/api/preferences",
                json={"preferences": {"sidebarSectionCollapsed": {"favorites": "yes"}}},
                headers=headers,
            )
            self.assertEqual(
                invalid_sidebar_collapse.status_code, 400, invalid_sidebar_collapse.text
            )

            current = ctx.client.get("/api/preferences", headers=headers)
            self.assertEqual(current.status_code, 200, current.text)
            self.assertEqual(current.json()["preferences"]["themePreference"], "light")
            self.assertNotIn("sidebarWidth", current.json()["preferences"])

    def test_favorites_resolve_current_targets_and_old_folder_is_not_navigable(self) -> None:
        headers = auth_headers("artist", ["vault-admin"])

        with vault_test_client() as ctx:
            created_parent = ctx.client.post("/folders", data={"folder": "Art"}, headers=headers)
            self.assertEqual(created_parent.status_code, 200, created_parent.text)
            parent_id = created_parent.json()["id"]
            created_child = ctx.client.post(
                "/folders",
                data={"folder": "Art/Props"},
                headers=headers,
            )
            self.assertEqual(created_child.status_code, 200, created_child.text)
            child_id = created_child.json()["id"]
            uploaded = ctx.client.post(
                "/documents",
                data={"folder": "Art/Props"},
                files={"file": ("crate.fbx", b"crate", "model/fbx")},
                headers=headers,
            )
            self.assertEqual(uploaded.status_code, 200, uploaded.text)
            document_id = uploaded.json()["id"]

            updated = ctx.client.patch(
                "/api/preferences",
                json={
                    "preferences": {
                        "favoriteItems": [
                            {"type": "folder", "id": parent_id},
                            {"type": "folder", "id": child_id},
                            {"type": "document", "id": document_id},
                        ]
                    }
                },
                headers=headers,
            )
            self.assertEqual(updated.status_code, 200, updated.text)

            renamed = ctx.client.post(
                "/api/rename",
                json={
                    "items": [{"type": "folder", "id": parent_id}],
                    "destination_folder": "",
                    "name": "Assets",
                },
                headers=headers,
            )
            self.assertEqual(renamed.status_code, 200, renamed.text)
            self.assertEqual(renamed.json()["failed"], [])

            preferences = ctx.client.get("/api/preferences", headers=headers)
            self.assertEqual(preferences.status_code, 200, preferences.text)
            favorites = preferences.json()["preferences"]["favoriteItems"]
            self.assertEqual(
                [(item["type"], item["id"]) for item in favorites],
                [
                    ("folder", parent_id),
                    ("folder", child_id),
                    ("document", document_id),
                ],
            )
            self.assertEqual(
                [item["path"] for item in favorites],
                [
                    "Assets",
                    "Assets/Props",
                    "Assets/Props/crate.fbx",
                ],
            )

            old_contents = ctx.client.get(
                "/api/folders/contents",
                params={"folder": "Art"},
                headers=headers,
            )
            self.assertEqual(old_contents.status_code, 404, old_contents.text)

            old_bootstrap = ctx.client.get(
                "/api/bootstrap",
                params={"folder": "Art"},
                headers=headers,
            )
            self.assertEqual(old_bootstrap.status_code, 404, old_bootstrap.text)

            new_contents = ctx.client.get(
                "/api/folders/contents",
                params={"folder": "Assets/Props"},
                headers=headers,
            )
            self.assertEqual(new_contents.status_code, 200, new_contents.text)
            self.assertEqual(new_contents.json()["documents"][0]["name"], "crate.fbx")

    def test_favorites_refresh_stale_folder_parent_before_access_filter(self) -> None:
        reader = user_context("reader", groups=["readers"], is_admin=False)

        with vault_runtime():
            with SessionLocal() as db:
                root = get_root_folder(db, "vault")
                readers = VaultGroup(name="readers")
                confidential = VaultGroup(name="confidential")
                db.add_all([readers, confidential])
                db.flush()
                add_permission(db, root, readers)

                project = get_or_create_folder_path(db, "Project")
                secret = get_or_create_folder_path(db, "Secret")
                db.flush()
                add_permission(db, secret, confidential)
                vault_user = VaultUser(
                    issuer="test",
                    subject="reader",
                    email="reader@example.com",
                    name="Reader",
                    preferences={"favoriteItems": [{"type": "folder", "id": project.id}]},
                )
                db.add(vault_user)
                db.commit()
                reader["vault_user_id"] = vault_user.id
                project_id = project.id
                secret_id = secret.id
                root_id = root.id

            stale_db = SessionLocal()
            try:
                stale_folder = stale_db.get(Folder, project_id)
                self.assertEqual(stale_folder.parent_id, root_id)
                self.assertEqual(stale_folder.parent.name, "Vault")

                with SessionLocal() as move_db:
                    moved_folder = move_db.get(Folder, project_id)
                    secret = move_db.get(Folder, secret_id)
                    moved_folder.parent = secret
                    moved_folder.parent_id = secret.id
                    move_db.commit()

                preferences = preferences_for_user(reader, stale_db)
                self.assertEqual(preferences["favoriteItems"], [])
            finally:
                stale_db.close()

    def test_favorites_refresh_stale_document_location_before_access_filter(self) -> None:
        reader = user_context("reader", groups=["readers"], is_admin=False)

        with vault_runtime():
            with SessionLocal() as db:
                root = get_root_folder(db, "vault")
                readers = VaultGroup(name="readers")
                confidential = VaultGroup(name="confidential")
                db.add_all([readers, confidential])
                db.flush()
                add_permission(db, root, readers)

                project = get_or_create_folder_path(db, "Project")
                secret = get_or_create_folder_path(db, "Secret")
                db.flush()
                add_permission(db, secret, confidential)
                doc = create_versioned_document(
                    db,
                    project,
                    name="brief.txt",
                    data=b"visible before move",
                )
                vault_user = VaultUser(
                    issuer="test",
                    subject="reader",
                    email="reader@example.com",
                    name="Reader",
                    preferences={"favoriteItems": [{"type": "document", "id": doc.id}]},
                )
                db.add(vault_user)
                db.commit()
                reader["vault_user_id"] = vault_user.id
                doc_id = doc.id
                secret_id = secret.id

            stale_db = SessionLocal()
            try:
                stale_doc = stale_db.get(Document, doc_id)
                self.assertEqual(stale_doc.folder.name, "Project")

                with SessionLocal() as move_db:
                    moved_doc = move_db.get(Document, doc_id)
                    secret = move_db.get(Folder, secret_id)
                    moved_doc.folder = secret
                    moved_doc.folder_id = secret.id
                    move_db.commit()

                preferences = preferences_for_user(reader, stale_db)
                self.assertEqual(preferences["favoriteItems"], [])
            finally:
                stale_db.close()


if __name__ == "__main__":
    unittest.main()
