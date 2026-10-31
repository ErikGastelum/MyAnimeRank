//! AniList GraphQL client. Port of anilist.py: every AniList query and
//! mutation in the app goes through the single `graphql()` helper.
//!
//! No MAL/Jikan calls anywhere: this is the ONLY module that talks to an
//! external metadata API.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::db::{normalize_score, round1, FollowedUser, FriendEntry, FriendList, MediaFields};
use crate::limiter::{Lane, Limiter, Priority, MAX_PAUSE_WAIT};

pub const ANILIST_URL: &str = "https://graphql.anilist.co";
pub const AUTHORIZE_URL: &str = "https://anilist.co/api/v2/oauth/authorize";

/// HTTP 429: every request pauses for Retry-After (FR-86); a call retries at
/// most this many times (background calls never), and never waits longer
/// than MAX_PAUSE_WAIT (asking for more means give up now and let the caller
/// / push retry queue try again later).
const MAX_429_RETRIES: usize = 2;
const DEFAULT_RETRY_AFTER_SECONDS: f64 = 2.0;
/// FR-79: 50 follows a page; more pages than this is a runaway answer.
const MAX_FOLLOW_PAGES: i64 = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum AlError {
    /// Transient: unreachable, timeout, 429 or 5xx. Pushes that fail this way
    /// are queued for retry (FR-26).
    Network(String),
    /// AniList answered with a GraphQL `errors` payload. Resending won't help.
    Api { message: String, status: u16 },
    /// HTTP 404: the requested Media/MediaList/etc doesn't exist.
    NotFound(String),
    /// AniList returned something this app can't interpret (e.g. an unknown
    /// MediaType). A bug or schema change, never a guess.
    Unexpected(String),
}

impl std::fmt::Display for AlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlError::Network(m) | AlError::NotFound(m) | AlError::Unexpected(m) => f.write_str(m),
            AlError::Api { message, .. } => f.write_str(message),
        }
    }
}

impl AlError {
    pub fn is_network(&self) -> bool {
        matches!(self, AlError::Network(_))
    }
}

impl From<reqwest::Error> for AlError {
    fn from(e: reqwest::Error) -> Self {
        AlError::Network(e.to_string())
    }
}

pub type AlResult<T> = Result<T, AlError>;

/// Every clone shares one limiter (FR-86). A clone made with `at()` sends
/// at that priority; otherwise requests are interactive.
#[derive(Clone)]
pub struct AniList {
    http: reqwest::Client,
    graphql_url: String,
    limiter: Arc<Limiter>,
    lane: Lane,
}

