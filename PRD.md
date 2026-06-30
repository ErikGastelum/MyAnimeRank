# MyAnimeRank — PRD

**Product Requirements Document**

**MyAnimeRank — A Personal Anime Ranking System, AniList Client & Taste-Comparison Tool**

*Local-first web app · Version 3.0 · June 2026*

---

## 1. Overview

MyAnimeRank is a personal, locally-hosted web application built around **anchored relative scoring** across five criteria, with a **two-way AniList client** layered on top. v3 keeps everything from v2 and adds a social-comparison layer that is read-only against *other people's* public AniList data: you can **compare your rankings against specific friends'** lists, and run a playful **LARP meter** that scores how much your (or a friend's) list reflects genuine, earned taste versus posturing.

The ranking engine is unchanged: instead of isolated 1–10 ratings, you score one anime per criterion and rate each new title by judging it against anime you've already scored, anchored within the same genre. The AniList client is unchanged: after OAuth2 login, the app browses, searches, manages lists, updates status/progress, and reads/writes scores with two-way sync.

What's new in v3 is **outward comparison**. v2 made MyAnimeRank a full client for *your* account; v3 lets you point it at friends' public lists to see where your tastes overlap and diverge, and adds a self-aware "how much of a poser am I" metric computed entirely from list data you already sync. Comparison reads are public-data-only and never write to anyone else's account.

The app still runs entirely on your machine via a local server (`http://localhost:6969`) with a Python/FastAPI backend, a local SQLite database, and a pywebview desktop shell. Friend lists and community data are cached locally like all other AniList reads.

---

## 2. Goals & Non-Goals

### Goals (carried from v2)

- Rate anime across **five criteria** (Story, Characters, Animation, Audio, Enjoyment) on a **1–10 scale, one decimal**.
- Drive new ratings through **genre-based comparison** against already-ranked anime.
- Compute overall as the **simple average** of the five.
- Authenticate with AniList via **OAuth2** and operate as a **full client** (browse, search, lists, status, progress, read/write scores).
- **Two-way sync** between local DB and AniList; overall maps to AniList's single score field.
- Work **locally and offline** for everything that doesn't need the network; queue changes to sync on reconnect.
- Persist all data locally so it survives restarts.

### New Goals (v3)

- **Compare your rankings against specific friends** by their AniList username: shared titles, where you score harsher/kinder, taste-overlap summary, and biggest disagreements.
- **Normalize across scoring systems** so a friend on the 100-point scale and you on the 10-decimal scale are compared apples-to-apples on your internal 1–10.
- **LARP meter:** a 0–100 score (with a transparent breakdown) measuring how much a list reflects earned taste vs posturing, runnable on your own list and on any friend's public list.
- Cache friend/comparison data locally for offline review.

### Non-Goals (v3)

- **No writes to other accounts** — friend data is strictly read-only; the app never mutates anyone's list but your own.
- No multi-user auth — still one authenticated AniList account at a time (your own); friends are referenced by public username, not logged in.
- No social graph management inside the app (no follow/unfollow, no DMs, no activity feed) — even though AniList has these. v3 reads public lists; it is not a social client.
- No cloud hosting of the app; remains a localhost tool.
- No weighted criteria; averaging stays unweighted.
- No syncing the five individual criteria to AniList — only the overall syncs (AniList has no field for the five).
- The LARP meter is **for-fun and self-directed**; it is not a judgment delivered *to* friends, has no leaderboard, and is never auto-shared.

---

## 3. Target User

A single user (you) — an anime fan with an AniList account who wants a private ranking tool that doubles as their primary AniList client, and who also wants to see how their taste stacks up against friends. Technical enough to run a local server and complete a one-time OAuth login, but wants a clean, low-friction UI.

---

## 4. Core Concepts

### 4.1 Anchored Relative Scoring (unchanged)

Each criterion is scored independently. When adding or re-rating an anime, the app shows already-scored anime sharing at least one genre, sorted by that criterion's score, as anchors. The user places the new title relative to those anchors and assigns 1.0–10.0. Overall is the simple average; the master ranking re-sorts automatically.

### 4.2 AniList as a Two-Way Sync Target (unchanged from v2)

AniList is the canonical cloud copy of *your* list; the local SQLite DB is a local-first mirror. Local `overall` pushes to AniList's score field on change. Inbound AniList scores that differ from local `overall` are stored as an **override** and flagged for **reconciliation** rather than silently overwriting the five criteria. Conflict policy is **last-write-wins with AniList as tiebreaker**, logged in a sync log.

### 4.3 Friend Comparison (new)

A friend is referenced by **AniList username** (no login on their part). The app fetches the friend's **public** `MediaListCollection` and scores, matches entries against your library on `mediaId`, and computes:

- **Overlap:** how many titles you've both rated.
- **Per-title delta:** your overall vs their (normalized) score, per shared title.
- **Aggregate skew:** whether you score systematically higher or lower than them.
- **Agreement & disagreement leaders:** titles where you're closest and furthest apart.

**Score-system normalization.** AniList users can be on the 100-point, 10-point decimal, 5-star, 3-smiley, or 10-point integer system. Every friend score is normalized to the internal **1–10.0** scale before comparison, and the comparison view notes which system the friend uses so deltas are interpreted correctly. If a friend's list is **private**, the app reports that clearly and compares nothing — it does not attempt to bypass privacy.

### 4.4 LARP Meter (new)

A playful, transparent **0–100** score estimating how much a list reflects *earned, discriminating taste* versus *posturing*. Lower = more authentic ("you've actually watched the things you rate highly"); higher = more LARP ("you rate everything a 9 and your favorites are half-finished"). It is computed entirely from list data already synced, runs on **your** list or any **friend's public** list, and always shows its component breakdown so the number is never a black box.

**Components (each contributes to the composite):**

| Signal | What it catches | Direction |
| --- | --- | --- |
| **Completion integrity** | High scores / favorites sitting on low episode progress (scored a 9 but watched 3/24). Dropped-with-high-score, or favorites never completed. | More incomplete high-scores → higher LARP |
| **Score inflation** | Average score sitting far above the community average across the list; little spread. | Everything's a 9 → higher LARP |
| **Discrimination / spread** | Standard deviation of scores. A flat list that never goes low isn't really ranking. | Low spread → higher LARP |
| **Breadth vs clustering** | Genre/era diversity. Rating only one safe lane vs a real range. | Narrow clustering → higher LARP |
| **Backlog honesty** | Planning-list size relative to Completed; a 400-deep Planning list that never converts. | Huge unconverted backlog → mild LARP |

The composite is a weighted blend of these normalized components. Weights are fixed and documented in-app (not user-tunable in v3, to keep the number comparable across people). The meter is **self-directed and for-fun**: no leaderboard, no auto-share, and when run on a friend it's framed as "what their public data suggests," not a verdict.

---

## 5. Functional Requirements

### 5.1 Authentication & Account (unchanged from v2)

| ID | Requirement |
| --- | --- |
| FR-1 | OAuth2 login; long-lived access token (~1yr) stored locally. No refresh tokens — prompt re-auth on expiry. |
| FR-2 | Sign out clears local tokens, retains local SQLite data. |
| FR-3 | Degrades gracefully unauthenticated: ranking and local browsing work; sync, write-back, **and friend comparison** are disabled (friend reads still require the network even though they don't require *your* auth — see FR-30). |

### 5.2 Anime Search & Import (unchanged from v2)

| ID | Requirement |
| --- | --- |
| FR-4 | Search anime by title via AniList GraphQL. |
| FR-5 | Selecting a result imports metadata: title, cover, genres, year, season, episodes, synopsis. |
| FR-6 | If AniList is unavailable, add manually with minimal fields; queue for sync. |
| FR-7 | AniList responses cached locally for fewer calls and offline viewing. |

### 5.3 AniList Client — Browsing & List Management (unchanged from v2)

| ID | Requirement |
| --- | --- |
| FR-8 | View full collection grouped by status (Watching, Completed, Planning, Paused, Dropped, Rewatching). |
| FR-9 | Change list status from the app; syncs to AniList. |
| FR-10 | Update episode progress; syncs to AniList. |
| FR-11 | Browse/search the general catalog and add titles to your list. |
| FR-12 | "Sync now" triggers full reconcile; also syncs on launch and after local changes. |

### 5.4 Rating Workflow (unchanged from v2)

| ID | Requirement |
| --- | --- |
| FR-13 | Rate five criteria: Story, Characters, Animation, Audio, Enjoyment. |
| FR-14 | Each criterion 1.0–10.0 in 0.1 increments. |
| FR-15 | Genre-matched anchors sorted by that criterion's score while rating. |
| FR-16 | Overall = simple average, one decimal. |
| FR-17 | Overall change pushes to AniList's score field. |
| FR-18 | Re-rate any time; rankings recompute and overall re-syncs. |

### 5.5 Sync & Reconciliation (unchanged from v2)

| ID | Requirement |
| --- | --- |
| FR-19 | Two-way sync: local pushes to AniList, AniList pulls into local. |
| FR-20 | Conflicts last-write-wins; AniList wins on ambiguous/equal timestamps. |
| FR-21 | Inbound score differing from local overall stored as override + flagged, not overwritten. |
| FR-22 | Reconciliation view: keep local or accept AniList per entry. |
| FR-23 | Sync log records every synced field change with source and timestamp. |
| FR-24 | Offline local changes queued and applied on next successful sync. |

### 5.6 Friend Comparison (new)

| ID | Requirement |
| --- | --- |
| FR-30 | User can add a friend by **AniList username**; the app fetches their **public** `MediaListCollection` and scoring system. Private lists are reported as un-comparable, with no bypass attempt. |
| FR-31 | The app normalizes the friend's scores from their AniList scoring system to the internal 1–10.0 scale before any comparison. |
| FR-32 | A comparison view shows: shared-title count, per-title delta (your overall vs their normalized score), aggregate skew (do you score higher/lower overall), and the biggest agreement/disagreement titles. |
| FR-33 | Friends can be saved for re-comparison; their fetched data is cached locally and refreshed on demand or on a comparison run. |
| FR-34 | Friend data is strictly **read-only** — the app never issues mutations against a friend's account. |
| FR-35 | The user can remove a saved friend and purge that friend's cached data locally. |

### 5.7 LARP Meter (new)

| ID | Requirement |
| --- | --- |
| FR-36 | The app computes a 0–100 LARP score for **the authenticated user's own list** from completion integrity, score inflation, score spread, breadth/clustering, and backlog honesty. |
| FR-37 | The LARP score always displays its **component breakdown** (each signal's contribution), never just the composite. |
| FR-38 | The user can run the LARP meter on **any saved friend's public list** using the same components; the result is framed as "based on public data," not a verdict, and is never auto-shared or ranked on a leaderboard. |
| FR-39 | LARP components requiring community comparison (score inflation) use AniList community averages, cached locally; if unavailable, that component is omitted and the breakdown notes it. |
| FR-40 | LARP weights are **fixed and documented in-app** in v3 (not user-tunable) so the number is comparable across people. |

### 5.8 Rankings & Views (unchanged from v2)

| ID | Requirement |
| --- | --- |
| FR-25 | Master list ranked by overall (rank, cover, title, scores). |
| FR-26 | Filter by genre, year/era, season. |
| FR-27 | Sort by any single criterion in addition to overall. |
| FR-28 | Detail view: five scores, AniList status/progress, metadata, **and friend deltas for that title when a friend is loaded**. |
| FR-29 | Delete from local rankings; an option controls whether this also removes it from AniList. |

### 5.9 Personal Insights (nice-to-have, extended)

- Taste stats: average score per genre, highest-rated criterion, score distribution.
- Where a criterion diverges most from a title's community average.
- Reconciliation history (how often local and AniList disagreed).
- **Taste-twin / taste-opposite:** across saved friends, who you agree with most and least.
- **LARP history:** how your own LARP score trends as you complete backlog and re-rate.

---

## 6. Data Model

Local SQLite. v3 adds tables for friends, their fetched lists, and LARP results. v2 tables are unchanged except where noted.

### anime (unchanged)

| Field | Type | Notes |
| --- | --- | --- |
| id | INTEGER PK | Local id |
| anilist_id | INTEGER | AniList media id (unique link) |
| title | TEXT | Display title |
| cover_url | TEXT | Cover image URL (cached) |
| genres | TEXT (JSON) | List of genre strings |
| year | INTEGER | Release year |
| season | TEXT | e.g. Winter 2024 |
| episodes | INTEGER | Episode count |
| community_score | REAL | AniList community average (cached, for inflation/LARP) — *new* |

### list_entry (unchanged from v2)

| Field | Type | Notes |
| --- | --- | --- |
| anime_id | INTEGER FK | References anime.id |
| status | TEXT | Watching / Completed / Planning / Paused / Dropped / Rewatching |
| progress | INTEGER | Episodes watched |
| anilist_entry_id | INTEGER | AniList list-entry id (for mutations) |
| updated_at | TEXT | Last local change |

### rating (unchanged from v2)

| Field | Type | Notes |
| --- | --- | --- |
| anime_id | INTEGER FK | References anime.id |
| story / characters / animation / audio / enjoyment | REAL | 1.0–10.0 each |
| overall | REAL | Computed average |
| anilist_score_override | REAL | Inbound score awaiting reconciliation (nullable) |
| needs_reconcile | INTEGER | 0/1 flag |
| updated_at | TEXT | Timestamp |

### friend (new)

| Field | Type | Notes |
| --- | --- | --- |
| id | INTEGER PK | Local id |
| anilist_username | TEXT | Friend's AniList handle (unique) |
| anilist_user_id | INTEGER | Resolved AniList user id |
| score_system | TEXT | Their scoring system (POINT_100, POINT_10_DECIMAL, POINT_5, etc.) |
| is_private | INTEGER | 0/1 — list not publicly readable |
| last_fetched | TEXT | When their list was last pulled |

### friend_entry (new)

| Field | Type | Notes |
| --- | --- | --- |
| friend_id | INTEGER FK | References friend.id |
| anilist_id | INTEGER | AniList media id (matches anime.anilist_id for overlap) |
| raw_score | REAL | Score in the friend's native system |
| normalized_score | REAL | Normalized to internal 1.0–10.0 |
| status | TEXT | Their list status for the title |
| progress | INTEGER | Their episode progress |

### larp_result (new)

| Field | Type | Notes |
| --- | --- | --- |
| id | INTEGER PK | Result id |
| subject_type | TEXT | self / friend |
| friend_id | INTEGER FK | Null when subject is self |
| composite | REAL | 0–100 LARP score |
| components | TEXT (JSON) | Per-signal contributions (completion, inflation, spread, breadth, backlog) |
| computed_at | TEXT | Timestamp |

### sync_log (unchanged from v2)

| Field | Type | Notes |
| --- | --- | --- |
| id | INTEGER PK | Log id |
| anime_id | INTEGER FK | References anime.id |
| field | TEXT | Field that changed (score, status, progress) |
| source | TEXT | local / anilist |
| old_value / new_value | TEXT | Prior / applied value |
| timestamp | TEXT | When applied |

### auth (unchanged from v2)

| Field | Type | Notes |
| --- | --- | --- |
| access_token | TEXT | AniList OAuth token (~1yr) |
| expires_at | TEXT | Expiry; re-auth on reach (no refresh token) |
| anilist_user_id | INTEGER | Authenticated account |

---

## 7. Technical Architecture

| Layer | Choice | Notes |
| --- | --- | --- |
| Backend | Python + FastAPI | API routes, sync engine, AniList proxy, comparison + LARP compute |
| Database | SQLite | Single local file; local-first mirror + friend/LARP cache |
| Frontend | HTML/CSS/JS (React, single-file) | Served by FastAPI |
| Desktop shell | pywebview | Frameless native-style window (`app.py`); `main.py` runs headless in-browser |
| Anime API | AniList GraphQL | Search, metadata, your lists/mutations, **friends' public lists**, community scores |
| Auth | AniList OAuth2 | One-time login; long-lived token (~1yr); re-auth on expiry; no scopes, no refresh tokens |
| Sync engine | FastAPI background task | Two-way reconcile; last-write-wins, AniList tiebreaker |
| Hosting | Uvicorn on localhost | Fixed at `http://localhost:6969` (registered OAuth redirect depends on this port) |

### Code Layout (matches existing repo)

```
MyAnimeRank/
├── app.py            # Desktop entry point (pywebview wrapper)
├── main.py           # FastAPI app and all API routes
├── database.py       # SQLite schema and connection helpers
├── models.py         # Pydantic request/response models
├── anilist.py        # AniList GraphQL client (search, lists, mutations, friends, community)
├── compare.py        # Friend comparison + score normalization — new
├── larp.py           # LARP meter component computation — new
├── MyAnimeRank.html  # Single-file frontend (React, served by FastAPI)
├── requirements.txt
└── myanimrank.db     # Created on first run (gitignored)
```

### Key Endpoints

Existing (v2):

- **GET /auth/login**, **GET /anilist/callback**, **POST /auth/logout**
- **GET /search?q=**, **POST /anime**
- **PUT /anime/{id}/rating**, **PUT /anime/{id}/list**
- **GET /rankings**, **GET /anime/{id}/anchors?criterion=story**
- **POST /sync**, **GET /sync/log**, **GET /reconcile**, **POST /reconcile/{id}**

New (v3):

- **POST /friends** — add a friend by AniList username; resolve user id, scoring system, privacy.
- **GET /friends** — list saved friends with cache freshness.
- **POST /friends/{id}/refresh** — re-fetch the friend's public list.
- **DELETE /friends/{id}** — remove friend and purge cached data.
- **GET /compare/{friend_id}** — overlap, per-title deltas, aggregate skew, agreement/disagreement leaders.
- **GET /larp?subject=self** — compute own LARP score with breakdown.
- **GET /larp?subject=friend&friend_id={id}** — compute a friend's LARP score with breakdown.

### AniList Integration Notes

- Reads use GraphQL for media search/details and `MediaListCollection`. For **your own single entry**, query with `mediaId` **and** `userId` together. For a **friend**, query `MediaListCollection(userName: $name, type: ANIME)` — public lists return without auth; private lists return an error or empty, which the app surfaces as "not comparable."
- A friend's scoring system comes from their `User.mediaListOptions.scoreFormat`; the app normalizes from it to internal 1–10.0.
- Community averages come from `Media.averageScore` (0–100), cached on the `anime` row for inflation/LARP.
- Writes (yours only) use `SaveMediaListEntry` (status/progress/score) and `DeleteMediaListEntry`. Friend endpoints issue **no mutations**.

### AniList API Constraints (verified)

- **OAuth2 has no scopes**; token grants near-full access to *your* account. Friend reads use public queries and don't require any extra grant.
- **Long-lived (~1yr) tokens, no refresh tokens** — store token + expiry, prompt re-login on expiry, never silent-refresh.
- **All mutations require auth** — write-back is gated on a valid token; friend reads are read-only public queries.
- **Rate limits enforced** — batch friend fetches and community-score lookups; back off on `429`. Comparing several friends should reuse cached community scores rather than re-querying per title.
- The AniList website runs on this same API, so reading public friend lists and community scores is achievable through public endpoints alone.

**Registered application & OAuth flow (unchanged from v2):**

- Registered as **MyAnimeRank** (developer: AniList user 5155273) — provides Client ID + Secret.
- **Authorization Code grant.** Redirect URL `http://localhost:6969/anilist/callback`; app must run on port **6969**.
- `/auth/login` → AniList authorize → `/anilist/callback?code=...` → backend exchanges code for token → token + expiry stored.
- Client Secret stays on the FastAPI backend, never exposed to the frontend.

**Reference endpoints & tooling:**

- GraphQL endpoint: `https://graphql.anilist.co`
- API docs / explorer: `https://anilist.gitbook.io/anilist-apiv2-docs/`
- OAuth docs: `https://anilist.gitbook.io/anilist-apiv2-docs/getting-started/authentication`
- Interactive query tester (GraphiQL): `https://anilist.co/graphiql`

---

## 8. Example User Flows

### 8.1 Rating with score push (unchanged)

Import JJK S1 → on Story, anchors show Naruto (8.0), Demon Slayer (7.8); assign 7.9 → rate the other four → overall computes → JJK slots into the ranking and the overall pushes to AniList.

### 8.2 List management from the client (unchanged)

Open Watching, mark an episode watched (7 → 8) → saves locally and syncs → anilist.co shows progress 8.

### 8.3 Compare against a friend (new)

- User adds friend `kenji_watches` by username. The app resolves their user id, sees they're on the **100-point** system, and confirms their list is public.
- The app fetches their list, normalizes each score to 1–10.0, and matches on `mediaId` against the user's library.
- The comparison view shows: 84 shared titles, the user scores **0.4 lower on average**, biggest disagreement is *Sword Art Online* (user 5.5, friend normalized 8.7), strongest agreement is *Frieren* (both ~9.4).
- Opening *SAO*'s detail view now shows the friend delta inline.

### 8.4 LARP meter on yourself (new)

- User runs the LARP meter. Composite comes back **38/100** ("more authentic than not").
- Breakdown: completion integrity good (favorites are completed), score spread healthy, but a **mild backlog-honesty hit** (Planning list 210 deep) and slight inflation vs community. The breakdown explains each contribution so the number isn't a black box.

### 8.5 LARP meter on a friend (new)

- User runs the meter on `kenji_watches`. Result **71/100**, driven mostly by completion integrity (several 9s and 10s on shows at <25% progress) and very low score spread (almost everything 8–10).
- The view frames it as "based on public data" — for fun, not a verdict, and nothing is shared back to the friend.

---

## 9. Suggested Build Milestones

| Phase | Deliverable |
| --- | --- |
| M1–M8 | *(All v2 milestones: FastAPI skeleton + schema, OAuth, search/import, rating form, list management, two-way sync, rankings/filters, reconciliation/offline/persistence.)* |
| M9 | **Friend model + fetch:** add friend by username, resolve id/scoring system/privacy, pull public list, cache locally (`friend`, `friend_entry`). |
| M10 | **Score normalization:** map every AniList scoring system to internal 1–10.0; unit-test each system. |
| M11 | **Comparison view:** overlap, per-title deltas, aggregate skew, agreement/disagreement leaders; friend delta inline in detail view. |
| M12 | **Community-score caching:** pull and store `Media.averageScore` for inflation/LARP. |
| M13 | **LARP meter:** implement the five components + composite, with always-visible breakdown; run on self. |
| M14 | **LARP on friends** + taste-twin/opposite insights and LARP history. |
| M15 (optional) | Insights dashboard polish; export comparisons. |

---

## 10. Open Questions

Carried from v2:

- Require the 10-point decimal scoring system, or normalize from whatever the user has set? *(v3 leans toward normalizing, since friend comparison forces normalization logic anyway.)*
- When deleting locally, remove from AniList by default or opt-in each time?
- How often should automatic sync run?
- Should reconciliation let the user back-fill the five criteria from an accepted AniList number?
- Sync notes/rewatch counts, or keep to status/progress/score?
- Export rankings (CSV/JSON) for backup?

New for v3:

- **Friend caching freshness:** auto-refresh a friend's list on every comparison, or only on explicit refresh? (Rate limits favor explicit/cached.)
- **LARP weighting:** fixed weights in v3 keep scores comparable — but do you eventually want a "show me the math / tweak weights" advanced mode, accepting that tuned scores aren't comparable across people?
- **Completion-integrity threshold:** what progress % counts as "scored without really watching" — <25%? <50%? Should it differ for currently-airing shows?
- **Backlog honesty:** should a huge Planning list count against LARP at all, or is hoarding a backlog orthogonal to posturing about taste?
- **Friend privacy UX:** when a friend's list is private, just report it — or offer to compare only on the handful of titles they've made public via activity? (Probably out of scope; flag for later.)
- **Multiple friends at once:** v3 compares one friend at a time. Do you want a combined "you vs your circle" aggregate view, or keep it pairwise?
