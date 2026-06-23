"""FastAPI entrypoint for the vault service."""

from pathlib import Path

from fastapi import FastAPI
from fastapi.responses import PlainTextResponse
from fastapi.staticfiles import StaticFiles

from . import config
from .db import init_db
from .routers import router, start_ttl_sweeper, stop_ttl_sweeper, sweep_expired_documents
from .storage import ensure_storage
from .version import APP_VERSION


def create_app(*, enable_ttl_sweeper: bool = True) -> FastAPI:
    application = FastAPI(title=config.SITE_NAME, version=APP_VERSION)
    application.mount(
        "/static",
        StaticFiles(directory=Path(__file__).parent / "static"),
        name="static",
    )

    @application.on_event("startup")
    async def startup_event() -> None:
        init_db()
        ensure_storage()
        sweep_expired_documents()
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
