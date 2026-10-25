"""AniList GraphQL client -- search and by-id lookups only.

No MAL/Jikan calls anywhere in this app (PRD Section 0 / Non-Goals):
this module is the ONLY place that talks to an external metadata API.
"""
from datetime import datetime, timezone

import httpx

ANILIST_URL = "https://graphql.anilist.co"

async def _graphql(
    query: str,
    variables: dict | None = None,
    token: str | None = None,
    timeout: float = 15,
) -> dict:
    payload = {"query": query}
    if variables is not None:
        payload["variables"] = variables
    headers = {"Authorization": f"Bearer {token}"} if token else None
    async with httpx.AsyncClient(timeout=timeout) as client:
        response = await client.post(ANILIST_URL, json=payload, headers=headers)
        response.raise_for_status()
        return response.json()["data"]


_MEDIA_FIELDS = """
    id
    type
    title { romaji english native }
    coverImage { large }
    genres
    tags { name }
    studios(isMain: true) { nodes { name } }
    seasonYear
    season
    format
    episodes
    chapters
    volumes
    duration
    description(asHtml: false)
    averageScore
"""

_SEARCH_QUERY = f"""
query ($q: String, $type: MediaType) {{
  Page(page: 1, perPage: 20) {{
    media(search: $q, type: $type, sort: SEARCH_MATCH) {{
      {_MEDIA_FIELDS}
    }}
  }}
}}
"""

_BY_ID_QUERY = f"""
query ($id: Int) {{
  Media(id: $id) {{
    {_MEDIA_FIELDS}
  }}
}}
"""

_AL_TYPE = {"anime": "ANIME", "manga": "MANGA"}


def _to_media_type(al_type: str) -> str:
    return "anime" if al_type == "ANIME" else "manga"


def _map(node: dict) -> dict:
    media_type = _to_media_type(node["type"])
    title = node.get("title") or {}
    studios = ((node.get("studios") or {}).get("nodes")) or []
    tags = node.get("tags") or []
    avg = node.get("averageScore")
    return {
        "anilist_id": node["id"],
        "media_type": media_type,
        "title": title.get("romaji") or title.get("english") or title.get("native"),
        "title_native": title.get("native"),
        "title_english": title.get("english"),
        "cover_url": (node.get("coverImage") or {}).get("large"),
        "genres": node.get("genres") or [],
        "tags": [t["name"] for t in tags],
        "producers": [s["name"] for s in studios],
        "year": node.get("seasonYear"),
        "season": node.get("season"),
        "format": node.get("format"),
        "episodes": node.get("episodes"),
        "chapters": node.get("chapters"),
        "volumes": node.get("volumes"),
        "runtime_minutes": node.get("duration"),
        "synopsis": node.get("description"),
        "avg_score": (avg / 10) if avg is not None else None,
    }


async def search(query: str, media_type: str) -> list[dict]:
    variables = {"q": query, "type": _AL_TYPE[media_type]}
    data = await _graphql(_SEARCH_QUERY, variables)
    nodes = data["Page"]["media"]
    return [_map(n) for n in nodes]


async def get_by_id(anilist_id: int) -> dict | None:
    variables = {"id": anilist_id}
    data = await _graphql(_BY_ID_QUERY, variables)
    node = data.get("Media")
    return _map(node) if node else None


# --------------------------------------------------- M4: authenticated calls ----
# Everything below requires an access token (FR-22) and is the ONLY place in
# this app that writes to the user's real AniList account.

TOKEN_URL = "https://anilist.co/api/v2/oauth/token"
AUTHORIZE_URL = "https://anilist.co/api/v2/oauth/authorize"

