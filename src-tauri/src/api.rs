//! The app's API: every route from main.py, same paths, same JSON shapes
//! (PRD §7), same status codes.
//!
//! In the desktop app this router never listens on a port: the `api` Tauri
//! command hands each request straight to it in-process. `--serve` mode binds
//! it to localhost for browser development, like `uvicorn main:app` did.
//!
//! No CORS layer, deliberately: several routes write to the user's real
//! AniList account, and nothing but the app's own page should reach them.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::anilist::{self, AlError, FriendLookup, PickCheck, PickFilters, PickPool, PickSize, PICK_DEPTH};
use crate::db::{self, AnchorError, Auth, MediaFields, Scores};
use crate::sync::{self, SyncError};
use crate::{oauth, Shared};

const FRONTEND_HTML: &str = include_str!("../../frontend/index.html");
const CACHE_TTL: Duration = Duration::from_secs(60);
/// FR-70: AniList's genre and tag lists barely change.
const PICK_OPTIONS_TTL: Duration = Duration::from_secs(24 * 60 * 60);

// ------------------------------------------------------------------- errors ----

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    detail: String,
}

impl ApiError {
    fn new(status: u16, detail: impl Into<String>) -> Self {
        ApiError { status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), detail: detail.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "detail": self.detail }))).into_response()
    }
}

/// AniList failures that escape a route become clear HTTP errors instead of a
/// bare 500. Routes that need a different outcome catch them first.
impl From<AlError> for ApiError {
    fn from(e: AlError) -> Self {
        match e {
            AlError::NotFound(m) => ApiError::new(404, format!("not found on AniList: {m}")),
            AlError::Api { message, .. } => ApiError::new(502, format!("AniList error: {message}")),
            AlError::Network(_) => ApiError::new(502, "could not reach AniList, try again shortly"),
            AlError::Unexpected(m) => {
                log::error!("unexpected AniList data: {m}");
                ApiError::new(500, "Internal Server Error")
            }
        }
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        log::error!("database error: {e}");
        ApiError::new(500, "Internal Server Error")
    }
}

impl From<SyncError> for ApiError {
    fn from(e: SyncError) -> Self {
        match e {
            SyncError::NotConnected => ApiError::new(401, "not connected"),
            SyncError::NotFound(m) => ApiError::new(404, m),
            SyncError::AniList(e) => e.into(),
            SyncError::Db(e) => e.into(),
        }
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn ok(v: Value) -> ApiResult {
    Ok(Json(v))
}

fn to_json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

// --------------------------------------------------------------- extractors ----

/// The single integer path parameter every `{id}` route has. A non-integer
/// is a 422, same as FastAPI.
struct Id(i64);

impl<S: Send + Sync> FromRequestParts<S> for Id {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(raw) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::new(422, "invalid path parameter"))?;
        raw.parse().map(Id).map_err(|_| ApiError::new(422, "path parameter must be an integer"))
    }
}

/// JSON request body; anything malformed or missing a field is a 422.
struct Body<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|_| ApiError::new(422, "could not read request body"))?;
        serde_json::from_slice(&bytes)
            .map(Body)
            .map_err(|e| ApiError::new(422, format!("invalid request body: {e}")))
    }
}

type Params = Query<HashMap<String, String>>;

fn required<'a>(q: &'a Params, name: &str) -> Result<&'a str, ApiError> {
    q.get(name)
        .map(String::as_str)
        .ok_or_else(|| ApiError::new(422, format!("missing required query parameter '{name}'")))
}

fn optional_int(q: &Params, name: &str) -> Result<Option<i64>, ApiError> {
    q.get(name)
        .map(|v| v.parse().map_err(|_| ApiError::new(422, format!("query parameter '{name}' must be an integer"))))
        .transpose()
}

fn check_media_type(value: Option<&str>, field: &str) -> Result<(), ApiError> {
    match value {
        None | Some("anime") | Some("manga") => Ok(()),
        Some(_) => Err(ApiError::new(400, format!("{field} must be 'anime' or 'manga'"))),
    }
}

/// FR-17 / PRD §7: status must be valid for THIS media_type, checked before
/// anything is stored or pushed.
fn validate_status(media_type: &str, status: &str) -> Result<(), ApiError> {
    let valid = anilist::local_statuses(media_type);
    if valid.contains(&status) {
        return Ok(());
    }
    let list = valid.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
    Err(ApiError::new(422, format!("status must be one of [{list}] for {media_type}")))
}

