import os
import sqlite3
import sys
from contextlib import contextmanager
from pathlib import Path

if getattr(sys, "frozen", False):
    # Frozen exe: store DB in %LOCALAPPDATA%\MyAnimeRank so it persists across runs
    _data_dir = Path(os.environ.get("LOCALAPPDATA", Path.home())) / "MyAnimeRank"
    _data_dir.mkdir(parents=True, exist_ok=True)
    DB_PATH = _data_dir / "myanimrank.db"
else:
    DB_PATH = Path(__file__).parent / "myanimrank.db"

_SCHEMA = """
CREATE TABLE IF NOT EXISTS anime (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    api_id    TEXT,
    title     TEXT NOT NULL,
    cover_url TEXT,
    genres    TEXT NOT NULL DEFAULT '[]',
    year      INTEGER,
    season    TEXT,
    episodes  INTEGER,
    format    TEXT,
    synopsis  TEXT
);

CREATE TABLE IF NOT EXISTS rating (
    anime_id   INTEGER PRIMARY KEY REFERENCES anime(id) ON DELETE CASCADE,
    story      REAL NOT NULL CHECK(story BETWEEN 1.0 AND 10.0),
    characters REAL NOT NULL CHECK(characters BETWEEN 1.0 AND 10.0),
    animation  REAL NOT NULL CHECK(animation BETWEEN 1.0 AND 10.0),
    audio      REAL NOT NULL CHECK(audio BETWEEN 1.0 AND 10.0),
    enjoyment  REAL NOT NULL CHECK(enjoyment BETWEEN 1.0 AND 10.0),
    overall    REAL NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS search_cache (
    query      TEXT PRIMARY KEY,
    results    TEXT NOT NULL,
    cached_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS friend (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    anilist_username  TEXT UNIQUE NOT NULL,
    anilist_user_id   INTEGER,
    score_system      TEXT NOT NULL DEFAULT 'POINT_10_DECIMAL',
    is_private        INTEGER NOT NULL DEFAULT 0,
    last_fetched      TEXT
);

CREATE TABLE IF NOT EXISTS friend_entry (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    friend_id        INTEGER NOT NULL REFERENCES friend(id) ON DELETE CASCADE,
    anilist_id       INTEGER NOT NULL,
    raw_score        REAL,
    normalized_score REAL,
    status           TEXT,
    progress         INTEGER NOT NULL DEFAULT 0,
    UNIQUE(friend_id, anilist_id)
);

CREATE TABLE IF NOT EXISTS larp_result (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    subject_type TEXT NOT NULL,
    friend_id    INTEGER REFERENCES friend(id) ON DELETE CASCADE,
    composite    REAL NOT NULL,
    components   TEXT NOT NULL,
    computed_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_log (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    anime_id  INTEGER REFERENCES anime(id) ON DELETE CASCADE,
    field     TEXT NOT NULL,
    source    TEXT NOT NULL,
    old_value TEXT,
    new_value TEXT,
    timestamp TEXT NOT NULL
);
"""


@contextmanager
def get_conn():
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = sqlite3.Row
    try:
        conn.execute("PRAGMA foreign_keys = ON")
        yield conn
        conn.commit()
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()


def init_db() -> None:
    with get_conn() as conn:
        conn.executescript(_SCHEMA)
    _migrate_db()
    seed_if_empty()


def seed_if_empty() -> None:
    pass


def _migrate_db() -> None:
    """Add columns introduced after the initial schema without losing data."""
    additions = [
        ("anime", "format",        "TEXT"),
        ("anime", "synopsis",      "TEXT"),
        ("anime", "mal_score",     "REAL"),
        ("anime", "title_ro",      "TEXT"),
        ("anime", "status",        "TEXT"),
        ("anime", "notes",         "TEXT"),
        ("anime", "rewatch_count", "INTEGER"),
        ("anime", "anilist_id",        "TEXT"),
        ("anime", "episodes_watched",  "INTEGER"),
        ("anime", "is_airing",         "INTEGER"),
        ("anime", "episodes_aired",    "INTEGER"),
        ("anime", "next_episode_at",   "INTEGER"),
        ("rating",  "anilist_score_override", "REAL"),
        ("rating",  "needs_reconcile",        "INTEGER"),
        ("friend",       "avatar_url",        "TEXT"),
        ("friend",       "banner_url",        "TEXT"),
        ("friend_entry", "community_score",   "REAL"),
        ("friend_entry", "genres",            "TEXT"),
        ("friend_entry", "adv_scores",        "TEXT"),
    ]
    with get_conn() as conn:
        for table, col, col_type in additions:
            existing = {r[1] for r in conn.execute(f"PRAGMA table_info({table})").fetchall()}
            if col not in existing:
                conn.execute(f"ALTER TABLE {table} ADD COLUMN {col} {col_type}")