# AniList's MediaListStatus enum <-> this app's local status strings (FR-17).
# CURRENT/REPEATING are media-type-ambiguous on AniList's side (it doesn't
# distinguish watching/reading or rewatching/rereading), so mapping AniList
# -> local needs the media_type; local -> AniList collapses both back down.
_AL_TO_LOCAL_STATUS = {
    "CURRENT": {"anime": "watching", "manga": "reading"},
    "PLANNING": {"anime": "planning", "manga": "planning"},
    "COMPLETED": {"anime": "completed", "manga": "completed"},
    "DROPPED": {"anime": "dropped", "manga": "dropped"},
    "PAUSED": {"anime": "paused", "manga": "paused"},
    "REPEATING": {"anime": "rewatching", "manga": "rereading"},
}
_LOCAL_TO_AL_STATUS = {
    "watching": "CURRENT",
    "reading": "CURRENT",
    "planning": "PLANNING",
    "completed": "COMPLETED",
    "dropped": "DROPPED",
    "paused": "PAUSED",
    "rewatching": "REPEATING",
    "rereading": "REPEATING",
}


def local_status_from_al(al_status: str, media_type: str) -> str:
    return _AL_TO_LOCAL_STATUS.get(al_status, {}).get(media_type, "planning")


def authorize_url(client_id: str, redirect_uri: str) -> str:
    return (
        f"{AUTHORIZE_URL}?client_id={client_id}"
        f"&redirect_uri={redirect_uri}&response_type=code"
    )


async def exchange_code(
    client_id: str, client_secret: str, code: str, redirect_uri: str
) -> dict:
    """FR-22. Returns AniList's token response ({access_token, expires_in, ...})."""
    async with httpx.AsyncClient(timeout=15) as client:
        resp = await client.post(
            TOKEN_URL,
            json={
                "grant_type": "authorization_code",
                "client_id": client_id,
                "client_secret": client_secret,
                "redirect_uri": redirect_uri,
                "code": code,
            },
            headers={"Accept": "application/json"},
        )
        resp.raise_for_status()
        return resp.json()


_VIEWER_QUERY = """
query {
  Viewer {
    id
    name
    mediaListOptions { scoreFormat }
  }
}
"""


async def get_viewer(access_token: str) -> dict:
    data = await _graphql(_VIEWER_QUERY, token=access_token)
    v = data["Viewer"]
    return {
        "id": v["id"],
        "name": v["name"],
        "score_format": (v.get("mediaListOptions") or {}).get("scoreFormat"),
    }


_SET_SCORE_FORMAT_MUTATION = """
mutation ($format: ScoreFormat) {
  UpdateUser(scoreFormat: $format) {
    id
    mediaListOptions { scoreFormat }
  }
}
"""


async def set_score_format(access_token: str, score_format: str = "POINT_10_DECIMAL") -> str:
    """FR-25/FR-28. Only ever called after explicit user consent -- never automatically."""
    data = await _graphql(
        _SET_SCORE_FORMAT_MUTATION,
        {"format": score_format},
        token=access_token,
    )
    return data["UpdateUser"]["mediaListOptions"]["scoreFormat"]


_LIST_COLLECTION_QUERY = f"""
query ($userId: Int, $type: MediaType) {{
  MediaListCollection(userId: $userId, type: $type) {{
    lists {{
      entries {{
        id
        status
        progress
        repeat
        score(format: POINT_10_DECIMAL)
        updatedAt
        media {{
          {_MEDIA_FIELDS}
        }}
      }}
    }}
  }}
}}
"""


async def get_media_list_collection(access_token: str, user_id: int, media_type: str) -> list[dict]:
    """FR-23. Returns one flat list of entries (AniList groups them into
    per-status 'lists', which we don't need to preserve -- flattened here)."""
    variables = {"userId": user_id, "type": _AL_TYPE[media_type]}
    data = await _graphql(
        _LIST_COLLECTION_QUERY,
        variables,
        token=access_token,
        timeout=20,
    )
    lists = data["MediaListCollection"]["lists"] or []
    entries = []
    for lst in lists:
        for e in lst["entries"]:
            media_fields = _map(e["media"])
            entries.append(
                {
                    "list_entry_id": e["id"],
                    "status": local_status_from_al(e["status"], media_type),
                    "progress": e["progress"],
                    "repeat_count": e["repeat"],
                    "score": e["score"],
                    "updated_at": (
                        datetime.fromtimestamp(e["updatedAt"], tz=timezone.utc).isoformat()
                        if e.get("updatedAt")
                        else None
                    ),
                    **media_fields,
                }
            )
    return entries


