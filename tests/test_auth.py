import os
import unittest
from pathlib import Path
from unittest.mock import patch

from fastapi import HTTPException
from starlette.datastructures import Headers

from app.auth import current_user


class FakeRequest:
    def __init__(self, headers: dict[str, str] | None = None) -> None:
        self.headers = Headers(headers or {})


class AuthTests(unittest.TestCase):
    def test_missing_identity_headers_reject_without_dev_auth(self) -> None:
        with patch.dict(os.environ, {"VAULT_DEV_AUTH": "0"}, clear=True):
            with self.assertRaises(HTTPException) as raised:
                current_user(FakeRequest())

        self.assertEqual(raised.exception.status_code, 401)
        self.assertEqual(raised.exception.detail, "Authentication required")

    def test_blank_remote_user_is_not_persisted_as_identity(self) -> None:
        with patch.dict(os.environ, {"VAULT_DEV_AUTH": "0"}, clear=True):
            with self.assertRaises(HTTPException) as raised:
                current_user(FakeRequest({"Remote-User": "   "}))

        self.assertEqual(raised.exception.status_code, 401)

    def test_remote_identity_headers_are_stripped(self) -> None:
        user = current_user(
            FakeRequest(
                {
                    "Remote-User": "  alice  ",
                    "Remote-Name": "  Alice Example  ",
                    "Remote-Email": "  alice@example.com  ",
                    "Remote-Groups": " vault-users, vault-admin ",
                },
            ),
        )

        self.assertEqual(user["id"], "alice")
        self.assertEqual(user["name"], "Alice Example")
        self.assertEqual(user["email"], "alice@example.com")
        self.assertEqual(user["groups"], ["vault-admin", "vault-users"])
        self.assertTrue(user["is_admin"])

    def test_dev_auth_is_rejected_on_nonlocal_base_domain(self) -> None:
        with patch.dict(
            os.environ,
            {
                "BASE_DOMAIN": "vault.example.com",
                "VAULT_DEV_AUTH": "1",
                "VAULT_DEV_USER": "local-admin",
            },
            clear=True,
        ):
            with self.assertRaises(HTTPException) as raised:
                current_user(FakeRequest())

        self.assertEqual(raised.exception.status_code, 401)

    def test_dev_auth_is_allowed_on_local_base_domain(self) -> None:
        with patch.dict(
            os.environ,
            {
                "BASE_DOMAIN": "localhost",
                "VAULT_DEV_AUTH": "1",
                "VAULT_DEV_USER": "dev-user",
                "VAULT_DEV_NAME": "Dev User",
                "VAULT_DEV_GROUPS": "vault-users,vault-admin",
            },
            clear=True,
        ):
            user = current_user(FakeRequest())

        self.assertEqual(user["id"], "dev-user")
        self.assertEqual(user["name"], "Dev User")
        self.assertEqual(user["groups"], ["vault-admin", "vault-users"])
        self.assertTrue(user["is_admin"])

    def test_compose_does_not_publish_dev_auth_to_all_interfaces_by_default(self) -> None:
        compose = (Path(__file__).resolve().parents[1] / "docker-compose.yml").read_text()

        self.assertIn('"127.0.0.1:8000:8000"', compose)
        self.assertIn("VAULT_DEV_AUTH: ${VAULT_DEV_AUTH:-0}", compose)
        self.assertNotIn('"0.0.0.0:8000:8000"', compose)
        self.assertNotIn('VAULT_DEV_AUTH: "1"', compose)


if __name__ == "__main__":
    unittest.main()
