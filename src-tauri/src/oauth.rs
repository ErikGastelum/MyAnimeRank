//! FR-22: a small, dedicated local listener for the OAuth callback only.
//! Port of oauth_listener.py, using AniList's implicit grant.
//!
//! Runs on a fixed port, separate from everything else, because the redirect
//! URI registered with AniList must be exact and unchanging
//! (`http://localhost:8931/callback`). Started on demand when login begins.
//! AniList redirects to /callback with the token in the URL fragment, which
//! browsers never send to a server, so /callback serves a static page whose
//! script passes the fragment to /token. /token checks the token with AniList,
//! stores it, then the listener shuts down.

use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use chrono::{Duration, SecondsFormat, Utc};
use tokio::sync::oneshot;

use crate::Shared;

pub const OAUTH_PORT: u16 = 8931;

/// Static: nothing from the request is ever written into it, and its script
/// shows every message with textContent, so no query or fragment text is ever
/// parsed as HTML.
const CALLBACK_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>MyAnimeRank</title></head>
<body><h3 id="msg">Connecting to AniList...</h3>
<script>
const msg = document.getElementById('msg');
const query = new URLSearchParams(location.search);
const hash = new URLSearchParams(location.hash.slice(1));
history.replaceState(null, '', location.pathname); // keep the token out of browser history
const fail = text => { msg.textContent = 'AniList login failed: ' + text; };
const error = query.get('error_description') || query.get('error') || hash.get('error_description') || hash.get('error');
if (error) {
  fail(error);
} else if (!hash.get('access_token')) {
  fail('no access token returned');
} else {
  const params = new URLSearchParams({ access_token: hash.get('access_token') });
  if (hash.get('expires_in')) params.set('expires_in', hash.get('expires_in'));
  fetch('/token?' + params)
    .then(async r => { if (r.ok) msg.textContent = 'Connected to AniList. You can close this tab.'; else fail(await r.text()); })
    .catch(e => fail(String(e)));
}
</script></body></html>"#;

/// Checks the token with AniList, then stores it. The account's CURRENT
/// scoreFormat is stored as-is: FR-25's consent step is a separate, explicit
/// action, so this never silently mutates the user's AniList settings.
async fn finish_login(st: &Shared, access_token: &str, expires_in: Option<i64>) -> Result<String, String> {
    let viewer = st.anilist.get_viewer(access_token).await.map_err(|e| e.to_string())?;
    // Checked math: expires_in now comes from a URL, not AniList's JSON.
    let expires_at = expires_in
        .filter(|s| *s > 0)
        .and_then(Duration::try_seconds)
        .and_then(|d| Utc::now().checked_add_signed(d))
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Micros, false));
    st.db
        .save_auth(access_token, expires_at.as_deref(), viewer.id, &viewer.name, viewer.score_format.as_deref())
        .map_err(|e| e.to_string())?;
    // No friend sync here: the page Pulls right after login, and that Pull
    // syncs (FR-79), so a second one would only repeat its requests.
    Ok(viewer.name)
}

type Done = Arc<Mutex<Option<oneshot::Sender<()>>>>;

fn shut_down(done: &Done) {
    if let Some(tx) = done.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = tx.send(());
    }
}

#[derive(serde::Deserialize)]
struct CallbackParams {
    error: Option<String>,
}

async fn callback(
    State((_, done)): State<(Shared, Done)>,
    Query(params): Query<CallbackParams>,
) -> (StatusCode, Html<&'static str>) {
    if params.error.is_some() {
        // The user (or AniList) refused; the page shows why. Nothing to wait for.
        shut_down(&done);
        return (StatusCode::BAD_REQUEST, Html(CALLBACK_HTML));
    }
    (StatusCode::OK, Html(CALLBACK_HTML))
}

#[derive(serde::Deserialize)]
struct TokenParams {
    access_token: Option<String>,
    expires_in: Option<i64>,
}

/// Plain-text replies: the callback page shows them with textContent.
async fn token(State((st, done)): State<(Shared, Done)>, Query(params): Query<TokenParams>) -> (StatusCode, String) {
    let response = match params.access_token.filter(|t| !t.is_empty()) {
        None => (StatusCode::BAD_REQUEST, "no access token returned".into()),
        Some(t) => match finish_login(&st, &t, params.expires_in).await {
            Ok(name) => (StatusCode::OK, format!("connected as {name}")),
            Err(e) => {
                log::error!("OAuth token check failed: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, e)
            }
        },
    };
    // The listener closes right after this one attempt.
    shut_down(&done);
    response
}

/// The listener's routes. `done` fires once the login attempt is over, which
/// shuts the listener down.
pub fn router(st: Shared, done: oneshot::Sender<()>) -> Router {
    let done: Done = Arc::new(Mutex::new(Some(done)));
    Router::new()
        .route("/callback", get(callback))
        .route("/token", get(token))
        .with_state((st, done))
}

/// Idempotent: a second login click reuses an already-active listener. If
/// another process holds the port, logs a warning and carries on, assuming a
/// listener (ours or a stale one) is there to catch the callback.
pub fn start_listener(st: &Shared) {
    let mut active = st.oauth_active.lock().unwrap_or_else(|e| e.into_inner());
    if *active {
        log::info!("OAuth listener already active on port {}, reusing it", st.oauth_port);
        return;
    }
    let std_listener = match std::net::TcpListener::bind(("127.0.0.1", st.oauth_port)) {
        Ok(l) => l,
        Err(_) => {
            log::warn!(
                "could not bind OAuth listener on port {} (already in use); assuming an existing listener will handle the callback",
                st.oauth_port
            );
            return;
        }
    };
    if std_listener.set_nonblocking(true).is_err() {
        return;
    }
    *active = true;
    drop(active);

    let st = st.clone();
    tokio::spawn(async move {
        let (tx, rx) = oneshot::channel();
        let app = router(st.clone(), tx);
        match tokio::net::TcpListener::from_std(std_listener) {
            Ok(listener) => {
                log::info!("OAuth callback listener started on port {}", st.oauth_port);
                let shutdown = async {
                    let _ = rx.await;
                };
                if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(shutdown).await {
                    log::error!("OAuth listener stopped with an error: {e}");
                }
            }
            Err(e) => log::error!("OAuth listener could not start: {e}"),
        }
        *st.oauth_active.lock().unwrap_or_else(|e| e.into_inner()) = false;
    });
}
