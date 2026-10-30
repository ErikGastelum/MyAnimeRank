# MyAnimeRank — Project Status

*Updated 2026-10-31: Friend Comparison (M10) is on `main`, and the fixes from
its independent audit are on `feature/friend-compare`. The port to Tauri 2 +
Rust replaced the Python app earlier (branch `remove-python`, merged); the
2026-10-25 Python status (cleanup and hardening, phases 1–7) is in git
history (`bea0e17`). 2026-10-30: Pick an anime (FR-70, branch
`feature/pick-an-anime`).*

---

## 1. What this project is

A personal, local desktop app for ranking anime and manga through **anchored
relative scoring**: you score each title on several criteria by comparing it
against titles you've already scored in the same genre and media type. It's
also an AniList client: login is required (FR-47), status, progress and
overall score sync both ways, and Discover, Feed, Characters/Staff/Studios/
Users, Favorites, Profile, Insights and Pick an anime are backed by AniList's
GraphQL API. The spec is `docs/PRD.md` (v2, through M10: Friend Comparison,
and Pick an anime).

| Layer | Tech |
|---|---|
| Desktop shell | Tauri 2 (frameless window, custom titlebar) |
| Language | Rust (edition 2021) |
| API | axum router, run in-process through one Tauri command (no server, no port) |
| HTTP client | reqwest (native TLS) |
| DB | SQLite via rusqlite (bundled), same schema as the Python app |
| Frontend | single vanilla HTML/JS file, no build step |
| Login | AniList implicit grant, built-in client ID, no secret |
| Tests / lint | `cargo test` (108), `cargo clippy --all-targets` (clean) |

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
src-tauri/src/sync.rs     Pull, guarded push, conflicts, in-memory retry queue, friend
                          sync, the friend-list queue (one fetch per friend at a time)
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
- **Tests:** 108 Rust tests (107 in `tests/backend.rs`, 1 in `config.rs`).
  Pick an anime's 23 run against a fake AniList that pages like the real
  one (5000-entry limit included) and, for the prequel check (FR-71), has
  relations and per-id lookups; its queries were also run against the live
  API, logged out (`docs/anilist-api-notes.md` #17-#26, Re:Zero's chain
  included).
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
- **Friend Comparison (M10):** 34 more tests (84 in `tests/backend.rs`): the
  friend tables added to an existing `library.db` with its other tables
  byte-identical, the pre-FR-79 `friend` table migrated without a value
  changing, cascade on removal, no token and no mutation on friend fetches,
  score normalization (POINT_5/POINT_3 read in the friend's own format),
  private/unknown/own-account refusals, an empty public list, the comparison
  math, friend sync (pagination, mutual only by `isFollower` with your token
  and its followers-list fallbacks, unfollow vs manual, hide, unhide), the
  list queue (one friend per turn, pauses for other calls and after a 429, a
  failing friend doesn't block it, private friends wait a week, both waits
  survive a restart), a
  comparison fetching first, a comparison or Refresh during a fetch reusing
  it, one sync per login plus its Pull, the comparison's rules (always the 5
  largest gaps however small, "rarely disagree" only within half a point,
  completed lists tied by popularity, rows carrying your criteria and
  whether you rated in the app), popularity from Search imports and Pull,
  Pull never changing `rating.source`, the `rating.source` migration leaving
  every row intact, and every PRD §7 shape. The UI was driven in headless
  Chromium against the real router: before/after screenshots of each
  comparison change against a fake AniList, the bottom nav on every page at
  900 and 1440 px, Back keeping scroll, open rows and the lit tab through a
  comparison, a title's friend chips catching up after the queue fetches a
  friend, and "Rate it now" saving through the stepper.
  Checked against the live AniList API (logged out) on temp DBs: public,
  private, unknown and empty-list users, every score format, lists of 11,905
  and 19,944 entries, add/compare/refresh/remove, two requests per fetch, a
  56-follow sync, mutual only, the queue's pace, a comparison jumping the
  queue, the migration on a database the earlier build wrote, and the
  comparison page with real data (no requests while expanding rows or
  showing whole lists). Findings in
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

- Default branch: **`main`**, on GitHub at `26b4737`: the Rust/Tauri app
  with Friend Comparison (M10). `rebuild`, `tauri-port` and `remove-python`
  were merged in and deleted.
- Current branch: **`feature/friend-compare`**, pushed and not merged yet:
  the fixes from the independent audit of M10, on top of `main`.

| Commit | What |
|---|---|
| `7a2fab6` | Search imports store AniList's popularity (PRD §5) |
| `8598de6` | Tests: a sub-point disagreement; Pull keeps `rating.source` |
| `0010746` | Each Back target keeps its lit tab; comparison pages light Profile |
| `d4b192b` | A login syncs friends once, through its Pull (FR-79) |
| `9d2bd6d` | Friend chips refresh once a friend's list is fetched |
| `8ef4698` | One fetch per friend at a time; others reuse it (FR-81) |
| `90b0a98` | A private list's status is grey, not amber |

The port's own commits, all on `main`:

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
    queue can keep the account rate-limited. Opening a title's detail view
    fetches its community data twice (a second render starts another fetch
    before the first returns), on `main` too.
12. **Friend Comparison, still unverified:** the desktop window by hand
    (Friends section, Compare page, taste-match tooltip, friend chips on a
    detail page, expanding rows, "Rate it now", the bottom nav highlight),
    and WebKit/WebKitGTK rendering of the two-card grid; adding a private
    user *by name* (only by id was possible); a real 429's `Retry-After`; whether AniList lists private users in a
    following list at all.
13. **Mutual only with a real token is unverified:** it reads your own
    following list with your token for `isFollower` (`docs/anilist-api-notes.md`
    #18), tested only against the fake server. If AniList doesn't confirm the
    viewer, it falls back to reading every followers page, which a popular
    account can't afford in one minute.
14. **`rating.source` backfill can misfile old ratings:** a rating made in the
    app before the column existed, with every criterion equal (all 8s), is
    classed as from AniList, so the comparison offers "Rate it now" for it
    instead of showing its criteria until it's rated again (PRD §5).
15. **Pick an anime limits:** AniList pages through 5,000 results at most, so
    with more matches than that, picks come from the 5,000 oldest or newest
    entries (the picker says so). The first pick on a new set of filters
    that matches more than 50 titles costs two to three requests, and up to
    ~5 s with tag filters (probes past the end of the matches are full scans
    on AniList's side); rerolls after that are one request. The prequel
    check (FR-71) can add up to 3 more page requests when pages hold nothing
    you can start, and up to 5 lookups to find a next-up: 11 requests is the
    worst case for one pick, against AniList's 30 a minute.
