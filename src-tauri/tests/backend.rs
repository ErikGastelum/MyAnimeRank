//! Backend tests, ported from the old Python suite and extended. Every test
//! gets its own in-memory (or temp-file) database, and AniList is always a
//! local fake server: nothing here touches library.db or the network.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use myanimerank_lib::anilist::{self, AlError, AniList, EntryState};
use myanimerank_lib::config::{self, Config};
use myanimerank_lib::db::{self, Db, FriendEntry, FriendList, MediaFields, Scores, SyncOutcome};
use myanimerank_lib::{api, oauth, sync, AppState, Shared};
use serde_json::{json, Value};
use tower::ServiceExt;

// ---------------------------------------------------------------- fake AniList ----

type Reply = (u16, Value, Option<&'static str>);
/// (query, variables, Authorization header) -> reply.
type Handler = Arc<dyn Fn(&str, &Value, Option<&str>) -> Reply + Send + Sync>;

struct Fake {
    url: String,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    auths: Arc<Mutex<Vec<Option<String>>>>,
    times: Arc<Mutex<Vec<Instant>>>,
}

impl Fake {
    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
    /// When each request arrived, in order.
    fn times(&self) -> Vec<Instant> {
        self.times.lock().unwrap().clone()
    }
    fn calls_containing(&self, needle: &str) -> Vec<Value> {
        self.calls().into_iter().filter(|(q, _)| q.contains(needle)).map(|(_, v)| v).collect()
    }
    /// Each request's Authorization header, in order.
    fn auths(&self) -> Vec<Option<String>> {
        self.auths.lock().unwrap().clone()
    }
}

async fn fake_anilist(handler: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static) -> Fake {
    fake_anilist_seeing_auth(move |q, v, _| handler(q, v)).await
}

/// `fake_anilist` for a handler that answers by who's asking, as AniList
/// does for viewer-relative fields like `isFollower`.
async fn fake_anilist_seeing_auth(handler: impl Fn(&str, &Value, Option<&str>) -> Reply + Send + Sync + 'static) -> Fake {
    let handler: Handler = Arc::new(handler);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let auths = Arc::new(Mutex::new(Vec::new()));
    let times = Arc::new(Mutex::new(Vec::new()));
    let (calls2, auths2, times2) = (calls.clone(), auths.clone(), times.clone());
    let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap, body: String| {
        let handler = handler.clone();
        let calls = calls2.clone();
        let auths = auths2.clone();
        let times = times2.clone();
        async move {
            times.lock().unwrap().push(Instant::now());
            let auth = headers.get("authorization").and_then(|h| h.to_str().ok()).map(str::to_string);
            auths.lock().unwrap().push(auth.clone());
            let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let query = body["query"].as_str().unwrap_or_default().to_string();
            let vars = body["variables"].clone();
            calls.lock().unwrap().push((query.clone(), vars.clone()));
            let (status, json, retry_after) = handler(&query, &vars, auth.as_deref());
            let mut resp = (axum::http::StatusCode::from_u16(status).unwrap(), axum::Json(json)).into_response();
            if let Some(ra) = retry_after {
                resp.headers_mut().insert("Retry-After", ra.parse().unwrap());
            }
            resp
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Fake { url, calls, auths, times }
}

fn state_with(fake: &Fake) -> Shared {
    let config = Config { client_id: "1".into() };
    AppState::new(Db::open_in_memory().unwrap(), AniList::with_url(&fake.url), config)
}

fn login(st: &Shared) {
    st.db.save_auth("test-token", None, 1, "tester", Some("POINT_10_DECIMAL")).unwrap();
}

async fn offline_state() -> Shared {
    let fake = fake_anilist(|_, _| (500, json!({}), None)).await;
    state_with(&fake)
}

async fn call(st: &Shared, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = api::router(st.clone()).oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Background pushes (FR-24) aren't awaited by the request: poll until
/// `done`, for up to two seconds.
async fn wait_for(done: impl Fn() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn media_node(id: i64, al_type: &str) -> Value {
    json!({
        "id": id, "type": al_type,
        "title": {"romaji": format!("Title {id}"), "english": null, "native": null},
        "coverImage": {"large": null}, "genres": ["Action"], "tags": [], "studios": {"nodes": []},
        "seasonYear": 2020, "season": null, "format": "TV",
        "episodes": if al_type == "ANIME" { json!(12) } else { Value::Null },
        "chapters": if al_type == "MANGA" { json!(100) } else { Value::Null },
        "volumes": null, "duration": null, "description": null, "averageScore": 75,
    })
}

/// AniList's `Media(id:)` lookup: 7 and 11 are manga, 9 is an anime, and any
/// other id gets AniList's real 404 shape.
fn media_lookup(q: &str, v: &Value) -> Reply {
    let al_type = match (q.contains("Media(id"), v["id"].as_i64()) {
        (true, Some(7 | 11)) => "MANGA",
        (true, Some(9)) => "ANIME",
        _ => return (404, json!({"data": {"Media": null}, "errors": [{"message": "Not Found.", "status": 404}]}), None),
    };
    (200, json!({"data": {"Media": media_node(v["id"].as_i64().unwrap(), al_type)}}), None)
}

fn fields(anilist_id: i64, media_type: &str) -> MediaFields {
    MediaFields {
        anilist_id,
        media_type: media_type.into(),
        title: Some(format!("Title {anilist_id}")),
        genres: vec!["Action".into()],
        episodes: (media_type == "anime").then_some(12),
        chapters: (media_type == "manga").then_some(100),
        ..Default::default()
    }
}

fn scores(v: f64) -> Scores {
    Scores { story: v, characters: v, animation: v, audio: v, enjoyment: v }
}

// ------------------------------------------------------------------ ratings ----

#[test]
fn overall_is_an_unweighted_average_of_the_active_criteria() {
    let s = Scores { story: 8.0, characters: 7.0, animation: 9.0, audio: 6.0, enjoyment: 10.0 };
    assert_eq!(db::compute_overall("anime", &s), 8.0);
    // Manga: audio excluded entirely, not averaged in as a zero.
    assert_eq!(db::compute_overall("manga", &s), 8.5);
}

#[test]
fn overall_rounds_half_to_even_like_python() {
    // (7 + 7 + 7.5 + 7.5) / 4 = 7.25 -> Python's round() gives 7.2
    let s = Scores { story: 7.0, characters: 7.0, animation: 7.5, audio: 0.0, enjoyment: 7.5 };
    assert_eq!(db::compute_overall("manga", &s), 7.2);
    // (8 + 8 + 8 + 8.5) / 4 = 8.125 -> 8.1
    let s = Scores { story: 8.0, characters: 8.0, animation: 8.0, audio: 10.0, enjoyment: 8.5 };
    assert_eq!(db::compute_overall("manga", &s), 8.1);
}

#[test]
fn manga_rating_forces_stored_audio_to_zero() {
    let db = Db::open_in_memory().unwrap();
    let id = db.insert_or_update_media(&fields(1, "manga"), None).unwrap();
    assert_eq!(db.upsert_rating(id, &scores(8.0)).unwrap(), Some((8.0, "manga".to_string())));
    assert_eq!(db.get_media_by_id(id).unwrap().unwrap().rating.unwrap().audio, 0.0);
}

#[test]
fn duplicate_import_updates_in_place_and_never_changes_media_type() {
    let db = Db::open_in_memory().unwrap();
    let id = db.insert_or_update_media(&fields(5, "anime"), Some("watching")).unwrap();
    let mut again = fields(5, "manga");
    again.title = Some("Renamed".into());
    assert_eq!(db.insert_or_update_media(&again, Some("planning")).unwrap(), id);
    let m = db.get_media_by_id(id).unwrap().unwrap();
    assert_eq!(m.fields.media_type, "anime");
    assert_eq!(m.fields.title.as_deref(), Some("Renamed"));
}

#[test]
fn anchors_never_cross_media_types_and_respect_genre_scope() {
    let db = Db::open_in_memory().unwrap();
    let mut subject = fields(1, "anime");
    subject.genres = vec!["Action".into(), "Drama".into()];
    let target = db.insert_or_update_media(&subject, None).unwrap();
    let mut shares_one = fields(2, "anime");
    shares_one.genres = vec!["Drama".into()];
    let same = db.insert_or_update_media(&shares_one, None).unwrap();
    let manga = db.insert_or_update_media(&fields(3, "manga"), None).unwrap();
    let mut other_genre = fields(4, "anime");
    other_genre.genres = vec!["Romance".into()];
    let other = db.insert_or_update_media(&other_genre, None).unwrap();
    for id in [same, manga, other] {
        db.upsert_rating(id, &scores(7.0)).unwrap();
    }
    let ids = |subject: i64, scope: &str| -> Vec<i64> {
        let mut ids: Vec<i64> =
            db.get_anchors(subject, "story", scope).unwrap().unwrap().iter().map(|a| a["id"].as_i64().unwrap()).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(target, "genre"), vec![same]);
    assert_eq!(ids(target, "all"), vec![same, other]);
    // The other way round: a manga subject only ever sees manga.
    let manga_subject = db.insert_or_update_media(&fields(5, "manga"), None).unwrap();
    assert_eq!(ids(manga_subject, "all"), vec![manga]);
    assert!(matches!(db.get_anchors(target, "overall; DROP TABLE media", "all"), Err(db::AnchorError::InvalidCriterion(_))));
}

#[tokio::test]
async fn anchors_skip_legacy_rows_with_a_null_criterion() {
    let fake = fake_anilist(|_, _| (500, json!({}), None)).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let st = AppState::new(Db::open(&path).unwrap(), AniList::with_url(&fake.url), Config { client_id: "1".into() });
    login(&st);
    let subject = st.db.insert_or_update_media(&fields(1, "anime"), None).unwrap();
    let rated = st.db.insert_or_update_media(&fields(2, "anime"), None).unwrap();
    let legacy = st.db.insert_or_update_media(&fields(3, "anime"), None).unwrap();
    st.db.upsert_rating(rated, &scores(7.0)).unwrap();
    st.db.upsert_rating(legacy, &scores(8.0)).unwrap();
    // The schema allows a NULL rating.animation, and old rows can have one.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE rating SET animation = NULL WHERE media_id = ?", [legacy])
        .unwrap();

    let (s, b) = call(&st, "GET", &format!("/media/{subject}/anchors?criterion=animation&scope=all"), None).await;
    assert_eq!(s, 200);
    let ids: Vec<i64> = b["anchors"].as_array().unwrap().iter().map(|a| a["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![rated]);
    // The legacy row itself still loads.
    let (s, b) = call(&st, "GET", &format!("/media/{legacy}"), None).await;
    assert_eq!((s, &b["rating"]["animation"]), (200, &Value::Null));
}

// ----------------------------------------------------------------- progress ----

#[test]
fn reaching_total_auto_completes_and_counts_a_rewatch_once() {
    let db = Db::open_in_memory().unwrap();
    let total = |media_type: &str| -> i64 { if media_type == "anime" { 12 } else { 100 } };
    for (anilist_id, media_type, status) in [(1, "anime", "watching"), (2, "manga", "reading")] {
        let id = db.insert_or_update_media(&fields(anilist_id, media_type), Some(status)).unwrap();
        assert_eq!(db.update_progress(id, 999).unwrap(), Some(total(media_type))); // clamped to total
        let m = db.get_media_by_id(id).unwrap().unwrap();
        assert_eq!((m.status.as_str(), m.repeat_count), ("completed", 0), "{media_type}");
    }
    for (anilist_id, media_type, status) in [(3, "anime", "rewatching"), (4, "manga", "rereading")] {
        let id = db.insert_or_update_media(&fields(anilist_id, media_type), Some(status)).unwrap();
        db.update_repeat_count(id, 2).unwrap();
        db.update_progress(id, total(media_type)).unwrap();
        db.update_progress(id, total(media_type) + 1).unwrap(); // +1 again at the cap: no double count
        let m = db.get_media_by_id(id).unwrap().unwrap();
        assert_eq!((m.status.as_str(), m.progress, m.repeat_count), ("completed", total(media_type), 3), "{media_type}");
    }
}

#[test]
fn reaching_total_leaves_paused_dropped_and_planning_alone() {
    let db = Db::open_in_memory().unwrap();
    for (anilist_id, status) in [(1, "paused"), (2, "dropped"), (3, "planning")] {
        let id = db.insert_or_update_media(&fields(anilist_id, "anime"), Some(status)).unwrap();
        db.update_progress(id, 12).unwrap();
        assert_eq!(db.get_media_by_id(id).unwrap().unwrap().status, status);
    }
}

#[test]
fn null_total_never_clamps_or_auto_completes_and_progress_floors_at_zero() {
    let db = Db::open_in_memory().unwrap();
    let mut f = fields(1, "anime");
    f.episodes = None;
    let id = db.insert_or_update_media(&f, Some("watching")).unwrap();
    assert_eq!(db.update_progress(id, 500).unwrap(), Some(500));
    assert_eq!(db.get_media_by_id(id).unwrap().unwrap().status, "watching");
    assert_eq!(db.update_progress(id, -3).unwrap(), Some(0));
}

// ------------------------------------------------------------ sync helpers ----

#[test]
fn detect_conflict_matches_the_python_rules() {
    let t = |m: i64| format!("2026-01-01T00:{m:02}:00+00:00");
    use SyncOutcome as O;
    let cases = [
        (Some(t(5)), Some(t(5)), None, O::Conflict), // never synced: no baseline, always ask
        (None, None, None, O::Conflict),
        (Some(t(5)), Some(t(5)), Some(t(2)), O::Conflict), // both changed since the sync
        (Some(t(1)), Some(t(5)), Some(t(2)), O::Remote),
        (None, Some(t(5)), Some(t(2)), O::Remote),
        (Some(t(5)), Some(t(1)), Some(t(2)), O::Local),
        (Some(t(5)), None, Some(t(2)), O::Local),
        (Some(t(1)), Some(t(1)), Some(t(2)), O::None),
        (Some(t(2)), Some(t(2)), Some(t(2)), O::None), // equal to the sync time is not "changed"
        (None, None, Some(t(2)), O::None),
    ];
    for (l, r, s, expected) in cases {
        assert_eq!(db::detect_conflict(l.as_deref(), r.as_deref(), s.as_deref()), expected, "{l:?} {r:?} {s:?}");
    }
    // Python-written micro-second stamps still compare against AniList's.
    assert_eq!(
        db::detect_conflict(Some("2026-01-01T00:02:00.500000+00:00"), Some("2026-01-01T00:02:00+00:00"), Some("2026-01-01T00:02:00.100000+00:00")),
        O::Local
    );
}

#[test]
fn apply_remote_entry_seeds_snapped_criteria_but_keeps_exact_overall() {
    let db = Db::open_in_memory().unwrap();
    let mut anilist_id = 0;
    for media_type in ["anime", "manga"] {
        for (score, snapped) in [(7.3, 7.5), (7.2, 7.0), (7.25, 7.5), (7.75, 8.0), (9.9, 10.0), (0.2, 1.0)] {
            anilist_id += 1;
            let id = db.insert_or_update_media(&fields(anilist_id, media_type), None).unwrap();
            db.apply_remote_entry(id, "completed", 12, 0, Some(score), "2026-01-01T00:00:00+00:00").unwrap();
            let r = db.get_media_by_id(id).unwrap().unwrap().rating.unwrap();
            let audio = if media_type == "anime" { snapped } else { 0.0 };
            assert_eq!(
                (r.story, r.characters, r.animation, r.audio, r.enjoyment),
                (snapped, snapped, Some(snapped), audio, snapped),
                "{media_type} {score}"
            );
            // Overall keeps AniList's exact score, so the next push doesn't rewrite it.
            assert_eq!(r.overall, score, "{media_type} {score}");
        }
    }
}

#[test]
fn apply_remote_entry_overwrites_overall_but_keeps_the_criteria() {
    let db = Db::open_in_memory().unwrap();
    let id = db.insert_or_update_media(&fields(1, "anime"), Some("watching")).unwrap();
    db.upsert_rating(id, &Scores { story: 6.0, characters: 7.0, animation: 8.0, audio: 9.0, enjoyment: 10.0 }).unwrap();
    let ts = "2026-01-01T00:20:00+00:00";
    db.apply_remote_entry(id, "completed", 12, 1, Some(9.5), ts).unwrap();
    let m = db.get_media_by_id(id).unwrap().unwrap();
    assert_eq!((m.status.as_str(), m.progress, m.repeat_count), ("completed", 12, 1));
    assert_eq!((m.updated_at.as_deref(), m.last_synced_at.as_deref()), (Some(ts), Some(ts)));
    let r = m.rating.unwrap();
    assert_eq!((r.story, r.characters, r.animation, r.audio, r.enjoyment, r.overall), (6.0, 7.0, Some(8.0), 9.0, 10.0, 9.5));
}

#[test]
fn apply_remote_entry_ignores_an_empty_score() {
    // AniList's 0 (or no score at all) means "not scored": no rating is
    // invented, and an existing one is left alone.
    let db = Db::open_in_memory().unwrap();
    for (n, score) in [Some(0.0), None].into_iter().enumerate() {
        let unrated = db.insert_or_update_media(&fields(10 + n as i64, "anime"), None).unwrap();
        let rated = db.insert_or_update_media(&fields(20 + n as i64, "anime"), None).unwrap();
        db.upsert_rating(rated, &scores(6.5)).unwrap();
        db.apply_remote_entry(unrated, "watching", 4, 0, score, "2026-01-01T00:00:00+00:00").unwrap();
        db.apply_remote_entry(rated, "watching", 4, 0, score, "2026-01-01T00:00:00+00:00").unwrap();
        assert!(db.get_media_by_id(unrated).unwrap().unwrap().rating.is_none(), "{score:?}");
        assert_eq!(db.get_media_by_id(rated).unwrap().unwrap().rating.unwrap().overall, 6.5, "{score:?}");
    }
}

#[test]
fn opens_an_older_library_db_and_adds_missing_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE media (id INTEGER PRIMARY KEY AUTOINCREMENT, anilist_id INTEGER UNIQUE NOT NULL,
               media_type TEXT NOT NULL, title TEXT, title_native TEXT, title_english TEXT, cover_url TEXT,
               genres TEXT, tags TEXT, producers TEXT, runtime_minutes INTEGER, year INTEGER, season TEXT,
               format TEXT, episodes INTEGER, chapters INTEGER, volumes INTEGER, synopsis TEXT, avg_score REAL,
               status TEXT NOT NULL DEFAULT 'planning', progress INTEGER NOT NULL DEFAULT 0,
               repeat_count INTEGER NOT NULL DEFAULT 0, anilist_list_entry_id INTEGER);
             INSERT INTO media (anilist_id, media_type, title, genres) VALUES (9, 'manga', 'Old', '[\"Drama\"]');",
        )
        .unwrap();
    }
    let db = Db::open(&path).unwrap();
    let m = db.get_media_by_anilist_id(9).unwrap().unwrap();
    assert_eq!((m.fields.media_type.as_str(), m.fields.genres.clone(), m.pending_conflict), ("manga", vec!["Drama".to_string()], false));
}

#[test]
fn opening_a_database_in_a_missing_folder_creates_the_folder() {
    // e.g. MYANIMERANK_DB under %TEMP% after Windows cleaned it out
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cleaned").join("out").join("library.db");
    Db::open(&path).unwrap();
    assert!(path.is_file());
}

// --------------------------------------------------------------------- routes ----

/// Reachable before login by design (FR-47): the page itself, the login flow,
/// the status check the page polls, and logout (which only clears the token).
const PUBLIC: [(&str, &str); 4] = [("GET", "/"), ("GET", "/auth/anilist/login"), ("GET", "/auth/status"), ("POST", "/auth/logout")];

/// Every (method, path) api.rs declares, read from its source, so a route
/// added later is covered without editing a list here.
fn declared_routes() -> Vec<(String, String)> {
    let src = include_str!("../src/api.rs");
    let mut routes = Vec::new();
    for (start, _) in src.match_indices(".route(\"") {
        // The .route(...) call's own text, up to its matching paren.
        let rest = &src[start + ".route(".len()..];
        let mut depth = 1;
        let end = rest
            .char_indices()
            .find_map(|(i, c)| {
                depth += match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                };
                (depth == 0).then_some(i)
            })
            .expect("unbalanced .route( call in api.rs");
        let args = &rest[..end];
        let path = args[1..].split('"').next().unwrap();
        for method in ["get", "post", "put", "patch", "delete"] {
            let declared = args
                .match_indices(&format!("{method}("))
                .any(|(i, _)| !args[..i].ends_with(|c: char| c.is_alphanumeric() || c == '_'));
            if declared {
                routes.push((method.to_uppercase(), path.to_string()));
            }
        }
    }
    routes
}

#[tokio::test]
async fn every_route_but_the_login_flow_is_401_without_login() {
    let routes = declared_routes();
    for (method, path) in PUBLIC {
        assert!(routes.contains(&(method.to_string(), path.to_string())), "{method} {path} not found in api.rs");
    }
    let protected: Vec<_> = routes.iter().filter(|(m, p)| !PUBLIC.contains(&(m.as_str(), p.as_str()))).collect();
    assert!(protected.len() > 30, "only found {protected:?}");
    let st = offline_state().await;
    for (method, path) in protected {
        // Any path parameter will do: the auth check runs before it's parsed.
        let url = path.split('/').map(|s| if s.starts_with('{') { "1" } else { s }).collect::<Vec<_>>().join("/");
        let (status, body) = call(&st, method, &url, None).await;
        assert_eq!((status, body["detail"].as_str()), (401, Some("not connected")), "{method} {path}");
    }
}

#[tokio::test]
async fn public_routes_work_without_login() {
    let st = offline_state().await;
    assert_eq!(call(&st, "GET", "/auth/status", None).await, (200, json!({"connected": false, "username": null})));
    assert_eq!(call(&st, "POST", "/auth/logout", None).await, (200, json!({"connected": false})));
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let resp = api::router(st.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
    let page = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&page).to_lowercase().contains("<html"));
}

#[tokio::test]
async fn api_sends_no_cors_headers_to_other_origins() {
    // Several routes write to the user's real AniList account: no other site
    // may be cleared to call them (and read the reply) from a browser.
    let st = offline_state().await;
    login(&st);
    let simple = Request::builder().uri("/auth/status").header("origin", "https://evil.example").body(Body::empty()).unwrap();
    let preflight = Request::builder()
        .method("OPTIONS")
        .uri("/media/1")
        .header("origin", "https://evil.example")
        .header("access-control-request-method", "DELETE")
        .body(Body::empty())
        .unwrap();
    for req in [simple, preflight] {
        let resp = api::router(st.clone()).oneshot(req).await.unwrap();
        assert!(resp.headers().get("access-control-allow-origin").is_none());
    }
}

#[tokio::test]
async fn rating_route_validates_scores_per_media_type_and_computes_overall() {
    let st = offline_state().await;
    login(&st);
    let manga = st.db.insert_or_update_media(&fields(1, "manga"), None).unwrap();
    let anime = st.db.insert_or_update_media(&fields(2, "anime"), None).unwrap();
    let body = |audio: f64| json!({"story": 8.0, "characters": 8.0, "animation": 8.0, "audio": audio, "enjoyment": 9.0, "overall": 1.0});

    let (s, b) = call(&st, "PUT", &format!("/media/{manga}/rating"), Some(body(0.0))).await;
    assert_eq!((s, b), (200, json!({"overall": 8.2, "media_type": "manga"}))); // 8.25, half-even like Python; client overall ignored
    // FR-6: manga audio must be exactly 0.0; anime audio is a normal criterion.
    let audio_cases = [
        (manga, 0.5, 422), (manga, 8.0, 422),
        (anime, 1.0, 200), (anime, 7.5, 200), (anime, 10.0, 200),
        (anime, 0.0, 422), (anime, 0.5, 422), (anime, 7.3, 422), (anime, 10.5, 422),
    ];
    for (id, audio, expected) in audio_cases {
        assert_eq!(call(&st, "PUT", &format!("/media/{id}/rating"), Some(body(audio))).await.0, expected, "media {id} audio {audio}");
    }
    // Every criterion: 1.0 to 10.0 in 0.5 steps.
    for criterion in ["story", "characters", "animation", "enjoyment"] {
        for bad in [0.5, 10.5, 1.1, 7.3, 9.9] {
            let mut b = body(8.0);
            b[criterion] = json!(bad);
            assert_eq!(call(&st, "PUT", &format!("/media/{anime}/rating"), Some(b)).await.0, 422, "{criterion} {bad}");
        }
    }
    assert_eq!(call(&st, "PUT", "/media/999/rating", Some(body(8.0))).await.0, 404);
    assert_eq!(call(&st, "PUT", "/media/abc/rating", Some(body(8.0))).await.0, 422);
}

const ANIME_STATUSES: [&str; 6] = ["watching", "rewatching", "completed", "paused", "dropped", "planning"];
const MANGA_STATUSES: [&str; 6] = ["reading", "rereading", "completed", "paused", "dropped", "planning"];

#[tokio::test]
async fn status_must_be_valid_for_the_titles_media_type_and_only_valid_changes_push() {
    let st = offline_state().await;
    login(&st);
    let mut next_anilist_id = 0;
    let mut add = |media_type: &str| -> i64 {
        next_anilist_id += 1;
        st.db.insert_or_update_media(&fields(next_anilist_id, media_type), None).unwrap()
    };
    let invalid = [
        ("anime", "reading"), ("anime", "rereading"),    // manga-only wording
        ("manga", "watching"), ("manga", "rewatching"),  // anime-only wording
        ("anime", "finished"), ("anime", ""), ("anime", "WATCHING"), ("manga", "CURRENT"),
    ];
    for (media_type, status) in invalid {
        let id = add(media_type);
        let (s, b) = call(&st, "PUT", &format!("/media/{id}/status"), Some(json!({"status": status}))).await;
        assert_eq!(s, 422, "{media_type} {status:?}");
        let listed = if media_type == "anime" { "'watching'" } else { "'reading'" };
        assert!(b["detail"].as_str().unwrap().contains(listed), "{b}");
        assert_eq!(st.db.get_media_by_id(id).unwrap().unwrap().status, "planning");
    }
    assert_eq!(call(&st, "PUT", "/media/999/status", Some(json!({"status": "watching"}))).await.0, 404);

    let mut accepted = Vec::new();
    for (media_type, statuses) in [("anime", ANIME_STATUSES), ("manga", MANGA_STATUSES)] {
        for status in statuses {
            let id = add(media_type);
            assert_eq!(call(&st, "PUT", &format!("/media/{id}/status"), Some(json!({"status": status}))).await, (200, json!({"status": status})));
            assert_eq!(st.db.get_media_by_id(id).unwrap().unwrap().status, status);
            accepted.push(id);
        }
    }
    // Only accepted changes push (FR-24). AniList is unreachable here, so
    // every push that runs lands in the retry queue (FR-26).
    wait_for(|| st.push_queue.lock().unwrap().len() >= accepted.len()).await;
    assert_eq!(st.push_queue.lock().unwrap().iter().copied().collect::<Vec<_>>(), accepted);

    let (mut anime, mut manga) = (ANIME_STATUSES.to_vec(), MANGA_STATUSES.to_vec());
    anime.sort_unstable();
    manga.sort_unstable();
    assert_eq!((anilist::local_statuses("anime"), anilist::local_statuses("manga")), (anime, manga));
}

#[tokio::test]
async fn import_takes_media_type_from_anilist_not_the_client() {
    let fake = fake_anilist(media_lookup).await;
    let st = state_with(&fake);
    login(&st);
    for (anilist_id, client_type, expected) in [(7, "anime", "manga"), (9, "manga", "anime"), (11, "manga", "manga")] {
        let (s, b) = call(&st, "POST", "/media", Some(json!({"anilist_id": anilist_id, "media_type": client_type, "title": "x"}))).await;
        assert_eq!(s, 200);
        let m = st.db.get_media_by_id(b["id"].as_i64().unwrap()).unwrap().unwrap();
        assert_eq!(m.fields.media_type, expected, "client said {client_type}");
    }
    // One AniList lookup per import, by the id the client sent.
    let looked_up: Vec<i64> = fake.calls_containing("Media(id").iter().map(|v| v["id"].as_i64().unwrap()).collect();
    assert_eq!(looked_up, vec![7, 9, 11]);

    let (s, b) = call(&st, "POST", "/media", Some(json!({"anilist_id": 8, "media_type": "anime", "title": "x"}))).await;
    assert_eq!((s, b["detail"].as_str()), (404, Some("anilist_id not found")));
    assert!(st.db.get_media_by_anilist_id(8).unwrap().is_none());
    let (s, _) = call(&st, "GET", "/media/by-anilist/8", None).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn import_stores_anilists_popularity_not_the_clients() {
    // PRD §5: popularity comes from AniList at import, the same request that
    // checks media_type, so a title added from Search can break FR-77 ties.
    let fake = fake_anilist(|_, _| {
        let mut node = media_node(7, "ANIME");
        node["popularity"] = json!(4321);
        (200, json!({"data": {"Media": node}}), None)
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    let (s, b) = call(&st, "POST", "/media", Some(json!({"anilist_id": 7, "media_type": "anime", "title": "x", "popularity": 1}))).await;
    assert_eq!(s, 200);
    let m = st.db.get_media_by_id(b["id"].as_i64().unwrap()).unwrap().unwrap();
    assert_eq!(m.fields.popularity, Some(4321));
    assert_eq!(fake.calls().len(), 1, "no extra request for it");
}

#[tokio::test]
async fn import_checks_status_against_anilists_media_type_not_the_clients() {
    let fake = fake_anilist(media_lookup).await;
    let st = state_with(&fake);
    login(&st);
    // The client claims "anime" every time; on AniList 7 is a manga, 9 an anime.
    for (anilist_id, status) in [(7, "watching"), (9, "reading"), (9, "done")] {
        let body = json!({"anilist_id": anilist_id, "media_type": "anime", "title": "T", "status": status});
        assert_eq!(call(&st, "POST", "/media", Some(body)).await.0, 422, "{anilist_id} {status}");
        assert!(st.db.get_media_by_anilist_id(anilist_id).unwrap().is_none());
    }
    for (anilist_id, status) in [(7, "reading"), (9, "rewatching")] {
        let body = json!({"anilist_id": anilist_id, "media_type": "anime", "title": "T", "status": status});
        let (s, b) = call(&st, "POST", "/media", Some(body)).await;
        assert_eq!(s, 200);
        assert_eq!(st.db.get_media_by_id(b["id"].as_i64().unwrap()).unwrap().unwrap().status, status);
    }
}

#[tokio::test]
async fn import_requires_a_title_like_pythons_media_import() {
    let fake = fake_anilist(|_, _| (200, json!({"data": {"Media": media_node(7, "ANIME")}}), None)).await;
    let st = state_with(&fake);
    login(&st);
    for body in [json!({"anilist_id": 7, "media_type": "anime"}), json!({"anilist_id": 7, "media_type": "anime", "title": null})] {
        let (s, _) = call(&st, "POST", "/media", Some(body.clone())).await;
        assert_eq!(s, 422, "{body}");
    }
    assert!(fake.calls().is_empty());
    assert!(st.db.get_media_by_anilist_id(7).unwrap().is_none());

    let (s, b) = call(&st, "POST", "/media", Some(json!({"anilist_id": 7, "media_type": "anime", "title": "x"}))).await;
    assert_eq!(s, 200);
    let m = st.db.get_media_by_id(b["id"].as_i64().unwrap()).unwrap().unwrap();
    assert_eq!(m.fields.title.as_deref(), Some("x"));
}

#[tokio::test]
async fn import_pushes_the_new_title_to_anilist_in_the_background() {
    let fake = fake_anilist(|q, _| {
        if q.contains("MediaList(userId") {
            (404, json!({"data": {"Viewer": null, "MediaList": null}, "errors": [{"message": "Not Found.", "status": 404}]}), None)
        } else if q.contains("Media(id") {
            (200, json!({"data": {"Media": media_node(7, "ANIME")}}), None)
        } else if q.contains("Viewer") {
            (200, json!({"data": {"Viewer": {"id": 1, "name": "tester", "mediaListOptions": {"scoreFormat": "POINT_10_DECIMAL"}}}}), None)
        } else {
            save_ok()
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    let (s, b) = call(&st, "POST", "/media", Some(json!({"anilist_id": 7, "media_type": "anime", "title": "x", "status": "planning"}))).await;
    assert_eq!(s, 200);
    let id = b["id"].as_i64().unwrap();

    // FR-24: the push runs in the background, so wait for it to land.
    wait_for(|| st.db.get_media_by_id(id).unwrap().unwrap().anilist_list_entry_id.is_some()).await;
    assert_eq!(st.db.get_media_by_id(id).unwrap().unwrap().anilist_list_entry_id, Some(555));
    let saves = fake.calls_containing("SaveMediaListEntry");
    assert_eq!(saves.len(), 1);
    assert_eq!((saves[0]["mediaId"].as_i64(), saves[0]["status"].as_str()), (Some(7), Some("PLANNING")));
}

#[tokio::test]
async fn import_502s_when_anilist_is_unreachable() {
    let st = offline_state().await;
    login(&st);
    let (s, b) = call(&st, "POST", "/media", Some(json!({"anilist_id": 7, "media_type": "anime", "title": "x"}))).await;
    assert_eq!((s, b["detail"].as_str()), (502, Some("could not reach AniList to verify this title")));
    assert!(st.db.get_media_by_anilist_id(7).unwrap().is_none());
}

#[tokio::test]
async fn anilist_errors_become_clear_502s() {
    let fake = fake_anilist(|_, _| (400, json!({"data": null, "errors": [{"message": "Invalid token", "status": 400}]}), None)).await;
    let st = state_with(&fake);
    login(&st);
    let (s, b) = call(&st, "GET", "/favorites", None).await;
    assert_eq!((s, b["detail"].as_str()), (502, Some("AniList error: Invalid token")));
}

#[tokio::test]
async fn graphql_errors_are_api_errors_404s_are_not_found_and_5xx_stays_transient() {
    for status in [200, 400] {
        let fake = fake_anilist(move |_, _| (status, json!({"data": null, "errors": [{"message": "Cannot query field \"nope\"", "status": status}]}), None)).await;
        let err = AniList::with_url(&fake.url).search_studios("b").await.unwrap_err();
        assert_eq!(err, AlError::Api { message: "Cannot query field \"nope\"".into(), status });
    }

    // AniList's real shape for a missing Media: HTTP 404, root field nulled.
    let missing = fake_anilist(|_, _| (404, json!({"data": {"Media": null}, "errors": [{"message": "Not Found.", "status": 404}]}), None)).await;
    let al = AniList::with_url(&missing.url);
    assert!(matches!(al.search_studios("b").await, Err(AlError::NotFound(_))));
    assert_eq!(al.get_by_id(999_999_999).await.unwrap(), None);

    for status in [500, 502, 503] {
        let fake = fake_anilist(move |_, _| (status, json!({"errors": [{"message": "Internal Server Error", "status": status}]}), None)).await;
        // Transient, so a push that fails this way is queued for retry (FR-26).
        assert!(AniList::with_url(&fake.url).search_studios("b").await.unwrap_err().is_network(), "{status}");
    }
}

#[tokio::test]
async fn http_429_honors_retry_after_and_is_bounded() {
    let hits = Arc::new(Mutex::new(0));
    let h = hits.clone();
    let fake = fake_anilist(move |_, _| {
        let mut n = h.lock().unwrap();
        *n += 1;
        if *n == 1 { (429, json!({}), Some("0.01")) } else { (200, json!({"data": {"Page": {"studios": [{"id": 1, "name": "Bones"}]}}}), None) }
    })
    .await;
    let al = AniList::with_url(&fake.url);
    assert_eq!(al.search_studios("b").await.unwrap(), json!([{"anilist_id": 1, "name": "Bones"}]));
    assert_eq!(*hits.lock().unwrap(), 2);

    let always = fake_anilist(|_, _| (429, json!({}), Some("0.01"))).await;
    let al = AniList::with_url(&always.url);
    assert!(matches!(al.search_studios("b").await, Err(AlError::Network(_))));
    assert_eq!(always.calls().len(), 3); // 1 try + 2 retries

    let too_long = fake_anilist(|_, _| (429, json!({}), Some("120"))).await;
    let al = AniList::with_url(&too_long.url);
    assert!(matches!(al.search_studios("b").await, Err(AlError::Network(_))));
    assert_eq!(too_long.calls().len(), 1); // fails fast instead of blocking
}

/// One 429 carrying `retry_after`, then success: how many requests it took.
async fn requests_after_one_429(retry_after: Option<&'static str>) -> usize {
    let hits = Arc::new(Mutex::new(0));
    let fake = fake_anilist(move |_, _| {
        let mut n = hits.lock().unwrap();
        *n += 1;
        if *n == 1 { (429, json!({}), retry_after) } else { (200, json!({"data": {"Page": {"studios": []}}}), None) }
    })
    .await;
    AniList::with_url(&fake.url).search_studios("b").await.unwrap();
    fake.calls().len()
}

#[tokio::test]
async fn http_429_without_a_usable_retry_after_waits_the_default_then_retries() {
    let started = Instant::now();
    let tries = tokio::join!(requests_after_one_429(None), requests_after_one_429(Some("soon")), requests_after_one_429(Some("-5")));
    assert_eq!(tries, (2, 2, 2));
    assert!(started.elapsed() >= Duration::from_secs(2)); // the 2s default, not an immediate retry
}

#[tokio::test]
async fn get_list_entry_returns_the_entry_and_score_format_in_one_request() {
    let fake = fake_anilist(|_, _| {
        (200, json!({"data": {
            "Viewer": {"mediaListOptions": {"scoreFormat": "POINT_10_DECIMAL"}},
            "MediaList": {"id": 77, "status": "REPEATING", "progress": 4, "repeat": 1, "score": 8.5, "updatedAt": 1_767_225_600},
        }}), None)
    })
    .await;
    let (entry, score_format) = AniList::with_url(&fake.url).get_list_entry("token", 1, 42, "manga").await.unwrap();
    assert_eq!(fake.calls().iter().map(|(_, v)| v.clone()).collect::<Vec<_>>(), vec![json!({"userId": 1, "mediaId": 42})]);
    assert_eq!(fake.auths(), vec![Some("Bearer token".to_string())]);
    assert_eq!(score_format.as_deref(), Some("POINT_10_DECIMAL"));
    let expected = EntryState {
        list_entry_id: 77,
        status: "rereading".into(),
        progress: 4,
        repeat_count: 1,
        score: Some(8.5),
        updated_at: Some("2026-01-01T00:00:00+00:00".into()),
    };
    assert_eq!(entry, Some(expected));

    // Not on the list: AniList 404s the whole query, Viewer included.
    let missing = fake_anilist(|_, _| {
        (404, json!({"data": {"Viewer": null, "MediaList": null}, "errors": [{"message": "Not Found.", "status": 404}]}), None)
    })
    .await;
    assert_eq!(AniList::with_url(&missing.url).get_list_entry("token", 1, 42, "anime").await.unwrap(), (None, None));
}

#[test]
fn unknown_media_types_are_errors_never_defaults() {
    assert_eq!(anilist::to_media_type(&json!("ANIME")).unwrap(), "anime");
    assert_eq!(anilist::to_media_type(&json!("MANGA")).unwrap(), "manga");
    for other in [json!("NOVEL"), json!("anime"), json!(""), Value::Null] {
        assert!(anilist::to_media_type(&other).is_err(), "{other}");
    }
}

// ----------------------------------------------------------------------- sync ----

fn collection(entries: Vec<Value>) -> Value {
    json!({"data": {"MediaListCollection": {"lists": [{"entries": entries}]}}})
}

fn list_node(anilist_id: i64, al_type: &str, updated_at: i64, score: f64) -> Value {
    json!({"id": 1000 + anilist_id, "status": "CURRENT", "progress": 3, "repeat": 0, "score": score,
           "updatedAt": updated_at, "media": media_node(anilist_id, al_type)})
}

#[tokio::test]
async fn pull_imports_both_media_types_and_seeds_ratings_from_anilist_scores() {
    let fake = fake_anilist(|q, v| {
        let ts = 1_700_000_000;
        match (q.contains("MediaListCollection"), v["type"].as_str()) {
            (true, Some("ANIME")) => (200, collection(vec![list_node(1, "ANIME", ts, 8.5), list_node(3, "ANIME", ts, 0.0)]), None),
            (true, Some("MANGA")) => (200, collection(vec![list_node(2, "MANGA", ts, 8.5)]), None),
            _ => (500, json!({}), None),
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);

    // New entries import without prompting.
    assert_eq!(sync::pull(&st).await.unwrap(), json!({"imported": 3, "updated": 0, "conflicts": []}));
    let anime = st.db.get_media_by_anilist_id(1).unwrap().unwrap();
    let manga = st.db.get_media_by_anilist_id(2).unwrap().unwrap();
    assert_eq!((anime.fields.media_type.as_str(), anime.status.as_str(), anime.anilist_list_entry_id), ("anime", "watching", Some(1001)));
    assert_eq!((manga.fields.media_type.as_str(), manga.status.as_str(), manga.anilist_list_entry_id), ("manga", "reading", Some(1002)));
    assert!(anime.last_synced_at.is_some() && manga.last_synced_at.is_some());
    let (a, m) = (anime.rating.unwrap(), manga.rating.unwrap());
    assert_eq!((a.story, a.characters, a.animation, a.audio, a.enjoyment, a.overall), (8.5, 8.5, Some(8.5), 8.5, 8.5, 8.5));
    assert_eq!((m.story, m.characters, m.animation, m.audio, m.enjoyment, m.overall), (8.5, 8.5, Some(8.5), 0.0, 8.5, 8.5));
    // AniList's 0 means no score: no rating is invented.
    assert!(st.db.get_media_by_anilist_id(3).unwrap().unwrap().rating.is_none());
}

#[tokio::test]
async fn a_pull_that_only_changes_the_overall_keeps_the_ratings_source() {
    // (updatedAt, title 1's score, title 2's score) on the AniList list.
    let remote = Arc::new(Mutex::new((1_700_000_000_i64, 8.5, 8.5)));
    let r = remote.clone();
    let fake = fake_anilist(move |q, v| {
        let (ts, one, two) = *r.lock().unwrap();
        match (q.contains("MediaListCollection"), v["type"].as_str()) {
            (true, Some("ANIME")) => (200, collection(vec![list_node(1, "ANIME", ts, one), list_node(2, "ANIME", ts, two)]), None),
            (true, _) => (200, collection(vec![]), None),
            _ => (500, json!({}), None),
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    // Title 2 was rated in the stepper and is in sync; title 1 comes from Pull.
    let two = st.db.insert_or_update_media(&fields(2, "anime"), Some("watching")).unwrap();
    st.db.upsert_rating(two, &Scores { story: 9.0, characters: 8.0, animation: 7.0, audio: 8.5, enjoyment: 9.5 }).unwrap();
    st.db.set_last_synced(two, &myanimerank_lib::db::now_iso()).unwrap();
    sync::pull(&st).await.unwrap();
    let rating = |anilist_id| st.db.get_media_by_anilist_id(anilist_id).unwrap().unwrap().rating.unwrap();
    assert_eq!((rating(1).source.as_str(), rating(2).source.as_str()), ("anilist", "app"));

    // Both scores change on AniList later; Pull overwrites the overall only.
    *remote.lock().unwrap() = (1_900_000_000, 6.0, 7.0);
    assert_eq!(sync::pull(&st).await.unwrap(), json!({"imported": 0, "updated": 2, "conflicts": []}));
    let (one, two) = (rating(1), rating(2));
    assert_eq!((one.overall, one.source.as_str(), one.story, one.enjoyment), (6.0, "anilist", 8.5, 8.5));
    assert_eq!((two.overall, two.source.as_str(), two.story, two.animation, two.enjoyment), (7.0, "app", 9.0, Some(7.0), 9.5));
}

#[tokio::test]
async fn pull_imports_anilist_values_as_is_without_auto_completing() {
    // A rewatch sitting at the final episode imports as-is: FR-53's
    // auto-complete is for local edits, not for rewriting AniList's data.
    let fake = fake_anilist(|q, v| {
        if q.contains("MediaListCollection") && v["type"] == "ANIME" {
            let mut node = list_node(1, "ANIME", 1_700_000_000, 0.0);
            node["status"] = json!("REPEATING");
            node["progress"] = json!(12);
            node["repeat"] = json!(2);
            (200, collection(vec![node]), None)
        } else {
            (200, collection(vec![]), None)
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    sync::pull(&st).await.unwrap();
    let m = st.db.get_media_by_anilist_id(1).unwrap().unwrap();
    assert_eq!((m.status.as_str(), m.progress, m.repeat_count), ("rewatching", 12, 2));
    assert_eq!(m.updated_at, m.last_synced_at);
}

#[tokio::test]
async fn pull_leaves_local_only_rows_alone() {
    // A local row with no AniList counterpart isn't a conflict: it just
    // hasn't been pushed yet.
    let fake = fake_anilist(|_, _| (200, collection(vec![]), None)).await;
    let st = state_with(&fake);
    login(&st);
    let id = st.db.insert_or_update_media(&fields(1, "anime"), Some("watching")).unwrap();
    st.db.update_progress(id, 3).unwrap();
    let before = st.db.get_media_by_id(id).unwrap().unwrap();
    assert_eq!(sync::pull(&st).await.unwrap(), json!({"imported": 0, "updated": 0, "conflicts": []}));
    assert_eq!(st.db.get_media_by_id(id).unwrap().unwrap(), before);
}

#[tokio::test]
async fn pull_imports_then_applies_a_later_remote_change_without_a_false_conflict() {
    // (updatedAt, status, progress) of the one entry on the AniList list.
    let remote = Arc::new(Mutex::new((1_700_000_000_i64, "CURRENT", 3_i64)));
    let r = remote.clone();
    let fake = fake_anilist(move |q, v| {
        if q.contains("MediaListCollection") && v["type"] == "MANGA" {
            let (ts, status, progress) = *r.lock().unwrap();
            let mut node = list_node(42, "MANGA", ts, 8.0);
            node["status"] = json!(status);
            node["progress"] = json!(progress);
            (200, collection(vec![node]), None)
        } else {
            (200, collection(vec![]), None)
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);

    let first = sync::pull(&st).await.unwrap();
    assert_eq!((first["imported"].as_i64(), first["updated"].as_i64()), (Some(1), Some(0)));
    let m = st.db.get_media_by_anilist_id(42).unwrap().unwrap();
    assert_eq!((m.fields.media_type.as_str(), m.status.as_str(), m.progress), ("manga", "reading", 3));
    assert_eq!(m.anilist_list_entry_id, Some(1042));
    assert_eq!(m.rating.as_ref().unwrap().overall, 8.0);
    assert_eq!(m.updated_at, m.last_synced_at);

    // AniList changes later, with no local edit: a one-sided remote change,
    // applied, not a conflict.
    *remote.lock().unwrap() = (chrono::Utc::now().timestamp() + 3600, "COMPLETED", 100);
    let second = sync::pull(&st).await.unwrap();
    assert_eq!((second["updated"].as_i64(), second["conflicts"].as_array().unwrap().len()), (Some(1), 0));
    let m = st.db.get_media_by_anilist_id(42).unwrap().unwrap();
    assert_eq!((m.status.as_str(), m.progress, m.pending_conflict), ("completed", 100, false));
}

#[tokio::test]
async fn pull_flags_both_sides_changed_as_a_conflict_and_keeps_local_values() {
    let future = chrono::Utc::now().timestamp() + 3600;
    let fake = fake_anilist(move |q, v| {
        if q.contains("MediaListCollection") && v["type"] == "ANIME" {
            (200, collection(vec![list_node(5, "ANIME", future, 0.0)]), None)
        } else {
            (200, collection(vec![]), None)
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    let id = st.db.insert_or_update_media(&fields(5, "anime"), Some("paused")).unwrap();
    st.db.set_last_synced(id, "2026-01-01T00:00:00+00:00").unwrap();
    st.db.update_progress(id, 7).unwrap(); // local edit after the last sync

    let result = sync::pull(&st).await.unwrap();
    let conflicts = result["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["local"]["progress"], 7);
    assert_eq!(conflicts[0]["remote"]["status"], "watching");
    let m = st.db.get_media_by_id(id).unwrap().unwrap();
    assert_eq!((m.progress, m.pending_conflict), (7, true));
}

#[tokio::test]
async fn one_failing_entry_is_skipped_and_imported_cleanly_on_the_next_pull() {
    let fake = fake_anilist(|q, v| {
        if q.contains("MediaListCollection") && v["type"] == "MANGA" {
            let ts = 1_700_000_000;
            (200, collection(vec![list_node(1, "MANGA", ts, 0.0), list_node(2, "MANGA", ts, 0.0), list_node(3, "MANGA", ts, 0.0)]), None)
        } else {
            (200, collection(vec![]), None)
        }
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let st = AppState::new(Db::open(&path).unwrap(), AniList::with_url(&fake.url), Config { client_id: "1".into() });
    login(&st);
    // Entry 2's write fails after its row is inserted, as one did in a real pull.
    let side = rusqlite::Connection::open(&path).unwrap();
    side.execute_batch(
        "CREATE TRIGGER fail_two BEFORE UPDATE OF last_synced_at ON media WHEN NEW.anilist_id = 2
         BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END;",
    )
    .unwrap();

    let first = sync::pull(&st).await.unwrap();
    assert_eq!(first["imported"], 2);
    assert!(st.db.get_media_by_anilist_id(3).unwrap().is_some()); // the import kept going
    assert!(st.db.get_media_by_anilist_id(2).unwrap().is_none()); // and left no half-imported row

    // Next pull: entry 2 imports as new, not as a "never synced" false conflict.
    side.execute_batch("DROP TRIGGER fail_two;").unwrap();
    let second = sync::pull(&st).await.unwrap();
    assert_eq!((second["imported"].as_i64(), second["conflicts"].as_array().unwrap().len()), (Some(1), 0));
    let m = st.db.get_media_by_anilist_id(2).unwrap().unwrap();
    assert_eq!((m.status.as_str(), m.anilist_list_entry_id, m.pending_conflict), ("reading", Some(1002), false));
}

fn save_ok() -> Reply {
    (200, json!({"data": {"SaveMediaListEntry": {"id": 555, "status": "CURRENT", "progress": 1, "updatedAt": 1}}}), None)
}

/// get_list_entry's reply for a title that IS on the list: the entry, last
/// changed on AniList at `updated_at`, plus the account's current scoreFormat.
fn on_list(score_format: &str, updated_at: i64) -> Reply {
    (200, json!({"data": {"Viewer": {"mediaListOptions": {"scoreFormat": score_format}},
        "MediaList": {"id": 1009, "status": "CURRENT", "progress": 3, "repeat": 0, "score": 0, "updatedAt": updated_at}}}), None)
}

/// An anime on the list, synced at the start of 2026 and finished locally since.
fn synced_then_edited(st: &Shared, anilist_id: i64) -> i64 {
    let id = st.db.insert_or_update_media(&fields(anilist_id, "anime"), Some("watching")).unwrap();
    st.db.set_anilist_list_entry_id(id, 1009).unwrap();
    st.db.set_last_synced(id, "2026-01-01T00:00:00+00:00").unwrap();
    st.db.update_progress(id, 12).unwrap();
    id
}

#[tokio::test]
async fn guarded_push_sends_a_clean_local_change_in_two_requests() {
    // AniList's copy last changed in 2023, before the sync: only local changed.
    let fake = fake_anilist(|q, _| if q.contains("MediaList(userId") { on_list("POINT_10_DECIMAL", 1_700_000_000) } else { save_ok() }).await;
    let st = state_with(&fake);
    login(&st);
    let id = synced_then_edited(&st, 9);
    st.db.upsert_rating(id, &scores(7.5)).unwrap();

    let out = sync::push_now_guarded(&st, id).await.unwrap();
    assert_eq!(out["score_format_drift"], false);
    assert_eq!(
        fake.calls_containing("SaveMediaListEntry"),
        vec![json!({"mediaId": 9, "status": "COMPLETED", "progress": 12, "repeat": 0, "score": 7.5})]
    );
    // One combined entry + scoreFormat lookup, then the mutation: no separate Viewer call.
    assert_eq!(fake.calls().len(), 2);
    assert_eq!(fake.auths(), vec![Some("Bearer test-token".to_string()); 2]);
    let m = st.db.get_media_by_id(id).unwrap().unwrap();
    assert_eq!(m.last_synced_at.as_deref(), out["synced_at"].as_str());
}

#[tokio::test]
async fn guarded_push_creates_the_entry_when_not_on_the_list_and_never_sends_a_null_score() {
    let fake = fake_anilist(|q, _| {
        if q.contains("MediaList(userId") {
            (404, json!({"data": {"Viewer": null, "MediaList": null}, "errors": [{"message": "Not Found.", "status": 404}]}), None)
        } else if q.contains("Viewer") {
            (200, json!({"data": {"Viewer": {"id": 1, "name": "tester", "mediaListOptions": {"scoreFormat": "POINT_10_DECIMAL"}}}}), None)
        } else {
            save_ok()
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    let id = st.db.insert_or_update_media(&fields(9, "anime"), Some("watching")).unwrap();

    let out = sync::push_now_guarded(&st, id).await.unwrap();
    assert_eq!(out["score_format_drift"], false);
    let saves = fake.calls_containing("SaveMediaListEntry");
    assert_eq!(saves.len(), 1);
    assert!(saves[0].get("score").is_none(), "unrated title must omit score, got {}", saves[0]);
    let m = st.db.get_media_by_id(id).unwrap().unwrap();
    assert_eq!((m.anilist_list_entry_id, m.pending_conflict), (Some(555), false));
    // AniList nulls Viewer on the 404, so scoreFormat takes its own call here.
    assert_eq!(fake.calls().len(), 3);
}

#[tokio::test]
async fn guarded_push_refuses_on_conflict_and_never_writes_to_anilist() {
    let future = chrono::Utc::now().timestamp() + 3600;
    let fake = fake_anilist(move |q, _| {
        if q.contains("MediaList(userId") {
            (200, json!({"data": {"Viewer": {"mediaListOptions": {"scoreFormat": "POINT_10_DECIMAL"}},
                "MediaList": {"id": 1, "status": "COMPLETED", "progress": 12, "repeat": 0, "score": 9.0, "updatedAt": future}}}), None)
        } else {
            save_ok()
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    let id = st.db.insert_or_update_media(&fields(9, "anime"), Some("watching")).unwrap();
    st.db.set_last_synced(id, "2026-01-01T00:00:00+00:00").unwrap();
    st.db.update_status(id, "paused").unwrap();
    st.push_queue.lock().unwrap().insert(id);

    let out = sync::push_now_guarded(&st, id).await.unwrap();
    assert_eq!(out["conflict"], true);
    assert!(fake.calls_containing("SaveMediaListEntry").is_empty());
    assert!(st.db.get_media_by_id(id).unwrap().unwrap().pending_conflict);
    // Only an explicit resolve settles it, so it leaves the retry queue.
    assert!(!st.push_queue.lock().unwrap().contains(&id));
}

#[tokio::test]
async fn guarded_push_rechecks_score_format_from_the_entry_lookup() {
    // FR-28: a format changed on the AniList website is caught before any
    // score is written.
    let fake = fake_anilist(|q, _| if q.contains("MediaList(userId") { on_list("POINT_100", 1_700_000_000) } else { save_ok() }).await;
    let st = state_with(&fake);
    login(&st);
    let id = synced_then_edited(&st, 9);

    let out = sync::push_now_guarded(&st, id).await.unwrap();
    assert_eq!(out, json!({"synced_at": null, "score_format_drift": true}));
    assert!(fake.calls_containing("SaveMediaListEntry").is_empty());
    assert_eq!(st.db.get_auth().unwrap().unwrap().score_format.as_deref(), Some("POINT_100"));
}

#[tokio::test]
async fn push_skips_and_reports_drift_when_score_format_is_not_decimal() {
    let fake = fake_anilist(|q, _| {
        if q.contains("Viewer") {
            (200, json!({"data": {"Viewer": {"id": 1, "name": "t", "mediaListOptions": {"scoreFormat": "POINT_100"}}}}), None)
        } else {
            save_ok()
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    let id = st.db.insert_or_update_media(&fields(9, "anime"), None).unwrap();
    let (s, b) = call(&st, "POST", &format!("/sync/push/{id}"), None).await;
    assert_eq!((s, b), (200, json!({"synced_at": null, "score_format_drift": true})));
    assert!(fake.calls_containing("SaveMediaListEntry").is_empty());
    assert_eq!(st.db.get_auth().unwrap().unwrap().score_format.as_deref(), Some("POINT_100"));
}

#[tokio::test]
async fn offline_push_is_queued_for_retry() {
    let st = offline_state().await;
    login(&st);
    let id = st.db.insert_or_update_media(&fields(9, "anime"), None).unwrap();
    assert!(sync::push_now_guarded(&st, id).await.is_err());
    assert!(st.push_queue.lock().unwrap().contains(&id));
}

#[tokio::test]
async fn delete_removes_the_anilist_entry_first_and_keeps_the_row_if_that_fails() {
    let st = offline_state().await;
    login(&st);
    let id = st.db.insert_or_update_media(&fields(9, "anime"), None).unwrap();
    st.db.set_anilist_list_entry_id(id, 77).unwrap();
    let (s, _) = call(&st, "DELETE", &format!("/media/{id}"), None).await;
    assert_eq!(s, 502);
    assert!(st.db.get_media_by_id(id).unwrap().is_some());

    let never_synced = st.db.insert_or_update_media(&fields(10, "anime"), None).unwrap();
    assert_eq!(call(&st, "DELETE", &format!("/media/{never_synced}"), None).await, (200, json!({"deleted": true})));
}

#[tokio::test]
async fn library_rankings_and_insights_return_prd_shapes() {
    let st = offline_state().await;
    login(&st);
    let a = st.db.insert_or_update_media(&fields(1, "anime"), Some("watching")).unwrap();
    st.db.insert_or_update_media(&fields(2, "manga"), None).unwrap();
    st.db.upsert_rating(a, &scores(8.0)).unwrap();

    let (s, lib) = call(&st, "GET", "/library?media_type=anime", None).await;
    assert_eq!((s, lib["count"].as_i64()), (200, Some(1)));
    let row = &lib["results"][0];
    for key in ["id", "anilist_id", "media_type", "status", "progress", "repeat_count", "pending_conflict", "genres", "rating"] {
        assert!(row.get(key).is_some(), "missing {key}");
    }
    assert!(row["rating"].get("media_id").is_none());
    assert_eq!(call(&st, "GET", "/library?media_type=novel", None).await.0, 400);

    let (_, rankings) = call(&st, "GET", "/rankings?genre=Action", None).await;
    assert_eq!(rankings["count"], 1); // unrated manga excluded
    let (_, insights) = call(&st, "GET", "/insights", None).await;
    assert_eq!(insights["top_criterion"]["anime"]["avg_score"], 8.0);
    assert_eq!(insights["top_criterion"]["manga"], Value::Null);
    assert_eq!(insights["distribution"]["anime"].as_array().unwrap().len(), 18);
}

#[tokio::test]
async fn profile_stats_carry_the_viewers_avatar_and_banner_or_null() {
    let cases = [
        (json!({"large": "https://img/avatar.png"}), json!("https://img/banner.jpg"), json!("https://img/avatar.png"), json!("https://img/banner.jpg")),
        (json!({"large": null}), Value::Null, Value::Null, Value::Null),
        (Value::Null, Value::Null, Value::Null, Value::Null),
    ];
    for (avatar, banner, want_avatar, want_banner) in cases {
        let fake = fake_anilist(move |_, _| {
            let viewer = json!({
                "avatar": avatar, "bannerImage": banner,
                "statistics": {"anime": {"count": 3, "minutesWatched": 2880}, "manga": {"count": 2, "chaptersRead": 40}},
            });
            (200, json!({"data": {"Viewer": viewer, "followersPage": {"pageInfo": {"total": 5}}, "followingPage": {"pageInfo": {"total": 6}}}}), None)
        })
        .await;
        let st = state_with(&fake);
        login(&st);

        let (s, body) = call(&st, "GET", "/profile/stats", None).await;
        assert_eq!(s, 200);
        assert_eq!(
            body,
            json!({
                "total_anime": 3, "total_manga": 2, "days_watched": 2.0, "chapters_read": 40,
                "followers": 5, "following": 6, "avatar_url": want_avatar, "banner_url": want_banner,
            })
        );
        // Avatar and banner ride along on the stats query, not a request of their own.
        assert_eq!(fake.calls().len(), 1);
    }
}

// -------------------------------------------------------------------- friends ----

fn friend_media(anilist_id: i64, al_type: &str) -> Value {
    json!({
        "id": anilist_id, "type": al_type,
        "title": {"romaji": format!("Title {anilist_id}"), "english": format!("English {anilist_id}"), "native": null},
        "coverImage": {"large": format!("https://img/{anilist_id}.jpg")}, "genres": ["Action"], "popularity": anilist_id * 100,
    })
}

/// `score` as AniList sends it: a whole converted score is a JSON integer
/// (`9`, `0`), not `9.0`.
fn friend_node(anilist_id: i64, al_type: &str, status: &str, score: Value) -> Value {
    json!({"status": status, "score": score, "progress": anilist_id + 2, "media": friend_media(anilist_id, al_type)})
}

/// AniList's `score(format:)` for an entry stored as POINT_100 `p`, as
/// recorded from real POINT_5 and POINT_3 lists (60 and 85 are entries scored
/// before the user switched format).
fn al_score(p: i64, format: &str) -> Value {
    match (format, p) {
        (_, 0) => json!(0),
        ("POINT_5", 10) => json!(1),
        ("POINT_5", 30) => json!(2),
        ("POINT_5", 50 | 60) => json!(3),
        ("POINT_5", 70 | 85) => json!(4),
        ("POINT_5", 90) => json!(5),
        ("POINT_3", 35) => json!(1),
        ("POINT_3", 60) => json!(2),
        ("POINT_3", 85) => json!(3),
        ("POINT_10_DECIMAL", p) if p % 10 == 0 => json!(p / 10),
        ("POINT_10_DECIMAL", p) => json!(p as f64 / 10.0),
        other => panic!("no recorded conversion for {other:?}"),
    }
}

/// Public friends besides Pal (id 42, POINT_10_DECIMAL): (id, name, scoreFormat).
/// "shy" (48) is private and "gone" (49) no longer exists: only their names
/// ever show, in a following list.
const FRIEND_USERS: [(i64, &str, &str); 7] = [
    (1, "tester", "POINT_10_DECIMAL"),
    (43, "hidden", "POINT_100"),
    (44, "empty", "POINT_10"),
    (45, "stars", "POINT_5"),
    (46, "faces", "POINT_3"),
    (48, "shy", "POINT_10_DECIMAL"),
    (49, "gone", "POINT_10_DECIMAL"),
];

/// Who a `User(id:, name:)` lookup finds: FRIEND_USERS, "u<id>" for any id
/// from 100 up (by id, or by that name), otherwise Pal.
fn friend_user(v: &Value) -> (i64, String, &'static str) {
    if let Some(&(id, name, format)) = FRIEND_USERS.iter().find(|(id, name, _)| v["id"] == *id || v["name"] == *name) {
        return (id, name.to_string(), format);
    }
    let numbered = v["id"].as_i64().filter(|id| *id >= 100).or_else(|| v["name"].as_str()?.strip_prefix('u')?.parse().ok());
    match numbered {
        Some(id) => (id, format!("u{id}"), "POINT_10_DECIMAL"),
        None => (42, "Pal".into(), "POINT_10_DECIMAL"),
    }
}

/// AniList as seen for friends, answering with the shapes recorded from the
/// live API: `User` by name or id (with the user's score format), then both
/// lists in one aliased query, scores in the requested `$format`. The `User`
/// lookup says "Not Found." for a private user exactly as for a missing one;
/// only `MediaListCollection` tells them apart ("Private User" vs "User not
/// found"). "ghost" doesn't exist; "locked" and "shy" are private; "hidden"
/// is found but its lists come back private; "empty" is public with nothing
/// listed; "stars" and "faces" score in POINT_5 and POINT_3; "tester" is the
/// logged-in account; everyone else (Pal, "u<id>") has Pal's lists. The
/// first request gets a 429, and `private_lists` makes Pal private.
async fn friend_anilist(private_lists: Arc<std::sync::atomic::AtomicBool>) -> Fake {
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    fake_anilist(move |q, v| {
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return (429, json!({}), Some("0.01"));
        }
        friend_reply(q, v, private_lists.load(std::sync::atomic::Ordering::SeqCst))
    })
    .await
}

/// The user lookups and lists behind `friend_anilist` and `follow_anilist`.
fn friend_reply(q: &str, v: &Value, pal_private: bool) -> Reply {
    let pal_private = pal_private && (v["id"] == 42 || v["userId"] == 42);
    let private = v["name"] == "locked" || pal_private || v["id"] == 48 || v["userId"] == 48;
    let error = |message: &str, data: Value| (404, json!({"data": data, "errors": [{"message": message, "status": 404}]}), None);
    if q.contains("User(") {
        if private || v["name"] == "ghost" || v["id"] == 49 {
            return error("Not Found.", json!({"User": null}));
        }
        let (id, name, format) = friend_user(v);
        let user = json!({"id": id, "name": name, "avatar": {"large": "https://img/u.png"}, "mediaListOptions": {"scoreFormat": format}});
        return (200, json!({"data": {"User": user}}), None);
    }
    if !q.contains("anime:") {
        return error(if private { "Private User" } else { "User not found" }, json!({"MediaListCollection": null}));
    }
    if private || v["userId"] == 43 {
        // One error per aliased list.
        let e = json!({"message": "Private User", "status": 404});
        return (404, json!({"data": {"anime": null, "manga": null}, "errors": [e, e]}), None);
    }
    if v["userId"] == 44 {
        return (200, json!({"data": {"anime": {"lists": []}, "manga": {"lists": []}}}), None);
    }
    // stars and faces: each entry stored as POINT_100, sent in `$format`.
    let stored: Option<([i64; 6], [i64; 2])> = match v["userId"].as_i64() {
        Some(45) => Some(([90, 70, 50, 30, 10, 0], [85, 60])),
        Some(46) => Some(([85, 60, 35, 0, 0, 0], [60, 35])),
        _ => None,
    };
    if let Some((anime, manga)) = stored {
        let format = v["format"].as_str().unwrap_or_default();
        let list = |al_type: &str, first: i64, p100: &[i64]| {
            let entries: Vec<Value> = p100.iter().zip(first..).map(|(p, id)| friend_node(id, al_type, "COMPLETED", al_score(*p, format))).collect();
            json!({"lists": [{"entries": entries}]})
        };
        return (200, json!({"data": {"anime": list("ANIME", 100, &anime), "manga": list("MANGA", 200, &manga)}}), None);
    }
    let anime = json!({"lists": [
        {"entries": [friend_node(1, "ANIME", "CURRENT", json!(8.5)), friend_node(2, "ANIME", "REPEATING", json!(0)), friend_node(3, "ANIME", "PLANNING", json!(0))]},
        // A custom list repeats an entry that's already in a status list.
        {"entries": [friend_node(1, "ANIME", "CURRENT", json!(8.5))]},
    ]});
    let manga = json!({"lists": [
        {"entries": [friend_node(10, "MANGA", "CURRENT", json!(0.4)), friend_node(11, "MANGA", "REPEATING", json!(10))]},
    ]});
    (200, json!({"data": {"anime": anime, "manga": manga}}), None)
}

/// The logged-in account's (tester, id 1) follow graph, plus switches for
/// AniList's moods: the next request gets a 429, every request fails, follow
/// pages from this one on fail, Discover doesn't answer until released, the
/// token is rejected, or a request with the token is answered as if logged
/// out (no Viewer, `isFollower` false).
#[derive(Default)]
struct Follows {
    following: Vec<i64>,
    followers: Vec<i64>,
    throttle_next: bool,
    down: bool,
    failing_page: Option<usize>,
    hold_discover: bool,
    reject_token: bool,
    token_ignored: bool,
}

/// `friend_anilist`'s users and lists, plus `Page.following` / `Page.followers`
/// two users per page (so even a short list spans pages), the Viewer lookup
/// behind login, Pull's own (empty) lists and Discover. As recorded live,
/// `isFollower` is relative to whoever the token says is asking, and false
/// for everyone when nobody is.
async fn follow_anilist(graph: Arc<Mutex<Follows>>) -> Fake {
    fake_anilist_seeing_auth(move |q, v, auth| {
        let mut g = graph.lock().unwrap();
        if std::mem::take(&mut g.throttle_next) {
            return (429, json!({}), Some("0.01"));
        }
        if g.down {
            return (500, json!({}), None);
        }
        if q.contains("Viewer { id name") {
            return (200, json!({"data": {"Viewer": {"id": 1, "name": "tester", "mediaListOptions": {"scoreFormat": "POINT_10_DECIMAL"}}}}), None);
        }
        if q.contains("media(type:") {
            let hold = g.hold_discover;
            drop(g);
            if hold {
                // Blocks this worker only: the runtime's timers move to
                // another one (multi-thread tests only).
                tokio::task::block_in_place(|| {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while graph.lock().unwrap().hold_discover && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                });
            }
            return (200, json!({"data": {"Page": {"media": []}}}), None);
        }
        if q.contains("MediaListCollection(userId: $userId, type: $type)") {
            return (200, json!({"data": {"MediaListCollection": {"lists": []}}}), None);
        }
        for (field, ids) in [("following", &g.following), ("followers", &g.followers)] {
            if q.contains(&format!("{field}(")) {
                let page = v["page"].as_u64().unwrap_or(1) as usize;
                if g.failing_page.is_some_and(|p| page >= p) {
                    return (500, json!({}), None);
                }
                if auth.is_some() && g.reject_token {
                    return (400, json!({"data": null, "errors": [{"message": "Invalid token", "status": 400}]}), None);
                }
                let viewer = (auth == Some("Bearer test-token") && !g.token_ignored).then_some(1);
                let users: Vec<Value> = ids
                    .iter()
                    .skip((page - 1) * 2)
                    .take(2)
                    .map(|id| {
                        let (id, name, _) = friend_user(&json!({"id": id}));
                        let mut u = json!({"id": id, "name": name, "avatar": {"large": format!("https://img/{id}.png")}});
                        if q.contains("isFollower") {
                            u["isFollower"] = json!(viewer.is_some() && g.followers.contains(&id));
                        }
                        u
                    })
                    .collect();
                let mut data = json!({"Page": {"pageInfo": {"hasNextPage": page * 2 < ids.len()}, field: users}});
                if q.contains("Viewer") {
                    data["viewer"] = viewer.map_or(Value::Null, |id| json!({"id": id}));
                }
                return (200, json!({"data": data}), None);
            }
        }
        drop(g);
        friend_reply(q, v, false)
    })
    .await
}

#[test]
fn friend_scores_normalize_to_the_one_to_ten_scale_and_zero_means_unscored() {
    // FR-72: POINT_10_DECIMAL is what AniList converts every other format to.
    let decimal = [(None, None), (Some(0.0), None), (Some(0.4), Some(1.0)), (Some(1.0), Some(1.0)), (Some(7.3), Some(7.3)), (Some(10.0), Some(10.0))];
    // POINT_5 and POINT_3 arrive in the friend's own format: stars x 2, and
    // 1/2/3 -> 4/7/10, so the top score is 10 in every format.
    let stars = [(Some(0.0), None), (Some(1.0), Some(2.0)), (Some(2.0), Some(4.0)), (Some(3.0), Some(6.0)), (Some(4.0), Some(8.0)), (Some(5.0), Some(10.0))];
    let faces = [(None, None), (Some(0.0), None), (Some(1.0), Some(4.0)), (Some(2.0), Some(7.0)), (Some(3.0), Some(10.0))];
    for (format, cases) in [("POINT_10_DECIMAL", &decimal[..]), ("POINT_5", &stars[..]), ("POINT_3", &faces[..])] {
        for (raw, want) in cases {
            assert_eq!(db::normalize_score(*raw, format), *want, "{format} {raw:?}");
        }
    }
}

#[tokio::test]
async fn point_5_and_point_3_friends_are_read_in_their_own_format_and_reach_ten() {
    let fake = friend_anilist(Arc::default()).await;
    let st = state_with(&fake);
    login(&st);
    // (username, id, format, anime scores, manga scores) as stored
    let cases = [
        ("stars", 45, "POINT_5", vec![Some(10.0), Some(8.0), Some(6.0), Some(4.0), Some(2.0), None], vec![Some(8.0), Some(6.0)]),
        ("faces", 46, "POINT_3", vec![Some(10.0), Some(7.0), Some(4.0), None, None, None], vec![Some(7.0), Some(4.0)]),
    ];
    for (username, id, format, anime, manga) in cases {
        for (method, path, body) in [("POST", "/friends".to_string(), Some(json!({"username": username}))), ("POST", format!("/friends/{id}/refresh"), None)] {
            let before = fake.calls().len();
            let (s, b) = call(&st, method, &path, body).await;
            assert_eq!(s, 200, "{path}: {b}");
            // Still two requests: the lookup brings the format, the lists come in it.
            let calls = &fake.calls()[before..];
            let n = calls.len();
            assert!(n == 2 || (n == 3 && before == 0), "{path}: {n} requests");
            assert_eq!(calls[n - 1].1, json!({"userId": id, "format": format}), "{path}");
            assert!(calls[n - 1].0.contains("score(format: $format)"), "{path}");
            let scores = |t| st.db.friend_entries(id, t).unwrap().into_iter().map(|e| e.score).collect::<Vec<_>>();
            assert_eq!((scores("anime"), scores("manga")), (anime.clone(), manga.clone()), "{path}");
        }
    }
    assert!(fake.auths().iter().all(Option::is_none));
}

#[tokio::test]
async fn adding_a_friend_reads_their_public_lists_without_the_token_and_writes_nothing() {
    let fake = friend_anilist(Arc::default()).await;
    let st = state_with(&fake);
    login(&st);

    let (s, b) = call(&st, "POST", "/friends", Some(json!({"username": "pal"}))).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (&b["anilist_user_id"], &b["name"], &b["avatar_url"], &b["anime_count"], &b["manga_count"]),
        (&json!(42), &json!("Pal"), &json!("https://img/u.png"), &json!(3), &json!(2))
    );
    assert!(b["fetched_at"].as_str().is_some_and(|t| t.starts_with("20")), "{b}");

    // One 429 (retried after Retry-After), the user lookup, then both lists in one query.
    let calls = fake.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].1, json!({"name": "pal"}));
    assert_eq!(calls[2].1, json!({"userId": 42, "format": "POINT_10_DECIMAL"}));
    assert!(calls[2].0.contains("score(format: $format)"));
    // Public data only: never the token, never a mutation.
    assert!(fake.auths().iter().all(Option::is_none), "{:?}", fake.auths());
    assert!(calls.iter().all(|(q, _)| !q.contains("mutation")));

    // Statuses in this app's vocabulary per media type; 0 = unscored; the
    // custom-list duplicate stored once.
    let anime: Vec<_> = st.db.friend_entries(42, "anime").unwrap().into_iter().map(|e| (e.anilist_id, e.status, e.score)).collect();
    assert_eq!(anime, vec![(1, "watching".into(), Some(8.5)), (2, "rewatching".into(), None), (3, "planning".into(), None)]);
    let manga: Vec<_> = st.db.friend_entries(42, "manga").unwrap().into_iter().map(|e| (e.anilist_id, e.media_type, e.status, e.score)).collect();
    assert_eq!(manga, vec![(10, "manga".into(), "reading".into(), Some(1.0)), (11, "manga".into(), "rereading".into(), Some(10.0))]);
    let first = st.db.friend_entries(42, "anime").unwrap().remove(0);
    assert_eq!(
        (first.title.as_deref(), first.title_english.as_deref(), first.cover_url.as_deref(), first.genres, first.popularity, first.progress),
        (Some("Title 1"), Some("English 1"), Some("https://img/1.jpg"), vec!["Action".to_string()], Some(100), Some(3))
    );
    assert!(calls[2].0.contains("popularity")); // in the same request, not a new one

    // A friend's list never becomes part of your library.
    assert!(st.db.get_library(None, None, "overall").unwrap().is_empty());

    // Refresh looks them up by id, since usernames can change.
    let (s, _) = call(&st, "POST", "/friends/42/refresh", None).await;
    assert_eq!(s, 200);
    assert_eq!(fake.calls()[3].1, json!({"id": 42}));
    assert_eq!(st.db.friend_entries(42, "anime").unwrap().len(), 3); // replaced, not appended
    assert_eq!(call(&st, "POST", "/friends/7/refresh", None).await.0, 404);
}

#[tokio::test]
async fn unknown_users_private_lists_and_your_own_account_are_refused_and_store_nothing() {
    let fake = friend_anilist(Arc::default()).await;
    let st = state_with(&fake);
    login(&st);
    // (username, status, detail, AniList requests)
    let cases = [
        ("ghost", 404, "No AniList user named ghost", 2),
        ("hidden", 403, "hidden's list is private on AniList", 2), // found, lists private
        ("locked", 403, "locked's list is private on AniList", 2), // the lookup says Not Found.
        ("Tester", 400, "That's your own AniList account", 0),
        ("", 422, "username is required", 0),
    ];
    let mut before = 1; // the first request is the fake's 429
    for (username, status, detail, requests) in cases {
        let (s, b) = call(&st, "POST", "/friends", Some(json!({"username": username}))).await;
        assert_eq!((s, b["detail"].as_str()), (status, Some(detail)), "{username}");
        assert_eq!(fake.calls().len() - before, requests, "{username}");
        before = fake.calls().len();
    }
    assert!(fake.auths().iter().all(Option::is_none), "{:?}", fake.auths());
    assert_eq!(call(&st, "GET", "/friends", None).await, (200, json!({"friends": [], "hidden": [], "synced_at": null, "mutual_only": false})));
    assert!(st.db.friend_entries(43, "anime").unwrap().is_empty());

    let offline = offline_state().await;
    login(&offline);
    assert_eq!(call(&offline, "POST", "/friends", Some(json!({"username": "pal"}))).await.0, 502);
    assert_eq!(call(&offline, "GET", "/friends", None).await, (200, json!({"friends": [], "hidden": [], "synced_at": null, "mutual_only": false})));
}

#[tokio::test]
async fn a_public_user_with_an_empty_list_is_a_friend_not_a_private_one() {
    let fake = friend_anilist(Arc::default()).await;
    let st = state_with(&fake);
    login(&st);
    let (s, b) = call(&st, "POST", "/friends", Some(json!({"username": "empty"}))).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!((&b["anilist_user_id"], &b["anime_count"], &b["manga_count"]), (&json!(44), &json!(0), &json!(0)));
    let (s, c) = call(&st, "GET", "/friends/44/compare?media_type=anime", None).await;
    assert_eq!((s, &c["overlap"], &c["taste_match"], &c["skew"]), (200, &json!(0), &Value::Null, &Value::Null));
}

#[tokio::test]
async fn a_refresh_that_finds_the_list_private_keeps_the_stored_copy() {
    let private_lists = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fake = friend_anilist(private_lists.clone()).await;
    let st = state_with(&fake);
    login(&st);
    let (_, added) = call(&st, "POST", "/friends", Some(json!({"username": "pal"}))).await;
    private_lists.store(true, std::sync::atomic::Ordering::SeqCst);

    let before = fake.calls().len();
    let (s, b) = call(&st, "POST", "/friends/42/refresh", None).await;
    assert_eq!((s, b["detail"].as_str()), (403, Some("Pal's list is private on AniList")));
    // The lookup by id, then the list query that says why it found no one.
    let calls = fake.calls();
    assert_eq!((calls.len() - before, &calls[before].1, &calls[before + 1].1), (2, &json!({"id": 42}), &json!({"id": 42})));
    let (_, friends) = call(&st, "GET", "/friends", None).await;
    let mut kept = added.clone();
    kept["list_status"] = json!("private"); // same entries, same fetched_at, now marked
    assert_eq!(friends["friends"][0], kept);
    assert_eq!(st.db.friend_entries(42, "anime").unwrap().len(), 3);
}

fn their(anilist_id: i64, media_type: &str, status: &str, score: Option<f64>, genres: &[&str]) -> FriendEntry {
    FriendEntry {
        anilist_id,
        media_type: media_type.into(),
        status: status.into(),
        score,
        title: Some(format!("Title {anilist_id}")),
        title_english: None,
        cover_url: Some(format!("https://img/{anilist_id}.jpg")),
        genres: genres.iter().map(|g| g.to_string()).collect(),
        popularity: None,
        progress: None,
    }
}

fn friend_list(anilist_user_id: i64, name: &str, entries: Vec<FriendEntry>) -> FriendList {
    FriendList { anilist_user_id, name: name.into(), avatar_url: None, entries }
}

/// A library title of yours; `overall` None leaves it unrated.
fn mine(st: &Shared, anilist_id: i64, media_type: &str, status: &str, overall: Option<f64>, genres: &[&str]) {
    let mut f = fields(anilist_id, media_type);
    f.genres = genres.iter().map(|g| g.to_string()).collect();
    let id = st.db.insert_or_update_media(&f, Some(status)).unwrap();
    if let Some(v) = overall {
        st.db.upsert_rating(id, &scores(v)).unwrap();
    }
}

fn ids(v: &Value) -> Vec<i64> {
    v.as_array().unwrap().iter().map(|x| x["anilist_id"].as_i64().unwrap()).collect()
}

#[tokio::test]
async fn comparison_matches_on_anilist_id_within_one_media_type() {
    let st = offline_state().await;
    login(&st);
    // Shared and rated by both (anime 1-7): deltas +1, 0, -3, -0.5, +3, 0, +2.5.
    for (id, status, score, genres) in [
        (1, "completed", 9.0, &["Action"][..]), (2, "completed", 8.0, &["Action", "Drama"]), (3, "completed", 6.0, &["Drama"]),
        (4, "completed", 7.0, &["Comedy"]), (5, "completed", 10.0, &["Action"]), (6, "completed", 5.0, &["Romance"]),
        (7, "watching", 8.5, &["Action"]),
        // Yours only, or only on their Planning list.
        (20, "completed", 9.0, &["Other"]), (21, "completed", 8.5, &["Other"]),
        (22, "completed", 7.5, &["Other"]), // under 8.0
        (23, "dropped", 9.0, &["Other"]),   // not completed
        (24, "rewatching", 8.0, &["Other"]), // they dropped it: on their list, not Planning
    ] {
        mine(&st, id, "anime", status, Some(score), genres);
    }
    mine(&st, 30, "anime", "planning", None, &["Other"]);
    mine(&st, 31, "anime", "dropped", None, &["Other"]);
    mine(&st, 40, "manga", "completed", Some(9.0), &["Action"]);
    let mut entries: Vec<FriendEntry> = [(1, "completed", 8.0, &["Action"][..]), (2, "completed", 8.0, &["Action", "Drama"]), (3, "completed", 9.0, &["Drama"]),
        (4, "completed", 7.5, &["Comedy"]), (5, "completed", 7.0, &["Action"]), (6, "completed", 5.0, &["Romance"]), (7, "watching", 6.0, &["Action"])]
        .into_iter()
        .map(|(id, status, score, genres)| their(id, "anime", status, Some(score), genres))
        .collect();
    entries.extend([
        their(21, "anime", "planning", None, &["Other"]),
        their(24, "anime", "dropped", None, &["Other"]),
        their(30, "anime", "completed", Some(9.0), &["Other"]),  // only on your Planning list
        their(31, "anime", "completed", Some(9.5), &["Other"]),  // you dropped it
        their(32, "anime", "completed", Some(8.0), &["Other"]),
        their(33, "anime", "rewatching", Some(10.0), &["Other", "Mecha"]),
        their(34, "anime", "completed", Some(7.5), &["Other"]),  // under 8.0
        their(35, "anime", "dropped", Some(9.0), &["Other"]),    // not completed
        their(36, "anime", "completed", None, &["Other"]),       // unscored
        their(41, "manga", "completed", Some(9.0), &["Horror"]), // the other media type
    ]);
    st.db.replace_friend_list(&friend_list(42, "Pal", entries), "2026-01-01T00:00:00+00:00").unwrap();

    let (s, b) = call(&st, "GET", "/friends/42/compare?media_type=anime", None).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!((&b["media_type"], &b["friend"]["name"], &b["friend"]["anime_count"], &b["friend"]["manga_count"]), (&json!("anime"), &json!("Pal"), &json!(16), &json!(1)));
    assert_eq!(b["overlap"], 7);
    // Mean gap 10/7 -> 100 * (1 - (10/7) / 9) = 84.1 -> 84.
    assert_eq!(b["taste_match"], json!(84));
    assert!((b["skew"].as_f64().unwrap() - 3.0 / 7.0).abs() < 1e-9, "{}", b["skew"]);
    // FR-75: the 5 largest gaps, however small (4's is 0.5); the tie at 3.0
    // goes to the higher average (5: 8.5 over 3: 7.5).
    assert_eq!(ids(&b["disagreements"]), vec![5, 3, 7, 1, 4]);
    // The rest, smallest gap first; the tie at 0 goes to the higher average.
    assert_eq!(ids(&b["agreements"]), vec![2, 6]);
    assert_eq!(b["rarely_disagree"], json!(false));
    assert_eq!(ids(&b["shared"]), vec![5, 1, 7, 2, 4, 3, 6]);
    let row = &b["shared"][0];
    for key in ["anilist_id", "media_id", "media_type", "title", "title_english", "cover_url", "your_score", "their_score", "delta"] {
        assert!(row.get(key).is_some(), "shared row missing {key}");
    }
    assert_eq!((&row["your_score"], &row["their_score"], &row["delta"]), (&json!(10.0), &json!(7.0), &json!(3.0)));
    assert_eq!(b["disagreements"][1]["delta"], -3.0);

    assert_eq!(ids(&b["they_completed"]), vec![33, 30, 32]);
    assert_eq!((&b["they_completed"][1]["your_status"], &b["they_completed"][0]["your_status"]), (&json!("planning"), &Value::Null));
    assert_eq!(b["they_completed"][0]["their_score"], 10.0);
    assert_eq!(ids(&b["you_completed"]), vec![20, 21]);
    assert_eq!((&b["you_completed"][1]["their_status"], &b["you_completed"][0]["their_status"]), (&json!("planning"), &Value::Null));
    assert!(b["you_completed"][0]["media_id"].as_i64().is_some());

    let genres: Vec<(String, Value, i64, Value, i64)> = b["genres"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["genre"].as_str().unwrap().into(), g["your_avg"].clone(), g["your_count"].as_i64().unwrap(), g["their_avg"].clone(), g["their_count"].as_i64().unwrap()))
        .collect();
    let close = |v: &Value, want: f64| (v.as_f64().unwrap() - want).abs() < 1e-9;
    assert_eq!(genres.iter().map(|g| g.0.as_str()).collect::<Vec<_>>(), vec!["Other", "Action", "Drama", "Comedy", "Romance", "Mecha"]);
    assert!(close(&genres[0].1, 8.4) && genres[0].2 == 5 && close(&genres[0].3, 53.0 / 6.0) && genres[0].4 == 6, "{:?}", genres[0]);
    assert!(close(&genres[1].1, 8.875) && genres[1].2 == 4 && close(&genres[1].3, 7.25) && genres[1].4 == 4, "{:?}", genres[1]);
    assert_eq!((&genres[5].1, genres[5].2, genres[5].4), (&Value::Null, 0, 1)); // only they have Mecha

    // Manga is its own comparison: one manga each, no overlap.
    let (_, m) = call(&st, "GET", "/friends/42/compare?media_type=manga", None).await;
    assert_eq!((&m["overlap"], &m["taste_match"], &m["skew"]), (&json!(0), &Value::Null, &Value::Null));
    assert_eq!((ids(&m["they_completed"]), ids(&m["you_completed"])), (vec![41], vec![40]));

    assert_eq!(call(&st, "GET", "/friends/42/compare?media_type=novel", None).await.0, 400);
    assert_eq!(call(&st, "GET", "/friends/42/compare", None).await.0, 422);
    assert_eq!(call(&st, "GET", "/friends/7/compare?media_type=anime", None).await.0, 404);
}

#[tokio::test]
async fn compare_rows_carry_your_criteria_and_whether_you_rated_in_the_app() {
    let st = offline_state().await;
    login(&st);
    // 1: rated here; 2: seeded by a Pull from AniList's 8.0; 3: a manga rated here.
    let one = st.db.insert_or_update_media(&fields(1, "anime"), Some("completed")).unwrap();
    st.db.upsert_rating(one, &Scores { story: 9.0, characters: 8.0, animation: 7.0, audio: 6.0, enjoyment: 10.0 }).unwrap();
    st.db.import_remote_entry(&fields(2, "anime"), 1002, "completed", 12, 0, Some(8.0), "2026-01-01T00:00:00.000000+00:00").unwrap();
    let three = st.db.insert_or_update_media(&fields(3, "manga"), Some("reading")).unwrap();
    st.db.upsert_rating(three, &Scores { story: 6.0, characters: 7.0, animation: 9.0, audio: 0.0, enjoyment: 8.0 }).unwrap();
    let mut entries = vec![their(1, "anime", "watching", Some(7.0), &["Action"]), their(2, "anime", "completed", Some(8.0), &["Action"]), their(3, "manga", "reading", Some(7.5), &["Action"])];
    entries[0].progress = Some(5);
    entries[2].progress = Some(120);
    st.db.replace_friend_list(&friend_list(42, "Pal", entries), "2026-01-01T00:00:00+00:00").unwrap();

    let (_, b) = call(&st, "GET", "/friends/42/compare?media_type=anime", None).await;
    let row = |id: i64| b["shared"].as_array().unwrap().iter().find(|r| r["anilist_id"] == id).unwrap().clone();
    // PRD §7 Shared, with FR-82's fields.
    let keys: Vec<String> = row(1).as_object().unwrap().keys().cloned().collect();
    for key in ["anilist_id", "media_id", "media_type", "title", "title_english", "cover_url", "your_score", "their_score", "delta",
        "rated_in_app", "your_criteria", "their_status", "their_progress"] {
        assert!(keys.contains(&key.to_string()), "shared row missing {key}: {keys:?}");
    }
    assert_eq!(
        (&row(1)["rated_in_app"], &row(1)["your_criteria"], &row(1)["their_status"], &row(1)["their_progress"]),
        (&json!(true), &json!({"story": 9.0, "characters": 8.0, "animation": 7.0, "audio": 6.0, "enjoyment": 10.0}), &json!("watching"), &json!(5))
    );
    // The Pull-seeded rating has no real criteria to show.
    assert_eq!((&row(2)["rated_in_app"], &row(2)["your_criteria"], &row(2)["their_progress"]), (&json!(false), &Value::Null, &Value::Null));
    // Disagreements and agreements carry the same fields.
    assert!(b["disagreements"].as_array().unwrap().iter().chain(b["agreements"].as_array().unwrap()).all(|r| r.get("rated_in_app").is_some()));

    // Manga: Art is the animation column, and there's no Audio.
    let (_, m) = call(&st, "GET", "/friends/42/compare?media_type=manga", None).await;
    assert_eq!(
        (&m["shared"][0]["your_criteria"], &m["shared"][0]["their_progress"]),
        (&json!({"story": 6.0, "characters": 7.0, "animation": 9.0, "enjoyment": 8.0}), &json!(120))
    );

    // A Pull that only updates the overall keeps a rating made here; rating
    // the seeded title here makes it real.
    st.db.apply_remote_entry(one, "completed", 12, 0, Some(6.0), "2026-02-01T00:00:00.000000+00:00").unwrap();
    let two = st.db.get_media_by_anilist_id(2).unwrap().unwrap().id;
    st.db.upsert_rating(two, &Scores { story: 5.0, characters: 6.0, animation: 7.0, audio: 8.0, enjoyment: 9.0 }).unwrap();
    let (_, b) = call(&st, "GET", "/friends/42/compare?media_type=anime", None).await;
    let flags: Vec<(i64, bool)> = b["shared"].as_array().unwrap().iter().map(|r| (r["anilist_id"].as_i64().unwrap(), r["rated_in_app"].as_bool().unwrap())).collect();
    assert!(flags.contains(&(1, true)) && flags.contains(&(2, true)), "{flags:?}");
}

#[test]
fn a_library_from_before_rating_source_keeps_every_row_and_ratings_are_classified_by_rule() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(Db::open(&path).unwrap());
    // Back to the tables as the previous build left them: no media.popularity,
    // rating.source, friend_entry.popularity or friend_entry.progress.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "ALTER TABLE media DROP COLUMN popularity;
         ALTER TABLE friend_entry DROP COLUMN popularity;
         ALTER TABLE friend_entry DROP COLUMN progress;
         DROP TABLE rating;
         CREATE TABLE rating (media_id INTEGER PRIMARY KEY REFERENCES media(id), story REAL NOT NULL, characters REAL NOT NULL,
             animation REAL, audio REAL NOT NULL DEFAULT 0.0, enjoyment REAL NOT NULL, overall REAL NOT NULL, updated_at TEXT NOT NULL);
         INSERT INTO media (id, anilist_id, media_type, title, status) VALUES
             (1, 101, 'anime', 'Seeded anime', 'completed'), (2, 102, 'manga', 'Seeded manga', 'completed'),
             (3, 103, 'anime', 'Mixed', 'completed'), (4, 104, 'anime', 'Audio differs', 'completed'),
             (5, 105, 'anime', 'No animation', 'completed'), (6, 106, 'manga', 'Mixed manga', 'reading');
         INSERT INTO rating VALUES
             (1, 8.0, 8.0, 8.0, 8.0, 8.0, 8.0, '2026-01-01T00:00:00.000000+00:00'),
             (2, 7.0, 7.0, 7.0, 0.0, 7.0, 7.25, '2026-01-01T00:00:00.000000+00:00'),
             (3, 9.0, 8.0, 7.0, 6.0, 10.0, 8.0, '2026-01-01T00:00:00.000000+00:00'),
             (4, 8.0, 8.0, 8.0, 5.0, 8.0, 7.4, '2026-01-01T00:00:00.000000+00:00'),
             (5, 8.0, 8.0, NULL, 8.0, 8.0, 8.0, '2026-01-01T00:00:00.000000+00:00'),
             (6, 6.0, 7.0, 8.0, 0.0, 9.0, 7.5, '2026-01-01T00:00:00.000000+00:00');
         INSERT INTO friend (anilist_user_id, name, added_at, source, fetched_at) VALUES (42, 'Pal', '2026-01-01T00:00:00.000000+00:00', 'anilist', '2026-01-01T00:00:00.000000+00:00');
         INSERT INTO friend_entry (friend_id, anilist_id, media_type, status, score, title, genres) VALUES (42, 101, 'anime', 'completed', 9.0, 'Seeded anime', '[]');",
    )
    .unwrap();
    let tables = ["media", "rating", "friend", "friend_entry"];
    let before: Vec<Vec<String>> = tables.iter().map(|t| dump(&conn, t)).collect();
    drop(conn);

    for _ in 0..2 {
        // Opening migrates once; opening again changes nothing.
        drop(Db::open(&path).unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        let after: Vec<Vec<String>> = tables.iter().map(|t| dump(&conn, t)).collect();
        // Every old value where it was; only the new columns follow them.
        let with = |rows: &Vec<String>, extra: &dyn Fn(usize) -> String| rows.iter().enumerate().map(|(i, r)| format!("{r}|{}", extra(i))).collect::<Vec<_>>();
        assert_eq!(after[0], with(&before[0], &|_| "Null".into())); // media.popularity
        assert_eq!(after[2], before[2]); // friend: untouched
        assert_eq!(after[3], with(&before[3], &|_| "Null|Null".into())); // friend_entry.popularity, .progress
        // rating.source, by rule: all active criteria equal means AniList.
        let src = ["anilist", "anilist", "app", "app", "app", "app"];
        assert_eq!(after[1], with(&before[1], &|i| format!("{:?}", rusqlite::types::ValueRef::Text(src[i].as_bytes()))));
    }
}

#[tokio::test]
async fn completed_lists_break_score_ties_by_popularity_and_list_every_title() {
    let st = offline_state().await;
    login(&st);
    // They finished these, you don't have them: (id, score, popularity).
    let theirs = [(100, 9.0, Some(50)), (101, 9.0, Some(500)), (102, 9.0, None), (103, 10.0, Some(1)), (104, 8.0, Some(999)),
        (105, 9.0, Some(500)), (106, 8.5, Some(7)), (107, 8.5, None), (108, 8.5, Some(70)), (109, 8.5, Some(70)), (110, 8.5, Some(3)), (111, 8.5, Some(9))];
    let entries = theirs.iter().map(|&(id, score, pop)| {
        let mut e = their(id, "anime", "completed", Some(score), &["Action"]);
        e.popularity = pop;
        e
    }).collect();
    st.db.replace_friend_list(&friend_list(42, "Pal", entries), "2026-01-01T00:00:00+00:00").unwrap();
    // You finished these, they don't have them.
    for (id, score, pop) in [(200, 9.0, None), (201, 9.0, Some(10)), (202, 9.5, Some(1)), (203, 9.0, Some(10)), (204, 8.0, Some(5000))] {
        let mut f = fields(id, "anime");
        f.popularity = pop;
        let media_id = st.db.insert_or_update_media(&f, Some("completed")).unwrap();
        st.db.upsert_rating(media_id, &scores(score)).unwrap();
    }

    let (_, b) = call(&st, "GET", "/friends/42/compare?media_type=anime", None).await;
    // Score first, then more popular, unknown last, then id. Every title is
    // in the response (12 here); the page shows the first 10.
    assert_eq!(ids(&b["they_completed"]), vec![103, 101, 105, 100, 102, 108, 109, 111, 106, 110, 107, 104]);
    assert_eq!(ids(&b["you_completed"]), vec![202, 201, 203, 200, 204]);
}

#[tokio::test]
async fn pull_records_popularity_for_titles_already_in_the_library() {
    let fake = fake_anilist(|q, v| {
        let node = |id: i64, pop: i64| {
            let mut n = list_node(id, "ANIME", 1_700_000_000, 0.0);
            n["media"]["popularity"] = json!(pop);
            n
        };
        match (q.contains("MediaListCollection"), v["type"].as_str()) {
            (true, Some("ANIME")) => (200, collection(vec![node(1, 1234), node(5, 77)]), None),
            (true, Some("MANGA")) => (200, collection(vec![]), None),
            _ => (500, json!({}), None),
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    st.db.insert_or_update_media(&fields(1, "anime"), Some("completed")).unwrap();
    assert_eq!(st.db.get_media_by_anilist_id(1).unwrap().unwrap().fields.popularity, None);

    sync::pull(&st).await.unwrap();
    let pop = |id| st.db.get_media_by_anilist_id(id).unwrap().unwrap().fields.popularity;
    assert_eq!((pop(1), pop(5)), (Some(1234), Some(77))); // already there, and newly imported
    assert!(fake.calls().iter().all(|(q, _)| !q.contains("Media(id")), "no extra lookups");
    // A re-import that doesn't know the popularity keeps the stored one.
    st.db.insert_or_update_media(&fields(1, "anime"), None).unwrap();
    assert_eq!(pop(1), Some(1234));
}

#[tokio::test]
async fn disagreements_are_always_the_five_largest_gaps_and_close_friends_rarely_disagree() {
    let st = offline_state().await;
    login(&st);
    // Eight shared anime, every gap 0.5 or 0: a friend who scores like you.
    let pairs = [(1, 9.0, 8.5), (2, 8.5, 8.5), (3, 8.0, 7.5), (4, 7.5, 7.5), (5, 7.0, 6.5), (6, 6.5, 6.5), (7, 6.0, 5.5), (8, 5.5, 5.5)];
    let mut entries = Vec::new();
    for (id, yours, theirs) in pairs {
        mine(&st, id, "anime", "completed", Some(yours), &["Action"]);
        entries.push(their(id, "anime", "completed", Some(theirs), &["Action"]));
    }
    // Manga is its own pool: one big gap, one none.
    for (id, yours, theirs) in [(50, 9.0, 5.0), (51, 7.0, 7.0)] {
        mine(&st, id, "manga", "completed", Some(yours), &["Drama"]);
        entries.push(their(id, "manga", "completed", Some(theirs), &["Drama"]));
    }
    // A friend with no titles in common.
    mine(&st, 60, "anime", "completed", Some(8.0), &["Action"]);
    st.db.replace_friend_list(&friend_list(42, "Pal", entries), "2026-01-01T00:00:00+00:00").unwrap();
    st.db.replace_friend_list(&friend_list(43, "Amy", vec![their(61, "anime", "completed", Some(8.0), &["Action"])]), "2026-01-01T00:00:00+00:00").unwrap();

    let (_, b) = call(&st, "GET", "/friends/42/compare?media_type=anime", None).await;
    // Half-point gaps still fill the card: the four 0.5s (higher average
    // first), then the best-liked of the exact matches.
    assert_eq!(ids(&b["disagreements"]), vec![1, 3, 5, 7, 2]);
    assert_eq!(ids(&b["agreements"]), vec![4, 6, 8]);
    assert_eq!(b["rarely_disagree"], json!(true));
    let (dis, agr) = (ids(&b["disagreements"]), ids(&b["agreements"]));
    assert!(dis.iter().all(|id| !agr.contains(id)), "a title in both lists");

    let (_, m) = call(&st, "GET", "/friends/42/compare?media_type=manga", None).await;
    assert_eq!((ids(&m["disagreements"]), ids(&m["agreements"]), &m["rarely_disagree"]), (vec![50, 51], vec![], &json!(false)));

    let (_, none) = call(&st, "GET", "/friends/43/compare?media_type=anime", None).await;
    assert_eq!((&none["overlap"], ids(&none["disagreements"]), ids(&none["agreements"]), &none["rarely_disagree"]), (&json!(0), vec![], vec![], &json!(false)));
}

#[tokio::test]
async fn a_gap_under_a_point_is_a_disagreement_and_one_past_half_a_point_ends_rarely_disagree() {
    let st = offline_state().await;
    login(&st);
    // Half-point gaps from awkward decimals (7.1/6.6, 8.3/7.8) and exact
    // matches, plus title 9's 0.8: under the old 1.0 rule, not a disagreement.
    let pairs = [(1, 7.1, 6.6), (2, 8.3, 7.8), (3, 6.0, 6.0), (4, 9.0, 9.5), (5, 7.5, 7.5), (6, 6.5, 6.5), (9, 8.0, 7.2)];
    let mut with_9 = Vec::new();
    for (id, yours, theirs) in pairs {
        mine(&st, id, "anime", "completed", Some(yours), &["Action"]);
        with_9.push(their(id, "anime", "completed", Some(theirs), &["Action"]));
    }
    let without_9 = with_9.iter().filter(|e| e.anilist_id != 9).cloned().collect();
    st.db.replace_friend_list(&friend_list(42, "Pal", with_9), "2026-01-01T00:00:00+00:00").unwrap();
    st.db.replace_friend_list(&friend_list(43, "Amy", without_9), "2026-01-01T00:00:00+00:00").unwrap();

    let (_, b) = call(&st, "GET", "/friends/42/compare?media_type=anime", None).await;
    assert_eq!((ids(&b["disagreements"]), ids(&b["agreements"])), (vec![9, 4, 2, 1, 5], vec![6, 3]));
    assert_eq!(b["rarely_disagree"], json!(false), "0.8 is past half a point");

    let (_, b) = call(&st, "GET", "/friends/43/compare?media_type=anime", None).await;
    assert_eq!((ids(&b["disagreements"]), ids(&b["agreements"])), (vec![4, 2, 1, 5, 6], vec![3]));
    assert_eq!(b["rarely_disagree"], json!(true), "every gap is 0.5 or less");
}

#[tokio::test]
async fn taste_match_needs_five_shared_titles_and_your_side_is_your_local_overall() {
    let st = offline_state().await;
    login(&st);
    // Manga overall is the unweighted 4-criterion average, half-to-even: 7.25 -> 7.2.
    let id = st.db.insert_or_update_media(&fields(1, "manga"), Some("completed")).unwrap();
    st.db.upsert_rating(id, &Scores { story: 7.0, characters: 7.0, animation: 7.5, audio: 0.0, enjoyment: 7.5 }).unwrap();
    for anilist_id in 2..=4 {
        mine(&st, anilist_id, "manga", "completed", Some(6.0), &["Drama"]);
    }
    let entries = (1..=4).map(|anilist_id| their(anilist_id, "manga", "completed", Some(6.0), &["Drama"])).collect();
    st.db.replace_friend_list(&friend_list(42, "Pal", entries), "2026-01-01T00:00:00+00:00").unwrap();

    let (_, b) = call(&st, "GET", "/friends/42/compare?media_type=manga", None).await;
    assert_eq!((&b["overlap"], &b["taste_match"]), (&json!(4), &Value::Null));
    assert!((b["skew"].as_f64().unwrap() - 0.3).abs() < 1e-9); // (1.2 + 0 + 0 + 0) / 4
    assert_eq!((&b["shared"][0]["your_score"], &b["shared"][0]["delta"]), (&json!(7.2), &json!(1.2)));

    mine(&st, 5, "manga", "completed", Some(6.0), &["Drama"]);
    let entries = (1..=5).map(|anilist_id| their(anilist_id, "manga", "completed", Some(6.0), &["Drama"])).collect();
    st.db.replace_friend_list(&friend_list(42, "Pal", entries), "2026-01-01T00:00:00+00:00").unwrap();
    let (_, b) = call(&st, "GET", "/friends/42/compare?media_type=manga", None).await;
    // Mean gap 1.2 / 5 = 0.24 -> 100 * (1 - 0.24 / 9) = 97.3 -> 97.
    assert_eq!((&b["overlap"], &b["taste_match"]), (&json!(5), &json!(97)));
}

#[tokio::test]
async fn friend_routes_return_prd_shapes_and_remove_is_local_only() {
    let fake = fake_anilist(|_, _| (500, json!({}), None)).await;
    let st = state_with(&fake);
    login(&st);
    st.db.replace_friend_list(
        &friend_list(42, "Pal", vec![their(3, "anime", "completed", Some(7.5), &[]), their(4, "anime", "planning", None, &[])]),
        "2026-01-01T00:00:00+00:00",
    ).unwrap();
    st.db.replace_friend_list(&friend_list(43, "amy", vec![their(3, "anime", "watching", Some(9.0), &[])]), "2026-01-02T00:00:00+00:00").unwrap();

    let (s, b) = call(&st, "GET", "/friends", None).await;
    assert_eq!(s, 200);
    // Alphabetical, ignoring case.
    assert_eq!(
        b["friends"],
        json!([
            {"anilist_user_id": 43, "name": "amy", "avatar_url": null, "source": "manual", "list_status": "fetched", "fetched_at": "2026-01-02T00:00:00+00:00", "anime_count": 1, "manga_count": 0},
            {"anilist_user_id": 42, "name": "Pal", "avatar_url": null, "source": "manual", "list_status": "fetched", "fetched_at": "2026-01-01T00:00:00+00:00", "anime_count": 2, "manga_count": 0},
        ])
    );

    // The detail view's friend scores: scored entries only, highest first.
    // Titles not in your library work too.
    let (s, b) = call(&st, "GET", "/media/by-anilist/3/friend-scores", None).await;
    assert_eq!(s, 200);
    assert_eq!(
        b,
        json!({"scores": [
            {"anilist_user_id": 43, "name": "amy", "avatar_url": null, "status": "watching", "score": 9.0},
            {"anilist_user_id": 42, "name": "Pal", "avatar_url": null, "status": "completed", "score": 7.5},
        ]})
    );
    assert_eq!(call(&st, "GET", "/media/by-anilist/4/friend-scores", None).await, (200, json!({"scores": []})));

    assert_eq!(call(&st, "DELETE", "/friends/42", None).await, (200, json!({"deleted": true, "hidden": false})));
    assert_eq!(call(&st, "DELETE", "/friends/42", None).await.0, 404);
    let (_, b) = call(&st, "GET", "/media/by-anilist/3/friend-scores", None).await;
    assert_eq!(b["scores"].as_array().unwrap().len(), 1);
    assert!(fake.calls().is_empty()); // none of this calls AniList
}

/// Every row of a table, as text, in rowid order.
fn dump(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("SELECT * FROM {table} ORDER BY rowid")).unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| Ok((0..n).map(|i| format!("{:?}", r.get_ref(i).unwrap())).collect::<Vec<_>>().join("|")))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn friend_tables_are_added_to_an_existing_library_db_without_touching_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    {
        let db = Db::open(&path).unwrap();
        let anime = db.insert_or_update_media(&fields(1, "anime"), Some("completed")).unwrap();
        let manga = db.insert_or_update_media(&fields(2, "manga"), Some("reading")).unwrap();
        db.upsert_rating(anime, &scores(8.5)).unwrap();
        db.upsert_rating(manga, &scores(7.0)).unwrap();
        db.save_auth("token", None, 1, "tester", Some("POINT_10_DECIMAL")).unwrap();
    }
    // A library.db from before this change: no friend tables.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("DROP TABLE friend_entry; DROP TABLE friend; DROP TABLE friend_sync;").unwrap();
    let before: Vec<_> = ["media", "rating", "anilist_auth"].iter().map(|t| dump(&conn, t)).collect();
    drop(conn);

    let db = Db::open(&path).unwrap();
    assert!(db.get_friends().unwrap().is_empty());
    db.replace_friend_list(&friend_list(42, "Pal", vec![their(1, "anime", "completed", Some(9.0), &[])]), "2026-01-01T00:00:00+00:00").unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let after: Vec<_> = ["media", "rating", "anilist_auth"].iter().map(|t| dump(&conn, t)).collect();
    assert_eq!(before, after);
    assert_eq!(before[0].len(), 2);

    // Opening again is a no-op, and removing a friend cascades to their entries.
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.get_friends().unwrap().len(), 1);
    assert_eq!(db.delete_friend(42).unwrap(), Some(false)); // deleted, not hidden
    let left: i64 = conn.query_row("SELECT COUNT(*) FROM friend_entry", [], |r| r.get(0)).unwrap();
    assert_eq!(left, 0);
}

#[test]
fn a_friend_table_from_before_friend_sync_is_migrated_without_changing_a_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    {
        let db = Db::open(&path).unwrap();
        let anime = db.insert_or_update_media(&fields(1, "anime"), Some("completed")).unwrap();
        db.upsert_rating(anime, &scores(8.5)).unwrap();
        db.save_auth("token", None, 1, "tester", Some("POINT_10_DECIMAL")).unwrap();
    }
    // The friend tables as the first Friend Comparison build created them.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE friend_entry; DROP TABLE friend; DROP TABLE friend_sync;
         CREATE TABLE friend (anilist_user_id INTEGER PRIMARY KEY, name TEXT NOT NULL, avatar_url TEXT, fetched_at TEXT NOT NULL);
         CREATE TABLE friend_entry (
             friend_id INTEGER NOT NULL REFERENCES friend(anilist_user_id) ON DELETE CASCADE,
             anilist_id INTEGER NOT NULL,
             media_type TEXT NOT NULL CHECK (media_type IN ('anime','manga')),
             status TEXT NOT NULL, score REAL, title TEXT, title_english TEXT, cover_url TEXT, genres TEXT,
             PRIMARY KEY (friend_id, anilist_id));
         INSERT INTO friend VALUES (42, 'Pal', 'https://img/u.png', '2026-01-01T00:00:00.000000+00:00'),
                                   (43, 'amy', NULL, '2026-02-01T00:00:00.000000+00:00');
         INSERT INTO friend_entry VALUES (42, 1, 'anime', 'completed', 9.0, 'Title 1', NULL, NULL, '[\"Action\"]'),
                                         (43, 2, 'manga', 'reading', NULL, 'Title 2', NULL, NULL, '[]');",
    )
    .unwrap();
    let tables = ["media", "rating", "anilist_auth", "friend_entry"];
    let mut before: Vec<_> = tables.iter().map(|t| dump(&conn, t)).collect();
    drop(conn);
    // friend_entry gains popularity and progress (FR-77, FR-82): every old value stays, NULL after them.
    before[3] = before[3].iter().map(|r| format!("{r}|Null|Null")).collect();

    type Row = (i64, String, Option<String>, String, String, Option<String>, Option<String>, i64, Option<String>);
    for _ in 0..2 {
        // Opening migrates once; opening again changes nothing.
        drop(Db::open(&path).unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(tables.iter().map(|t| dump(&conn, t)).collect::<Vec<_>>(), before);
        let cols: Vec<String> = conn.prepare("SELECT name FROM pragma_table_info('friend')").unwrap()
            .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(cols, ["anilist_user_id", "name", "avatar_url", "added_at", "source", "fetched_at", "private_at", "hidden", "retry_at"]);
        // Every old value where it was; the old fetch time also fills the new fetched_at.
        let rows: Vec<Row> = conn.prepare("SELECT * FROM friend ORDER BY anilist_user_id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))
            .unwrap().map(Result::unwrap).collect();
        let row = |id, name: &str, avatar: Option<&str>, at: &str| (id, name.into(), avatar.map(Into::into), at.into(), "manual".into(), Some(at.into()), None, 0, None);
        assert_eq!(rows, vec![
            row(42, "Pal", Some("https://img/u.png"), "2026-01-01T00:00:00.000000+00:00"),
            row(43, "amy", None, "2026-02-01T00:00:00.000000+00:00"),
        ]);
    }
    let friends = Db::open(&path).unwrap().get_friends().unwrap();
    let seen: Vec<_> = friends.iter().map(|f| (f.anilist_user_id, f.source.as_str(), f.list_status.as_str(), f.anime_count + f.manga_count)).collect();
    assert_eq!(seen, vec![(43, "manual", "fetched", 1), (42, "manual", "fetched", 1)]);
}

#[test]
fn a_friend_table_without_retry_at_only_gains_that_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(Db::open(&path).unwrap());
    // `friend` as the first friend-sync build created it.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE friend_entry; DROP TABLE friend;
         CREATE TABLE friend (anilist_user_id INTEGER PRIMARY KEY, name TEXT NOT NULL, avatar_url TEXT, added_at TEXT NOT NULL,
             source TEXT NOT NULL DEFAULT 'manual' CHECK (source IN ('anilist','manual')), fetched_at TEXT, private_at TEXT,
             hidden INTEGER NOT NULL DEFAULT 0);
         INSERT INTO friend VALUES (42, 'Pal', NULL, '2026-01-01T00:00:00.000000+00:00', 'anilist', NULL, '2026-01-02T00:00:00.000000+00:00', 1);",
    )
    .unwrap();
    let before = dump(&conn, "friend");
    drop(conn);
    drop(Db::open(&path).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    let after = dump(&conn, "friend");
    assert_eq!(after, before.iter().map(|r| format!("{r}|Null")).collect::<Vec<_>>());
}

// ---------------------------------------------------------------- friend sync ----
// PRD FR-79 to FR-81, against `follow_anilist`. The logged-in account is
// tester (id 1); nothing here may carry its token.

fn follows(following: &[i64], followers: &[i64]) -> Arc<Mutex<Follows>> {
    Arc::new(Mutex::new(Follows { following: following.to_vec(), followers: followers.to_vec(), ..Default::default() }))
}

/// A logged-in state on a temp-file database, so a test can age rows through
/// a second connection.
fn file_state(fake: &Fake) -> (Shared, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("library.db")).unwrap();
    let st = AppState::new(db, AniList::with_url(&fake.url), Config { client_id: "1".into() });
    login(&st);
    (st, dir)
}

fn sql(dir: &tempfile::TempDir, sql: &str) {
    rusqlite::Connection::open(dir.path().join("library.db")).unwrap().execute_batch(sql).unwrap();
}

fn days_ago(days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

/// GET /friends as (id, source, list_status), by id.
async fn friend_rows(st: &Shared) -> Vec<(i64, String, String)> {
    let (_, b) = call(st, "GET", "/friends", None).await;
    let mut rows: Vec<_> = b["friends"].as_array().unwrap().iter()
        .map(|f| (f["anilist_user_id"].as_i64().unwrap(), f["source"].as_str().unwrap().into(), f["list_status"].as_str().unwrap().into()))
        .collect();
    rows.sort();
    rows
}

fn row(id: i64, source: &str, status: &str) -> (i64, String, String) {
    (id, source.into(), status.into())
}

fn list_fetches(fake: &Fake) -> usize {
    fake.calls_containing("anime: MediaListCollection").len()
}

#[tokio::test]
async fn friend_sync_imports_every_page_of_who_you_follow_and_fetches_no_lists() {
    let graph = follows(&[100, 101, 102, 45, 48], &[]);
    let fake = follow_anilist(graph.clone()).await;
    let st = state_with(&fake);
    login(&st);

    let (s, b) = call(&st, "POST", "/friends/sync", None).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(b, call(&st, "GET", "/friends", None).await.1); // PRD §7: same shape as GET /friends
    assert!(b["synced_at"].as_str().is_some_and(|t| t.starts_with("20")), "{b}");
    assert_eq!(b["mutual_only"], json!(false));
    let stars = b["friends"].as_array().unwrap().iter().find(|f| f["anilist_user_id"] == 45).unwrap();
    assert_eq!(*stars, json!({
        "anilist_user_id": 45, "name": "stars", "avatar_url": "https://img/45.png", "source": "anilist",
        "list_status": "pending", "fetched_at": null, "anime_count": 0, "manga_count": 0,
    }));
    let all = ["anilist", "pending"];
    assert_eq!(friend_rows(&st).await, [45, 48, 100, 101, 102].map(|id| row(id, all[0], all[1])));

    // Three pages of who tester follows, logged out, and no lists yet.
    let calls = fake.calls();
    assert_eq!(calls.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>(), [1, 2, 3].map(|p| json!({"userId": 1, "page": p})));
    assert!(calls.iter().all(|(q, _)| q.contains("following(")));
    // FR-78: detail-page chips only show friends whose lists are cached.
    assert_eq!(call(&st, "GET", "/media/by-anilist/1/friend-scores", None).await, (200, json!({"scores": []})));

    // Failing part-way changes nothing: page 1 (100, 101) arrives, page 2 fails,
    // and 45, 48 and 102 must not be taken for unfollowed.
    {
        let mut g = graph.lock().unwrap();
        g.following = vec![100, 101, 102, 103];
        g.failing_page = Some(2);
    }
    assert_eq!(call(&st, "POST", "/friends/sync", None).await.0, 502);
    graph.lock().unwrap().failing_page = None;
    let (_, unchanged) = call(&st, "GET", "/friends", None).await;
    assert_eq!(unchanged, b);
    assert!(fake.auths().iter().all(Option::is_none), "{:?}", fake.auths());
}

fn friend_ids(b: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = b["friends"].as_array().unwrap().iter().map(|f| f["anilist_user_id"].as_i64().unwrap()).collect();
    ids.sort();
    ids
}

/// Each request since `from`: (which follow list, whether it carried the token).
fn follow_requests(fake: &Fake, from: usize) -> Vec<(&'static str, bool)> {
    fake.calls()[from..].iter().zip(&fake.auths()[from..])
        .map(|((q, _), auth)| (if q.contains("following(") { "following" } else if q.contains("followers(") { "followers" } else { "other" }, auth.is_some()))
        .collect()
}

#[tokio::test]
async fn mutual_only_reads_your_own_following_list_with_your_token_and_no_followers() {
    let graph = follows(&[100, 101, 102], &[101, 102, 103]);
    let fake = follow_anilist(graph).await;
    let st = state_with(&fake);
    login(&st);

    assert_eq!(call(&st, "PUT", "/friends/settings", Some(json!({"mutual_only": true}))).await, (200, json!({"mutual_only": true})));
    assert!(fake.calls().is_empty()); // the setting alone is local
    let (s, b) = call(&st, "POST", "/friends/sync", None).await;
    assert_eq!((s, &b["mutual_only"], friend_ids(&b)), (200, &json!(true), vec![101, 102]));
    // As many requests as an everyone sync: both following pages, read with
    // your token so AniList can say who follows you back. No followers pages.
    assert_eq!(follow_requests(&fake, 0), [("following", true), ("following", true)]);
    assert!(fake.calls().iter().all(|(q, _)| q.contains("isFollower") && q.contains("Viewer")));
    assert_eq!(fake.auths()[0].as_deref(), Some("Bearer test-token"));

    // The token is for your own following list only: friends' lists never
    // get it, and an everyone sync doesn't need it.
    let n = fake.calls().len();
    assert_eq!(call(&st, "POST", "/friends/101/refresh", None).await.0, 200);
    call(&st, "PUT", "/friends/settings", Some(json!({"mutual_only": false}))).await;
    assert_eq!(friend_ids(&call(&st, "POST", "/friends/sync", None).await.1), vec![100, 101, 102]);
    assert!(fake.auths()[n..].iter().all(Option::is_none), "{:?}", fake.auths());
    call(&st, "PUT", "/friends/settings", Some(json!({"mutual_only": true}))).await;
    assert_eq!(friend_ids(&call(&st, "POST", "/friends/sync", None).await.1), vec![101, 102]);
    assert_eq!(call(&st, "PUT", "/friends/settings", Some(json!({"mutual_only": "yes"}))).await.0, 422);
    assert!(fake.calls().iter().all(|(q, _)| !q.contains("mutation")));
}

#[tokio::test]
async fn mutual_only_falls_back_to_your_followers_list_when_anilist_does_not_vouch_for_is_follower() {
    let graph = follows(&[100, 101, 102], &[101, 102, 103]);
    let fake = follow_anilist(graph.clone()).await;
    let st = state_with(&fake);
    login(&st);
    call(&st, "PUT", "/friends/settings", Some(json!({"mutual_only": true}))).await;

    // The token answered as if logged out: no Viewer, isFollower false for
    // everyone. Trusting that would drop every friend; the followers list decides.
    graph.lock().unwrap().token_ignored = true;
    let (s, b) = call(&st, "POST", "/friends/sync", None).await;
    assert_eq!((s, friend_ids(&b)), (200, vec![101, 102]));
    assert_eq!(follow_requests(&fake, 0), [("following", true), ("following", true), ("followers", false), ("followers", false)]);

    // The token rejected outright: your following list is read again, logged out.
    let n = fake.calls().len();
    {
        let mut g = graph.lock().unwrap();
        g.token_ignored = false;
        g.reject_token = true;
    }
    let (s, b) = call(&st, "POST", "/friends/sync", None).await;
    assert_eq!((s, friend_ids(&b)), (200, vec![101, 102]));
    assert_eq!(
        follow_requests(&fake, n),
        [("following", true), ("following", false), ("following", false), ("followers", false), ("followers", false)]
    );
}

#[tokio::test]
async fn unfollowing_removes_friends_from_anilist_only_and_removing_one_you_follow_hides_them() {
    let graph = follows(&[100, 101, 102], &[]);
    let fake = follow_anilist(graph.clone()).await;
    let st = state_with(&fake);
    login(&st);
    call(&st, "POST", "/friends/sync", None).await;
    // 100's list is stored, so it must go with them later.
    assert_eq!(call(&st, "POST", "/friends/100/refresh", None).await.0, 200);
    assert_eq!(st.db.friend_entries(100, "anime").unwrap().len(), 3);
    // Pal is added by hand, and so is 101, which makes a followed friend manual.
    for username in ["pal", "u101"] {
        assert_eq!(call(&st, "POST", "/friends", Some(json!({"username": username}))).await.0, 200, "{username}");
    }
    // Removing someone you still follow hides them (FR-70).
    assert_eq!(call(&st, "DELETE", "/friends/102", None).await, (200, json!({"deleted": true, "hidden": true})));
    for (method, path) in [("DELETE", "/friends/102"), ("POST", "/friends/102/refresh"), ("GET", "/friends/102/compare?media_type=anime")] {
        assert_eq!(call(&st, method, path, None).await.0, 404, "{path}");
    }
    assert_eq!(friend_rows(&st).await, vec![row(42, "manual", "fetched"), row(100, "anilist", "fetched"), row(101, "manual", "fetched")]);

    // Unfollow 100 and 101 on AniList; 102 is still followed, and still hidden.
    graph.lock().unwrap().following = vec![102];
    assert_eq!(call(&st, "POST", "/friends/sync", None).await.0, 200);
    assert_eq!(friend_rows(&st).await, vec![row(42, "manual", "fetched"), row(101, "manual", "fetched")]);
    assert!(st.db.friend_entries(100, "anime").unwrap().is_empty());

    // Adding a hidden friend by username brings them back, as a manual friend.
    assert_eq!(call(&st, "POST", "/friends", Some(json!({"username": "u102"}))).await.0, 200);
    // A manual friend is simply deleted.
    assert_eq!(call(&st, "DELETE", "/friends/42", None).await, (200, json!({"deleted": true, "hidden": false})));
    assert_eq!(friend_rows(&st).await, vec![row(101, "manual", "fetched"), row(102, "manual", "fetched")]);
    assert!(st.db.friend_entries(42, "anime").unwrap().is_empty());
    assert!(fake.auths().iter().all(Option::is_none));
}

#[tokio::test]
async fn the_list_queue_takes_one_friend_per_turn_never_fetched_first_then_the_oldest() {
    // The real pace: a turn every 12 s at most, two requests each, 10 a minute.
    assert!(sync::FRIEND_QUEUE_SPACING >= Duration::from_secs(12));
    let fake = follow_anilist(follows(&[100, 101, 102], &[])).await;
    let (st, dir) = file_state(&fake);
    call(&st, "POST", "/friends/sync", None).await;
    for id in [101, 102] {
        assert_eq!(call(&st, "POST", &format!("/friends/{id}/refresh"), None).await.0, 200);
    }
    // 100 never fetched, 102 two days ago, 101 just now (fresh).
    sql(&dir, &format!("UPDATE friend SET fetched_at = '{}' WHERE anilist_user_id = 102", days_ago(2)));

    let before = fake.calls().len();
    let spacing = Duration::from_millis(150);
    let queue = tokio::spawn(sync::friend_queue_loop(st.clone(), spacing));
    wait_for(|| list_fetches(&fake) >= 4).await;
    tokio::time::sleep(spacing * 3).await; // then nothing: every list is fresh
    queue.abort();

    let calls = &fake.calls()[before..];
    let lookups: Vec<&Value> = calls.iter().filter(|(q, _)| q.contains("User(")).map(|(_, v)| v).collect();
    assert_eq!(lookups, [&json!({"id": 100}), &json!({"id": 102})]);
    assert_eq!(calls.len(), 4); // two requests per friend
    let times = &fake.times()[before..];
    assert!(times[2] - times[0] >= spacing, "turns {:?} apart", times[2] - times[0]);
    assert_eq!(friend_rows(&st).await, [100, 101, 102].map(|id| row(id, "anilist", "fetched")));
    assert!(fake.auths().iter().all(Option::is_none));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_list_queue_waits_while_other_anilist_calls_run_and_after_a_429() {
    let graph = follows(&[100, 101], &[]);
    let fake = follow_anilist(graph.clone()).await;
    let st = state_with(&fake);
    login(&st);
    call(&st, "POST", "/friends/sync", None).await;

    // A Discover request is in flight: the queue's turn does nothing.
    graph.lock().unwrap().hold_discover = true;
    let busy = tokio::spawn({
        let st = st.clone();
        async move { call(&st, "GET", "/discover?section=trending&media_type=anime", None).await.0 }
    });
    wait_for(|| !st.anilist.is_quiet()).await;
    assert!(!st.anilist.is_quiet());
    let before = fake.calls().len();
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), None);
    assert_eq!(fake.calls().len(), before);
    graph.lock().unwrap().hold_discover = false;
    assert_eq!(busy.await.unwrap(), 200);
    assert!(st.anilist.is_quiet());
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), Some(100));

    // A 429 on any call pauses the queue, even after that call got through.
    graph.lock().unwrap().throttle_next = true;
    assert_eq!(call(&st, "POST", "/friends/sync", None).await.0, 200);
    assert!(!st.anilist.is_quiet());
    let before = fake.calls().len();
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), None);
    assert_eq!(fake.calls().len(), before);
    assert!(fake.auths().iter().all(Option::is_none));
}

#[tokio::test]
async fn opening_a_comparison_fetches_a_missing_or_stale_list_first() {
    let graph = follows(&[100, 101, 48], &[]);
    let fake = follow_anilist(graph.clone()).await;
    let (st, dir) = file_state(&fake);
    call(&st, "POST", "/friends/sync", None).await;
    let compare = |id: i64| format!("/friends/{id}/compare?media_type=anime");

    // Never fetched: fetched first, ahead of the queue (two requests).
    let n = fake.calls().len();
    let (s, b) = call(&st, "GET", &compare(100), None).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!((&b["friend"]["list_status"], &b["friend"]["anime_count"]), (&json!("fetched"), &json!(3)));
    assert_eq!((fake.calls().len() - n, &fake.calls()[n].1), (2, &json!({"id": 100})));
    // Fresh now: the next comparison reads the stored copy only.
    let n = fake.calls().len();
    assert_eq!(call(&st, "GET", "/friends/100/compare?media_type=manga", None).await.0, 200);
    assert_eq!(fake.calls().len(), n);
    // Older than 24 hours: fetched again first...
    let stale = format!("UPDATE friend SET fetched_at = '{}' WHERE anilist_user_id = 100", days_ago(2));
    sql(&dir, &stale);
    assert_eq!(call(&st, "GET", &compare(100), None).await.0, 200);
    assert_eq!(fake.calls().len(), n + 2);
    // ...and with AniList down, the stored copy still compares; with no copy, 502.
    sql(&dir, &stale);
    graph.lock().unwrap().down = true;
    let (s, b) = call(&st, "GET", &compare(100), None).await;
    assert_eq!((s, &b["friend"]["anime_count"]), (200, &json!(3)), "{b}");
    assert_eq!(call(&st, "GET", &compare(101), None).await.0, 502);
    graph.lock().unwrap().down = false;

    // A private friend is marked on the first try, then not asked again here.
    let (s, b) = call(&st, "GET", &compare(48), None).await;
    assert_eq!((s, b["detail"].as_str()), (403, Some("shy's list is private on AniList")));
    let n = fake.calls().len();
    assert_eq!(call(&st, "GET", &compare(48), None).await.0, 403);
    assert_eq!(fake.calls().len(), n);
    assert!(friend_rows(&st).await.contains(&row(48, "anilist", "private")));
    assert_eq!(call(&st, "GET", &compare(7), None).await.0, 404);
    assert!(fake.auths().iter().all(Option::is_none));
}

#[tokio::test]
async fn a_friend_whose_fetch_keeps_failing_does_not_hold_up_the_queue() {
    let fake = follow_anilist(follows(&[49, 100], &[])).await;
    let st = state_with(&fake);
    login(&st);
    call(&st, "POST", "/friends/sync", None).await;
    // 49's account is gone: tried once, then skipped while 100 goes ahead.
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), Some(49));
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), Some(100));
    let n = fake.calls().len();
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), None);
    assert_eq!(fake.calls().len(), n);
    assert_eq!(friend_rows(&st).await, vec![row(49, "anilist", "pending"), row(100, "anilist", "fetched")]);
}

#[tokio::test]
async fn private_friends_stay_listed_and_are_not_asked_again_for_a_week() {
    let fake = follow_anilist(follows(&[48], &[])).await;
    let (st, dir) = file_state(&fake);
    call(&st, "POST", "/friends/sync", None).await;
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), Some(48));
    assert_eq!(friend_rows(&st).await, vec![row(48, "anilist", "private")]);

    // Neither the queue nor a sync asks for their list again...
    let n = fake.calls().len();
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), None);
    assert_eq!(call(&st, "POST", "/friends/sync", None).await.0, 200);
    assert!(fake.calls()[n..].iter().all(|(q, _)| q.contains("following(")));
    assert_eq!(friend_rows(&st).await, vec![row(48, "anilist", "private")]);
    // ...Refresh always does...
    let n = fake.calls().len();
    let (s, b) = call(&st, "POST", "/friends/48/refresh", None).await;
    assert_eq!((s, b["detail"].as_str()), (403, Some("shy's list is private on AniList")));
    assert_eq!(fake.calls().len(), n + 2);
    // ...and a week later the queue tries once more.
    sql(&dir, &format!("UPDATE friend SET private_at = '{}'", days_ago(8)));
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), Some(48));
    assert_eq!(fake.calls().len(), n + 4);
    assert_eq!(friend_rows(&st).await, vec![row(48, "anilist", "private")]);
    assert!(fake.auths().iter().all(Option::is_none));
}

#[tokio::test]
async fn a_login_and_its_pull_sync_friends_once_without_the_token() {
    let graph = follows(&[100], &[]);
    let fake = follow_anilist(graph.clone()).await;
    let st = state_with(&fake);
    let syncs = || fake.calls().iter().filter(|(q, _)| q.contains("following(")).count();
    let (s, _, _) = oauth_call(&st, "/token?access_token=good-token&expires_in=3600").await;
    assert_eq!(s, 200);
    // The page Pulls right after login (FR-23), and that Pull's sync is the one.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(syncs(), 0, "login on its own doesn't sync");
    assert_eq!(call(&st, "POST", "/sync/pull", None).await.0, 200);
    wait_for(|| st.db.get_friends().unwrap().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(syncs(), 1, "one sync for a login plus its Pull");

    graph.lock().unwrap().following = vec![100, 101];
    assert_eq!(call(&st, "POST", "/sync/pull", None).await.0, 200);
    wait_for(|| st.db.get_friends().unwrap().len() == 2).await;
    assert_eq!(syncs(), 2, "and one per later Pull");
    // Login and Pull carry the token; the friend sync never does.
    let (calls, auths) = (fake.calls(), fake.auths());
    for ((q, _), auth) in calls.iter().zip(&auths) {
        assert_eq!(auth.is_some(), !q.contains("following("), "{q}");
    }
}

#[tokio::test]
async fn unhiding_brings_back_a_friend_you_still_follow_and_drops_one_you_no_longer_do() {
    let graph = follows(&[100, 101], &[]);
    let fake = follow_anilist(graph.clone()).await;
    let st = state_with(&fake);
    login(&st);
    call(&st, "POST", "/friends/sync", None).await;
    for id in [100, 101] {
        assert_eq!(call(&st, "DELETE", &format!("/friends/{id}"), None).await.1, json!({"deleted": true, "hidden": true}));
    }
    let hidden = |id: i64| json!({"anilist_user_id": id, "name": format!("u{id}"), "avatar_url": format!("https://img/{id}.png")});
    let (_, b) = call(&st, "GET", "/friends", None).await;
    assert_eq!((&b["friends"], &b["hidden"]), (&json!([]), &json!([hidden(100), hidden(101)])));

    // You unfollowed 101 on AniList since the last sync: unhiding just drops them.
    graph.lock().unwrap().following = vec![100];
    let n = fake.calls().len();
    let (s, b) = call(&st, "POST", "/friends/101/unhide", None).await;
    assert_eq!((s, &b["unhidden"]), (200, &json!(false)), "{b}");
    assert_eq!((&b["friends"], &b["hidden"]), (&json!([]), &json!([hidden(100)])));
    assert_eq!(follow_requests(&fake, n), [("following", false)]); // deciding took one sync
    // Still followed: back on the spot, their list next in line.
    let (s, b) = call(&st, "POST", "/friends/100/unhide", None).await;
    assert_eq!((s, &b["unhidden"], &b["hidden"]), (200, &json!(true), &json!([])));
    assert_eq!(friend_rows(&st).await, vec![row(100, "anilist", "pending")]);
    assert_eq!(call(&st, "POST", "/friends/100/unhide", None).await.0, 404); // not hidden now
    assert_eq!(call(&st, "POST", "/friends/7/unhide", None).await.0, 404);

    // With AniList down they're still unhidden; the next sync settles it.
    call(&st, "DELETE", "/friends/100", None).await;
    graph.lock().unwrap().down = true;
    let (s, b) = call(&st, "POST", "/friends/100/unhide", None).await;
    assert_eq!((s, &b["unhidden"]), (200, &json!(true)));
    assert!(fake.auths().iter().all(Option::is_none));
}

#[tokio::test]
async fn the_queue_remembers_whom_it_set_aside_across_a_restart() {
    let fake = follow_anilist(follows(&[48, 49, 100], &[])).await;
    let (st, dir) = file_state(&fake);
    call(&st, "POST", "/friends/sync", None).await;
    // 48 is private and 49's account is gone: both set aside.
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), Some(48));
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), Some(49));

    // Restart: a new app on the same database goes straight to 100.
    drop(st);
    let db = Db::open(&dir.path().join("library.db")).unwrap();
    let st = AppState::new(db, AniList::with_url(&fake.url), Config { client_id: "1".into() });
    let n = fake.calls().len();
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), Some(100));
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), None);
    assert_eq!(fake.calls().len(), n + 2);
    // An hour on, 49 is tried again; 48 still waits out its week.
    let an_hour_ago = (chrono::Utc::now() - chrono::Duration::minutes(61)).to_rfc3339_opts(chrono::SecondsFormat::Micros, false);
    sql(&dir, &format!("UPDATE friend SET retry_at = '{an_hour_ago}' WHERE anilist_user_id = 49"));
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), Some(49));
    assert_eq!(sync::friend_queue_step(&st).await.unwrap(), None);
}

