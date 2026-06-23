"""FastAPI entrypoint for the vault service."""

from pathlib import Path

from fastapi import FastAPI
from fastapi.responses import PlainTextResponse
from fastapi.staticfiles import StaticFiles

from .config import SITE_NAME
from .db import init_db
from .routers import router, start_ttl_sweeper, stop_ttl_sweeper, sweep_expired_documents
from .storage import ensure_storage
from .version import APP_VERSION

app = FastAPI(title=SITE_NAME, version=APP_VERSION)
app.mount("/static", StaticFiles(directory=Path(__file__).parent / "static"), name="static")


@app.on_event("startup")
async def startup_event() -> None:
    init_db()
    ensure_storage()
    sweep_expired_documents()
    start_ttl_sweeper()


@app.on_event("shutdown")
async def shutdown_event() -> None:
    await stop_ttl_sweeper()


@app.get("/health", response_class=PlainTextResponse)
def health() -> str:
    return "ok"


app.include_router(router)
