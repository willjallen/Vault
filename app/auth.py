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


def current_user(request: Request) -> UserContext:
    """Resolve the current user from Authelia headers or reject the request."""
    remote_user = request.headers.get("Remote-User")
    if not remote_user:
        raise HTTPException(status_code=401, detail="Authentication required")

    groups_header = request.headers.get("Remote-Groups") or ""
    scopes = {g.strip().lower() for g in groups_header.split(",") if g.strip()}
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