_SAVE_LIST_ENTRY_MUTATION = """
mutation ($mediaId: Int, $status: MediaListStatus, $progress: Int, $repeat: Int, $score: Float) {
  SaveMediaListEntry(mediaId: $mediaId, status: $status, progress: $progress, repeat: $repeat, score: $score) {
    id
    status
    progress
    updatedAt
  }
}
"""


async def push_list_entry(
    access_token: str,
    anilist_media_id: int,
    local_status: str,
    progress: int,
    repeat_count: int,
    overall_score: float | None,
) -> dict:
    """FR-24/FR-25. Pushes the current state of one entry. AniList interprets
    `score` according to the account's CURRENT scoreFormat -- callers must have
    already confirmed it's POINT_10_DECIMAL (FR-28) before calling this, since
    this function does no conversion math itself.

    `score` is omitted from `variables` entirely when there's no rating yet
    (FR-21: Planning/early-Watching titles routinely have none) -- AniList's
    API distinguishes "score key absent" from "score key present but null":
    sending `score: null` explicitly fails their validation ("must be a
    number") even though the field is nullable, while omitting the key
    outright succeeds. Confirmed directly against the live API."""
    variables = {
        "mediaId": anilist_media_id,
        "status": _LOCAL_TO_AL_STATUS.get(local_status, "PLANNING"),
        "progress": progress,
        "repeat": repeat_count,
    }
    if overall_score is not None:
        variables["score"] = overall_score
    data = await _graphql(
        _SAVE_LIST_ENTRY_MUTATION,
        variables,
        token=access_token,
    )
    return data["SaveMediaListEntry"]


# --------------------------------------------------------- M5: Discover ----

_DISCOVER_SORT = {
    "trending": "TRENDING_DESC",
    "current_season": "POPULARITY_DESC",
    "upcoming_season": "POPULARITY_DESC",
    "top_ranked": "SCORE_DESC",
    "top_ranked_movies": "SCORE_DESC",
    "popular": "POPULARITY_DESC",
}
_SEASONS = ("WINTER", "SPRING", "SUMMER", "FALL")


def _season_year(offset_seasons: int = 0) -> tuple[str, int]:
    now = datetime.now(timezone.utc)
    idx = (now.month - 1) // 3 + offset_seasons
    year = now.year + idx // 4
    return _SEASONS[idx % 4], year


_DISCOVER_QUERY = f"""
query ($type: MediaType, $season: MediaSeason, $seasonYear: Int, $format: MediaFormat, $sort: [MediaSort]) {{
  Page(page: 1, perPage: 20) {{
    media(type: $type, season: $season, seasonYear: $seasonYear, format: $format, sort: $sort) {{
      {_MEDIA_FIELDS}
    }}
  }}
}}
"""


async def discover(section: str, media_type: str) -> list[dict]:
    """FR-32. `season`/`seasonYear`/`format` are only included in `variables`
    when the section needs them -- same "omit rather than send null" rule
    as push_list_entry's score field."""
    variables = {"type": _AL_TYPE[media_type], "sort": [_DISCOVER_SORT.get(section, "TRENDING_DESC")]}
    if section == "current_season":
        variables["season"], variables["seasonYear"] = _season_year(0)
    elif section == "upcoming_season":
        variables["season"], variables["seasonYear"] = _season_year(1)
    elif section == "top_ranked_movies":
        variables["format"] = "MOVIE"

    data = await _graphql(_DISCOVER_QUERY, variables)
    return [_map(n) for n in data["Page"]["media"]]


# ------------------------------------------------------------- M5: Feed ----

_ACTIVITY_QUERY = """
query ($userId: Int, $isFollowing: Boolean, $page: Int) {
  Page(page: $page, perPage: 25) {
    pageInfo { hasNextPage }
    activities(userId: $userId, isFollowing: $isFollowing, sort: ID_DESC, type: MEDIA_LIST) {
      ... on ListActivity {
        status
        progress
        createdAt
        user { name }
        media { id type title { romaji english native } coverImage { large } }
      }
    }
  }
}
"""


