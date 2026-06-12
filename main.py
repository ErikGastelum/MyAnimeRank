import difflib
import io
import os
import json
import re
import sys
import time
import xml.etree.ElementTree as ET
import zipfile
from contextlib import asynccontextmanager
from datetime import date, datetime
from pathlib import Path

import httpx
import logging

from fastapi import BackgroundTasks, FastAPI, HTTPException, Query, Request, UploadFile
from fastapi.responses import FileResponse, Response
from pydantic import BaseModel as _BaseModel

from compare import normalize_score, compare_friend
from larp import compute_larp_self, compute_larp_friend, WEIGHTS as _LARP_WEIGHTS

_log = logging.getLogger("myanimrank")


def _strip_html(s: str | None) -> str | None:
    if not s:
        return s
    s = re.sub(r'<[^>]+>', ' ', s)
    s = re.sub(r'\s{2,}', ' ', s)
    return s.strip() or None

from database import get_conn, init_db, DB_PATH

_BACKUPS_DIR = DB_PATH.parent / "backups"
from jikan import search as jikan_search, seasonal as jikan_seasonal, by_mal_id as jikan_by_id
from models import AnimeCreate, AnimeUpdate, RatingIn

# When frozen by PyInstaller, data files live in sys._MEIPASS; otherwise next to this script.
_BASE = Path(sys._MEIPASS) if getattr(sys, "frozen", False) else Path(__file__).parent
HTML_FILE = _BASE / "MyAnimeRank.html"

_VALID_CRITERIA = {"story", "characters", "animation", "audio", "enjoyment"}
_SORT_COLS = {c: f"r.{c}" for c in _VALID_CRITERIA}
_SORT_COLS["overall"] = "r.overall"

ANILIST_CLIENT_ID = 43290
try:
    from _secrets import ANILIST_CLIENT_SECRET  # type: ignore[import]  # git-ignored, set before release
except ImportError:
    ANILIST_CLIENT_SECRET: str = os.environ.get("ANILIST_CLIENT_SECRET", "")

_PAGE_CSS = (
    "<style>body{font-family:system-ui,sans-serif;background:#0c0d10;color:#eef1f6;"
    "display:flex;align-items:center;justify-content:center;height:100vh;margin:0;"
    "flex-direction:column;gap:14px;font-size:16px;}"
    ".ok{color:#4ade80;}.err{color:#f87171;}"
    "p{margin:0;max-width:420px;text-align:center;line-height:1.6;}</style>"
)


def _callback_page(success: bool, detail: str) -> str:
    if success:
        body = (
            f'<span class="ok">&#10003; Connected as <strong>{detail}</strong>!</span>'
            "<br><br>You can close this tab and return to MyAnimeRank."
        )
    else:
        body = f'<span class="err">&#10007; {detail}</span><br><br>Close this tab and try again.'
    return (
        '<!DOCTYPE html><html><head><meta charset="UTF-8">'
        "<title>AniList — MyAnimeRank</title>"
        f"{_PAGE_CSS}</head><body><p>{body}</p></body></html>"
    )


def _get_setting(key: str) -> str | None:
    with get_conn() as conn:
        row = conn.execute("SELECT value FROM settings WHERE key=?", (key,)).fetchone()
        return row["value"] if row else None


def _set_setting(key: str, value: str | None) -> None:
    with get_conn() as conn:
        if value is None:
            conn.execute("DELETE FROM settings WHERE key=?", (key,))
        else:
            conn.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)", (key, value)
            )


_STATUS_MAP = {
    "completed":     "COMPLETED",
    "watching":      "CURRENT",
    "rewatching":    "REPEATING",
    "planning":      "PLANNING",
    "dropped":       "DROPPED",
    "paused":        "PAUSED",
    "repeating":     "REPEATING",
    # legacy values stored before rename
    "plan_to_watch": "PLANNING",
    "on_hold":       "PAUSED",
}

_ANILIST_STATUS_REVERSE = {
    "COMPLETED":  "completed",
    "CURRENT":    "watching",
    "REPEATING":  "rewatching",
    "PLANNING":   "planning",
    "DROPPED":    "dropped",
    "PAUSED":     "paused",
}


def _get_anilist_id(anime_id: int, token: str) -> int | None:
    with get_conn() as conn:
        row = conn.execute(
            "SELECT anilist_id, api_id, title FROM anime WHERE id=?", (anime_id,)
        ).fetchone()
        if not row:
            return None
        if row["anilist_id"]:
            try:
                return int(row["anilist_id"])
            except (ValueError, TypeError):
                pass
        mal_id = row["api_id"]
        title  = row["title"]

    headers = {
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
        "Accept": "application/json",
    }

    if mal_id:
        try:
            resp = httpx.post(
                "https://graphql.anilist.co",
                json={
                    "query": "query($idMal:Int){Media(idMal:$idMal,type:ANIME){id}}",
                    "variables": {"idMal": int(mal_id)},
                },
                headers=headers,
                timeout=8.0,
            )
            resp.raise_for_status()
            al_id = resp.json().get("data", {}).get("Media", {}).get("id")
            if al_id:
                with get_conn() as conn:
                    conn.execute(
                        "UPDATE anime SET anilist_id=? WHERE id=?", (str(al_id), anime_id)
                    )
                return int(al_id)
        except Exception:
            pass

    # Fallback: search by title (covers seed anime that have no MAL ID)
    if title:
        try:
            resp = httpx.post(
                "https://graphql.anilist.co",
                json={
                    "query": "query($search:String){Media(search:$search,type:ANIME){id idMal title{english romaji native}}}",
                    "variables": {"search": title},
                },
                headers=headers,
                timeout=8.0,
            )
            resp.raise_for_status()
            media = resp.json().get("data", {}).get("Media") or {}
            al_id = media.get("id")
            if al_id:
                al_titles = media.get("title") or {}
                candidates = [v.lower() for v in al_titles.values() if v]
                best = max(
                    (difflib.SequenceMatcher(None, title.lower(), c).ratio() for c in candidates),
                    default=0,
                )
                if best < 0.6:
                    _log.warning("_get_anilist_id: title mismatch for %r (best=%.2f), skipping", title, best)
                    return None
                with get_conn() as conn:
                    conn.execute(
                        "UPDATE anime SET anilist_id=?, api_id=COALESCE(api_id,?) WHERE id=?",
                        (str(al_id), str(media["idMal"]) if media.get("idMal") else None, anime_id),
                    )
                return int(al_id)
        except Exception:
            pass

    return None


# Maps each app criterion to common AniList category name aliases (lowercase).
_ADV_SCORE_ALIASES: dict[str, list[str]] = {
    "story":      ["story", "narrative", "plot", "writing"],
    "characters": ["characters", "character", "chars"],
    "animation":  ["animation", "art", "visuals", "visual", "artwork", "visual production"],
    "audio":      ["audio", "music", "sound", "soundtrack", "ost"],
    "enjoyment":  ["enjoyment", "fun", "overall enjoyment"],
}


def _match_adv_score(label: str, row: dict) -> float:
    """Return the app score whose criterion best matches an AniList category label."""
    key = label.lower().strip()
    for criterion, aliases in _ADV_SCORE_ALIASES.items():
        if any(key == a or key.startswith(a) for a in aliases):
            return float(row[criterion])
    return 0.0


def _map_adv_scores_from_anilist(adv_scores, use100: bool) -> dict | None:
    """Map AniList advancedScores JSON object {"Story": 8.0, ...} to app criterion dict."""
    if not adv_scores or not isinstance(adv_scores, dict):
        return None
    result: dict[str, float] = {}
    for label, raw in adv_scores.items():
        if not raw:
            continue
        key = label.lower().strip()
        for criterion, aliases in _ADV_SCORE_ALIASES.items():
            if any(key == a or key.startswith(a) for a in aliases):
                result[criterion] = round(min(10.0, max(1.0, raw / 10 if use100 else float(raw))), 1)
                break
    return result if result else None


def _fetch_and_cache_viewer(token: str) -> None:
    """Fetch the AniList Viewer profile and cache all derived settings."""
    try:
        vr = httpx.post(
            "https://graphql.anilist.co",
            json={"query": "query{Viewer{id name mediaListOptions{scoreFormat animeList{advancedScoringEnabled advancedScoring}}}}"},
            headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json", "Accept": "application/json"},
            timeout=10.0,
        )
        vr.raise_for_status()
        viewer   = vr.json().get("data", {}).get("Viewer") or {}
        opts     = viewer.get("mediaListOptions") or {}
        adv_opts = opts.get("animeList") or {}
        _set_setting("anilist_adv_enabled",  "1" if adv_opts.get("advancedScoringEnabled") else "0")
        _set_setting("anilist_adv_scoring",  json.dumps(adv_opts.get("advancedScoring") or []))
        _set_setting("anilist_score_format", opts.get("scoreFormat") or "POINT_10_DECIMAL")
        if viewer.get("id") and not _get_setting("anilist_user_id"):
            _set_setting("anilist_user_id", str(viewer["id"]))
        if viewer.get("name") and not _get_setting("anilist_username"):
            _set_setting("anilist_username", viewer["name"])
    except Exception:
        _log.exception("Failed to refresh AniList Viewer profile")


