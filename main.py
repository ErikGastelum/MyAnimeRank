import io
import json
import re
import xml.etree.ElementTree as ET
import zipfile
from contextlib import asynccontextmanager
from datetime import date
from pathlib import Path

from fastapi import FastAPI, HTTPException, Query, UploadFile
from fastapi.responses import FileResponse

from database import get_conn, init_db
from jikan import search as jikan_search
from models import AnimeCreate, RatingIn

HTML_FILE = Path(__file__).parent / "MyAnimeRank.html"

_VALID_CRITERIA = {"story", "characters", "animation", "audio", "enjoyment"}


@asynccontextmanager
async def lifespan(app: FastAPI):
    init_db()
    yield


app = FastAPI(title="MyAnimeRank", lifespan=lifespan)


@app.get("/")
def index():
    return FileResponse(HTML_FILE)


@app.get("/search")
def search(q: str = Query("", description="Title search query")):
    if not q.strip():
        return {"results": [], "q": q}
    return {"results": jikan_search(q), "q": q}


@app.post("/anime", status_code=201)
def import_anime(body: AnimeCreate):
    with get_conn() as conn:
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
def set_rating(anime_id: int, body: RatingIn):
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
    return {"anime_id": anime_id, "overall": computed_overall}


@app.get("/rankings")
def rankings(
    genre:  str = Query(None),
    season: str = Query(None),
    format: str = Query(None),
    sort:   str = Query("overall"),
):
    if sort not in _VALID_CRITERIA | {"overall"}:
        sort = "overall"
    col = f"r.{sort}" if sort != "overall" else "r.overall"

    with get_conn() as conn:
        rows = conn.execute(
            "SELECT a.id, a.api_id, a.title, a.title_ro, a.cover_url, a.genres, a.year, a.season, "
            "       a.episodes, a.format, a.synopsis, a.mal_score, "
            "       r.story, r.characters, r.animation, r.audio, r.enjoyment, "
            "       r.overall, r.updated_at "
            "FROM anime a JOIN rating r ON a.id = r.anime_id "
            f"ORDER BY {col} DESC"
        ).fetchall()

    results = []
    for row in rows:
        item = dict(row)
        item["genres"] = json.loads(item["genres"] or "[]")
        if genre and genre not in item["genres"]:
            continue
        if format and item.get("format") != format:
            continue
        if season and item.get("season") != season:
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

    return results


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
    return results


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