async def feed(feed_type: str, page: int, own_user_id: int, access_token: str) -> dict:
    """FR-34/FR-35/FR-36. type=your scopes to the connected account; following
    uses AniList's own isFollowing filter; global passes neither (recent
    public activity, most-recent first).

    isFollowing is resolved relative to the authenticated viewer, so this call
    must carry the access token -- otherwise AniList has no viewer to filter
    against and isFollowing silently becomes a no-op.
    """
    variables: dict = {"page": page}
    if feed_type == "your":
        variables["userId"] = own_user_id
    elif feed_type == "following":
        variables["isFollowing"] = True

    data = await _graphql(_ACTIVITY_QUERY, variables, token=access_token)
    page_data = data["Page"]
    activities = []
    for a in page_data["activities"]:
        media = (a or {}).get("media")
        if not a or not media:
            continue  # non-ListActivity items in the union come back as {}
        title = media.get("title") or {}
        created = a.get("createdAt")
        activities.append(
            {
                "username": (a.get("user") or {}).get("name"),
                "verb": a.get("status"),
                "title": title.get("romaji") or title.get("english") or title.get("native"),
                "title_english": title.get("english"),
                "anilist_id": media.get("id"),
                "media_type": (media.get("type") or "").lower() or None,
                "cover_url": (media.get("coverImage") or {}).get("large"),
                "progress_range": a.get("progress"),
                "occurred_at": (
                    datetime.fromtimestamp(created, tz=timezone.utc).isoformat() if created else None
                ),
            }
        )
    return {"activities": activities, "has_more": page_data["pageInfo"]["hasNextPage"]}


# -------------------------------------------------------- M5: Characters ----

_CHARACTER_SEARCH_QUERY = """
query ($q: String) {
  Page(page: 1, perPage: 20) {
    characters(search: $q) {
      id
      name { full }
      image { large }
    }
  }
}
"""


async def search_characters(query: str) -> list[dict]:
    data = await _graphql(_CHARACTER_SEARCH_QUERY, {"q": query})
    return [
        {
            "anilist_id": n["id"],
            "name": n["name"]["full"],
            "image_url": (n.get("image") or {}).get("large"),
        }
        for n in data["Page"]["characters"]
    ]


_STAFF_SEARCH_QUERY = """
query ($q: String) {
  Page(page: 1, perPage: 20) {
    staff(search: $q) {
      id
      name { full }
      image { large }
    }
  }
}
"""


async def search_staff(query: str) -> list[dict]:
    data = await _graphql(_STAFF_SEARCH_QUERY, {"q": query})
    return [
        {
            "anilist_id": n["id"],
            "name": n["name"]["full"],
            "image_url": (n.get("image") or {}).get("large"),
        }
        for n in data["Page"]["staff"]
    ]


_CHARACTER_DETAIL_QUERY = """
query ($id: Int) {
  Character(id: $id) {
    id
    name { full }
    image { large }
    description(asHtml: false)
    favourites
    media(sort: START_DATE_DESC, perPage: 50) {
      edges {
        voiceActors { id name { full } languageV2 image { large } }
        node { id title { romaji english native } coverImage { large } startDate { year } }
      }
    }
  }
}
"""


async def get_character(anilist_id: int) -> dict | None:
    """FR-38. Live-fetched, never persisted."""
    data = await _graphql(_CHARACTER_DETAIL_QUERY, {"id": anilist_id})
    node = data.get("Character")
    if not node:
        return None

    voice_actors, appearances, seen = [], [], set()
    for edge in (node.get("media") or {}).get("edges", []):
        for va in edge.get("voiceActors") or []:
            if va["id"] not in seen:
                seen.add(va["id"])
                voice_actors.append(
                    {
                        "anilist_id": va["id"],
                        "name": va["name"]["full"],
                        "language": va.get("languageV2"),
                        "image_url": (va.get("image") or {}).get("large"),
                    }
                )
        m = edge.get("node") or {}
        t = m.get("title") or {}
        appearances.append(
            {
                "anilist_id": m.get("id"),
                "title": t.get("romaji") or t.get("english") or t.get("native"),
                "title_english": t.get("english"),
                "cover_url": (m.get("coverImage") or {}).get("large"),
                "year": (m.get("startDate") or {}).get("year"),
            }
        )

    return {
        "anilist_id": node["id"],
        "name": node["name"]["full"],
        "image_url": (node.get("image") or {}).get("large"),
        "bio": node.get("description"),
        "favorites_count": node.get("favourites"),
        "voice_actors": voice_actors,
        "appearances": appearances,
    }


