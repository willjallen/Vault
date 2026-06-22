# Copyright (c) 2024 The Allen Family
"""Configuration helpers for the vault service."""

import os
import secrets
from pathlib import Path

BASE_DOMAIN = os.getenv("BASE_DOMAIN", "family.localhost")
DATA_DIR = Path(os.getenv("VAULT_DATA_DIR", "/data")).resolve()
DB_PATH = Path(os.getenv("VAULT_DB_PATH", str(DATA_DIR / "vault.db"))).resolve()
PUBLIC_URL = os.getenv("VAULT_PUBLIC_URL", "").strip().rstrip("/")
_REQUIRE_SESSION_SECRET_ENV = os.getenv("VAULT_REQUIRE_SESSION_SECRET")
_DOCKER_RUNTIME_ENV = os.getenv("VAULT_DOCKER_RUNTIME", "0")
REQUIRE_SESSION_SECRET = (
    _REQUIRE_SESSION_SECRET_ENV if _REQUIRE_SESSION_SECRET_ENV is not None else _DOCKER_RUNTIME_ENV
).strip().lower() in {
    "1",
    "true",
    "yes",
    "on",
}

AUTH_MODE = os.getenv(
    "VAULT_AUTH_MODE",
    "dev" if os.getenv("VAULT_DEV_AUTH", "").strip().lower() in {"1", "true", "yes", "on"} else "headers",
).strip().lower()
SESSION_COOKIE_NAME = os.getenv("VAULT_SESSION_COOKIE_NAME", "vault_session").strip()
_SESSION_SECRET_ENV = os.getenv("VAULT_SESSION_SECRET", "").strip()
_OIDC_CLIENT_SECRET_ENV = os.getenv("VAULT_OIDC_CLIENT_SECRET", "").strip()
if REQUIRE_SESSION_SECRET and not _SESSION_SECRET_ENV:
    raise RuntimeError("VAULT_SESSION_SECRET is required when VAULT_REQUIRE_SESSION_SECRET=1")
SESSION_SECRET = _SESSION_SECRET_ENV or _OIDC_CLIENT_SECRET_ENV or "dev-insecure-session-secret"
SESSION_MAX_AGE_SECONDS = int(os.getenv("VAULT_SESSION_MAX_AGE_SECONDS", "604800"))
BOOTSTRAP_ADMIN_EMAILS = {
    item.strip().lower()
    for item in os.getenv("VAULT_BOOTSTRAP_ADMIN_EMAILS", "").split(",")
    if item.strip()
}
ADMIN_GROUPS = {
    item.strip().lower()
    for item in os.getenv("VAULT_ADMIN_GROUPS", "admin,vault-admin").split(",")
    if item.strip()
}
HEADER_AUTH_ISSUER = os.getenv("VAULT_HEADER_AUTH_ISSUER", "headers").strip() or "headers"
DEV_AUTH_ISSUER = os.getenv("VAULT_DEV_AUTH_ISSUER", "dev").strip() or "dev"

OIDC_ISSUER = os.getenv("VAULT_OIDC_ISSUER", "").strip().rstrip("/")
OIDC_CLIENT_ID = os.getenv("VAULT_OIDC_CLIENT_ID", "").strip()
OIDC_CLIENT_SECRET = _OIDC_CLIENT_SECRET_ENV
OIDC_SCOPES = os.getenv("VAULT_OIDC_SCOPES", "openid email profile").strip()
OIDC_REDIRECT_URI = os.getenv("VAULT_OIDC_REDIRECT_URI", "").strip()
OIDC_CLIENT_AUTH = os.getenv("VAULT_OIDC_CLIENT_AUTH", "client_secret_basic").strip().lower()
OIDC_STATE_COOKIE_NAME = os.getenv("VAULT_OIDC_STATE_COOKIE_NAME", "vault_oidc_state").strip()
OIDC_GROUPS_CLAIM = os.getenv("VAULT_OIDC_GROUPS_CLAIM", "groups").strip() or "groups"
OIDC_EMAIL_CLAIM = os.getenv("VAULT_OIDC_EMAIL_CLAIM", "email").strip() or "email"
OIDC_NAME_CLAIM = os.getenv("VAULT_OIDC_NAME_CLAIM", "name").strip() or "name"
OIDC_USERNAME_CLAIM = (
    os.getenv("VAULT_OIDC_USERNAME_CLAIM", "preferred_username").strip() or "preferred_username"
)
OIDC_NONCE_BYTES = int(os.getenv("VAULT_OIDC_NONCE_BYTES", "24"))
OIDC_DISCOVERY_TTL_SECONDS = int(os.getenv("VAULT_OIDC_DISCOVERY_TTL_SECONDS", "3600"))
OIDC_HTTP_TIMEOUT_SECONDS = float(os.getenv("VAULT_OIDC_HTTP_TIMEOUT_SECONDS", "8"))

if OIDC_NONCE_BYTES < 16:
    OIDC_NONCE_BYTES = 16


def new_token_urlsafe() -> str:
    return secrets.token_urlsafe(OIDC_NONCE_BYTES)

STORAGE_BACKEND = os.getenv("VAULT_STORAGE_BACKEND", "local").strip().lower()
STORAGE_PREFIX = os.getenv("VAULT_STORAGE_PREFIX", "objects").strip().strip("/")
OBJECTS_PATH = Path(
    os.getenv(
        "VAULT_OBJECTS_PATH",
        os.getenv(
            "VAULT_LOCAL_OBJECTS_PATH",
            os.getenv("VAULT_FILES_PATH", str(DATA_DIR / "objects")),
        ),
    )
).resolve()

S3_BUCKET = os.getenv("VAULT_S3_BUCKET", "").strip()
S3_REGION = os.getenv("VAULT_S3_REGION", "us-east-1").strip()
S3_ENDPOINT_URL = os.getenv("VAULT_S3_ENDPOINT_URL", "").strip() or None
S3_ACCESS_KEY_ID = os.getenv("VAULT_S3_ACCESS_KEY_ID", os.getenv("AWS_ACCESS_KEY_ID", "")).strip()
S3_SECRET_ACCESS_KEY = os.getenv(
    "VAULT_S3_SECRET_ACCESS_KEY",
    os.getenv("AWS_SECRET_ACCESS_KEY", ""),
).strip()
S3_SESSION_TOKEN = os.getenv("VAULT_S3_SESSION_TOKEN", os.getenv("AWS_SESSION_TOKEN", "")).strip()

R2_BUCKET = os.getenv("VAULT_R2_BUCKET", "").strip()
R2_ACCOUNT_ID = os.getenv("VAULT_R2_ACCOUNT_ID", "").strip()
R2_ACCESS_KEY_ID = os.getenv("VAULT_R2_ACCESS_KEY_ID", "").strip()
R2_SECRET_ACCESS_KEY = os.getenv("VAULT_R2_SECRET_ACCESS_KEY", "").strip()
R2_ENDPOINT_URL = (
    os.getenv("VAULT_R2_ENDPOINT_URL", "").strip()
    or (f"https://{R2_ACCOUNT_ID}.r2.cloudflarestorage.com" if R2_ACCOUNT_ID else None)
)
RESET_DB_ON_START = os.getenv("VAULT_RESET_DB_ON_START", "0").strip().lower() in {
    "1",
    "true",
    "yes",
    "on",
}
