import pytest
from fastapi import BackgroundTasks, HTTPException
from pydantic import ValidationError

import database
import main
from models import RatingIn


def _scores(
    *,
    story: float,
    characters: float,
    animation: float,
    audio: float,
    enjoyment: float,
) -> dict[str, float]:
    return {
        "story": story,
        "characters": characters,
        "animation": animation,
        "audio": audio,
        "enjoyment": enjoyment,
    }


def _add_media(anilist_id: int, media_type: str, title: str) -> int:
    return database.insert_or_update_media(
        {
            "anilist_id": anilist_id,
            "media_type": media_type,
            "title": title,
            "genres": ["Action"],
        }
    )


def test_anime_overall_averages_all_five_criteria() -> None:
    scores = _scores(
        story=8.0,
        characters=7.0,
        animation=9.0,
        audio=6.0,
        enjoyment=10.0,
    )

    assert database.compute_overall("anime", scores) == 8.0


def test_manga_overall_averages_four_criteria_and_excludes_audio() -> None:
    scores = _scores(
        story=8.0,
        characters=7.0,
        animation=9.0,
        audio=1.0,
        enjoyment=10.0,
    )

    assert database.compute_overall("manga", scores) == 8.5


def test_manga_rating_forces_stored_audio_to_zero() -> None:
    media_id = _add_media(1, "manga", "Manga subject")
    scores = _scores(
        story=8.0,
        characters=7.0,
        animation=9.0,
        audio=9.5,
        enjoyment=10.0,
    )

    result = database.upsert_rating(media_id, scores)
    stored = database.get_media_by_id(media_id)

    assert result == {"overall": 8.5, "media_type": "manga"}
    assert stored is not None
    assert stored["rating"]["audio"] == 0.0


def test_overall_rounds_to_one_decimal() -> None:
    scores = _scores(
        story=8.0,
        characters=8.0,
        animation=8.0,
        audio=10.0,
        enjoyment=8.5,
    )

    assert database.compute_overall("manga", scores) == 8.1


@pytest.mark.parametrize(
    ("subject_type", "different_type"),
    [("anime", "manga"), ("manga", "anime")],
)
def test_anchors_never_cross_media_types(
    subject_type: str,
    different_type: str,
) -> None:
    subject_id = _add_media(1, subject_type, "Subject")
    same_type_id = _add_media(2, subject_type, "Same type")
    different_type_id = _add_media(3, different_type, "Different type")
    scores = _scores(
        story=8.0,
        characters=8.0,
        animation=8.0,
        audio=8.0,
        enjoyment=8.0,
    )
    database.upsert_rating(same_type_id, scores)
    database.upsert_rating(different_type_id, scores)

    anchors = database.get_anchors(subject_id, "story", "all")

    assert anchors is not None
    assert [anchor["id"] for anchor in anchors] == [same_type_id]
    assert different_type_id not in {anchor["id"] for anchor in anchors}


def test_duplicate_import_does_not_change_media_type() -> None:
    media_id = _add_media(100, "anime", "Original title")

    updated_id = database.insert_or_update_media(
        {
            "anilist_id": 100,
            "media_type": "manga",
            "title": "Updated title",
            "genres": ["Drama"],
        }
    )
    stored = database.get_media_by_id(media_id)

    assert updated_id == media_id
    assert stored is not None
    assert stored["title"] == "Updated title"
    assert stored["media_type"] == "anime"


@pytest.mark.parametrize(
    "invalid_score",
    [0.5, 10.5],
    ids=["below-minimum", "above-maximum"],
)
def test_rating_in_rejects_scores_outside_allowed_range(invalid_score: float) -> None:
    scores = _scores(
        story=invalid_score,
        characters=8.0,
        animation=8.0,
        audio=8.0,
        enjoyment=8.0,
    )

    with pytest.raises(ValidationError):
        RatingIn(**scores)


@pytest.mark.parametrize(
    "invalid_score",
    [1.1, 7.3, 9.9],
    ids=["near-minimum", "middle", "near-maximum"],
)
def test_rating_in_rejects_scores_outside_half_point_steps(
    invalid_score: float,
) -> None:
    scores = _scores(
        story=8.0,
        characters=invalid_score,
        animation=8.0,
        audio=8.0,
        enjoyment=8.0,
    )

    with pytest.raises(ValidationError):
        RatingIn(**scores)


def _put_route_rating(media_type: str, audio: float) -> dict:
    media_id = _add_media(200, media_type, "Route subject")
    body = RatingIn(
        story=8.0,
        characters=8.0,
        animation=8.0,
        audio=audio,
        enjoyment=8.0,
    )
    return main.put_rating(media_id, body, BackgroundTasks())


def test_rating_route_accepts_zero_audio_for_manga() -> None:
    result = _put_route_rating("manga", 0.0)

    assert result == {"overall": 8.0, "media_type": "manga"}


@pytest.mark.parametrize("audio", [0.5, 8.0])
def test_rating_route_rejects_nonzero_audio_for_manga(audio: float) -> None:
    with pytest.raises(HTTPException) as exc_info:
        _put_route_rating("manga", audio)

    assert exc_info.value.status_code == 422


@pytest.mark.parametrize("audio", [1.0, 7.5, 10.0])
def test_rating_route_accepts_half_step_audio_for_anime(audio: float) -> None:
    result = _put_route_rating("anime", audio)

    assert result["media_type"] == "anime"


@pytest.mark.parametrize("audio", [0.0, 0.5, 7.3, 10.5])
def test_rating_route_rejects_invalid_audio_for_anime(audio: float) -> None:
    with pytest.raises(HTTPException) as exc_info:
        _put_route_rating("anime", audio)

    assert exc_info.value.status_code == 422
