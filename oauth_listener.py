"""FR-22: a small, dedicated local HTTP listener for the OAuth callback only.

Runs on a fixed port -- separate from main.py's own port -- because the
redirect URI registered with AniList must be exact and unchanging. Spun up
on demand when login starts (see main.py's /auth/anilist/login), handles
exactly one /callback request, exchanges the code, then shuts itself down.
It is intentionally NOT a long-running part of the app.
"""
import asyncio
import logging
import threading
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

import anilist
import database
import local_config

OAUTH_PORT = 8931
OAUTH_REDIRECT_URI = f"http://localhost:{OAUTH_PORT}/callback"

log = logging.getLogger(__name__)

_SUCCESS_HTML = b"<html><body><h3>Connected to AniList. You can close this tab.</h3></body></html>"


def _finish_login(code: str) -> dict:
    token_data = asyncio.run(
        anilist.exchange_code(
            local_config.ANILIST_CLIENT_ID,
            local_config.ANILIST_CLIENT_SECRET,
            code,
            OAUTH_REDIRECT_URI,
        )
    )
    access_token = token_data["access_token"]
    expires_in = token_data.get("expires_in")
    expires_at = (
        (datetime.now(timezone.utc) + timedelta(seconds=expires_in)).isoformat()
        if expires_in
        else None
    )

    viewer = asyncio.run(anilist.get_viewer(access_token))
    # First login stores whatever the account's CURRENT scoreFormat is -- it is
    # NOT force-set to POINT_10_DECIMAL here. FR-25's consent step is a
    # separate, explicit action the frontend triggers after showing the user
    # a consent prompt (POST /auth/score-format/confirm) -- this callback must
    # not silently mutate the user's real AniList account settings.
    database.save_auth(
        access_token=access_token,
        token_expires_at=expires_at,
        anilist_user_id=viewer["id"],
        anilist_username=viewer["name"],
        score_format=viewer["score_format"],
    )
    return {"ok": True, "username": viewer["name"]}


class _CallbackHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass  # keep stdlib server quiet; errors are logged via `log` below

    def _respond(self, status: int, body: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(body.encode())

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path != "/callback":
            self.send_response(404)
            self.end_headers()
            return

        qs = parse_qs(parsed.query)
        code = (qs.get("code") or [None])[0]
        error = (qs.get("error") or [None])[0]

        if error or not code:
            msg = error or "no code returned"
            self._respond(400, f"<h3>AniList login failed: {msg}</h3>")
        else:
            try:
                _finish_login(code)
                self.send_response(200)
                self.send_header("Content-Type", "text/html")
                self.end_headers()
                self.wfile.write(_SUCCESS_HTML)
            except Exception as e:
                log.exception("OAuth callback exchange failed")
                self._respond(500, f"<h3>AniList login failed: {e}</h3>")

        # FR-22: the listener closes immediately after that one exchange --
        # shutdown() must run on a separate thread since it blocks until
        # serve_forever() (running on the thread that called us) returns.
        threading.Thread(target=self.server.shutdown, daemon=True).start()


class _CallbackHTTPServer(HTTPServer):
    # Lets a fresh bind succeed against a socket only lingering in TIME_WAIT
    # from a previous listener that didn't shut down cleanly (e.g. a crashed
    # request) -- does NOT let us steal a port from something genuinely still
    # listening right now.
    allow_reuse_address = True


_lock = threading.Lock()
_active_server: HTTPServer | None = None


def start_listener() -> HTTPServer | None:
    """Idempotent: /auth/anilist/login can fire more than once in quick
    succession (browser prefetch, a duplicate request, etc.) -- reuses an
    already-active listener instead of attempting a second bind, and never
    lets a bind failure escape as an unhandled 500. Worst case (some other
    process is genuinely holding the port), logs a warning and returns None
    so the caller can still proceed with the redirect on the assumption a
    listener -- ours or a stale one -- is already there to catch the callback.
    """
    global _active_server
    with _lock:
        if _active_server is not None:
            log.info("OAuth listener already active on port %s, reusing it", OAUTH_PORT)
            return _active_server

        try:
            server = _CallbackHTTPServer(("localhost", OAUTH_PORT), _CallbackHandler)
        except OSError:
            log.warning(
                "Could not bind OAuth listener on port %s (already in use) -- "
                "assuming an existing listener will handle the callback",
                OAUTH_PORT,
            )
            return None

        _active_server = server

        def _serve():
            global _active_server
            try:
                server.serve_forever()
            finally:
                with _lock:
                    if _active_server is server:
                        _active_server = None

        threading.Thread(target=_serve, daemon=True).start()
        log.info("OAuth callback listener started on port %s", OAUTH_PORT)
        return server
