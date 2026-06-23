import unittest

from tests.support import auth_headers, vault_test_client


class AppearanceOverrideTests(unittest.TestCase):
    def test_html_entry_uses_sanitized_host_appearance_headers(self) -> None:
        headers = {
            **auth_headers("admin", ["vault-admin"]),
            "X-Vault-Palette": "WinUI",
            "X-Vault-Theme": "dark",
        }

        with vault_test_client() as ctx:
            response = ctx.client.get("/", headers=headers)

        self.assertEqual(response.status_code, 200, response.text)
        self.assertIn('"palette": "winui"', response.text)
        self.assertIn('"theme": "dark"', response.text)
        self.assertIn("dataset.paletteOverride", response.text)
        self.assertIn("dataset.themeOverride", response.text)

    def test_invalid_host_appearance_headers_are_ignored(self) -> None:
        headers = {
            **auth_headers("admin", ["vault-admin"]),
            "X-Vault-Palette": "purple",
            "X-Vault-Theme": "solarized",
        }

        with vault_test_client() as ctx:
            response = ctx.client.get("/", headers=headers)

        self.assertEqual(response.status_code, 200, response.text)
        self.assertIn('"palette": null', response.text)
        self.assertIn('"theme": null', response.text)
        self.assertNotIn("purple", response.text)
        self.assertNotIn("solarized", response.text)


if __name__ == "__main__":
    unittest.main()