fn is_half_step_in_range(v: f64) -> bool {
    v.is_finite() && (1.0..=10.0).contains(&v) && (v * 2.0).fract() == 0.0
}

/// FR-6: manga audio must be exactly 0.0; anime audio is a normal criterion.
fn validate_scores(media_type: &str, s: &Scores) -> Result<(), ApiError> {
    for (name, v) in [("story", s.story), ("characters", s.characters), ("animation", s.animation), ("enjoyment", s.enjoyment)] {
        if !is_half_step_in_range(v) {
            return Err(ApiError::new(422, format!("{name} must be between 1.0 and 10.0 in 0.5 increments")));
        }
    }
    if media_type == "manga" {
        if s.audio != 0.0 {
            return Err(ApiError::new(422, "audio must be 0.0 for manga"));
        }
    } else if !is_half_step_in_range(s.audio) {
        return Err(ApiError::new(422, "audio must be between 1.0 and 10.0 in 0.5 increments for anime"));
    }
    Ok(())
}

// ------------------------------------------------------------------- router ----

pub fn router(st: Shared) -> Router {
    let protected = Router::new()
        .route("/auth/score-format/confirm", post(confirm_score_format))
        .route("/search", get(search_media))
        .route("/media", post(import_media))
        .route("/media/{id}", get(get_media).delete(delete_media))
        .route("/media/by-anilist/{id}", get(get_media_by_anilist))
        .route("/media/{id}/rating", put(put_rating))
        .route("/media/{id}/anchors", get(get_anchors))
        .route("/media/{id}/status", put(put_status))
        .route("/media/{id}/progress", put(put_progress))
        .route("/media/{id}/repeat", put(put_repeat))
        .route("/sync/pull", post(sync_pull))
        .route("/sync/pending-conflicts", get(sync_pending_conflicts))
        .route("/sync/resolve/{id}", post(sync_resolve))
        .route("/sync/push/{id}", post(sync_push))
        .route("/discover", get(get_discover))
        .route("/feed", get(get_feed))
        .route("/characters/search", get(characters_search))
        .route("/staff/search", get(staff_search))
        .route("/characters/{id}", get(characters_detail))
        .route("/staff/{id}", get(staff_detail))
        .route("/studios/search", get(studios_search))
        .route("/studios/{id}", get(studios_detail))
        .route("/users/search", get(users_search))
        .route("/users/{id}", get(users_detail))
        .route("/favorites/toggle", post(favorites_toggle))
        .route("/favorites", get(favorites_get))
        .route("/media/{id}/community", get(media_community))
        .route("/media/by-anilist/{id}/community", get(media_community_by_anilist))
        .route("/library", get(get_library))
        .route("/insights", get(get_insights))
        .route("/rankings", get(get_rankings))
        .route("/profile/stats", get(profile_stats))
        .route("/friends", get(friends_list).post(friends_add))
        .route("/friends/sync", post(friends_sync))
        .route("/friends/settings", put(friends_settings))
        .route("/friends/{id}", delete(friends_delete))
        .route("/friends/{id}/refresh", post(friends_refresh))
        .route("/friends/{id}/unhide", post(friends_unhide))
        .route("/friends/{id}/compare", get(friends_compare))
        .route("/media/by-anilist/{id}/friend-scores", get(media_friend_scores))
        .route("/pick/options", get(pick_options))
        .route("/pick", post(pick))
        // FR-47: nothing below is usable without a stored token.
        .route_layer(middleware::from_fn_with_state(st.clone(), require_auth));

    Router::new()
        .route("/", get(|| async { Html(FRONTEND_HTML) }))
        .route("/auth/anilist/login", get(auth_login))
        .route("/auth/status", get(auth_status))
        .route("/auth/logout", post(auth_logout))
        .merge(protected)
        .fallback(|| async { ApiError::new(404, "Not Found") })
        .with_state(st)
}

async fn require_auth(State(st): State<Shared>, mut req: Request, next: Next) -> Result<Response, ApiError> {
    let auth = st.db.get_auth()?.ok_or_else(|| ApiError::new(401, "not connected"))?;
    req.extensions_mut().insert(auth);
    Ok(next.run(req).await)
}

