import asyncio
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

import database
import main
import sync


def test_startup_initializes_db_and_shutdown_stops_retry_loop(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fresh = tmp_path / "fresh.db"
    monkeypatch.setattr(database, "DB_PATH", fresh)
    events: list[str] = []

    async def fake_retry_loop() -> None:
        events.append("started")
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            events.append("cancelled")
            raise

    monkeypatch.setattr(sync, "retry_loop", fake_retry_loop)

    with TestClient(main.app) as client:
        client.get("/auth/status")  # lets the event loop start the background task
        with database.get_conn() as conn:
            tables = {r["name"] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        assert {"media", "rating", "anilist_auth"} <= tables

    assert events == ["started", "cancelled"]


def test_app_uses_lifespan_not_deprecated_on_event() -> None:
    assert main.app.router.on_startup == []