/// One AniList MediaList entry joined with its media (Pull / conflict data).
#[derive(Debug, Clone, PartialEq)]
pub struct ListEntry {
    pub list_entry_id: i64,
    pub status: String,
    pub progress: i64,
    pub repeat_count: i64,
    pub score: Option<f64>,
    pub updated_at: Option<String>,
    pub media: MediaFields,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntryState {
    pub list_entry_id: i64,
    pub status: String,
    pub progress: i64,
    pub repeat_count: i64,
    pub score: Option<f64>,
    pub updated_at: Option<String>,
}

/// FR-73: what looking up a friend found.
#[derive(Debug, Clone, PartialEq)]
pub enum FriendLookup {
    Found(FriendList),
    NoSuchUser,
    /// AniList won't show this user's lists to anyone else. Carries the name.
    Private(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Viewer {
    pub id: i64,
    pub name: String,
    pub score_format: Option<String>,
}

impl Default for AniList {
    fn default() -> Self {
        Self::with_url(ANILIST_URL)
    }
}

impl AniList {
    pub fn with_url(graphql_url: &str) -> Self {
        Self::with_window(graphql_url, crate::limiter::WINDOW)
    }

    /// Tests shorten the limiter's window (FR-86); its budget stays the same.
    pub fn with_window(graphql_url: &str, window: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("HTTP client builds with static settings");
        AniList { http, graphql_url: graphql_url.into(), limiter: Arc::new(Limiter::new(window)), lane: Lane::new(Priority::Interactive) }
    }

    /// The same client, sending at `priority` (FR-86). Each call starts a
    /// lane of its own, so raising it touches only this piece of work.
    pub fn at(&self, priority: Priority) -> AniList {
        AniList { lane: Lane::new(priority), ..self.clone() }
    }

    pub fn lane(&self) -> &Lane {
        &self.lane
    }

    pub fn limiter(&self) -> &Limiter {
        &self.limiter
    }

    /// FR-80/FR-26: background work may take a turn of two requests (a
    /// friend fetch, a push retry): nothing else is waiting or running, no
    /// 429 in the last minute, and the background share has room.
    pub fn is_quiet(&self) -> bool {
        self.limiter.background_ready(2)
    }

    async fn graphql(&self, query: &str, variables: Option<Value>, token: Option<&str>, timeout: u64) -> AlResult<Value> {
        self.graphql_within(query, variables, token, timeout, &mut (MAX_429_RETRIES + 1)).await
    }

    /// The one path to AniList, for graphql() and every Pick an anime
    /// request: each request waits for its turn in the shared limiter
    /// (FR-86), whose budget the response headers keep honest, and a 429
    /// pauses every request. It sends at most `budget` HTTP requests, 429
    /// retries included (and never more than MAX_429_RETRIES of those, none
    /// at background priority), taking each one it sends out of `budget`.
    /// `budget` must be at least 1.
    async fn graphql_within(&self, query: &str, variables: Option<Value>, token: Option<&str>, timeout: u64, budget: &mut usize) -> AlResult<Value> {
        let mut payload = json!({ "query": query });
        if let Some(v) = variables {
            payload["variables"] = v;
        }
        let mut attempt = 0;
        loop {
            let _sending = self.limiter.acquire(&self.lane).await.map_err(|left| {
                AlError::Network(format!("AniList is rate-limiting; try again in {} s", left.as_secs().max(1)))
            })?;
            let mut req = self
                .http
                .post(&self.graphql_url)
                .timeout(Duration::from_secs(timeout))
                .json(&payload);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            *budget -= 1;
            let response = req.send().await?;
            if response.status().as_u16() != 429 {
                let header = |name: &str| response.headers().get(name)?.to_str().ok()?.trim().parse::<u64>().ok();
                let (limit, remaining) = (header("X-RateLimit-Limit"), header("X-RateLimit-Remaining"));
                self.limiter.observe(limit.map(|n| n as usize), remaining.map(|n| n as usize), header("X-RateLimit-Reset"));
                return unwrap_response(response).await;
            }
            let wait = retry_after_seconds(&response);
            self.limiter.pause(Duration::try_from_secs_f64(wait.min(3600.0)).unwrap_or(MAX_PAUSE_WAIT));
            let retries = if self.lane.get() == Priority::Background { 0 } else { MAX_429_RETRIES };
            if attempt >= retries || wait > MAX_PAUSE_WAIT.as_secs_f64() || *budget == 0 {
                return unwrap_response(response).await; // a shorter wait would only earn another 429
            }
            attempt += 1;
        }
    }

    // ------------------------------------------------------ search / media ----

    pub async fn search(&self, query: &str, media_type: &str) -> AlResult<Vec<MediaFields>> {
        let q = format!(
            "query ($q: String, $type: MediaType) {{ Page(page: 1, perPage: 20) {{
               media(search: $q, type: $type, sort: SEARCH_MATCH) {{ {MEDIA_FIELDS} }} }} }}"
        );
        let data = self.graphql(&q, Some(json!({"q": query, "type": al_type(media_type)})), None, 15).await?;
        map_nodes(&data["Page"]["media"])
    }

    pub async fn get_by_id(&self, anilist_id: i64) -> AlResult<Option<MediaFields>> {
        let q = format!("query ($id: Int) {{ Media(id: $id) {{ {MEDIA_FIELDS} }} }}");
        match self.graphql(&q, Some(json!({"id": anilist_id})), None, 15).await {
            // AniList 404s an unknown id rather than returning null.
            Err(AlError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
            Ok(data) if data["Media"].is_null() => Ok(None),
            Ok(data) => map_media(&data["Media"]).map(Some),
        }
    }

    // ---------------------------------------------------- authenticated ----
    // Everything below requires an access token and is the ONLY place in this
    // app that writes to the user's real AniList account.

    pub async fn get_viewer(&self, token: &str) -> AlResult<Viewer> {
        let q = "query { Viewer { id name mediaListOptions { scoreFormat } } }";
        let data = self.graphql(q, None, Some(token), 15).await?;
        let v = &data["Viewer"];
        Ok(Viewer {
            id: v["id"].as_i64().ok_or_else(|| unexpected("Viewer.id missing"))?,
            name: v["name"].as_str().unwrap_or_default().to_string(),
            score_format: str_of(&v["mediaListOptions"]["scoreFormat"]),
        })
    }

    /// FR-25/FR-28. Only ever called after explicit user consent.
    pub async fn set_score_format(&self, token: &str) -> AlResult<String> {
        let q = "mutation ($format: ScoreFormat) { UpdateUser(scoreFormat: $format) { id mediaListOptions { scoreFormat } } }";
        let data = self.graphql(q, Some(json!({"format": "POINT_10_DECIMAL"})), Some(token), 15).await?;
        str_of(&data["UpdateUser"]["mediaListOptions"]["scoreFormat"]).ok_or_else(|| unexpected("scoreFormat missing"))
    }

    /// FR-23. One flat list of entries (AniList's per-status lists flattened).
    pub async fn get_media_list_collection(&self, token: &str, user_id: i64, media_type: &str) -> AlResult<Vec<ListEntry>> {
        let q = format!(
            "query ($userId: Int, $type: MediaType) {{ MediaListCollection(userId: $userId, type: $type) {{
               lists {{ entries {{ id status progress repeat score(format: POINT_10_DECIMAL) updatedAt
                 media {{ {MEDIA_FIELDS} }} }} }} }} }}"
        );
        let vars = json!({"userId": user_id, "type": al_type(media_type)});
        let data = self.graphql(&q, Some(vars), Some(token), 20).await?;
        let mut out = Vec::new();
        for list in arr(&data["MediaListCollection"]["lists"]) {
            for e in arr(&list["entries"]) {
                let state = list_entry(&e, media_type);
                out.push(ListEntry {
                    list_entry_id: state.list_entry_id,
                    status: state.status,
                    progress: state.progress,
                    repeat_count: state.repeat_count,
                    score: state.score,
                    updated_at: state.updated_at,
                    media: map_media(&e["media"])?,
                });
            }
        }
        Ok(out)
    }

    /// FR-24/FR-28/FR-58: one request for this title's own list entry plus a
    /// fresh scoreFormat. When the title isn't on the list AniList 404s the
    /// whole query (Viewer included), so score_format is None then.
    pub async fn get_list_entry(&self, token: &str, user_id: i64, anilist_id: i64, media_type: &str) -> AlResult<(Option<EntryState>, Option<String>)> {
        let q = "query ($userId: Int, $mediaId: Int) {
            Viewer { mediaListOptions { scoreFormat } }
            MediaList(userId: $userId, mediaId: $mediaId) { id status progress repeat score(format: POINT_10_DECIMAL) updatedAt } }";
        match self.graphql(q, Some(json!({"userId": user_id, "mediaId": anilist_id})), Some(token), 15).await {
            Err(AlError::NotFound(_)) => Ok((None, None)),
            Err(e) => Err(e),
            Ok(data) => {
                let node = &data["MediaList"];
                let entry = (!node.is_null()).then(|| list_entry(node, media_type));
                Ok((entry, str_of(&data["Viewer"]["mediaListOptions"]["scoreFormat"])))
            }
        }
    }

    /// FR-24/FR-25. `score` is omitted entirely when there's no rating yet:
    /// AniList rejects an explicit `score: null` but accepts the key absent.
    pub async fn push_list_entry(&self, token: &str, anilist_id: i64, local_status: &str, progress: i64, repeat_count: i64, overall: Option<f64>) -> AlResult<i64> {
        let q = "mutation ($mediaId: Int, $status: MediaListStatus, $progress: Int, $repeat: Int, $score: Float) {
            SaveMediaListEntry(mediaId: $mediaId, status: $status, progress: $progress, repeat: $repeat, score: $score) {
              id status progress updatedAt } }";
        let mut vars = json!({
            "mediaId": anilist_id,
            "status": local_to_al_status(local_status),
            "progress": progress,
            "repeat": repeat_count,
        });
        if let Some(score) = overall {
            vars["score"] = json!(score);
        }
        let data = self.graphql(q, Some(vars), Some(token), 15).await?;
        data["SaveMediaListEntry"]["id"].as_i64().ok_or_else(|| unexpected("SaveMediaListEntry.id missing"))
    }

    pub async fn delete_list_entry(&self, token: &str, list_entry_id: i64) -> AlResult<bool> {
        let q = "mutation ($id: Int) { DeleteMediaListEntry(id: $id) { deleted } }";
        let data = self.graphql(q, Some(json!({"id": list_entry_id})), Some(token), 15).await?;
        Ok(data["DeleteMediaListEntry"]["deleted"].as_bool().unwrap_or(false))
    }

    // ------------------------------------------------------------ discover ----

    /// FR-32. season/seasonYear/format are only sent when the section needs them.
    pub async fn discover(&self, section: &str, media_type: &str) -> AlResult<Vec<MediaFields>> {
        let sort = match section {
            "trending" => "TRENDING_DESC",
            "top_ranked" | "top_ranked_movies" => "SCORE_DESC",
            _ => "POPULARITY_DESC",
        };
        let mut vars = json!({"type": al_type(media_type), "sort": [sort]});
        match section {
            "current_season" | "upcoming_season" => {
                let (season, year) = season_year(if section == "current_season" { 0 } else { 1 });
                vars["season"] = json!(season);
                vars["seasonYear"] = json!(year);
            }
            "top_ranked_movies" => vars["format"] = json!("MOVIE"),
            _ => {}
        }
        let q = format!(
            "query ($type: MediaType, $season: MediaSeason, $seasonYear: Int, $format: MediaFormat, $sort: [MediaSort]) {{
               Page(page: 1, perPage: 20) {{
                 media(type: $type, season: $season, seasonYear: $seasonYear, format: $format, sort: $sort) {{ {MEDIA_FIELDS} }} }} }}"
        );
        let data = self.graphql(&q, Some(vars), None, 15).await?;
        map_nodes(&data["Page"]["media"])
    }

    // ---------------------------------------------------------------- feed ----

    /// FR-34..36. Carries the token: isFollowing is relative to the viewer.
    pub async fn feed(&self, feed_type: &str, page: i64, own_user_id: i64, token: &str) -> AlResult<Value> {
        let q = "query ($userId: Int, $isFollowing: Boolean, $page: Int) {
            Page(page: $page, perPage: 25) { pageInfo { hasNextPage }
              activities(userId: $userId, isFollowing: $isFollowing, sort: ID_DESC, type: MEDIA_LIST) {
                ... on ListActivity { status progress createdAt user { name }
                  media { id type title { romaji english native } coverImage { large } } } } } }";
        let mut vars = json!({"page": page});
        match feed_type {
            "your" => vars["userId"] = json!(own_user_id),
            "following" => vars["isFollowing"] = json!(true),
            _ => {}
        }
        let data = self.graphql(q, Some(vars), Some(token), 15).await?;
        let page_data = &data["Page"];
        let mut activities = Vec::new();
        for a in arr(&page_data["activities"]) {
            let media = &a["media"];
            if media.is_null() || media.as_object().is_some_and(Map::is_empty) {
                continue; // non-ListActivity items in the union come back as {}
            }
            activities.push(json!({
                "username": str_of(&a["user"]["name"]),
                "verb": str_of(&a["status"]),
                "title": pick_title(&media["title"]),
                "title_english": str_of(&media["title"]["english"]),
                "anilist_id": media["id"].as_i64(),
                "media_type": str_of(&media["type"]).map(|t| t.to_lowercase()).filter(|t| !t.is_empty()),
                "cover_url": str_of(&media["coverImage"]["large"]),
                "progress_range": str_of(&a["progress"]),
                "occurred_at": a["createdAt"].as_i64().filter(|t| *t != 0).map(iso_from_unix),
            }));
        }
        Ok(json!({
            "activities": activities,
            "has_more": page_data["pageInfo"]["hasNextPage"].as_bool().unwrap_or(false),
        }))
    }

    // ------------------------------------------------- characters / staff ----

    async fn search_people(&self, field: &str, query: &str) -> AlResult<Value> {
        let q = format!("query ($q: String) {{ Page(page: 1, perPage: 20) {{ {field}(search: $q) {{ id name {{ full }} image {{ large }} }} }} }}");
        let data = self.graphql(&q, Some(json!({"q": query})), None, 15).await?;
        Ok(Value::Array(arr(&data["Page"][field]).iter().map(person).collect()))
    }

    pub async fn search_characters(&self, query: &str) -> AlResult<Value> {
        self.search_people("characters", query).await
    }

    pub async fn search_staff(&self, query: &str) -> AlResult<Value> {
        self.search_people("staff", query).await
    }

    /// FR-38. Live-fetched, never persisted.
    pub async fn get_character(&self, anilist_id: i64) -> AlResult<Option<Value>> {
        let q = "query ($id: Int) { Character(id: $id) { id name { full } image { large } description(asHtml: false) favourites
            media(sort: START_DATE_DESC, perPage: 50) { edges {
              voiceActors { id name { full } languageV2 image { large } }
              node { id title { romaji english native } coverImage { large } startDate { year } } } } } }";
        let data = self.graphql(q, Some(json!({"id": anilist_id})), None, 15).await?;
        let node = &data["Character"];
        if node.is_null() {
            return Ok(None);
        }
        let mut voice_actors = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut appearances = Vec::new();
        for edge in arr(&node["media"]["edges"]) {
            for va in arr(&edge["voiceActors"]) {
                if seen.insert(va["id"].as_i64()) {
                    voice_actors.push(json!({
                        "anilist_id": va["id"],
                        "name": str_of(&va["name"]["full"]),
                        "language": str_of(&va["languageV2"]),
                        "image_url": str_of(&va["image"]["large"]),
                    }));
                }
            }
            appearances.push(work(&edge["node"]));
        }
        Ok(Some(json!({
            "anilist_id": node["id"],
            "name": str_of(&node["name"]["full"]),
            "image_url": str_of(&node["image"]["large"]),
            "bio": str_of(&node["description"]),
            "favorites_count": node["favourites"].as_i64(),
            "voice_actors": voice_actors,
            "appearances": appearances,
        })))
    }

    pub async fn get_staff(&self, anilist_id: i64) -> AlResult<Option<Value>> {
        let q = "query ($id: Int) { Staff(id: $id) { id name { full } image { large } description(asHtml: false)
            favourites gender dateOfBirth { year month day } age yearsActive homeTown bloodType
            characters(sort: FAVOURITES_DESC, perPage: 50) { edges { role node { id name { full } image { large } }
              media { id title { romaji english native } coverImage { large } startDate { year } } } }
            staffMedia(sort: START_DATE_DESC, perPage: 50) { nodes { id title { romaji english native } coverImage { large } startDate { year } } } } }";
        let data = self.graphql(q, Some(json!({"id": anilist_id})), None, 15).await?;
        let node = &data["Staff"];
        if node.is_null() {
            return Ok(None);
        }
        let characters_voiced: Vec<Value> = arr(&node["characters"]["edges"])
            .iter()
            .map(|e| {
                let c = &e["node"];
                let media_list = arr(&e["media"]);
                let primary = media_list.first().cloned().unwrap_or(Value::Null);
                json!({
                    "anilist_id": c["id"].as_i64(),
                    "name": str_of(&c["name"]["full"]),
                    "image_url": str_of(&c["image"]["large"]),
                    "role": str_of(&e["role"]),
                    "work_anilist_id": primary["id"].as_i64(),
                    "work_title": pick_title(&primary["title"]),
                    "work_cover_url": str_of(&primary["coverImage"]["large"]),
                    "year": primary["startDate"]["year"].as_i64(),
                    "work_anilist_ids": media_list.iter().filter_map(|m| m["id"].as_i64()).collect::<Vec<_>>(),
                })
            })
            .collect();
        let works: Vec<Value> = arr(&node["staffMedia"]["nodes"]).iter().map(work).collect();
        let dob = &node["dateOfBirth"];
        Ok(Some(json!({
            "anilist_id": node["id"],
            "name": str_of(&node["name"]["full"]),
            "image_url": str_of(&node["image"]["large"]),
            "bio": str_of(&node["description"]),
            "favorites_count": node["favourites"].as_i64(),
            "gender": str_of(&node["gender"]),
            "birth_year": dob["year"].as_i64(),
            "birth_month": dob["month"].as_i64(),
            "birth_day": dob["day"].as_i64(),
            "age": node["age"].as_i64(),
            "years_active": arr(&node["yearsActive"]),
            "home_town": str_of(&node["homeTown"]),
            "blood_type": str_of(&node["bloodType"]),
            "characters_voiced": characters_voiced,
            "works": works,
        })))
    }

    // ------------------------------------------------------------ favorites ----

    /// FR-39. AniList returns the updated favourites collections; favorited
    /// state is derived by checking membership.
    pub async fn toggle_favorite(&self, token: &str, fav_type: &str, anilist_id: i64) -> AlResult<bool> {
        let (arg, key) = match fav_type {
            "anime" => ("animeId", "anime"),
            "manga" => ("mangaId", "manga"),
            "character" => ("characterId", "characters"),
            "staff" => ("staffId", "staff"),
            _ => ("studioId", "studios"),
        };
        let q = format!(
            "mutation ($id: Int) {{ ToggleFavourite({arg}: $id) {{ anime {{ nodes {{ id }} }} manga {{ nodes {{ id }} }} \
             characters {{ nodes {{ id }} }} staff {{ nodes {{ id }} }} studios {{ nodes {{ id }} }} }} }}"
        );
        let data = self.graphql(&q, Some(json!({"id": anilist_id})), Some(token), 15).await?;
        Ok(arr(&data["ToggleFavourite"][key]["nodes"]).iter().any(|n| n["id"].as_i64() == Some(anilist_id)))
    }

    /// FR-40. Live every call, deliberately not cached.
    pub async fn get_favorites(&self, token: &str) -> AlResult<Value> {
        let q = format!("query {{ Viewer {{ {FAVOURITES_FIELDS} }} }}");
        let data = self.graphql(&q, None, Some(token), 15).await?;
        Ok(favourites(&data["Viewer"]["favourites"]))
    }

    // ------------------------------------------------------ studios / users ----
    // Studio has no image field on AniList's schema.

    pub async fn search_studios(&self, query: &str) -> AlResult<Value> {
        let q = "query ($q: String) { Page(page: 1, perPage: 20) { studios(search: $q) { id name } } }";
        let data = self.graphql(q, Some(json!({"q": query})), None, 15).await?;
        Ok(Value::Array(arr(&data["Page"]["studios"]).iter().map(studio).collect()))
    }

    pub async fn get_studio(&self, anilist_id: i64) -> AlResult<Option<Value>> {
        let q = "query ($id: Int) { Studio(id: $id) { id name isAnimationStudio favourites
            media(sort: START_DATE_DESC, perPage: 50) { nodes { id title { romaji english native } coverImage { large } startDate { year } } } } }";
        let data = self.graphql(q, Some(json!({"id": anilist_id})), None, 15).await?;
        let node = &data["Studio"];
        if node.is_null() {
            return Ok(None);
        }
        Ok(Some(json!({
            "anilist_id": node["id"],
            "name": str_of(&node["name"]),
            "is_animation_studio": node["isAnimationStudio"].as_bool().unwrap_or(false),
            "favorites_count": node["favourites"].as_i64(),
            "works": arr(&node["media"]["nodes"]).iter().map(work).collect::<Vec<_>>(),
        })))
    }

    pub async fn search_users(&self, query: &str) -> AlResult<Value> {
        let q = "query ($q: String) { Page(page: 1, perPage: 20) { users(search: $q) { id name avatar { large } } } }";
        let data = self.graphql(q, Some(json!({"q": query})), None, 15).await?;
        Ok(Value::Array(
            arr(&data["Page"]["users"])
                .iter()
                .map(|n| json!({"anilist_id": n["id"], "name": str_of(&n["name"]), "avatar_url": str_of(&n["avatar"]["large"])}))
                .collect(),
        ))
    }

    /// Public profile for any AniList user; no token needed.
    pub async fn get_user(&self, anilist_id: i64) -> AlResult<Option<Value>> {
        let q = format!(
            "query ($id: Int) {{ User(id: $id) {{ id name about(asHtml: false) avatar {{ large }} bannerImage siteUrl createdAt
               statistics {{ anime {{ count meanScore episodesWatched }} manga {{ count meanScore chaptersRead }} }}
               {FAVOURITES_FIELDS} }} }}"
        );
        let data = self.graphql(&q, Some(json!({"id": anilist_id})), None, 15).await?;
        let node = &data["User"];
        if node.is_null() {
            return Ok(None);
        }
        let a = &node["statistics"]["anime"];
        let m = &node["statistics"]["manga"];
        Ok(Some(json!({
            "anilist_id": node["id"],
            "name": str_of(&node["name"]),
            "avatar_url": str_of(&node["avatar"]["large"]),
            "banner_url": str_of(&node["bannerImage"]),
            "about": str_of(&node["about"]),
            "site_url": str_of(&node["siteUrl"]),
            "created_at": node["createdAt"].as_i64(),
            "anime_count": a["count"].as_i64().unwrap_or(0),
            "anime_mean_score": a["meanScore"].as_f64().unwrap_or(0.0),
            "anime_episodes_watched": a["episodesWatched"].as_i64().unwrap_or(0),
            "manga_count": m["count"].as_i64().unwrap_or(0),
            "manga_mean_score": m["meanScore"].as_f64().unwrap_or(0.0),
            "manga_chapters_read": m["chaptersRead"].as_i64().unwrap_or(0),
            "favorites": favourites(&node["favourites"]),
        })))
    }

    // -------------------------------------------------------------- friends ----

    /// FR-71..73: a friend's anime and manga lists, in two requests (the user,
    /// then both lists). Never sends the token: public data only, so nothing
    /// here can act as, or for, anyone. Looks them up by id when known (a
    /// refresh), since usernames can change.
    pub async fn get_friend_list(&self, name: &str, id: Option<i64>) -> AlResult<FriendLookup> {
        let q = "query ($id: Int, $name: String) { User(id: $id, name: $name) { id name avatar { large } mediaListOptions { scoreFormat } } }";
        let vars = match id {
            Some(id) => json!({"id": id}),
            None => json!({"name": name}),
        };
        let user = match self.graphql(q, Some(vars.clone()), None, 15).await {
            // The lookup says "Not Found." for a private user exactly as for a
            // missing one; only the list query tells them apart ("Private
            // User" vs "User not found"), still two requests in all.
            Err(AlError::NotFound(_)) => {
                let q = "query ($id: Int, $name: String) { MediaListCollection(userId: $id, userName: $name, type: ANIME) { hasNextChunk } }";
                return match self.graphql(q, Some(vars), None, 15).await {
                    Err(e) if is_private(&e) => Ok(FriendLookup::Private(name.to_string())),
                    Err(AlError::NotFound(_)) | Ok(_) => Ok(FriendLookup::NoSuchUser),
                    Err(e) => Err(e),
                };
            }
            Err(e) => return Err(e),
            Ok(data) if data["User"].is_null() => return Ok(FriendLookup::NoSuchUser),
            Ok(data) => data["User"].clone(),
        };
        let user_id = user["id"].as_i64().ok_or_else(|| unexpected("User.id missing"))?;
        let name = str_of(&user["name"]).unwrap_or_else(|| name.to_string());
        // FR-72: POINT_5 and POINT_3 are read in the friend's own format and
        // mapped by normalize_score(); AniList converts every other format.
        let format = match user["mediaListOptions"]["scoreFormat"].as_str() {
            Some("POINT_5") => "POINT_5",
            Some("POINT_3") => "POINT_3",
            _ => "POINT_10_DECIMAL",
        };

        let q = format!(
            "query ($userId: Int, $format: ScoreFormat) {{
               anime: MediaListCollection(userId: $userId, type: ANIME) {{ {FRIEND_LIST_FIELDS} }}
               manga: MediaListCollection(userId: $userId, type: MANGA) {{ {FRIEND_LIST_FIELDS} }} }}"
        );
        let data = match self.graphql(&q, Some(json!({"userId": user_id, "format": format})), None, 30).await {
            // The lookup above found them, so a 404 here is AniList
            // withholding their lists rather than a missing user.
            Err(e) if is_private(&e) || matches!(e, AlError::NotFound(_)) => return Ok(FriendLookup::Private(name)),
            Err(e) => return Err(e),
            Ok(data) => data,
        };
        let mut seen = std::collections::HashSet::new();
        let mut entries = Vec::new();
        for list in ["anime", "manga"].iter().flat_map(|key| arr(&data[*key]["lists"])) {
            for e in arr(&list["entries"]) {
                let media = &e["media"];
                let anilist_id = media["id"].as_i64().ok_or_else(|| unexpected("Media.id missing"))?;
                // Custom lists repeat entries that are already in a status list.
                if !seen.insert(anilist_id) {
                    continue;
                }
                let media_type = to_media_type(&media["type"])?;
                entries.push(FriendEntry {
                    anilist_id,
                    status: local_status_from_al(e["status"].as_str().unwrap_or_default(), &media_type),
                    media_type,
                    score: normalize_score(e["score"].as_f64(), format),
                    title: pick_title(&media["title"]),
                    title_english: str_of(&media["title"]["english"]),
                    cover_url: str_of(&media["coverImage"]["large"]),
                    genres: arr(&media["genres"]).iter().filter_map(str_of).collect(),
                    popularity: media["popularity"].as_i64(),
                    progress: e["progress"].as_i64(),
                });
            }
        }
        Ok(FriendLookup::Found(FriendList {
            anilist_user_id: user_id,
            name,
            avatar_url: str_of(&user["avatar"]["large"]),
            entries,
        }))
    }

    /// FR-79: everyone `user_id` follows, or only those who follow them back,
    /// every page; fails as a whole if any page does, so a sync never acts on
    /// part of the list. Logged out, except one read: for mutual only, your own
    /// following list is read with your `token`, so AniList fills `isFollower`
    /// (whether each of them follows you) and no followers pages are needed.
    /// That answer only counts when AniList confirms the viewer is you; logged
    /// out, `isFollower` is false for everyone. Otherwise (the token refused,
    /// or not vouched for) your followers list decides, read logged out.
    pub async fn get_following(&self, user_id: i64, mutual_only: bool, token: Option<&str>) -> AlResult<Vec<FollowedUser>> {
        let users = |pages: Vec<(FollowedUser, Option<bool>)>| pages.into_iter().map(|(u, _)| u).collect::<Vec<_>>();
        if !mutual_only {
            return Ok(users(self.follow_pages("following", user_id, None).await?));
        }
        let following = match token {
            Some(token) => match self.follow_pages("following", user_id, Some(token)).await {
                Ok(pages) if pages.iter().all(|(_, follows_you)| follows_you.is_some()) => {
                    return Ok(pages.into_iter().filter(|(_, f)| *f == Some(true)).map(|(u, _)| u).collect());
                }
                Ok(pages) => Some(users(pages)),
                Err(e) if e.is_network() => return Err(e),
                Err(e) => {
                    log::warn!("reading your following list with your token failed ({e}); using your followers list");
                    None
                }
            },
            None => None,
        };
        let following = match following {
            Some(f) => f,
            None => users(self.follow_pages("following", user_id, None).await?),
        };
        let followers: HashSet<i64> = self.follow_pages("followers", user_id, None).await?.into_iter().map(|(u, _)| u.anilist_user_id).collect();
        Ok(following.into_iter().filter(|u| followers.contains(&u.anilist_user_id)).collect())
    }

    /// `field` is "following" or "followers". With a token, each user comes
    /// with whether they follow the viewer, but only when every page's Viewer
    /// is `user_id`; otherwise None.
    async fn follow_pages(&self, field: &str, user_id: i64, token: Option<&str>) -> AlResult<Vec<(FollowedUser, Option<bool>)>> {
        let (viewer, is_follower) = if token.is_some() { ("viewer: Viewer { id }", " isFollower") } else { ("", "") };
        let q = format!(
            "query ($userId: Int!, $page: Int) {{ {viewer} Page(page: $page, perPage: 50) {{ pageInfo {{ hasNextPage }}
               {field}(userId: $userId, sort: [ID]) {{ id name avatar {{ large }}{is_follower} }} }} }}"
        );
        let mut users = Vec::new();
        for page in 1..=MAX_FOLLOW_PAGES {
            let data = self.graphql(&q, Some(json!({"userId": user_id, "page": page})), token, 15).await?;
            let vouched = token.is_some() && data["viewer"]["id"].as_i64() == Some(user_id);
            users.extend(arr(&data["Page"][field]).iter().filter_map(|u| {
                let user = FollowedUser { anilist_user_id: u["id"].as_i64()?, name: str_of(&u["name"])?, avatar_url: str_of(&u["avatar"]["large"]) };
                Some((user, u["isFollower"].as_bool().filter(|_| vouched)))
            }));
            if data["Page"]["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                return Ok(users);
            }
        }
        Err(unexpected("more than 10,000 follows"))
    }

    // ------------------------------------------------------------ community ----

    /// FR-49/FR-55: by anilist_id only, never re-derived by title.
    pub async fn get_community(&self, anilist_id: i64) -> AlResult<Option<Value>> {
        let q = "query ($id: Int) { Media(id: $id) {
            relations { edges { relationType node { id title { romaji english native } coverImage { large } type startDate { year } } } }
            characters(sort: ROLE, perPage: 10) { edges { role node { id name { full } image { large } } } }
            recommendations(sort: RATING_DESC, perPage: 6) { nodes { rating mediaRecommendation { id title { romaji english native } coverImage { large } } } }
            popularity favourites trending rankings { rank type allTime } } }";
        let data = self.graphql(q, Some(json!({"id": anilist_id})), None, 15).await?;
        let node = &data["Media"];
        if node.is_null() {
            return Ok(None);
        }
        let relations: Vec<Value> = arr(&node["relations"]["edges"])
            .iter()
            .map(|edge| {
                let n = &edge["node"];
                json!({
                    "anilist_id": n["id"].as_i64(),
                    "type": str_of(&edge["relationType"]),
                    "title": pick_title(&n["title"]),
                    "title_english": str_of(&n["title"]["english"]),
                    "cover_url": str_of(&n["coverImage"]["large"]),
                    "year": n["startDate"]["year"].as_i64(),
                    "media_type": str_of(&n["type"]).map(|t| t.to_lowercase()).filter(|t| !t.is_empty()),
                })
            })
            .collect();
        let characters: Vec<Value> = arr(&node["characters"]["edges"])
            .iter()
            .map(|edge| {
                let n = &edge["node"];
                json!({
                    "anilist_id": n["id"].as_i64(),
                    "name": str_of(&n["name"]["full"]),
                    "role": str_of(&edge["role"]),
                    "image_url": str_of(&n["image"]["large"]),
                })
            })
            .collect();
        let recommended: Vec<Value> = arr(&node["recommendations"]["nodes"])
            .iter()
            .filter(|rec| !rec["mediaRecommendation"].is_null())
            .map(|rec| {
                let m = &rec["mediaRecommendation"];
                json!({
                    "anilist_id": m["id"].as_i64(),
                    "title": pick_title(&m["title"]),
                    "title_english": str_of(&m["title"]["english"]),
                    "cover_url": str_of(&m["coverImage"]["large"]),
                    "thumbs": rec["rating"].as_i64(),
                })
            })
            .collect();
        let rank = |kind: &str| {
            arr(&node["rankings"])
                .iter()
                .find(|r| r["type"] == kind && r["allTime"].as_bool() == Some(true))
                .and_then(|r| r["rank"].as_i64())
        };
        Ok(Some(json!({
            "relations": relations,
            "characters": characters,
            "recommended": recommended,
            "popularity": node["popularity"].as_i64(),
            "favourites": node["favourites"].as_i64(),
            "trending": node["trending"].as_i64(),
            "rank_rated": rank("RATED"),
            "rank_popular": rank("POPULAR"),
        })))
    }

    // -------------------------------------------------------------- profile ----

    /// FR-42: all six stats live from AniList, not local library counts. The
    /// Profile header's avatar and banner come back on the same Viewer query.
    pub async fn get_profile_stats(&self, token: &str, user_id: i64) -> AlResult<Value> {
        let q = "query ($userId: Int!) {
            Viewer { avatar { large } bannerImage statistics { anime { count minutesWatched } manga { count chaptersRead } } }
            followersPage: Page(perPage: 1) { pageInfo { total } followers(userId: $userId) { id } }
            followingPage: Page(perPage: 1) { pageInfo { total } following(userId: $userId) { id } } }";
        let data = self.graphql(q, Some(json!({"userId": user_id})), Some(token), 15).await?;
        let viewer = &data["Viewer"];
        let stats = &viewer["statistics"];
        let minutes = stats["anime"]["minutesWatched"].as_f64().unwrap_or(0.0);
        Ok(json!({
            "total_anime": stats["anime"]["count"].as_i64().unwrap_or(0),
            "total_manga": stats["manga"]["count"].as_i64().unwrap_or(0),
            "days_watched": round1(minutes / 60.0 / 24.0),
            "chapters_read": stats["manga"]["chaptersRead"].as_i64().unwrap_or(0),
            "followers": data["followersPage"]["pageInfo"]["total"].as_i64().unwrap_or(0),
            "following": data["followingPage"]["pageInfo"]["total"].as_i64().unwrap_or(0),
            "avatar_url": str_of(&viewer["avatar"]["large"]),
            "banner_url": str_of(&viewer["bannerImage"]),
        }))
    }

    // -------------------------------------------------------- pick an anime ----

    /// FR-84: the genre and tag lists for the picker's filters, in one request.
    pub async fn pick_options(&self) -> AlResult<Value> {
        let q = "{ GenreCollection MediaTagCollection { name category isGeneralSpoiler isAdult } }";
        let data = self.graphql(q, None, None, 15).await?;
        let tags: Vec<Value> = arr(&data["MediaTagCollection"])
            .iter()
            .map(|t| {
                json!({
                    "name": t["name"], "category": t["category"],
                    "is_spoiler": t["isGeneralSpoiler"].as_bool().unwrap_or(false),
                    "is_adult": t["isAdult"].as_bool().unwrap_or(false),
                })
            })
            .collect();
        let genres: Vec<String> = arr(&data["GenreCollection"]).iter().filter_map(str_of).collect();
        Ok(json!({"genres": genres, "tags": tags}))
    }

    /// FR-84/FR-85: one random anime from everything the filters match in
    /// `pool`, never a sequel you aren't caught up on unless `check` is None,
    /// in at most PICK_BUDGET requests to AniList, 429 retries included.
    ///
    /// Page 1 comes first, alone: a pool that fits on it is counted exactly
    /// in one request. pageInfo.total isn't a count (notes #19), so a bigger
    /// pool is then bracketed with single-entry probes (each probe past the
    /// end costs AniList a full scan, notes #27, which is why small pools
    /// never send them), and pick_pages() takes it from there. `known` is the
    /// size from an earlier pick on the same filters, already shrunk by the
    /// titles shown since: it skips straight to pick_pages(). `roll` comes
    /// from the client (0 <= roll < 1) and seeds every draw, so this stays
    /// deterministic.
    pub async fn pick(&self, filters: &PickFilters, pool: &PickPool, roll: f64, known: Option<PickSize>, check: Option<&PickCheck>) -> AlResult<Picked> {
        let mut vars = pick_variables(filters, pool);
        vars["sort"] = json!(["ID"]);
        let mut run = PickRun { vars, filters, check, draws: Draws::new(roll), budget: PICK_BUDGET };
        let mut picked = self.pick_within(&mut run, known).await?;
        picked.requests = PICK_BUDGET - run.budget;
        Ok(picked)
    }

    async fn pick_within(&self, run: &mut PickRun<'_>, known: Option<PickSize>) -> AlResult<Picked> {
        if let Some(size) = known.filter(|s| PICK_PAGE < s.lo && s.lo <= s.hi && s.hi <= PICK_DEPTH) {
            if let Some(picked) = self.pick_pages(run, size, false, None).await? {
                return Ok(picked);
            }
            // The page it tried was empty: the pool shrank more than the hint knew. Size it afresh.
        }

        let first = match self.pick_fetch(run, 1).await? {
            Some((first, more)) if !first.is_empty() => Some((first, more)),
            _ => None,
        };
        let Some((first, more)) = first else {
            return Ok(Picked::nothing(0, true, None, false));
        };
        let (size, exact) = if more {
            let probes = pick_probes();
            let pages: String = probes.iter().map(|i| pick_probe(*i)).collect();
            match self.pick_request(run, &format!("query ({PICK_VARS}) {{{pages} }}"), None).await? {
                Some(data) => {
                    let exists = |i: &&usize| !arr(&data[format!("at{i}")]["media"]).is_empty();
                    let lo = probes.iter().take_while(exists).last().map_or(first.len(), |i| i + 1);
                    let hi = probes.iter().find(|i| !exists(i)).copied().unwrap_or(PICK_DEPTH);
                    (PickSize { lo, hi }, false)
                }
                // Out of requests to size it: page 1 is all there is to pick from.
                None => (PickSize { lo: first.len(), hi: first.len() }, false),
            }
        } else {
            (PickSize { lo: first.len(), hi: first.len() }, true)
        };
        let picked = self.pick_pages(run, size, exact, Some(&first)).await?;
        Ok(picked.unwrap_or_else(|| Picked::nothing(0, true, None, false)))
    }

    /// Fetches a random page of a pool of `size` titles whole and tries its
    /// titles in random order: the first one the prequel check lets through
    /// is the pick. Landing on a held-back title first walks back its chain
    /// for a next-up (once per pick). A page with nothing usable (or empty,
    /// past the end) leads to `page_one` when it's in hand, which costs no
    /// request, then to another random page while the pick has requests left.
    /// None when the first page fetched is empty with no page 1 to fall back
    /// on: `size` was stale.
    async fn pick_pages(&self, run: &mut PickRun<'_>, size: PickSize, exact: bool, page_one: Option<&[Value]>) -> AlResult<Option<Picked>> {
        // Over the paging limit (notes #20): alternate the oldest and newest 5000.
        let sort = if size.lo >= PICK_DEPTH && run.draws.next() >= 0.5 { "ID_DESC" } else { "ID" };
        run.vars["sort"] = json!([sort]);
        let in_hand = page_one.filter(|_| sort == "ID");
        let (maybe, sure) = (size.hi.div_ceil(PICK_PAGE), size.lo.div_ceil(PICK_PAGE).max(1));
        let size_hint = (!exact).then_some(size);
        let mut page = 1 + ((run.draws.next() * maybe as f64) as usize).min(maybe - 1);
        let (mut tried, mut walked, mut blocked) = (Vec::new(), false, false);
        loop {
            tried.push(page);
            let items = match in_hand {
                Some(first) if page == 1 => first.to_vec(),
                _ => match self.pick_fetch(run, page).await? {
                    Some((items, _)) if items.is_empty() && page_one.is_none() && tried.len() == 1 => return Ok(None),
                    Some((items, _)) => items,
                    None => Vec::new(), // out of requests
                },
            };
            let mut order: Vec<usize> = (0..items.len()).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, (run.draws.next() * (i + 1) as f64) as usize);
            }
            for node in order.into_iter().map(|i| &items[i]) {
                let Some(check) = run.check else {
                    return Ok(Some(Picked::found(node, None, size.lo, exact, size_hint)?));
                };
                let Some(prequel) = blocking_prequel(node, &check.watched) else {
                    return Ok(Some(Picked::found(node, None, size.lo, exact, size_hint)?));
                };
                blocked = true;
                if !walked {
                    walked = true;
                    if let Some(next) = self.next_up(run, node, prequel).await {
                        return Ok(Some(Picked::found(&next, Some(node), size.lo, exact, size_hint)?));
                    }
                }
            }
            // Page 1 in hand costs nothing, so it's always next, even out of requests.
            let next = if in_hand.is_some() && !tried.contains(&1) {
                Some(1)
            } else if run.budget == 0 {
                None
            } else {
                let left: Vec<usize> = (1..=sure).filter(|p| !tried.contains(p)).collect();
                (!left.is_empty()).then(|| left[(run.draws.next() * left.len() as f64) as usize])
            };
            match next {
                Some(p) => page = p,
                None => break,
            }
        }
        Ok(Some(Picked::nothing(size.lo, exact, size_hint, blocked)))
    }

    /// graphql() for a pick: None once the pick's budget is spent, and every
    /// request it does send, 429 retries included, comes out of that budget.
    /// `vars` defaults to the pick's filters.
    async fn pick_request(&self, run: &mut PickRun<'_>, query: &str, vars: Option<Value>) -> AlResult<Option<Value>> {
        if run.budget == 0 {
            return Ok(None);
        }
        let vars = vars.unwrap_or_else(|| run.vars.clone());
        self.graphql_within(query, Some(vars), None, 20, &mut run.budget).await.map(Some)
    }

    /// One whole page of the pool, aliased `page{n}`: its titles, and whether
    /// more follow. None when the pick is out of requests.
    async fn pick_fetch(&self, run: &mut PickRun<'_>, page: usize) -> AlResult<Option<(Vec<Value>, bool)>> {
        let q = format!(
            "query ({PICK_VARS}) {{ page{page}: Page(page: {page}, perPage: {PICK_PAGE}) {{ pageInfo {{ hasNextPage }}
               media({PICK_ARGS}) {{ {MEDIA_FIELDS} {PICK_FIELDS} }} }} }}"
        );
        Ok(self.pick_request(run, &q, None).await?.map(|data| {
            let found = &data[format!("page{page}")];
            (arr(&found["media"]), found["pageInfo"]["hasNextPage"] == true)
        }))
    }

    /// FR-85: the earliest entry you haven't completed in `node`'s PREQUEL
    /// chain, from its blocking `prequel` back, for as long as the pick has
    /// requests left. Each lookup brings the entry's related entries in full,
    /// with their own relations (notes #29), so one request settles two
    /// entries: the one looked up and the prequel it's waiting on. None when
    /// the requests run out first, the chain loops or can't be read, or it
    /// ends on an entry the filters rule out or already shown: the next-up is
    /// only ever a bonus, so a failed lookup never fails the pick.
    async fn next_up(&self, run: &mut PickRun<'_>, node: &Value, prequel: i64) -> Option<Value> {
        let (filters, check) = (run.filters, run.check?);
        let q = format!(
            "query ($id: Int) {{ Media(id: $id, type: ANIME) {{ {MEDIA_FIELDS} {PICK_FIELDS}
               relations {{ edges {{ node {{ {MEDIA_FIELDS} {PICK_FIELDS} }} }} }} }} }}"
        );
        let mut visited: HashSet<i64> = node["id"].as_i64().into_iter().collect();
        let mut id = prequel;
        loop {
            if !visited.insert(id) {
                return None;
            }
            let entry = match self.pick_request(run, &q, Some(json!({"id": id}))).await {
                Ok(Some(data)) => data["Media"].clone(),
                Ok(None) | Err(AlError::NotFound(_)) => return None,
                Err(e) => {
                    log::warn!("next-up lookup for anilist_id={id} failed: {e}");
                    return None;
                }
            };
            if entry.is_null() {
                return None;
            }
            let Some(earlier) = blocking_prequel(&entry, &check.watched) else {
                return next_up_allowed(&entry, filters, &check.shown).then_some(entry);
            };
            // The prequel came along in full. Only trust it with its own
            // relations in hand; otherwise it takes a lookup of its own.
            let related = arr(&entry["relations"]["edges"]).into_iter().map(|e| e["node"].clone()).find(|n| n["id"] == earlier);
            let Some(prequel) = related.filter(|n| n["relations"]["edges"].is_array()) else {
                id = earlier;
                continue;
            };
            if !visited.insert(earlier) {
                return None;
            }
            match blocking_prequel(&prequel, &check.watched) {
                Some(further) => id = further,
                None => return next_up_allowed(&prequel, filters, &check.shown).then_some(prequel),
            }
        }
    }
}

