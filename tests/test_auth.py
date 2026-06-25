import os
import unittest
from pathlib import Path

from fastapi import HTTPException
from starlette.datastructures import Headers
from tests.support import vault_runtime

from app.auth import current_user
from app.config import HEADER_AUTH_ISSUER
from app.models import Folder, FolderPermission, VaultGroup, VaultGroupMembership, VaultUser


class HeaderRequest:
    def __init__(self, headers: dict[str, str] | None = None) -> None:
        self.headers = Headers(headers or {})


class AuthTests(unittest.TestCase):
    def test_missing_identity_headers_reject_without_dev_auth(self) -> None:
        with vault_runtime(auth_mode="headers") as ctx, ctx.db() as db:
            with self.assertRaises(HTTPException) as raised:
                current_user(HeaderRequest(), db)

            self.assertEqual(raised.exception.status_code, 401)
            self.assertEqual(raised.exception.detail, "Authentication required")

    def test_header_identity_is_stripped_and_groups_are_synced(self) -> None:
        request = HeaderRequest(
            {
                "Remote-User": "  alice  ",
                "Remote-Name": "  Alice Example  ",
                "Remote-Email": "  alice@example.com  ",
                "Remote-Groups": " vault-users, vault-admin ",
            },
        )

        with vault_runtime(auth_mode="headers") as ctx, ctx.db() as db:
            user = current_user(request, db)
            vault_users = db.query(VaultGroup).filter_by(name="vault-users").one()
            roots = db.query(Folder).filter_by(is_root=True).all()
            self.assertEqual(len(roots), 2)
            for root in roots:
                rule = (
                    db.query(FolderPermission)
                    .filter_by(folder_id=root.id, group_id=vault_users.id)
                    .one()
                )
                self.assertTrue(rule.can_view)
                self.assertTrue(rule.can_read)
                self.assertTrue(rule.can_write)

        self.assertEqual(user["subject"], "alice")
        self.assertEqual(user["name"], "Alice Example")
        self.assertEqual(user["email"], "alice@example.com")
        self.assertEqual(user["groups"], ["vault-admin", "vault-users"])
        self.assertTrue(user["is_admin"])

    def test_disabled_header_user_request_does_not_sync_groups_or_profile(self) -> None:
        request = HeaderRequest(
            {
                "Remote-User": "disabled",
                "Remote-Name": "Updated Name",
                "Remote-Email": "updated@example.com",
                "Remote-Groups": "new-disabled-group",
            },
        )

        with vault_runtime(auth_mode="headers") as ctx, ctx.db() as db:
            user = VaultUser(
                issuer=HEADER_AUTH_ISSUER,
                subject="disabled",
                email="old@example.com",
                name="Disabled User",
                is_active=False,
            )
            db.add(user)
            db.commit()
            user_id = user.id

            with self.assertRaises(HTTPException) as raised:
                current_user(request, db)

            self.assertEqual(raised.exception.status_code, 403)
            self.assertEqual(raised.exception.detail, "User is disabled")

            db.expire_all()
            disabled_user = db.get(VaultUser, user_id)
            self.assertEqual(disabled_user.name, "Disabled User")
            self.assertEqual(disabled_user.email, "old@example.com")
            self.assertFalse(disabled_user.is_admin)
            self.assertEqual(db.query(VaultGroup).filter_by(name="new-disabled-group").count(), 0)
            self.assertEqual(db.query(VaultGroupMembership).count(), 0)
            self.assertEqual(
                db.query(FolderPermission)
                .join(VaultGroup)
                .filter(VaultGroup.name == "new-disabled-group")
                .count(),
                0,
            )

    def test_dev_auth_requires_local_base_domain(self) -> None:
        with vault_runtime(auth_mode="dev") as ctx, ctx.db() as db:
            os.environ["BASE_DOMAIN"] = "vault.example.com"

            with self.assertRaises(HTTPException) as raised:
                current_user(HeaderRequest(), db)

            self.assertEqual(raised.exception.status_code, 401)

    def test_dev_auth_syncs_configured_groups_on_local_domain(self) -> None:
        with vault_runtime(auth_mode="dev") as ctx, ctx.db() as db:
            os.environ["BASE_DOMAIN"] = "localhost"
            os.environ["VAULT_DEV_USER"] = "dev-user"
            os.environ["VAULT_DEV_NAME"] = "Dev User"
            os.environ["VAULT_DEV_GROUPS"] = "vault-users,vault-admin"

            user = current_user(HeaderRequest(), db)

        self.assertEqual(user["subject"], "dev-user")
        self.assertEqual(user["name"], "Dev User")
        self.assertEqual(user["groups"], ["vault-admin", "vault-users"])
        self.assertTrue(user["is_admin"])

    def test_compose_does_not_publish_dev_auth_to_all_interfaces_by_default(self) -> None:
        compose = (Path(__file__).resolve().parents[1] / "docker-compose.yml").read_text()
        dev_compose = (Path(__file__).resolve().parents[1] / "docker-compose.dev.yml").read_text()

        self.assertIn("${VAULT_BIND_ADDRESS:-127.0.0.1}:${VAULT_PORT:-8000}:8000", compose)
        self.assertIn("VAULT_AUTH_MODE: ${VAULT_AUTH_MODE:-headers}", compose)
        self.assertNotIn("VAULT_DEV_AUTH", compose)
        self.assertNotIn("0.0.0.0:8000:8000", compose)
        self.assertNotIn("VAULT_AUTH_MODE: ${VAULT_AUTH_MODE:-dev}", compose)
        self.assertNotIn("dev-insecure-session-secret", compose)
        self.assertIn("VAULT_AUTH_MODE: dev", dev_compose)
        self.assertIn('VAULT_DEV_AUTH: "1"', dev_compose)


if __name__ == "__main__":
    unittest.main()
