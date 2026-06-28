import json
import os
import subprocess  # noqa: S404 - isolated config import checks need subprocesses
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
APP_VERSION = (ROOT / "VERSION").read_text().strip()
RELEASE_IMAGE = f"ghcr.io/willjallen/vault:v{APP_VERSION}"


class DockerDeployTests(unittest.TestCase):
    def run_config_script(self, env_overrides: dict[str, str]) -> dict[str, str]:
        env = os.environ.copy()
        for key in (
            "VAULT_DATA_DIR",
            "VAULT_DB_PATH",
            "VAULT_OBJECTS_PATH",
            "VAULT_LOCAL_OBJECTS_PATH",
            "VAULT_FILES_PATH",
            "VAULT_REQUIRE_SESSION_SECRET",
            "VAULT_DOCKER_RUNTIME",
            "VAULT_SESSION_SECRET",
            "VAULT_SITE_NAME",
        ):
            env.pop(key, None)
        env.update(env_overrides)

        script = """
        import json

        from app import config

        print(json.dumps({
            "data_dir": str(config.DATA_DIR),
            "db_path": str(config.DB_PATH),
            "objects_path": str(config.OBJECTS_PATH),
            "site_name": config.SITE_NAME,
        }))
        """
        completed = subprocess.run(  # noqa: S603 - fixed interpreter and repo-local script
            [sys.executable, "-c", textwrap.dedent(script)],
            cwd=ROOT,
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout)
        return json.loads(completed.stdout)

    def test_data_dir_drives_default_database_and_object_paths(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-data-dir-") as temp_dir:
            data_dir = Path(temp_dir) / "data"
            paths = self.run_config_script({"VAULT_DATA_DIR": str(data_dir)})

        self.assertEqual(paths["data_dir"], str(data_dir.resolve()))
        self.assertEqual(paths["db_path"], str((data_dir / "vault.db").resolve()))
        self.assertEqual(paths["objects_path"], str((data_dir / "objects").resolve()))

    def test_explicit_database_and_object_paths_override_data_dir(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-data-dir-") as temp_dir:
            base = Path(temp_dir)
            paths = self.run_config_script(
                {
                    "VAULT_DATA_DIR": str(base / "data"),
                    "VAULT_DB_PATH": str(base / "metadata" / "vault.db"),
                    "VAULT_OBJECTS_PATH": str(base / "blobs"),
                },
            )

        self.assertEqual(paths["db_path"], str((base / "metadata" / "vault.db").resolve()))
        self.assertEqual(paths["objects_path"], str((base / "blobs").resolve()))

    def test_site_name_defaults_and_can_be_overridden(self) -> None:
        self.assertEqual(self.run_config_script({})["site_name"], "Vault")
        self.assertEqual(
            self.run_config_script({"VAULT_SITE_NAME": "Studio Vault"})["site_name"],
            "Studio Vault",
        )

    def test_app_version_comes_only_from_version_file(self) -> None:
        env = os.environ.copy()
        env["VAULT_VERSION"] = "9.9.9"
        script = "from app.version import APP_VERSION; print(APP_VERSION)"

        completed = subprocess.run(  # noqa: S603 - fixed interpreter and repo-local import check
            [sys.executable, "-c", script],
            cwd=ROOT,
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout)
        self.assertEqual(completed.stdout.strip(), (ROOT / "VERSION").read_text().strip())

    def test_docker_runtime_requires_explicit_session_secret(self) -> None:
        env = os.environ.copy()
        env.pop("VAULT_REQUIRE_SESSION_SECRET", None)
        env["VAULT_DOCKER_RUNTIME"] = "1"
        env.pop("VAULT_SESSION_SECRET", None)
        script = "import app.config"

        completed = subprocess.run(  # noqa: S603 - fixed interpreter and repo-local import check
            [sys.executable, "-c", script],
            cwd=ROOT,
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("VAULT_SESSION_SECRET is required", completed.stderr)

    def test_runtime_validation_rejects_insecure_oidc_production_origin(self) -> None:
        env = os.environ.copy()
        env["VAULT_AUTH_MODE"] = "oidc"
        env["VAULT_DEV_MODE"] = "0"
        env.pop("VAULT_DEV_AUTH", None)
        env.pop("VAULT_OIDC_ALLOW_INSECURE_HTTP", None)
        env.pop("VAULT_PUBLIC_URL", None)
        env["VAULT_SESSION_SECRET"] = "test-session-secret"  # noqa: S105 - test-only secret
        env["VAULT_OIDC_ISSUER"] = "http://idp.example.com"
        env["VAULT_OIDC_CLIENT_ID"] = "vault"
        env["VAULT_OIDC_CLIENT_SECRET"] = "oidc-secret"  # noqa: S105 - test-only secret
        script = "from app import config; config.validate_runtime_config()"

        completed = subprocess.run(  # noqa: S603 - fixed interpreter and repo-local import check
            [sys.executable, "-c", script],
            cwd=ROOT,
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("VAULT_OIDC_ISSUER must use https", completed.stderr)

    def test_compose_uses_single_data_volume_and_production_auth_defaults(self) -> None:
        compose = (ROOT / "docker-compose.yml").read_text()

        self.assertIn(RELEASE_IMAGE, compose)
        self.assertIn("- vault-data:/data", compose)
        self.assertIn("vault-data:", compose)
        self.assertEqual(compose.count(":/data"), 1)
        self.assertIn("VAULT_SITE_NAME: ${VAULT_SITE_NAME:-Vault}", compose)
        self.assertIn("VAULT_DEV_MODE: ${VAULT_DEV_MODE:-0}", compose)
        self.assertIn(
            "VAULT_TTL_SWEEP_INTERVAL_SECONDS: ${VAULT_TTL_SWEEP_INTERVAL_SECONDS:-60}",
            compose,
        )
        self.assertIn("VAULT_EXPORT_WORKERS: ${VAULT_EXPORT_WORKERS:-1}", compose)
        self.assertIn("VAULT_TRANSFERS_PATH: ${VAULT_TRANSFERS_PATH:-/data/transfers}", compose)
        self.assertNotIn("/vault-metadata", compose)
        self.assertNotIn("/vault-objects", compose)
        self.assertIn("VAULT_DOCKER_RUNTIME: ${VAULT_DOCKER_RUNTIME:-1}", compose)
        self.assertIn("VAULT_REQUIRE_SESSION_SECRET: ${VAULT_REQUIRE_SESSION_SECRET:-}", compose)
        self.assertIn("VAULT_SESSION_SECRET: ${VAULT_SESSION_SECRET:-}", compose)
        self.assertIn(
            "VAULT_SESSION_COOKIE_NAME: ${VAULT_SESSION_COOKIE_NAME:-vault_session}",
            compose,
        )
        self.assertIn("VAULT_SESSION_COOKIE_SECURE: ${VAULT_SESSION_COOKIE_SECURE:-auto}", compose)
        self.assertNotIn("FORWARDED_ALLOW_IPS", compose)
        self.assertIn("VAULT_GZIP_MINIMUM_SIZE: ${VAULT_GZIP_MINIMUM_SIZE:-1024}", compose)
        self.assertIn("VAULT_GZIP_COMPRESSLEVEL: ${VAULT_GZIP_COMPRESSLEVEL:-6}", compose)
        self.assertIn("VAULT_CONTENT_SECURITY_POLICY: ${VAULT_CONTENT_SECURITY_POLICY:-}", compose)
        self.assertIn("VAULT_HSTS_PRELOAD: ${VAULT_HSTS_PRELOAD:-0}", compose)
        self.assertIn("VAULT_AUTH_MODE: ${VAULT_AUTH_MODE:-headers}", compose)
        self.assertIn(
            "VAULT_OIDC_STATE_COOKIE_NAME: ${VAULT_OIDC_STATE_COOKIE_NAME:-vault_oidc_state}",
            compose,
        )
        self.assertIn(
            "VAULT_OIDC_AUTHORIZATION_ENDPOINT: ${VAULT_OIDC_AUTHORIZATION_ENDPOINT:-}",
            compose,
        )
        self.assertIn(
            "VAULT_OIDC_ALLOW_INSECURE_HTTP: ${VAULT_OIDC_ALLOW_INSECURE_HTTP:-0}",
            compose,
        )
        self.assertIn("VAULT_OIDC_NONCE_BYTES: ${VAULT_OIDC_NONCE_BYTES:-24}", compose)
        self.assertIn(
            "VAULT_OIDC_DISCOVERY_TTL_SECONDS: ${VAULT_OIDC_DISCOVERY_TTL_SECONDS:-3600}",
            compose,
        )
        self.assertIn(
            "VAULT_OIDC_HTTP_TIMEOUT_SECONDS: ${VAULT_OIDC_HTTP_TIMEOUT_SECONDS:-8}",
            compose,
        )
        self.assertNotIn("VAULT_DEV_AUTH", compose)
        self.assertNotIn("dev-insecure-session-secret", compose)

    def test_dev_compose_is_the_only_compose_file_that_enables_dev_auth(self) -> None:
        compose = (ROOT / "docker-compose.yml").read_text()
        dev_compose = (ROOT / "docker-compose.dev.yml").read_text()

        self.assertNotIn("VAULT_DEV_AUTH", compose)
        self.assertIn("build:", dev_compose)
        self.assertIn("VAULT_AUTH_MODE: dev", dev_compose)
        self.assertIn("VAULT_SITE_NAME: ${VAULT_SITE_NAME:-Vault}", dev_compose)
        self.assertIn('VAULT_DEV_MODE: "1"', dev_compose)
        self.assertNotIn("VAULT_VERSION", dev_compose)
        self.assertIn(
            "VAULT_TTL_SWEEP_INTERVAL_SECONDS: ${VAULT_TTL_SWEEP_INTERVAL_SECONDS:-60}",
            dev_compose,
        )
        self.assertIn('VAULT_DEV_AUTH: "1"', dev_compose)
        self.assertIn("dev-insecure-session-secret-change-me", dev_compose)

    def test_dockerfile_declares_clean_runtime_contract(self) -> None:
        dockerfile = (ROOT / "Dockerfile").read_text()

        self.assertIn("FROM node:22-slim AS assets", dockerfile)
        self.assertIn("RUN npm ci", dockerfile)
        self.assertIn("RUN npm run build:assets", dockerfile)
        self.assertIn("FROM rust:1.95-slim-bookworm AS rust-builder", dockerfile)
        self.assertIn("RUN cargo build --release -p vault-server", dockerfile)
        self.assertIn("FROM debian:bookworm-slim", dockerfile)
        self.assertIn(
            "COPY --from=rust-builder --chown=vault:vault "
            "/build/target/release/vault-server /app/vault-server",
            dockerfile,
        )
        self.assertIn("COPY --from=assets --chown=vault:vault /build/app/static/dist", dockerfile)
        self.assertIn("VAULT_DATA_DIR=/data", dockerfile)
        self.assertIn("VAULT_DB_PATH=/data/vault.db", dockerfile)
        self.assertIn("VAULT_OBJECTS_PATH=/data/objects", dockerfile)
        self.assertIn("VAULT_STATIC_DIR=/app/app/static", dockerfile)
        self.assertIn("VAULT_DOCKER_RUNTIME=1", dockerfile)
        self.assertIn('VOLUME ["/data"]', dockerfile)
        self.assertIn("EXPOSE 8000", dockerfile)
        self.assertIn("USER vault", dockerfile)
        self.assertIn("HEALTHCHECK", dockerfile)
        self.assertIn("curl -fsS --max-time 2 http://127.0.0.1:8000/health", dockerfile)
        self.assertIn('CMD ["/app/vault-server"]', dockerfile)
        self.assertNotIn("uvicorn", dockerfile)
        self.assertNotIn("python:3.11", dockerfile)
        self.assertNotIn("VAULT_VERSION", dockerfile)
        self.assertNotIn("/vault-metadata", dockerfile)
        self.assertNotIn("/vault-objects", dockerfile)

    def test_generated_static_assets_are_ignored_build_output(self) -> None:
        dockerignore = (ROOT / ".dockerignore").read_text()
        gitignore = (ROOT / ".gitignore").read_text()

        self.assertIn("app/static/dist/", dockerignore)
        self.assertIn("target/", dockerignore)
        self.assertIn("app/static/dist/", gitignore)

    def test_semver_tag_workflow_builds_and_publishes_ghcr_image(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "docker-image.yml").read_text()

        self.assertIn('      - "v*.*.*"', workflow)
        self.assertIn('      - "[0-9]*.[0-9]*.[0-9]*"', workflow)
        self.assertIn("Validate semantic version tag", workflow)
        self.assertIn("ghcr.io/${GITHUB_REPOSITORY,,}", workflow)
        self.assertIn("docker/login-action@v3", workflow)
        self.assertIn("docker/metadata-action@v5", workflow)
        self.assertIn("docker/build-push-action@v6", workflow)
        self.assertIn("push: true", workflow)
        self.assertNotIn("VAULT_VERSION", workflow)
        self.assertIn("type=semver,pattern={{version}}", workflow)
        self.assertNotIn("type=semver,pattern={{major}}.{{minor}}", workflow)
        self.assertNotIn("type=semver,pattern={{major}}", workflow)
        self.assertNotIn("type=raw,value=latest", workflow)


if __name__ == "__main__":
    unittest.main()