def _anilist_sync(anime_id: int, _retry: bool = False) -> None:
    token = _get_setting("anilist_token")
    if not token:
        return

    # Populate viewer settings on-demand if they were never saved (e.g. pre-v2 login).
    if _get_setting("anilist_adv_enabled") is None:
        _fetch_and_cache_viewer(token)

    al_id = _get_anilist_id(anime_id, token)
    if not al_id:
        return

    with get_conn() as conn:
        row = conn.execute(
            "SELECT a.status, a.episodes_watched, a.rewatch_count, r.story, r.characters, r.animation, r.audio, r.enjoyment, r.overall "
            "FROM anime a LEFT JOIN rating r ON a.id=r.anime_id WHERE a.id=?",
            (anime_id,),
        ).fetchone()

    if not row:
        return

    variables: dict = {
        "mediaId":  al_id,
        "status":   _STATUS_MAP.get(row["status"] or "completed", "COMPLETED"),
        "progress": row["episodes_watched"] or 0,
    }
    if row["rewatch_count"] is not None:
        variables["repeat"] = int(row["rewatch_count"])

    if row["overall"] is not None:
        score_format = _get_setting("anilist_score_format") or "POINT_10_DECIMAL"
        variables["score"] = float(row["overall"]) * 10 if "100" in score_format else float(row["overall"])

        adv_enabled = _get_setting("anilist_adv_enabled") == "1"
        if adv_enabled:
            categories = json.loads(_get_setting("anilist_adv_scoring") or "[]")
            if categories:
                variables["advancedScores"] = [_match_adv_score(cat, row) for cat in categories]

    mutation = (
        "mutation($mediaId:Int,$score:Float,$status:MediaListStatus,$advancedScores:[Float],$progress:Int,$repeat:Int){"
        "SaveMediaListEntry(mediaId:$mediaId,score:$score,status:$status,advancedScores:$advancedScores,progress:$progress,repeat:$repeat)"
        "{id score advancedScores progress repeat}}"
    )
    try:
        resp = httpx.post(
            "https://graphql.anilist.co",
            json={"query": mutation, "variables": variables},
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
            timeout=10.0,
        )
        if not resp.is_success:
            if resp.status_code == 400 and not _retry:
                # Stale advancedScores settings (user changed categories on AniList) — refresh and retry once.
                _fetch_and_cache_viewer(token)
                _anilist_sync(anime_id, _retry=True)
                return
            _set_setting("anilist_sync_error", "token_expired" if resp.status_code == 401 else f"http_{resp.status_code}")
            _log.error("AniList sync failed for anime_id=%s (HTTP %s): %s", anime_id, resp.status_code, resp.text[:500])
            return
        _set_setting("anilist_stats_fetched_at", "0")
        _set_setting("anilist_sync_error", None)
    except Exception:
        _set_setting("anilist_sync_error", "network_error")
        _log.exception("AniList sync failed for anime_id=%s", anime_id)


@asynccontextmanager
async def lifespan(app: FastAPI):
    init_db()
    yield


app = FastAPI(title="MyAnimeRank", lifespan=lifespan)


@app.get("/")
def index():
    return FileResponse(HTML_FILE)


class _AniListAuthStartIn(_BaseModel):
    redirect_uri: str


@app.post("/anilist/auth-start")
def anilist_auth_start(body: _AniListAuthStartIn):
    _set_setting("anilist_redirect_uri", body.redirect_uri)
    return {"ok": True}