// --------------------------------------------------------- pick an anime ----

/// FR-84 filters, as POST /pick's body carries them. All optional; ranges
/// are inclusive here and converted to AniList's bounds in pick_variables().
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PickFilters {
    #[serde(default)]
    pub genres_in: Vec<String>,
    #[serde(default)]
    pub genres_not_in: Vec<String>,
    #[serde(default)]
    pub tags_in: Vec<String>,
    #[serde(default)]
    pub tags_not_in: Vec<String>,
    pub popularity_min: Option<i64>,
    pub popularity_max: Option<i64>,
    /// The app's 1-10 scale.
    pub score_min: Option<f64>,
    #[serde(default)]
    pub formats: Vec<String>,
    pub year_min: Option<i64>,
    pub year_max: Option<i64>,
    pub season: Option<String>,
    pub episodes_min: Option<i64>,
    pub episodes_max: Option<i64>,
    #[serde(default)]
    pub statuses: Vec<String>,
    #[serde(default)]
    pub include_adult: bool,
    /// FR-85's toggle: skip the prequel check.
    #[serde(default)]
    pub include_unwatched_sequels: bool,
}

/// What FR-85's prequel check needs from the library.
pub struct PickCheck {
    /// Anime that are Completed or Rewatching.
    pub watched: HashSet<i64>,
    /// Titles already shown this session, never offered again as a next-up.
    pub shown: HashSet<i64>,
}

