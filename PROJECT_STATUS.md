# MyAnimeRank — Project Status

*Updated 2026-10-25, after the cleanup (phases 1–3) and hardening (phases 4–7) passes. This replaces the 2026-10-20 reconnaissance snapshot; that version is in git history (`bb0119b`).*

---

## 1. What this project is

A personal, locally-hosted desktop app for ranking anime and manga through **anchored relative scoring**. You score each title on several criteria by comparing it against titles you've already scored in the same genre and media type. It's also an AniList client: login is required (FR-47), and status, progress and overall score sync both ways. Discover, Feed, Characters/Staff/Studios/Users, Favorites, Profile and Insights screens are all backed by AniList's GraphQL API. The spec is `docs/PRD.md` (v2, now including M9).

| Layer | Tech | Version (pinned = installed) |
|---|---|---|
| Language | Python | 3.14.3 locally (README says 3.12+) |
| API server | FastAPI (lifespan startup) | 0.136.3 (Starlette 1.10.0) |
| ASGI server | uvicorn | 0.49.0 |
| Validation | pydantic | 2.13.4 |
| HTTP client | httpx | 0.28.1 |
| Desktop shell | pywebview (frameless) | 6.2.1 |
| DB | SQLite via stdlib `sqlite3` | — |
| Frontend | single vanilla HTML/JS file, no build step | — |
| Test / lint | pytest, ruff (in `requirements-dev.txt`) | 9.0.3 / 0.17.2 |

---

## 2. How it's organized

```text
main.py               FastAPI app: 37 API routes + GET / (frontend); lifespan starts DB + retry loop; AniList error handlers
database.py           schema, queries, rating math, sync helpers (detect_conflict, apply_remote_entry)
models.py             Pydantic request/response models (PRD §7)
anilist.py            ALL AniList GraphQL via one _graphql() (errors/404/429 handling); AniListError, AniListNotFound
sync.py               Pull, guarded push (one-request pre-check), conflict resolution, in-memory retry queue
oauth_listener.py     one-shot OAuth callback listener, fixed port 8931 (escaped error pages)
cache.py              60s TTL cache for Discover/Feed
app.py                pywebview shell: uvicorn subprocess on a free port + frameless window
frontend/index.html   entire UI (real API only; "can't reach backend" screen if the API is down)
tests/                pytest: 174 tests, 9 test files + conftest (temp DB per test, AniList always faked)
requirements.txt      runtime dependencies (direct imports only)
requirements-dev.txt  requirements.txt + pytest + ruff
docs/PRD.md           product contract (authoritative)
docs/anilist-api-notes.md  AniList schema notes and gotchas
README.md             what it is, setup, run, test
pyproject.toml        ruff config
.claude/              CLAUDE.md (agent instructions), hooks, settings, launch.json, plans/
local_config.py       ANILIST_CLIENT_ID / ANILIST_CLIENT_SECRET (gitignored)
library.db            live user data (gitignored); *.db.bak* backups also gitignored
myanimrank.db         v1 database (gitignored), only useful for migrating v1 ratings
backups/*.json        v1 JSON backups (committed)
```

**Data flow:** the frontend calls same-origin `fetch()` against `main.py` routes. `require_auth` gates everything except `/`, `/auth/status`, `/auth/anilist/login` and `/auth/logout`; a test enumerates every route to enforce this. Routes use `database.py` (SQLite) and `anilist.py` (GraphQL). Local edits write SQLite, then queue `sync.push_now_guarded` as a background task.

**Config:** no environment variables. Secrets live in `local_config.py`. The AniList client's redirect URI must be `http://localhost:8931/callback`.

---

## 3. How to run it

```powershell
python -m venv .venv; .venv\Scripts\Activate.ps1
pip install -r requirements-dev.txt   # runtime deps + pytest + ruff (requirements.txt alone to just run)
python app.py                         # desktop window
uvicorn main:app --port 8420          # browser dev
python -m pytest -q                   # tests
ruff check .                          # lint
```

