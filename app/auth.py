# Copyright (c) 2024 The Allen Family
"""Authentication and canonical Vault identity helpers."""

import base64
import datetime as dt
import hashlib
import hmac
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import TypedDict

from fastapi import Depends, HTTPException, Request
from fastapi.responses import RedirectResponse
from joserfc import jwk, jwt
from joserfc.errors import JoseError
from joserfc.jwt import JWTClaimsRegistry
from sqlalchemy import select
from sqlalchemy.orm import Session

from .config import (
    ADMIN_GROUPS,
    AUTH_MODE,
    BOOTSTRAP_ADMIN_EMAILS,
    DEV_AUTH_ISSUER,
    HEADER_AUTH_ISSUER,
    OIDC_CLIENT_AUTH,
    OIDC_CLIENT_ID,
    OIDC_CLIENT_SECRET,
    OIDC_DISCOVERY_TTL_SECONDS,
    OIDC_EMAIL_CLAIM,
    OIDC_GROUPS_CLAIM,
    OIDC_HTTP_TIMEOUT_SECONDS,
    OIDC_ISSUER,
    OIDC_NAME_CLAIM,
    OIDC_REDIRECT_URI,
    OIDC_SCOPES,
    OIDC_STATE_COOKIE_NAME,
    OIDC_USERNAME_CLAIM,
    PUBLIC_URL,
    SESSION_COOKIE_NAME,
    SESSION_MAX_AGE_SECONDS,
    SESSION_SECRET,
    new_token_urlsafe,
)
from .db import get_db
from .models import VaultGroup, VaultGroupMembership, VaultUser

LOCAL_DEV_DOMAINS = {"localhost", "127.0.0.1", "::1", "family.localhost"}


class UserContext(TypedDict):
    """Canonical Vault user attributes."""

    id: str
    vault_user_id: int
    issuer: str
    subject: str
    name: str
    email: str
    groups: list[str]
    is_admin: bool


_DISCOVERY_CACHE: dict[str, object] = {"expires_at": 0.0, "config": None}


def _env_flag(name: str) -> bool:
    value = (os.getenv(name) or "").strip().lower()
    return value in {"1", "true", "yes", "on"}


def _split_groups(value: str | None) -> set[str]:
    return {group.strip().lower() for group in (value or "").split(",") if group.strip()}


def _clean_header(value: str | None) -> str:
    return (value or "").strip()


def _dev_auth_allowed_for_domain() -> bool:
    base_domain = _clean_header(os.getenv("BASE_DOMAIN", "family.localhost")).lower()
    return base_domain in LOCAL_DEV_DOMAINS or base_domain.endswith(".localhost")


def _now_utc() -> dt.datetime:
    return dt.datetime.now(tz=dt.UTC)


