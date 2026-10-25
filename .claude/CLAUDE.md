Always refer to `docs/PRD.md` when unsure. It is the product contract; if
anything here or in a chat attachment contradicts it, the PRD wins. If the PRD
itself is ambiguous, ask rather than picking a reading.

## AniList API
- GraphQL endpoint: https://graphql.anilist.co
- Docs/explorer: https://anilist.gitbook.io/anilist-apiv2-docs/
- OAuth docs: https://anilist.gitbook.io/anilist-apiv2-docs/getting-started/authentication
- Use the interactive explorer at https://anilist.co/graphiql to test queries
- Schema notes and confirmed gotchas: `docs/anilist-api-notes.md`

# MyAnimeRank — Project Context

## What this is

A personal, locally-hosted desktop app for ranking anime and manga through
**anchored relative scoring**: each new title is rated by comparing it against
titles already scored, anchored within the same genre and the same media type.
It is also an AniList client: login is required (FR-47), and status/progress/
overall score sync both ways with the user's AniList account.

Stack: Python/FastAPI backend, SQLite, pywebview desktop shell (frameless
window, custom titlebar), single-file vanilla HTML/JS frontend, metadata from
**AniList's GraphQL API only**.

## Repo layout

```text
docs/PRD.md          product contract (authoritative)
main.py              FastAPI routes; also serves frontend/index.html at GET /
database.py          schema, queries, rating math
models.py            Pydantic API models
anilist.py           ALL GraphQL operations, via one _graphql() helper
sync.py              Pull, push, conflict handling, retry queue
oauth_listener.py    one-shot OAuth callback listener on fixed port 8931
cache.py             TTL cache for Discover/Feed only
app.py               desktop shell: uvicorn subprocess on a free port + pywebview
frontend/index.html  entire UI
tests/               pytest
library.db           live user data (gitignored, no other backup)
local_config.py      ANILIST_CLIENT_ID / ANILIST_CLIENT_SECRET (gitignored)
```

## Commands

```powershell
pip install -r requirements.txt
python -m pytest -q
uvicorn main:app --port 8420   # browser dev (matches .claude/launch.json)
python app.py                  # desktop window
```

**Starting the server initializes and may migrate `library.db`.** That file
is real user data. Copy it aside (or point `database.DB_PATH` at a temp file)
before any run that could touch the schema. If a change needs a migration,
describe what happens to existing rows and wait for a yes.

## Why this is a rebuild, not a patch (read this first)

v1 shipped anime-only, sourced from AniList. Manga support was bolted on
later, and partway through, MAL (via Jikan) was added *alongside* AniList.
That produced two ID spaces, two field vocabularies (episodes vs. chapters),
and no single source of truth for "what kind of media is this" — which is
why manga entries started getting mis-synced as anime.

v2 fixes this at the architecture level:

- **One API: AniList only.** No Jikan/MAL calls anywhere in the app. If you
  see one, it's a leftover from v1 and should be deleted, not "fixed."
  All GraphQL stays in `anilist.py`; routes never build query strings.
- **One table.** Anime and manga are rows in a single `media` table,
  distinguished by a required `media_type` column — never parallel code
  paths that can drift apart.
- **`media_type` is set once, at import**, directly from AniList's own
  `MediaType` enum (`ANIME` / `MANGA`). It is **never inferred, guessed,
  defaulted, or edited after the fact.** Any code path that tries to derive
  or change media_type post-import is a bug.
- Anime and manga **never anchor against each other**: anchor queries filter
  to the subject's `media_type`. Manga has no Animation/Audio criteria — this
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
- No weighted criteria — overall is always a simple, unweighted average of
  the active criteria for that media type.
- No social write features: no follow/unfollow, messaging, list comparison,
  or friend features. User profiles are read-only browsing (FR-67).
- No offline/mock mode: the app is unusable without a backend and a login.
- No custom themes, no airing notifications (PRD §10).

If a task seems to require one of these, stop and ask rather than building
toward it.

## Data model essentials

