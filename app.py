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
    import logging
    import pathlib
    log_dir = pathlib.Path(os.environ.get("LOCALAPPDATA", pathlib.Path.home())) / "MyAnimeRank"
    log_dir.mkdir(parents=True, exist_ok=True)
    logging.basicConfig(
        filename=str(log_dir / "server.log"),
        level=logging.ERROR,
        format="%(asctime)s %(name)s %(levelname)s %(message)s",
    )
    uvicorn.run(fastapi_app, host=_HOST, port=port, log_level="error")


class WindowApi:
    def __init__(self):
        self._win = None

    def save_file(self, filename: str, content: str) -> dict:
        import pathlib, base64
        try:
            data = base64.b64decode(content)
        except Exception:
            data = content.encode("utf-8")
        if self._win:
            result = self._win.create_file_dialog(
                webview.SAVE_DIALOG,
                save_filename=filename,
            )
            if result:
                path = result[0] if isinstance(result, (list, tuple)) else result
                pathlib.Path(path).write_bytes(data)
                return {"path": str(path)}
            return {"path": None}
        # fallback when window ref unavailable
        downloads = pathlib.Path.home() / "Downloads"
        downloads.mkdir(parents=True, exist_ok=True)
        dest = downloads / filename
        dest.write_bytes(data)
        return {"path": str(dest)}

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
            import ctypes
            ctypes.windll.user32.MessageBoxW(
                0,
                f"MyAnimeRank is already running.\n\nIf it isn't, port {port} is in use by another program. "
                f"Close it and try again, or set MAR_PORT=<number> to use a different port.",
                "MyAnimeRank",
                0x30,  # MB_ICONWARNING
            )
            raise SystemExit(f"Port {port} already in use")
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
