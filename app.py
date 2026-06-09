import socket
import threading
import time

import uvicorn
import webview

from main import app as fastapi_app

_HOST = "127.0.0.1"


def _free_port() -> int:
    with socket.socket() as s:
        s.bind((_HOST, 0))
        return s.getsockname()[1]


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
    port = _free_port()
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
    webview.start()