// ---------------------------------------------------------------------- login ----

/// One request to the OAuth listener's router: (status, body, whether the
/// listener was told to shut down).
async fn oauth_call(st: &Shared, path: &str) -> (u16, String, bool) {
    let (done, mut closed) = tokio::sync::oneshot::channel();
    let req = Request::builder().uri(path).body(Body::empty()).unwrap();
    let resp = oauth::router(st.clone(), done).oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned(), closed.try_recv().is_ok())
}

#[tokio::test]
async fn token_from_the_callback_page_is_checked_with_anilist_then_saved() {
    let fake = fake_anilist(|q, _| {
        if q.contains("Viewer") {
            (200, json!({"data": {"Viewer": {"id": 42, "name": "Alice", "mediaListOptions": {"scoreFormat": "POINT_100"}}}}), None)
        } else {
            (500, json!({}), None)
        }
    })
    .await;
    let st = state_with(&fake);
    let (s, _, closed) = oauth_call(&st, "/token?access_token=good-token&expires_in=31536000").await;
    assert_eq!((s, closed), (200, true));
    assert_eq!(call(&st, "GET", "/auth/status", None).await, (200, json!({"connected": true, "username": "Alice"})));

    let auth = st.db.get_auth().unwrap().unwrap();
    assert_eq!((auth.access_token.as_str(), auth.anilist_user_id), ("good-token", 42));
    let expires = chrono::DateTime::parse_from_rfc3339(auth.token_expires_at.as_deref().unwrap()).unwrap();
    assert!((expires.timestamp() - chrono::Utc::now().timestamp() - 31_536_000).abs() < 60);
    // FR-25: the account's current format is stored as-is, never changed here.
    assert_eq!(auth.score_format.as_deref(), Some("POINT_100"));
    assert!(fake.calls_containing("UpdateUser").is_empty());
}