/// Which AniList ids a pick may land on.
pub enum PickPool {
    /// On Planning: only these.
    Only(Vec<i64>),
    /// Not on Planning: anything but these.
    Except(Vec<i64>),
}

pub struct Picked {
    pub result: Option<Value>,
    /// Matches left: exact when `pool_exact`, else a lower bound (at most PICK_DEPTH).
    pub pool: usize,
    pub pool_exact: bool,
    /// For a pool bigger than a page: its size, for the next pick to reuse.
    pub size: Option<PickSize>,
    /// FR-85: the held-back sequel that `result` is the next-up for.
    pub next_up_for: Option<Value>,
    /// No result, and titles were tried that FR-85 held back.
    pub blocked: bool,
    /// AniList requests this pick sent, 429 retries included (at most PICK_BUDGET).
    pub requests: usize,
}

impl Picked {
    fn found(node: &Value, via: Option<&Value>, pool: usize, pool_exact: bool, size: Option<PickSize>) -> AlResult<Self> {
        let next_up_for = via.map(|v| {
            json!({"anilist_id": v["id"], "title": pick_title(&v["title"]), "title_english": str_of(&v["title"]["english"])})
        });
        Ok(Picked { result: Some(map_pick(node)?), pool, pool_exact, size, next_up_for, blocked: false, requests: 0 })
    }

