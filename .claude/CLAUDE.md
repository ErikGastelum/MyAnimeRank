Always refer to `docs/PRD.md` when unsure. It is the product contract; if
anything here or in a chat attachment contradicts it, the PRD wins. If the PRD
itself is ambiguous, ask rather than picking a reading.

## AniList API
- GraphQL endpoint: https://graphql.anilist.co
- Docs: https://docs.anilist.co (implicit-grant login: https://docs.anilist.co/guide/auth/implicit)
- Use the interactive explorer at https://anilist.co/graphiql to test queries
- Schema notes and confirmed gotchas: `docs/anilist-api-notes.md`

# MyAnimeRank — Project Context

## What this is

A personal, local desktop app for ranking anime and manga through
**anchored relative scoring**: each new title is rated by comparing it against
titles already scored, anchored within the same genre and the same media type.
It is also an AniList client: login is required (FR-47), and status/progress/
overall score sync both ways with the user's AniList account.

Stack: **Tauri 2 + Rust** desktop app (`src-tauri/`). One native process: the
API router (axum) runs in-process and the page reaches it through a single
Tauri command, so there is no local web server and no open port, except the
one-shot OAuth callback listener on 8931. SQLite via rusqlite. The UI is the
single-file vanilla HTML/JS `frontend/index.html` in a frameless window with
a custom titlebar. Metadata comes from **AniList's GraphQL API only**. Runs on
Windows (WebView2), macOS (WebKit) and Linux (WebKitGTK).

The Rust app replaced an earlier Python implementation (FastAPI + uvicorn +
pywebview), now deleted; it's in git history before the `remove-python`
branch. Existing `library.db` files it wrote open unchanged.

## Repo layout

```text
docs/PRD.md               product contract (authoritative)
src-tauri/
  src/main.rs             entry point; `--serve <port>` = browser dev mode
  src/lib.rs              app state, Tauri setup, `api` + `start_login` commands
  src/api.rs              every API route (same paths/JSON as PRD §7), auth gate
  src/db.rs               schema, queries, rating math, sync helpers
  src/anilist.rs          ALL GraphQL operations, via one graphql() helper
  src/sync.rs             Pull, push, conflict handling, retry queue
  src/oauth.rs            one-shot OAuth callback listener on fixed port 8931
  src/cache.rs            TTL cache for Discover/Feed only
  src/config.rs           AniList client ID (built in; optional overrides)
  tests/backend.rs        integration tests: in-memory DB, fake AniList server
  tauri.conf.json, capabilities/, icons/
frontend/index.html       entire UI (works in Tauri and in a browser via --serve)
README.md                 setup and run, per platform
PROJECT_STATUS.md         current state and open items
.claude/hooks/*.py        Claude Code hooks (not app code)
```

