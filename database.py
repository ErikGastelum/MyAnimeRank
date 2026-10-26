"""SQLite schema and data access for MyAnimeRank (PRD Section 5).

One `media` table for both anime and manga, distinguished by `media_type` --
set once at import time, never inferred or defaulted. `rating` is a separate
table so a media row can exist with no rating yet (Planning / early Watching).
`anilist_auth` is a single row (id=1) holding the OAuth session written by
oauth_listener.py; main.require_auth() gates every non-auth route on it.
"""
import json
import logging
import math
import sqlite3
from datetime import UTC, datetime
from pathlib import Path

DB_PATH = Path(__file__).parent / "library.db"

log = logging.getLogger(__name__)

_JSON_FIELDS = ("genres", "tags", "producers")


def get_conn() -> sqlite3.Connection:
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def init_db() -> None:
    with get_conn() as conn:
        conn.executescript(
            """
            CREATE TABLE IF NOT EXISTS media (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                anilist_id INTEGER UNIQUE NOT NULL,
                media_type TEXT NOT NULL CHECK (media_type IN ('anime','manga')),
                title TEXT,
                title_native TEXT,
                title_english TEXT,
                cover_url TEXT,
                genres TEXT,
                tags TEXT,
                producers TEXT,
                runtime_minutes INTEGER,
                year INTEGER,
                season TEXT,
                format TEXT,
                episodes INTEGER,
                chapters INTEGER,
                volumes INTEGER,
                synopsis TEXT,
                avg_score REAL,
                status TEXT NOT NULL DEFAULT 'planning',
                progress INTEGER NOT NULL DEFAULT 0,
                repeat_count INTEGER NOT NULL DEFAULT 0,
                anilist_list_entry_id INTEGER,
                updated_at TEXT,
                last_synced_at TEXT,
                pending_conflict INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS rating (
                media_id INTEGER PRIMARY KEY REFERENCES media(id),
                story REAL NOT NULL,
                characters REAL NOT NULL,
                animation REAL,
                audio REAL NOT NULL DEFAULT 0.0,
                enjoyment REAL NOT NULL,
                overall REAL NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS anilist_auth (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                access_token TEXT,
                token_expires_at TEXT,
                anilist_user_id INTEGER,
                anilist_username TEXT,
                score_format TEXT
            );
            """
        )
        _migrate_existing_media_columns(conn)
    log.info("database initialized at %s", DB_PATH)


def _migrate_existing_media_columns(conn: sqlite3.Connection) -> None:
    """CREATE TABLE IF NOT EXISTS is a no-op against an already-existing table,
    so a library.db created before M4 (e.g. during M2/M3 testing) won't pick up
    the new columns from the statement above -- add them here, idempotently."""
    existing_cols = {row["name"] for row in conn.execute("PRAGMA table_info(media)")}
    for col in ("updated_at", "last_synced_at"):
        if col not in existing_cols:
            conn.execute(f"ALTER TABLE media ADD COLUMN {col} TEXT")
            log.info("migrated: added media.%s", col)
    if "pending_conflict" not in existing_cols:
        conn.execute(
            "ALTER TABLE media ADD COLUMN pending_conflict INTEGER NOT NULL DEFAULT 0"
        )
        log.info("migrated: added media.pending_conflict")


def _row_to_media_dict(row: sqlite3.Row) -> dict:
    d = dict(row)
    for f in _JSON_FIELDS:
        d[f] = json.loads(d[f]) if d.get(f) else []
    return d


def _row_to_rating_dict(row: sqlite3.Row | None) -> dict | None:
    return dict(row) if row else None