// --------------------------------------------------------------------- auth ----

/// Starts the one-shot callback listener and returns AniList's authorize URL.
/// Shared by the browser route below and the desktop `start_login` command.
pub fn begin_login(st: &Shared) -> String {
    oauth::start_listener(st);
    anilist::authorize_url(&st.config.client_id)
}

async fn auth_login(State(st): State<Shared>) -> Redirect {
    Redirect::temporary(&begin_login(&st))
}

async fn auth_status(State(st): State<Shared>) -> ApiResult {
    let auth = st.db.get_auth()?;
    ok(json!({"connected": auth.is_some(), "username": auth.and_then(|a| a.anilist_username)}))
}

async fn auth_logout(State(st): State<Shared>) -> ApiResult {
    st.db.clear_auth()?; // FR-27: token only, library data untouched
    ok(json!({"connected": false}))
}

/// FR-25/FR-28: only called after the frontend's explicit consent dialog.
async fn confirm_score_format(State(st): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    let format = st.anilist.set_score_format(&auth.access_token).await?;
    st.db.update_score_format(Some(&format))?;
    ok(json!({"score_format": format}))
}

// ------------------------------------------------------------- search/media ----

async fn search_media(State(st): State<Shared>, q: Params) -> ApiResult {
    let query = required(&q, "q")?;
    let media_type = required(&q, "type")?;
    if !matches!(media_type, "anime" | "manga") {
        return Err(ApiError::new(400, "type must be 'anime' or 'manga'"));
    }
    let results = st.anilist.search(query, media_type).await?;
    ok(json!({"results": results, "q": query}))
}

#[derive(Deserialize)]
struct MediaImport {
    #[serde(flatten)]
    fields: MediaFields,
    /// Required, like Python's MediaImport (`title: str`): missing or null is
    /// a 422. MediaFields keeps it optional (AniList titles can be null), so
    /// this takes the key and import_media puts it back into `fields`.
    title: String,
    #[serde(default = "planning")]
    status: String,
}

fn planning() -> String {
    "planning".into()
}

/// FR-3: media_type comes from AniList's own record for this anilist_id,
/// never from the client body. A disagreeing client value is logged, ignored.
async fn import_media(State(st): State<Shared>, Body(body): Body<MediaImport>) -> ApiResult {
    let remote = match st.anilist.get_by_id(body.fields.anilist_id).await {
        Ok(r) => r,
        Err(AlError::Network(_)) => return Err(ApiError::new(502, "could not reach AniList to verify this title")),
        Err(e) => return Err(e.into()),
    };
    let Some(remote) = remote else {
        return Err(ApiError::new(404, "anilist_id not found"));
    };
    let mut fields = body.fields;
    fields.title = Some(body.title);
    if fields.media_type != remote.media_type {
        log::warn!(
            "import anilist_id={}: client sent media_type={:?}, AniList says {:?}; using AniList's",
            fields.anilist_id, fields.media_type, remote.media_type
        );
    }
    fields.media_type = remote.media_type;
    // PRD §5: from the same lookup, never from the client (FR-77's tie-break).
    fields.popularity = remote.popularity;
    validate_status(&fields.media_type, &body.status)?;
    let id = st.db.insert_or_update_media(&fields, Some(&body.status))?;
    // FR-24: adding a title sets its status, so it pushes like any other change.
    sync::spawn_push(&st, id);
    ok(json!({"id": id}))
}

async fn get_media(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    let media = st.db.get_media_by_id(id)?.ok_or_else(|| ApiError::new(404, "media not found"))?;
    ok(to_json(&media))
}

async fn get_media_by_anilist(State(st): State<Shared>, Id(anilist_id): Id) -> ApiResult {
    let mut out = if let Some(media) = st.db.get_media_by_anilist_id(anilist_id)? {
        (to_json(&media), true)
    } else {
        let fields = st.anilist.get_by_id(anilist_id).await?.ok_or_else(|| ApiError::new(404, "anilist_id not found"))?;
        (to_json(&fields), false)
    };
    out.0["in_library"] = json!(out.1);
    ok(out.0)
}

