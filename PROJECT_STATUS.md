# MyAnimeRank — Project Status

*Updated 2026-10-27, after the port to Tauri 2 + Rust and the removal of the
Python app (branch `remove-python`). The 2026-10-25 Python status (cleanup and
hardening, phases 1–7) is in git history (`bea0e17`).*

---

## 1. What this project is

A personal, local desktop app for ranking anime and manga through **anchored
relative scoring**: you score each title on several criteria by comparing it
against titles you've already scored in the same genre and media type. It's
also an AniList client: login is required (FR-47), status, progress and
overall score sync both ways, and Discover, Feed, Characters/Staff/Studios/
Users, Favorites, Profile and Insights are backed by AniList's GraphQL API.
The spec is `docs/PRD.md` (v2, through M10: Friend Comparison).

| Layer | Tech |
|---|---|
| Desktop shell | Tauri 2 (frameless window, custom titlebar) |
| Language | Rust (edition 2021) |
| API | axum router, run in-process through one Tauri command (no server, no port) |
| HTTP client | reqwest (native TLS) |
| DB | SQLite via rusqlite (bundled), same schema as the Python app |
| Frontend | single vanilla HTML/JS file, no build step |
| Login | AniList implicit grant, built-in client ID, no secret |
| Tests / lint | `cargo test` (72), `cargo clippy --all-targets` (clean) |

Platforms: Windows (WebView2), macOS (WebKit), Linux (WebKitGTK).

---

## 2. Why the port happened

The goal was a lighter, faster app. The Python version ran three things: a
Python interpreter, a uvicorn server in a subprocess, and a pywebview window
talking to it over HTTP. The Rust version is one native process (an ~8.5 MB
release binary) plus the OS webview. On Windows the whole app measured about
100 MB in Task Manager.

Tauri was chosen over a pure-Rust UI (GPUI, Iced, Slint) because it keeps the
existing frontend, which made the port a backend rewrite rather than a full
UI rebuild. A native UI would use less memory still, at the cost of
rebuilding every screen.

---

## 3. How it's organized

```text
src-tauri/src/main.rs     entry point; `--serve <port>` = browser dev mode
src-tauri/src/lib.rs      app state, Tauri setup, `api` and `start_login` commands
src-tauri/src/api.rs      every route (same paths/JSON/status codes as PRD §7), auth gate
src-tauri/src/db.rs       schema, migration of older DBs, rating math, queries
src-tauri/src/anilist.rs  all GraphQL through one graphql() (errors/404/429 handling)
src-tauri/src/sync.rs     Pull, guarded push, conflicts, in-memory retry queue
src-tauri/src/oauth.rs    one-shot callback listener on 8931 (implicit grant)
src-tauri/src/cache.rs    60s TTL cache for Discover/Feed
src-tauri/src/config.rs   built-in AniList client ID + optional overrides
src-tauri/tests/backend.rs  integration tests: in-memory DB, fake AniList server
frontend/index.html       entire UI; talks to the API via Tauri, or fetch under --serve
```

**Data flow:** the page calls `apiFetch(path)`. In the app that becomes
`invoke('api', {method, path, body})`, which hands the request to the axum
router in-process. In `--serve` mode it's a normal same-origin fetch. Every
route except `/`, `/auth/status`, `/auth/anilist/login` and `/auth/logout` is
behind the login gate. Local edits (status, progress, repeat, rating, import)
write SQLite, then push to AniList in the background with a pre-push conflict
check (FR-58).

