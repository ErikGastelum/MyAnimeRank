"""FastAPI routes for MyAnimeRank.

Also serves frontend/index.html at GET / -- app.py (pywebview shell) points
its window at this same process, so the single-file frontend and the API
now share one server. Still runs standalone for browser dev too:
uvicorn main:app --port 8420
"""
import asyncio
import logging
import math
from pathlib import Path

import httpx
from fastapi import BackgroundTasks, Depends, FastAPI, HTTPException
from fastapi.responses import FileResponse, RedirectResponse

import anilist
import cache
import database
import local_config
import oauth_listener
import sync
from models import (
    AnchorsResponse,
    AuthStatus,
    CharacterDetail,
    CharacterSearchResponse,
    CommunityResponse,
    DeleteResult,
    DiscoverResponse,
    FavoritesResponse,
    FavoriteToggleIn,
    FavoriteToggleOut,
    FeedResponse,
    InsightsResponse,
    LibraryResponse,
    LogoutResult,
    MediaImport,
    MediaOut,
    PendingConflictsResult,
    ProfileStatsResponse,
    ProgressIn,
    ProgressOut,
    PublicUserDetail,
    PushResult,
    RankingsResponse,
    RatingIn,
    RatingOut,
    RatingResult,
    RepeatIn,
    RepeatOut,
    ResolveIn,
    ResolveOut,
    ScoreFormatResult,
    SearchResponse,
    StaffDetail,
    StaffSearchResponse,
    StatusIn,
    StatusOut,
    StudioDetail,
    StudioSearchResponse,
    SyncPullResult,
    UserSearchResponse,
)

logging.basicConfig(level=logging.INFO)
log = logging.getLogger(__name__)

app = FastAPI(title="MyAnimeRank API")

# No CORS middleware, deliberately: the frontend is served same-origin from
# GET / below, so it never needs CORS -- and adding it would let any other
# site open in a browser call (and read) these endpoints, several of which
# write to the user's real AniList account.

_FRONTEND_PATH = Path(__file__).parent / "frontend" / "index.html"


# frontend/index.html is a single self-contained file (no separate CSS/JS/
# image assets), so one exact route at / is enough -- no StaticFiles mount
# needed, and no risk of shadowing any of the bare-path API routes below
# (none of them match "/" itself).
@app.get("/", include_in_schema=False)
def serve_frontend() -> FileResponse:
    return FileResponse(_FRONTEND_PATH)


@app.on_event("startup")
def _startup() -> None:
    database.init_db()
    asyncio.create_task(sync.retry_loop())


# FR-47: no route/screen is usable without a valid token. Applied to every
# route below except the auth routes themselves (you need to be able to
# reach login before you're authenticated, obviously).
def require_auth() -> dict:
    auth = database.get_auth()
    if not auth:
        raise HTTPException(status_code=401, detail="not connected")
    return auth


def _to_media_out(record: dict) -> MediaOut:
    rating = record.get("rating")
    return MediaOut(
        **{k: v for k, v in record.items() if k != "rating"},
        rating=RatingOut(**rating) if rating else None,
    )


def _validate_rating_audio(media_type: str, audio: float) -> None:
    if media_type == "manga":
        if audio != 0.0:
            raise HTTPException(status_code=422, detail="audio must be 0.0 for manga")
        return

    is_half_step = math.isfinite(audio) and (audio * 2).is_integer()
    if not 1.0 <= audio <= 10.0 or not is_half_step:
        raise HTTPException(
            status_code=422,
            detail="audio must be between 1.0 and 10.0 in 0.5 increments for anime",
        )


# ---------------------------------------------------------------- auth ----


@app.get("/auth/anilist/login")
def auth_login():
    oauth_listener.start_listener()
    url = anilist.authorize_url(
        local_config.ANILIST_CLIENT_ID, oauth_listener.OAUTH_REDIRECT_URI
    )
    return RedirectResponse(url)


@app.get("/auth/status", response_model=AuthStatus)
def auth_status():
    auth = database.get_auth()
    return {"connected": auth is not None, "username": auth["anilist_username"] if auth else None}


@app.post("/auth/logout", response_model=LogoutResult)
def auth_logout():
    database.clear_auth()  # FR-27: token only, library data untouched
    return {"connected": False}