Data and config live outside the repo, in the OS app-data folder for
`com.erikgastelum.myanimerank` (Windows: `%APPDATA%\com.erikgastelum.myanimerank\`,
macOS: `~/Library/Application Support/...`, Linux: `~/.local/share/...` for the
database, `~/.config/...` for config). The database is `library.db` there,
unless `MYANIMERANK_DB` points elsewhere.

## Commands

Run from `src-tauri/`:

```powershell
cargo test                     # all tests; never touch the network or a real DB
cargo clippy --all-targets     # lint; keep it warning-free
cargo run                      # desktop window (debug)
cargo run --release            # optimized build
cargo run -- --serve 8420      # browser dev at http://127.0.0.1:8420
```

**Never run the app against the real `library.db`.** It is the user's real
data. Point `MYANIMERANK_DB` at a temp copy first (the app creates the folder
if it's missing). Opening a database may migrate its schema. If a change needs
a migration, describe what happens to existing rows and wait for a yes.

Edits made in the app sync to the user's **real AniList account** (status,
progress, score, import, delete). A temp DB protects `library.db`, not
AniList. When testing by hand, use a throwaway title.

## Login

AniList implicit grant: only the client ID is needed, and it is public by
design, so it's built in (`config::BUILTIN_CLIENT_ID`). **There is no client
secret anywhere; never add one.** Anyone who builds the repo can log in with
their own account, no setup. Login opens the system browser; AniList
redirects to `http://localhost:8931/callback` with the token in the URL
fragment, the callback page hands it to the listener, the token is validated
with a Viewer query, then saved. The AniList client's redirect URI must stay
exactly `http://localhost:8931/callback`. Overrides for a different client:
the `ANILIST_CLIENT_ID` env var, `local_config.toml` in the app config folder,
or `local_config.py` in the current folder or its parent.

## Why this is a rebuild, not a patch (read this first)

v1 shipped anime-only, sourced from AniList. Manga support was bolted on
later, and partway through, MAL (via Jikan) was added *alongside* AniList.
That produced two ID spaces, two field vocabularies (episodes vs. chapters),
and no single source of truth for "what kind of media is this", which is
why manga entries started getting mis-synced as anime.

v2 fixes this at the architecture level, and the Rust port keeps every rule:

- **One API: AniList only.** No Jikan/MAL calls anywhere in the app. If you
  see one, it's a leftover from v1 and should be deleted, not "fixed."
  All GraphQL stays in `anilist.rs`; routes never build query strings.
- **One table.** Anime and manga are rows in a single `media` table,
  distinguished by a required `media_type` column, never parallel code
  paths that can drift apart.
- **`media_type` is set once, at import**, directly from AniList's own
  `MediaType` enum (`ANIME` / `MANGA`). It is **never inferred, guessed,
  defaulted, or edited after the fact.** Any code path that tries to derive
  or change media_type post-import is a bug.
- Anime and manga **never anchor against each other**: anchor queries filter
  to the subject's `media_type`. Manga has no Animation/Audio criteria; this
  absence is intentional, not a bug to fix.

## Scope

In scope per the PRD (don't remove these as "unspecified"): AniList OAuth
login with a mandatory login gate (FR-47), Pull + automatic push sync with
conflict resolution (4.6), Discover, Feed, Characters/Staff/Favorites,
Profile, Insights, and M9 Studios & Users search/profiles (4.11).

Non-goals (do not implement without an explicit go-ahead):

- No MAL/Jikan integration of any kind.
- No user accounts of our own, cloud hosting, or multi-user support. Single
  local user, one AniList account connected at a time.
- No weighted criteria: overall is always a simple, unweighted average of
  the active criteria for that media type.
- No social write features: no follow/unfollow, messaging, list comparison,
  or friend features. User profiles are read-only browsing (FR-67).
- No offline/mock mode: the app is unusable without a login.
- No custom themes, no airing notifications (PRD §10).

If a task seems to require one of these, stop and ask rather than building
toward it.

## Data model essentials

- Anime: 5 criteria: Story, Characters, Animation, Audio, Enjoyment.
- Manga: 4 criteria: Story, Characters, Art, Enjoyment. Art is stored in the
  `rating.animation` column. No Audio step in the manga flow (absent, not
  hidden). The client must send `audio: 0.0` for manga; non-zero is rejected
  with 422 (FR-6, covered by tests). Audio is stored as 0.0 and excluded from
  the manga average.
- Scores: 1.0–10.0 in 0.5 increments (validated in `api.rs`).
- Overall score is **always computed server-side** from `media_type`, never
  trusted from the client. It rounds half-to-even to one decimal, as the
  old Python app did (manga 7.25 → 7.2).
- Duplicate import (same `anilist_id`) updates the existing row; it does
  not create a second entry and never changes `media_type`.
- Timestamps are written in Python's `isoformat()` shape
  (`2026-01-01T00:00:00.123456+00:00`) and compared as parsed datetimes, so
  rows the old Python app wrote still compare correctly.

## Coding conventions

- **Minimal working changes. Reuse first.** No speculative abstractions,
  no unrequested dependencies, no "while I'm in here" scope creep. If a
  task looks like it wants a new pattern/crate, flag it and ask instead
  of introducing it.
- Don't add all-caps text in the UI. If a design calls for emphasis, use
  weight or size, not case.
- Keep Cargo dependencies to what the code directly uses; prefer
  `default-features = false` and turn on only the features needed.
- One `graphql()`-style helper per external API integration point, not
  N copies of the same request boilerplate.
- Before deleting anything flagged as dead/unused, do a repo-wide search to
  confirm nothing references it.
- Frontend: this runs in a frameless desktop window, not a browser tab.
  Window controls (minimize/maximize/close) and the drag region
  (`data-tauri-drag-region`) are real UI you own. Don't use `alert()`; it
  does nothing on macOS. Show errors in the page.
- Run shell commands from the repo root, or in a `( cd src-tauri && ... )`
  subshell, so the project hooks keep working.
- Git: never run `commit`, `push`, `checkout`, `stash`, `reset`, `rebase`, or
  `clean` unless the user explicitly asks in that conversation. Committing is
  the user's; give them the commands instead. Never delete or move untracked
  files without asking. The default branch is `main`.

## Definition of done

1. Write the test first, then implement. Test what would actually break the
   app if wrong (media_type immutability, overall computation, anchor
   filtering, sync conflict logic, what gets sent to AniList), not every line.
2. `cargo test` and `cargo clippy --all-targets` pass with no warnings.
3. The app starts with no error against a temp DB (`MYANIMERANK_DB`).
4. Affected endpoints return their PRD §7 shapes, shown with a test.
5. Both anime and manga paths exercised when shared code changes.
6. No new dependency without saying so.
7. State what you actually ran. "Should work" is not verification.

## Known defects (raise them, get a decision, fix one at a time)

- Schema allows nullable `rating.animation` and has no `ON DELETE CASCADE`.
  Fixing either is a migration against live data; see the `library.db` rule.
  (The Rust `/insights` treats a NULL criterion as 0 where Python crashed.)
- The push retry queue is in-memory only; `token_expires_at` is never checked.
- Some UI labels use `text-transform:uppercase`, against the convention above.
- FR-25/FR-28 score-format consent and drift have no frontend wiring: nothing
  calls `POST /auth/score-format/confirm`, and a push's `score_format_drift`
  result is never surfaced (pushes silently skip on non-decimal accounts).
- The detail-page hero's per-title colour tint is invalid CSS (a gradient
  nested in a gradient), so browsers drop it. Fixing it changes the look; ask.
- Builds aren't code-signed, so Windows SmartScreen and macOS Gatekeeper warn
  on first launch of a downloaded build.

## Current status

- The Rust/Tauri app is the app, on `main`. Full picture and open items:
  `PROJECT_STATUS.md`.

## Working philosophy

Treat every task as a diff to review, not a feature to ship and forget.
Prefer deleting speculative or unused code over documenting why it's there.
When something in the codebase looks unusual, assume it's either (a) a
deliberate architectural decision from the v1→v2 rebuild or the Rust port
(check the PRD and this file before "fixing" it) or (b) leftover cruft
(check for a live reference before assuming it's needed). Don't assume, check.