def insert_or_update_media(data: dict) -> int:
    """FR-4: same anilist_id updates the existing row instead of duplicating."""
    payload = dict(data)
    for f in _JSON_FIELDS:
        payload[f] = json.dumps(payload.get(f) or [])

    with get_conn() as conn:
        existing = conn.execute(
            "SELECT id FROM media WHERE anilist_id = ?", (payload["anilist_id"],)
        ).fetchone()
        if existing:
            media_id = existing["id"]
            # media_type is set once at import and never changed afterwards
            cols = [k for k in payload if k not in ("anilist_id", "media_type")]
            set_clause = ", ".join(f"{c} = :{c}" for c in cols)
            conn.execute(
                f"UPDATE media SET {set_clause} WHERE anilist_id = :anilist_id",
                payload,
            )
            return media_id

        cols = list(payload.keys())
        placeholders = ", ".join(f":{c}" for c in cols)
        cur = conn.execute(
            f"INSERT INTO media ({', '.join(cols)}) VALUES ({placeholders})",
            payload,
        )
        return cur.lastrowid


def get_media_by_id(media_id: int) -> dict | None:
    with get_conn() as conn:
        row = conn.execute("SELECT * FROM media WHERE id = ?", (media_id,)).fetchone()
        if not row:
            return None
        result = _row_to_media_dict(row)
        rating_row = conn.execute(
            "SELECT * FROM rating WHERE media_id = ?", (media_id,)
        ).fetchone()
        result["rating"] = _row_to_rating_dict(rating_row)
        return result


def get_media_by_anilist_id(anilist_id: int) -> dict | None:
    with get_conn() as conn:
        row = conn.execute(
            "SELECT * FROM media WHERE anilist_id = ?", (anilist_id,)
        ).fetchone()
        if not row:
            return None
        result = _row_to_media_dict(row)
        rating_row = conn.execute(
            "SELECT * FROM rating WHERE media_id = ?", (row["id"],)
        ).fetchone()
        result["rating"] = _row_to_rating_dict(rating_row)
        return result


# ---------------------------------------------------------------- M3 ----

ANIME_CRITERIA = ("story", "characters", "animation", "audio", "enjoyment")
MANGA_CRITERIA = ("story", "characters", "animation", "enjoyment")  # audio excluded
_ANCHOR_CRITERIA = frozenset(ANIME_CRITERIA)  # union of both -- valid criterion column names


def compute_overall(media_type: str, scores: dict) -> float:
    """Flat, deliberately UNWEIGHTED average. An earlier Claude Design mock had
    a 1.5x enjoyment multiplier that had to be caught and reverted -- do not
    reintroduce per-criterion weights here. Mirrors the frontend's computeOverall()."""
    keys = ANIME_CRITERIA if media_type == "anime" else MANGA_CRITERIA
    return round(sum(scores.get(k) or 0 for k in keys) / len(keys), 1)


def upsert_rating(media_id: int, scores: dict) -> dict:
    """FR-21: creates the rating row if one doesn't exist yet."""
    with get_conn() as conn:
        media_row = conn.execute(
            "SELECT media_type FROM media WHERE id = ?", (media_id,)
        ).fetchone()
        if not media_row:
            return None
        media_type = media_row["media_type"]
        overall = compute_overall(media_type, scores)
        # Section 5: audio is always 0.0 for manga, never NULL -- server does not
        # trust whatever the client sent for audio on a manga title.
        audio = scores["audio"] if media_type == "anime" else 0.0
        now = datetime.now(UTC).isoformat()
        conn.execute(
            """
            INSERT INTO rating (media_id, story, characters, animation, audio, enjoyment, overall, updated_at)
            VALUES (:media_id, :story, :characters, :animation, :audio, :enjoyment, :overall, :updated_at)
            ON CONFLICT(media_id) DO UPDATE SET
                story=:story, characters=:characters, animation=:animation,
                audio=:audio, enjoyment=:enjoyment, overall=:overall, updated_at=:updated_at
            """,
            {
                "media_id": media_id,
                "story": scores["story"],
                "characters": scores["characters"],
                "animation": scores["animation"],
                "audio": audio,
                "enjoyment": scores["enjoyment"],
                "overall": overall,
                "updated_at": now,
            },
        )
        # FR-23/FR-24: score changes also need to be pushable, so they bump the
        # same media.updated_at that status/progress changes bump.
        conn.execute("UPDATE media SET updated_at = ? WHERE id = ?", (now, media_id))
        return {"overall": overall, "media_type": media_type}