**Where data lives:** `library.db` in the OS app-data folder for
`com.erikgastelum.myanimerank` (Windows: `%APPDATA%\com.erikgastelum.myanimerank\`),
or wherever `MYANIMERANK_DB` points. The app creates the folder if it's
missing, and shows an error dialog instead of crashing if it can't open the
database.

---

## 4. How it was verified

- **Parity with the Python app:** both backends were run on copies of the same
  fixture database and sent the same requests, including writes and error
  cases. Responses and final database state were identical (46/46 in the first
  run, 80 requests in a later review), apart from error-detail wording.
- **Tests:** 51 Rust tests (50 in `tests/backend.rs`, 1 in `config.rs`).
  Before the Python app was deleted, its 174 pytest cases were read through
  and everything not yet covered was ported: the full conflict-detection
  table, Pull imports of both media types (seeded ratings, values copied
  as-is, local-only rows untouched), guarded push (two-request clean push,
  score-format recheck, conflict leaves the retry queue, unrated never sends
  a score), media_type from AniList on import, NULL-criterion anchors,
  per-type status and score validation, AniList error classes (GraphQL
  errors, 404, 5xx, 429 with and without Retry-After), the login gate on
  every route enumerated from `api.rs`, and no CORS headers. No ported test
  found a behavior difference.
- **Not ported:** the Python retry-loop logging test (the Rust loop sleeps a
  real 30 s; testing it would need tokio's `test-util` feature) and the
  FastAPI lifespan tests (no Rust equivalent).
- **By hand:** Windows (window controls, library, login, writes, adding a
  title syncs to AniList, ~100 MB memory) and Linux (fresh user with no
  config, an existing library, login URL).
- **Friend Comparison (M10):** 20 more tests (71 in `tests/backend.rs`): the
  friend tables added to an existing `library.db` with its other tables
  byte-identical, the pre-FR-79 `friend` table migrated without a value
  changing, cascade on removal, no token and no mutation on friend fetches,
  score normalization (POINT_5/POINT_3 read in the friend's own format),
  private/unknown/own-account refusals, an empty public list, the comparison
  math, friend sync (pagination, mutual only, unfollow vs manual, hide), the
  list queue (one friend per turn, pauses for other calls and after a 429, a
  failing friend doesn't block it, private friends wait a week), a
  comparison fetching first, sync after login and Pull, and every PRD §7
  shape. The UI was driven in headless Chromium against the real router.
  Checked against the live AniList API (logged out) on temp DBs: public,
  private, unknown and empty-list users, every score format, lists of 11,905
  and 19,944 entries, add/compare/refresh/remove, two requests per fetch, a
  56-follow sync, mutual only, the queue's pace, a comparison jumping the
  queue, and the migration on a database the earlier build wrote. Findings in
  `docs/anilist-api-notes.md` #17 and #18.

---

## 5. Differences from the Python app (deliberate)

- Login uses the implicit grant with a built-in client ID. No secret and no
  config file needed, and the login page opens in the system browser. A
  different client ID can come from `ANILIST_CLIENT_ID` or `local_config.toml`.
- Adding a title (`POST /media`) now pushes it to AniList right away. Python
  only pushed on the next status/progress/rating change.
- Pull imports each new entry atomically and skips a failing entry instead of
  aborting the whole pull, and it runs right after login.
- `/insights` treats a NULL criterion as 0, where Python returned 500.
- The database lives in the app-data folder, not the repo.
- Expanded/collapsed library sections persist between launches (the page
  origin is now fixed).
- Error-detail text on 422s is a single string, not FastAPI's list.

---

## 6. Git state

- Default branch: **`main`**. `rebuild` and `tauri-port` were merged in and
  deleted.
- Current branch: **`remove-python`** (deletes the Python app, ports its
  remaining test coverage, moves the PRD's stack sections to Rust). Its first
  commit (`f611e7f`) is also on local `main`; neither branch is pushed.

| Commit | What |
|---|---|
| `7c54c06` | Port the desktop app to Tauri 2 + Rust (`src-tauri/`) |
| `bc9f2e1` | Unclip detail Status/Progress menus; require title on import; clippy |
| `fe9c2c3` | Hooks use `$CLAUDE_PROJECT_DIR` |
| `6a6a9a8`, `29504b4` | Login: implicit grant, built-in client ID; Pull after login |
| `c948df3` | Pull: atomic per-entry import, skip failures |
| `8995800` | Create a missing DB folder; error dialog instead of panic |
| `5e322b1` | Push a newly added title to AniList (FR-24) |
| `1d9cd8c` | README: macOS and Linux setup |

---

## 7. Still open

Roughly by priority:

1. **Downloadable builds for friends:** a GitHub Actions release workflow
   (tauri-action) that builds Windows/macOS/Linux installers on a version tag.
   There is no `.github/` yet. Builds will be unsigned (SmartScreen and
   Gatekeeper warnings on first launch).
2. **`--serve` defaults to `./library.db`.** `.claude/launch.json` runs it
   from `src-tauri/`, so it uses `src-tauri/library.db` unless
   `MYANIMERANK_DB` is set. Consider requiring `MYANIMERANK_DB` in that mode.
3. **Retry queue is in memory only (FR-26):** queued pushes are lost when the
   app closes. See item 6.
4. **Wire FR-25/FR-28:** the score-format consent dialog on first login, and a
   drift message when a push reports `score_format_drift`.
5. **Token expiry:** `token_expires_at` is stored but never checked (implicit
   grant tokens last about a year); show a clear re-login prompt.
6. **Persist the retry queue** across restarts (FR-26).
7. **macOS look:** the custom Windows-style titlebar buttons; consider native
   traffic-light controls on macOS only.
8. **Schema defects** (each a migration against live data): nullable
   `rating.animation`, no `ON DELETE CASCADE`.
9. **Hero tint CSS** is invalid and silently dropped; fixing it changes the look.
10. **UI labels use `text-transform:uppercase`**, against the CLAUDE.md convention.
11. **Request volume** (AniList's limit is now 30 req/min): debounce rapid +1
    progress clicks; use the single-entry lookup in `resolve(remote)` and
    pending conflicts instead of the whole list. Discover fires its 6 shelves
    in parallel per media type (12 with the toggle), and the push retry loop
    retries every queued title each 30 s without stopping at a 429, so a long
    queue can keep the account rate-limited.
12. **Friend Comparison, still unverified:** the desktop window by hand
    (Friends section, Compare page, taste-match tooltip, friend chips on a
    detail page); adding a private user *by name* (only by id was possible);
    a real 429's `Retry-After`; whether AniList lists private users in a
    following list at all.
13. **Friend sync cost:** mutual only reads every followers page, so a popular
    account can spend a whole minute's 30 requests on one sync (see
    `docs/anilist-api-notes.md` #18). The queue's skip-after-failure list is
    in memory, like the push retry queue.
