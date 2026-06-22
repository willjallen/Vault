import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class AuthTests(unittest.TestCase):
    def run_script(self, script: str, extra_env: dict[str, str] | None = None) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-auth-") as temp_dir:
            env = os.environ.copy()
            env["VAULT_DB_PATH"] = str(Path(temp_dir) / "vault.db")
            env["VAULT_OBJECTS_PATH"] = str(Path(temp_dir) / "objects")
            env.update(extra_env or {})

            completed = subprocess.run(
                [sys.executable, "-c", textwrap.dedent(script)],
                check=False,
                cwd=Path(__file__).resolve().parents[1],
                env=env,
                stderr=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout)

    def test_missing_identity_headers_reject_without_dev_auth(self) -> None:
        self.run_script(
            """
            from fastapi import HTTPException
            from starlette.datastructures import Headers

            from app.auth import current_user
            from app.db import SessionLocal, init_db


            class FakeRequest:
                headers = Headers({})


            init_db()
            with SessionLocal() as db:
                try:
                    current_user(FakeRequest(), db)
                except HTTPException as exc:
                    assert exc.status_code == 401
                    assert exc.detail == "Authentication required"
                else:
                    raise AssertionError("missing headers unexpectedly authenticated")
            """,
            {"VAULT_AUTH_MODE": "headers", "VAULT_DEV_AUTH": "0"},
        )

    def test_header_identity_is_stripped_and_groups_are_synced(self) -> None:
        self.run_script(
            """
            from starlette.datastructures import Headers

            from app.auth import current_user
            from app.db import SessionLocal, init_db
            from app.models import Folder, FolderPermission, VaultGroup


            class FakeRequest:
                headers = Headers(
                    {
                        "Remote-User": "  alice  ",
                        "Remote-Name": "  Alice Example  ",
                        "Remote-Email": "  alice@example.com  ",
                        "Remote-Groups": " vault-users, vault-admin ",
                    },
                )


            init_db()
            with SessionLocal() as db:
                user = current_user(FakeRequest(), db)
                vault_users = db.query(VaultGroup).filter_by(name="vault-users").one()
                roots = db.query(Folder).filter_by(is_root=True).all()
                assert len(roots) == 2
                for root in roots:
                    rule = (
                        db.query(FolderPermission)
                        .filter_by(folder_id=root.id, group_id=vault_users.id)
                        .one()
                    )
                    assert rule.can_view and rule.can_read and rule.can_write

            assert user["subject"] == "alice"
            assert user["name"] == "Alice Example"
            assert user["email"] == "alice@example.com"
            assert user["groups"] == ["vault-admin", "vault-users"]
            assert user["is_admin"] is True
            """,
            {"VAULT_AUTH_MODE": "headers", "VAULT_DEV_AUTH": "0"},
        )

    def test_dev_auth_requires_local_base_domain(self) -> None:
        self.run_script(
            """
            from fastapi import HTTPException
            from starlette.datastructures import Headers

            from app.auth import current_user
            from app.db import SessionLocal, init_db


            class FakeRequest:
                headers = Headers({})


            init_db()
            with SessionLocal() as db:
                try:
                    current_user(FakeRequest(), db)
                except HTTPException as exc:
                    assert exc.status_code == 401
                else:
                    raise AssertionError("dev auth worked on a nonlocal domain")
            """,
            {
                "BASE_DOMAIN": "vault.example.com",
                "VAULT_AUTH_MODE": "dev",
                "VAULT_DEV_AUTH": "1",
            },
        )

    def test_dev_auth_syncs_configured_groups_on_local_domain(self) -> None:
        self.run_script(
            """
            from starlette.datastructures import Headers

            from app.auth import current_user
            from app.db import SessionLocal, init_db


            class FakeRequest:
                headers = Headers({})


            init_db()
            with SessionLocal() as db:
                user = current_user(FakeRequest(), db)

            assert user["subject"] == "dev-user"
            assert user["name"] == "Dev User"
            assert user["groups"] == ["vault-admin", "vault-users"]
            assert user["is_admin"] is True
            """,
            {
                "BASE_DOMAIN": "localhost",
                "VAULT_AUTH_MODE": "dev",
                "VAULT_DEV_AUTH": "1",
                "VAULT_DEV_USER": "dev-user",
                "VAULT_DEV_NAME": "Dev User",
                "VAULT_DEV_GROUPS": "vault-users,vault-admin",
            },
        )

    def test_compose_does_not_publish_dev_auth_to_all_interfaces_by_default(self) -> None:
        compose = (Path(__file__).resolve().parents[1] / "docker-compose.yml").read_text()

        self.assertIn('"127.0.0.1:8000:8000"', compose)
        self.assertIn("VAULT_AUTH_MODE: ${VAULT_AUTH_MODE:-headers}", compose)
        self.assertIn("VAULT_DEV_AUTH: ${VAULT_DEV_AUTH:-0}", compose)
        self.assertNotIn('"0.0.0.0:8000:8000"', compose)
        self.assertNotIn("VAULT_AUTH_MODE: ${VAULT_AUTH_MODE:-dev}", compose)
        self.assertNotIn("VAULT_DEV_AUTH: ${VAULT_DEV_AUTH:-1}", compose)


if __name__ == "__main__":
    unittest.main()