**Current results (2026-10-25):**
- `pytest`: **174 passed**, 1 warning. The warning is Starlette 1.10's TestClient deprecation, asking for a different HTTP client package for tests; not installed, see §7.
- `ruff check .`: app and test code clean. 3 findings remain in `.claude/hooks/`, deliberately untouched (see §6).
- Boot smoke test (TestClient on a temp DB): starts cleanly; `/` 200, `/auth/status` 200, protected routes 401.
- Live run (Phase 3, against a temp copy of the DB and your real AniList account, read-only screens only): boot, library, Discover, Feed, Profile, Insights, detail, character/VA chain, catalog/studio/user search all worked with zero console errors.

---

## 4. Git state

- **Branch `rebuild`**, pushed through `5bab9ce` (Phase 7). The post-Phase-7 follow-ups below are uncommitted.
- `main` and `archive/broken-v1` are unchanged at `c6ac79c` (v1).

| Commit | Phase | What |
|---|---|---|
| `3010521` | 1 | Gitignore (now UTF-8; it was UTF-16 and ignored nothing), dev tooling, rewritten CLAUDE.md |
| `bb0119b` | 1 | First commit of the whole v2 app, which had been untracked for months |
| `e8f6bc0` | 2 | CORS removed, OAuth error HTML escaped, stale comments fixed |
| `67a3f77` | 3 | Frontend mock/offline mode removed; backend-unreachable screen |
| `ff2339b` | 4 | media_type taken from AniList on import; strict `_to_media_type`; NULL-criterion anchors |
| `8f4dd87` | 5 | False-conflict-after-import fix; ratings seeded from AniList scores; sync test coverage |
| `1fce4f4` | 6 | AniList client hardening; one-request pre-push check |
| `5bab9ce` | 7 | Lifespan, ruff config + trivial fixes, README, release.yml deleted, status docs |
| *(uncommitted)* | follow-up | Status validation (FR-17), `requirements-dev.txt`, venv setup in README, `.venv/` ignored |

---

## 5. What was done (phases 1–7)

**Phase 1: docs and instructions**
- The PRD now covers M9 (§4.11, FR-64–FR-67: Studios & Users search and profiles, read-only).
- FR-6 now says non-zero manga audio is rejected with 422.
- CLAUDE.md is rewritten to match the PRD's scope. AGENTS.md and `.codex/` are deleted.

**Phase 2: security**
- Wildcard CORS removed. Any website could previously call the API, including endpoints that delete from your AniList account.
- OAuth callback error text is now escaped.
- Five stale comments fixed.

**Phase 3: frontend**
- Mock data, fixtures, `BACKEND_AVAILABLE` branches, the localStorage library cache (cleared on load) and the demo-only "simulate drift" UI are removed.
- If the backend is down, a single "can't reach the backend" screen shows.
- Fixed along the way:
  - A failed import used to add a fake library row, so every later write 404'd.
  - Progress edits stamped the literal date `2026-07-02`.

**Phase 4: media_type invariant**
- `_to_media_type` raises on anything except ANIME/MANGA.
- `POST /media` stores AniList's `media_type`. A disagreeing client value is ignored and logged rather than rejected, the same way FR-9 treats `overall`.
- Anchors skip rows with a NULL criterion (previously a 500).
- All tests get a temp DB through an autouse fixture.

**Phase 5: sync coverage, plus a real bug**
- Fixed: every Pull-imported title looked locally edited, so the next AniList-side change raised a false conflict. Import now goes through `apply_remote_entry`, which means:
  - AniList values are copied exactly (no local auto-complete).
  - The AniList score seeds a rating, with criteria snapped to 0.5 steps; `overall` keeps AniList's exact score so the next push doesn't rewrite it.
- Confirmed by test: pushing an unrated title never sends a score, so an AniList score can't be cleared.