def update_status(media_id: int, status: str) -> str | None:
    with get_conn() as conn:
        row = conn.execute("SELECT id FROM media WHERE id = ?", (media_id,)).fetchone()
        if not row:
            return None
        now = datetime.now(UTC).isoformat()
        conn.execute(
            "UPDATE media SET status = ?, updated_at = ? WHERE id = ?",
            (status, now, media_id),
        )
        return status


def update_progress(media_id: int, new_progress: int) -> int | None:
    """FR-53 auto-complete + FR-57 null-total exception, ported from the
    frontend's applyProgress(): clamp only when a total exists, auto-complete
    only when there IS a total, it's actually reached, and it wasn't already there."""
    with get_conn() as conn:
        row = conn.execute(
            "SELECT media_type, episodes, chapters, status, progress, repeat_count "
            "FROM media WHERE id = ?",
            (media_id,),
        ).fetchone()
        if not row:
            return None

        total = row["episodes"] if row["media_type"] == "anime" else row["chapters"]
        old = row["progress"]
        p = max(0, new_progress)
        if total is not None:
            p = min(p, total)

        status = row["status"]
        repeat_count = row["repeat_count"]
        if total and p == total and old != total:
            if status in ("watching", "reading"):
                status = "completed"
            elif status in ("rewatching", "rereading"):
                repeat_count += 1
                status = "completed"

        now = datetime.now(UTC).isoformat()
        conn.execute(
            "UPDATE media SET progress = ?, status = ?, repeat_count = ?, updated_at = ? WHERE id = ?",
            (p, status, repeat_count, now, media_id),
        )
        return p


def update_repeat_count(media_id: int, repeat_count: int) -> int | None:
    with get_conn() as conn:
        row = conn.execute("SELECT id FROM media WHERE id = ?", (media_id,)).fetchone()
        if not row:
            return None
        now = datetime.now(UTC).isoformat()
        conn.execute(
            "UPDATE media SET repeat_count = ?, updated_at = ? WHERE id = ?",
            (repeat_count, now, media_id),
        )
        return repeat_count


def get_anchors(media_id: int, criterion: str, scope: str) -> list[dict] | None:
    """FR-8. `criterion` selects a SQL column, so it MUST be whitelisted before
    use -- never interpolate an unvalidated query param into a column position."""
    if criterion not in _ANCHOR_CRITERIA:
        raise ValueError(f"invalid criterion: {criterion!r}")

    with get_conn() as conn:
        target = conn.execute(
            "SELECT media_type, genres FROM media WHERE id = ?", (media_id,)
        ).fetchone()
        if not target:
            return None
        target_genres = set(json.loads(target["genres"] or "[]"))

        rows = conn.execute(
            f"""
            SELECT m.id, m.title, m.title_english, m.cover_url, m.genres, r.{criterion} AS score
            FROM media m JOIN rating r ON r.media_id = m.id
            WHERE m.media_type = ? AND m.id != ? AND r.{criterion} IS NOT NULL
            """,
            (target["media_type"], media_id),
        ).fetchall()

        results = []
        for row in rows:
            if scope == "genre":
                row_genres = set(json.loads(row["genres"] or "[]"))
                if not (row_genres & target_genres):
                    continue
            results.append(
                {
                    "id": row["id"],
                    "title": row["title"],
                    "title_english": row["title_english"],
                    "cover_url": row["cover_url"],
                    "score": row["score"],
                }
            )
        results.sort(key=lambda x: x["score"] or 0, reverse=True)
        return results


# ---------------------------------------------------------------- M4 ----
# anilist_auth: single row (id=1). client_secret NEVER lives here or anywhere
# in this database -- it stays in local_config.py only (Section 5).

def get_auth() -> dict | None:
    with get_conn() as conn:
        row = conn.execute("SELECT * FROM anilist_auth WHERE id = 1").fetchone()
        return dict(row) if row and row["access_token"] else None


def is_connected() -> bool:
    return get_auth() is not None


