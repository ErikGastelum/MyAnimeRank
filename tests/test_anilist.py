import asyncio
import json

import httpx
import pytest

import anilist

_RealAsyncClient = httpx.AsyncClient


@pytest.fixture
def transport(monkeypatch: pytest.MonkeyPatch):
    """Route anilist's httpx.AsyncClient through a MockTransport. Tests queue
    responses in `responses`; every request lands in `requests`. Sleeps are
    recorded instead of awaited."""
    state = {"responses": [], "requests": [], "client_kwargs": [], "sleeps": []}

    def handler(request: httpx.Request) -> httpx.Response:
        state["requests"].append(request)
        return state["responses"].pop(0)

    def client_factory(**kwargs):
        state["client_kwargs"].append(kwargs)
        return _RealAsyncClient(transport=httpx.MockTransport(handler), **kwargs)

    async def fake_sleep(seconds: float) -> None:
        state["sleeps"].append(seconds)

    monkeypatch.setattr(anilist.httpx, "AsyncClient", client_factory)
    monkeypatch.setattr(anilist, "_sleep", fake_sleep)
    return state


def ok(data: dict) -> httpx.Response:
    return httpx.Response(200, json={"data": data})


def gql_error(status: int, message: str, data=None) -> httpx.Response:
    return httpx.Response(status, json={"errors": [{"message": message, "status": status}], "data": data})


def too_many(retry_after: str | None) -> httpx.Response:
    headers = {"Retry-After": retry_after} if retry_after is not None else {}
    return httpx.Response(429, headers=headers, json={"errors": [{"message": "Too Many Requests.", "status": 429}]})


# ---------------------------------------------------------------- request shape


def test_graphql_posts_query_variables_and_token(transport) -> None:
    transport["responses"] = [ok({"Media": {"id": 1}})]

    result = asyncio.run(anilist._graphql("query", {"id": 1}, "token", 20))

    assert result == {"Media": {"id": 1}}
    (request,) = transport["requests"]
    assert str(request.url) == anilist.ANILIST_URL
    assert json.loads(request.content) == {"query": "query", "variables": {"id": 1}}
    assert request.headers["Authorization"] == "Bearer token"
    assert transport["client_kwargs"] == [{"timeout": 20}]


# ---------------------------------------------------------------- GraphQL errors


@pytest.mark.parametrize("status", [200, 400])
def test_graphql_errors_payload_raises_anilist_error(transport, status: int) -> None:
    transport["responses"] = [gql_error(status, 'Cannot query field "nope"')]

    with pytest.raises(anilist.AniListError, match='Cannot query field "nope"') as exc_info:
        asyncio.run(anilist._graphql("query"))

    assert not isinstance(exc_info.value, anilist.AniListNotFound)


def test_graphql_404_raises_not_found(transport) -> None:
    # Real AniList shape for a missing Media/MediaList: HTTP 404, every root field nulled.
    transport["responses"] = [gql_error(404, "Not Found.", data={"Media": None})]

    with pytest.raises(anilist.AniListNotFound):
        asyncio.run(anilist._graphql("query"))


def test_get_by_id_returns_none_when_anilist_404s(transport) -> None:
    transport["responses"] = [gql_error(404, "Not Found.", data={"Media": None})]

    assert asyncio.run(anilist.get_by_id(999999999)) is None


@pytest.mark.parametrize("status", [500, 502, 503])
def test_server_errors_stay_transient_http_errors(transport, status: int) -> None:
    # 5xx must remain httpx.HTTPError so push_now() queues it for retry (FR-26).
    transport["responses"] = [gql_error(status, "Internal Server Error")]

    with pytest.raises(httpx.HTTPStatusError):
        asyncio.run(anilist._graphql("query"))


# ---------------------------------------------------------------- 429 / Retry-After


def test_429_honors_retry_after_then_succeeds(transport) -> None:
    transport["responses"] = [too_many("3"), ok({"Viewer": {"id": 1}})]

    result = asyncio.run(anilist._graphql("query"))

    assert result == {"Viewer": {"id": 1}}
    assert transport["sleeps"] == [3]
    assert len(transport["requests"]) == 2


def test_429_retries_are_bounded(transport) -> None:
    transport["responses"] = [too_many("2"), too_many("2"), too_many("2"), ok({})]

    with pytest.raises(httpx.HTTPStatusError) as exc_info:
        asyncio.run(anilist._graphql("query"))

    assert exc_info.value.response.status_code == 429
    assert transport["sleeps"] == [2, 2]
    assert len(transport["requests"]) == anilist._MAX_429_RETRIES + 1


def test_429_with_retry_after_beyond_cap_fails_fast(transport) -> None:
    transport["responses"] = [too_many("120")]

    with pytest.raises(httpx.HTTPStatusError):
        asyncio.run(anilist._graphql("query"))

    assert transport["sleeps"] == []
    assert len(transport["requests"]) == 1


@pytest.mark.parametrize("header", [None, "soon", "-5"])
def test_429_without_usable_retry_after_waits_default(transport, header) -> None:
    transport["responses"] = [too_many(header), ok({})]

    asyncio.run(anilist._graphql("query"))

    assert transport["sleeps"] == [anilist._DEFAULT_RETRY_AFTER_SECONDS]


# ---------------------------------------------------------------- single list entry


def test_get_list_entry_returns_entry_and_score_format_in_one_request(transport) -> None:
    transport["responses"] = [ok({
        "Viewer": {"mediaListOptions": {"scoreFormat": "POINT_10_DECIMAL"}},
        "MediaList": {"id": 77, "status": "REPEATING", "progress": 4, "repeat": 1, "score": 8.5, "updatedAt": 1767225600},
    })]

    result = asyncio.run(anilist.get_list_entry("token", 1, 42, "manga"))

    assert len(transport["requests"]) == 1
    assert json.loads(transport["requests"][0].content)["variables"] == {"userId": 1, "mediaId": 42}
    assert result["score_format"] == "POINT_10_DECIMAL"
    assert result["entry"] == {
        "list_entry_id": 77, "status": "rereading", "progress": 4, "repeat_count": 1,
        "score": 8.5, "updated_at": "2026-01-01T00:00:00+00:00",
    }


def test_by_anilist_route_404s_for_unknown_id_through_real_graphql(transport, authed_client) -> None:
    # End to end: AniList's real 404 shape must surface as a 404, not a 500/502.
    transport["responses"] = [gql_error(404, "Not Found.", data={"Media": None})]

    resp = authed_client.get("/media/by-anilist/999999999")

    assert resp.status_code == 404


def test_import_404s_for_unknown_id_through_real_graphql(transport, authed_client) -> None:
    transport["responses"] = [gql_error(404, "Not Found.", data={"Media": None})]

    resp = authed_client.post("/media", json={"anilist_id": 999999999, "media_type": "anime", "title": "Ghost"})

    assert resp.status_code == 404


def test_routes_turn_anilist_errors_into_clear_502s(transport, authed_client) -> None:
    transport["responses"] = [gql_error(400, "Invalid token")]

    resp = authed_client.get("/characters/5")

    assert resp.status_code == 502
    assert "Invalid token" in resp.json()["detail"]


def test_get_list_entry_not_on_list_returns_none(transport) -> None:
    transport["responses"] = [gql_error(404, "Not Found.", data={"Viewer": None, "MediaList": None})]

    assert asyncio.run(anilist.get_list_entry("token", 1, 42, "anime")) == {"entry": None, "score_format": None}