#[tokio::test]
async fn a_token_anilist_rejects_saves_nothing() {
    let fake = fake_anilist(|_, _| (400, json!({"data": null, "errors": [{"message": "Invalid token", "status": 400}]}), None)).await;
    let st = state_with(&fake);
    let (s, body, closed) = oauth_call(&st, "/token?access_token=bad-token&expires_in=31536000").await;
    assert_ne!(s, 200);
    assert!(body.contains("Invalid token"), "{body}");
    assert!(closed);
    assert!(st.db.get_auth().unwrap().is_none());
    assert_eq!(call(&st, "GET", "/auth/status", None).await, (200, json!({"connected": false, "username": null})));

    let (s, _, _) = oauth_call(&st, "/token").await;
    assert_eq!(s, 400);
    assert!(st.db.get_auth().unwrap().is_none());
}

#[tokio::test]
async fn token_errors_come_back_as_plain_text_never_html() {
    // The callback page shows this reply with textContent; served as
    // text/plain, error text from AniList can't be rendered as markup either.
    let fake = fake_anilist(|_, _| (400, json!({"data": null, "errors": [{"message": "<img src=x onerror=alert(1)>", "status": 400}]}), None)).await;
    let st = state_with(&fake);
    let (done, _closed) = tokio::sync::oneshot::channel();
    let req = Request::builder().uri("/token?access_token=bad-token").body(Body::empty()).unwrap();
    let resp = oauth::router(st, done).oneshot(req).await.unwrap();
    assert_eq!(resp.headers()["content-type"], "text/plain; charset=utf-8");
}