async fn put_rating(State(st): State<Shared>, Id(id): Id, Body(scores): Body<Scores>) -> ApiResult {
    let media = st.db.get_media_by_id(id)?.ok_or_else(|| ApiError::new(404, "media not found"))?;
    validate_scores(&media.fields.media_type, &scores)?;
    let (overall, media_type) = st.db.upsert_rating(id, &scores)?.ok_or_else(|| ApiError::new(404, "media not found"))?;
    // FR-24/FR-25: local changes push automatically in the background.
    sync::spawn_push(&st, id);
    ok(json!({"overall": overall, "media_type": media_type}))
}

async fn get_anchors(State(st): State<Shared>, Id(id): Id, q: Params) -> ApiResult {
    let criterion = required(&q, "criterion")?;
    let scope = q.get("scope").map(String::as_str).unwrap_or("genre");
    if !matches!(scope, "genre" | "all") {
        return Err(ApiError::new(400, "scope must be 'genre' or 'all'"));
    }
    match st.db.get_anchors(id, criterion, scope) {
        Ok(Some(anchors)) => ok(json!({"anchors": anchors})),
        Ok(None) => Err(ApiError::new(404, "media not found")),
        Err(AnchorError::InvalidCriterion(c)) => Err(ApiError::new(400, format!("invalid criterion: '{c}'"))),
        Err(AnchorError::Db(e)) => Err(e.into()),
    }
}

#[derive(Deserialize)]
struct StatusIn {
    status: String,
}

async fn put_status(State(st): State<Shared>, Id(id): Id, Body(body): Body<StatusIn>) -> ApiResult {
    let media = st.db.get_media_by_id(id)?.ok_or_else(|| ApiError::new(404, "media not found"))?;
    validate_status(&media.fields.media_type, &body.status)?;
    let status = st.db.update_status(id, &body.status)?.ok_or_else(|| ApiError::new(404, "media not found"))?;
    sync::spawn_push(&st, id);
    ok(json!({"status": status}))
}

#[derive(Deserialize)]
struct ProgressIn {
    progress: i64,
}

async fn put_progress(State(st): State<Shared>, Id(id): Id, Body(body): Body<ProgressIn>) -> ApiResult {
    let progress = st.db.update_progress(id, body.progress)?.ok_or_else(|| ApiError::new(404, "media not found"))?;
    sync::spawn_push(&st, id);
    ok(json!({"progress": progress}))
}

#[derive(Deserialize)]
struct RepeatIn {
    repeat_count: i64,
}

async fn put_repeat(State(st): State<Shared>, Id(id): Id, Body(body): Body<RepeatIn>) -> ApiResult {
    let repeat_count = st.db.update_repeat_count(id, body.repeat_count)?.ok_or_else(|| ApiError::new(404, "media not found"))?;
    sync::spawn_push(&st, id);
    ok(json!({"repeat_count": repeat_count}))
}

// --------------------------------------------------------------------- sync ----

/// FR-79: a friend sync follows every Pull, in the background.
async fn sync_pull(State(st): State<Shared>) -> ApiResult {
    let result = sync::pull(&st).await?;
    sync::sync_friends_later(&st);
    ok(result)
}

async fn sync_pending_conflicts(State(st): State<Shared>) -> ApiResult {
    ok(json!({"conflicts": sync::get_pending_conflicts(&st).await?}))
}

#[derive(Deserialize)]
struct ResolveIn {
    winner: String,
}

async fn sync_resolve(State(st): State<Shared>, Id(id): Id, Body(body): Body<ResolveIn>) -> ApiResult {
    if !matches!(body.winner.as_str(), "local" | "remote") {
        return Err(ApiError::new(400, "winner must be 'local' or 'remote'"));
    }
    ok(sync::resolve(&st, id, &body.winner).await?)
}

async fn sync_push(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    match sync::push_now(&st, id, None).await {
        Ok(v) => ok(v),
        Err(SyncError::NotFound(m)) => Err(ApiError::new(404, m)),
        Err(e) => Err(ApiError::new(502, format!("push failed, queued for retry: {e}"))),
    }
}

// ------------------------------------------------------------ discover / feed ----

