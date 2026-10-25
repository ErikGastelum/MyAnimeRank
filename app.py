"""Frameless pywebview desktop shell for MyAnimeRank (Section 6).

Wraps main.py's FastAPI app in a real subprocess (not a thread) running on
a dynamically-found free port -- kept separate from oauth_listener.py's
fixed OAUTH_PORT (8931), which is reserved solely for the OAuth callback
(FR-22). Using a subprocess (rather than a thread) makes shutdown a real,
verifiable process termination instead of relying on daemon-thread-dies-
with-parent behavior. main.py itself is unaffected: `uvicorn main:app
--reload` still works standalone for browser-based dev.
"""
import socket
import subprocess
import sys
import time

import httpx
import webview


def _find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_for_server(port: int, timeout: float = 10.0) -> None:
    """GET /auth/status needs no auth (same route the frontend's own boot
    sequence pings), so it's a safe readiness check before pointing the
    window at the port."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            httpx.get(f"http://127.0.0.1:{port}/auth/status", timeout=1)
            return
        except httpx.HTTPError:
            time.sleep(0.2)
    raise RuntimeError(f"uvicorn did not become ready on port {port} in time")


def _terminate(process: subprocess.Popen) -> None:
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()


class WindowApi:
    """Exposed to the frontend as window.pywebview.api -- backs the
    titlebar's minimize/maximize/close buttons (frontend/index.html)."""

    def __init__(self) -> None:
        self._window: webview.Window | None = None
        self._maximized = False

    def bind(self, window: webview.Window) -> None:
        self._window = window

    def minimize(self) -> None:
        self._window.minimize()

    def toggle_maximize(self) -> None:
        if self._maximized:
            self._window.restore()
        else:
            self._window.maximize()
        self._maximized = not self._maximized

    def close(self) -> None:
        self._window.destroy()


def main() -> None:
    port = _find_free_port()
    process = subprocess.Popen(
        [sys.executable, "-m", "uvicorn", "main:app", "--host", "127.0.0.1", "--port", str(port)]
    )
    try:
        _wait_for_server(port)

        api = WindowApi()
        window = webview.create_window(
            "MyAnimeRank",
            f"http://127.0.0.1:{port}",
            js_api=api,
            frameless=True,
            # Ground-truthed against the installed pywebview (6.2.1): its
            # default easy_drag=True registers a global JS mousedown listener
            # (webview/js/customize.js) that starts window-drag tracking on
            # ANY click anywhere in the page -- it never consults
            # -webkit-app-region CSS at all, which is a different mechanism
            # entirely. With easy_drag disabled here, only the always-active,
            # properly-scoped drag-region-class mechanism remains (frontend
            # marks just the titlebar's logo/title block with
            # class="pywebview-drag-region"), so clicking/dragging anywhere
            # else in the app -- the Story slider included -- behaves
            # normally instead of moving the window.
            easy_drag=False,
            width=1180,
            height=760,
            min_size=(900, 600),
        )
        api.bind(window)
        window.events.closed += lambda: _terminate(process)

        webview.start()
    finally:
        # webview.start() blocks until the window closes, and the closed
        # event above already terminates the process by then -- this only
        # fires if something raised before the window ever opened.
        if process.poll() is None:
            _terminate(process)


if __name__ == "__main__":
    main()
