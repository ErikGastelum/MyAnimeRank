//! AniList GraphQL client. Port of anilist.py: every AniList query and
//! mutation in the app goes through the single `graphql()` helper.
//!
//! No MAL/Jikan calls anywhere: this is the ONLY module that talks to an
//! external metadata API.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use serde_json::{json, Map, Value};

use crate::db::{normalize_score, round1, FollowedUser, FriendEntry, FriendList, MediaFields};

pub const ANILIST_URL: &str = "https://graphql.anilist.co";
pub const AUTHORIZE_URL: &str = "https://anilist.co/api/v2/oauth/authorize";

/// HTTP 429: honor Retry-After, at most this many retries per call, and never
/// block longer than this per wait (asking for more means give up now and let
/// the caller / push retry queue try again later).
const MAX_429_RETRIES: usize = 2;
const MAX_RETRY_WAIT_SECONDS: f64 = 30.0;
const DEFAULT_RETRY_AFTER_SECONDS: f64 = 2.0;
/// FR-80: the friend-list queue stays off this long after any 429, or for
/// Retry-After if that's longer (up to an hour).
const QUEUE_PAUSE_AFTER_429: Duration = Duration::from_secs(60);
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

#[derive(Clone)]
pub struct AniList {
    http: reqwest::Client,
    graphql_url: String,
    /// FR-80: requests running right now, and until when a 429 keeps the
    /// friend-list queue off. Shared by every clone.
    in_flight: Arc<AtomicUsize>,
    queue_paused_until: Arc<Mutex<Option<Instant>>>,
}

/// Counts a request as running until dropped, however `graphql()` returns.
struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        InFlight(count)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("HTTP client builds with static settings");
        AniList { http, graphql_url: graphql_url.into(), in_flight: Default::default(), queue_paused_until: Default::default() }
    }

    /// FR-80: the friend-list queue may take a turn: no other AniList call is
    /// running and no 429 came in the last minute.
    pub fn is_quiet(&self) -> bool {
        let paused_until = *self.queue_paused_until.lock().unwrap_or_else(|e| e.into_inner());
        self.in_flight.load(Ordering::SeqCst) == 0 && paused_until.is_none_or(|t| Instant::now() >= t)
    }

    async fn graphql(&self, query: &str, variables: Option<Value>, token: Option<&str>, timeout: u64) -> AlResult<Value> {
        let _running = InFlight::new(&self.in_flight);
        let mut payload = json!({ "query": query });
        if let Some(v) = variables {
            payload["variables"] = v;
        }
        let mut attempt = 0;
        let response = loop {
            let mut req = self
                .http
                .post(&self.graphql_url)
                .timeout(Duration::from_secs(timeout))
                .json(&payload);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            let response = req.send().await?;
            if response.status().as_u16() != 429 {
                break response;
            }
            let wait = retry_after_seconds(&response);
            let pause = Duration::try_from_secs_f64(wait.min(3600.0)).map_or(QUEUE_PAUSE_AFTER_429, |w| w.max(QUEUE_PAUSE_AFTER_429));
            *self.queue_paused_until.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + pause);
            if attempt == MAX_429_RETRIES || wait > MAX_RETRY_WAIT_SECONDS {
                break response; // a shorter wait would only earn another 429
            }
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            attempt += 1;
        };
        unwrap_response(response).await
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
    /// read without the token, every page. Fails as a whole if any page does,
    /// so a sync never acts on part of the list.
    pub async fn get_following(&self, user_id: i64, mutual_only: bool) -> AlResult<Vec<FollowedUser>> {
        let following = self.follow_pages("following", user_id).await?;
        if !mutual_only {
            return Ok(following);
        }
        let followers: HashSet<i64> = self.follow_pages("followers", user_id).await?.into_iter().map(|u| u.anilist_user_id).collect();
        Ok(following.into_iter().filter(|u| followers.contains(&u.anilist_user_id)).collect())
    }

    /// `field` is "following" or "followers".
    async fn follow_pages(&self, field: &str, user_id: i64) -> AlResult<Vec<FollowedUser>> {
        let q = format!(
            "query ($userId: Int!, $page: Int) {{ Page(page: $page, perPage: 50) {{ pageInfo {{ hasNextPage }}
               {field}(userId: $userId, sort: [ID]) {{ id name avatar {{ large }} }} }} }}"
        );
        let mut users = Vec::new();
        for page in 1..=MAX_FOLLOW_PAGES {
            let data = self.graphql(&q, Some(json!({"userId": user_id, "page": page})), None, 15).await?;
            users.extend(arr(&data["Page"][field]).iter().filter_map(|u| {
                Some(FollowedUser { anilist_user_id: u["id"].as_i64()?, name: str_of(&u["name"])?, avatar_url: str_of(&u["avatar"]["large"]) })
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
}

// ------------------------------------------------------------------ helpers ----

const MEDIA_FIELDS: &str = "id type title { romaji english native } coverImage { large } genres tags { name }
    studios(isMain: true) { nodes { name } } seasonYear season format episodes chapters volumes duration
    description(asHtml: false) averageScore";

const FAVOURITES_FIELDS: &str = "favourites {
    anime { nodes { id title { romaji english native } coverImage { large } } }
    manga { nodes { id title { romaji english native } coverImage { large } } }
    characters { nodes { id name { full } image { large } } }
    staff { nodes { id name { full } image { large } } }
    studios { nodes { id name } } }";

/// FR-72: scores in `$format` (see get_friend_list). A bare `score` isn't
/// reliably in any one format, so the format is always named.
const FRIEND_LIST_FIELDS: &str = "lists { entries { status score(format: $format)
    media { id type title { romaji english native } coverImage { large } genres } } }";

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

fn season_year(offset: i32) -> (&'static str, i32) {
    const SEASONS: [&str; 4] = ["WINTER", "SPRING", "SUMMER", "FALL"];
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