async fn get_discover(State(st): State<Shared>, q: Params) -> ApiResult {
    const SECTIONS: [&str; 6] = ["current_season", "popular", "top_ranked", "top_ranked_movies", "trending", "upcoming_season"];
    let section = required(&q, "section")?;
    let media_type = required(&q, "media_type")?;
    if !SECTIONS.contains(&section) {
        let list = SECTIONS.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
        return Err(ApiError::new(400, format!("section must be one of [{list}]")));
    }
    check_media_type(Some(media_type), "media_type")?;
    let key = format!("discover:{section}:{media_type}");
    let results = st
        .cache
        .get_or_fetch(&key, CACHE_TTL, || async { st.anilist.discover(section, media_type).await.map(|r| to_json(&r)) })
        .await?;
    // FR-33: annotate with local_status from the library, null if not imported.
    let mut annotated = Vec::new();
    for mut r in results.as_array().cloned().unwrap_or_default() {
        let local = r["anilist_id"].as_i64().map(|id| st.db.get_media_by_anilist_id(id)).transpose()?.flatten();
        r["local_status"] = json!(local.map(|m| m.status));
        annotated.push(r);
    }
    ok(json!({"count": annotated.len(), "results": annotated}))
}

async fn get_feed(State(st): State<Shared>, Extension(auth): Extension<Auth>, q: Params) -> ApiResult {
    let feed_type = required(&q, "type")?;
    let page = optional_int(&q, "page")?.unwrap_or(1);
    if !matches!(feed_type, "your" | "following" | "global") {
        return Err(ApiError::new(400, "type must be 'your', 'following', or 'global'"));
    }
    let key = format!("feed:{feed_type}:{page}");
    let value = st
        .cache
        .get_or_fetch(&key, CACHE_TTL, || st.anilist.feed(feed_type, page, auth.anilist_user_id, &auth.access_token))
        .await?;
    ok(value)
}

// ------------------------------------------- characters / staff / studios / users ----

async fn characters_search(State(st): State<Shared>, q: Params) -> ApiResult {
    ok(json!({"results": st.anilist.search_characters(required(&q, "q")?).await?}))
}

async fn staff_search(State(st): State<Shared>, q: Params) -> ApiResult {
    ok(json!({"results": st.anilist.search_staff(required(&q, "q")?).await?}))
}

async fn studios_search(State(st): State<Shared>, q: Params) -> ApiResult {
    ok(json!({"results": st.anilist.search_studios(required(&q, "q")?).await?}))
}

async fn users_search(State(st): State<Shared>, q: Params) -> ApiResult {
    ok(json!({"results": st.anilist.search_users(required(&q, "q")?).await?}))
}

fn found(v: Option<Value>, what: &str) -> ApiResult {
    v.map(Json).ok_or_else(|| ApiError::new(404, format!("{what} not found")))
}

async fn characters_detail(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    found(st.anilist.get_character(id).await?, "character")
}

async fn staff_detail(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    found(st.anilist.get_staff(id).await?, "staff")
}

async fn studios_detail(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    found(st.anilist.get_studio(id).await?, "studio")
}

async fn users_detail(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    found(st.anilist.get_user(id).await?, "user")
}

// ---------------------------------------------------------------- favorites ----

#[derive(Deserialize)]
struct FavoriteToggleIn {
    #[serde(rename = "type")]
    fav_type: String,
    anilist_id: i64,
}

async fn favorites_toggle(State(st): State<Shared>, Extension(auth): Extension<Auth>, Body(body): Body<FavoriteToggleIn>) -> ApiResult {
    if !matches!(body.fav_type.as_str(), "anime" | "manga" | "character" | "staff" | "studio") {
        return Err(ApiError::new(400, "invalid favorite type"));
    }
    let favorited = st.anilist.toggle_favorite(&auth.access_token, &body.fav_type, body.anilist_id).await?;
    ok(json!({"favorited": favorited}))
}

/// FR-40: live every call, deliberately not cached.
async fn favorites_get(State(st): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    ok(st.anilist.get_favorites(&auth.access_token).await?)
}

// ---------------------------------------------------------------- community ----

async fn media_community(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    let local = st.db.get_media_by_id(id)?.ok_or_else(|| ApiError::new(404, "media not found"))?;
    // FR-55: query by the row's own stored anilist_id, never by title.
    found(st.anilist.get_community(local.fields.anilist_id).await?, "community data")
}

/// FR-52: Discover-sourced titles have no local row, only their anilist_id.
async fn media_community_by_anilist(State(st): State<Shared>, Id(anilist_id): Id) -> ApiResult {
    found(st.anilist.get_community(anilist_id).await?, "community data")
}

// -------------------------------------------------- library / rankings / profile ----

