"""Configuration helpers for the vault service."""

import os
import secrets
import urllib.parse
from pathlib import Path


def _env_flag(name: str, default: str = "0") -> bool:
    return os.getenv(name, default).strip().lower() in {"1", "true", "yes", "on"}


def _development_session_secret() -> str:
    return "dev-" + "insecure-" + "session-" + "secret"


LOCAL_DEV_HOSTS = {"localhost", "127.0.0.1", "::1"}
VALID_AUTH_MODES = {"headers", "oidc", "dev"}
VALID_COOKIE_SECURE_MODES = {"auto", "1", "true", "yes", "on", "0", "false", "no", "off"}
VALID_OIDC_CLIENT_AUTH_MODES = {"client_secret_basic", "client_secret_post", "none"}

BASE_DOMAIN = os.getenv("BASE_DOMAIN", "localhost")
SITE_NAME = os.getenv("VAULT_SITE_NAME", "Vault").strip() or "Vault"
DATA_DIR = Path(os.getenv("VAULT_DATA_DIR", "/data")).resolve()
DB_PATH = Path(os.getenv("VAULT_DB_PATH", str(DATA_DIR / "vault.db"))).resolve()
TRANSFERS_PATH = Path(os.getenv("VAULT_TRANSFERS_PATH", str(DATA_DIR / "transfers"))).resolve()
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

AUTH_MODE = (
    os.getenv(
        "VAULT_AUTH_MODE",
        ("dev" if _env_flag("VAULT_DEV_AUTH") else "headers"),
    )
    .strip()
    .lower()
)
DEV_MODE = _env_flag("VAULT_DEV_MODE") or AUTH_MODE == "dev" or _env_flag("VAULT_DEV_AUTH")
SESSION_COOKIE_NAME = os.getenv("VAULT_SESSION_COOKIE_NAME", "vault_session").strip()
SESSION_COOKIE_SECURE = os.getenv("VAULT_SESSION_COOKIE_SECURE", "auto").strip().lower() or "auto"
_SESSION_SECRET_ENV = os.getenv("VAULT_SESSION_SECRET", "").strip()
_OIDC_CLIENT_SECRET_ENV = os.getenv("VAULT_OIDC_CLIENT_SECRET", "").strip()
if REQUIRE_SESSION_SECRET and not _SESSION_SECRET_ENV:
    raise RuntimeError("VAULT_SESSION_SECRET is required when VAULT_REQUIRE_SESSION_SECRET=1")
SESSION_SECRET = _SESSION_SECRET_ENV or _OIDC_CLIENT_SECRET_ENV or _development_session_secret()
SESSION_MAX_AGE_SECONDS = int(os.getenv("VAULT_SESSION_MAX_AGE_SECONDS", "604800"))
TTL_SWEEP_INTERVAL_SECONDS = max(10, int(os.getenv("VAULT_TTL_SWEEP_INTERVAL_SECONDS", "60")))
MAX_UPLOAD_BYTES = max(1, int(os.getenv("VAULT_MAX_UPLOAD_BYTES", str(5 * 1024 * 1024 * 1024))))
TRANSFER_CHUNK_BYTES = max(
    1,
    int(os.getenv("VAULT_TRANSFER_CHUNK_BYTES", str(32 * 1024 * 1024))),
)
TRANSFER_SESSION_TTL_SECONDS = max(
    60,
    int(os.getenv("VAULT_TRANSFER_SESSION_TTL_SECONDS", "86400")),
)
EXPORT_TTL_SECONDS = max(60, int(os.getenv("VAULT_EXPORT_TTL_SECONDS", "86400")))
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
OIDC_ALLOW_INSECURE_HTTP = _env_flag("VAULT_OIDC_ALLOW_INSECURE_HTTP")
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
R2_ENDPOINT_URL = os.getenv("VAULT_R2_ENDPOINT_URL", "").strip() or (
    f"https://{R2_ACCOUNT_ID}.r2.cloudflarestorage.com" if R2_ACCOUNT_ID else None
)