def save_auth(
    access_token: str,
    token_expires_at: str,
    anilist_user_id: int,
    anilist_username: str,
    score_format: str,
) -> None:
    with get_conn() as conn:
        conn.execute(
            """
            INSERT INTO anilist_auth (id, access_token, token_expires_at, anilist_user_id, anilist_username, score_format)
            VALUES (1, :access_token, :token_expires_at, :anilist_user_id, :anilist_username, :score_format)
            ON CONFLICT(id) DO UPDATE SET
                access_token=:access_token, token_expires_at=:token_expires_at,
                anilist_user_id=:anilist_user_id, anilist_username=:anilist_username,
                score_format=:score_format
            """,
            {
                "access_token": access_token,
                "token_expires_at": token_expires_at,
                "anilist_user_id": anilist_user_id,
                "anilist_username": anilist_username,
                "score_format": score_format,
            },
        )


def update_score_format(score_format: str) -> None:
    with get_conn() as conn:
        conn.execute(
            "UPDATE anilist_auth SET score_format = ? WHERE id = 1", (score_format,)
        )


def clear_auth() -> None:
    """FR-27: clears the stored token only. No local library data is touched."""
    with get_conn() as conn:
        conn.execute(
            "UPDATE anilist_auth SET access_token=NULL, token_expires_at=NULL, "
            "anilist_user_id=NULL, anilist_username=NULL, score_format=NULL WHERE id = 1"
        )


def set_last_synced(media_id: int, ts: str) -> None:
    with get_conn() as conn:
        conn.execute(
            "UPDATE media SET last_synced_at = ? WHERE id = ?", (ts, media_id)
        )


def set_anilist_list_entry_id(media_id: int, entry_id: int) -> None:
    with get_conn() as conn:
        conn.execute(
            "UPDATE media SET anilist_list_entry_id = ? WHERE id = ?", (entry_id, media_id)
        )


def apply_remote_entry(
    media_id: int, status: str, progress: int, repeat_count: int, score: float | None, ts: str
) -> None:
    """FR-23: AniList's value wins for this entry -- apply it locally and stamp
    last_synced_at so this entry doesn't look changed again until it actually is.

    AniList only ever carries one score, never this app's 5-criterion breakdown
    (FR-25), so a nonzero remote score overwrites local `overall` directly
    without touching stored criteria -- never fabricating a breakdown that was
    never actually rated. The resulting overall/criteria-sum mismatch is
    intentionally visible in the Detail view rather than hidden, and resolves
    itself the next time the user re-rates. If no local rating exists yet, all
    criteria seed to this one known value (never a fabricated split), since the
    NOT NULL columns need something -- snapped to the nearest 0.5 step so
    criteria stay on the rating scale, while overall keeps AniList's exact
    score so the next push doesn't rewrite it. Pull's first import of a new
    entry goes through here too. Score is applied in the same transaction
    as the status/progress/timestamp write, so last_synced_at can never be
    stamped ahead of the score actually landing -- that gap is what let a
    stale local score silently overwrite a real AniList change on the next
    automatic push.
    """
    with get_conn() as conn:
        conn.execute(
            "UPDATE media SET status = ?, progress = ?, repeat_count = ?, "
            "updated_at = ?, last_synced_at = ? WHERE id = ?",
            (status, progress, repeat_count, ts, ts, media_id),
        )
        if score:  # AniList's 0 means "no score entered", not a real rating
            existing = conn.execute("SELECT 1 FROM rating WHERE media_id = ?", (media_id,)).fetchone()
            if existing:
                conn.execute(
                    "UPDATE rating SET overall = ?, updated_at = ? WHERE media_id = ?", (score, ts, media_id)
                )
            else:
                media_row = conn.execute("SELECT media_type FROM media WHERE id = ?", (media_id,)).fetchone()
                seed = _snap_to_half_step(score)
                audio = seed if media_row["media_type"] == "anime" else 0.0
                conn.execute(
                    "INSERT INTO rating (media_id, story, characters, animation, audio, enjoyment, overall, updated_at) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    (media_id, seed, seed, seed, audio, seed, score, ts),
                )


