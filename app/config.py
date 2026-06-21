# Copyright (c) 2024 The Allen Family
"""Configuration helpers for the vault service."""

import os
from pathlib import Path

BASE_DOMAIN = os.getenv("BASE_DOMAIN", "family.localhost")
FILES_PATH = Path(os.getenv("VAULT_FILES_PATH", "/vault-files")).resolve()
DB_PATH = Path(os.getenv("VAULT_DB_PATH", "/vault-metadata/vault-metadata.db")).resolve()
