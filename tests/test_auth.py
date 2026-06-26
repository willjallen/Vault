import os
import unittest
from pathlib import Path

from fastapi import HTTPException
from sqlalchemy import event
from starlette.datastructures import URL, Headers
from tests.support import vault_runtime

import app.auth as auth_module
from app.auth import current_user
from app.config import HEADER_AUTH_ISSUER
from app.models import Folder, FolderPermission, VaultGroup, VaultGroupMembership, VaultUser


class HeaderRequest:
    def __init__(self, headers: dict[str, str] | None = None) -> None:
        self.headers = Headers(headers or {})


class CookieRequest:
    method = "GET"

    def __init__(
        self,
        cookies: dict[str, str] | None = None,
        url: str = "http://testserver/api/bootstrap",
    ) -> None:
        self.cookies = cookies or {}
        self.url = URL(url)


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

    def test_header_admin_group_removal_revokes_admin_context(self) -> None:
        admin_request = HeaderRequest(
            {
                "Remote-User": "alice",
                "Remote-Name": "Alice Example",
                "Remote-Email": "alice@example.com",
                "Remote-Groups": "vault-users,vault-admin",
            },
        )
        user_request = HeaderRequest(
            {
                "Remote-User": "alice",
                "Remote-Name": "Alice Example",
                "Remote-Email": "alice@example.com",
                "Remote-Groups": "vault-users",
            },
        )

        with vault_runtime(auth_mode="headers") as ctx, ctx.db() as db:
            first_context = current_user(admin_request, db)
            self.assertTrue(first_context["is_admin"])

            second_context = current_user(user_request, db)

            self.assertFalse(second_context["is_admin"])
            self.assertEqual(second_context["groups"], ["vault-users"])
            stored_user = db.query(VaultUser).filter_by(subject="alice").one()
            self.assertFalse(stored_user.is_admin)

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

    def test_oidc_session_cookie_requires_expiration(self) -> None:
        with vault_runtime(auth_mode="oidc") as ctx, ctx.db() as db:
            user = VaultUser(
                issuer="issuer",
                subject="alice",
                email="alice@example.com",
                name="Alice",
                is_active=True,
            )
            db.add(user)
            db.commit()

            cookie = auth_module._sign_payload({"uid": user.id})
            request = CookieRequest({auth_module.SESSION_COOKIE_NAME: cookie})

            with self.assertRaises(HTTPException) as raised:
                current_user(request, db)

            self.assertEqual(raised.exception.status_code, 401)
            self.assertEqual(raised.exception.detail, "Authentication required")

    def test_oidc_session_cookie_rejects_boolean_user_id(self) -> None:
        with vault_runtime(auth_mode="oidc") as ctx, ctx.db() as db:
            user = VaultUser(
                issuer="issuer",
                subject="alice",
                email="alice@example.com",
                name="Alice",
                is_active=True,
            )
            db.add(user)
            db.commit()
            self.assertEqual(user.id, 1)

            cookie = auth_module._sign_payload(
                {"uid": True, "exp": auth_module.time.time() + 60},
            )
            request = CookieRequest({auth_module.SESSION_COOKIE_NAME: cookie})

            with self.assertRaises(HTTPException) as raised:
                current_user(request, db)

            self.assertEqual(raised.exception.status_code, 401)
            self.assertEqual(raised.exception.detail, "Authentication required")

    def test_oidc_session_cookie_rejects_non_ascii_payload(self) -> None:
        with vault_runtime(auth_mode="oidc") as ctx, ctx.db() as db:
            request = CookieRequest(
                {auth_module.SESSION_COOKIE_NAME: "not-ascii-\u2603.signature"},
            )

            with self.assertRaises(HTTPException) as raised:
                current_user(request, db)

            self.assertEqual(raised.exception.status_code, 401)
            self.assertEqual(raised.exception.detail, "Authentication required")

    def test_oidc_browser_get_redirects_to_login_with_return_path(self) -> None:
        with vault_runtime(auth_mode="oidc") as ctx, ctx.db() as db:
            request = CookieRequest(url="http://testserver/s/share-code?preview=1")

            with self.assertRaises(HTTPException) as raised:
                current_user(request, db)

            self.assertEqual(raised.exception.status_code, 303)
            self.assertEqual(
                raised.exception.headers["Location"],
                "/login?rd=/s/share-code%3Fpreview%3D1",
            )

    def test_oidc_login_rejects_insecure_authorization_endpoint(self) -> None:
        original_auth_mode = auth_module.AUTH_MODE
        original_discovery = auth_module._oidc_discovery
        try:
            auth_module.AUTH_MODE = "oidc"
            auth_module._oidc_discovery = lambda: {
                "authorization_endpoint": "http://idp.example.com/auth"
            }
            with self.assertRaises(HTTPException) as raised:
                auth_module.oidc_login_response(CookieRequest())

            self.assertEqual(raised.exception.status_code, 502)
            self.assertEqual(
                raised.exception.detail,
                "OIDC authorization endpoint must use HTTPS",
            )
        finally:
            auth_module.AUTH_MODE = original_auth_mode
            auth_module._oidc_discovery = original_discovery

    def test_oidc_client_auth_none_does_not_send_configured_secret(self) -> None:
        original_client_auth = auth_module.OIDC_CLIENT_AUTH
        original_client_id = auth_module.OIDC_CLIENT_ID
        original_client_secret = auth_module.OIDC_CLIENT_SECRET
        original_redirect_uri = auth_module.OIDC_REDIRECT_URI
        original_http_json = auth_module._http_json
        captured: dict[str, object] = {}

        def fake_http_json(
            url: str,
            data: dict[str, str] | None = None,
            headers: dict[str, str] | None = None,
        ) -> dict[str, object]:
            captured["url"] = url
            captured["data"] = data or {}
            captured["headers"] = headers or {}
            return {"id_token": "token"}

        try:
            auth_module.OIDC_CLIENT_AUTH = "none"
            auth_module.OIDC_CLIENT_ID = "public-client"
            auth_module.OIDC_CLIENT_SECRET = "configured-but-unused"  # noqa: S105
            auth_module.OIDC_REDIRECT_URI = "https://vault.example.com/auth/callback"
            auth_module._http_json = fake_http_json

            token = auth_module._exchange_code_for_token(
                CookieRequest(),
                "auth-code",
                {"token_endpoint": "https://idp.example.com/token"},
            )

            self.assertEqual(token, {"id_token": "token"})
            self.assertNotIn("client_secret", captured["data"])
            self.assertNotIn("Authorization", captured["headers"])
        finally:
            auth_module.OIDC_CLIENT_AUTH = original_client_auth
            auth_module.OIDC_CLIENT_ID = original_client_id
            auth_module.OIDC_CLIENT_SECRET = original_client_secret
            auth_module.OIDC_REDIRECT_URI = original_redirect_uri
            auth_module._http_json = original_http_json

    def test_cookie_secure_auto_honors_https_public_url(self) -> None:
        original_public_url = auth_module.PUBLIC_URL
        original_cookie_secure = auth_module.SESSION_COOKIE_SECURE
        request = CookieRequest()
        try:
            auth_module.SESSION_COOKIE_SECURE = "auto"
            auth_module.PUBLIC_URL = ""
            self.assertFalse(auth_module._cookie_secure(request))

            auth_module.PUBLIC_URL = "https://vault.example.com"
            self.assertTrue(auth_module._cookie_secure(request))

            auth_module.SESSION_COOKIE_SECURE = "false"
            self.assertFalse(auth_module._cookie_secure(request))

            auth_module.SESSION_COOKIE_SECURE = "true"
            self.assertTrue(auth_module._cookie_secure(request))
        finally:
            auth_module.PUBLIC_URL = original_public_url
            auth_module.SESSION_COOKIE_SECURE = original_cookie_secure

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

    def test_identity_upsert_recovers_from_concurrent_user_insert(self) -> None:
        with vault_runtime(auth_mode="headers") as ctx, ctx.db() as db:
            inserted = False

            def insert_duplicate_user(session, _flush_context, _instances):
                nonlocal inserted
                if inserted:
                    return
                inserted = True
                with ctx.db() as other_db:
                    other_db.add(
                        VaultUser(
                            issuer=HEADER_AUTH_ISSUER,
                            subject="race",
                            email="race@example.com",
                            name="Race Winner",
                        ),
                    )
                    other_db.commit()

            event.listen(db, "before_flush", insert_duplicate_user)
            try:
                user = auth_module._upsert_vault_user(
                    db,
                    HEADER_AUTH_ISSUER,
                    "race",
                    "race@example.com",
                    "Race Loser",
                    {"vault-users"},
                )
            finally:
                event.remove(db, "before_flush", insert_duplicate_user)

            self.assertEqual(user.subject, "race")
            self.assertEqual(user.name, "Race Loser")
            self.assertEqual(db.query(VaultUser).filter_by(subject="race").count(), 1)
            self.assertEqual(db.query(VaultGroup).filter_by(name="vault-users").count(), 1)
            self.assertEqual(db.query(VaultGroupMembership).count(), 1)

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