async fn get_library(State(st): State<Shared>, q: Params) -> ApiResult {
    let media_type = q.get("media_type").map(String::as_str);
    check_media_type(media_type, "media_type")?;
    let status = q.get("status").map(String::as_str).unwrap_or("all");
    let sort = q.get("sort").map(String::as_str).unwrap_or("overall");
    let results = st.db.get_library(media_type, Some(status), sort)?;
    ok(json!({"count": results.len(), "results": results}))
}

async fn delete_media(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    if st.db.get_media_by_id(id)?.is_none() {
        return Err(ApiError::new(404, "media not found"));
    }
    match sync::delete_remote_entry(&st, id).await {
        Err(SyncError::AniList(AlError::Network(_))) => {
            return Err(ApiError::new(502, "failed to delete from AniList; local entry not removed"))
        }
        Err(e) => return Err(e.into()),
        Ok(()) => {}
    }
    ok(json!({"deleted": st.db.delete_media(id)?}))
}

async fn get_insights(State(st): State<Shared>) -> ApiResult {
    ok(st.db.get_insights()?)
}

async fn get_rankings(State(st): State<Shared>, q: Params) -> ApiResult {
    let media_type = q.get("media_type").map(String::as_str);
    check_media_type(media_type, "media_type")?;
    let year = optional_int(&q, "year")?;
    let sort = q.get("sort").map(String::as_str).unwrap_or("overall");
    let results = st.db.get_rankings(
        media_type,
        q.get("genre").map(String::as_str),
        year,
        sort,
        q.get("q").map(String::as_str),
    )?;
    ok(json!({"count": results.len(), "results": results}))
}

/// FR-42: all six stats live from AniList's viewer statistics.
async fn profile_stats(State(st): State<Shared>, Extension(auth): Extension<Auth>) -> ApiResult {
    ok(st.anilist.get_profile_stats(&auth.access_token, auth.anilist_user_id).await?)
}

// ------------------------------------------------------------------ friends ----
// PRD 4.12. Add, sync, refresh and a stale comparison call AniList (never
// with the token); everything else reads the stored lists.

const OWN_ACCOUNT: &str = "That's your own AniList account";

fn private_list(name: &str) -> ApiError {
    ApiError::new(403, format!("{name}'s list is private on AniList"))
}

/// GET /friends' shape, also POST /friends/sync's.
fn friends_json(st: &Shared) -> ApiResult {
    let fetching: HashSet<i64> = st.friend_fetching.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
    let mut friends = st.db.get_friends()?;
    for f in friends.iter_mut().filter(|f| fetching.contains(&f.anilist_user_id)) {
        f.list_status = "fetching".into();
    }
    let (mutual_only, synced_at) = st.db.friend_sync_state()?;
    ok(json!({"friends": friends, "hidden": st.db.get_hidden_friends()?, "synced_at": synced_at, "mutual_only": mutual_only}))
}

async fn friends_list(State(st): State<Shared>) -> ApiResult {
    friends_json(&st)
}

/// FR-79: nothing changes unless the whole following list arrived.
async fn friends_sync(State(st): State<Shared>) -> ApiResult {
    sync::sync_friends(&st).await?;
    friends_json(&st)
}

#[derive(Deserialize)]
struct FriendSettingsIn {
    mutual_only: bool,
}

async fn friends_settings(State(st): State<Shared>, Body(body): Body<FriendSettingsIn>) -> ApiResult {
    st.db.set_mutual_only(body.mutual_only)?;
    ok(json!({"mutual_only": body.mutual_only}))
}

#[derive(Deserialize)]
struct FriendIn {
    username: String,
}

/// FR-70: adding someone who's already a friend (from AniList, hidden or
/// manual) makes them a visible manual friend and refreshes them.
async fn friends_add(State(st): State<Shared>, Extension(auth): Extension<Auth>, Body(body): Body<FriendIn>) -> ApiResult {
    let username = body.username.trim();
    if username.is_empty() {
        return Err(ApiError::new(422, "username is required"));
    }
    if auth.anilist_username.as_deref().is_some_and(|me| me.eq_ignore_ascii_case(username)) {
        return Err(ApiError::new(400, OWN_ACCOUNT));
    }
    fetched_friend(&st, &auth, username, None).await
}