def _snap_to_half_step(score: float) -> float:
    """Nearest valid criterion value: 1.0-10.0 in 0.5 steps, halves round up
    (7.25 -> 7.5). AniList's POINT_10_DECIMAL allows e.g. 7.3, criteria don't."""
    return min(10.0, max(1.0, math.floor(score * 2 + 0.5) / 2))


def detect_conflict(local_updated_at: str | None, remote_updated_at: str | None, last_synced_at: str | None) -> str:
    """FR-23's core comparison, kept as pure logic (no I/O) so it's directly
    unit-testable against synthetic timestamps without a real AniList account.
    Returns one of: 'none', 'local', 'remote', 'conflict'.

    A never-synced local entry (last_synced_at is None) that already has a
    remote counterpart IS treated as a conflict, deliberately: with no synced
    baseline to compare against, we can't tell which side (if either) actually
    changed, so this asks rather than silently picking a winner -- matching
    FR-23's overall "never auto-resolve when uncertain" stance. (Brand new
    AniList entries with no local row at all are a separate path in the Pull
    orchestration -- they import directly and never reach this function.)
    """
    local_changed = last_synced_at is None or (
        local_updated_at is not None and local_updated_at > last_synced_at
    )
    remote_changed = last_synced_at is None or (
        remote_updated_at is not None and remote_updated_at > last_synced_at
    )
    if local_changed and remote_changed:
        return "conflict"
    if remote_changed:
        return "remote"
    if local_changed:
        return "local"
    return "none"


# ---------------------------------------------------------------- FR-58 ----

def set_pending_conflict(media_id: int, value: bool) -> None:
    with get_conn() as conn:
        conn.execute(
            "UPDATE media SET pending_conflict = ? WHERE id = ?",
            (1 if value else 0, media_id),
        )


def get_pending_conflict_rows() -> list[dict]:
    with get_conn() as conn:
        rows = conn.execute(
            "SELECT * FROM media WHERE pending_conflict = 1"
        ).fetchall()
        return [_row_to_media_dict(r) for r in rows]


# ---------------------------------------------------------------- M6 ----

_SORT_CRITERIA = frozenset({"story", "characters", "animation", "audio", "enjoyment"})


def _attach_ratings(conn: sqlite3.Connection, rows: list[sqlite3.Row]) -> list[dict]:
    results = []
    for row in rows:
        d = _row_to_media_dict(row)
        rating_row = conn.execute(
            "SELECT * FROM rating WHERE media_id = ?", (row["id"],)
        ).fetchone()
        d["rating"] = _row_to_rating_dict(rating_row)
        results.append(d)
    return results


def _sort_key(sort: str):
    key = sort if sort in _SORT_CRITERIA else "overall"
    return lambda x: (x["rating"] or {}).get(key) or 0


def get_library(media_type: str | None, status: str | None, sort: str) -> list[dict]:
    with get_conn() as conn:
        query = "SELECT * FROM media WHERE 1=1"
        params: list = []
        if media_type:
            query += " AND media_type = ?"
            params.append(media_type)
        if status and status != "all":
            query += " AND status = ?"
            params.append(status)
        rows = conn.execute(query, params).fetchall()
        results = _attach_ratings(conn, rows)
    results.sort(key=_sort_key(sort), reverse=True)
    return results


def get_rankings(
    media_type: str | None, genre: str | None, year: int | None, sort: str, q: str | None
) -> list[dict]:
    """Only rated titles participate -- rankings are anchored-score sort,
    meaningless for an unrated entry."""
    with get_conn() as conn:
        query = "SELECT m.* FROM media m JOIN rating r ON r.media_id = m.id WHERE 1=1"
        params: list = []
        if media_type:
            query += " AND m.media_type = ?"
            params.append(media_type)
        if year:
            query += " AND m.year = ?"
            params.append(year)
        if q:
            query += " AND m.title LIKE ?"
            params.append(f"%{q}%")
        rows = conn.execute(query, params).fetchall()
        results = _attach_ratings(conn, rows)
    if genre:
        results = [r for r in results if genre in r["genres"]]
    results.sort(key=_sort_key(sort), reverse=True)
    return results