@app.post("/auth/score-format/confirm", response_model=ScoreFormatResult)
async def confirm_score_format(auth: dict = Depends(require_auth)):
    """FR-25/FR-28: only ever called after the frontend has shown an explicit
    consent dialog -- this endpoint performs the actual AniList mutation."""
    new_format = await anilist.set_score_format(auth["access_token"])
    database.update_score_format(new_format)
    return {"score_format": new_format}


# ------------------------------------------------------------- search/media ----


@app.get("/search", response_model=SearchResponse, dependencies=[Depends(require_auth)])
async def search_media(q: str, type: str):
    if type not in ("anime", "manga"):
        raise HTTPException(status_code=400, detail="type must be 'anime' or 'manga'")
    results = await anilist.search(q, type)
    return {"results": results, "q": q}


@app.post("/media", dependencies=[Depends(require_auth)])
async def import_media(body: MediaImport):
    media_id = database.insert_or_update_media(body.model_dump())
    return {"id": media_id}


@app.get("/media/{media_id}", response_model=MediaOut, dependencies=[Depends(require_auth)])
def get_media(media_id: int):
    record = database.get_media_by_id(media_id)
    if not record:
        raise HTTPException(status_code=404, detail="media not found")
    return _to_media_out(record)


@app.get("/media/by-anilist/{anilist_id}", dependencies=[Depends(require_auth)])
async def get_media_by_anilist(anilist_id: int):
    record = database.get_media_by_anilist_id(anilist_id)
    if record:
        out = _to_media_out(record)
        return {"in_library": True, **out.model_dump()}

    fields = await anilist.get_by_id(anilist_id)
    if not fields:
        raise HTTPException(status_code=404, detail="anilist_id not found")
    return {"in_library": False, **fields}


@app.put("/media/{media_id}/rating", response_model=RatingResult, dependencies=[Depends(require_auth)])
def put_rating(media_id: int, body: RatingIn, background_tasks: BackgroundTasks):
    media = database.get_media_by_id(media_id)
    if media is None:
        raise HTTPException(status_code=404, detail="media not found")
    _validate_rating_audio(media["media_type"], body.audio)

    result = database.upsert_rating(media_id, body.model_dump())
    # FR-24/FR-25: local changes push automatically in the background -- never
    # awaited inline, so a slow/unreachable AniList never blocks the response.
    background_tasks.add_task(sync.push_now_guarded, media_id)
    return result


@app.get("/media/{media_id}/anchors", response_model=AnchorsResponse, dependencies=[Depends(require_auth)])
def get_anchors(media_id: int, criterion: str, scope: str = "genre"):
    if scope not in ("genre", "all"):
        raise HTTPException(status_code=400, detail="scope must be 'genre' or 'all'")
    try:
        anchors = database.get_anchors(media_id, criterion, scope)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    if anchors is None:
        raise HTTPException(status_code=404, detail="media not found")
    return {"anchors": anchors}


@app.put("/media/{media_id}/status", response_model=StatusOut, dependencies=[Depends(require_auth)])
def put_status(media_id: int, body: StatusIn, background_tasks: BackgroundTasks):
    status = database.update_status(media_id, body.status)
    if status is None:
        raise HTTPException(status_code=404, detail="media not found")
    background_tasks.add_task(sync.push_now_guarded, media_id)
    return {"status": status}


@app.put("/media/{media_id}/progress", response_model=ProgressOut, dependencies=[Depends(require_auth)])
def put_progress(media_id: int, body: ProgressIn, background_tasks: BackgroundTasks):
    progress = database.update_progress(media_id, body.progress)
    if progress is None:
        raise HTTPException(status_code=404, detail="media not found")
    background_tasks.add_task(sync.push_now_guarded, media_id)
    return {"progress": progress}


@app.put("/media/{media_id}/repeat", response_model=RepeatOut, dependencies=[Depends(require_auth)])
def put_repeat(media_id: int, body: RepeatIn, background_tasks: BackgroundTasks):
    repeat_count = database.update_repeat_count(media_id, body.repeat_count)
    if repeat_count is None:
        raise HTTPException(status_code=404, detail="media not found")
    background_tasks.add_task(sync.push_now_guarded, media_id)
    return {"repeat_count": repeat_count}


# ---------------------------------------------------------------- sync ----