def _save_anilist_token(access_token: str) -> dict:
    username = ""
    adv_enabled = False
    adv_scoring: list = []
    viewer_id = None
    score_format = "POINT_10_DECIMAL"
    avatar = ""
    banner = ""
    try:
        vr = httpx.post(
            "https://graphql.anilist.co",
            json={"query": "query{Viewer{id name avatar{large} bannerImage mediaListOptions{scoreFormat animeList{advancedScoringEnabled advancedScoring}}}}"},
            headers={
                "Authorization": f"Bearer {access_token}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
            timeout=10.0,
        )
        vr.raise_for_status()
        viewer       = vr.json().get("data", {}).get("Viewer") or {}
        opts         = viewer.get("mediaListOptions") or {}
        adv_opts     = opts.get("animeList") or {}
        username     = viewer.get("name", "")
        adv_enabled  = adv_opts.get("advancedScoringEnabled", False)
        adv_scoring  = adv_opts.get("advancedScoring", [])
        viewer_id    = viewer.get("id")
        score_format = opts.get("scoreFormat", "POINT_10_DECIMAL")
        avatar       = (viewer.get("avatar") or {}).get("large") or ""
        banner       = viewer.get("bannerImage") or ""
    except Exception:
        _log.exception("Failed to fetch AniList Viewer profile — token will still be saved")
    _set_setting("anilist_token",        access_token)
    _set_setting("anilist_username",     username)
    _set_setting("anilist_user_id",      str(viewer_id) if viewer_id else None)
    _set_setting("anilist_adv_enabled",  "1" if adv_enabled else "0")
    _set_setting("anilist_adv_scoring",  json.dumps(adv_scoring))
    _set_setting("anilist_score_format", score_format)
    _set_setting("anilist_avatar",       avatar or None)
    _set_setting("anilist_banner",       banner or None)
    return {"username": username, "adv_enabled": adv_enabled, "adv_scoring": adv_scoring}


@app.get("/anilist/callback")
def anilist_callback(
    request: Request,
    code: str = Query(None),
    error: str = Query(None),
    message: str = Query(None),
):
    _log.warning("AniList callback received — full URL: %s", request.url)
    if error:
        detail = f"{error}" + (f": {message}" if message else "")
        return Response(content=_callback_page(False, f"AniList error — {detail}"), media_type="text/html")
    if not code:
        return Response(content=_callback_page(False, "No authorization code received."), media_type="text/html")

    redirect_uri = _get_setting("anilist_redirect_uri")
    if not redirect_uri:
        return Response(content=_callback_page(False, "App not configured — open MyAnimeRank and click Connect AniList first."), media_type="text/html")
    client_secret = ANILIST_CLIENT_SECRET

    try:
        tr = httpx.post(
            "https://anilist.co/api/v2/oauth/token",
            json={
                "grant_type":    "authorization_code",
                "client_id":     ANILIST_CLIENT_ID,
                "client_secret": client_secret,
                "redirect_uri":  redirect_uri,
                "code":          code,
            },
            headers={"Content-Type": "application/json", "Accept": "application/json"},
            timeout=10.0,
        )
        if not tr.is_success:
            body = tr.json() if tr.headers.get("content-type", "").startswith("application/json") else {}
            al_error = body.get("error") or body.get("message") or tr.text[:200]
            return Response(
                content=_callback_page(False, f"Token exchange failed ({tr.status_code}): {al_error}"),
                media_type="text/html",
            )
        access_token = tr.json().get("access_token", "")
        if not access_token:
            raise ValueError("empty access_token")
    except Exception as exc:
        return Response(content=_callback_page(False, f"Token exchange failed: {exc}"), media_type="text/html")

    try:
        info = _save_anilist_token(access_token)
        display_name = info.get("username") or "unknown"
    except Exception:
        _log.exception("Failed to persist AniList token metadata — token was saved, profile info may be missing")
        display_name = "unknown"
    return Response(content=_callback_page(True, display_name), media_type="text/html")


@app.get("/anilist/token")
def anilist_status():
    token    = _get_setting("anilist_token")
    username = _get_setting("anilist_username")
    adv_cats     = json.loads(_get_setting("anilist_adv_scoring") or "[]")
    score_format = _get_setting("anilist_score_format") or "POINT_10_DECIMAL"
    sync_error = _get_setting("anilist_sync_error")
    return {"connected": bool(token), "username": username or None, "adv_scoring": adv_cats, "score_format": score_format, "sync_error": sync_error}


@app.delete("/anilist/token", status_code=204)
def anilist_disconnect():
    for key in ("anilist_token", "anilist_username", "anilist_user_id",
                "anilist_adv_enabled", "anilist_adv_scoring", "anilist_redirect_uri",
                "anilist_avatar", "anilist_banner",
                "anilist_total_anime", "anilist_days_watched", "anilist_mean_score",
                "anilist_stats_fetched_at", "anilist_sync_error"):
        _set_setting(key, None)


@app.get("/anilist/profile")
def anilist_profile_stats():
    token = _get_setting("anilist_token")
    if not token:
        raise HTTPException(status_code=404, detail="Not connected")

    avatar       = _get_setting("anilist_avatar") or ""
    banner       = _get_setting("anilist_banner") or ""
    total_anime  = int(_get_setting("anilist_total_anime") or 0)
    days_watched = float(_get_setting("anilist_days_watched") or 0)
    mean_score   = float(_get_setting("anilist_mean_score") or 0)
    fetched_at   = float(_get_setting("anilist_stats_fetched_at") or 0)

    # Re-fetch from AniList at most once every 5 minutes
    if time.time() - fetched_at > 300:
        try:
            vr = httpx.post(
                "https://graphql.anilist.co",
                json={"query": "query{Viewer{avatar{large} bannerImage statistics{anime{count minutesWatched meanScore}}}}"},
                headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json", "Accept": "application/json"},
                timeout=10.0,
            )
            vr.raise_for_status()
            viewer       = vr.json().get("data", {}).get("Viewer") or {}
            avatar       = (viewer.get("avatar") or {}).get("large") or avatar
            banner       = viewer.get("bannerImage") or banner
            stats        = (viewer.get("statistics") or {}).get("anime") or {}
            total_anime  = stats.get("count") or 0
            minutes      = stats.get("minutesWatched") or 0
            days_watched = round(minutes / 1440, 1)
            mean_score   = round(float(stats.get("meanScore") or 0), 1)
            _set_setting("anilist_avatar",           avatar or None)
            _set_setting("anilist_banner",           banner or None)
            _set_setting("anilist_total_anime",      str(total_anime))
            _set_setting("anilist_days_watched",     str(days_watched))
            _set_setting("anilist_mean_score",       str(mean_score))
            _set_setting("anilist_stats_fetched_at", str(time.time()))
        except Exception:
            _log.exception("Failed to fetch AniList profile stats")

    return {
        "avatar":      avatar,
        "banner":      banner,
        "totalAnime":  total_anime,
        "daysWatched": days_watched,
        "meanScore":   mean_score,
    }


def _log_sync(conn, anime_id: int, field: str, source: str, old_value, new_value) -> None:
    if str(old_value) == str(new_value):
        return
    conn.execute(
        "INSERT INTO sync_log (anime_id, field, source, old_value, new_value, timestamp) VALUES (?,?,?,?,?,?)",
        (anime_id, field, source, str(old_value) if old_value is not None else None,
         str(new_value) if new_value is not None else None, datetime.now().isoformat()),
    )


def _auto_backup() -> None:
    try:
        with get_conn() as conn:
            rows = conn.execute(
                "SELECT a.*, r.story, r.characters, r.animation, r.audio, r.enjoyment, r.overall, r.updated_at "
                "FROM anime a LEFT JOIN rating r ON a.id = r.anime_id"
            ).fetchall()
        entries = [dict(r) | {"genres": json.loads(r["genres"] or "[]")} for r in rows]
        _BACKUPS_DIR.mkdir(parents=True, exist_ok=True)
        ts = datetime.now().strftime("%Y-%m-%d_%H-%M-%S")
        path = _BACKUPS_DIR / f"backup_{ts}.json"
        path.write_text(json.dumps({"version": 1, "entries": entries}, ensure_ascii=False), encoding="utf-8")
        for old in sorted(_BACKUPS_DIR.glob("backup_*.json"))[:-10]:
            old.unlink(missing_ok=True)
    except Exception:
        _log.exception("auto-backup failed")


def _pull_anilist_library() -> dict:
    token = _get_setting("anilist_token")
    if not token:
        return {"added": 0, "updated": 0, "deleted": 0}

    user_id_str = _get_setting("anilist_user_id")
    if not user_id_str:
        # Connected before user_id storage was added — fetch and cache it now
        try:
            vr = httpx.post(
                "https://graphql.anilist.co",
                json={"query": "query{Viewer{id name}}"},
                headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json", "Accept": "application/json"},
                timeout=10.0,
            )
            viewer = vr.json().get("data", {}).get("Viewer") or {}
            uid = viewer.get("id")
            uname = viewer.get("name", "")
            if not uid:
                return {"added": 0, "updated": 0, "deleted": 0}
            user_id_str = str(uid)
            _set_setting("anilist_user_id", user_id_str)
            if uname and not _get_setting("anilist_username"):
                _set_setting("anilist_username", uname)
        except Exception:
            return {"added": 0, "updated": 0, "deleted": 0}

    query = """
    query($userId:Int){
      MediaListCollection(userId:$userId,type:ANIME){
        lists{entries{score advancedScores status repeat progress media{
          id idMal
          title{english romaji native}
          coverImage{large}
          episodes format averageScore status nextAiringEpisode{episode airingAt}
          startDate{year}
          genres
          description(asHtml:false)
        }}}
      }
    }"""
    try:
        resp = httpx.post(
            "https://graphql.anilist.co",
            json={"query": query, "variables": {"userId": int(user_id_str)}},
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
            timeout=30.0,
        )
        if not resp.is_success:
            return {"added": 0, "updated": 0, "deleted": 0}
        raw_entries = [
            e
            for lst in (resp.json().get("data", {}).get("MediaListCollection") or {}).get("lists", [])
            for e in lst.get("entries", [])
        ]
    except Exception:
        return {"added": 0, "updated": 0, "deleted": 0}

    scores = [e["score"] for e in raw_entries if e.get("score", 0) > 0]
    use100 = any(s > 10 for s in scores)

    added = updated = deleted = 0
    seen_al_ids: set[str] = set()
    today = date.today().isoformat()
    with get_conn() as conn:
        for e in raw_entries:
            media = e.get("media") or {}
            mal_id = media.get("idMal")
            al_id  = media.get("id")
            if al_id:
                seen_al_ids.add(str(al_id))
            titles = media.get("title") or {}
            title  = titles.get("english") or titles.get("romaji") or titles.get("native") or ""
            if not title:
                continue

            status = _ANILIST_STATUS_REVERSE.get(e.get("status", ""), "completed")
            is_airing = 1 if media.get("status") == "RELEASING" else 0
            nae_obj = media.get("nextAiringEpisode") or {}
            nae = nae_obj.get("episode")
            episodes_aired    = (nae - 1) if nae else None
            next_episode_at   = nae_obj.get("airingAt")  # Unix seconds, None when finished
            al_progress = e.get("progress") or 0
            al_repeat   = e.get("repeat") or 0
            raw_score = e.get("score", 0)
            score: float | None = None
            if raw_score > 0:
                score = round(min(10.0, max(1.0, raw_score / 10 if use100 else float(raw_score))), 1)
            adv = _map_adv_scores_from_anilist(e.get("advancedScores"), use100)

            existing = None
            if mal_id:
                existing = conn.execute(
                    "SELECT id FROM anime WHERE api_id=?", (str(mal_id),)
                ).fetchone()
            if not existing:
                existing = conn.execute(
                    "SELECT id FROM anime WHERE lower(title)=?", (title.lower(),)
                ).fetchone()

            if existing:
                anime_id = existing["id"]
                conn.execute(
                    "UPDATE anime SET status=?, is_airing=?, episodes_aired=?, next_episode_at=?,"
                    " episodes_watched=MAX(COALESCE(episodes_watched,0),?),"
                    " rewatch_count=MAX(COALESCE(rewatch_count,0),?) WHERE id=?",
                    (status, is_airing, episodes_aired, next_episode_at, al_progress, al_repeat, anime_id),
                )
                if al_id:
                    conn.execute(
                        "UPDATE anime SET anilist_id=? WHERE id=? AND (anilist_id IS NULL OR anilist_id='')",
                        (str(al_id), anime_id),
                    )
                existing_rating = conn.execute(
                    "SELECT overall, needs_reconcile FROM rating WHERE anime_id=?", (anime_id,)
                ).fetchone()
                if not existing_rating and score is not None:
                    conn.execute(
                        "INSERT OR REPLACE INTO rating "
                        "(anime_id,story,characters,animation,audio,enjoyment,overall,updated_at) "
                        "VALUES (?,?,?,?,?,?,?,?)",
                        (
                            anime_id,
                            adv.get("story", score) if adv else score,
                            adv.get("characters", score) if adv else score,
                            adv.get("animation", score) if adv else score,
                            adv.get("audio", score) if adv else score,
                            adv.get("enjoyment", score) if adv else score,
                            score,
                            today,
                        ),
                    )
                elif existing_rating and adv:
                    conn.execute(
                        "UPDATE rating SET "
                        "story=COALESCE(?,story), characters=COALESCE(?,characters), "
                        "animation=COALESCE(?,animation), audio=COALESCE(?,audio), "
                        "enjoyment=COALESCE(?,enjoyment), overall=COALESCE(?,overall), "
                        "updated_at=? WHERE anime_id=?",
                        (
                            adv.get("story"),
                            adv.get("characters"),
                            adv.get("animation"),
                            adv.get("audio"),
                            adv.get("enjoyment"),
                            score,
                            today,
                            anime_id,
                        ),
                    )
                elif (existing_rating and score is not None
                      and existing_rating["overall"] is not None
                      and abs(existing_rating["overall"] - score) > 0.1
                      and not existing_rating["needs_reconcile"]):
                    conn.execute(
                        "UPDATE rating SET anilist_score_override=?, needs_reconcile=1 WHERE anime_id=?",
                        (score, anime_id),
                    )
                    _log_sync(conn, anime_id, "score", "anilist", existing_rating["overall"], score)
                updated += 1
            else:
                title_ro = titles.get("romaji")
                if title_ro == title:
                    title_ro = None
                avg = media.get("averageScore")
                cur = conn.execute(
                    "INSERT INTO anime "
                    "(api_id,title,title_ro,cover_url,genres,year,episodes,format,synopsis,mal_score,status,anilist_id,is_airing,episodes_aired,next_episode_at,episodes_watched,rewatch_count) "
                    "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        str(mal_id) if mal_id else None,
                        title,
                        title_ro,
                        (media.get("coverImage") or {}).get("large"),
                        json.dumps(media.get("genres") or []),
                        (media.get("startDate") or {}).get("year"),
                        media.get("episodes"),
                        media.get("format"),
                        _strip_html(media.get("description")),
                        round(avg / 10, 1) if avg else None,
                        status,
                        str(al_id) if al_id else None,
                        is_airing,
                        episodes_aired,
                        next_episode_at,
                        al_progress,
                        al_repeat,
                    ),
                )
                if score is not None:
                    conn.execute(
                        "INSERT OR REPLACE INTO rating "
                        "(anime_id,story,characters,animation,audio,enjoyment,overall,updated_at) "
                        "VALUES (?,?,?,?,?,?,?,?)",
                        (
                            cur.lastrowid,
                            adv.get("story", score) if adv else score,
                            adv.get("characters", score) if adv else score,
                            adv.get("animation", score) if adv else score,
                            adv.get("audio", score) if adv else score,
                            adv.get("enjoyment", score) if adv else score,
                            score,
                            today,
                        ),
                    )
                added += 1

        linked = conn.execute(
            "SELECT id, anilist_id FROM anime WHERE anilist_id IS NOT NULL AND anilist_id != ''"
        ).fetchall()
        for row in linked:
            if row["anilist_id"] not in seen_al_ids:
                conn.execute("DELETE FROM anime WHERE id=?", (row["id"],))
                deleted += 1

    return {"added": added, "updated": updated, "deleted": deleted}


