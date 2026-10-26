import threading

import httpx
import pytest
from fastapi.testclient import TestClient

import main
import oauth_listener


def test_app_serves_frontend_and_auth_status() -> None:
    with TestClient(main.app) as client:
        root = client.get("/")
        status = client.get("/auth/status")

    assert root.status_code == 200
    assert "<html" in root.text.lower()
    assert status.json() == {"connected": False, "username": None}


def test_api_sends_no_cors_headers_to_other_origins() -> None:
    # The frontend is served same-origin from GET /, so no other site should
    # ever be cleared to call (and read) this API from a browser.
    with TestClient(main.app) as client:
        simple = client.get("/auth/status", headers={"Origin": "https://evil.example"})
        preflight = client.options(
            "/media/1",
            headers={
                "Origin": "https://evil.example",
                "Access-Control-Request-Method": "DELETE",
            },
        )

    assert "access-control-allow-origin" not in simple.headers
    assert "access-control-allow-origin" not in preflight.headers


def _callback(query: str) -> httpx.Response:
    server = oauth_listener._CallbackHTTPServer(("127.0.0.1", 0), oauth_listener._CallbackHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = server.server_address[1]
        return httpx.get(f"http://127.0.0.1:{port}/callback?{query}", timeout=5)
    finally:
        thread.join(timeout=5)  # handler shuts the server down after one request
        server.server_close()


def test_oauth_callback_escapes_reflected_error_param() -> None:
    response = _callback("error=%3Cscript%3Ealert(1)%3C/script%3E")

    assert response.status_code == 400
    assert "<script>" not in response.text
    assert "&lt;script&gt;" in response.text


def test_oauth_callback_escapes_exception_text(monkeypatch: pytest.MonkeyPatch) -> None:
    def fail(_code: str) -> dict:
        raise RuntimeError("<img src=x onerror=alert(1)>")

    monkeypatch.setattr(oauth_listener, "_finish_login", fail)

    response = _callback("code=abc")

    assert response.status_code == 500
    assert "<img" not in response.text
    assert "&lt;img" in response.text