@app.post("/sync/pull", response_model=SyncPullResult, dependencies=[Depends(require_auth)])
async def sync_pull():
    return await sync.pull()


@app.get("/sync/pending-conflicts", response_model=PendingConflictsResult, dependencies=[Depends(require_auth)])
async def sync_pending_conflicts():
    return {"conflicts": await sync.get_pending_conflicts()}


@app.post("/sync/resolve/{media_id}", response_model=ResolveOut, dependencies=[Depends(require_auth)])
async def sync_resolve(media_id: int, body: ResolveIn):
    if body.winner not in ("local", "remote"):
        raise HTTPException(status_code=400, detail="winner must be 'local' or 'remote'")
    try:
        return await sync.resolve(media_id, body.winner)
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))


@app.post("/sync/push/{media_id}", response_model=PushResult, dependencies=[Depends(require_auth)])
async def sync_push(media_id: int):
    try:
        return await sync.push_now(media_id)
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))
    except Exception as e:
        raise HTTPException(status_code=502, detail=f"push failed, queued for retry: {e}")


# ---------------------------------------------------- discover / feed (M5) ----


@app.get("/discover", response_model=DiscoverResponse, dependencies=[Depends(require_auth)])
async def get_discover(section: str, media_type: str):
    valid_sections = {"trending", "current_season", "upcoming_season", "top_ranked", "top_ranked_movies", "popular"}
    if section not in valid_sections:
        raise HTTPException(status_code=400, detail=f"section must be one of {sorted(valid_sections)}")
    if media_type not in ("anime", "manga"):
        raise HTTPException(status_code=400, detail="media_type must be 'anime' or 'manga'")

    async def fetch():
        return await anilist.discover(section, media_type)

    results = await cache.get_or_fetch(f"discover:{section}:{media_type}", 60, fetch)
    # FR-33: annotate with local_status by cross-referencing the local library, null if not imported.
    annotated = []
    for r in results:
        local = database.get_media_by_anilist_id(r["anilist_id"])
        annotated.append({**r, "local_status": local["status"] if local else None})
    return {"results": annotated, "count": len(annotated)}


@app.get("/feed", response_model=FeedResponse)
async def get_feed(type: str, page: int = 1, auth: dict = Depends(require_auth)):
    if type not in ("your", "following", "global"):
        raise HTTPException(status_code=400, detail="type must be 'your', 'following', or 'global'")

    async def fetch():
        return await anilist.feed(type, page, auth["anilist_user_id"], auth["access_token"])

    return await cache.get_or_fetch(f"feed:{type}:{page}", 60, fetch)


# --------------------------------------------------- characters / staff (M5) ----


@app.get("/characters/search", response_model=CharacterSearchResponse, dependencies=[Depends(require_auth)])
async def characters_search(q: str):
    return {"results": await anilist.search_characters(q)}


@app.get("/staff/search", response_model=StaffSearchResponse, dependencies=[Depends(require_auth)])
async def staff_search(q: str):
    return {"results": await anilist.search_staff(q)}


@app.get("/characters/{anilist_id}", response_model=CharacterDetail, dependencies=[Depends(require_auth)])
async def characters_detail(anilist_id: int):
    result = await anilist.get_character(anilist_id)
    if not result:
        raise HTTPException(status_code=404, detail="character not found")
    return result


@app.get("/staff/{anilist_id}", response_model=StaffDetail, dependencies=[Depends(require_auth)])
async def staff_detail(anilist_id: int):
    result = await anilist.get_staff(anilist_id)
    if not result:
        raise HTTPException(status_code=404, detail="staff not found")
    return result


# ------------------------------------------------------- studios / users (M9) ----


@app.get("/studios/search", response_model=StudioSearchResponse, dependencies=[Depends(require_auth)])
async def studios_search(q: str):
    return {"results": await anilist.search_studios(q)}


@app.get("/studios/{anilist_id}", response_model=StudioDetail, dependencies=[Depends(require_auth)])
async def studios_detail(anilist_id: int):
    result = await anilist.get_studio(anilist_id)
    if not result:
        raise HTTPException(status_code=404, detail="studio not found")
    return result


@app.get("/users/search", response_model=UserSearchResponse, dependencies=[Depends(require_auth)])
async def users_search(q: str):
    return {"results": await anilist.search_users(q)}


