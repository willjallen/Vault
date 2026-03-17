# Copyright (c) 2024 The Allen Family
"""FastAPI entrypoint for the vault service."""

from pathlib import Path

from fastapi import FastAPI
from fastapi.responses import PlainTextResponse
from fastapi.staticfiles import StaticFiles

from .db import init_db
from .routers import router
from .storage import ensure_storage

app = FastAPI(title="Family Vault", version="1.0.0")
app.mount("/static", StaticFiles(directory=Path(__file__).parent / "static"), name="static")


@app.on_event("startup")
def startup_event() -> None:
    init_db()
    ensure_storage()


@app.get("/health", response_class=PlainTextResponse)
def health() -> str:
    return "ok"


app.include_router(router)
