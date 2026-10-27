//! Backend tests, ported from the Python suite (tests/*.py). Every test gets
//! its own in-memory database, and AniList is always a local fake server:
//! nothing here touches library.db or the network.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use myanimerank_lib::anilist::{self, AlError, AniList};
use myanimerank_lib::config::{self, Config};
use myanimerank_lib::db::{self, Db, MediaFields, Scores, SyncOutcome};
use myanimerank_lib::{api, oauth, sync, AppState, Shared};
use serde_json::{json, Value};
use tower::ServiceExt;

// ---------------------------------------------------------------- fake AniList ----

type Reply = (u16, Value, Option<&'static str>);
type Handler = Arc<dyn Fn(&str, &Value) -> Reply + Send + Sync>;

struct Fake {
    url: String,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Fake {
    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
    fn calls_containing(&self, needle: &str) -> Vec<Value> {
        self.calls().into_iter().filter(|(q, _)| q.contains(needle)).map(|(_, v)| v).collect()
    }
}

async fn fake_anilist(handler: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static) -> Fake {
    let handler: Handler = Arc::new(handler);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls2 = calls.clone();
    let app = axum::Router::new().fallback(move |body: String| {
        let handler = handler.clone();
        let calls = calls2.clone();
        async move {
            let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let query = body["query"].as_str().unwrap_or_default().to_string();
            let vars = body["variables"].clone();
            calls.lock().unwrap().push((query.clone(), vars.clone()));
            let (status, json, retry_after) = handler(&query, &vars);
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
    Fake { url, calls }
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
}

#[test]
fn manga_rating_forces_stored_audio_to_zero() {
    let db = Db::open_in_memory().unwrap();
    let id = db.insert_or_update_media(&fields(1, "manga"), None).unwrap();
    db.upsert_rating(id, &scores(8.0)).unwrap();
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
    let target = db.insert_or_update_media(&fields(1, "anime"), None).unwrap();
    let same = db.insert_or_update_media(&fields(2, "anime"), None).unwrap();
    let manga = db.insert_or_update_media(&fields(3, "manga"), None).unwrap();
    let mut other_genre = fields(4, "anime");
    other_genre.genres = vec!["Romance".into()];
    let other = db.insert_or_update_media(&other_genre, None).unwrap();
    for id in [same, manga, other] {
        db.upsert_rating(id, &scores(7.0)).unwrap();
    }
    let ids = |scope| -> Vec<i64> {
        db.get_anchors(target, "story", scope).unwrap().unwrap().iter().map(|a| a["id"].as_i64().unwrap()).collect()
    };
    assert_eq!(ids("genre"), vec![same]);
    let mut all = ids("all");
    all.sort();
    assert_eq!(all, vec![same, other]);
    assert!(matches!(db.get_anchors(target, "overall; DROP TABLE media", "all"), Err(db::AnchorError::InvalidCriterion(_))));
}

// ----------------------------------------------------------------- progress ----

#[test]
fn reaching_total_auto_completes_and_counts_a_rewatch_once() {
    let db = Db::open_in_memory().unwrap();
    let id = db.insert_or_update_media(&fields(1, "anime"), Some("watching")).unwrap();
    assert_eq!(db.update_progress(id, 99).unwrap(), Some(12)); // clamped to total
    assert_eq!(db.get_media_by_id(id).unwrap().unwrap().status, "completed");

    let id = db.insert_or_update_media(&fields(2, "manga"), Some("rereading")).unwrap();
    db.update_progress(id, 100).unwrap();
    db.update_progress(id, 100).unwrap(); // already at total: no double count
    let m = db.get_media_by_id(id).unwrap().unwrap();
    assert_eq!((m.status.as_str(), m.repeat_count), ("completed", 1));
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
        (Some(t(5)), Some(t(5)), None, O::Conflict), // never synced: always ask
        (Some(t(5)), Some(t(1)), Some(t(2)), O::Local),
        (Some(t(1)), Some(t(5)), Some(t(2)), O::Remote),
        (Some(t(5)), Some(t(5)), Some(t(2)), O::Conflict),
        (Some(t(1)), Some(t(1)), Some(t(2)), O::None),
    ];
    for (l, r, s, expected) in cases {
        assert_eq!(db::detect_conflict(l.as_deref(), r.as_deref(), s.as_deref()), expected);
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
    let anime = db.insert_or_update_media(&fields(1, "anime"), None).unwrap();
    let manga = db.insert_or_update_media(&fields(2, "manga"), None).unwrap();
    db.apply_remote_entry(anime, "watching", 3, 0, Some(7.3), "2026-01-01T00:00:00+00:00").unwrap();
    db.apply_remote_entry(manga, "reading", 3, 0, Some(7.25), "2026-01-01T00:00:00+00:00").unwrap();
    let a = db.get_media_by_id(anime).unwrap().unwrap().rating.unwrap();
    assert_eq!((a.story, a.audio, a.overall), (7.5, 7.5, 7.3));
    let m = db.get_media_by_id(manga).unwrap().unwrap().rating.unwrap();
    assert_eq!((m.story, m.audio, m.overall), (7.5, 0.0, 7.25));

    // AniList's 0 means "no score": no rating is invented.
    let none = db.insert_or_update_media(&fields(3, "anime"), None).unwrap();
    db.apply_remote_entry(none, "planning", 0, 0, Some(0.0), "2026-01-01T00:00:00+00:00").unwrap();
    assert!(db.get_media_by_id(none).unwrap().unwrap().rating.is_none());
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

// --------------------------------------------------------------------- routes ----

const PROTECTED: &[(&str, &str)] = &[
    ("POST", "/auth/score-format/confirm"), ("GET", "/search?q=x&type=anime"), ("POST", "/media"),
    ("GET", "/media/1"), ("DELETE", "/media/1"), ("GET", "/media/by-anilist/1"), ("PUT", "/media/1/rating"),
    ("GET", "/media/1/anchors?criterion=story"), ("PUT", "/media/1/status"), ("PUT", "/media/1/progress"),
    ("PUT", "/media/1/repeat"), ("POST", "/sync/pull"), ("GET", "/sync/pending-conflicts"),
    ("POST", "/sync/resolve/1"), ("POST", "/sync/push/1"), ("GET", "/discover?section=trending&media_type=anime"),
    ("GET", "/feed?type=global"), ("GET", "/characters/search?q=x"), ("GET", "/staff/search?q=x"),
    ("GET", "/characters/1"), ("GET", "/staff/1"), ("GET", "/studios/search?q=x"), ("GET", "/studios/1"),
    ("GET", "/users/search?q=x"), ("GET", "/users/1"), ("POST", "/favorites/toggle"), ("GET", "/favorites"),
    ("GET", "/media/1/community"), ("GET", "/media/by-anilist/1/community"), ("GET", "/library"),
    ("GET", "/insights"), ("GET", "/rankings"), ("GET", "/profile/stats"),
];

#[tokio::test]
async fn every_protected_route_is_401_without_login() {
    let st = offline_state().await;
    assert_eq!(PROTECTED.len(), 33);
    for (method, path) in PROTECTED {
        let (status, body) = call(&st, method, path, Some(json!({}))).await;
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
}

#[tokio::test]
async fn rating_route_validates_audio_per_media_type_and_computes_overall() {
    let st = offline_state().await;
    login(&st);
    let manga = st.db.insert_or_update_media(&fields(1, "manga"), None).unwrap();
    let anime = st.db.insert_or_update_media(&fields(2, "anime"), None).unwrap();
    let body = |audio: f64| json!({"story": 8.0, "characters": 8.0, "animation": 8.0, "audio": audio, "enjoyment": 9.0, "overall": 1.0});

    let (s, b) = call(&st, "PUT", &format!("/media/{manga}/rating"), Some(body(0.0))).await;
    assert_eq!((s, b), (200, json!({"overall": 8.2, "media_type": "manga"}))); // 8.25, half-even like Python; client overall ignored
    assert_eq!(call(&st, "PUT", &format!("/media/{manga}/rating"), Some(body(5.0))).await.0, 422);
    assert_eq!(call(&st, "PUT", &format!("/media/{anime}/rating"), Some(body(0.0))).await.0, 422);
    assert_eq!(call(&st, "PUT", &format!("/media/{anime}/rating"), Some(body(7.5))).await.0, 200);
    let mut off_step = body(8.0);
    off_step["story"] = json!(7.3);
    assert_eq!(call(&st, "PUT", &format!("/media/{anime}/rating"), Some(off_step)).await.0, 422);
    assert_eq!(call(&st, "PUT", "/media/999/rating", Some(body(8.0))).await.0, 404);
    assert_eq!(call(&st, "PUT", "/media/abc/rating", Some(body(8.0))).await.0, 422);
}

#[tokio::test]
async fn status_must_be_valid_for_the_titles_media_type() {
    let st = offline_state().await;
    login(&st);
    let anime = st.db.insert_or_update_media(&fields(1, "anime"), None).unwrap();
    let (s, b) = call(&st, "PUT", &format!("/media/{anime}/status"), Some(json!({"status": "reading"}))).await;
    assert_eq!(s, 422);
    assert!(b["detail"].as_str().unwrap().contains("'watching'"));
    assert_eq!(st.db.get_media_by_id(anime).unwrap().unwrap().status, "planning");
    assert_eq!(call(&st, "PUT", &format!("/media/{anime}/status"), Some(json!({"status": "watching"}))).await, (200, json!({"status": "watching"})));
    assert_eq!(anilist::local_statuses("manga"), vec!["completed", "dropped", "paused", "planning", "reading", "rereading"]);
}

#[tokio::test]
async fn import_takes_media_type_from_anilist_not_the_client() {
    let fake = fake_anilist(|q, v| {
        if q.contains("Media(id") && v["id"] == 7 {
            (200, json!({"data": {"Media": media_node(7, "MANGA")}}), None)
        } else {
            (404, json!({"data": {"Media": null}, "errors": [{"message": "Not Found.", "status": 404}]}), None)
        }
    })
    .await;
    let st = state_with(&fake);
    login(&st);
    let (s, b) = call(&st, "POST", "/media", Some(json!({"anilist_id": 7, "media_type": "anime", "title": "x", "status": "reading"}))).await;
    assert_eq!(s, 200);
    let m = st.db.get_media_by_id(b["id"].as_i64().unwrap()).unwrap().unwrap();
    assert_eq!((m.fields.media_type.as_str(), m.status.as_str()), ("manga", "reading"));

    let (s, b) = call(&st, "POST", "/media", Some(json!({"anilist_id": 8, "media_type": "anime", "title": "x"}))).await;
    assert_eq!((s, b["detail"].as_str()), (404, Some("anilist_id not found")));
    let (s, _) = call(&st, "GET", "/media/by-anilist/8", None).await;
    assert_eq!(s, 404);
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
async fn import_502s_when_anilist_is_unreachable() {
    let st = offline_state().await;
    login(&st);
    let (s, b) = call(&st, "POST", "/media", Some(json!({"anilist_id": 7, "media_type": "anime", "title": "x"}))).await;
    assert_eq!((s, b["detail"].as_str()), (502, Some("could not reach AniList to verify this title")));
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

#[test]
fn unknown_media_types_are_errors_never_defaults() {
    assert_eq!(anilist::to_media_type(&json!("ANIME")).unwrap(), "anime");
    assert_eq!(anilist::to_media_type(&json!("MANGA")).unwrap(), "manga");
    assert!(anilist::to_media_type(&json!("NOVEL")).is_err());
    assert!(anilist::to_media_type(&Value::Null).is_err());
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
async fn pull_imports_then_applies_a_later_remote_change_without_a_false_conflict() {
    let remote_ts = Arc::new(Mutex::new(1_700_000_000_i64));
    let ts = remote_ts.clone();
    let fake = fake_anilist(move |q, v| {
        if q.contains("MediaListCollection") && v["type"] == "MANGA" {
            (200, collection(vec![list_node(42, "MANGA", *ts.lock().unwrap(), 8.0)]), None)
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

    // AniList changes later: one-sided remote change, applied, not a conflict.
    *remote_ts.lock().unwrap() = chrono::Utc::now().timestamp() + 3600;
    let second = sync::pull(&st).await.unwrap();
    assert_eq!((second["updated"].as_i64(), second["conflicts"].as_array().unwrap().len()), (Some(1), 0));
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
    assert_eq!(st.db.get_media_by_id(id).unwrap().unwrap().anilist_list_entry_id, Some(555));
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

    let out = sync::push_now_guarded(&st, id).await.unwrap();
    assert_eq!(out["conflict"], true);
    assert!(fake.calls_containing("SaveMediaListEntry").is_empty());
    assert!(st.db.get_media_by_id(id).unwrap().unwrap().pending_conflict);
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
fn config_falls_back_to_the_builtin_client_id() {
    let dir = tempfile::tempdir().unwrap();
    let files = [dir.path().join("local_config.toml"), dir.path().join("local_config.py")];
    assert_eq!(Config::from_sources(None, &files).client_id, config::BUILTIN_CLIENT_ID);
    assert_eq!(Config::from_sources(Some(String::new()), &files).client_id, config::BUILTIN_CLIENT_ID);
    assert_eq!(
        anilist::authorize_url(config::BUILTIN_CLIENT_ID),
        format!("https://anilist.co/api/v2/oauth/authorize?client_id={}&response_type=token", config::BUILTIN_CLIENT_ID)
    );

    // Overrides: a file, then the environment variable over it.
    std::fs::write(&files[1], "ANILIST_CLIENT_ID = \"777\"\n").unwrap();
    assert_eq!(Config::from_sources(None, &files).client_id, "777");
    assert_eq!(Config::from_sources(Some("888".into()), &files).client_id, "888");
}