@app.post("/anilist/pull")
def anilist_pull():
    if not _get_setting("anilist_token"):
        raise HTTPException(status_code=400, detail="Not connected to AniList")
    _auto_backup()
    return _pull_anilist_library()


def _push_anilist_library() -> dict:
    token = _get_setting("anilist_token")
    if not token:
        return {"synced": 0, "skipped": 0, "errors": 0}
    with get_conn() as conn:
        rows = conn.execute("SELECT id, anilist_id FROM anime").fetchall()
    synced = skipped = errors = 0
    for row in rows:
        if not row["anilist_id"]:
            skipped += 1
            continue
        try:
            _anilist_sync(row["id"])
            synced += 1
        except Exception:
            _log.exception("push failed for anime_id=%s", row["id"])
            errors += 1
        time.sleep(0.4)
    _set_setting("anilist_last_push", date.today().isoformat())
    return {"synced": synced, "skipped": skipped, "errors": errors}


@app.post("/anilist/push")
def anilist_push():
    if not _get_setting("anilist_token"):
        raise HTTPException(status_code=400, detail="Not connected to AniList")
    return _push_anilist_library()


@app.get("/backups")
def list_backups():
    _BACKUPS_DIR.mkdir(parents=True, exist_ok=True)
    files = sorted(_BACKUPS_DIR.glob("backup_*.json"), reverse=True)
    result = []
    for f in files:
        try:
            data = json.loads(f.read_text(encoding="utf-8"))
            entry_count = len(data.get("entries", []))
        except Exception:
            entry_count = 0
        st = f.stat()
        result.append({
            "filename": f.name,
            "created_at": datetime.fromtimestamp(st.st_mtime).isoformat(),
            "entry_count": entry_count,
        })
    return {"backups": result}


@app.get("/backups/{filename}")
def download_backup_file(filename: str):
    if "/" in filename or "\\" in filename or ".." in filename:
        raise HTTPException(status_code=400, detail="Invalid filename")
    path = _BACKUPS_DIR / filename
    if not path.is_file():
        raise HTTPException(status_code=404, detail="Backup not found")
    return FileResponse(path, media_type="application/json", filename=filename)


@app.delete("/backups/{filename}", status_code=204)
def delete_backup(filename: str):
    if "/" in filename or "\\" in filename or ".." in filename:
        raise HTTPException(status_code=400, detail="Invalid filename")
    path = _BACKUPS_DIR / filename
    if not path.is_file():
        raise HTTPException(status_code=404, detail="Backup not found")
    path.unlink()


@app.get("/search")
def search(q: str = Query("", description="Title search query")):
    if not q.strip():
        return {"results": [], "q": q}
    return {"results": jikan_search(q), "q": q}


@app.post("/anime", status_code=201)
def import_anime(body: AnimeCreate, bg: BackgroundTasks):
    with get_conn() as conn:
        if body.api_id:
            existing = conn.execute(
                "SELECT id FROM anime WHERE api_id=?", (body.api_id,)
            ).fetchone()
            if existing:
                bg.add_task(_anilist_sync, existing["id"])
                return {"id": existing["id"]}
        cur = conn.execute(
            "INSERT INTO anime (api_id, title, title_ro, cover_url, genres, year, season, episodes, format, synopsis, mal_score, status) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                body.api_id,
                body.title,
                body.title_ro,
                body.cover_url,
                json.dumps(body.genres),
                body.year,
                body.season,
                body.episodes,
                body.format,
                body.synopsis,
                body.mal_score,
                body.status or "completed",
            ),
        )
    bg.add_task(_anilist_sync, cur.lastrowid)
    return {"id": cur.lastrowid}


@app.put("/anime/{anime_id}/rating")
def set_rating(anime_id: int, body: RatingIn, bg: BackgroundTasks):
    criteria = [body.story, body.characters, body.animation, body.audio, body.enjoyment]
    computed_overall = round(sum(criteria) / len(criteria), 1)
    updated_at = date.today().isoformat()

    with get_conn() as conn:
        if not conn.execute("SELECT id FROM anime WHERE id=?", (anime_id,)).fetchone():
            raise HTTPException(status_code=404, detail="Anime not found")
        conn.execute(
            "INSERT INTO rating (anime_id, story, characters, animation, audio, enjoyment, overall, updated_at) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?) "
            "ON CONFLICT(anime_id) DO UPDATE SET "
            "  story=excluded.story, characters=excluded.characters, "
            "  animation=excluded.animation, audio=excluded.audio, "
            "  enjoyment=excluded.enjoyment, overall=excluded.overall, "
            "  updated_at=excluded.updated_at",
            (anime_id, body.story, body.characters, body.animation,
             body.audio, body.enjoyment, computed_overall, updated_at),
        )
    bg.add_task(_anilist_sync, anime_id)
    return {"anime_id": anime_id, "overall": computed_overall}


@app.get("/rankings")
def rankings(
    genre:  str = Query(None),
    season: str = Query(None),
    format: str = Query(None),
    status: str = Query(None),
    sort:   str = Query("overall"),
):
    col = _SORT_COLS.get(sort, "r.overall")

    with get_conn() as conn:
        rows = conn.execute(
            "SELECT a.id, a.api_id, a.title, a.title_ro, a.cover_url, a.genres, a.year, a.season, "
            "       a.episodes, a.episodes_watched, a.episodes_aired, a.is_airing, a.next_episode_at, a.format, a.synopsis, a.mal_score, a.status, a.notes, a.rewatch_count, "
            "       r.story, r.characters, r.animation, r.audio, r.enjoyment, "
            "       r.overall, r.updated_at "
            "FROM anime a LEFT JOIN rating r ON a.id = r.anime_id "
            f"ORDER BY COALESCE({col}, 0) DESC"
        ).fetchall()

    results = []
    for row in rows:
        item = dict(row)
        item["genres"] = json.loads(item["genres"] or "[]")
        item["status"] = item.get("status") or "completed"
        item["rewatch_count"] = item.get("rewatch_count") or 0
        item["episodes_watched"] = item.get("episodes_watched") or 0
        item["is_airing"] = bool(item.get("is_airing"))
        if item.get("episodes_aired") is None:
            item["episodes_aired"] = item.get("episodes")
        if genre and genre not in item["genres"]:
            continue
        if format and item.get("format") != format:
            continue
        if season and item.get("season") != season:
            continue
        if status and item.get("status") != status:
            continue
        results.append(item)

    return {"results": results, "count": len(results)}