**Phase 6: AniList robustness**
- `_graphql` changes:
  - A GraphQL `errors` payload → `AniListError` (route → 502 with AniList's message).
  - HTTP 404 → `AniListNotFound` (route → 404).
  - 429 → honors Retry-After, at most 2 retries and 30s per wait.
  - 5xx stays retryable.
- Fixed: AniList 404s unknown ids rather than returning null, so `get_by_id` returned errors instead of `None`. The 404s from `/media/by-anilist` and import now work end to end.
- Pre-push check per edit went from 3 requests (including your **entire** list) to 2. scoreFormat rides along in the entry lookup, so it's still re-checked on every push (FR-28) without a cache.
- Fixed: edits made offline were never queued for retry (FR-26).
- `retry_loop` logs failures instead of swallowing them.

**Phase 7: housekeeping**
- Environment synced to `requirements.txt`.
- The `on_event` startup hook is replaced by a `lifespan` handler, which also cancels the retry loop on shutdown. Warnings went 299 → 1.
- Ruff config added, with trivial fixes (UTC alias, import sorting, `collections.abc`, exception chaining, unused imports). FastAPI's `Depends` is allowlisted for B008.
- README added; v1 `release.yml` deleted.
- `.gitignore` widened to `*.db.bak*`. Your dated backup `library.db.bak-20261025` was *not* ignored before this.

**Follow-ups after Phase 7**
- Status validation: a status must be valid for the title's media type (FR-17, PRD §7), checked on both status edits and import, before anything is stored or pushed. The valid set comes from the existing AniList-enum mapping (`anilist.local_statuses()`), so there's one source of truth.
- `requirements-dev.txt` (runtime deps + pytest + ruff), venv setup in the README, `.venv/` gitignored.
- Checked, no change needed: `app.py` already launches uvicorn with `sys.executable`.

**Tests:** 23 → 174. They now cover `detect_conflict`, Pull/push paths, `apply_remote_entry`, progress auto-complete, anchors, the media_type invariant, AniList error/404/429 handling, auth on every route, CORS, OAuth escaping and lifespan.

---

## 6. Health assessment

**In good shape:**
- AniList-only and the single `media` table hold, and media_type is now enforced at every entry point.
- Sync logic has real test coverage.
- No mock paths remain.
- Failures from AniList surface clearly instead of as 500s.

**Remaining risks:**
1. ~~**Old credentials in public git history.**~~ **Resolved (2026-10-25):** the old AniList token has been revoked and the old client secret deleted, so the copies still in commit `403be7e` (`_secrets.py`, `myanimrank.db`) are dead. Scrubbing them from history is optional cleanup, not urgent.
2. **FR-25/FR-28 score-format flow isn't wired in the frontend.** Nothing calls `POST /auth/score-format/confirm`, and a drift result from a push is never shown. If your account isn't on POINT_10_DECIMAL, pushes silently skip.
3. **Write paths weren't exercised live after Phase 3.** Status, progress, rating, favorite, import and delete are covered by backend tests, but haven't been clicked through in the real UI since the mock removal. Try one of each once.
4. **The retry queue is in-memory.** Queued pushes are lost on app close. An item that becomes permanently failing logs a traceback every 30s.

**Ruff findings left in `.claude/hooks/`** (not app code, left untouched on purpose): I001 import order and BLE001 blind except in `pre_bash.py` (your security gate); PLW1510 `subprocess.run` without `check` in `session_end.py`.

---

## 7. Still open

Roughly by priority:

1. ~~Rotate the old AniList client secret and revoke the old token~~ **Done.** Optional, not urgent: scrub `403be7e` from history with `git filter-repo` and a force-push.
2. ~~Push `rebuild` and commit Phase 7~~ **Done** (`5bab9ce`, pushed).
3. **Wire FR-25/FR-28:** the consent dialog on first login, and a drift dialog when a push reports `score_format_drift`.
4. ~~Validate status values~~ **Done:** `PUT /media/{id}/status` and `POST /media` now return 422 unless the status is one of the FR-17 values for that title's media type.
5. **Token expiry** (PRD §11 open question): `token_expires_at` is stored but never checked; show a clear re-login prompt.
6. **Click-test the write paths** in the real app once (see §6.3).
7. **Frontend `anchorsFor()`** treats a NULL criterion as score 0 (the client-side twin of the Phase 4 server fix).
8. **Request volume follow-ups:**
   - Debounce rapid +1 progress clicks; each click is currently its own push.
   - Use the single-entry lookup in `sync.resolve(winner="remote")` and `get_pending_conflicts`, which still fetch the whole list.
9. **Schema defects** (each needs a migration against live data): nullable `rating.animation`, no `ON DELETE CASCADE`.
10. **Persist the retry queue** across restarts (FR-26).
11. **UI labels use `text-transform:uppercase`**, against the CLAUDE.md convention.
12. **v2 packaging** (PyInstaller or similar) as its own task, now that the v1 workflow is gone.
13. **Test-client warning:** decide whether to add the HTTP client package Starlette 1.10's TestClient now asks for (a new dev dependency).