def _b64encode(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def _b64decode(value: str) -> bytes:
    padding = "=" * (-len(value) % 4)
    return base64.urlsafe_b64decode((value + padding).encode("ascii"))


def _sign_payload(payload: dict[str, object]) -> str:
    body = _b64encode(json.dumps(payload, separators=(",", ":")).encode("utf-8"))
    signature = hmac.new(SESSION_SECRET.encode("utf-8"), body.encode("ascii"), hashlib.sha256)
    return f"{body}.{_b64encode(signature.digest())}"


def _verify_payload(value: str | None) -> dict[str, object] | None:
    if not value or "." not in value:
        return None
    body, signature = value.rsplit(".", 1)
    expected = hmac.new(SESSION_SECRET.encode("utf-8"), body.encode("ascii"), hashlib.sha256)
    if not hmac.compare_digest(_b64encode(expected.digest()), signature):
        return None
    try:
        payload = json.loads(_b64decode(body))
    except (ValueError, json.JSONDecodeError):
        return None
    expires_at = payload.get("exp")
    if isinstance(expires_at, (int, float)) and expires_at < time.time():
        return None
    return payload if isinstance(payload, dict) else None


def _cookie_secure(request: Request) -> bool:
    return request.url.scheme == "https"


def _safe_redirect(value: str | None) -> str:
    if value and value.startswith("/") and not value.startswith("//"):
        return value
    return "/"


def _request_path_with_query(request: Request) -> str:
    query = request.url.query
    return f"{request.url.path}?{query}" if query else request.url.path


def _admin_hint(email: str | None, groups: set[str]) -> bool:
    normalized_email = (email or "").strip().lower()
    return normalized_email in BOOTSTRAP_ADMIN_EMAILS or bool(groups & ADMIN_GROUPS)


def _vault_group_names(user_id: int, db: Session) -> list[str]:
    return list(
        db.execute(
            select(VaultGroup.name)
            .join(VaultGroupMembership)
            .where(VaultGroupMembership.user_id == user_id)
            .order_by(VaultGroup.name),
        )
        .scalars()
        .all()
    )


def _context_for_user(user: VaultUser, db: Session) -> UserContext:
    return {
        "id": str(user.id),
        "vault_user_id": user.id,
        "issuer": user.issuer,
        "subject": user.subject,
        "name": user.name,
        "email": user.email or "",
        "groups": _vault_group_names(user.id, db),
        "is_admin": bool(user.is_admin),
    }


def _upsert_vault_user(
    db: Session,
    issuer: str,
    subject: str,
    email: str | None,
    name: str | None,
    admin_hint: bool = False,
    mark_login: bool = False,
) -> VaultUser:
    if not issuer or not subject:
        raise HTTPException(status_code=401, detail="Identity provider did not supply a subject")

    user = (
        db.execute(
            select(VaultUser).where(
                VaultUser.issuer == issuer,
                VaultUser.subject == subject,
            ),
        )
        .scalars()
        .first()
    )
    display_name = (name or email or subject).strip() or subject
    now = _now_utc()
    if not user:
        user = VaultUser(
            issuer=issuer,
            subject=subject,
            email=email,
            name=display_name,
            is_admin=admin_hint,
            is_active=True,
            last_login_at=now if mark_login else None,
            last_seen_at=now,
        )
        db.add(user)
    else:
        user.email = email
        user.name = display_name
        user.last_seen_at = now
        if mark_login:
            user.last_login_at = now
        if admin_hint:
            user.is_admin = True
    db.commit()
    db.refresh(user)
    if not user.is_active:
        raise HTTPException(status_code=403, detail="User is disabled")
    return user


def _dev_identity(db: Session) -> UserContext | None:
    if not _env_flag("VAULT_DEV_AUTH"):
        return None
    if not _dev_auth_allowed_for_domain():
        return None

    subject = (os.getenv("VAULT_DEV_USER", "local-admin") or "local-admin").strip()
    groups = _split_groups(os.getenv("VAULT_DEV_GROUPS", "admin,vault-admin"))
    email = (
        os.getenv("VAULT_DEV_EMAIL")
        or os.getenv("VAULT_DEFAULT_USER_EMAIL", "admin@example.com")
        or "admin@example.com"
    )
    user = _upsert_vault_user(
        db,
        DEV_AUTH_ISSUER,
        subject,
        email,
        (os.getenv("VAULT_DEV_NAME", "Local Admin") or subject).strip(),
        admin_hint=_admin_hint(email, groups),
    )
    return _context_for_user(user, db)


def _header_identity(request: Request, db: Session) -> UserContext:
    remote_user = _clean_header(request.headers.get("Remote-User"))
    if not remote_user:
        local_user = _dev_identity(db)
        if local_user:
            return local_user
        raise HTTPException(status_code=401, detail="Authentication required")

    groups = _split_groups(request.headers.get("Remote-Groups"))
    email = (
        _clean_header(request.headers.get("Remote-Email"))
        or os.getenv("VAULT_DEFAULT_USER_EMAIL", "admin@example.com")
        or "admin@example.com"
    )
    remote_name = _clean_header(request.headers.get("Remote-Name")) or remote_user
    user = _upsert_vault_user(
        db,
        HEADER_AUTH_ISSUER,
        remote_user,
        email,
        remote_name,
        admin_hint=_admin_hint(email, groups),
    )
    return _context_for_user(user, db)


def _session_identity(request: Request, db: Session) -> UserContext | None:
    payload = _verify_payload(request.cookies.get(SESSION_COOKIE_NAME))
    user_id = payload.get("uid") if payload else None
    if not isinstance(user_id, int):
        return None
    user = db.execute(select(VaultUser).where(VaultUser.id == user_id)).scalars().first()
    if not user or not user.is_active:
        return None
    user.last_seen_at = _now_utc()
    db.commit()
    return _context_for_user(user, db)


def _auth_required(request: Request) -> None:
    if AUTH_MODE == "oidc" and request.method == "GET" and request.url.path == "/":
        rd = urllib.parse.quote(_request_path_with_query(request))
        raise HTTPException(
            status_code=303,
            detail="Login required",
            headers={"Location": f"/login?rd={rd}"},
        )
    raise HTTPException(status_code=401, detail="Authentication required")


def current_user(request: Request, db: Session = Depends(get_db)) -> UserContext:
    """Resolve or JIT-create the canonical Vault user for this request."""
    if AUTH_MODE == "dev":
        user = _dev_identity(db)
        if user:
            return user
        raise HTTPException(status_code=401, detail="Development auth is disabled")
    if AUTH_MODE == "headers":
        return _header_identity(request, db)
    if AUTH_MODE == "oidc":
        user = _session_identity(request, db)
        if user:
            return user
        _auth_required(request)
    raise HTTPException(status_code=500, detail=f"Unsupported auth mode: {AUTH_MODE}")


def require_admin(user: UserContext = Depends(current_user)) -> UserContext:
    if not user["is_admin"]:
        raise HTTPException(status_code=403, detail="Admin access required")
    return user


def _oidc_redirect_uri(request: Request) -> str:
    if OIDC_REDIRECT_URI:
        return OIDC_REDIRECT_URI
    base = PUBLIC_URL or str(request.base_url).rstrip("/")
    return f"{base}/auth/callback"


def _oidc_discovery() -> dict[str, object]:
    if not OIDC_ISSUER or not OIDC_CLIENT_ID:
        raise HTTPException(status_code=500, detail="OIDC is not configured")
    cached_config = _DISCOVERY_CACHE.get("config")
    if cached_config and float(_DISCOVERY_CACHE.get("expires_at", 0)) > time.time():
        return cached_config  # type: ignore[return-value]
    config = _http_json(f"{OIDC_ISSUER}/.well-known/openid-configuration")
    _DISCOVERY_CACHE["config"] = config
    _DISCOVERY_CACHE["expires_at"] = time.time() + OIDC_DISCOVERY_TTL_SECONDS
    return config


def _http_json(url: str, data: dict[str, str] | None = None, headers: dict[str, str] | None = None) -> dict[str, object]:
    body = urllib.parse.urlencode(data).encode("utf-8") if data is not None else None
    request = urllib.request.Request(url, data=body, headers=headers or {})
    try:
        with urllib.request.urlopen(request, timeout=OIDC_HTTP_TIMEOUT_SECONDS) as response:
            parsed = json.loads(response.read().decode("utf-8"))
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as exc:
        raise HTTPException(status_code=502, detail="OIDC provider request failed") from exc
    if not isinstance(parsed, dict):
        raise HTTPException(status_code=502, detail="OIDC provider returned invalid JSON")
    return parsed


def oidc_login_response(request: Request) -> RedirectResponse:
    if AUTH_MODE != "oidc":
        return RedirectResponse(url="/", status_code=303)
    discovery = _oidc_discovery()
    authorization_endpoint = str(discovery.get("authorization_endpoint") or "")
    if not authorization_endpoint:
        raise HTTPException(status_code=500, detail="OIDC authorization endpoint is missing")

    state = new_token_urlsafe()
    nonce = new_token_urlsafe()
    rd = _safe_redirect(request.query_params.get("rd"))
    state_cookie = _sign_payload({"state": state, "nonce": nonce, "rd": rd, "exp": time.time() + 600})
    params = {
        "client_id": OIDC_CLIENT_ID,
        "redirect_uri": _oidc_redirect_uri(request),
        "response_type": "code",
        "scope": OIDC_SCOPES,
        "state": state,
        "nonce": nonce,
    }
    response = RedirectResponse(
        url=f"{authorization_endpoint}?{urllib.parse.urlencode(params)}",
        status_code=303,
    )
    response.set_cookie(
        OIDC_STATE_COOKIE_NAME,
        state_cookie,
        httponly=True,
        max_age=600,
        samesite="lax",
        secure=_cookie_secure(request),
    )
    return response


def _exchange_code_for_token(request: Request, code: str, discovery: dict[str, object]) -> dict[str, object]:
    token_endpoint = str(discovery.get("token_endpoint") or "")
    if not token_endpoint:
        raise HTTPException(status_code=500, detail="OIDC token endpoint is missing")
    form = {
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": _oidc_redirect_uri(request),
        "client_id": OIDC_CLIENT_ID,
    }
    headers = {"Accept": "application/json", "Content-Type": "application/x-www-form-urlencoded"}
    if OIDC_CLIENT_AUTH == "client_secret_post":
        form["client_secret"] = OIDC_CLIENT_SECRET
    elif OIDC_CLIENT_SECRET:
        credentials = f"{OIDC_CLIENT_ID}:{OIDC_CLIENT_SECRET}".encode("utf-8")
        headers["Authorization"] = f"Basic {base64.b64encode(credentials).decode('ascii')}"
    return _http_json(token_endpoint, form, headers)


def _verified_id_claims(id_token: str, nonce: str, discovery: dict[str, object]) -> dict[str, object]:
    jwks_uri = str(discovery.get("jwks_uri") or "")
    if not jwks_uri:
        raise HTTPException(status_code=500, detail="OIDC JWKS endpoint is missing")
    try:
        token = jwt.decode(
            id_token,
            jwk.KeySet.import_key_set(_http_json(jwks_uri)),
            algorithms=["RS256", "RS384", "RS512", "ES256", "ES384", "ES512"],
        )
        claims = dict(token.claims)
        JWTClaimsRegistry(
            iss={"essential": True, "value": OIDC_ISSUER},
            aud={"essential": True, "value": OIDC_CLIENT_ID},
            exp={"essential": True},
            nonce={"essential": True, "value": nonce},
        ).validate(claims)
    except JoseError as exc:
        raise HTTPException(status_code=401, detail="OIDC ID token validation failed") from exc
    return claims


def _userinfo(access_token: str, discovery: dict[str, object]) -> dict[str, object]:
    endpoint = str(discovery.get("userinfo_endpoint") or "")
    if not endpoint or not access_token:
        return {}
    return _http_json(endpoint, headers={"Authorization": f"Bearer {access_token}"})


def oidc_callback_response(request: Request, db: Session) -> RedirectResponse:
    if AUTH_MODE != "oidc":
        return RedirectResponse(url="/", status_code=303)
    error = request.query_params.get("error")
    if error:
        raise HTTPException(status_code=401, detail=f"OIDC login failed: {error}")
    code = request.query_params.get("code")
    state = request.query_params.get("state")
    state_payload = _verify_payload(request.cookies.get(OIDC_STATE_COOKIE_NAME))
    if not code or not state or not state_payload or state_payload.get("state") != state:
        raise HTTPException(status_code=401, detail="OIDC state validation failed")
    nonce = str(state_payload.get("nonce") or "")
    discovery = _oidc_discovery()
    token = _exchange_code_for_token(request, code, discovery)
    id_token = str(token.get("id_token") or "")
    if not id_token:
        raise HTTPException(status_code=401, detail="OIDC provider did not return an ID token")
    claims = _verified_id_claims(id_token, nonce, discovery)
    userinfo = _userinfo(str(token.get("access_token") or ""), discovery)
    if userinfo and userinfo.get("sub") != claims.get("sub"):
        raise HTTPException(status_code=401, detail="OIDC userinfo subject mismatch")
    identity = {**claims, **userinfo}
    raw_groups = identity.get(OIDC_GROUPS_CLAIM, [])
    if isinstance(raw_groups, str):
        groups = _split_groups(raw_groups)
    elif isinstance(raw_groups, (list, tuple, set)):
        groups = {str(group).lower() for group in raw_groups if str(group).strip()}
    else:
        groups = set()
    email = str(identity.get(OIDC_EMAIL_CLAIM) or "") or None
    subject = str(identity.get("sub") or "")
    name = str(
        identity.get(OIDC_NAME_CLAIM)
        or identity.get(OIDC_USERNAME_CLAIM)
        or identity.get(OIDC_EMAIL_CLAIM)
        or subject,
    )
    user = _upsert_vault_user(
        db,
        OIDC_ISSUER,
        subject,
        email,
        name,
        admin_hint=_admin_hint(email, groups),
        mark_login=True,
    )
    response = RedirectResponse(url=_safe_redirect(str(state_payload.get("rd") or "/")), status_code=303)
    response.set_cookie(
        SESSION_COOKIE_NAME,
        _sign_payload({"uid": user.id, "exp": time.time() + SESSION_MAX_AGE_SECONDS}),
        httponly=True,
        max_age=SESSION_MAX_AGE_SECONDS,
        samesite="lax",
        secure=_cookie_secure(request),
    )
    response.delete_cookie(OIDC_STATE_COOKIE_NAME)
    return response


def logout_response(request: Request) -> RedirectResponse:
    rd = _safe_redirect(request.query_params.get("rd"))
    response = RedirectResponse(url=rd, status_code=303)
    response.delete_cookie(SESSION_COOKIE_NAME)
    response.delete_cookie(OIDC_STATE_COOKIE_NAME)
    return response