def delete_media(media_id: int) -> bool:
    with get_conn() as conn:
        conn.execute("DELETE FROM rating WHERE media_id = ?", (media_id,))
        cur = conn.execute("DELETE FROM media WHERE id = ?", (media_id,))
        return cur.rowcount > 0


# ---------------------------------------------------------------- M8 ----

def get_insights() -> dict:
    """FR-60 through FR-63. Only rated titles participate (same precondition
    as get_rankings()); everything computed here from data already local --
    no AniList call, no schema change."""
    with get_conn() as conn:
        rows = conn.execute(
            "SELECT m.* FROM media m JOIN rating r ON r.media_id = m.id"
        ).fetchall()
        rated = _attach_ratings(conn, rows)

    by_genre = _insights_by_genre(rated)
    top_criterion = {
        "anime": _insights_top_criterion(rated, "anime", ANIME_CRITERIA),
        "manga": _insights_top_criterion(rated, "manga", MANGA_CRITERIA),
    }
    community_divergence = _insights_divergence(rated)
    distribution = {
        "anime": _insights_distribution(rated, "anime"),
        "manga": _insights_distribution(rated, "manga"),
    }
    return {
        "by_genre": by_genre,
        "top_criterion": top_criterion,
        "community_divergence": community_divergence,
        "distribution": distribution,
    }


def _insights_by_genre(rated: list[dict]) -> list[dict]:
    # (genre, media_type) kept as separate groups -- FR-60 never blends them,
    # even when the same genre name appears in both.
    groups: dict[tuple[str, str], list[float]] = {}
    for r in rated:
        for genre in r["genres"]:
            groups.setdefault((genre, r["media_type"]), []).append(r["rating"]["overall"])
    return [
        {"genre": genre, "media_type": mt, "avg_overall": sum(scores) / len(scores), "count": len(scores)}
        for (genre, mt), scores in groups.items()
    ]


def _insights_top_criterion(rated: list[dict], media_type: str, criteria: tuple[str, ...]) -> dict | None:
    # audio is simply absent from `criteria` for manga -- never averaged in as
    # a zero, same rule FR-9's compute_overall() already follows.
    subset = [r for r in rated if r["media_type"] == media_type]
    if not subset:
        return None
    averages = {c: sum(r["rating"][c] for r in subset) / len(subset) for c in criteria}
    top = max(averages, key=averages.get)
    return {"criterion": top, "avg_score": averages[top]}


def _insights_divergence(rated: list[dict]) -> dict:
    scored = [r for r in rated if r.get("avg_score") is not None]
    entries = [
        {
            "media_id": r["id"],
            "title": r["title"],
            "title_english": r["title_english"],
            "your_score": r["rating"]["overall"],
            "avg_score": r["avg_score"],
            "delta": r["rating"]["overall"] - r["avg_score"],
        }
        for r in scored
    ]
    avg_divergence = sum(e["delta"] for e in entries) / len(entries) if entries else 0.0
    most_above = sorted(entries, key=lambda e: e["delta"], reverse=True)[:5]
    most_below = sorted(entries, key=lambda e: e["delta"])[:5]
    return {"avg_divergence": avg_divergence, "most_above": most_above, "most_below": most_below}


def _insights_distribution(rated: list[dict], media_type: str) -> list[dict]:
    # 18 buckets, 0.5 wide, [1.0, 10.0] -- `overall` is a computed average of
    # 0.5-stepped criteria so it isn't itself naturally 0.5-aligned, this is
    # real interval bucketing, not a group-by-exact-value.
    buckets = [1.0 + 0.5 * i for i in range(18)]
    counts = {b: 0 for b in buckets}
    for r in rated:
        if r["media_type"] != media_type:
            continue
        overall = r["rating"]["overall"]
        idx = min(17, max(0, int((overall - 1.0) / 0.5)))
        counts[buckets[idx]] += 1
    return [{"bucket": b, "count": counts[b]} for b in buckets]
