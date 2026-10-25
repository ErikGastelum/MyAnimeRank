import asyncio

import anilist


def test_graphql_posts_query_variables_and_token(monkeypatch) -> None:
    captured = {}

    class Response:
        def raise_for_status(self) -> None:
            captured["status_checked"] = True

        def json(self) -> dict:
            return {"data": {"Media": {"id": 1}}}

    class Client:
        def __init__(self, timeout: float) -> None:
            captured["timeout"] = timeout

        async def __aenter__(self):
            return self

        async def __aexit__(self, *_args) -> None:
            pass

        async def post(self, url: str, **kwargs):
            captured["url"] = url
            captured.update(kwargs)
            return Response()

    monkeypatch.setattr(anilist.httpx, "AsyncClient", Client)

    result = asyncio.run(anilist._graphql("query", {"id": 1}, "token", 20))

    assert result == {"Media": {"id": 1}}
    assert captured == {
        "timeout": 20,
        "url": anilist.ANILIST_URL,
        "json": {"query": "query", "variables": {"id": 1}},
        "headers": {"Authorization": "Bearer token"},
        "status_checked": True,
    }