- Anime: 5 criteria — Story, Characters, Animation, Audio, Enjoyment.
- Manga: 4 criteria — Story, Characters, Art, Enjoyment. Art is stored in the
  `rating.animation` column. No Audio step in the manga flow (absent, not
  hidden). The client must send `audio: 0.0` for manga; non-zero is rejected
  with 422 (FR-6, covered by tests). Audio is stored as 0.0 and excluded from
  the manga average.
- Scores: 1.0–10.0 in 0.5 increments (validated in `models.RatingIn`).
- Overall score is **always computed server-side** from `media_type` —
  never trust an overall score sent from the client.
- Duplicate import (same `anilist_id`) updates the existing row; it does
  not create a second entry and never changes `media_type`.

## Coding conventions

- **Minimal working changes. Reuse first.** No speculative abstractions,
  no unrequested dependencies, no "while I'm in here" scope creep. If a
  task looks like it wants a new pattern/library, flag it and ask instead
  of introducing it.
- Don't add all-caps text in the UI. If a design calls for emphasis, use
  weight or size, not case.
- Keep `requirements.txt` to direct imports only — let pip resolve the
  transitive graph. Don't hand-pin transitive dependencies.
- One `_graphql()`-style helper per external API integration point, not
  N copies of the same request boilerplate. If you find yourself copying a
  request/response pattern a second time, extract it first.
- Before deleting anything flagged as dead/unused, do a repo-wide search to
  confirm nothing references it — don't rely on a single file's local view.
- For frontend changes: this runs in a pywebview window, not a browser tab
  — there is no browser chrome, so window controls (minimize/maximize/close)
  are real UI you own, not decorative.
- Git: never run `commit`, `push`, `checkout`, `stash`, `reset`, `rebase`, or
  `clean`. Committing is the user's; give them the commands instead. Never
  delete or move untracked files without asking.

## Definition of done

1. Write the test first, then implement. Test what would actually break the
   app if wrong (media_type immutability, overall computation, anchor
   filtering, sync conflict logic), not every line.
2. `python -m pytest -q` passes; run linters/type checks if configured.
3. `uvicorn main:app` boots with no exception (smoke-test against a temp DB).
4. Affected endpoints return their PRD §7 shapes, shown with a test or curl.
5. Both anime and manga paths exercised when shared code changes.
6. No new dependency without saying so.
7. State what you actually ran. "Should work" is not verification.

## Known defects (raise them, get a decision, fix one at a time)

- Schema allows nullable `rating.animation` and has no `ON DELETE CASCADE`.
  Fixing either is a migration against live data — see the `library.db` rule.
- `anilist._to_media_type` maps anything that isn't `ANIME` to `manga` (a
  silent default), and `POST /media` trusts the client-supplied `media_type`
  instead of taking it from AniList.
- `anilist._graphql()` doesn't handle GraphQL `errors` payloads or 429s.
- The push retry queue is in-memory only; `token_expires_at` is never checked.
- Some UI labels use `text-transform:uppercase`, against the convention above.
- FR-25/FR-28 score-format consent and drift have no frontend wiring: nothing
  calls `POST /auth/score-format/confirm`, and `push_now`'s
  `score_format_drift` result is never surfaced (pushes silently skip).

## Current status

- v2 rebuild feature-complete through M9 per the PRD.
- Done: `_graphql()` consolidation, debounced-search consolidation (one
  `SEARCH_FETCHERS` dispatcher + `_searchResults` map), `requirements.txt`
  trimmed to direct deps, dead OAuth values and no-op window handlers removed,
  wildcard CORS middleware removed (frontend is same-origin), frontend
  mock/offline mode and its localStorage library cache removed (FR-47).

## Working philosophy

Treat every task as a diff to review, not a feature to ship and forget.
Prefer deleting speculative or unused code over documenting why it's there.
When something in the codebase looks unusual, assume it's either (a) a
deliberate architectural decision from the v1→v2 rebuild (check the PRD
before "fixing" it) or (b) leftover cruft from v1 (check for a live
reference before assuming it's needed). Don't assume — check.