    fn nothing(pool: usize, pool_exact: bool, size: Option<PickSize>, blocked: bool) -> Self {
        Picked { result: None, pool, pool_exact, size, next_up_for: None, blocked, requests: 0 }
    }
}

/// What every request of one pick shares.
struct PickRun<'a> {
    vars: Value,
    filters: &'a PickFilters,
    check: Option<&'a PickCheck>,
    draws: Draws,
    /// Requests the pick may still send.
    budget: usize,
}

/// Every random choice of one pick: the client's roll first, then a
/// splitmix64 sequence seeded from it, so a given roll always picks the same.
struct Draws {
    first: Option<f64>,
    state: u64,
}

impl Draws {
    fn new(roll: f64) -> Self {
        Draws { first: Some(roll), state: (roll * (1u64 << 53) as f64) as u64 }
    }

    /// The next draw, 0 <= x < 1.
    fn next(&mut self) -> f64 {
        if let Some(roll) = self.first.take() {
            return roll;
        }
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A pool bigger than a page holds `lo` to `hi` titles (`hi` within 25% of
/// `lo`); 5000 and 5000 when it's past the paging limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PickSize {
    pub lo: usize,
    pub hi: usize,
}

pub const PICK_FORMATS: [&str; 6] = ["TV", "TV_SHORT", "MOVIE", "OVA", "ONA", "SPECIAL"];
pub const PICK_STATUSES: [&str; 3] = ["FINISHED", "RELEASING", "NOT_YET_RELEASED"];

/// AniList won't page past its first 5000 matches (notes #20).
pub const PICK_DEPTH: usize = 5000;
const PICK_PAGE: usize = 50;
/// AniList allows 30 requests a minute (notes #26): one pick never sends more
/// than this many, 429 retries included. Sizing, extra pages and the FR-85
/// walk back all share it.
pub const PICK_BUDGET: usize = 4;
/// FR-85: the prequel formats that count.
const PREQUEL_FORMATS: [&str; 4] = ["TV", "TV_SHORT", "MOVIE", "ONA"];

const PICK_VARS: &str = "$idIn: [Int], $idNotIn: [Int], $genreIn: [String], $genreNotIn: [String],
    $tagIn: [String], $tagNotIn: [String], $popularityGreater: Int, $popularityLesser: Int,
    $scoreGreater: Int, $formatIn: [MediaFormat], $startGreater: FuzzyDateInt, $startLesser: FuzzyDateInt,
    $season: MediaSeason, $episodesGreater: Int, $episodesLesser: Int, $statusIn: [MediaStatus],
    $isAdult: Boolean, $sort: [MediaSort]";

const PICK_ARGS: &str = "type: ANIME, id_in: $idIn, id_not_in: $idNotIn, genre_in: $genreIn,
    genre_not_in: $genreNotIn, tag_in: $tagIn, tag_not_in: $tagNotIn,
    popularity_greater: $popularityGreater, popularity_lesser: $popularityLesser,
    averageScore_greater: $scoreGreater, format_in: $formatIn, startDate_greater: $startGreater,
    startDate_lesser: $startLesser, season: $season, episodes_greater: $episodesGreater,
    episodes_lesser: $episodesLesser, status_in: $statusIn, isAdult: $isAdult, sort: $sort";

/// On top of MEDIA_FIELDS (GraphQL merges the two `tags` selections).
/// Relations are for FR-85's prequel check.
const PICK_FIELDS: &str = "tags { rank isMediaSpoiler isGeneralSpoiler isAdult } popularity status startDate { year } isAdult
    relations { edges { relationType node { id format type } } }";

/// FR-84 filters as AniList variables. Only the filters that are set are
/// sent: an explicit null for a list argument is a 500 on AniList (notes #22).
fn pick_variables(f: &PickFilters, pool: &PickPool) -> Value {
    let mut vars = Map::new();
    let (pool_key, ids) = match pool {
        PickPool::Only(ids) => ("idIn", ids),
        PickPool::Except(ids) => ("idNotIn", ids),
    };
    if !ids.is_empty() {
        vars.insert(pool_key.into(), json!(ids));
    }
    let lists = [
        ("genreIn", &f.genres_in), ("genreNotIn", &f.genres_not_in), ("tagIn", &f.tags_in),
        ("tagNotIn", &f.tags_not_in), ("formatIn", &f.formats), ("statusIn", &f.statuses),
    ];
    for (key, list) in lists {
        if !list.is_empty() {
            vars.insert(key.into(), json!(list));
        }
    }
    // Popularity and episode bounds are strict on AniList; averageScore_greater
    // already includes its bound; year-only start dates are YYYY0000 (notes #25).
    let bounds = [
        ("popularityGreater", f.popularity_min.map(|n| n - 1)),
        ("popularityLesser", f.popularity_max.map(|n| n + 1)),
        ("episodesGreater", f.episodes_min.map(|n| n - 1)),
        ("episodesLesser", f.episodes_max.map(|n| n + 1)),
        ("scoreGreater", f.score_min.map(|s| (s * 10.0).round() as i64)),
        ("startGreater", f.year_min.map(|y| y * 10000 - 1)),
        ("startLesser", f.year_max.map(|y| y * 10000 + 9999)),
    ];
    for (key, bound) in bounds {
        if let Some(n) = bound {
            vars.insert(key.into(), json!(n));
        }
    }
    if let Some(season) = &f.season {
        vars.insert("season".into(), json!(season));
    }
    if !f.include_adult {
        vars.insert("isAdult".into(), json!(false));
    }
    Value::Object(vars)
}

/// Pool indexes probed to bracket a pool's size: page 1 covers 0..50, then a
/// x1.25 ladder up to the paging limit, so `hi` is within 25% of `lo`.
fn pick_probes() -> Vec<usize> {
    let mut probes = Vec::new();
    let mut x = PICK_PAGE as f64;
    while x < (PICK_DEPTH - 1) as f64 {
        probes.push(x.round() as usize);
        x *= 1.25;
    }
    probes.push(PICK_DEPTH - 1);
    probes
}

/// Whether the pool has an entry at `index`, aliased `at{index}`.
fn pick_probe(index: usize) -> String {
    format!(" at{index}: Page(page: {}, perPage: 1) {{ media({PICK_ARGS}) {{ id }} }}", index + 1)
}

/// FR-85: the prequel holding `node` back, if any: a direct PREQUEL to an
/// anime in a format you'd watch first (TV, TV Short, Movie, ONA) that isn't
/// Completed or Rewatching. A series entry comes before a movie when several do.
fn blocking_prequel(node: &Value, watched: &HashSet<i64>) -> Option<i64> {
    arr(&node["relations"]["edges"])
        .iter()
        .filter(|e| e["relationType"] == "PREQUEL" && e["node"]["type"] == "ANIME")
        .filter_map(|e| Some((e["node"]["id"].as_i64()?, e["node"]["format"].as_str()?.to_string())))
        .filter(|(id, format)| PREQUEL_FORMATS.contains(&format.as_str()) && !watched.contains(id))
        .min_by_key(|(_, format)| format == "MOVIE")
        .map(|(id, _)| id)
}

/// FR-85: a next-up is offered only within the adult setting and the genre
/// and tag filters, and never twice in a session.
fn next_up_allowed(entry: &Value, f: &PickFilters, shown: &HashSet<i64>) -> bool {
    let genres: Vec<String> = arr(&entry["genres"]).iter().filter_map(str_of).collect();
    let tags: Vec<String> = arr(&entry["tags"]).iter().filter_map(|t| str_of(&t["name"])).collect();
    let fits = |all: &[String], none: &[String], has: &[String]| all.iter().all(|x| has.contains(x)) && !none.iter().any(|x| has.contains(x));
    !entry["id"].as_i64().is_some_and(|id| shown.contains(&id))
        && (f.include_adult || entry["isAdult"] != true)
        && fits(&f.genres_in, &f.genres_not_in, &genres)
        && fits(&f.tags_in, &f.tags_not_in, &tags)
}

/// A search result (map_media) plus what the picker's card shows on top.
fn map_pick(node: &Value) -> AlResult<Value> {
    let mut out = json!(map_media(node)?);
    let mut tags: Vec<Value> = arr(&node["tags"])
        .iter()
        .map(|t| {
            let flag = |k: &str| t[k].as_bool().unwrap_or(false);
            json!({
                "name": t["name"], "rank": t["rank"],
                "is_spoiler": flag("isMediaSpoiler") || flag("isGeneralSpoiler"),
                "is_adult": flag("isAdult"),
            })
        })
        .collect();
    tags.sort_by_key(|t| std::cmp::Reverse(t["rank"].as_i64().unwrap_or(0)));
    out["popularity"] = node["popularity"].clone();
    out["airing_status"] = node["status"].clone();
    // The year filter is on start date; `year` is seasonYear, which some titles lack.
    out["start_year"] = node["startDate"]["year"].clone();
    out["tags_ranked"] = json!(tags);
    Ok(out)
}

// ------------------------------------------------------------------ helpers ----

const MEDIA_FIELDS: &str = "id type title { romaji english native } coverImage { large } genres tags { name }
    studios(isMain: true) { nodes { name } } seasonYear season format episodes chapters volumes duration
    description(asHtml: false) averageScore popularity";

const FAVOURITES_FIELDS: &str = "favourites {
    anime { nodes { id title { romaji english native } coverImage { large } } }
    manga { nodes { id title { romaji english native } coverImage { large } } }
    characters { nodes { id name { full } image { large } } }
    staff { nodes { id name { full } image { large } } }
    studios { nodes { id name } } }";

/// FR-72: scores in `$format` (see get_friend_list). A bare `score` isn't
/// reliably in any one format, so the format is always named.
const FRIEND_LIST_FIELDS: &str = "lists { entries { status progress score(format: $format)
    media { id type title { romaji english native } coverImage { large } genres popularity } } }";

