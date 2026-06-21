# Copyright (c) 2024 The Allen Family
"""Authentication helpers for the vault service."""

import os
from typing import TypedDict

from fastapi import HTTPException, Request


class UserContext(TypedDict):
    """User attributes supplied by Authelia."""

    id: str
    name: str
    email: str
    groups: list[str]
    is_admin: bool


def _env_flag(name: str) -> bool:
    value = (os.getenv(name) or "").strip().lower()
    return value in {"1", "true", "yes", "on"}


def _split_groups(value: str | None) -> set[str]:
    return {group.strip().lower() for group in (value or "").split(",") if group.strip()}


def _local_dev_user() -> UserContext | None:
    if not _env_flag("VAULT_DEV_AUTH"):
        return None

    remote_user = os.getenv("VAULT_DEV_USER", "local-admin").strip() or "local-admin"
    scopes = _split_groups(os.getenv("VAULT_DEV_GROUPS", "admin,vault-admin"))
    email = (
        os.getenv("VAULT_DEV_EMAIL")
        or os.getenv("VAULT_DEFAULT_USER_EMAIL", "admin@example.com")
        or "admin@example.com"
    )

    return {
        "id": remote_user,
        "name": os.getenv("VAULT_DEV_NAME", "Local Admin").strip() or remote_user,
        "email": email,
        "groups": sorted(scopes),
        "is_admin": "admin" in scopes or "vault-admin" in scopes,
    }


def current_user(request: Request) -> UserContext:
    """Resolve the current user from Authelia headers or reject the request."""
    remote_user = request.headers.get("Remote-User")
    if not remote_user:
        local_user = _local_dev_user()
        if local_user:
            return local_user
        raise HTTPException(status_code=401, detail="Authentication required")

    scopes = _split_groups(request.headers.get("Remote-Groups"))
    is_admin = "admin" in scopes or "vault-admin" in scopes
    email = (
        request.headers.get("Remote-Email")
        or os.getenv("VAULT_DEFAULT_USER_EMAIL", "admin@example.com")
        or "admin@example.com"
    )

    return {
        "id": remote_user,
        "name": request.headers.get("Remote-Name") or remote_user,
        "email": email,
        "groups": sorted(scopes),
        "is_admin": is_admin,
    }
