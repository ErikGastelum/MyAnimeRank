from __future__ import annotations

import json
import math

from database import get_conn

# Fixed weights — documented in-app; not user-tunable in v3
WEIGHTS = {
    "concurrent_watching": 0.30,
    "score_inflation":     0.25,
    "score_spread":        0.20,
    "breadth":             0.15,
    "backlog":             0.10,
}

WEIGHT_DESCRIPTIONS = {
    "concurrent_watching": "Non-seasonal shows sitting in Watching (>3 = LARP territory)",
    "score_inflation":     "Your average vs AniList community average",
    "score_spread":        "How flat/uniform your scores are",
    "breadth":             "Genre diversity in your rated titles",
    "backlog":             "Huge Planning list that never converts",
}


def _std_dev(values: list[float]) -> float:
    if len(values) < 2:
        return 0.0
    mean = sum(values) / len(values)
    return math.sqrt(sum((v - mean) ** 2 for v in values) / len(values))


def _compute_components(entries: list[dict]) -> dict:
    """
    entries: list of dicts with keys:
      score (float|None), status (str), progress (int),
      episodes (int|None), community_score (float|None, already 0-10),
      genres (list[str]), is_airing (bool|int), episodes_aired (int|None)
    Returns composite 0-100 (higher = more LARP) with component breakdown.
    """
    if not entries:
        return {}
    rated = [e for e in entries if e.get("score") is not None]

    # 1. Concurrent watching — shows in "watching" that are NOT seasonal and NOT unreleased.
    #    Seasonal = is_airing is truthy.
    #    Unreleased = episodes_aired is 0 (no episodes have aired yet).
    #    Threshold: ≤3 → 0 LARP; 15+ → 100 LARP (linear between).
    stale_watching = [
        e for e in entries
        if (e.get("status") or "").lower() in ("watching", "current", "rewatching", "repeating")
        and not e.get("is_airing")
        and (e.get("episodes_aired") is None or (e.get("episodes_aired") or 0) > 0)
    ]
    watching_count = len(stale_watching)
    concurrent_component = round(
        min(100.0, max(0.0, (watching_count - 3) / 12.0 * 100.0)), 1
    )

    # 2. Score inflation vs community average (community_score already 0-10)
    pairs = [
        (e["score"], float(e["community_score"]))
        for e in rated
        if e.get("community_score") is not None and e["community_score"] > 0
    ]
    if pairs:
        user_mean = sum(p[0] for p in pairs) / len(pairs)
        comm_mean = sum(p[1] for p in pairs) / len(pairs)
        # 0 delta = 50, +2 above community = 100, -2 below = 0
        inflation_component: float | None = round(
            min(100.0, max(0.0, 50.0 + (user_mean - comm_mean) * 25.0)), 1
        )
        inflation_available = True
    else:
        inflation_component = None
        inflation_available = False

    # 3. Score spread (std dev): low spread = high LARP; None when no scored entries
    spread_component: float | None
    if rated:
        sd = _std_dev([e["score"] for e in rated])
        spread_component = round(min(100.0, max(0.0, 100.0 * (1.0 - sd / 3.0))), 1)
    else:
        spread_component = None

    # 4. Genre breadth: use all entries (scored or not) so friends without scores still get a reading
    unique_genres: set[str] = set()
    for e in entries:
        unique_genres.update(e.get("genres") or [])
    # 15+ unique genres → 0 LARP; 0 → 100 LARP
    breadth_component = round(
        min(100.0, max(0.0, 100.0 * (1.0 - len(unique_genres) / 15.0))), 1
    )

    # 5. Backlog honesty: large unconverted Planning list = mild LARP
    completed_count = sum(
        1 for e in entries
        if (e.get("status") or "").lower() in ("completed", "rewatching", "repeating")
    )
    planning_count = sum(
        1 for e in entries
        if (e.get("status") or "").lower() in ("planning", "plan_to_watch")
    )
    # ratio 5:1 planning:completed → 100 LARP
    backlog_component = round(
        min(100.0, (planning_count / max(1, completed_count)) / 5.0 * 100.0), 1
    )

    # Composite: redistribute weight for any unavailable components
    skip = set()
    if not inflation_available:
        skip.add("score_inflation")
    if spread_component is None:
        skip.add("score_spread")
    if skip:
        avail = {k: v for k, v in WEIGHTS.items() if k not in skip}
        total = sum(avail.values())
        adjusted = {k: v / total for k, v in avail.items()}
        component_values = {
            "concurrent_watching": concurrent_component,
            "score_spread":        spread_component,
            "breadth":             breadth_component,
            "backlog":             backlog_component,
        }
        if inflation_available:
            component_values["score_inflation"] = inflation_component
        composite = sum(
            component_values[k] * adjusted[k]
            for k in adjusted
            if component_values.get(k) is not None
        )
    else:
        composite = (
            concurrent_component * WEIGHTS["concurrent_watching"] +
            inflation_component  * WEIGHTS["score_inflation"] +
            spread_component     * WEIGHTS["score_spread"] +
            breadth_component    * WEIGHTS["breadth"] +
            backlog_component    * WEIGHTS["backlog"]
        )

    return {
        "composite": round(min(100.0, max(0.0, composite)), 1),
        "components": {
            "concurrent_watching": concurrent_component,
            "score_inflation":     inflation_component,
            "score_spread":        spread_component,
            "breadth":             breadth_component,
            "backlog":             backlog_component,
        },
        "weights":             WEIGHTS,
        "descriptions":        WEIGHT_DESCRIPTIONS,
        "inflation_available": inflation_available,
        "watching_count":      watching_count,
        "rated_count":         len(rated),
        "entry_count":         len(entries),
    }


