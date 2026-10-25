# MyAnimeRank — Project Status

*Read-only reconnaissance, 2026-10-20. No code was modified. The only commands run were tests, a compile check, a smoke boot against a throwaway temp DB, and read-only SQLite opens.*

---

## 1. What this project is

**Purpose.** A personal, locally-hosted desktop app for ranking anime and manga through *anchored relative scoring*. You score each title on several criteria by comparing it against titles you've already scored, filtered to the same genre and the same media type. In its current form (v2 rebuild) it also acts as an AniList client: it requires an AniList OAuth login and syncs status, progress and score both ways. It adds Discover, Feed, Characters/Staff/Studios, Favorites, Profile and Insights screens, all backed by AniList's GraphQL API.

**Tech stack**

| Layer | Tech | Pinned (`requirements.txt`) | Actually installed locally |
|---|---|---|---|
| Language | Python | — | **3.14.3** (CI workflow uses 3.12) |
| API server | FastAPI | 0.136.3 | **0.115.6** |
| ASGI server | uvicorn | 0.49.0 | **0.34.0** |
| Validation | pydantic | 2.13.4 | **2.13.1** |
| HTTP client | httpx | 0.28.1 | 0.28.1 |
| Desktop shell | pywebview (frameless window) | 6.2.1 | 6.2.1 |
| DB | SQLite via stdlib `sqlite3` (no ORM) | — | — |
| Frontend | One hand-written HTML/JS file (~2.7k lines). AGENTS.md calls it "React", but it's string-template rendering with no framework. Google Fonts are loaded from a CDN. | — | — |
| Metadata | AniList GraphQL (`https://graphql.anilist.co`) only | — | — |

> ⚠️ Your local environment doesn't match `requirements.txt` (fastapi and uvicorn are several minors behind the pins). Tests pass on the installed versions; I haven't verified the pinned versions.

---

## 2. How it's organized

### Directory map

```text
main.py               FastAPI app: 41 routes, plus GET / serving frontend/index.html
database.py           SQLite schema, migrations, all queries, rating math (compute_overall, anchors, insights)
models.py             Pydantic request/response models (PRD §7 contract); RatingIn validation
anilist.py            ALL AniList GraphQL: one _graphql() helper + ~30 query/mutation functions
sync.py               Pull / push / conflict detection + in-memory retry queue
oauth_listener.py     One-shot HTTP server on fixed port 8931 for the OAuth callback
cache.py              60s in-memory TTL cache (Discover/Feed only)
app.py                pywebview shell: starts uvicorn as a subprocess on a free port, opens a frameless window, exposes window-control JS API
local_config.py       ANILIST_CLIENT_ID, ANILIST_CLIENT_SECRET (gitignored)
frontend/index.html   Entire UI (CSS + JS); real-API mode and a mock/offline mode
tests/                pytest: test_anilist.py (1 test), test_database_ratings.py (22 test cases)
docs/PRD.md           PRD v2 — the authoritative spec (536 lines)
docs/anilist-api-notes.md  AniList schema/gotcha notes
AGENTS.md             Agent instructions (from the Codex era) — partly stale, see §6
.claude/              CLAUDE.md, hooks (pre_bash.py, session_end.py), settings, launch.json, plans/ (M7, M8, app.py plans)
.codex/hooks.json     Legacy Codex hook config; hooks/ dir is empty
.github/workflows/release.yml  v1-era PyInstaller release workflow (broken for v2, see §5)
backups/*.json        10 v1 JSON backups from 2026-06-15 (committed on rebuild)
library.db            Live v2 user DB (gitignored): 40 media (37 anime, 3 manga), 13 ratings, 1 auth row
library.db.bak        Byte-identical copy of library.db — NOT gitignored, contains the access token
myanimrank.db         v1 database (tables: anime, friend, friend_entry, larp_result, search_cache…) — 5.8 MB, NOT gitignored
```

### Entry points