/// FR-71/FR-73: asks AniList even for a friend marked private.
async fn friends_refresh(State(st): State<Shared>, Extension(auth): Extension<Auth>, Id(id): Id) -> ApiResult {
    let friend = st.db.get_friend(id)?.ok_or_else(|| ApiError::new(404, "friend not found"))?;
    fetched_friend(&st, &auth, &friend.name, Some(id)).await
}

/// The lists are stored only once the whole fetch succeeded, so a failed
/// refresh keeps the previous copy and its fetched_at (FR-73).
async fn fetched_friend(st: &Shared, auth: &Auth, name: &str, id: Option<i64>) -> ApiResult {
    match sync::fetch_friend(st, name, id).await? {
        FriendLookup::Found(list) if list.anilist_user_id == auth.anilist_user_id => Err(ApiError::new(400, OWN_ACCOUNT)),
        FriendLookup::Found(list) => found(st.db.get_friend(list.anilist_user_id)?.map(|f| to_json(&f)), "friend"),
        FriendLookup::NoSuchUser => Err(ApiError::new(404, format!("No AniList user named {name}"))),
        FriendLookup::Private(name) => Err(private_list(&name)),
    }
}

/// FR-70: whether you still follow them decides, so a sync follows right
/// away: still followed, they're back; not, the hidden record goes. If
/// AniList can't be reached they stay unhidden, and the next sync settles it.
async fn friends_unhide(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    if !st.db.unhide_friend(id)? {
        return Err(ApiError::new(404, "no hidden friend with that id"));
    }
    if let Err(e) = sync::sync_friends(&st).await {
        log::warn!("friend sync after unhiding {id} failed: {e}");
    }
    let Json(mut out) = friends_json(&st)?;
    out["unhidden"] = json!(st.db.get_friend(id)?.is_some());
    ok(out)
}

/// FR-70: a friend from AniList is hidden rather than deleted.
async fn friends_delete(State(st): State<Shared>, Id(id): Id) -> ApiResult {
    let hidden = st.db.delete_friend(id)?.ok_or_else(|| ApiError::new(404, "friend not found"))?;
    ok(json!({"deleted": true, "hidden": hidden}))
}

/// FR-81: a missing or stale list is fetched first, ahead of the queue; if
/// that fails, the stored copy is used, and without one the fetch's error
/// is the answer.
async fn friends_compare(State(st): State<Shared>, Id(id): Id, q: Params) -> ApiResult {
    let media_type = required(&q, "media_type")?;
    check_media_type(Some(media_type), "media_type")?;
    let friend = st.db.get_friend(id)?.ok_or_else(|| ApiError::new(404, "friend not found"))?;
    let refused = if friend.list_status == "private" {
        Some(private_list(&friend.name))
    } else if db::friend_list_is_stale(friend.fetched_at.as_deref()) {
        match sync::fetch_friend(&st, &friend.name, Some(id)).await {
            Ok(FriendLookup::Found(_)) => None,
            Ok(FriendLookup::Private(name)) => Some(private_list(&name)),
            Ok(FriendLookup::NoSuchUser) => Some(ApiError::new(404, format!("No AniList user named {}", friend.name))),
            Err(e) => Some(e.into()),
        }
    } else {
        None
    };
    if let Some(e) = refused.filter(|_| friend.fetched_at.is_none()) {
        return Err(e);
    }
    found(st.db.compare_with_friend(id, media_type)?, "friend")
}

/// FR-78: works whether or not the title is in the library.
async fn media_friend_scores(State(st): State<Shared>, Id(anilist_id): Id) -> ApiResult {
    ok(json!({"scores": st.db.friend_scores(anilist_id)?}))
}

// ------------------------------------------------------------ pick (FR-70) ----

