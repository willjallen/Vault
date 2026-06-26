"""FastAPI entrypoint for the vault service."""

import logging
import secrets
from collections.abc import Awaitable, Callable
from pathlib import Path

from fastapi import FastAPI, Request, Response
from fastapi.responses import PlainTextResponse
from fastapi.staticfiles import StaticFiles
from starlette.middleware.gzip import GZipMiddleware

from . import config
from .assets import validate_static_assets
from .db import init_db
from .routers import (
    recover_interrupted_transfers,
    router,
    start_ttl_sweeper,
    stop_ttl_sweeper,
    sweep_expired_documents,
    sweep_expired_transfers,
)
from .storage import ensure_storage
from .version import APP_VERSION

logger = logging.getLogger(__name__)


def _set_default_header(response: Response, name: str, value: str) -> None:
    if name not in response.headers:
        response.headers[name] = value


def _request_is_public_https(request: Request) -> bool:
    return request.url.scheme == "https" or config.public_url_is_https()


def _hsts_header_value() -> str:
    value = f"max-age={config.HSTS_MAX_AGE_SECONDS}"
    if config.HSTS_INCLUDE_SUBDOMAINS:
        value += "; includeSubDomains"
    if config.HSTS_PRELOAD:
        value += "; preload"
    return value


def _content_security_policy(nonce: str) -> str:
    if config.CONTENT_SECURITY_POLICY:
        return config.CONTENT_SECURITY_POLICY.replace("{nonce}", nonce)
    return (
        "default-src 'self'; "
        "base-uri 'self'; "
        "object-src 'none'; "
        "frame-ancestors 'none'; "
        "form-action 'self'; "
        "img-src 'self' data: blob:; "
        "style-src 'self' 'unsafe-inline'; "
        f"script-src 'self' 'nonce-{nonce}'; "
        "connect-src 'self'; "
        "font-src 'self' data:"
    )


def apply_security_headers(request: Request, response: Response) -> None:
    if not config.SECURITY_HEADERS_ENABLED:
        return
    _set_default_header(response, "X-Content-Type-Options", "nosniff")
    _set_default_header(response, "X-Frame-Options", "DENY")
    _set_default_header(response, "Referrer-Policy", "no-referrer")
    _set_default_header(
        response,
        "Permissions-Policy",
        "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
    )
    nonce = str(getattr(request.state, "csp_nonce", ""))
    _set_default_header(response, "Content-Security-Policy", _content_security_policy(nonce))
    if (
        not config.DEV_MODE
        and config.HSTS_MAX_AGE_SECONDS > 0
        and _request_is_public_https(request)
    ):
        _set_default_header(response, "Strict-Transport-Security", _hsts_header_value())


def create_app(*, enable_ttl_sweeper: bool = True) -> FastAPI:
    application = FastAPI(title=config.SITE_NAME, version=APP_VERSION)
    if config.GZIP_MINIMUM_SIZE > 0:
        application.add_middleware(
            GZipMiddleware,
            compresslevel=config.GZIP_COMPRESSLEVEL,
            minimum_size=config.GZIP_MINIMUM_SIZE,
        )
    application.mount(
        "/static",
        StaticFiles(directory=Path(__file__).parent / "static"),
        name="static",
    )

    @application.middleware("http")
    async def security_headers_middleware(
        request: Request,
        call_next: Callable[[Request], Awaitable[Response]],
    ) -> Response:
        request.state.csp_nonce = secrets.token_urlsafe(16)
        response = await call_next(request)
        apply_security_headers(request, response)
        return response

    @application.on_event("startup")
    async def startup_event() -> None:
        config.validate_runtime_config()
        validate_static_assets()
        if config.DEV_MODE:
            logger.warning("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!")
            logger.warning("VAULT IS RUNNING IN DEVELOPMENT MODE. DEBUG TOOLS ARE ENABLED.")
            logger.warning("DO NOT USE THIS CONTAINER WITH REAL OR PRODUCTION DATA.")
            logger.warning("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!")
        init_db()
        ensure_storage()
        sweep_expired_documents()
        sweep_expired_transfers()
        recover_interrupted_transfers()
        if enable_ttl_sweeper:
            start_ttl_sweeper()

    @application.on_event("shutdown")
    async def shutdown_event() -> None:
        await stop_ttl_sweeper()

    @application.get("/health", response_class=PlainTextResponse)
    def health() -> str:
        return "ok"

    application.include_router(router)
    return application


app = create_app()
