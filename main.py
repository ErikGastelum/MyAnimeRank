import io
import os
import json
import re
import sys
import time
import xml.etree.ElementTree as ET
import zipfile
from contextlib import asynccontextmanager
from datetime import date
from pathlib import Path

import httpx

from fastapi import BackgroundTasks, FastAPI, HTTPException, Query, UploadFile
from fastapi.responses import FileResponse, Response
from pydantic import BaseModel as _BaseModel

from database import get_conn, init_db
from jikan import search as jikan_search, seasonal as jikan_seasonal, by_mal_id as jikan_by_id
from models import AnimeCreate, AnimeUpdate, RatingIn

# When frozen by PyInstaller, data files live in sys._MEIPASS; otherwise next to this script.
_BASE = Path(sys._MEIPASS) if getattr(sys, "frozen", False) else Path(__file__).parent
HTML_FILE = _BASE / "MyAnimeRank.html"

_VALID_CRITERIA = {"story", "characters", "animation", "audio", "enjoyment"}

ANILIST_CLIENT_ID = 43290
ANILIST_CLIENT_SECRET: str = os.environ.get("ANILIST_CLIENT_SECRET", "PLACEHOLDER_ANILIST_SECRET")

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


def _get_anilist_id(anime_id: int, token: str) -> int | None:
    with get_conn() as conn:
        row = conn.execute(
            "SELECT anilist_id, api_id FROM anime WHERE id=?", (anime_id,)
        ).fetchone()
        if not row:
            return None
        if row["anilist_id"]:
            try:
                return int(row["anilist_id"])
            except (ValueError, TypeError):
                pass
        mal_id = row["api_id"]

    if not mal_id:
        return None

    try:
        resp = httpx.post(
            "https://graphql.anilist.co",
            json={
                "query": "query($idMal:Int){Media(idMal:$idMal,type:ANIME){id}}",
                "variables": {"idMal": int(mal_id)},
            },
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
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
    return None


def _anilist_sync(anime_id: int) -> None:
    token = _get_setting("anilist_token")
    if not token:
        return

    al_id = _get_anilist_id(anime_id, token)
    if not al_id:
        return

    with get_conn() as conn:
        row = conn.execute(
            "SELECT a.status, r.story, r.characters, r.animation, r.audio, r.enjoyment, r.overall "
            "FROM anime a LEFT JOIN rating r ON a.id=r.anime_id WHERE a.id=?",
            (anime_id,),
        ).fetchone()

    if not row or row["overall"] is None:
        return

    adv_enabled = _get_setting("anilist_adv_enabled") == "1"
    variables: dict = {
        "mediaId": al_id,
        "score":   float(row["overall"]),
        "status":  _STATUS_MAP.get(row["status"] or "completed", "COMPLETED"),
    }
    if adv_enabled:
        variables["advancedScores"] = [
            float(row["story"]),
            float(row["characters"]),
            float(row["animation"]),
            float(row["audio"]),
            float(row["enjoyment"]),
        ]

    mutation = (
        "mutation($mediaId:Int,$score:Float,$status:MediaListStatus,$advancedScores:[Float]){"
        "SaveMediaListEntry(mediaId:$mediaId,score:$score,status:$status,advancedScores:$advancedScores)"
        "{id score advancedScores}}"
    )
    try:
        httpx.post(
            "https://graphql.anilist.co",
            json={"query": mutation, "variables": variables},
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
            timeout=10.0,
        ).raise_for_status()
    except Exception:
        pass


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
    try:
        vr = httpx.post(
            "https://graphql.anilist.co",
            json={"query": "query{Viewer{name advancedScoringEnabled advancedScoring}}"},
            headers={
                "Authorization": f"Bearer {access_token}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
            timeout=10.0,
        )
        vr.raise_for_status()
        viewer = vr.json().get("data", {}).get("Viewer") or {}
        username    = viewer.get("name", "")
        adv_enabled = viewer.get("advancedScoringEnabled", False)
        adv_scoring = viewer.get("advancedScoring", [])
    except Exception:
        pass
    _set_setting("anilist_token",       access_token)
    _set_setting("anilist_username",    username)
    _set_setting("anilist_adv_enabled", "1" if adv_enabled else "0")
    _set_setting("anilist_adv_scoring", json.dumps(adv_scoring))
    return {"username": username, "adv_enabled": adv_enabled, "adv_scoring": adv_scoring}


@app.get("/anilist/callback")
def anilist_callback(code: str = Query(None), error: str = Query(None)):
    if error:
        return Response(content=_callback_page(False, f"AniList error: {error}"), media_type="text/html")
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
        tr.raise_for_status()
        access_token = tr.json().get("access_token", "")
        if not access_token:
            raise ValueError("empty access_token")
    except Exception as exc:
        return Response(content=_callback_page(False, f"Token exchange failed: {exc}"), media_type="text/html")

    info = _save_anilist_token(access_token)
    return Response(content=_callback_page(True, info["username"] or "unknown"), media_type="text/html")


@app.get("/anilist/token")
def anilist_status():
    token    = _get_setting("anilist_token")
    username = _get_setting("anilist_username")
    adv_cats = json.loads(_get_setting("anilist_adv_scoring") or "[]")
    return {"connected": bool(token), "username": username or None, "adv_scoring": adv_cats}


@app.delete("/anilist/token", status_code=204)
def anilist_disconnect():
    for key in ("anilist_token", "anilist_username", "anilist_adv_enabled",
                "anilist_adv_scoring", "anilist_redirect_uri"):
        _set_setting(key, None)


@app.get("/search")
def search(q: str = Query("", description="Title search query")):
    if not q.strip():
        return {"results": [], "q": q}
    return {"results": jikan_search(q), "q": q}


@app.post("/anime", status_code=201)
def import_anime(body: AnimeCreate):
    with get_conn() as conn:
        if body.api_id:
            existing = conn.execute(
                "SELECT id FROM anime WHERE api_id=?", (body.api_id,)
            ).fetchone()
            if existing:
                return {"id": existing["id"]}
        cur = conn.execute(
            "INSERT INTO anime (api_id, title, title_ro, cover_url, genres, year, season, episodes, format, synopsis, mal_score) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
            ),
        )
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
    if sort not in _VALID_CRITERIA | {"overall"}:
        sort = "overall"
    col = f"r.{sort}" if sort != "overall" else "r.overall"

    with get_conn() as conn:
        rows = conn.execute(
            "SELECT a.id, a.api_id, a.title, a.title_ro, a.cover_url, a.genres, a.year, a.season, "
            "       a.episodes, a.format, a.synopsis, a.mal_score, a.status, a.notes, a.rewatch_count, "
            "       r.story, r.characters, r.animation, r.audio, r.enjoyment, "
            "       r.overall, r.updated_at "
            "FROM anime a JOIN rating r ON a.id = r.anime_id "
            f"ORDER BY {col} DESC"
        ).fetchall()

    results = []
    for row in rows:
        item = dict(row)
        item["genres"] = json.loads(item["genres"] or "[]")
        item["status"] = item.get("status") or "completed"
        item["rewatch_count"] = item.get("rewatch_count") or 0
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

        rows = conn.execute(
            f"SELECT a.id, a.title, a.title_ro, a.cover_url, a.genres, a.year, r.{criterion} AS score "
            "FROM anime a JOIN rating r ON a.id = r.anime_id "
            f"WHERE a.id != ? ORDER BY r.{criterion} DESC",
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
        cols = ", ".join(f"{k}=?" for k in updates)
        conn.execute(f"UPDATE anime SET {cols} WHERE id=?", (*updates.values(), anime_id))
    if "status" in body.model_dump(exclude_none=True):
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


@app.post("/restore", status_code=204)
def restore(body: dict):
    entries = body.get("entries", [])
    updated_at = str(date.today())
    with get_conn() as conn:
        for e in entries:
            genres = json.dumps(e.get("genres") or [])
            cur = conn.execute(
                "INSERT OR REPLACE INTO anime "
                "(id, api_id, title, title_ro, cover_url, genres, year, season, episodes, format, synopsis, mal_score, status, notes, rewatch_count) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    e.get("id"), e.get("api_id"), e.get("title", ""), e.get("title_ro"),
                    e.get("cover_url"), genres, e.get("year"), e.get("season"),
                    e.get("episodes"), e.get("format"), e.get("synopsis"),
                    e.get("mal_score"), e.get("status"), e.get("notes"),
                    e.get("rewatch_count", 0),
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


@app.get("/export/mal-xml")
def export_mal_xml():
    with get_conn() as conn:
        rows = conn.execute(
            "SELECT a.api_id, a.title, a.format, a.episodes, a.genres, "
            "       r.story, r.characters, r.animation, r.audio, r.enjoyment, "
            "       r.overall, r.updated_at "
            "FROM anime a JOIN rating r ON a.id = r.anime_id"
        ).fetchall()

    entries = []
    for row in rows:
        item = dict(row)
        genres = ", ".join(json.loads(item.get("genres") or "[]"))
        score = round(item["overall"]) if item["overall"] is not None else 0
        finish = item.get("updated_at") or "0000-00-00"
        criteria = (
            f"Story:{item['story']} Characters:{item['characters']} "
            f"Animation:{item['animation']} Audio:{item['audio']} "
            f"Enjoyment:{item['enjoyment']}"
        )
        entries.append(
            f"  <anime>\n"
            f"    <series_animedb_id>{item.get('api_id') or 0}</series_animedb_id>\n"
            f"    <series_title>{_cdata(item['title'])}</series_title>\n"
            f"    <series_type>{item.get('format') or ''}</series_type>\n"
            f"    <series_episodes>{item.get('episodes') or 0}</series_episodes>\n"
            f"    <my_id>0</my_id>\n"
            f"    <my_watched_episodes>{item.get('episodes') or 0}</my_watched_episodes>\n"
            f"    <my_start_date>0000-00-00</my_start_date>\n"
            f"    <my_finish_date>{finish}</my_finish_date>\n"
            f"    <my_score>{score}</my_score>\n"
            f"    <my_status>Completed</my_status>\n"
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
        f"    <user_total_completed>{total}</user_total_completed>\n"
        "    <user_total_watching>0</user_total_watching>\n"
        "    <user_total_onhold>0</user_total_onhold>\n"
        "    <user_total_dropped>0</user_total_dropped>\n"
        "    <user_total_plantowatch>0</user_total_plantowatch>\n"
        "  </myinfo>\n"
        + "\n".join(entries)
        + "\n</myanimelist>"
    )

    return Response(
        content=xml,
        media_type="application/xml",
        headers={"Content-Disposition": 'attachment; filename="myanimelist.xml"'},
    )