- **Desktop:** `python app.py` → `app.py:main()` finds a free port, spawns `python -m uvicorn main:app`, waits for `/auth/status`, then opens a pywebview window at that origin. Closing the window terminates the subprocess.
- **Browser dev:** `uvicorn main:app --port 8420` (`.claude/launch.json`), then open `http://127.0.0.1:8420/`.

### Data flow

```text
frontend/index.html ──fetch(window.location.origin + path)──▶ main.py routes
   │                                                           │ require_auth() → database.get_auth()
   │                                                           ├─▶ database.py (SQLite library.db)
   │                                                           ├─▶ anilist.py._graphql() ──▶ graphql.anilist.co
   │                                                           └─▶ sync.py (pull / push_now_guarded as BackgroundTasks)
   │
   └─ login: window.open(/auth/anilist/login) → AniList authorize → redirect to
      localhost:8931/callback (oauth_listener.py) → exchange code → database.save_auth()
      frontend polls /auth/status every 1.5s until connected
```

- Boot (`frontend/index.html:2675`): detect backend → `/auth/status` → if connected, `POST /sync/pull` then `GET /library`.
- Every local edit (rating/status/progress/repeat) writes SQLite, then queues `sync.push_now_guarded` as a FastAPI background task (`main.py:215`, `237`, `246`, `255`).
- `overall` is always computed server-side in `database.compute_overall` (`database.py:179`).

### Config and environment

- **No environment variables are read by the app.** Secrets live in `local_config.py` as module constants: `ANILIST_CLIENT_ID`, `ANILIST_CLIENT_SECRET`.
- Fixed OAuth port is `8931` (`oauth_listener.py:20`). The redirect URI registered with AniList must be `http://localhost:8931/callback`.
- CI (`release.yml`) expects a GitHub secret named `ANILIST_CLIENT_SECRET` and writes it to `_secrets.py`, a v1 filename that v2 doesn't use.
- No `pyproject.toml`, linter, type-checker or formatter config exists. ruff, mypy, pyright and pip-audit aren't installed.

---

## 3. How to run it

```powershell
pip install -r requirements.txt        # aligns your env with the pins (currently out of sync)
python -m pytest -q                    # tests
uvicorn main:app --port 8420           # browser dev
python app.py                          # desktop window
```

> Starting the server runs `database.init_db()`, which may `ALTER TABLE` `library.db` (`database.py:87`). `library.db.bak` is currently an exact copy, so you have one backup.

