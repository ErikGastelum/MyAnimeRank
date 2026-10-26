import re

import pytest
from fastapi.routing import APIRoute
from fastapi.testclient import TestClient

import main

# Reachable before login by design: the frontend itself, the login flow, the
# status check boot() polls, and logout (which only clears the token).
PUBLIC = {
    ("GET", "/"),
    ("GET", "/auth/anilist/login"),
    ("GET", "/auth/status"),
    ("POST", "/auth/logout"),
}

PROTECTED = sorted(
    (method, route.path)
    for route in main.app.routes
    if isinstance(route, APIRoute)
    for method in route.methods
    if (method, route.path) not in PUBLIC
)


def test_protected_route_list_is_not_empty() -> None:
    assert len(PROTECTED) > 30


@pytest.mark.parametrize(("method", "path"), PROTECTED)
def test_protected_routes_401_without_login(method: str, path: str) -> None:
    # FR-47: nothing but the login flow is usable without a stored token.
    # Enumerated from the app itself, so a new route that forgets
    # require_auth fails here automatically.
    url = re.sub(r"\{[^}]+\}", "1", path)
    with TestClient(main.app) as client:
        resp = client.request(method, url)
    assert resp.status_code == 401, resp.text