SECURITY_HEADERS_ENABLED = _env_flag("VAULT_SECURITY_HEADERS_ENABLED", "1")
CONTENT_SECURITY_POLICY = os.getenv("VAULT_CONTENT_SECURITY_POLICY", "").strip()
HSTS_MAX_AGE_SECONDS = max(0, int(os.getenv("VAULT_HSTS_MAX_AGE_SECONDS", "31536000")))
HSTS_INCLUDE_SUBDOMAINS = _env_flag("VAULT_HSTS_INCLUDE_SUBDOMAINS")
HSTS_PRELOAD = _env_flag("VAULT_HSTS_PRELOAD")
GZIP_MINIMUM_SIZE = max(0, int(os.getenv("VAULT_GZIP_MINIMUM_SIZE", "1024")))
GZIP_COMPRESSLEVEL = min(9, max(1, int(os.getenv("VAULT_GZIP_COMPRESSLEVEL", "6"))))


def is_local_hostname(hostname: str | None) -> bool:
    normalized = (hostname or "").strip().lower()
    return normalized in LOCAL_DEV_HOSTS or normalized.endswith(".localhost")


def parsed_absolute_url(value: str) -> urllib.parse.ParseResult:
    return urllib.parse.urlparse(value.strip())


def public_url_is_https() -> bool:
    return PUBLIC_URL.lower().startswith("https://")


def oidc_url_uses_secure_transport(url: str) -> bool:
    parsed_url = parsed_absolute_url(url)
    if parsed_url.scheme == "https":
        return True
    return parsed_url.scheme == "http" and (
        OIDC_ALLOW_INSECURE_HTTP or is_local_hostname(parsed_url.hostname)
    )


def validate_runtime_config() -> None:
    """Fail fast on production settings that would weaken auth or TLS behavior."""
    errors: list[str] = []
    if AUTH_MODE not in VALID_AUTH_MODES:
        errors.append(f"VAULT_AUTH_MODE must be one of {', '.join(sorted(VALID_AUTH_MODES))}")
    if SESSION_COOKIE_SECURE not in VALID_COOKIE_SECURE_MODES:
        errors.append("VAULT_SESSION_COOKIE_SECURE must be auto, true, or false")
    if OIDC_CLIENT_AUTH not in VALID_OIDC_CLIENT_AUTH_MODES:
        errors.append(
            "VAULT_OIDC_CLIENT_AUTH must be client_secret_basic, client_secret_post, or none",
        )
    if not DEV_MODE and SESSION_SECRET == _development_session_secret():
        errors.append("VAULT_SESSION_SECRET is required outside development mode")
    if PUBLIC_URL:
        public_url = parsed_absolute_url(PUBLIC_URL)
        if public_url.scheme not in {"http", "https"} or not public_url.netloc:
            errors.append("VAULT_PUBLIC_URL must be an absolute http(s) URL")
        elif (
            not DEV_MODE
            and public_url.scheme != "https"
            and not is_local_hostname(public_url.hostname)
        ):
            errors.append("VAULT_PUBLIC_URL must use https outside local development")
    if AUTH_MODE == "oidc":
        if not OIDC_ISSUER:
            errors.append("VAULT_OIDC_ISSUER is required when VAULT_AUTH_MODE=oidc")
        elif not oidc_url_uses_secure_transport(OIDC_ISSUER):
            errors.append("VAULT_OIDC_ISSUER must use https outside local development")
        if not OIDC_CLIENT_ID:
            errors.append("VAULT_OIDC_CLIENT_ID is required when VAULT_AUTH_MODE=oidc")
        if (
            OIDC_CLIENT_AUTH in {"client_secret_basic", "client_secret_post"}
            and not OIDC_CLIENT_SECRET
        ):
            errors.append(
                "VAULT_OIDC_CLIENT_SECRET is required for confidential OIDC client auth",
            )
        redirect_origin = OIDC_REDIRECT_URI or PUBLIC_URL
        if redirect_origin:
            redirect_url = parsed_absolute_url(redirect_origin)
            if redirect_url.scheme not in {"http", "https"} or not redirect_url.netloc:
                errors.append("OIDC redirect/public URL must be an absolute http(s) URL")
            elif (
                not DEV_MODE
                and redirect_url.scheme != "https"
                and not is_local_hostname(redirect_url.hostname)
            ):
                errors.append("OIDC redirect/public URL must use https outside local development")
    if errors:
        raise RuntimeError("Invalid Vault runtime configuration: " + "; ".join(errors))
