import pytest

import anilist
import database

# ---------------------------------------------------------------- _to_media_type


@pytest.mark.parametrize(("al_type", "expected"), [("ANIME", "anime"), ("MANGA", "manga")])
def test_to_media_type_maps_anilist_enum(al_type: str, expected: str) -> None:
    assert anilist._to_media_type(al_type) == expected


@pytest.mark.parametrize("al_type", ["NOVEL", "anime", "", None])
def test_to_media_type_rejects_anything_else(al_type) -> None:
    # Never default: an unknown type must fail loudly, not become "manga".
    with pytest.raises(ValueError):
        anilist._to_media_type(al_type)


# ---------------------------------------------------------------- POST /media


def _fake_get_by_id(media_type: str, calls: list[int]):
    async def get_by_id(anilist_id: int) -> dict:
        calls.append(anilist_id)
        return {"anilist_id": anilist_id, "media_type": media_type, "title": "From AniList"}

    return get_by_id


@pytest.mark.parametrize(
    ("client_type", "anilist_type"),
    [("anime", "manga"), ("manga", "anime"), ("manga", "manga")],
)
def test_import_stores_media_type_from_anilist_not_client(
    authed_client, monkeypatch: pytest.MonkeyPatch, client_type: str, anilist_type: str
) -> None:
    calls: list[int] = []
    monkeypatch.setattr(anilist, "get_by_id", _fake_get_by_id(anilist_type, calls))

    resp = authed_client.post(
        "/media", json={"anilist_id": 42, "media_type": client_type, "title": "Client title"}
    )

    assert resp.status_code == 200
    stored = database.get_media_by_id(resp.json()["id"])
    assert stored["media_type"] == anilist_type
    assert calls == [42]


def test_import_404s_when_anilist_has_no_such_id(authed_client, monkeypatch: pytest.MonkeyPatch) -> None:
    async def get_by_id(anilist_id: int) -> None:
        return None

    monkeypatch.setattr(anilist, "get_by_id", get_by_id)

    resp = authed_client.post("/media", json={"anilist_id": 999, "media_type": "anime", "title": "Ghost"})

    assert resp.status_code == 404
    assert database.get_media_by_anilist_id(999) is None


def test_import_502s_when_anilist_unreachable(authed_client, monkeypatch: pytest.MonkeyPatch) -> None:
    async def get_by_id(anilist_id: int) -> dict:
        raise anilist.httpx.ConnectError("AniList down")

    monkeypatch.setattr(anilist, "get_by_id", get_by_id)

    resp = authed_client.post("/media", json={"anilist_id": 7, "media_type": "anime", "title": "Offline"})

    assert resp.status_code == 502
    assert database.get_media_by_anilist_id(7) is None


# ---------------------------------------------------------------- anchors with NULL criterion


def test_anchors_skip_rows_with_null_criterion(authed_client) -> None:
    subject = database.insert_or_update_media({"anilist_id": 1, "media_type": "anime", "title": "Subject"})
    rated = database.insert_or_update_media({"anilist_id": 2, "media_type": "anime", "title": "Rated"})
    legacy = database.insert_or_update_media({"anilist_id": 3, "media_type": "anime", "title": "Legacy"})
    scores = {"story": 8.0, "characters": 8.0, "animation": 7.0, "audio": 8.0, "enjoyment": 8.0}
    database.upsert_rating(rated, scores)
    database.upsert_rating(legacy, scores)
    # Simulate a legacy row: the schema allows NULL rating.animation.
    with database.get_conn() as conn:
        conn.execute("UPDATE rating SET animation = NULL WHERE media_id = ?", (legacy,))

    resp = authed_client.get(f"/media/{subject}/anchors", params={"criterion": "animation", "scope": "all"})

    assert resp.status_code == 200
    assert [a["id"] for a in resp.json()["anchors"]] == [rated]
