//! FR-22: a small, dedicated local listener for the OAuth callback only.
//! Port of oauth_listener.py.
//!
//! Runs on a fixed port, separate from everything else, because the redirect
//! URI registered with AniList must be exact and unchanging
//! (`http://localhost:8931/callback`). Started on demand when login begins,
//! handles exactly one /callback request, exchanges the code, then shuts down.

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

pub fn redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/callback")
}

const SUCCESS_HTML: &str = "<html><body><h3>Connected to AniList. You can close this tab.</h3></body></html>";

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Exchanges the code, then stores the token. The account's CURRENT
/// scoreFormat is stored as-is: FR-25's consent step is a separate, explicit
/// action, so this never silently mutates the user's AniList settings.
async fn finish_login(st: &Shared, code: &str) -> Result<String, String> {
    let (Some(id), Some(secret)) = (&st.config.client_id, &st.config.client_secret) else {
        return Err(st.config.missing_message());
    };
    let token_data = st
        .anilist
        .exchange_code(id, secret, code, &redirect_uri(st.oauth_port))
        .await
        .map_err(|e| e.to_string())?;
    let access_token = token_data["access_token"].as_str().ok_or("no access_token in AniList's response")?;
    let expires_at = token_data["expires_in"]
        .as_i64()
        .filter(|s| *s != 0)
        .map(|s| (Utc::now() + Duration::seconds(s)).to_rfc3339_opts(SecondsFormat::Micros, false));
    let viewer = st.anilist.get_viewer(access_token).await.map_err(|e| e.to_string())?;
    st.db
        .save_auth(access_token, expires_at.as_deref(), viewer.id, &viewer.name, viewer.score_format.as_deref())
        .map_err(|e| e.to_string())?;
    Ok(viewer.name)
}

type Done = Arc<Mutex<Option<oneshot::Sender<()>>>>;

#[derive(serde::Deserialize)]
struct CallbackParams {
    code: Option<String>,
    error: Option<String>,
}

async fn callback(
    State((st, done)): State<(Shared, Done)>,
    Query(params): Query<CallbackParams>,
) -> (StatusCode, Html<String>) {
    let response = match (params.error, params.code.filter(|c| !c.is_empty())) {
        (Some(err), _) => (StatusCode::BAD_REQUEST, format!("<h3>AniList login failed: {}</h3>", html_escape(&err))),
        (None, None) => (StatusCode::BAD_REQUEST, "<h3>AniList login failed: no code returned</h3>".into()),
        (None, Some(code)) => match finish_login(&st, &code).await {
            Ok(_) => (StatusCode::OK, SUCCESS_HTML.into()),
            Err(e) => {
                log::error!("OAuth callback exchange failed: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, format!("<h3>AniList login failed: {}</h3>", html_escape(&e)))
            }
        },
    };
    // The listener closes right after this one exchange.
    if let Some(tx) = done.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = tx.send(());
    }
    (response.0, Html(response.1))
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
        let done: Done = Arc::new(Mutex::new(Some(tx)));
        let app = Router::new().route("/callback", get(callback)).with_state((st.clone(), done));
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
