import os
import socket
import threading
import time

import uvicorn
import webview

from main import app as fastapi_app

_HOST = "127.0.0.1"
_PORT = int(os.environ.get("MAR_PORT", "6969"))


def _port_available(port: int) -> bool:
    with socket.socket() as s:
        try:
            s.bind((_HOST, port))
            return True
        except OSError:
            return False


def _wait_for_server(port: int, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((_HOST, port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"Server did not start on port {port} within {timeout}s")


def _run_server(port: int) -> None:
    uvicorn.run(fastapi_app, host=_HOST, port=port, log_level="warning")


class WindowApi:
    def __init__(self):
        self._win = None

    def close(self):
        if self._win:
            self._win.destroy()

    def minimize(self):
        if self._win:
            self._win.minimize()

    def maximize_window(self):
        if self._win:
            try:
                self._win.maximize()
            except AttributeError:
                self._win.toggle_fullscreen()

    def restore_window(self):
        if self._win:
            try:
                self._win.restore()
            except AttributeError:
                self._win.toggle_fullscreen()

    def get_position(self):
        if self._win:
            try:
                return {'x': self._win.x, 'y': self._win.y}
            except AttributeError:
                return {'x': 0, 'y': 0}
        return {'x': 0, 'y': 0}

    def move_to(self, x: int, y: int):
        if self._win:
            self._win.move(x, y)


if __name__ == "__main__":
    import multiprocessing
    import traceback
    multiprocessing.freeze_support()

    try:
        port = _PORT
        if not _port_available(port):
            raise SystemExit(
                f"Port {port} is already in use. "
                f"Set MAR_PORT=<number> to use a different port, "
                f"then update your AniList app's redirect URI to http://localhost:<number>/anilist/callback"
            )
        server_thread = threading.Thread(target=_run_server, args=(port,), daemon=True)
        server_thread.start()
        _wait_for_server(port)
        api = WindowApi()
        win = webview.create_window(
            "MyAnimeRank",
            f"http://{_HOST}:{port}",
            width=1400,
            height=900,
            min_size=(900, 600),
            frameless=True,
            easy_drag=False,
            js_api=api,
        )
        api._win = win
        webview.start(gui='edgechromium')
    except SystemExit:
        raise
    except Exception:
        import ctypes
        import pathlib
        tb = traceback.format_exc()
        log_dir = pathlib.Path(os.environ.get("LOCALAPPDATA", pathlib.Path.home())) / "MyAnimeRank"
        log_dir.mkdir(parents=True, exist_ok=True)
        log_path = log_dir / "crash.log"
        log_path.write_text(tb, encoding="utf-8")
        ctypes.windll.user32.MessageBoxW(
            0,
            f"MyAnimeRank failed to start.\n\n{tb[:800]}\n\nFull log saved to:\n{log_path}",
            "MyAnimeRank — Startup Error",
            0x10,  # MB_ICONERROR
        )
        raise
