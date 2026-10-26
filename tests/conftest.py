from collections.abc import Iterator
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

import database
import main


@pytest.fixture(autouse=True)
def isolated_database(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Every test gets its own SQLite database; library.db is never opened."""
    db_path = tmp_path / "library.db"
    monkeypatch.setattr(database, "DB_PATH", db_path)
    database.init_db()
    return db_path


@pytest.fixture
def authed_client() -> Iterator[TestClient]:
    """TestClient with a stored (fake) AniList session, so require_auth passes."""
    database.save_auth(
        access_token="test-token",
        token_expires_at=None,
        anilist_user_id=1,
        anilist_username="tester",
        score_format="POINT_10_DECIMAL",
    )
    with TestClient(main.app) as client:
        yield client