@app.post("/import/anilist-gdpr")
async def import_anilist_gdpr(file: UploadFile):
    raw = await file.read()
    name = (file.filename or "").lower()

    if name.endswith(".zip"):
        try:
            with zipfile.ZipFile(io.BytesIO(raw)) as zf:
                candidates = [n for n in zf.namelist()
                              if n.lower().endswith(".json")
                              and "anime" in n.lower()
                              and "manga" not in n.lower()]
                if not candidates:
                    candidates = [n for n in zf.namelist() if n.lower().endswith(".json")]
                if not candidates:
                    raise HTTPException(status_code=422, detail="No JSON file found in ZIP")
                json_bytes = zf.read(candidates[0])
        except zipfile.BadZipFile:
            raise HTTPException(status_code=422, detail="Invalid ZIP file")
        try:
            data = json.loads(json_bytes)
        except json.JSONDecodeError:
            raise HTTPException(status_code=422, detail="Invalid JSON inside ZIP")
        return {"entries": _parse_gdpr_entries(data)}

    if name.endswith(".xml"):
        return {"entries": _parse_xml_entries(raw)}

    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        raise HTTPException(status_code=422, detail="Invalid JSON")
    return {"entries": _parse_gdpr_entries(data)}


def _parse_gdpr_entries(data) -> list[dict]:
    raw_entries: list[dict] = []

    if isinstance(data, list):
        raw_entries = data
    elif isinstance(data, dict):
        # {lists: [{entries: [...]}, ...]}
        for lst in data.get("lists", []):
            raw_entries.extend(lst.get("entries", []))
        # fallback: {entries: [...]}
        if not raw_entries:
            raw_entries = data.get("entries", [])

    results = []
    scores_sample = [e.get("score", 0) for e in raw_entries if e.get("score", 0) > 0]
    use100 = any(s > 10 for s in scores_sample)

    for e in raw_entries:
        score_raw = e.get("score", 0)
        if not score_raw:
            continue

        score = score_raw / 10 if use100 else float(score_raw)
        score = round(min(10.0, max(1.0, score)), 1)

        # title resolution: flat string, or nested media.title object
        title = ""
        if isinstance(e.get("title"), str):
            title = e["title"]
        elif isinstance(e.get("media"), dict):
            t = e["media"].get("title", {})
            title = t.get("english") or t.get("romaji") or t.get("native") or ""
        elif isinstance(e.get("title"), dict):
            t = e["title"]
            title = t.get("english") or t.get("romaji") or t.get("native") or ""

        if not title:
            continue

        results.append({
            "title": title.strip(),
            "score": score,
            "status": e.get("status", ""),
        })

    seen: dict[str, dict] = {}
    for r in results:
        seen[r["title"].lower()] = r
    return list(seen.values())


def _parse_xml_entries(xml_bytes: bytes) -> list[dict]:
    try:
        root = ET.fromstring(xml_bytes)
    except ET.ParseError as exc:
        raise HTTPException(status_code=422, detail=f"Invalid XML: {exc}")

    def text(el, tag: str) -> str:
        node = el.find(tag)
        return (node.text or "").strip() if node is not None else ""

    results = []
    for anime in root.findall("anime"):
        title = text(anime, "series_title")
        score_str = text(anime, "my_score")
        if not title or not score_str:
            continue
        try:
            score = float(score_str)
        except ValueError:
            continue
        if score == 0:
            continue
        score = round(min(10.0, max(1.0, score)), 1)
        entry: dict = {"title": title, "score": score, "status": text(anime, "my_status")}
        criteria = _parse_mar_comments(text(anime, "my_comments"))
        if criteria:
            entry["criteria"] = criteria
        results.append(entry)
    seen: dict[str, dict] = {}
    for r in results:
        seen[r["title"].lower()] = r
    return list(seen.values())


_MAR_COMMENTS_RE = re.compile(
    r"Story:([\d.]+)\s+Characters:([\d.]+)\s+Animation:([\d.]+)\s+Audio:([\d.]+)\s+Enjoyment:([\d.]+)"
)


def _parse_mar_comments(comments: str) -> dict | None:
    m = _MAR_COMMENTS_RE.search(comments)
    if not m:
        return None
    keys = ("story", "characters", "animation", "audio", "enjoyment")
    try:
        return {k: round(min(10.0, max(1.0, float(v))), 1) for k, v in zip(keys, m.groups())}
    except ValueError:
        return None


@app.delete("/anime/{anime_id}", status_code=204)
def delete_anime(anime_id: int):
    with get_conn() as conn:
        if not conn.execute("SELECT id FROM anime WHERE id=?", (anime_id,)).fetchone():
            raise HTTPException(status_code=404, detail="Anime not found")
        conn.execute("DELETE FROM anime WHERE id=?", (anime_id,))


@app.get("/anime/{anime_id}/next-season")
def anime_next_season(anime_id: int):
    with get_conn() as conn:
        row = conn.execute("SELECT anilist_id FROM anime WHERE id=?", (anime_id,)).fetchone()
    if not row or not row["anilist_id"]:
        return {"found": False}
    try:
        al_id = int(row["anilist_id"])
    except (ValueError, TypeError):
        return {"found": False}

    query = """
    query($id:Int){
      Media(id:$id,type:ANIME){
        relations{ edges{ relationType node{ id idMal title{english romaji} coverImage{large} } } }
      }
    }"""
    try:
        resp = httpx.post(
            "https://graphql.anilist.co",
            json={"query": query, "variables": {"id": al_id}},
            headers={"Content-Type": "application/json", "Accept": "application/json"},
            timeout=10.0,
        )
        resp.raise_for_status()
        edges = (((resp.json().get("data") or {}).get("Media") or {}).get("relations") or {}).get("edges") or []
    except Exception:
        return {"found": False}

    def _node_info(node):
        if not node:
            return None
        titles = node.get("title") or {}
        al_id  = node["id"]
        with get_conn() as conn:
            local = conn.execute("SELECT id FROM anime WHERE anilist_id=?", (str(al_id),)).fetchone()
        return {
            "anilist_id": al_id,
            "mal_id":     node.get("idMal"),
            "local_id":   local["id"] if local else None,
            "title":      titles.get("english") or titles.get("romaji") or "",
            "cover_url":  (node.get("coverImage") or {}).get("large") or None,
        }

    sequel  = next((e["node"] for e in edges if e.get("relationType") == "SEQUEL"),  None)
    prequel = next((e["node"] for e in edges if e.get("relationType") == "PREQUEL"), None)

    result = {"found": bool(sequel or prequel),
              "sequel":  _node_info(sequel),
              "prequel": _node_info(prequel)}
    return result


@app.delete("/library", status_code=204)
def delete_library():
    with get_conn() as conn:
        conn.execute("DELETE FROM anime")


@app.get("/anime/{anime_id}/anchors")
def anchors(
    anime_id:  int,
    criterion: str = Query("story"),
):
    if criterion not in _VALID_CRITERIA:
        criterion = "story"

    with get_conn() as conn:
        target = conn.execute(
            "SELECT genres FROM anime WHERE id=?", (anime_id,)
        ).fetchone()
        if not target:
            raise HTTPException(status_code=404, detail="Anime not found")

        target_genres: list[str] = json.loads(target["genres"] or "[]")

        crit_col = _SORT_COLS.get(criterion, "r.story")
        rows = conn.execute(
            f"SELECT a.id, a.title, a.title_ro, a.cover_url, a.genres, a.year, {crit_col} AS score "
            "FROM anime a JOIN rating r ON a.id = r.anime_id "
            f"WHERE a.id != ? ORDER BY {crit_col} DESC",
            (anime_id,),
        ).fetchall()

    result = []
    for row in rows:
        item = dict(row)
        item["genres"] = json.loads(item["genres"] or "[]")
        if any(g in item["genres"] for g in target_genres):
            result.append(item)

    return {"anchors": result, "criterion": criterion, "anime_id": anime_id}


