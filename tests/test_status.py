import pytest

import anilist
import database
import sync

ANIME = ["watching", "rewatching", "completed", "paused", "dropped", "planning"]
MANGA = ["reading", "rereading", "completed", "paused", "dropped", "planning"]


@pytest.fixture
def pushes(monkeypatch: pytest.MonkeyPatch) -> list[int]:
    """Record background pushes instead of calling AniList."""
    calls: list[int] = []

    async def fake_push(media_id: int) -> dict:
        calls.append(media_id)
        return {}

    monkeypatch.setattr(sync, "push_now_guarded", fake_push)
    return calls


def add(media_type: str) -> int:
    return database.insert_or_update_media({"anilist_id": 1, "media_type": media_type, "title": "T"})


# ---------------------------------------------------------------- PUT /media/{id}/status (FR-17)


@pytest.mark.parametrize(
    ("media_type", "status"),
    [("anime", s) for s in ANIME] + [("manga", s) for s in MANGA],
)
def test_valid_status_for_media_type_is_accepted(authed_client, pushes, media_type: str, status: str) -> None:
    media_id = add(media_type)

    resp = authed_client.put(f"/media/{media_id}/status", json={"status": status})

    assert resp.status_code == 200
    assert database.get_media_by_id(media_id)["status"] == status
    assert pushes == [media_id]


@pytest.mark.parametrize(
    ("media_type", "status"),
    [
        ("anime", "reading"), ("anime", "rereading"),     # manga-only wording
        ("manga", "watching"), ("manga", "rewatching"),   # anime-only wording
        ("anime", "finished"), ("anime", ""), ("anime", "WATCHING"), ("manga", "CURRENT"),
    ],
)
def test_invalid_status_is_rejected_and_never_pushed(authed_client, pushes, media_type: str, status: str) -> None:
    media_id = add(media_type)

    resp = authed_client.put(f"/media/{media_id}/status", json={"status": status})

    assert resp.status_code == 422
    assert database.get_media_by_id(media_id)["status"] == "planning"
    assert pushes == []


def test_status_for_missing_media_is_404(authed_client, pushes) -> None:
    resp = authed_client.put("/media/999/status", json={"status": "watching"})

    assert resp.status_code == 404
    assert pushes == []


# ---------------------------------------------------------------- POST /media status


@pytest.fixture
def anilist_type(monkeypatch: pytest.MonkeyPatch):
    def set_type(media_type: str) -> None:
        async def get_by_id(anilist_id: int) -> dict:
            return {"anilist_id": anilist_id, "media_type": media_type, "title": "From AniList"}

        monkeypatch.setattr(anilist, "get_by_id", get_by_id)

    return set_type


@pytest.mark.parametrize(("media_type", "status"), [("manga", "reading"), ("anime", "rewatching")])
def test_import_accepts_valid_status(authed_client, anilist_type, media_type: str, status: str) -> None:
    anilist_type(media_type)

    resp = authed_client.post("/media", json={"anilist_id": 5, "media_type": media_type, "title": "T", "status": status})

    assert resp.status_code == 200
    assert database.get_media_by_id(resp.json()["id"])["status"] == status


@pytest.mark.parametrize(("media_type", "status"), [("manga", "watching"), ("anime", "reading"), ("anime", "done")])
def test_import_rejects_status_invalid_for_anilist_media_type(
    authed_client, anilist_type, media_type: str, status: str
) -> None:
    # Checked against AniList's media_type, not whatever the client claimed.
    anilist_type(media_type)

    resp = authed_client.post("/media", json={"anilist_id": 5, "media_type": "anime", "title": "T", "status": status})

    assert resp.status_code == 422
    assert database.get_media_by_anilist_id(5) is None


def test_local_statuses_match_frontend_and_prd() -> None:
    assert sorted(anilist.local_statuses("anime")) == sorted(ANIME)
    assert sorted(anilist.local_statuses("manga")) == sorted(MANGA)
