//! MyAnimeRank desktop app (Tauri 2).
//!
//! Replaces the Python stack (FastAPI + uvicorn subprocess + pywebview) with
//! one native process: the API router runs in-process and the window's page
//! reaches it through a single Tauri command, so there is no local web server
//! and no open port (apart from the one-shot OAuth callback listener).

pub mod anilist;
pub mod api;
pub mod cache;
pub mod config;
pub mod db;
pub mod oauth;
pub mod sync;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Method, Request};
use http_body_util::BodyExt;
use tauri::Manager;
use tower::ServiceExt;

pub struct AppState {
    pub db: db::Db,
    pub anilist: anilist::AniList,
    pub config: config::Config,
    /// FR-26 retry queue. In memory, as before (a known open item).
    pub push_queue: Mutex<BTreeSet<i64>>,
    pub cache: cache::TtlCache,
    /// FR-70's genre/tag lists. Its own instance: a TtlCache evicts by the
    /// caller's TTL, so sharing Discover's 60 s would drop a 24 h entry.
    pub pick_cache: cache::TtlCache,
    pub oauth_port: u16,
    pub oauth_active: Mutex<bool>,
    /// FR-70: friends whose lists are being fetched right now ("fetching"),
    /// each with the channel that fetch answers on, so a second caller waits
    /// for it instead of fetching again (FR-80/FR-81).
    pub friend_fetching: Mutex<HashMap<i64, sync::FetchAnswer>>,
}

pub type Shared = Arc<AppState>;

impl AppState {
    pub fn new(db: db::Db, anilist: anilist::AniList, config: config::Config) -> Shared {
        Arc::new(AppState {
            db,
            anilist,
            config,
            push_queue: Mutex::new(BTreeSet::new()),
            cache: cache::TtlCache::default(),
            pick_cache: cache::TtlCache::default(),
            oauth_port: oauth::OAUTH_PORT,
            oauth_active: Mutex::new(false),
            friend_fetching: Mutex::new(HashMap::new()),
        })
    }
}

fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
}

// ------------------------------------------------------------- Tauri bridge ----

#[derive(serde::Serialize)]
struct ApiReply {
    status: u16,
    body: String,
}

/// The page's only way into the API: one in-process request per call, with
/// the same method/path/JSON the old HTTP API took.
#[tauri::command]
async fn api(
    router: tauri::State<'_, axum::Router>,
    method: String,
    path: String,
    body: Option<String>,
) -> Result<ApiReply, String> {
    let method = Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.unwrap_or_default()))
        .map_err(|e| e.to_string())?;
    let response = router.inner().clone().oneshot(request).await.map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
    Ok(ApiReply { status, body: String::from_utf8_lossy(&bytes).into_owned() })
}

/// Login opens AniList in the user's real browser (where they're likely
/// already signed in); the one-shot listener catches the callback.
#[tauri::command]
async fn start_login(app: tauri::AppHandle, state: tauri::State<'_, Shared>) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = api::begin_login(state.inner());
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

fn database_path(app: &tauri::App) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(p) = std::env::var_os("MYANIMERANK_DB") {
        return Ok(PathBuf::from(p));
    }
    Ok(app.path().app_data_dir()?.join("library.db"))
}

/// A release build has no console, so a startup failure is shown in a native
/// dialog, with the (empty) window hidden behind it, and the app exits once
/// it's dismissed. Non-blocking: on macOS the dialog needs the main thread,
/// which the setup hook is still holding.
fn exit_with_error(app: &tauri::App, message: String) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
    log::error!("{message}");
    for window in app.webview_windows().values() {
        let _ = window.hide();
    }
    let handle = app.handle().clone();
    app.dialog()
        .message(message)
        .title("MyAnimeRank can't start")
        .kind(MessageDialogKind::Error)
        .show(move |_| handle.exit(1));
}

pub fn run() {
    init_logging();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let db_path = database_path(app)?;
            log::info!("library database: {}", db_path.display());
            let db = match db::Db::open(&db_path) {
                Ok(db) => db,
                Err(e) => {
                    // Returning Err here would make Tauri panic instead.
                    exit_with_error(app, format!("Could not open the library database at\n{}\n\n{e}", db_path.display()));
                    return Ok(());
                }
            };
            let config_dir = app.path().app_config_dir().ok();
            let config = config::Config::load(config_dir.as_deref());
            let state = AppState::new(db, anilist::AniList::default(), config);
            app.manage(api::router(state.clone()));
            app.manage(state.clone());
            tauri::async_runtime::spawn(sync::retry_loop(state.clone()));
            tauri::async_runtime::spawn(sync::friend_queue_loop(state, sync::FRIEND_QUEUE_SPACING));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![api, start_login])
        .run(tauri::generate_context!())
        .expect("error while running MyAnimeRank");
}

// ---------------------------------------------------------- browser dev mode ----

/// `myanimerank --serve [port]`: the same API plus the page on
/// http://127.0.0.1:<port>, for frontend work in a normal browser (what
/// `uvicorn main:app --port 8420` used to be). Uses `MYANIMERANK_DB` or
/// `./library.db`.
pub fn serve(port: u16) {
    init_logging();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async move {
        let db_path = std::env::var_os("MYANIMERANK_DB").map(PathBuf::from).unwrap_or_else(|| "library.db".into());
        log::info!("library database: {}", db_path.display());
        let db = match db::Db::open(&db_path) {
            Ok(db) => db,
            Err(e) => {
                log::error!("could not open the library database at {}: {e}", db_path.display());
                std::process::exit(1);
            }
        };
        let config = config::Config::load(None);
        let state = AppState::new(db, anilist::AniList::default(), config);
        tokio::spawn(sync::retry_loop(state.clone()));
        tokio::spawn(sync::friend_queue_loop(state.clone(), sync::FRIEND_QUEUE_SPACING));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind dev port");
        log::info!("serving on http://127.0.0.1:{port}");
        axum::serve(listener, api::router(state)).await.expect("dev server");
    });
}