# ------------------------------------------------------------- M5: Staff ----

_STAFF_DETAIL_QUERY = """
query ($id: Int) {
  Staff(id: $id) {
    id
    name { full }
    image { large }
    description(asHtml: false)
    favourites
    gender
    dateOfBirth { year month day }
    age
    yearsActive
    homeTown
    bloodType
    characters(sort: FAVOURITES_DESC, perPage: 50) {
      edges {
        role
        node { id name { full } image { large } }
        media { id title { romaji english native } coverImage { large } startDate { year } }
      }
    }
    staffMedia(sort: START_DATE_DESC, perPage: 50) {
      nodes { id title { romaji english native } coverImage { large } startDate { year } }
    }
  }
}
"""


async def get_staff(anilist_id: int) -> dict | None:
    data = await _graphql(_STAFF_DETAIL_QUERY, {"id": anilist_id})
    node = data.get("Staff")
    if not node:
        return None

    characters_voiced = []
    for e in (node.get("characters") or {}).get("edges", []):
        c = e.get("node") or {}
        media_list = e.get("media") or []
        primary = media_list[0] if media_list else {}
        t = primary.get("title") or {}
        characters_voiced.append(
            {
                "anilist_id": c.get("id"),
                "name": (c.get("name") or {}).get("full"),
                "image_url": (c.get("image") or {}).get("large"),
                "role": e.get("role"),
                "work_anilist_id": primary.get("id"),
                "work_title": t.get("romaji") or t.get("english") or t.get("native"),
                "work_cover_url": (primary.get("coverImage") or {}).get("large"),
                "year": (primary.get("startDate") or {}).get("year"),
                "work_anilist_ids": [m["id"] for m in media_list],
            }
        )
    works = []
    for m in (node.get("staffMedia") or {}).get("nodes", []):
        t = m.get("title") or {}
        works.append(
            {
                "anilist_id": m["id"],
                "title": t.get("romaji") or t.get("english") or t.get("native"),
                "title_english": t.get("english"),
                "cover_url": (m.get("coverImage") or {}).get("large"),
                "year": (m.get("startDate") or {}).get("year"),
            }
        )

    dob = node.get("dateOfBirth") or {}
    return {
        "anilist_id": node["id"],
        "name": node["name"]["full"],
        "image_url": (node.get("image") or {}).get("large"),
        "bio": node.get("description"),
        "favorites_count": node.get("favourites"),
        "gender": node.get("gender"),
        "birth_year": dob.get("year"),
        "birth_month": dob.get("month"),
        "birth_day": dob.get("day"),
        "age": node.get("age"),
        "years_active": node.get("yearsActive") or [],
        "home_town": node.get("homeTown"),
        "blood_type": node.get("bloodType"),
        "characters_voiced": characters_voiced,
        "works": works,
    }


# --------------------------------------------------------- M5: Favorites ----

_FAVORITE_ARG = {
    "anime": "animeId",
    "manga": "mangaId",
    "character": "characterId",
    "staff": "staffId",
    "studio": "studioId",
}
_FAVORITE_COLLECTION_KEY = {
    "anime": "anime",
    "manga": "manga",
    "character": "characters",
    "staff": "staff",
    "studio": "studios",
}


_DELETE_LIST_ENTRY_MUTATION = "mutation ($id: Int) { DeleteMediaListEntry(id: $id) { deleted } }"