fn unexpected(msg: &str) -> AlError {
    AlError::Unexpected(msg.to_string())
}

/// AniList's "Private User" error: the user's lists aren't public.
fn is_private(e: &AlError) -> bool {
    match e {
        AlError::NotFound(m) | AlError::Api { message: m, .. } => m.to_ascii_lowercase().contains("private"),
        _ => false,
    }
}

fn retry_after_seconds(response: &reqwest::Response) -> f64 {
    response
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| *s > 0.0 && s.is_finite())
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS)
}

async fn unwrap_response(response: reqwest::Response) -> AlResult<Value> {
    let status = response.status().as_u16();
    // Rate limits and server errors are transient: queue-for-retry class.
    if status == 429 || status >= 500 {
        return Err(AlError::Network(format!("AniList HTTP {status}")));
    }
    let text = response.text().await?;
    let body: Value = match serde_json::from_str(&text) {
        Ok(b) => b,
        Err(_) if status >= 400 => return Err(AlError::Network(format!("AniList HTTP {status}"))),
        Err(_) => return Err(AlError::Api { message: "AniList returned a non-JSON response".into(), status }),
    };
    if let Some(errors) = body["errors"].as_array().filter(|e| !e.is_empty()) {
        let message = errors
            .iter()
            .map(|e| e["message"].as_str().map(str::to_string).unwrap_or_else(|| "unknown error".into()))
            .collect::<Vec<_>>()
            .join("; ");
        if status == 404 || errors.iter().any(|e| e["status"] == 404) {
            return Err(AlError::NotFound(message));
        }
        return Err(AlError::Api { message, status });
    }
    if status >= 400 {
        return Err(AlError::Network(format!("AniList HTTP {status}")));
    }
    Ok(body["data"].clone())
}