@app.get("/users/{anilist_id}", response_model=PublicUserDetail, dependencies=[Depends(require_auth)])
async def users_detail(anilist_id: int):
    result = await anilist.get_user(anilist_id)
    if not result:
        raise HTTPException(status_code=404, detail="user not found")
    return result


# ------------------------------------------------------- favorites (M5) ----


@app.post("/favorites/toggle", response_model=FavoriteToggleOut)
async def favorites_toggle(body: FavoriteToggleIn, auth: dict = Depends(require_auth)):
    if body.type not in ("anime", "manga", "character", "staff", "studio"):
        raise HTTPException(status_code=400, detail="invalid favorite type")
    favorited = await anilist.toggle_favorite(auth["access_token"], body.type, body.anilist_id)
    return {"favorited": favorited}


@app.get("/favorites", response_model=FavoritesResponse)
async def favorites_get(auth: dict = Depends(require_auth)):
    """FR-40: live every call, deliberately not cached (unlike Discover/Feed)."""
    return await anilist.get_favorites(auth["access_token"])


# ------------------------------------------------------- community (M5) ----


@app.get("/media/{media_id}/community", response_model=CommunityResponse, dependencies=[Depends(require_auth)])
async def media_community(media_id: int):
    local = database.get_media_by_id(media_id)
    if not local:
        raise HTTPException(status_code=404, detail="media not found")
    # FR-55: query by the row's own stored anilist_id -- never re-derive by title.
    result = await anilist.get_community(local["anilist_id"])
    if not result:
        raise HTTPException(status_code=404, detail="community data not found")
    return result


# FR-52: not-yet-imported titles (Discover-sourced) have no local media_id to
# look up -- they already have the real anilist_id, so skip straight to it
# instead of requiring an import first. Same anilist.get_community() call as
# media_community() above; this route only removes the local-row indirection.
@app.get(
    "/media/by-anilist/{anilist_id}/community",
    response_model=CommunityResponse,
    dependencies=[Depends(require_auth)],
)
async def media_community_by_anilist(anilist_id: int):
    result = await anilist.get_community(anilist_id)
    if not result:
        raise HTTPException(status_code=404, detail="community data not found")
    return result


# ------------------------------------------------------- library/rankings/profile (M6) ----


@app.get("/library", response_model=LibraryResponse, dependencies=[Depends(require_auth)])
def get_library(media_type: str | None = None, status: str = "all", sort: str = "overall"):
    if media_type is not None and media_type not in ("anime", "manga"):
        raise HTTPException(status_code=400, detail="media_type must be 'anime' or 'manga'")
    results = database.get_library(media_type, status, sort)
    return {"results": [_to_media_out(r) for r in results], "count": len(results)}


@app.delete("/media/{media_id}", response_model=DeleteResult, dependencies=[Depends(require_auth)])
async def delete_media(media_id: int):
    if not database.get_media_by_id(media_id):
        raise HTTPException(status_code=404, detail="media not found")
    try:
        await sync.delete_remote_entry(media_id)
    except httpx.HTTPError:
        raise HTTPException(status_code=502, detail="failed to delete from AniList; local entry not removed")
    deleted = database.delete_media(media_id)
    return {"deleted": deleted}


@app.get("/insights", response_model=InsightsResponse, dependencies=[Depends(require_auth)])
def get_insights():
    return database.get_insights()


@app.get("/rankings", response_model=RankingsResponse, dependencies=[Depends(require_auth)])
def get_rankings(
    media_type: str | None = None,
    genre: str | None = None,
    year: int | None = None,
    sort: str = "overall",
    q: str | None = None,
):
    if media_type is not None and media_type not in ("anime", "manga"):
        raise HTTPException(status_code=400, detail="media_type must be 'anime' or 'manga'")
    results = database.get_rankings(media_type, genre, year, sort, q)
    return {"results": [_to_media_out(r) for r in results], "count": len(results)}


@app.get("/profile/stats", response_model=ProfileStatsResponse, dependencies=[Depends(require_auth)])
async def profile_stats(auth: dict = Depends(require_auth)):
    """FR-42: all six stats pulled live from AniList's viewer statistics --
    not local library counts, even for total_anime/total_manga."""
    return await anilist.get_profile_stats(auth["access_token"], auth["anilist_user_id"])
