import unittest

from tests.support import auth_headers, vault_test_client


class UserPreferenceTests(unittest.TestCase):
    def test_preferences_sync_through_bootstrap_and_api(self) -> None:
        headers = auth_headers("artist", ["vault-admin"])

        with vault_test_client() as ctx:
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

            current = ctx.client.get("/api/preferences", headers=headers)
            self.assertEqual(current.status_code, 200, current.text)
            self.assertEqual(current.json()["preferences"]["themePreference"], "light")
            self.assertNotIn("sidebarWidth", current.json()["preferences"])


if __name__ == "__main__":
    unittest.main()