async fn pick_options(State(st): State<Shared>) -> ApiResult {
    ok(st.pick_cache.get_or_fetch("pick:options", PICK_OPTIONS_TTL, || st.anilist.pick_options()).await?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PickIn {
    source: String,
    roll: f64,
    /// Titles already shown for this source and filters.
    #[serde(default)]
    exclude: Vec<i64>,
    #[serde(default)]
    filters: PickFilters,
    /// The previous response's `size` for this source and filters.
    size: Option<SizeHint>,
}

/// A pool's measured size, and how many titles `exclude` held then.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SizeHint {
    lo: usize,
    hi: usize,
    shown: usize,
}

fn validate_pick(p: &PickIn) -> Result<(), ApiError> {
    let fail = |detail: String| Err(ApiError::new(422, detail));
    if !matches!(p.source.as_str(), "planning" | "not_planning") {
        return fail("source must be 'planning' or 'not_planning'".into());
    }
    if !(0.0..1.0).contains(&p.roll) {
        return fail("roll must be at least 0 and less than 1".into());
    }
    let f = &p.filters;
    for (field, values, allowed) in [("formats", &f.formats, &anilist::PICK_FORMATS[..]), ("statuses", &f.statuses, &anilist::PICK_STATUSES[..])] {
        if let Some(v) = values.iter().find(|v| !allowed.contains(&v.as_str())) {
            return fail(format!("{field} can't include {v:?}; use {allowed:?}"));
        }
    }
    if let Some(season) = f.season.as_deref().filter(|s| !anilist::SEASONS.contains(s)) {
        return fail(format!("season can't be {season:?}; use {:?}", anilist::SEASONS));
    }
    if f.score_min.is_some_and(|s| !(1.0..=10.0).contains(&s)) {
        return fail("score_min must be between 1 and 10".into());
    }
    let ranges = [("popularity", f.popularity_min, f.popularity_max), ("episodes", f.episodes_min, f.episodes_max), ("year", f.year_min, f.year_max)];
    for (name, min, max) in ranges {
        if min.into_iter().chain(max).any(|n| n < 0) {
            return fail(format!("{name} can't be negative"));
        }
        if let (Some(min), Some(max)) = (min, max) {
            if min > max {
                return fail(format!("{name}_min can't be more than {name}_max"));
            }
        }
    }
    if f.year_min.into_iter().chain(f.year_max).any(|y| !(1900..=2100).contains(&y)) {
        return fail("years must be between 1900 and 2100".into());
    }
    Ok(())
}

/// FR-70. Anime only, from either source; nothing here writes anything.
async fn pick(State(st): State<Shared>, Body(body): Body<PickIn>) -> ApiResult {
    validate_pick(&body)?;
    let shown: HashSet<i64> = body.exclude.iter().copied().collect();
    let library = st.db.get_library(Some("anime"), None, "overall")?;
    let pool = if body.source == "planning" {
        let planning: Vec<i64> = library.iter().filter(|m| m.status == "planning").map(|m| m.fields.anilist_id).collect();
        let left: Vec<i64> = planning.iter().copied().filter(|id| !shown.contains(id)).collect();
        if left.is_empty() {
            // No AniList call: there's nothing to ask about.
            return ok(json!({"result": null, "pool": 0, "pool_exact": true, "exhausted": !planning.is_empty(), "size": null, "next_up_for": null, "blocked": false}));
        }
        PickPool::Only(left)
    } else {
        PickPool::Except(library.iter().map(|m| m.fields.anilist_id).chain(shown.iter().copied()).collect())
    };
    // Every title shown since the size was measured came from this pool, so
    // the pool is that many smaller (a capped pool still fills the limit).
    let known = body.size.filter(|s| s.shown <= body.exclude.len()).map(|s| {
        let since = if s.lo >= PICK_DEPTH { 0 } else { body.exclude.len() - s.shown };
        PickSize { lo: s.lo.saturating_sub(since), hi: s.hi.saturating_sub(since) }
    });
    // FR-71: a prequel counts as seen when it's Completed or Rewatching.
    let check = (!body.filters.include_unwatched_sequels).then(|| PickCheck {
        watched: library.iter().filter(|m| matches!(m.status.as_str(), "completed" | "rewatching")).map(|m| m.fields.anilist_id).collect(),
        shown: shown.clone(),
    });
    let picked = st.anilist.pick(&body.filters, &pool, body.roll, known, check.as_ref()).await?;
    // Nothing, with titles already shown: the client clears `exclude` and
    // tries once more, which tells "seen them all" from "nothing usable".
    let exhausted = picked.result.is_none() && !shown.is_empty();
    let size = picked.size.map(|s| json!({"lo": s.lo, "hi": s.hi, "shown": body.exclude.len()}));
    ok(json!({
        "result": picked.result, "pool": picked.pool, "pool_exact": picked.pool_exact, "exhausted": exhausted,
        "size": size, "next_up_for": picked.next_up_for, "blocked": picked.blocked,
    }))
}