async def delete_list_entry(access_token: str, list_entry_id: int) -> bool:
    """Removes an entry from the account's real AniList list entirely --
    distinct from push_list_entry, which only ever upserts."""
    data = await _graphql(
        _DELETE_LIST_ENTRY_MUTATION,
        {"id": list_entry_id},
        token=access_token,
    )
    return data["DeleteMediaListEntry"]["deleted"]


async def toggle_favorite(access_token: str, fav_type: str, anilist_id: int) -> bool:
    """FR-39. AniList's ToggleFavourite takes one of 5 mutually-exclusive id
    args depending on type, and returns the account's updated favourites
    collections -- favorited state is derived by checking membership."""
    arg = _FAVORITE_ARG[fav_type]
    query = (
        f"mutation ($id: Int) {{ ToggleFavourite({arg}: $id) {{ "
        "anime { nodes { id } } manga { nodes { id } } "
        "characters { nodes { id } } staff { nodes { id } } studios { nodes { id } } } }"
    )
    data = await _graphql(query, {"id": anilist_id}, token=access_token)
    result = data["ToggleFavourite"]
    key = _FAVORITE_COLLECTION_KEY[fav_type]
    return anilist_id in [n["id"] for n in result[key]["nodes"]]


_FAVOURITES_QUERY = """
query {
  Viewer {
    favourites {
      anime { nodes { id title { romaji english native } coverImage { large } } }
      manga { nodes { id title { romaji english native } coverImage { large } } }
      characters { nodes { id name { full } image { large } } }
      staff { nodes { id name { full } image { large } } }
      studios { nodes { id name } }
    }
  }
}
"""


async def get_favorites(access_token: str) -> dict:
    """FR-40. Live every call -- deliberately not cached, unlike Discover/Feed."""
    data = await _graphql(_FAVOURITES_QUERY, token=access_token)
    fav = data["Viewer"]["favourites"]

    def media_list(nodes):
        out = []
        for n in nodes:
            t = n.get("title") or {}
            out.append(
                {
                    "anilist_id": n["id"],
                    "title": t.get("romaji") or t.get("english") or t.get("native"),
                    "title_english": t.get("english"),
                    "cover_url": (n.get("coverImage") or {}).get("large"),
                }
            )
        return out

    def person_list(nodes):
        return [
            {"anilist_id": n["id"], "name": n["name"]["full"], "image_url": (n.get("image") or {}).get("large")}
            for n in nodes
        ]

    return {
        "anime": media_list(fav["anime"]["nodes"]),
        "manga": media_list(fav["manga"]["nodes"]),
        "characters": person_list(fav["characters"]["nodes"]),
        "staff": person_list(fav["staff"]["nodes"]),
        "studios": [{"anilist_id": n["id"], "name": n["name"]} for n in fav["studios"]["nodes"]],
    }


# --------------------------------------------------- M9: Studios & Users ----
# Studio has no image field at all on AniList's schema (confirmed via live
# introspection) -- unlike Character/Staff, its detail page is name + a
# media grid, nothing more.

_STUDIO_SEARCH_QUERY = """
query ($q: String) {
  Page(page: 1, perPage: 20) {
    studios(search: $q) {
      id
      name
    }
  }
}
"""


async def search_studios(query: str) -> list[dict]:
    data = await _graphql(_STUDIO_SEARCH_QUERY, {"q": query})
    return [{"anilist_id": n["id"], "name": n["name"]} for n in data["Page"]["studios"]]


_STUDIO_DETAIL_QUERY = """
query ($id: Int) {
  Studio(id: $id) {
    id
    name
    isAnimationStudio
    favourites
    media(sort: START_DATE_DESC, perPage: 50) {
      nodes { id title { romaji english native } coverImage { large } startDate { year } }
    }
  }
}
"""


