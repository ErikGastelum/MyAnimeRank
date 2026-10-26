"""FR-50: server-side TTL cache for Discover/Feed only.

Explicitly NOT used for Favorites (FR-40: "not cached") or any Library/sync
data -- this exists purely to keep Discover/Feed tab-switching within
AniList's ~90 req/min limit, not as a general-purpose cache.
"""
import time
from collections.abc import Awaitable, Callable

_store: dict[str, tuple[float, object]] = {}


async def get_or_fetch(key: str, ttl_seconds: int, fetch: Callable[[], Awaitable]) -> object:
    now = time.monotonic()
    cached = _store.get(key)
    if cached is not None:
        cached_at, value = cached
        if now - cached_at < ttl_seconds:
            return value

    value = await fetch()
    _store[key] = (now, value)
    return value
