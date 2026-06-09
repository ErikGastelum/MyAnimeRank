import json
from datetime import datetime, timedelta, timezone

import httpx

from database import get_conn

_JIKAN_URL      = "https://api.jikan.moe/v4/anime"
_JIKAN_SEASONAL = "https://api.jikan.moe/v4/seasons/now"
_SEASONAL_KEY   = "__seasonal__"
_CACHE_TTL      = timedelta(hours=24)


def search(q: str) -> list[dict]:
    cached = _cache_get(q)
    if cached is not None:
        return cached
    results = _fetch(q)
    _cache_set(q, results)
    return results


def by_mal_id(mal_id: int) -> dict | None:
    key = f"__id__{mal_id}"
    cached = _cache_get(key)
    if cached is not None:
        return cached[0] if cached else None
    try:
        with httpx.Client(timeout=8.0) as client:
            resp = client.get(f"{_JIKAN_URL}/{mal_id}")
            resp.raise_for_status()
        item = _normalise(resp.json()["data"])
        _cache_set(key, [item])
        return item
    except (httpx.HTTPStatusError, httpx.TimeoutException, httpx.RequestError, KeyError):
        _cache_set(key, [])
        return None


def seasonal() -> list[dict]:
    cached = _cache_get(_SEASONAL_KEY)
    if cached is not None:
        return cached
    results = _fetch_seasonal()
    _cache_set(_SEASONAL_KEY, results)
    return results


def _fetch_seasonal() -> list[dict]:
    try:
        with httpx.Client(timeout=10.0) as client:
            resp = client.get(_JIKAN_SEASONAL, params={"limit": 25})
            resp.raise_for_status()
        return [_normalise(item) for item in resp.json().get("data", [])]
    except (httpx.HTTPStatusError, httpx.TimeoutException, httpx.RequestError):
        return []


def _fetch(q: str) -> list[dict]:
    try:
        with httpx.Client(timeout=8.0) as client:
            resp = client.get(_JIKAN_URL, params={"q": q, "limit": 20, "sfw": "true"})
            resp.raise_for_status()
        return [_normalise(item) for item in resp.json().get("data", [])]
    except (httpx.HTTPStatusError, httpx.TimeoutException, httpx.RequestError):
        return []


def _normalise(item: dict) -> dict:
    season_raw = item.get("season")
    season_year = item.get("year")
    season = f"{season_raw.title()} {season_year}" if season_raw and season_year else None

    return {
        "api_id":    str(item["mal_id"]),
        "title":     item.get("title_english") or item.get("title") or "",
        "title_ro":  item.get("title") or "",
        "cover_url": (item.get("images") or {}).get("jpg", {}).get("large_image_url"),
        "genres":    [g["name"] for g in (item.get("genres") or [])],
        "year":      item.get("year"),
        "season":    season,
        "episodes":  item.get("episodes"),
        "format":    item.get("type"),
        "synopsis":  item.get("synopsis") or "",
        "mal_score": item.get("score"),
    }


def _cache_get(q: str) -> list[dict] | None:
    with get_conn() as conn:
        row = conn.execute(
            "SELECT results, cached_at FROM search_cache WHERE query=?", (q,)
        ).fetchone()
    if not row:
        return None
    cached_at = datetime.fromisoformat(row["cached_at"]).replace(tzinfo=timezone.utc)
    if datetime.now(timezone.utc) - cached_at > _CACHE_TTL:
        return None
    return json.loads(row["results"])


def _cache_set(q: str, results: list[dict]) -> None:
    now = datetime.now(timezone.utc).isoformat()
    with get_conn() as conn:
        conn.execute(
            "INSERT INTO search_cache (query, results, cached_at) VALUES (?, ?, ?) "
            "ON CONFLICT(query) DO UPDATE SET results=excluded.results, cached_at=excluded.cached_at",
            (q, json.dumps(results), now),
        )