fn al_type(media_type: &str) -> &'static str {
    if media_type == "manga" { "MANGA" } else { "ANIME" }
}

/// AniList's MediaType -> ours. Anything else is an error, never a default:
/// media_type is set once from AniList and never guessed.
pub fn to_media_type(al: &Value) -> AlResult<String> {
    match al.as_str() {
        Some("ANIME") => Ok("anime".into()),
        Some("MANGA") => Ok("manga".into()),
        _ => Err(AlError::Unexpected(format!("unexpected AniList media type: {al}"))),
    }
}

/// AniList MediaListStatus <-> local status (FR-17). CURRENT/REPEATING are
/// media-type-ambiguous on AniList's side, so mapping in needs media_type.
pub fn local_status_from_al(al_status: &str, media_type: &str) -> String {
    let anime = media_type == "anime";
    match al_status {
        "CURRENT" => if anime { "watching" } else { "reading" },
        "COMPLETED" => "completed",
        "DROPPED" => "dropped",
        "PAUSED" => "paused",
        "REPEATING" => if anime { "rewatching" } else { "rereading" },
        _ => "planning",
    }
    .to_string()
}

fn local_to_al_status(local: &str) -> &'static str {
    match local {
        "watching" | "reading" => "CURRENT",
        "completed" => "COMPLETED",
        "dropped" => "DROPPED",
        "paused" => "PAUSED",
        "rewatching" | "rereading" => "REPEATING",
        _ => "PLANNING",
    }
}

