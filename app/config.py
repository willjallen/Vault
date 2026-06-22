# Copyright (c) 2024 The Allen Family
"""Configuration helpers for the vault service."""

import os
from pathlib import Path

BASE_DOMAIN = os.getenv("BASE_DOMAIN", "family.localhost")
DB_PATH = Path(os.getenv("VAULT_DB_PATH", "/vault-metadata/vault-metadata.db")).resolve()

STORAGE_BACKEND = os.getenv("VAULT_STORAGE_BACKEND", "local").strip().lower()
STORAGE_PREFIX = os.getenv("VAULT_STORAGE_PREFIX", "objects").strip().strip("/")
OBJECTS_PATH = Path(
    os.getenv(
        "VAULT_OBJECTS_PATH",
        os.getenv("VAULT_LOCAL_OBJECTS_PATH", os.getenv("VAULT_FILES_PATH", "/vault-objects")),
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
