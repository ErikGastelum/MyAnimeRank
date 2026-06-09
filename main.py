import json
from contextlib import asynccontextmanager
from datetime import date
from pathlib import Path

from fastapi import FastAPI, HTTPException, Query
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
            "INSERT INTO anime (api_id, title, cover_url, genres, year, season, episodes, format, synopsis, mal_score) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                body.api_id,
                body.title,
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
            "SELECT a.id, a.api_id, a.title, a.cover_url, a.genres, a.year, a.season, "
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
            f"SELECT a.id, a.title, a.cover_url, a.genres, a.year, r.{criterion} AS score "
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