@app.patch("/anime/{anime_id}", status_code=204)
def update_anime(anime_id: int, body: AnimeUpdate, bg: BackgroundTasks):
    with get_conn() as conn:
        if not conn.execute("SELECT id FROM anime WHERE id=?", (anime_id,)).fetchone():
            raise HTTPException(status_code=404, detail="Anime not found")
        updates = body.model_dump(exclude_none=True)
        if not updates:
            return
        if "genres" in updates:
            updates["genres"] = json.dumps(updates["genres"])
        if "episodes_watched" in updates:
            row = conn.execute(
                "SELECT episodes, episodes_aired FROM anime WHERE id=?", (anime_id,)
            ).fetchone()
            cap = row["episodes_aired"] if row["episodes_aired"] is not None else row["episodes"]
            if cap is not None:
                updates["episodes_watched"] = min(updates["episodes_watched"], cap)
        cols = ", ".join(f"{k}=?" for k in updates)
        conn.execute(f"UPDATE anime SET {cols} WHERE id=?", (*updates.values(), anime_id))
    changed = body.model_dump(exclude_none=True)
    if "status" in changed or "episodes_watched" in changed or "rewatch_count" in changed:
        bg.add_task(_anilist_sync, anime_id)


@app.get("/backup")
def backup():
    with get_conn() as conn:
        rows = conn.execute(
            "SELECT a.*, r.story, r.characters, r.animation, r.audio, r.enjoyment, r.overall, r.updated_at "
            "FROM anime a LEFT JOIN rating r ON a.id = r.anime_id"
        ).fetchall()
    entries = []
    for row in rows:
        item = dict(row)
        item["genres"] = json.loads(item.get("genres") or "[]")
        entries.append(item)
    return {"version": 1, "entries": entries}


class _RestoreBody(_BaseModel):
    version: int = 1
    entries: list[dict] = []


@app.post("/restore")
def restore(body: _RestoreBody):
    _auto_backup()
    entries = body.entries
    updated_at = str(date.today())
    with get_conn() as conn:
        for e in entries:
            genres = json.dumps(e.get("genres") or [])
            cur = conn.execute(
                "INSERT OR REPLACE INTO anime "
                "(id, api_id, anilist_id, title, title_ro, cover_url, genres, year, season, episodes, format, synopsis, mal_score, status, notes, rewatch_count, episodes_watched, is_airing, episodes_aired, next_episode_at) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    e.get("id"), e.get("api_id"), e.get("anilist_id"),
                    e.get("title", ""), e.get("title_ro"),
                    e.get("cover_url"), genres, e.get("year"), e.get("season"),
                    e.get("episodes"), e.get("format"), e.get("synopsis"),
                    e.get("mal_score"), e.get("status"), e.get("notes"),
                    e.get("rewatch_count", 0), e.get("episodes_watched", 0),
                    e.get("is_airing"), e.get("episodes_aired"), e.get("next_episode_at"),
                ),
            )
            aid = e.get("id") or cur.lastrowid
            if e.get("overall") is not None:
                conn.execute(
                    "INSERT OR REPLACE INTO rating (anime_id, story, characters, animation, audio, enjoyment, overall, updated_at) "
                    "VALUES (?,?,?,?,?,?,?,?)",
                    (
                        aid, e.get("story", 5.0), e.get("characters", 5.0),
                        e.get("animation", 5.0), e.get("audio", 5.0),
                        e.get("enjoyment", 5.0), e["overall"],
                        e.get("updated_at") or updated_at,
                    ),
                )
    sync = _push_anilist_library() if _get_setting("anilist_token") else None
    return {"restored": len(entries), "sync": sync}


@app.get("/seasonal")
def seasonal():
    return {"results": jikan_seasonal()}


@app.get("/jikan/anime/{mal_id}")
def jikan_anime_by_id(mal_id: int):
    item = jikan_by_id(mal_id)
    if not item:
        raise HTTPException(status_code=404, detail="Anime not found")
    return item


_JIKAN_USER = "https://api.jikan.moe/v4/users/{}/animelist"


@app.get("/jikan/user/{username}")
def mal_user_list(username: str):
    entries = []
    page = 1
    with httpx.Client(timeout=12.0) as client:
        while page <= 10:
            try:
                resp = client.get(
                    _JIKAN_USER.format(username),
                    params={"page": page, "limit": 300, "status": "completed"},
                )
                if resp.status_code == 404:
                    raise HTTPException(status_code=404, detail="MAL user not found")
                resp.raise_for_status()
            except httpx.HTTPStatusError as exc:
                if exc.response.status_code == 404:
                    raise HTTPException(status_code=404, detail="MAL user not found")
                raise HTTPException(status_code=502, detail="Jikan API error")
            except (httpx.TimeoutException, httpx.RequestError):
                raise HTTPException(status_code=502, detail="Jikan API unreachable")

            data = resp.json().get("data", [])
            for item in data:
                media = item.get("anime") or {}
                score = item.get("score") or 0
                if not score:
                    continue
                title = (media.get("title_english") or media.get("title") or "").strip()
                title_ro = (media.get("title") or "").strip()
                api_id = str(media["mal_id"]) if media.get("mal_id") else None
                cover = (media.get("images") or {}).get("jpg", {}).get("large_image_url")
                entries.append({
                    "api_id":   api_id,
                    "title":    title,
                    "title_ro": title_ro,
                    "cover_url": cover,
                    "score":    float(score),
                    "status":   item.get("status") or "completed",
                    "genres":   [],
                })
            if len(data) < 300:
                break
            page += 1
            time.sleep(0.35)

    return {"entries": entries}


def _cdata(s: str) -> str:
    return f"<![CDATA[{(s or '').replace(']]>', ']]]]><![CDATA[>')}]]>"


_MAL_XML_STATUS = {
    "completed":  "Completed",
    "watching":   "Watching",
    "rewatching": "Watching",
    "repeating":  "Watching",
    "planning":   "Plan to Watch",
    "dropped":    "Dropped",
    "paused":     "On-Hold",
}


@app.get("/export/mal-xml")
def export_mal_xml():
    with get_conn() as conn:
        rows = conn.execute(
            "SELECT a.api_id, a.title, a.format, a.episodes, a.genres, a.status, "
            "       a.episodes_watched, a.rewatch_count, "
            "       r.story, r.characters, r.animation, r.audio, r.enjoyment, "
            "       r.overall, r.updated_at "
            "FROM anime a LEFT JOIN rating r ON a.id = r.anime_id"
        ).fetchall()

    entries = []
    excluded = 0
    counts = {"Completed": 0, "Watching": 0, "Plan to Watch": 0, "Dropped": 0, "On-Hold": 0}

    for row in rows:
        item = dict(row)
        if not item.get("api_id"):
            excluded += 1
            continue

        mal_status = _MAL_XML_STATUS.get(item.get("status") or "completed", "Completed")
        counts[mal_status] = counts.get(mal_status, 0) + 1
        score = round(item["overall"]) if item.get("overall") is not None else 0
        finish = item.get("updated_at") if mal_status == "Completed" else "0000-00-00"
        genres = ", ".join(json.loads(item.get("genres") or "[]"))
        watched = item.get("episodes_watched") or 0
        rewatched = item.get("rewatch_count") or 0
        criteria = (
            f"Story:{item['story']} Characters:{item['characters']} "
            f"Animation:{item['animation']} Audio:{item['audio']} "
            f"Enjoyment:{item['enjoyment']}"
        ) if item.get("overall") is not None else ""

        entries.append(
            f"  <anime>\n"
            f"    <series_animedb_id>{item['api_id']}</series_animedb_id>\n"
            f"    <series_title>{_cdata(item['title'])}</series_title>\n"
            f"    <series_type>{item.get('format') or ''}</series_type>\n"
            f"    <series_episodes>{item.get('episodes') or 0}</series_episodes>\n"
            f"    <my_id>0</my_id>\n"
            f"    <my_watched_episodes>{watched}</my_watched_episodes>\n"
            f"    <my_start_date>0000-00-00</my_start_date>\n"
            f"    <my_finish_date>{finish or '0000-00-00'}</my_finish_date>\n"
            f"    <my_score>{score}</my_score>\n"
            f"    <my_status>{mal_status}</my_status>\n"
            f"    <my_times_watched>{rewatched}</my_times_watched>\n"
            f"    <my_tags>{_cdata(genres)}</my_tags>\n"
            f"    <my_comments>{_cdata(criteria)}</my_comments>\n"
            f"    <update_on_import>1</update_on_import>\n"
            f"  </anime>"
        )

    total = len(entries)
    xml = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        "<myanimelist>\n"
        "  <myinfo>\n"
        "    <user_export_type>1</user_export_type>\n"
        f"    <user_total_anime>{total}</user_total_anime>\n"
        f"    <user_total_completed>{counts['Completed']}</user_total_completed>\n"
        f"    <user_total_watching>{counts['Watching']}</user_total_watching>\n"
        f"    <user_total_onhold>{counts['On-Hold']}</user_total_onhold>\n"
        f"    <user_total_dropped>{counts['Dropped']}</user_total_dropped>\n"
        f"    <user_total_plantowatch>{counts['Plan to Watch']}</user_total_plantowatch>\n"
        "  </myinfo>\n"
        + "\n".join(entries)
        + "\n</myanimelist>"
    )

    return Response(
        content=xml,
        media_type="application/xml",
        headers={
            "Content-Disposition": 'attachment; filename="myanimelist.xml"',
            "X-Entry-Count": str(total),
            "X-Excluded-Count": str(excluded),
            "Access-Control-Expose-Headers": "X-Entry-Count, X-Excluded-Count",
        },
    )


