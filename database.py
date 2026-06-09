import os
import sqlite3
import sys
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
"""


def get_conn() -> sqlite3.Connection:
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def init_db() -> None:
    with get_conn() as conn:
        conn.executescript(_SCHEMA)
    _migrate_db()
    seed_if_empty()


_SEED: list[tuple] = [
    # (title, api_id, cover_url, year, season, episodes, format, genres_json, synopsis,
    #  story, characters, animation, audio, enjoyment)
    ('Fullmetal Alchemist: Brotherhood', None, None, 2009, 'Spring 2009', 64, 'TV',
     '["Action","Adventure","Drama","Fantasy"]',
     "Two brothers search for a Philosopher's Stone after an alchemy experiment to bring back their mother goes horribly wrong.",
     9.4, 9.3, 8.6, 9.0, 9.5),
    ('Steins;Gate', None, None, 2011, 'Spring 2011', 24, 'TV',
     '["Sci-Fi","Thriller","Drama"]',
     'A self-proclaimed mad scientist discovers a way to send messages to the past, unraveling a conspiracy around time travel.',
     9.6, 9.0, 7.8, 8.4, 9.2),
    ('Attack on Titan', None, None, 2013, 'Spring 2013', 25, 'TV',
     '["Action","Drama","Fantasy","Mystery"]',
     'Humanity fights for survival against giant humanoid Titans behind enormous walls.',
     9.0, 8.5, 8.8, 9.3, 9.1),
    ('Cowboy Bebop', None, None, 1998, 'Spring 1998', 26, 'TV',
     '["Action","Sci-Fi","Adventure","Drama"]',
     'A ragtag crew of bounty hunters chases criminals across the solar system while running from their pasts.',
     8.6, 9.1, 8.4, 9.7, 9.0),
    ('Demon Slayer', None, None, 2019, 'Spring 2019', 26, 'TV',
     '["Action","Supernatural","Fantasy"]',
     'A kind-hearted boy becomes a demon slayer after his family is slaughtered and his sister turned into a demon.',
     7.8, 7.9, 9.6, 9.0, 8.7),
    ('Naruto', None, None, 2002, 'Fall 2002', 220, 'TV',
     '["Action","Adventure","Martial Arts"]',
     "An orphaned ninja with a sealed beast inside him dreams of becoming his village's leader.",
     8.0, 8.2, 7.0, 7.6, 8.4),
    ('Death Note', None, None, 2006, 'Fall 2006', 37, 'TV',
     '["Mystery","Psychological","Supernatural","Thriller"]',
     'A gifted student finds a notebook that kills anyone whose name is written in it, and a cat-and-mouse game begins.',
     8.9, 8.8, 7.9, 8.6, 9.0),
    ('Vinland Saga', None, None, 2019, 'Summer 2019', 24, 'TV',
     '["Action","Adventure","Drama"]',
     'A young Viking seeks revenge against the mercenary who killed his father, amid the brutal wars of 11th-century England.',
     8.8, 8.7, 8.5, 8.3, 8.6),
    ('Mob Psycho 100', None, None, 2016, 'Summer 2016', 12, 'TV',
     '["Action","Comedy","Supernatural","Slice of Life"]',
     'A powerful young psychic tries to live an ordinary life while keeping his overwhelming emotions in check.',
     8.2, 8.6, 9.4, 8.1, 8.9),
    ('Your Name', None, None, 2016, 'Summer 2016', 1, 'Movie',
     '["Romance","Drama","Supernatural"]',
     'Two teenagers discover they are mysteriously swapping bodies, and set out to find one another.',
     8.7, 8.0, 9.5, 9.2, 9.0),
    ("Frieren: Beyond Journey's End", None, None, 2023, 'Fall 2023', 28, 'TV',
     '["Adventure","Drama","Fantasy"]',
     'An elven mage who outlives her hero companions sets out on a journey to understand humanity and memory.',
     9.1, 9.2, 9.0, 8.8, 9.0),
    ('Spy x Family', None, None, 2022, 'Spring 2022', 25, 'TV',
     '["Action","Comedy","Slice of Life"]',
     "A spy, an assassin, and a telepath form a fake family — none of them aware of the others' secrets.",
     7.6, 8.4, 8.5, 8.0, 8.8),
]


def seed_if_empty() -> None:
    with get_conn() as conn:
        if conn.execute("SELECT COUNT(*) FROM anime").fetchone()[0] > 0:
            return
        updated_at = "2026-05-30"
        for row in _SEED:
            title, api_id, cover_url, year, season, episodes, fmt, genres, synopsis, \
                story, characters, animation, audio, enjoyment = row
            overall = round((story + characters + animation + audio + enjoyment) / 5, 1)
            cur = conn.execute(
                "INSERT INTO anime (title, api_id, cover_url, year, season, episodes, format, genres, synopsis) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (title, api_id, cover_url, year, season, episodes, fmt, genres, synopsis),
            )
            conn.execute(
                "INSERT INTO rating (anime_id, story, characters, animation, audio, enjoyment, overall, updated_at) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                (cur.lastrowid, story, characters, animation, audio, enjoyment, overall, updated_at),
            )


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
    ]
    with get_conn() as conn:
        for table, col, col_type in additions:
            existing = {r[1] for r in conn.execute(f"PRAGMA table_info({table})").fetchall()}
            if col not in existing:
                conn.execute(f"ALTER TABLE {table} ADD COLUMN {col} {col_type}")