def compute_larp_self() -> dict:
    with get_conn() as conn:
        rows = conn.execute(
            """
            SELECT r.overall AS score, a.status,
                   a.episodes_watched AS progress, a.episodes,
                   a.mal_score AS community_score, a.genres,
                   a.is_airing, a.episodes_aired
            FROM rating r
            JOIN anime a ON a.id = r.anime_id
            """,
        ).fetchall()

    entries = [
        {
            "score":           row["score"],
            "status":          row["status"],
            "progress":        row["progress"] or 0,
            "episodes":        row["episodes"],
            "community_score": row["community_score"],
            "genres":          json.loads(row["genres"] or "[]"),
            "is_airing":       bool(row["is_airing"]),
            "episodes_aired":  row["episodes_aired"],
        }
        for row in rows
    ]

    # Also include unrated entries for status-based components (watching, planning counts)
    with get_conn() as conn:
        unrated = conn.execute(
            """
            SELECT a.status, a.is_airing, a.episodes_aired
            FROM anime a
            LEFT JOIN rating r ON r.anime_id = a.id
            WHERE r.anime_id IS NULL
            """,
        ).fetchall()

    for row in unrated:
        entries.append({
            "score":          None,
            "status":         row["status"],
            "progress":       0,
            "episodes":       None,
            "community_score": None,
            "genres":         [],
            "is_airing":      bool(row["is_airing"]),
            "episodes_aired": row["episodes_aired"],
        })

    return _compute_components(entries)


def compute_larp_friend(friend_id: int) -> dict | None:
    with get_conn() as conn:
        friend = conn.execute("SELECT * FROM friend WHERE id=?", (friend_id,)).fetchone()
        if not friend:
            return None
        rows = conn.execute(
            """
            SELECT fe.normalized_score AS score, fe.status, fe.progress,
                   a.episodes,
                   COALESCE(fe.community_score, a.mal_score) AS community_score,
                   COALESCE(fe.genres, a.genres)             AS genres,
                   a.is_airing, a.episodes_aired
            FROM friend_entry fe
            LEFT JOIN anime a ON CAST(a.anilist_id AS INTEGER) = fe.anilist_id
            WHERE fe.friend_id = ?
            """,
            (friend_id,),
        ).fetchall()

    entries = [
        {
            "score":           row["score"],
            "status":          row["status"],
            "progress":        row["progress"] or 0,
            "episodes":        row["episodes"],
            "community_score": row["community_score"],
            "genres":          json.loads(row["genres"] or "[]") if row["genres"] else [],
            "is_airing":       bool(row["is_airing"]) if row["is_airing"] is not None else False,
            "episodes_aired":  row["episodes_aired"],
        }
        for row in rows
    ]
    result = _compute_components(entries)
    if result:
        result["friend_id"]       = friend_id
        result["friend_username"] = dict(friend)["anilist_username"]
    return result