# ── Friends ────────────────────────────────────────────────────────────────

_AL_HEADERS = {"Content-Type": "application/json", "Accept": "application/json"}

_FRIEND_USER_QUERY = """
query($name:String){
  User(name:$name){
    id
    avatar{ large }
    bannerImage
    mediaListOptions{ scoreFormat }
  }
}"""

_FRIEND_LIST_QUERY = """
query($name:String){
  MediaListCollection(userName:$name,type:ANIME){
    lists{entries{score advancedScores status progress media{id averageScore genres}}}
  }
}"""


class _FriendAdd(_BaseModel):
    username: str


def _fetch_friend_list(username: str) -> tuple[int | None, str, str | None, str | None, list[dict], bool]:
    """Returns (anilist_user_id, score_format, avatar_url, banner_url, raw_entries, is_private)."""
    try:
        ur = httpx.post(
            "https://graphql.anilist.co",
            json={"query": _FRIEND_USER_QUERY, "variables": {"name": username}},
            headers=_AL_HEADERS, timeout=10.0,
        )
        ur.raise_for_status()
        user = (ur.json().get("data") or {}).get("User")
        if not user:
            return None, "POINT_10_DECIMAL", None, None, [], False
        user_id     = user["id"]
        score_format = (user.get("mediaListOptions") or {}).get("scoreFormat") or "POINT_10_DECIMAL"
        avatar_url  = (user.get("avatar") or {}).get("large") or None
        banner_url  = user.get("bannerImage") or None
    except Exception:
        return None, "POINT_10_DECIMAL", None, None, [], False

    try:
        lr = httpx.post(
            "https://graphql.anilist.co",
            json={"query": _FRIEND_LIST_QUERY, "variables": {"name": username}},
            headers=_AL_HEADERS, timeout=20.0,
        )
        lr.raise_for_status()
        data = lr.json()
        errors = data.get("errors") or []
        is_private = any("private" in (e.get("message") or "").lower() for e in errors)
        lists = ((data.get("data") or {}).get("MediaListCollection") or {}).get("lists") or []
        entries = [e for lst in lists for e in (lst.get("entries") or [])]
        if not is_private and not entries and not lists:
            # MediaListCollection returned null — list is private (AniList omits it silently)
            is_private = True
    except Exception:
        # Transient error — return None so callers know NOT to wipe existing cached data
        return user_id, score_format, avatar_url, banner_url, None, False

    return user_id, score_format, avatar_url, banner_url, entries, is_private


@app.post("/friends", status_code=201)
def add_friend(body: _FriendAdd):
    username = body.username.strip()
    if not username:
        raise HTTPException(status_code=400, detail="username required")

    user_id, score_format, avatar_url, banner_url, raw_entries, is_private = _fetch_friend_list(username)
    if user_id is None:
        raise HTTPException(status_code=404, detail="AniList user not found")
    if raw_entries is None:
        raise HTTPException(status_code=502, detail="Could not fetch anime list from AniList — try again")

    now = datetime.now().isoformat()
    with get_conn() as conn:
        existing = conn.execute(
            "SELECT id FROM friend WHERE anilist_username=?", (username,)
        ).fetchone()
        if existing:
            friend_id = existing["id"]
            conn.execute(
                "UPDATE friend SET anilist_user_id=?, score_system=?, is_private=?, last_fetched=?,"
                " avatar_url=?, banner_url=? WHERE id=?",
                (user_id, score_format, 1 if is_private else 0, now,
                 avatar_url, banner_url, friend_id),
            )
        else:
            cur = conn.execute(
                "INSERT INTO friend (anilist_username, anilist_user_id, score_system, is_private,"
                " last_fetched, avatar_url, banner_url) VALUES (?,?,?,?,?,?,?)",
                (username, user_id, score_format, 1 if is_private else 0, now,
                 avatar_url, banner_url),
            )
            friend_id = cur.lastrowid

        conn.execute("DELETE FROM friend_entry WHERE friend_id=?", (friend_id,))
        for e in raw_entries:
            media = e.get("media") or {}
            al_id = media.get("id")
            if not al_id:
                continue
            raw_score = e.get("score") or 0
            norm = normalize_score(raw_score, score_format) if raw_score > 0 else None
            avg = media.get("averageScore")
            community_score = round(avg / 10.0, 1) if avg else None
            genres = json.dumps(media.get("genres") or [])
            adv_scores = json.dumps(e.get("advancedScores") or {})
            conn.execute(
                "INSERT OR REPLACE INTO friend_entry "
                "(friend_id, anilist_id, raw_score, normalized_score, status, progress, community_score, genres, adv_scores) "
                "VALUES (?,?,?,?,?,?,?,?,?)",
                (friend_id, al_id, raw_score if raw_score > 0 else None, norm,
                 e.get("status"), e.get("progress") or 0, community_score, genres, adv_scores),
            )

    return {"id": friend_id, "is_private": is_private, "entry_count": len(raw_entries)}


@app.get("/friends")
def list_friends():
    with get_conn() as conn:
        rows = conn.execute(
            """
            SELECT f.*, COUNT(fe.id) AS entry_count
            FROM friend f
            LEFT JOIN friend_entry fe ON fe.friend_id = f.id
            GROUP BY f.id
            ORDER BY f.anilist_username
            """
        ).fetchall()
    return {"friends": [dict(r) for r in rows]}


@app.post("/friends/{friend_id}/refresh")
def refresh_friend(friend_id: int):
    with get_conn() as conn:
        friend = conn.execute("SELECT * FROM friend WHERE id=?", (friend_id,)).fetchone()
    if not friend:
        raise HTTPException(status_code=404, detail="Friend not found")

    username = friend["anilist_username"]
    user_id, score_format, avatar_url, banner_url, raw_entries, is_private = _fetch_friend_list(username)
    if user_id is None:
        raise HTTPException(status_code=502, detail="Could not reach AniList")
    if raw_entries is None:
        raise HTTPException(status_code=502, detail="Could not fetch anime list — AniList may be rate-limiting, try again")

    now = datetime.now().isoformat()
    with get_conn() as conn:
        conn.execute(
            "UPDATE friend SET score_system=?, is_private=?, last_fetched=?,"
            " avatar_url=COALESCE(?,avatar_url), banner_url=COALESCE(?,banner_url) WHERE id=?",
            (score_format, 1 if is_private else 0, now, avatar_url, banner_url, friend_id),
        )
        conn.execute("DELETE FROM friend_entry WHERE friend_id=?", (friend_id,))
        for e in raw_entries:
            media = e.get("media") or {}
            al_id = media.get("id")
            if not al_id:
                continue
            raw_score = e.get("score") or 0
            norm = normalize_score(raw_score, score_format) if raw_score > 0 else None
            avg = media.get("averageScore")
            community_score = round(avg / 10.0, 1) if avg else None
            genres = json.dumps(media.get("genres") or [])
            adv_scores = json.dumps(e.get("advancedScores") or {})
            conn.execute(
                "INSERT OR REPLACE INTO friend_entry "
                "(friend_id, anilist_id, raw_score, normalized_score, status, progress, community_score, genres, adv_scores) "
                "VALUES (?,?,?,?,?,?,?,?,?)",
                (friend_id, al_id, raw_score if raw_score > 0 else None, norm,
                 e.get("status"), e.get("progress") or 0, community_score, genres, adv_scores),
            )

    return {"is_private": is_private, "entry_count": len(raw_entries)}


@app.delete("/friends/{friend_id}", status_code=204)
def delete_friend(friend_id: int):
    with get_conn() as conn:
        if not conn.execute("SELECT id FROM friend WHERE id=?", (friend_id,)).fetchone():
            raise HTTPException(status_code=404, detail="Friend not found")
        conn.execute("DELETE FROM friend WHERE id=?", (friend_id,))


_FOLLOWING_QUERY = """
query($userId:Int!,$page:Int){
  Page(page:$page,perPage:50){
    pageInfo{ hasNextPage }
    following(userId:$userId){
      id name
      avatar{ large }
      bannerImage
    }
  }
}"""