There is no build step (there's no bundler; the frontend is a static file). PyInstaller packaging only exists in the v1 workflow and won't work as-is.

### Results of what I actually ran

| Check | Result |
|---|---|
| `python -m pytest -q` | ✅ **23 passed**, 84 warnings (all deprecations: FastAPI `on_event` at `main.py:96`, and `asyncio.iscoroutinefunction` inside FastAPI on Py 3.14) |
| `python -m py_compile *.py` | ✅ OK |
| Smoke boot via `TestClient` with `database.DB_PATH` pointed at a temp file | ✅ startup OK; `GET /` → 200 (230 KB HTML); `/auth/status` → `{"connected":false}`; `/library`, `/rankings`, `/insights` → 401 (auth gate works); `/openapi.json` → 200; 41 routes registered |
| `pip check` | ✅ No broken requirements |
| `python app.py` (real window) | ⏭️ Not run: it would touch the live `library.db` and needs a GUI |
| Lint / type check | ⏭️ None configured |

---

## 4. Where you left off (git state)

### Branches

| Branch | Points at | Purpose |
|---|---|---|
| `rebuild` *(current)* | `e0c9e87` | v2 rebuild. **3 commits ahead of `origin/rebuild` (unpushed).** |
| `main` / `origin/main` | `c6ac79c` "Goated changes" (tag `v2.2.0`) | Old v1 app (jikan.py, larp.py, compare.py, MyAnimeRank.html, PyInstaller spec) |
| `archive/broken-v1` (branch **and** tag) | `c6ac79c` | Same commit as `main`, a snapshot of v1 before the rebuild |
| `origin/rebuild` | `403be7e` "Start rebuild from scratch" | Remote copy of rebuild, 3 commits behind |

No stashes.

### Commit history (last ~25, all branches)

- **2026-06-10 → 06-15 (v1 on `main`):** ~20 rapid commits with tags `v2.0.0-beta` … `v2.2.0`: exe backup, friend sync ("FRIEND SYNC WORKS"), settings persistence, "Rate series / Rerate all". Messages are terse ("idk", "larp larp sahur", "Bug fixes").
- **2026-06-30** `403be7e` Start rebuild from scratch.
- **2026-07-02** `66b3bb0` gitignore for local secrets.
- **2026-07-03** `5a6c60d` requirements.txt for FastAPI backend.
- **2026-07-04** `e0c9e87` Untrack stale pre-rebuild artifacts and pycache. **← last commit, 3.5 months ago.**

### Uncommitted state — the critical part

**The v2 application has never been committed.** On `rebuild`, git tracks only `.claude/*`, `.gitignore`, `.vscode/extensions.json`, `PRD.md` (old), `backups/*.json`, and `requirements.txt`. Everything that is the actual app is **untracked**:

`main.py`, `database.py`, `models.py`, `anilist.py`, `sync.py`, `oauth_listener.py`, `cache.py`, `app.py`, `frontend/`, `tests/`, `docs/`, `AGENTS.md`, `.github/`, `.claude/plans/`, `.claude/launch.json`, `.claude/settings.local.json`, `.codex/`.

That's months of work (M2–M8, plus an "M9") sitting only on this disk.

**Modified (unstaged):**
- `.claude/CLAUDE.md`: rewritten (+126 lines) with project context, non-goals and status.
- `.claude/hooks/pre_bash.py`: now reads hook input from stdin (`tool_input.command`) and exits 2 to block (the correct Claude Code protocol). The old version read `CLAUDE_TOOL_INPUT`.
- `.claude/settings.json`: `python3` → `python` (Windows), key reorder.
- `.gitignore`: binary diff 70 → 46 bytes. Probably an encoding change (e.g. UTF-16 → UTF-8); current content is `local_config.py`, `library.db`, `__pycache__/`, `*.pyc`.
- `requirements.txt`: trimmed to 5 direct deps.
- `PRD.md` (root) **deleted**. This was the *old* v1-era "Version 3.0 … Taste-Comparison Tool" PRD; the v2 PRD lives at `docs/PRD.md`.

**Staged:** deletion of `.devcontainer/` (Dockerfile, devcontainer.json, init-firewall.sh). The plans reference `/workspace/PRD.md` and a Linux container, so earlier milestones were built in that devcontainer before you moved to native Windows.

### What you were working on most recently (from file mtimes)

| Date | Files | Interpretation |
|---|---|---|
| Aug 14 | `docs/PRD.md`, `sync.py`, `app.py`, `anilist-api-notes.md` | Finishing M4–M8 + desktop shell |
| Sep 6 | `main.py`, `models.py`, `tests/test_database_ratings.py` | Rating validation (`RatingScore`, `_validate_rating_audio`) + first tests |
| Sep 10 | `anilist.py`, `tests/test_anilist.py`, `oauth_listener.py`, `requirements.txt`, `.codex/hooks.json` | `_graphql()` consolidation, requirements trim, hook migration |
| **Sep 27** | `.claude/CLAUDE.md`, `frontend/index.html`, `database.py`, `AGENTS.md` | **Audit cleanup pass**: dead-code removal, search-dispatcher consolidation, docs |

### Half-finished work

- **Audit cleanup list in CLAUDE.md.** Debounced-search consolidation is marked *pending*, but it looks **done**: `frontend/index.html:214-252` already has one `SEARCH_FETCHERS` dispatcher, one `_searchResults` map and one shared debounce timer. Mock/offline runtime removal is still pending (see §5/§6).
- **`.codex/` migration.** `hooks/` is empty; `hooks.json` only points at `.claude/hooks/*`. CLAUDE.md says it can be deleted once migration is complete.
- **`.devcontainer` deletion** is staged but not committed.
- **"M9" (studios + user search/profiles).** Implemented (`main.py:355-381`, `anilist.py:732-893`, frontend search categories) but not in the PRD (see §6).

---

## 5. Loose ends

### TODO / FIXME / HACK

**None.** A repo-wide grep for `TODO|FIXME|HACK|XXX|NotImplemented` and for skip/xfail markers found nothing. The only bare `pass` is a test stub (`tests/test_anilist.py:24`), which is fine.

### Stale or misleading comments (code says one thing, comment another)

| Location | Problem |
|---|---|
| `frontend/index.html:46-55` | Says "app.py never sets easy_drag=False", but `app.py:98` sets `easy_drag=False`. |
| `frontend/index.html:626-633` | Says "no app.py/pywebview shell exists yet". It does. |
| `main.py:72-75` | Says the frontend is "served from a different origin". Since `GET /` was added, it's same-origin, so the `allow_origins=["*"]` CORS rationale no longer holds. |
| `database.py:6-7` | Says `anilist_auth` "stays unused until the M4 OAuth milestone". M4 is done. |
| `anilist.py:1` | Docstring says "search and by-id lookups only". The file now holds all ~30 AniList operations. |

### Dead or legacy code and files

- **Mock/offline mode in `frontend/index.html`:** mock data at `:299-335`, `CATALOG`, `INSIGHTS`, demo PERSON and community fixtures (`:384-540`), plus about 40 `BACKEND_AVAILABLE` branches. In the real app (pywebview or `uvicorn`) the backend is always reachable, so this only runs if the HTML is opened standalone. Nothing else depends on it that I could find. It also conflicts with the PRD (see §6).
- **`localStorage` library persistence (`:337-357`, `persistLibrary()` called in every `render()` at `:1440`):** a v1/mock-era fix. In real mode it still writes your real AniList library into `localStorage['mar_library']` on every render.
- **`.github/workflows/release.yml`:** v1 workflow. It builds `MyAnimeRank.spec` (exists only on `main`), writes `_secrets.py` (v2 uses `local_config.py`), and its release notes say "no account, no cloud" (v2 *requires* AniList login). Pushing a `v*` tag from `rebuild` would fail the build.
- **`myanimrank.db`:** v1 database (5.8 MB), untracked and not ignored. Only useful if you want to migrate v1 ratings (11 rows). Note the misspelled name.
- **`backups/*.json`:** 10 v1 backups committed on `rebuild`. Probably not v2 data.
- **`database.is_connected()` (`database.py:340`):** used only by `sync.retry_loop`. Fine, just thin.
- **`.codex/`:** see above.

### Failing or skipped tests

None. 23/23 pass.

### Dependencies

`pip list --outdated` (installed → latest): fastapi 0.115.6 → **0.156.4**, starlette 0.41.3 → **1.9.0**, uvicorn 0.34.0 → **0.54.0**, pydantic 2.13.1 → 2.14.1. Your pins (fastapi 0.136.3, uvicorn 0.49.0) are already newer than what's installed. I didn't run a CVE scan (pip-audit isn't installed). Starlette 0.41.x is old enough that upgrading is worth doing on general principle.

---

## 6. Health assessment

### Biggest risks (ordered)

1. **All v2 code is uncommitted.** One disk failure, an accidental `git clean`, or a bad checkout loses the whole rebuild. `origin/rebuild` has none of it.

2. **The project instruction files contradict the PRD and each other.**
   - **CLAUDE.md** lists as non-goals "No AniList OAuth, list sync, or friend comparison" and "No arbitrary user search / social features". But **`docs/PRD.md` §2 Goals, §4.6 (FR-22–FR-28), §4.8 Feed and FR-47 explicitly put OAuth login, two-way sync and Feed in scope**, and the code implements them. The PRD is meant to win ("Always refer to PRD"), so CLAUDE.md's non-goals section is out of date. That makes it dangerous: an agent following it could rip out working, specified features.
   - **User search and profile browsing** (`/users/search`, `/users/{id}`, and also `/studios/search`) are *not* in the PRD's API contract (§7) or milestones (M1–M8). CLAUDE.md says they're a non-goal. They exist as "M9". **This needs a product decision from you.**
   - **AGENTS.md** is stale. It says the PRD is at the repo root (now `docs/PRD.md`), "No `tests/` directory" (there is one), known defect #1 "RatingIn doesn't validate range/0.5 steps" (fixed in `models.py:59-67`), dev port 8000 (launch.json uses 8420), and calls the frontend "React".
   - **Manga audio behavior:** the PRD (FR-6) and CLAUDE.md say audio is "excluded … regardless of what the client sends". But `main.py:120-131` **rejects** non-zero manga audio with a 422, and tests enforce the 422 (`tests/test_database_ratings.py:248`). That's a stricter, deliberate-looking divergence; either the PRD or the code should change.

3. **Wide-open CORS on an API that mutates your real AniList account** (`main.py:76-82`, `allow_origins=["*"]`, all methods). The only auth is "a token exists in the DB". Any webpage open in a normal browser could call `http://127.0.0.1:<port>/...` while the app runs, e.g. `DELETE /media/{id}`, which also deletes the entry from AniList (`sync.delete_remote_entry`). The port is random under `app.py` (lower risk) but fixed at 8420 in dev. Since the frontend is now same-origin, the CORS middleware can most likely be removed outright.

4. **The login gate (FR-47) can be bypassed by the frontend's mock and offline modes.**
   - When the backend is unreachable, `H.connect` (`frontend/index.html:651`) sets `connected=true` with no login.
   - Because `render()` persists the real library to `localStorage`, that offline session can show cached real data, which FR-47 explicitly forbids.
   - AGENTS.md defect #4 already flags this as your decision.

5. **The media_type invariant has two soft spots.**
   - `anilist._to_media_type` (`anilist.py:68-69`) maps *anything that isn't `"ANIME"`* to `"manga"`. That's a silent default, which CLAUDE.md forbids. It should raise on unexpected values.
   - `POST /media` (`main.py:178-181`) trusts the **client-supplied** `media_type` in `MediaImport` instead of re-fetching it from AniList by `anilist_id`. That's the same "never trust the client" logic you apply to `overall`, but not applied here.

6. **AniList rate-limit and error handling.**
   - Every local edit's background push does about 3 requests: `push_now_guarded` fetches the **entire** MediaListCollection (`sync.py:224`), `push_now` calls `get_viewer` (`sync.py:153`), then the mutation. Rapid edits (dragging a progress counter) can approach AniList's ~90 req/min.
   - `_graphql()` (`anilist.py:12-25`) doesn't handle GraphQL `errors` payloads (a 200 with `data: null` becomes a `TypeError`/500) and has no 429/`Retry-After` handling.

7. **Sync robustness.**
   - The retry queue is in-memory only (`sync.py:16`), so queued pushes are lost on app close.
   - `retry_loop` swallows all exceptions silently (`sync.py:258-259`).
   - `token_expires_at` is stored but never checked anywhere. The PRD Open Questions (§11) flags expired-token UX as unresolved.

8. **Minor items.**
   - The OAuth callback reflects the `error` query param and exception text into HTML unescaped (`oauth_listener.py:84`, `:94`). It's localhost-only, so low risk.
   - `rating.animation` is nullable in the schema (`database.py:66`) while `AnchorItem.score: float` is required. A legacy NULL row would 500 the anchors endpoint.
   - `library.db.bak` and `myanimrank.db` aren't gitignored, so a careless `git add .` would commit your OAuth token.

### Confusing areas

- The `rating.animation` column stores **Art** for manga (`database.py:175`, the UI relabels it). It's documented in the PRD §5 but surprising.
- `apply_remote_entry` (`database.py:401-439`) deliberately lets `overall` disagree with the criteria after a remote score change. It's well commented, but expect confusing Detail views.
- There are three overlapping instruction files (CLAUDE.md, AGENTS.md, PRD.md) plus three plan files that reference a `/workspace` container path that no longer exists.

### Missing tests (by risk)

The existing tests cover overall computation, manga audio forcing, anchor media-type isolation, media_type immutability on re-import, and rating validation. Not covered:
- **`database.detect_conflict`**: pure logic and the heart of sync. Untested.
- **`sync.pull` / `push_now_guarded`** (with `anilist` monkeypatched): new-entry import, one-sided apply, conflict flagging, and the "no remote counterpart ≠ conflict" rule.
- **`database.update_progress`**: auto-complete and rewatch counting (FR-53/57).
- **`database.apply_remote_entry`**: the score-overwrite and seed logic.
- **`anilist._map` / `_to_media_type`**: media_type mapping from AniList's enum.
- **Genre-scoped anchors** (only `scope="all"` is tested).
- `require_auth` gating on routes; `get_insights` bucketing.
- There's no frontend testing at all, which is expected for a single HTML file.

### Missing docs

- No README on `rebuild` (v1's README is on `main` only).
- No setup instructions for registering an AniList API client with redirect URI `http://localhost:8931/callback` and creating `local_config.py`.

---

## 7. Suggested next steps (prioritized)

1. **Commit the v2 code and push `rebuild`.** Do the `.gitignore` additions first (`library.db.bak`, `myanimrank.db`, `.pytest_cache/`, `*.db.bak`, `.claude/settings.local.json`). Right now the whole rebuild exists only on this disk.
2. **Fix CLAUDE.md's non-goals so they match the PRD**, and refresh AGENTS.md (PRD path, tests exist, defect #1 fixed, port, "React"), or delete AGENTS.md if Claude Code is now your only agent. The current non-goals contradict shipped, specified features and will steer future agent sessions wrong.
3. **Decide on "M9" (user search/profiles, studio search).** Either add them to the PRD or remove `main.py:355-381`, `anilist.py:732-893` and the frontend categories. Scope should be explicit, not accidental.
4. **Decide the mock/offline-mode question, then act.** If FR-47 stands (the PRD says it does), remove mock mode and the `localStorage` library persistence in `frontend/index.html`. This also closes the login-gate bypass and deletes a large share of the file's branching.
5. **Remove or tighten CORS** (`main.py:76-82`). The frontend is same-origin now, so `allow_origins=["*"]` is unnecessary and lets any webpage drive destructive endpoints.
6. **Harden the media_type invariant.** Make `_to_media_type` raise on unknown values, and have `POST /media` take `media_type` from AniList (`get_by_id`) rather than the client body. Write the tests first, per your conventions.
7. **Add tests for sync logic:** `detect_conflict`, `pull()`, `push_now_guarded()` with a fake `anilist`, plus `update_progress`. This is the highest-risk untested code and the area that broke v1.
8. **Make `_graphql()` handle GraphQL `errors` and 429s**, and cut the per-edit request count (e.g. fetch a single `MediaList` entry instead of the whole collection in `push_now_guarded`, and cache `score_format` instead of calling `get_viewer` on every push).
9. **Sync your env to the pins and modernize.** Run `pip install -r requirements.txt`, then move `@app.on_event("startup")` to a `lifespan` handler (clears the deprecation warnings). Consider adding ruff, since your conventions say to "run linters if configured".
10. **Retire or rewrite `.github/workflows/release.yml`** (it references the v1 `.spec` and `_secrets.py`), delete `.codex/`, fix the five stale comments listed in §5, and resolve the manga-audio 422-vs-ignore mismatch with the PRD.
