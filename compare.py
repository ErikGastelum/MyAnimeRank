from __future__ import annotations

import json

from database import get_conn

# Map every AniList scoring system to internal 1.0–10.0
_SCORE_NORMALIZERS: dict = {
    "POINT_100":        lambda s: round(min(10.0, max(1.0, s / 10.0)), 1),
    "POINT_10_DECIMAL": lambda s: round(min(10.0, max(1.0, float(s))), 1),
    "POINT_10":         lambda s: round(min(10.0, max(1.0, float(int(s)))), 1),
    "POINT_5":          lambda s: round(min(10.0, max(1.0, s * 2.0)), 1),
    "POINT_3":          lambda s: round(min(10.0, max(1.0, s * 10.0 / 3.0)), 1),
}


def normalize_score(raw: float, score_format: str) -> float:
    fn = _SCORE_NORMALIZERS.get(score_format, _SCORE_NORMALIZERS["POINT_10_DECIMAL"])
    return fn(raw)


def compare_friend(friend_id: int) -> dict | None:
    with get_conn() as conn:
        friend = conn.execute("SELECT * FROM friend WHERE id=?", (friend_id,)).fetchone()
        if not friend:
            return None
        friend_dict = dict(friend)

        rows = conn.execute(
            """
            SELECT fe.anilist_id, fe.normalized_score AS friend_score,
                   fe.adv_scores AS friend_adv_scores,
                   r.overall AS my_score,
                   r.story, r.characters, r.animation, r.audio, r.enjoyment,
                   a.title, a.cover_url
            FROM friend_entry fe
            JOIN anime a ON CAST(a.anilist_id AS INTEGER) = fe.anilist_id
            JOIN rating r ON r.anime_id = a.id
            WHERE fe.friend_id = ? AND fe.normalized_score IS NOT NULL
            """,
            (friend_id,),
        ).fetchall()

    if not rows:
        return {
            "friend": friend_dict,
            "overlap_count": 0,
            "aggregate_skew": 0.0,
            "shared": [],
            "most_agreement": [],
            "most_disagreement": [],
        }

    shared = []
    for row in rows:
        delta = round(float(row["my_score"]) - float(row["friend_score"]), 1)
        friend_adv = {}
        try:
            raw_adv = json.loads(row["friend_adv_scores"] or "{}")
            # Only include categories with a non-zero score
            friend_adv = {k: v for k, v in raw_adv.items() if v}
        except Exception:
            pass
        shared.append({
            "anilist_id":        row["anilist_id"],
            "title":             row["title"],
            "cover_url":         row["cover_url"],
            "my_score":          float(row["my_score"]),
            "friend_score":      float(row["friend_score"]),
            "delta":             delta,
            "my_breakdown": {
                "story":      row["story"],
                "characters": row["characters"],
                "animation":  row["animation"],
                "audio":      row["audio"],
                "enjoyment":  row["enjoyment"],
            },
            "friend_adv_scores": friend_adv,
        })

    aggregate_skew = round(sum(e["delta"] for e in shared) / len(shared), 2)
    by_abs = sorted(shared, key=lambda e: abs(e["delta"]))

    return {
        "friend":            friend_dict,
        "overlap_count":     len(shared),
        "aggregate_skew":    aggregate_skew,
        "shared":            sorted(shared, key=lambda e: e["title"].lower()),
        "most_agreement":    by_abs[:5],
        "most_disagreement": list(reversed(by_abs[-5:])),
    }