/// FR-17: the statuses valid for a media_type.
pub fn local_statuses(media_type: &str) -> Vec<&'static str> {
    let mut v = if media_type == "anime" {
        vec!["completed", "dropped", "paused", "planning", "rewatching", "watching"]
    } else {
        vec!["completed", "dropped", "paused", "planning", "reading", "rereading"]
    };
    v.sort_unstable();
    v
}

/// FR-22, implicit grant: AniList sends the token back to the client's
/// registered redirect URI, so none is passed here.
pub fn authorize_url(client_id: &str) -> String {
    format!("{AUTHORIZE_URL}?client_id={client_id}&response_type=token")
}

pub const SEASONS: [&str; 4] = ["WINTER", "SPRING", "SUMMER", "FALL"];

fn season_year(offset: i32) -> (&'static str, i32) {
    let now = Utc::now();
    let idx = (now.month() as i32 - 1) / 3 + offset;
    (SEASONS[(idx % 4) as usize], now.year() + idx / 4)
}

fn iso_from_unix(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Secs, false)
}

fn arr(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn str_of(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// romaji, else english, else native (empty strings skipped, like Python's `or`).
fn pick_title(t: &Value) -> Option<String> {
    ["romaji", "english", "native"]
        .iter()
        .find_map(|k| t[*k].as_str().filter(|s| !s.is_empty()).map(str::to_string))
}

fn map_media(node: &Value) -> AlResult<MediaFields> {
    let t = &node["title"];
    Ok(MediaFields {
        anilist_id: node["id"].as_i64().ok_or_else(|| unexpected("Media.id missing"))?,
        media_type: to_media_type(&node["type"])?,
        title: pick_title(t),
        title_native: str_of(&t["native"]),
        title_english: str_of(&t["english"]),
        cover_url: str_of(&node["coverImage"]["large"]),
        genres: arr(&node["genres"]).iter().filter_map(str_of).collect(),
        tags: arr(&node["tags"]).iter().filter_map(|g| str_of(&g["name"])).collect(),
        producers: arr(&node["studios"]["nodes"]).iter().filter_map(|s| str_of(&s["name"])).collect(),
        year: node["seasonYear"].as_i64(),
        season: str_of(&node["season"]),
        format: str_of(&node["format"]),
        episodes: node["episodes"].as_i64(),
        chapters: node["chapters"].as_i64(),
        volumes: node["volumes"].as_i64(),
        runtime_minutes: node["duration"].as_i64(),
        synopsis: str_of(&node["description"]),
        avg_score: node["averageScore"].as_f64().map(|a| a / 10.0),
        popularity: node["popularity"].as_i64(),
    })
}

fn map_nodes(nodes: &Value) -> AlResult<Vec<MediaFields>> {
    arr(nodes).iter().map(map_media).collect()
}

fn list_entry(e: &Value, media_type: &str) -> EntryState {
    EntryState {
        list_entry_id: e["id"].as_i64().unwrap_or_default(),
        status: local_status_from_al(e["status"].as_str().unwrap_or_default(), media_type),
        progress: e["progress"].as_i64().unwrap_or(0),
        repeat_count: e["repeat"].as_i64().unwrap_or(0),
        score: e["score"].as_f64(),
        updated_at: e["updatedAt"].as_i64().filter(|t| *t != 0).map(iso_from_unix),
    }
}

fn person(n: &Value) -> Value {
    json!({"anilist_id": n["id"], "name": str_of(&n["name"]["full"]), "image_url": str_of(&n["image"]["large"])})
}

fn studio(n: &Value) -> Value {
    json!({"anilist_id": n["id"], "name": str_of(&n["name"])})
}

fn work(m: &Value) -> Value {
    json!({
        "anilist_id": m["id"].as_i64(),
        "title": pick_title(&m["title"]),
        "title_english": str_of(&m["title"]["english"]),
        "cover_url": str_of(&m["coverImage"]["large"]),
        "year": m["startDate"]["year"].as_i64(),
    })
}

fn favourites(fav: &Value) -> Value {
    let media = |key: &str| -> Vec<Value> {
        arr(&fav[key]["nodes"])
            .iter()
            .map(|n| json!({
                "anilist_id": n["id"],
                "title": pick_title(&n["title"]),
                "title_english": str_of(&n["title"]["english"]),
                "cover_url": str_of(&n["coverImage"]["large"]),
            }))
            .collect()
    };
    json!({
        "anime": media("anime"),
        "manga": media("manga"),
        "characters": arr(&fav["characters"]["nodes"]).iter().map(person).collect::<Vec<_>>(),
        "staff": arr(&fav["staff"]["nodes"]).iter().map(person).collect::<Vec<_>>(),
        "studios": arr(&fav["studios"]["nodes"]).iter().map(studio).collect::<Vec<_>>(),
    })
}
