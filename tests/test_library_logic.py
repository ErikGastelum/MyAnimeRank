import pytest

import database


def add(anilist_id: int, media_type: str = "anime", **kw) -> int:
    data = {"anilist_id": anilist_id, "media_type": media_type, "title": f"Title {anilist_id}", "genres": ["Action"]}
    data.update(kw)
    return database.insert_or_update_media(data)


# ---------------------------------------------------------------- update_progress (FR-53 / FR-57)


@pytest.mark.parametrize(
    ("media_type", "total_field", "status"),
    [("anime", "episodes", "watching"), ("manga", "chapters", "reading")],
)
def test_reaching_total_auto_completes(media_type: str, total_field: str, status: str) -> None:
    media_id = add(1, media_type, status=status, progress=11, **{total_field: 12})

    assert database.update_progress(media_id, 12) == 12

    row = database.get_media_by_id(media_id)
    assert (row["status"], row["repeat_count"]) == ("completed", 0)


@pytest.mark.parametrize(
    ("media_type", "total_field", "status"),
    [("anime", "episodes", "rewatching"), ("manga", "chapters", "rereading")],
)
def test_finishing_a_rewatch_counts_it_once(media_type: str, total_field: str, status: str) -> None:
    media_id = add(1, media_type, status=status, progress=11, repeat_count=2, **{total_field: 12})

    database.update_progress(media_id, 12)
    database.update_progress(media_id, 13)   # tapping +1 again at the cap

    row = database.get_media_by_id(media_id)
    assert (row["status"], row["progress"], row["repeat_count"]) == ("completed", 12, 3)


@pytest.mark.parametrize("status", ["paused", "dropped", "planning"])
def test_reaching_total_leaves_other_statuses_alone(status: str) -> None:
    media_id = add(1, status=status, progress=11, episodes=12)

    database.update_progress(media_id, 12)

    assert database.get_media_by_id(media_id)["status"] == status


def test_progress_is_clamped_to_zero_and_total() -> None:
    media_id = add(1, status="watching", progress=5, episodes=12)

    assert database.update_progress(media_id, 50) == 12
    assert database.update_progress(media_id, -3) == 0


def test_null_total_never_clamps_or_auto_completes() -> None:
    # FR-57: ongoing/hiatus titles have no total (AniList returns null).
    media_id = add(1, "manga", status="reading", progress=300, chapters=None)

    assert database.update_progress(media_id, 380) == 380
    assert database.get_media_by_id(media_id)["status"] == "reading"


# ---------------------------------------------------------------- genre-scoped anchors


def test_genre_scope_keeps_only_titles_sharing_a_genre() -> None:
    scores = {"story": 8.0, "characters": 8.0, "animation": 8.0, "audio": 8.0, "enjoyment": 8.0}
    subject = add(1, genres=["Action", "Drama"])
    shares = add(2, genres=["Drama"])
    disjoint = add(3, genres=["Romance"])
    other_type = add(4, "manga", genres=["Action"])
    for media_id in (shares, disjoint, other_type):
        database.upsert_rating(media_id, scores)

    genre_ids = [a["id"] for a in database.get_anchors(subject, "story", "genre")]
    all_ids = sorted(a["id"] for a in database.get_anchors(subject, "story", "all"))

    assert genre_ids == [shares]
    assert all_ids == sorted([shares, disjoint])


def test_anchors_reject_unknown_criterion() -> None:
    subject = add(1)
    with pytest.raises(ValueError):
        database.get_anchors(subject, "overall; DROP TABLE media", "all")
