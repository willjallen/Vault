"""Signed active-transfer tokens.

Upload part requests can use these tokens to avoid a DB-backed auth dependency for every chunk.
The final completion request still uses normal authenticated metadata checks.
"""

from __future__ import annotations

import base64
import binascii
import datetime as dt
import hashlib
import hmac
import json
import math
import time

from app import config


class TransferTokenError(Exception):
    """A transfer token is missing, invalid, expired, or scoped to another session."""


def _b64encode(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def _b64decode(value: str) -> bytes:
    padding = "=" * (-len(value) % 4)
    return base64.urlsafe_b64decode((value + padding).encode("ascii"))


def _token_secret() -> bytes:
    return config.SESSION_SECRET.encode("utf-8")


def sign_upload_token(session_id: str, owner_id: str, expires_at: dt.datetime | None) -> str:
    expires_timestamp = (
        expires_at.timestamp()
        if expires_at is not None
        else time.time() + config.TRANSFER_SESSION_TTL_SECONDS
    )
    payload = {
        "exp": expires_timestamp,
        "owner": owner_id,
        "sid": session_id,
        "typ": "upload-part",
    }
    body = _b64encode(json.dumps(payload, separators=(",", ":")).encode("utf-8"))
    signature = hmac.new(_token_secret(), body.encode("ascii"), hashlib.sha256)
    return f"{body}.{_b64encode(signature.digest())}"


def verify_upload_token(token: str | None, session_id: str) -> str:
    if not token or "." not in token:
        raise TransferTokenError("Upload token is required")
    body, signature = token.rsplit(".", 1)
    try:
        body_bytes = body.encode("ascii")
        signature.encode("ascii")
    except UnicodeEncodeError as exc:
        raise TransferTokenError("Upload token is invalid") from exc
    expected = hmac.new(_token_secret(), body_bytes, hashlib.sha256)
    if not hmac.compare_digest(_b64encode(expected.digest()), signature):
        raise TransferTokenError("Upload token is invalid")
    try:
        payload = json.loads(_b64decode(body))
    except (binascii.Error, UnicodeDecodeError, ValueError, json.JSONDecodeError) as exc:
        raise TransferTokenError("Upload token is invalid") from exc
    if not isinstance(payload, dict):
        raise TransferTokenError("Upload token is invalid")
    if payload.get("typ") != "upload-part" or payload.get("sid") != session_id:
        raise TransferTokenError("Upload token is not valid for this session")
    expires_at = payload.get("exp")
    if (
        isinstance(expires_at, bool)
        or not isinstance(expires_at, int | float)
        or not math.isfinite(float(expires_at))
        or float(expires_at) < time.time()
    ):
        raise TransferTokenError("Upload token expired")
    owner_id = payload.get("owner")
    if not isinstance(owner_id, str) or not owner_id:
        raise TransferTokenError("Upload token is invalid")
    return owner_id