def _sync_new_friend_entries(usernames: list[str]) -> None:
    for uname in usernames:
        time.sleep(0.5)
        try:
            with get_conn() as conn:
                friend_row = conn.execute(
                    "SELECT id, score_system FROM friend WHERE anilist_username=?", (uname,)
                ).fetchone()
            if not friend_row:
                continue
            friend_id = friend_row["id"]
            _, score_format, avatar_url, banner_url, raw_entries, is_private = _fetch_friend_list(uname)
            fetch_now = datetime.now().isoformat()
            with get_conn() as conn:
                if raw_entries is None:
                    # Transient error — update avatar/banner but leave entries untouched
                    conn.execute(
                        "UPDATE friend SET avatar_url=COALESCE(?,avatar_url),"
                        " banner_url=COALESCE(?,banner_url) WHERE id=?",
                        (avatar_url, banner_url, friend_id),
                    )
                    continue
                conn.execute(
                    "UPDATE friend SET is_private=?, last_fetched=?,"
                    " avatar_url=COALESCE(?,avatar_url), banner_url=COALESCE(?,banner_url) WHERE id=?",
                    (1 if is_private else 0, fetch_now, avatar_url, banner_url, friend_id),
                )
                conn.execute("DELETE FROM friend_entry WHERE friend_id=?", (friend_id,))
                for e in raw_entries:
                    media = e.get("media") or {}
                    al_id = media.get("id")
                    if not al_id:
                        continue
                    raw_score = e.get("score") or 0
                    norm = normalize_score(raw_score, score_format) if raw_score > 0 else None
                    avg = media.get("averageScore")
                    community_score = round(avg / 10.0, 1) if avg else None
                    genres = json.dumps(media.get("genres") or [])
                    adv_scores = json.dumps(e.get("advancedScores") or {})
                    conn.execute(
                        "INSERT OR REPLACE INTO friend_entry "
                        "(friend_id, anilist_id, raw_score, normalized_score, status, progress, community_score, genres, adv_scores) "
                        "VALUES (?,?,?,?,?,?,?,?,?)",
                        (friend_id, al_id, raw_score if raw_score > 0 else None, norm,
                         e.get("status"), e.get("progress") or 0, community_score, genres, adv_scores),
                    )
        except Exception:
            _log.exception("sync_friends: failed to fetch entries for %s", uname)


@app.post("/friends/sync-from-anilist")
def sync_friends_from_anilist(bg: BackgroundTasks):
    token = _get_setting("anilist_token")
    if not token:
        raise HTTPException(status_code=400, detail="Not connected to AniList")
    user_id_str = _get_setting("anilist_user_id")
    if not user_id_str:
        raise HTTPException(status_code=400, detail="AniList user ID not cached — re-sync first")

    try:
        user_id_int = int(user_id_str)
    except (ValueError, TypeError):
        raise HTTPException(status_code=400, detail="Cached AniList user ID is invalid — disconnect and reconnect AniList")

    headers = {**_AL_HEADERS, "Authorization": f"Bearer {token}"}
    following: list[dict] = []
    page = 1
    while page <= 10:
        resp = httpx.post(
            "https://graphql.anilist.co",
            json={"query": _FOLLOWING_QUERY, "variables": {"userId": user_id_int, "page": page}},
            headers=headers, timeout=15.0,
        )
        if resp.status_code == 401:
            raise HTTPException(status_code=401, detail="AniList token expired — please reconnect AniList")
        if not resp.is_success:
            raise HTTPException(status_code=502, detail=f"AniList API error ({resp.status_code})")
        body = resp.json()
        gql_errors = body.get("errors") or []
        if gql_errors:
            msg = (gql_errors[0].get("message") or "unknown error")
            raise HTTPException(status_code=502, detail=f"AniList GraphQL error: {msg}")
        page_data = (body.get("data") or {}).get("Page") or {}
        following.extend(page_data.get("following") or [])
        if not (page_data.get("pageInfo") or {}).get("hasNextPage"):
            break
        page += 1
        time.sleep(0.4)

    added = already_existed = 0
    now = datetime.now().isoformat()
    new_usernames: list[str] = []
    with get_conn() as conn:
        for u in following:
            uname = u.get("name")
            if not uname:
                continue
            uid        = u.get("id")
            avatar_url = (u.get("avatar") or {}).get("large") or None
            banner_url  = u.get("bannerImage") or None
            existing = conn.execute(
                "SELECT id FROM friend WHERE anilist_username=?", (uname,)
            ).fetchone()
            if existing:
                conn.execute(
                    "UPDATE friend SET anilist_user_id=?, score_system=?,"
                    " avatar_url=COALESCE(?,avatar_url), banner_url=COALESCE(?,banner_url) WHERE id=?",
                    (uid, "POINT_10_DECIMAL", avatar_url, banner_url, existing["id"]),
                )
                already_existed += 1
            else:
                conn.execute(
                    "INSERT INTO friend (anilist_username, anilist_user_id, score_system,"
                    " is_private, last_fetched, avatar_url, banner_url) VALUES (?,?,?,0,NULL,?,?)",
                    (uname, uid, "POINT_10_DECIMAL", avatar_url, banner_url),
                )
                new_usernames.append(uname)
                added += 1

    if new_usernames:
        bg.add_task(_sync_new_friend_entries, new_usernames[:15])

    return {"added": added, "already_existed": already_existed, "total": len(following)}


@app.get("/compare/{friend_id}")
def compare(friend_id: int):
    result = compare_friend(friend_id)
    if result is None:
        raise HTTPException(status_code=404, detail="Friend not found")
    with get_conn() as conn:
        row = conn.execute(
            "SELECT COUNT(*) AS cnt FROM friend_entry WHERE friend_id=?", (friend_id,)
        ).fetchone()
    result["friend_entry_count"] = row["cnt"] if row else 0
    return result


# ── LARP meter ─────────────────────────────────────────────────────────────

@app.get("/larp")
def larp(subject: str = Query("self"), friend_id: int = Query(None)):
    if subject == "friend":
        if not friend_id:
            raise HTTPException(status_code=400, detail="friend_id required for subject=friend")
        result = compute_larp_friend(friend_id)
        if result is None:
            raise HTTPException(status_code=404, detail="Friend not found")
        subject_type = "friend"
    else:
        result = compute_larp_self()
        subject_type = "self"

    if not result:
        raise HTTPException(status_code=422, detail="Not enough data to compute LARP score")

    with get_conn() as conn:
        conn.execute(
            "INSERT INTO larp_result (subject_type, friend_id, composite, components, computed_at) VALUES (?,?,?,?,?)",
            (subject_type, friend_id if subject == "friend" else None,
             result["composite"], json.dumps(result["components"]), datetime.now().isoformat()),
        )

    return result


# ── Reconciliation ─────────────────────────────────────────────────────────

@app.get("/reconcile")
def list_reconcile():
    with get_conn() as conn:
        rows = conn.execute(
            """
            SELECT a.id, a.title, a.cover_url,
                   r.overall AS local_score, r.anilist_score_override
            FROM rating r
            JOIN anime a ON a.id = r.anime_id
            WHERE r.needs_reconcile = 1
            ORDER BY a.title
            """,
        ).fetchall()
    return {"entries": [dict(r) for r in rows]}


class _ReconcileAction(_BaseModel):
    action: str  # "keep_local" | "accept_anilist"


@app.post("/reconcile/{anime_id}")
def resolve_reconcile(anime_id: int, body: _ReconcileAction):
    with get_conn() as conn:
        row = conn.execute(
            "SELECT overall, anilist_score_override FROM rating WHERE anime_id=? AND needs_reconcile=1",
            (anime_id,),
        ).fetchone()
        if not row:
            raise HTTPException(status_code=404, detail="No pending reconciliation for this entry")

        if body.action == "accept_anilist":
            override = row["anilist_score_override"]
            conn.execute(
                "UPDATE rating SET overall=?, anilist_score_override=NULL, needs_reconcile=0, updated_at=? WHERE anime_id=?",
                (override, date.today().isoformat(), anime_id),
            )
            _log_sync(conn, anime_id, "score", "reconcile:anilist", row["overall"], override)
        else:
            conn.execute(
                "UPDATE rating SET anilist_score_override=NULL, needs_reconcile=0 WHERE anime_id=?",
                (anime_id,),
            )
            _log_sync(conn, anime_id, "score", "reconcile:local", row["anilist_score_override"], row["overall"])

    return {"ok": True}


# ── Sync log ───────────────────────────────────────────────────────────────

@app.get("/sync/log")
def sync_log(limit: int = Query(100), offset: int = Query(0)):
    with get_conn() as conn:
        rows = conn.execute(
            """
            SELECT sl.id, sl.field, sl.source, sl.old_value, sl.new_value, sl.timestamp,
                   a.title, a.cover_url
            FROM sync_log sl
            LEFT JOIN anime a ON a.id = sl.anime_id
            ORDER BY sl.id DESC LIMIT ? OFFSET ?
            """,
            (limit, offset),
        ).fetchall()
        total = conn.execute("SELECT COUNT(*) FROM sync_log").fetchone()[0]
    return {"entries": [dict(r) for r in rows], "total": total}
