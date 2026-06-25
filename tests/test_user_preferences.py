import unittest

from tests.support import auth_headers, vault_test_client

from app.models import VaultUser


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
            self.assertEqual([(item["type"], item["id"]) for item in favorites], [
                ("folder", folder_id),
                ("document", document_id),
            ])
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
            self.assertEqual(
                invalid_folder_favorite.status_code, 400, invalid_folder_favorite.text
            )

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
            self.assertEqual([(item["type"], item["id"]) for item in favorites], [
                ("folder", parent_id),
                ("folder", child_id),
                ("document", document_id),
            ])
            self.assertEqual([item["path"] for item in favorites], [
                "Assets",
                "Assets/Props",
                "Assets/Props/crate.fbx",
            ])

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


if __name__ == "__main__":
    unittest.main()
