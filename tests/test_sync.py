import asyncio
from datetime import datetime, timedelta, timezone

import pytest

import anilist
import database
import sync

T0 = datetime(2026, 1, 1, tzinfo=timezone.utc)


def ts(minutes: int) -> str:
    return (T0 + timedelta(minutes=minutes)).isoformat()


def future(hours: int = 1) -> str:
    return (datetime.now(timezone.utc) + timedelta(hours=hours)).isoformat()


@pytest.fixture(autouse=True)
def connected() -> None:
    database.save_auth("test-token", None, 1, "tester", "POINT_10_DECIMAL")
    sync._push_queue.clear()


def entry(anilist_id: int, media_type: str = "anime", **kw) -> dict:
    """One flattened entry, shaped like anilist.get_media_list_collection()'s output."""
    base = {
        "list_entry_id": 1000 + anilist_id,
        "status": "watching" if media_type == "anime" else "reading",
        "progress": 3,
        "repeat_count": 0,
        "score": 0,
        "updated_at": ts(0),
        "anilist_id": anilist_id,
        "media_type": media_type,
        "title": f"Title {anilist_id}",
        "title_native": None,
        "title_english": None,
        "cover_url": None,
        "genres": ["Action"],
        "tags": [],
        "producers": [],
        "year": 2020,
        "season": None,
        "format": "TV" if media_type == "anime" else "MANGA",
        "episodes": 12 if media_type == "anime" else None,
        "chapters": None if media_type == "anime" else 100,
        "volumes": None,
        "runtime_minutes": None,
        "synopsis": None,
        "avg_score": None,
    }
    base.update(kw)
    return base


def fake_collection(monkeypatch: pytest.MonkeyPatch, entries: list[dict]) -> None:
    async def get_media_list_collection(token: str, user_id: int, media_type: str) -> list[dict]:
        return [e for e in entries if e["media_type"] == media_type]

    monkeypatch.setattr(anilist, "get_media_list_collection", get_media_list_collection)


def local_row(anilist_id: int, media_type: str = "anime", **kw) -> int:
    data = {"anilist_id": anilist_id, "media_type": media_type, "title": f"Title {anilist_id}",
            "genres": ["Action"], "episodes": 12, "status": "watching", "progress": 3}
    data.update(kw)
    return database.insert_or_update_media(data)


# ---------------------------------------------------------------- detect_conflict


@pytest.mark.parametrize(
    ("local", "remote", "synced", "expected"),
    [
        (ts(5), ts(5), None, "conflict"),      # never synced: no baseline, always ask
        (None, None, None, "conflict"),
        (ts(10), ts(10), ts(5), "conflict"),   # both changed since sync
        (ts(1), ts(10), ts(5), "remote"),      # only remote changed
        (None, ts(10), ts(5), "remote"),
        (ts(10), ts(1), ts(5), "local"),       # only local changed
        (ts(10), None, ts(5), "local"),
        (ts(1), ts(1), ts(5), "none"),         # neither changed
        (ts(5), ts(5), ts(5), "none"),         # equal to sync time is not "changed"
        (None, None, ts(5), "none"),
    ],
)
def test_detect_conflict(local, remote, synced, expected) -> None:
    assert database.detect_conflict(local, remote, synced) == expected


# ---------------------------------------------------------------- pull