async def get_studio(anilist_id: int) -> dict | None:
    data = await _graphql(_STUDIO_DETAIL_QUERY, {"id": anilist_id})
    node = data.get("Studio")
    if not node:
        return None
    works = []
    for m in (node.get("media") or {}).get("nodes", []):
        t = m.get("title") or {}
        works.append(
            {
                "anilist_id": m["id"],
                "title": t.get("romaji") or t.get("english") or t.get("native"),
                "title_english": t.get("english"),
                "cover_url": (m.get("coverImage") or {}).get("large"),
                "year": (m.get("startDate") or {}).get("year"),
            }
        )
    return {
        "anilist_id": node["id"],
        "name": node["name"],
        "is_animation_studio": node.get("isAnimationStudio", False),
        "favorites_count": node.get("favourites"),
        "works": works,
    }


_USER_SEARCH_QUERY = """
query ($q: String) {
  Page(page: 1, perPage: 20) {
    users(search: $q) {
      id
      name
      avatar { large }
    }
  }
}
"""


async def search_users(query: str) -> list[dict]:
    data = await _graphql(_USER_SEARCH_QUERY, {"q": query})
    return [
        {"anilist_id": n["id"], "name": n["name"], "avatar_url": (n.get("avatar") or {}).get("large")}
        for n in data["Page"]["users"]
    ]


_USER_DETAIL_QUERY = """
query ($id: Int) {
  User(id: $id) {
    id
    name
    about(asHtml: false)
    avatar { large }
    bannerImage
    siteUrl
    createdAt
    statistics {
      anime { count meanScore episodesWatched }
      manga { count meanScore chaptersRead }
    }
    favourites {
      anime { nodes { id title { romaji english native } coverImage { large } } }
      manga { nodes { id title { romaji english native } coverImage { large } } }
      characters { nodes { id name { full } image { large } } }
      staff { nodes { id name { full } image { large } } }
      studios { nodes { id name } }
    }
  }
}
"""


async def get_user(anilist_id: int) -> dict | None:
    """Public profile for an arbitrary AniList user -- id/about/avatar/stats/
    favourites all resolve for any user, not just the connected Viewer (confirmed
    via live introspection); no auth token needed for this query."""
    data = await _graphql(_USER_DETAIL_QUERY, {"id": anilist_id})
    node = data.get("User")
    if not node:
        return None

    def media_list(nodes):
        out = []
        for n in nodes:
            t = n.get("title") or {}
            out.append(
                {
                    "anilist_id": n["id"],
                    "title": t.get("romaji") or t.get("english") or t.get("native"),
                    "title_english": t.get("english"),
                    "cover_url": (n.get("coverImage") or {}).get("large"),
                }
            )
        return out

    def person_list(nodes):
        return [
            {"anilist_id": n["id"], "name": n["name"]["full"], "image_url": (n.get("image") or {}).get("large")}
            for n in nodes
        ]

    fav = node.get("favourites") or {}
    stats = node.get("statistics") or {}
    anime_stats = stats.get("anime") or {}
    manga_stats = stats.get("manga") or {}
    return {
        "anilist_id": node["id"],
        "name": node["name"],
        "avatar_url": (node.get("avatar") or {}).get("large"),
        "banner_url": node.get("bannerImage"),
        "about": node.get("about"),
        "site_url": node.get("siteUrl"),
        "created_at": node.get("createdAt"),
        "anime_count": anime_stats.get("count", 0),
        "anime_mean_score": anime_stats.get("meanScore", 0),
        "anime_episodes_watched": anime_stats.get("episodesWatched", 0),
        "manga_count": manga_stats.get("count", 0),
        "manga_mean_score": manga_stats.get("meanScore", 0),
        "manga_chapters_read": manga_stats.get("chaptersRead", 0),
        "favorites": {
            "anime": media_list((fav.get("anime") or {}).get("nodes", [])),
            "manga": media_list((fav.get("manga") or {}).get("nodes", [])),
            "characters": person_list((fav.get("characters") or {}).get("nodes", [])),
            "staff": person_list((fav.get("staff") or {}).get("nodes", [])),
            "studios": [{"anilist_id": n["id"], "name": n["name"]} for n in (fav.get("studios") or {}).get("nodes", [])],
        },
    }


# ------------------------------------------------------- M5: Community ----