#[tokio::test]
async fn callback_page_never_reflects_query_text_as_html() {
    let st = offline_state().await;
    let (s, page, closed) = oauth_call(&st, "/callback").await;
    assert_eq!((s, closed), (200, false)); // still waiting for the page to send the token
    assert!(page.contains("location.hash") && page.contains("textContent") && !page.contains("innerHTML"));

    let injected = "/callback?error=%3Cscript%3Ealert(1)%3C%2Fscript%3E&error_description=%3Cimg%20src%3Dx%20onerror%3Dalert(2)%3E";
    let (s, error_page, closed) = oauth_call(&st, injected).await;
    assert_eq!((s, closed), (400, true)); // AniList refused: nothing left to wait for
    assert_eq!(error_page, page); // static: nothing from the request is written into it
    assert!(!error_page.contains("alert("));
}

#[test]
fn client_id_comes_from_the_env_var_then_local_config_toml_then_the_builtin() {
    let dir = tempfile::tempdir().unwrap();
    let toml = dir.path().join("local_config.toml");
    // Nothing set (no config folder, or no file in it): the built-in ID.
    assert_eq!(Config::from_sources(None, None).client_id, config::BUILTIN_CLIENT_ID);
    assert_eq!(Config::from_sources(None, Some(&toml)).client_id, config::BUILTIN_CLIENT_ID);
    assert_eq!(Config::from_sources(Some(String::new()), Some(&toml)).client_id, config::BUILTIN_CLIENT_ID);
    assert_eq!(
        anilist::authorize_url(config::BUILTIN_CLIENT_ID),
        format!("https://anilist.co/api/v2/oauth/authorize?client_id={}&response_type=token", config::BUILTIN_CLIENT_ID)
    );

    // Overrides: local_config.toml, then the environment variable over it.
    std::fs::write(&toml, "ANILIST_CLIENT_ID = \"777\"\n").unwrap();
    assert_eq!(Config::from_sources(None, Some(&toml)).client_id, "777");
    assert_eq!(Config::from_sources(Some("888".into()), Some(&toml)).client_id, "888");

    // A local_config.py in the config folder is never read.
    std::fs::remove_file(&toml).unwrap();
    std::fs::write(dir.path().join("local_config.py"), "ANILIST_CLIENT_ID = \"999\"\n").unwrap();
    assert_eq!(Config::from_sources(None, Some(&toml)).client_id, config::BUILTIN_CLIENT_ID);
}