def test_pull_imports_new_entries_without_prompting(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_collection(monkeypatch, [entry(1, "anime", progress=5), entry(2, "manga", progress=40)])

    result = asyncio.run(sync.pull())

    assert result == {"imported": 2, "updated": 0, "conflicts": []}
    anime = database.get_media_by_anilist_id(1)
    manga = database.get_media_by_anilist_id(2)
    assert (anime["media_type"], anime["progress"], anime["anilist_list_entry_id"]) == ("anime", 5, 1001)
    assert (manga["media_type"], manga["progress"], manga["anilist_list_entry_id"]) == ("manga", 40, 1002)
    assert anime["last_synced_at"] is not None and manga["last_synced_at"] is not None


def test_pull_applies_one_sided_remote_change(monkeypatch: pytest.MonkeyPatch) -> None:
    media_id = local_row(1, updated_at=ts(0), last_synced_at=ts(5), anilist_list_entry_id=1001)
    fake_collection(monkeypatch, [entry(1, status="completed", progress=12, updated_at=ts(10))])

    result = asyncio.run(sync.pull())

    assert result["updated"] == 1 and result["conflicts"] == []
    row = database.get_media_by_id(media_id)
    assert (row["status"], row["progress"], row["pending_conflict"]) == ("completed", 12, 0)


def test_pull_flags_conflict_and_keeps_local_values(monkeypatch: pytest.MonkeyPatch) -> None:
    media_id = local_row(1, status="paused", progress=7, updated_at=ts(10), last_synced_at=ts(5),
                         anilist_list_entry_id=1001)
    fake_collection(monkeypatch, [entry(1, status="completed", progress=12, updated_at=ts(10))])

    result = asyncio.run(sync.pull())

    assert [c["media_id"] for c in result["conflicts"]] == [media_id]
    row = database.get_media_by_id(media_id)
    assert (row["status"], row["progress"], row["pending_conflict"]) == ("paused", 7, 1)


def test_pull_leaves_local_only_rows_alone(monkeypatch: pytest.MonkeyPatch) -> None:
    # A local row with no AniList counterpart is not a conflict -- it just
    # hasn't been pushed yet.
    media_id = local_row(1, updated_at=ts(10), last_synced_at=None)
    fake_collection(monkeypatch, [])

    result = asyncio.run(sync.pull())

    assert result == {"imported": 0, "updated": 0, "conflicts": []}
    assert database.get_media_by_id(media_id)["pending_conflict"] == 0


def test_pull_after_import_applies_later_remote_change_without_conflict(monkeypatch: pytest.MonkeyPatch) -> None:
    # Import, make no local edits, then change the entry on AniList: the next
    # Pull should apply it as a one-sided remote change.
    fake_collection(monkeypatch, [entry(1, progress=3)])
    asyncio.run(sync.pull())

    fake_collection(monkeypatch, [entry(1, progress=8, updated_at=future())])
    result = asyncio.run(sync.pull())

    assert result["conflicts"] == []
    assert result["updated"] == 1
    assert database.get_media_by_anilist_id(1)["progress"] == 8


@pytest.mark.parametrize(("media_type", "expected_audio"), [("anime", 8.5), ("manga", 0.0)])
def test_pull_import_seeds_rating_from_anilist_score(
    monkeypatch: pytest.MonkeyPatch, media_type: str, expected_audio: float
) -> None:
    fake_collection(monkeypatch, [entry(1, media_type, score=8.5)])

    asyncio.run(sync.pull())

    rating = database.get_media_by_anilist_id(1)["rating"]
    assert rating["overall"] == 8.5
    assert (rating["story"], rating["characters"], rating["animation"], rating["enjoyment"]) == (8.5,) * 4
    assert rating["audio"] == expected_audio


def test_pull_import_without_score_creates_no_rating(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_collection(monkeypatch, [entry(1, score=0)])

    asyncio.run(sync.pull())

    assert database.get_media_by_anilist_id(1)["rating"] is None


def test_pull_import_copies_anilist_values_exactly(monkeypatch: pytest.MonkeyPatch) -> None:
    # A rewatch sitting at the final episode on AniList must import as-is --
    # the app's own auto-complete rule (FR-53) is for local edits, not for
    # rewriting AniList's data on the way in.
    fake_collection(monkeypatch, [entry(1, status="rewatching", progress=12, repeat_count=2, episodes=12)])

    asyncio.run(sync.pull())

    row = database.get_media_by_anilist_id(1)
    assert (row["status"], row["progress"], row["repeat_count"]) == ("rewatching", 12, 2)
    assert row["updated_at"] == row["last_synced_at"]


# ---------------------------------------------------------------- push_now_guarded


@pytest.fixture
def remote(monkeypatch: pytest.MonkeyPatch) -> dict:
    """Fake AniList for the push path. Set remote["entry"] (or None = not on
    the list) and remote["score_format"]; every call is counted, and the full
    collection fetch is wired to fail the test if push ever uses it."""
    state = {"entry": None, "score_format": "POINT_10_DECIMAL", "pushed": [],
             "calls": {"get_list_entry": 0, "get_viewer": 0, "push_list_entry": 0}}

    async def get_list_entry(token, user_id, anilist_id, media_type) -> dict:
        state["calls"]["get_list_entry"] += 1
        if state["entry"] is None:   # AniList 404s the whole query, Viewer included
            return {"entry": None, "score_format": None}
        return {"entry": state["entry"], "score_format": state["score_format"]}

    async def get_viewer(token: str) -> dict:
        state["calls"]["get_viewer"] += 1
        return {"id": 1, "name": "tester", "score_format": state["score_format"]}

    async def push_list_entry(token, anilist_id, status, progress, repeat_count, overall) -> dict:
        state["calls"]["push_list_entry"] += 1
        state["pushed"].append({"anilist_id": anilist_id, "status": status, "progress": progress,
                                "repeat_count": repeat_count, "overall": overall})
        return {"id": 5555}

    async def no_collection(*args):
        raise AssertionError("push path must not fetch the whole list collection")

    monkeypatch.setattr(anilist, "get_list_entry", get_list_entry)
    monkeypatch.setattr(anilist, "get_viewer", get_viewer)
    monkeypatch.setattr(anilist, "push_list_entry", push_list_entry)
    monkeypatch.setattr(anilist, "get_media_list_collection", no_collection)
    return state


def remote_entry(**kw) -> dict:
    base = {"list_entry_id": 1001, "status": "watching", "progress": 3, "repeat_count": 0,
            "score": 0, "updated_at": ts(1)}
    base.update(kw)
    return base


def test_guarded_push_pushes_a_clean_local_change_in_two_requests(remote) -> None:
    media_id = local_row(1, status="completed", progress=12, updated_at=ts(10), last_synced_at=ts(5),
                         anilist_list_entry_id=1001)
    remote["entry"] = remote_entry(updated_at=ts(1))

    result = asyncio.run(sync.push_now_guarded(media_id))

    assert result["synced_at"] is not None
    assert remote["pushed"] == [{"anilist_id": 1, "status": "completed", "progress": 12, "repeat_count": 0, "overall": None}]
    assert database.get_media_by_id(media_id)["last_synced_at"] == result["synced_at"]
    # One combined entry+scoreFormat lookup, one mutation; no separate Viewer call.
    assert remote["calls"] == {"get_list_entry": 1, "get_viewer": 0, "push_list_entry": 1}


def test_guarded_push_creates_entry_when_no_remote_counterpart(remote) -> None:
    media_id = local_row(1, updated_at=ts(10), last_synced_at=None)
    remote["entry"] = None

    asyncio.run(sync.push_now_guarded(media_id))

    row = database.get_media_by_id(media_id)
    assert (row["anilist_list_entry_id"], row["pending_conflict"]) == (5555, 0)
    # AniList nulls Viewer on the 404, so scoreFormat needs its own call here.
    assert remote["calls"] == {"get_list_entry": 1, "get_viewer": 1, "push_list_entry": 1}


def test_guarded_push_refuses_on_conflict(remote) -> None:
    media_id = local_row(1, updated_at=ts(10), last_synced_at=ts(5), anilist_list_entry_id=1001)
    remote["entry"] = remote_entry(progress=9, updated_at=ts(10))
    sync._push_queue.add(media_id)

    result = asyncio.run(sync.push_now_guarded(media_id))

    assert result["conflict"] is True
    assert remote["pushed"] == []
    assert database.get_media_by_id(media_id)["pending_conflict"] == 1
    assert media_id not in sync._push_queue


def test_guarded_push_rechecks_score_format_on_every_push(remote) -> None:
    # FR-28: the format comes back fresh with the entry lookup, so a drift made
    # on the AniList website is caught before any score is written.
    media_id = local_row(1, updated_at=ts(10), last_synced_at=ts(5), anilist_list_entry_id=1001)
    remote["entry"] = remote_entry(updated_at=ts(1))
    remote["score_format"] = "POINT_100"

    result = asyncio.run(sync.push_now_guarded(media_id))

    assert result == {"synced_at": None, "score_format_drift": True}
    assert remote["pushed"] == []
    assert database.get_auth()["score_format"] == "POINT_100"


def test_guarded_push_queues_for_retry_when_offline(monkeypatch: pytest.MonkeyPatch, remote) -> None:
    # FR-26: a network failure on the pre-push lookup must still queue the
    # edit, or it would never reach AniList.
    async def offline(*args):
        raise anilist.httpx.ConnectError("offline")

    monkeypatch.setattr(anilist, "get_list_entry", offline)
    media_id = local_row(1, updated_at=ts(10), last_synced_at=ts(5), anilist_list_entry_id=1001)

    with pytest.raises(anilist.httpx.ConnectError):
        asyncio.run(sync.push_now_guarded(media_id))

    assert media_id in sync._push_queue


def test_retry_loop_logs_failures_instead_of_swallowing_them(
    monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    sleeps = 0

    async def one_tick(seconds: float) -> None:
        nonlocal sleeps
        sleeps += 1
        if sleeps > 1:
            raise asyncio.CancelledError  # stop the infinite loop after one pass

    async def boom(media_id: int) -> dict:
        raise RuntimeError("boom")

    monkeypatch.setattr(sync.asyncio, "sleep", one_tick)
    monkeypatch.setattr(sync, "push_now_guarded", boom)
    sync._push_queue.add(42)

    with caplog.at_level("WARNING", logger="sync"), pytest.raises(asyncio.CancelledError):
        asyncio.run(sync.retry_loop())

    assert "media_id=42" in caplog.text and "boom" in caplog.text
    assert 42 in sync._push_queue   # still queued for the next pass


def test_push_of_unrated_title_never_sends_a_score(monkeypatch: pytest.MonkeyPatch) -> None:
    # Runs the real push_now -> push_list_entry path; only the HTTP layer is
    # faked. With no local rating, the mutation must omit `score` entirely so
    # an existing AniList score is left untouched (never sent as null or 0).
    sent: list[dict] = []

    async def fake_graphql(query, variables=None, token=None, timeout=15):
        if query is anilist._VIEWER_QUERY:
            return {"Viewer": {"id": 1, "name": "tester", "mediaListOptions": {"scoreFormat": "POINT_10_DECIMAL"}}}
        sent.append(variables)
        return {"SaveMediaListEntry": {"id": 1001, "status": "CURRENT", "progress": 3, "updatedAt": 0}}

    monkeypatch.setattr(anilist, "_graphql", fake_graphql)
    media_id = local_row(1, anilist_list_entry_id=1001)

    asyncio.run(sync.push_now(media_id))

    assert len(sent) == 1
    assert "score" not in sent[0]
    assert sent[0]["mediaId"] == 1


# ---------------------------------------------------------------- apply_remote_entry

SCORES = {"story": 6.0, "characters": 7.0, "animation": 8.0, "audio": 9.0, "enjoyment": 10.0}


def test_apply_remote_entry_overwrites_overall_but_not_criteria() -> None:
    media_id = local_row(1)
    database.upsert_rating(media_id, SCORES)

    database.apply_remote_entry(media_id, "completed", 12, 1, 9.5, ts(20))

    row = database.get_media_by_id(media_id)
    assert (row["status"], row["progress"], row["repeat_count"]) == ("completed", 12, 1)
    assert row["updated_at"] == row["last_synced_at"] == ts(20)
    rating = row["rating"]
    assert rating["overall"] == 9.5
    assert {k: rating[k] for k in SCORES} == SCORES


@pytest.mark.parametrize(("media_type", "expected_audio"), [("anime", 7.5), ("manga", 0.0)])
def test_apply_remote_entry_seeds_rating_from_score(media_type: str, expected_audio: float) -> None:
    media_id = local_row(1, media_type=media_type)

    database.apply_remote_entry(media_id, "completed", 12, 0, 7.5, ts(20))

    rating = database.get_media_by_id(media_id)["rating"]
    assert (rating["story"], rating["characters"], rating["animation"], rating["enjoyment"]) == (7.5, 7.5, 7.5, 7.5)
    assert rating["audio"] == expected_audio
    assert rating["overall"] == 7.5


@pytest.mark.parametrize(
    ("score", "snapped"),
    [(7.3, 7.5), (7.2, 7.0), (7.25, 7.5), (7.75, 8.0), (9.9, 10.0), (0.2, 1.0)],
)
@pytest.mark.parametrize("media_type", ["anime", "manga"])
def test_apply_remote_entry_snaps_seeded_criteria_to_half_steps(media_type: str, score: float, snapped: float) -> None:
    # Criteria must stay on the 1.0-10.0 / 0.5-step scale; overall keeps
    # AniList's exact score so the next push doesn't rewrite it.
    media_id = local_row(1, media_type=media_type)

    database.apply_remote_entry(media_id, "completed", 12, 0, score, ts(20))

    rating = database.get_media_by_id(media_id)["rating"]
    assert (rating["story"], rating["characters"], rating["animation"], rating["enjoyment"]) == (snapped,) * 4
    assert rating["audio"] == (snapped if media_type == "anime" else 0.0)
    assert rating["overall"] == score


@pytest.mark.parametrize("score", [0, None])
def test_apply_remote_entry_ignores_empty_score(score) -> None:
    unrated = local_row(1)
    rated = local_row(2)
    database.upsert_rating(rated, SCORES)
    before = database.get_media_by_id(rated)["rating"]["overall"]

    database.apply_remote_entry(unrated, "watching", 4, 0, score, ts(20))
    database.apply_remote_entry(rated, "watching", 4, 0, score, ts(20))

    assert database.get_media_by_id(unrated)["rating"] is None
    assert database.get_media_by_id(rated)["rating"]["overall"] == before