_COMMUNITY_QUERY = """
query ($id: Int) {
  Media(id: $id) {
    relations {
      edges {
        relationType
        node { id title { romaji english native } coverImage { large } type startDate { year } }
      }
    }
    characters(sort: ROLE, perPage: 10) {
      edges { role node { id name { full } image { large } } }
    }
    recommendations(sort: RATING_DESC, perPage: 6) {
      nodes { rating mediaRecommendation { id title { romaji english native } coverImage { large } } }
    }
    popularity
    favourites
    trending
    rankings { rank type allTime }
  }
}
"""


async def get_community(anilist_id: int) -> dict | None:
    """FR-49/FR-55: queries by anilist_id only -- the caller must pass the
    local row's own stored anilist_id, never re-derive it by title, since
    AniList's id is already unambiguous per media object (an anime and a
    manga sharing a title have different ids)."""
    data = await _graphql(_COMMUNITY_QUERY, {"id": anilist_id})
    node = data.get("Media")
    if not node:
        return None

    relations = []
    for edge in (node.get("relations") or {}).get("edges", []):
        n = edge.get("node") or {}
        t = n.get("title") or {}
        relations.append(
            {
                "anilist_id": n.get("id"),
                "type": edge.get("relationType"),
                "title": t.get("romaji") or t.get("english") or t.get("native"),
                "title_english": t.get("english"),
                "cover_url": (n.get("coverImage") or {}).get("large"),
                "year": (n.get("startDate") or {}).get("year"),
                "media_type": (n.get("type") or "").lower() or None,
            }
        )

    characters = [
        {
            "anilist_id": (edge.get("node") or {}).get("id"),
            "name": ((edge.get("node") or {}).get("name") or {}).get("full"),
            "role": edge.get("role"),
            "image_url": ((edge.get("node") or {}).get("image") or {}).get("large"),
        }
        for edge in (node.get("characters") or {}).get("edges", [])
    ]

    recommended = []
    for rec in (node.get("recommendations") or {}).get("nodes", []):
        m = rec.get("mediaRecommendation")
        if not m:
            continue
        t = m.get("title") or {}
        recommended.append(
            {
                "anilist_id": m.get("id"),
                "title": t.get("romaji") or t.get("english") or t.get("native"),
                "title_english": t.get("english"),
                "cover_url": (m.get("coverImage") or {}).get("large"),
                "thumbs": rec.get("rating"),
            }
        )

    rankings = node.get("rankings") or []
    rank_rated = next((r["rank"] for r in rankings if r.get("type") == "RATED" and r.get("allTime")), None)
    rank_popular = next((r["rank"] for r in rankings if r.get("type") == "POPULAR" and r.get("allTime")), None)

    return {
        "relations": relations,
        "characters": characters,
        "recommended": recommended,
        "popularity": node.get("popularity"),
        "favourites": node.get("favourites"),
        "trending": node.get("trending"),
        "rank_rated": rank_rated,
        "rank_popular": rank_popular,
    }


# ---------------------------------------------------------- M6: profile ----

_PROFILE_STATS_QUERY = """
query ($userId: Int!) {
  Viewer {
    statistics { anime { count minutesWatched } manga { count chaptersRead } }
  }
  followersPage: Page(perPage: 1) { pageInfo { total } followers(userId: $userId) { id } }
  followingPage: Page(perPage: 1) { pageInfo { total } following(userId: $userId) { id } }
}
"""


async def get_profile_stats(access_token: str, user_id: int) -> dict:
    data = await _graphql(
        _PROFILE_STATS_QUERY,
        {"userId": user_id},
        token=access_token,
    )
    stats = data["Viewer"]["statistics"]
    minutes_watched = stats["anime"]["minutesWatched"] or 0
    return {
        "total_anime": stats["anime"]["count"],
        "total_manga": stats["manga"]["count"],
        "days_watched": round(minutes_watched / 60 / 24, 1),
        "chapters_read": stats["manga"]["chaptersRead"],
        "followers": data["followersPage"]["pageInfo"]["total"],
        "following": data["followingPage"]["pageInfo"]["total"],
    }
