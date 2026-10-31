*MyAnimeRank — PRD v2*

**Product Requirements Document**

**MyAnimeRank — A Personal Comparative Anime & Manga Ranking System**

*Local-first desktop app  •  Version 2.0 (Rebuild)  •  June 2026*

# 0. Why This Is a Rebuild, Not a Patch

Version 1 shipped anime-only, sourced from AniList for search/metadata. Manga support was added later as a series of incremental patches on top of that v1 code, and partway through, MAL (via the Jikan API) was introduced alongside AniList for import/export. That created two different ID spaces, two different field vocabularies (episodes vs. chapters, Watching vs. Reading), and two different type systems describing the same library — which is why manga entries started getting synced as anime: nothing in the data model forced "what kind of media is this" to be a single, unambiguous fact tracked from the moment an item enters the database.

This version fixes that at the architecture level instead of patching symptoms:

- **One API**: AniList only. No Jikan/MAL calls anywhere in the app.
- **One table**: anime and manga are both rows in a single `media` table, distinguished by a required `media_type` column — not parallel code paths that can drift apart.
- **`media_type` is set once**, at import, directly from AniList's own `MediaType` enum (`ANIME` / `MANGA`) — never inferred, guessed, or defaulted later.

# 1. Overview

MyAnimeRank is a personal, locally-hosted desktop app for ranking anime and manga through anchored relative scoring. Instead of isolated 1–10 ratings, you score one title per criterion, then rate each new title by judging it against titles you've already scored — anchored within the same genre and the same media type.

Example: if Naruto's Story is anchored at 8.0, adding Jujutsu Kaisen surfaces Naruto and other action anime you've rated so you can decide whether JJK's story is better or worse, and by how much. Manga anchors only against other manga — a manga is never compared against an anime's Animation/Audio criteria, because manga doesn't have those criteria.

The app runs entirely on your machine: a Rust backend running in-process inside a Tauri 2 desktop shell, a local SQLite database, and metadata pulled live from AniList's public GraphQL API. An active AniList login is required to use the app at all — see 4.6 — so the local library is always a mirror of a real AniList account, never a standalone dataset.

# 2. Goals & Non-Goals

## Goals

- Support both anime and manga as equal, first-class entries from a single AniList source.
- Rate anime on five criteria: Story, Characters, Animation, Audio, Enjoyment.
- Rate manga on four criteria: Story, Characters, Art, Enjoyment (no Audio).
- Score **1.0–10.0 in 0.5 increments**; overall = simple average of the active criteria for that media type.
- Drive new ratings through genre-based, same-media-type comparison against already-ranked titles.
- Autofill metadata (title, cover, genres, year, episodes/chapters) from AniList.
- Persist a local SQLite mirror of the connected AniList account so the app is fast and responsive, with restart-safe storage.
- Log in via AniList OAuth and keep status/progress/score in sync with your real AniList.co account (see 4.6).
- Browse Discover shelves, a Feed of your own/following/global activity, and Character/Staff pages with live favoriting (see 4.7–4.9).
- Look up studios and other AniList users' public profiles from catalog Search (see 4.11).
- Compare your list against the public AniList lists of the people you follow (or anyone you add by username): taste match, score skew, disagreements, and what each of you has completed that the other hasn't (see 4.12).

## Non-Goals (v2 rebuild)

- No MAL/Jikan integration of any kind — removed entirely to eliminate the dual-source conflict.
- No user accounts, cloud hosting, or multi-user support — single local user, one AniList account connected at a time.
- No weighted criteria — averaging is unweighted.

# 3. Core Concept: Anchored Relative Scoring (Media-Type Aware)

- User selects a title to rate via AniList search (Anime tab or Manga tab).
- For each active criterion, the app shows the user's already-scored titles of the same `media_type`, filtered to shared genres, sorted by that criterion's score, as anchors.
- User places the new title relative to those anchors and assigns a 1.0–10.0 score (0.5 steps).
- Overall recomputes as the simple average of the criteria that apply to that media type (5 for anime, 4 for manga).
- The master ranking list re-sorts immediately by overall score.

*Anime and manga never anchor against each other — the comparison only makes sense within the same medium and the same criteria set.*

# 4. Functional Requirements

## 4.1 Search & Import

| **ID** | **Requirement** |
| --- | --- |
| FR-1 | User can search AniList by title, scoped to a media type toggle: Anime or Manga. |
| FR-2 | Selecting a result imports: title, native title, English title, cover image, genres, tags, producers, year, season, format, episodes (anime) or chapters/volumes (manga), average runtime per episode, synopsis, AniList average score. Tags and producers are stored the same way as genres — static metadata captured at import, refreshed only on re-import. |
| FR-3 | `media_type` is taken directly from the AniList result and stored immutably on import — it is never edited after the fact. |
| FR-4 | Duplicate import (same `anilist_id`) updates the existing row rather than creating a second entry. |

## 4.2 Rating Workflow

| **ID** | **Requirement** |
| --- | --- |
| FR-5 | Anime: 5-step rating stepper — Story, Characters, Animation, Audio, Enjoyment. |
| FR-6 | Manga: 4-step rating stepper — Story, Characters, Art, Enjoyment. The Audio step does not exist in the manga flow (not hidden — absent). The client must send `audio: 0.0` for manga; any non-zero value is rejected with 422 rather than silently ignored. Audio is stored as 0.0 and is always excluded from the manga average. |
| FR-7 | Each criterion: 1.0–10.0 **in 0.5 increments**. |
| FR-8 | While rating, the app shows anchors: same `media_type`, shared genre (default), sorted by that criterion. A pill switch labeled "Your calibration" (default: shared genre) lets the user manually flip to "All [Anime\|Manga]" — same `media_type`, no genre filter — or "Same Series" — the title's franchise (FR-69), same `media_type`, no genre filter — at any point during rating, without leaving the stepper. "Same Series" is offered only when at least one other rated title of the same `media_type` is in that franchise; it appears once the relations arrive, without shifting the layout. Every scope lists its anchors the same way, sorted by the current criterion, and marks them on the score slider; the chosen scope stays selected when the criterion changes. |
| FR-9 | Overall = average of active criteria, rounded to one decimal; computed server-side from `media_type`, never trusted from the client. |
| FR-10 | Re-rating any title recomputes overall and re-sorts rankings immediately. |
| FR-44 | "Rate Series" (available from a title's detail view when it has prequel/sequel relations) walks through every entry in that title's chain in chronological order, running the normal rating stepper (FR-5/FR-6) for each one in turn. The chain is filtered to the same media_type as the title Rate Series was launched from — an Adaptation relation (e.g. an anime's manga adaptation) is never included, consistent with anime and manga never anchoring against each other elsewhere in the app. Only chain entries already in the local library are included; a chain entry that isn't imported yet is shown as a skipped step with an inline "Add & rate" option, not silently omitted. |
| FR-45 | Rate Series has two distinct back controls, not one: the existing per-criterion back (previous step within the current title's stepper) and a separate "back a season" control that returns to the previous title in the chain, with that title's already-entered scores still editable rather than reset. |
| FR-46 | Completing all criteria for one entry auto-advances to the next entry in the chain rather than exiting the flow; Rate Series only ends after the last chain entry or when the user explicitly exits. |
| FR-68 | "Rate unrated" queue: the Anime and Manga tabs' Library header has a button labeled with a count (e.g. "Rate 12 unrated"), hidden when the count is 0. The queue is that tab's `media_type` titles with no rating whose status is Completed, Rewatching/Rereading, Watching/Reading, Paused or Dropped — never Planning, since an unseen title can't be rated. Entries run most recently updated first (grouping them by franchise would need an AniList request per title before the queue could start). The queue reuses the rating stepper (FR-5/FR-6) and Rate Series' banner ("Rate unrated · 3 of 12"), auto-advance (FR-46) and Back a Season (FR-45), plus a Skip control that moves to the next title without saving anything. A skip lasts only for that run of the queue: nothing is stored, so a skipped title is back in the queue the next time the button is pressed. Each finished title saves through the normal rating save (PUT /media/{id}/rating). Leaving mid-queue returns to the Library with the ratings already saved kept; finishing shows a short summary ("Rated 8 · Skipped 4") with a way back to the Library. |
| FR-69 | A title's franchise, for "Same Series" (FR-8): the entries reached by walking PREQUEL and SEQUEL edges multi-hop (the same walk, hop cap and cycle guard as Rate Series, FR-44), plus the entries one hop away from the title over PARENT, SIDE_STORY, ALTERNATIVE, SPIN_OFF and SUMMARY edges. Same `media_type` only, never crossing anime and manga, and only titles in the local library. It is built in the frontend from the relations GET /media/{id}/community returns (FR-49); relations already fetched this session are reused, so there is no new API and no repeat AniList request for those titles. |

## 4.3 Rankings & Views

| **ID** | **Requirement** |
| --- | --- |
| FR-11 | Master list shows all rated titles ranked by overall (rank, cover, title, media-type badge, scores). |
| FR-12 | Filter by media type (All / Anime / Manga), genre, and year. No season filter. |
| FR-13 | Sort by overall or any single criterion. |
| FR-14 | Episode/chapter count label adapts to media type: "12 eps" for anime, "84 ch" for manga — driven by which field is populated, not a separate flag. |
| FR-15 | Detail view shows all active criteria, metadata, and edit/delete actions. |
| FR-59 | Delete pushes to AniList before deleting locally, not the reverse: if the entry has an anilist_list_entry_id (has been synced at least once), the app calls AniList to remove it from the real list first, and only deletes the local row on that call's success. If the remote delete fails, the local row is NOT deleted — delete is not optimistic and not retry-queued, unlike FR-24's status/progress push. A never-synced entry (no anilist_list_entry_id) skips the AniList call entirely and deletes locally only. This keeps local and remote from ever silently diverging on a destructive action, the one place where FR-24's usual async-and-retry approach is deliberately NOT used. |
| FR-49 | Detail view's Relations, Characters, Recommended, and volatile stats block (popularity, favourites, trending, all-time rankings) load from GET /media/{id}/community on open — live, not stored. A brief loading state is expected here since it's a separate network call from the base detail data. |
| FR-16 | Search-within-rankings: a text field filters the currently displayed rankings list by title as you type. Separate from the AniList search screen used for import — this never calls AniList, it only filters titles already in the local library. |

## 4.4 Personal Insights (nice-to-have)

| **ID** | **Requirement** |
| --- | --- |
| FR-60 | By-genre breakdown: average overall score per genre, computed separately for anime and manga (a genre like Action can have different average scores across the two media types, and criteria differ, so they are never blended). |
| FR-61 | Top criterion: the single highest-averaging criterion across all rated titles, computed separately for anime (from Story/Characters/Animation/Audio/Enjoyment) and manga (from Story/Characters/Art/Enjoyment) — never combined into one cross-media number, since manga has no Audio. |
| FR-62 | Community divergence: for every rated title, compare your overall (FR-9) against avg_score captured at import (FR-2). Show an aggregate average divergence, plus the top 5 titles you rated highest ABOVE the community and the top 5 you rated LOWEST below it — not just a single number, since the interesting cases are the outliers, not the mean. |
| FR-63 | Score distribution: count of your overall ratings falling into each 0.5-point bucket from 1.0 to 10.0, split by media type (an anime-heavy vs. manga-heavy distribution shading differently is meaningful, per the same reasoning as FR-60). |

## 4.5 Library (Status Tracking)

Rankings (4.3) sorts by anchored score. Library is a separate view: it organizes titles by watch/read status, the way AniList's own list page does — this is the part of the app that makes it function as an AniList replacement rather than just a scoring tool.

*Anime and Manga are their own bottom-nav tabs, not one screen with a type toggle — each tab is that media type's Library, scoped to it automatically.*

| **ID** | **Requirement** |
| --- | --- |
| FR-17 | Every media row has a status: Watching, Completed, Paused, Dropped, Planning, Rewatching (anime) / Reading, Completed, Paused, Dropped, Planning, Rereading (manga). Set at import via a status picker (defaults to Planning) and editable anytime, independent of whether the title has been rated yet. |
| FR-18 | The Anime tab and Manga tab each show their entries grouped into collapsible accordion sections by status, stacked in one scroll — not tabs. Section order: Watching, Rewatching, Completed, Paused, Dropped, Planning (Reading/Rereading swapped in for manga mode). Each section header shows a count. |
| FR-19 | Each entry tracks progress: current episode (anime) or chapter (manga), separate from the total episode/chapter count from AniList. A quick +1 control increments progress without opening the detail view. |
| FR-57 | AniList returns null for episodes/chapters on titles that are hiatus, ongoing/irregularly-tracked, or otherwise unannounced (confirmed real case: Berserk's chapters/volumes are null in AniList's own data, not a bug). When total is null, progress displays as a bare read count — e.g. "142 ch read" — with no denominator, no "?" placeholder, no "Ongoing" label. Applies to both anime (null episodes) and manga (null chapters/volumes) equally, not just manga. FR-53's auto-complete-on-total-reached can never trigger for a null total, since there's nothing to compare progress against — status must be changed manually for these titles. |
| FR-53 | When progress reaches the title's total episode/chapter count while status is Watching/Reading, status auto-transitions to Completed — no confirmation prompt, matching AniList's own behavior. If status is Rewatching/Rereading when progress reaches the total, repeat_count (FR-51) increments by 1 and status returns to Completed rather than staying in Rewatching indefinitely. This applies via any progress-update path (quick +1, manual entry, detail view), not just one. |
| FR-56 | Manually changing a title's status TO Rewatching/Rereading (as opposed to FR-53's automatic transition away from it) shows a confirm dialog warning that progress will reset to 0, before applying the change. On confirm, progress resets to 0 and status becomes Rewatching/Rereading. On cancel, nothing changes — status stays at whatever it was. This is never silent in either direction (doesn't silently reset, and doesn't silently leave stale progress on old episode/chapter count). |
| FR-51 | Entries also track a repeat count (times rewatched/reread), mirroring AniList's own repeat field. Shown on the entry/detail view only when > 0 (e.g. "Rewatched 2x"); round-trips with Pull/Push (FR-23/FR-24) the same as status and progress. |
| FR-20 | Library supports the same List/Grid view toggle as Rankings. |
| FR-21 | A title can carry a status with no rating yet (e.g., Planning or early Watching) — Library must not require a rating to display an entry, since anchored scoring assumes you rate after forming an opinion, not on add. |
| FR-29 | The Completed section specifically splits into sub-sections by format (TV, Movie, OVA, ONA, TV Short, Special for anime / Manga, Novel, One Shot for manga), each with its own header, ordered TV/Manga first. Empty formats don't get a header. Every other status section stays a single flat list. |
| FR-30 | In-progress sections (Watching/Reading, Rewatching/Rereading) default expanded. All other sections (Completed, Paused, Dropped, Planning) default collapsed to just a header + count; tapping expands. Expand/collapse state persists between sessions. |
| FR-31 | A sort icon (title, progress, score, last updated) and a search-within-this-tab field sit at the top of the Anime and Manga tabs, searching only the entries already in that tab, same as FR-16's search-within-rankings but scoped to Library instead. |

## 4.6 AniList Account Sync

This is the feature that makes the app an actual AniList replacement rather than a local-only tracker sitting beside your real AniList list. It's also the highest-risk feature in this rebuild — it's the same general area (external sync) that contributed to the original bugs — so it gets an explicit, narrow contract instead of being folded silently into Library.

| **ID** | **Requirement** |
| --- | --- |
| FR-22 | User logs in via AniList OAuth. The app runs a small local HTTP listener on a fixed, hardcoded port dedicated solely to the OAuth callback, since the redirect URI registered with AniList must be exact and unchanging — e.g. http://localhost:8931/callback. The desktop (Tauri) app uses the Implicit Grant: the client_id is public by design (it's in the login URL), so it is built into the app (overridable via ANILIST_CLIENT_ID or local_config.toml) and there is no client_secret. AniList redirects there with the access token in the URL fragment, which never reaches a server, so the callback serves a static page that passes the token to the same listener, which validates it against AniList (Viewer) before storing it. The listener closes immediately after that one exchange. Only the access token is stored locally (anilist_auth); a client_secret never touches the database or version control. |
| FR-23 | On first login (or on demand), Pull imports the user's existing AniList anime + manga lists. Per entry, compare last_synced_at against both the local status/progress's own updated_at AND AniList's list-entry updatedAt: if only ONE side changed since last_synced_at, apply that side's value automatically (no prompt needed for a one-sided change). If BOTH sides changed since last_synced_at — a genuine conflict — show a per-entry prompt with both values (local vs. AniList) and let the user pick which one wins; Pull does not proceed past that entry silently. New AniList entries with no local counterpart import directly, no prompt. Pull also backfills anilist_list_entry_id (FR-59) onto any existing local row that matches a real AniList entry but never had it recorded — needed so delete/push know which rows are truly synced versus local-only. |
| FR-24 | Any local status or progress change automatically pushes to AniList in the background. Local is authoritative for anything edited inside this app — UNLESS the automatic push detects a genuine conflict (FR-58), in which case it doesn't push at all until resolved. |
| FR-25 | This app's anchored overall score pushes to AniList's score field with no conversion math: on first login the app requests the user's consent to set the account's scoreFormat to POINT_10_DECIMAL (1.0–10.0, one decimal) via AniList's own settings mutation, since that format maps 1:1 to this app's 0.5-step scale. AniList still has no concept of this app's 5-criteria breakdown — only the overall number travels; Story/Characters/Animation/Audio/Enjoyment stay local-only. |
| FR-26 | Once logged in, brief network loss doesn't log you out or block the app — failed pushes queue and retry silently when connectivity returns. This is distinct from FR-47: a *deliberate* logged-out state (never logged in, or logged out) blocks the app entirely; a *transient* dropped connection while logged in does not. |
| FR-47 | The app requires an active AniList login to be used at all. On launch, if not connected, only a login/connect screen is shown — no tab (Anime, Manga, Discover, Feed, Profile), no local library, and no cached data from a previous session is reachable until login completes. |
| FR-48 | Logging out (FR-27) returns immediately to the login-gate screen, same as a fresh launch — logout no longer drops into a disconnected-but-usable local mode. |
| FR-27 | Logout clears the stored token only. No local library data is deleted. |
| FR-28 | Every sync re-checks the account's scoreFormat before pushing a score. If it's drifted away from POINT_10_DECIMAL (e.g. changed on the AniList website), the app shows a fresh confirm dialog before switching it back — the original first-login consent (FR-25) does NOT cover this; every drift correction gets its own explicit approval, since it's a real change to the account each time, not a one-time grant. Never silently re-applies. |
| FR-58 | Every AUTOMATIC push path (the background task fired after PUT status/progress/repeat/rating succeeds, and the retry queue) runs the same conflict check Pull uses before pushing. If AniList's copy of this entry changed since last_synced_at (independent of the local edit that just happened), the push is skipped — never silently overwritten, never silently dropped — and the entry is flagged pending_conflict. This does NOT apply to sync/resolve's own push: once the user has explicitly chosen a winner for a conflict, that push is final and unconditional — re-checking it would make conflict resolution circular and impossible to complete. |

## 4.7 Discover

A new tab, separate from the AniList-catalog search used for import. Discover is for browsing — trending, seasonal, and top-ranked shelves — the way AniList's own Discover page works.

| **ID** | **Requirement** |
| --- | --- |
| FR-32 | Discover tab shows horizontal shelves: Currently Trending Anime/Manga, Current Season, Upcoming Season, Top Ranked Anime, Top Ranked Anime Movies, Popular Anime/Manga — scoped to whichever media type (Anime/Manga) is active. |
| FR-50 | Discover and Feed responses are cached server-side for 60 seconds per unique query (e.g. per shelf, per feed type) to stay within AniList's 30 req/min limit during normal tab-switching. Revisiting within that window serves the cached response instead of re-querying AniList; after 60 seconds the next visit refetches. |
| FR-33 | If a shelf title is already in the local library, its cover shows a small status label ("Reading", "Completed", "Planned", etc.) instead of nothing, so Discover doubles as a quick at-a-glance library check. |
| FR-55 | Discover shelves must be strictly scoped to the active Anime/Manga toggle — both the underlying title data AND its cover image. A title existing as both an anime and a manga (e.g. One Piece) has a DIFFERENT anilist_id and DIFFERENT cover art per media_type; any image lookup must key on (anilist_id, media_type) together, never on title text alone, or entries cross-contaminate between the two tabs. |
| FR-52 | Tapping any Discover card opens that title's detail view, same as tapping a Library entry. If the title isn't in the local library yet, the detail view shows an "Add to Library" action with the status picker (FR-17) in place of the status/progress/score pills — Discover never leaves a title without a path to actually add it, matching the existing Search flow (FR-2) rather than requiring a separate re-search by name. |

## 4.8 Feed

Friend/social activity — previously deferred, now in scope. Unlike Library and Rankings, Feed is read-only and never persisted locally: it's a live window into AniList's own Activity API, scoped to the connected account's follow graph.

| **ID** | **Requirement** |
| --- | --- |
| FR-34 | Feed has a Your Feed / Following Feed / Global Feed picker, matching AniList's own three scopes. |
| FR-35 | Activity items render as "{username} watched episode X–Y of {title}", "{username} completed {title}", "{username} rewatched episode X–Y of {title}", etc., each with a relative timestamp and cover thumbnail. Tapping an item opens that title's detail view. |
| FR-36 | Feed is fetched (subject to the 60-second cache in FR-50) and paginated on scroll; nothing about other users' activity is persisted in the local database beyond that short-lived cache. |
| FR-37 | Feed requires an active AniList connection (FR-22) — automatically satisfied by FR-47's app-wide login gate, so Feed never needs its own separate connect prompt. |

## 4.9 Characters, Staff & Favorites

New entity types the app has never modeled before. To avoid opening a second sync-conflict surface, these are handled as a live pass-through to AniList rather than a locally-cached, locally-editable copy — AniList stays the single source of truth for who's favorited, with nothing to drift out of sync.

| **ID** | **Requirement** |
| --- | --- |
| FR-38 | Character and Staff (voice actor / mangaka) detail pages are fetched live from AniList on open — bio, member-favorites count, voice actors by language (characters) or filmography (staff), anime/manga appearances. Nothing here is stored in the local database. |
| FR-39 | A heart/favorite icon appears on anime, manga, character, staff, and studio pages. Tapping it calls AniList's favorite toggle directly on the connected account — there is no local-only favorite state to fall out of sync. |
| FR-40 | Profile shows one shelf per favorited category — Anime, Manga, Characters, Staff, Studios — pulled live from the connected account on each Profile visit, not cached. A shelf renders only if that category has at least one favorite; empty categories are omitted entirely rather than shown empty. |
| FR-54 | Every card on every Profile favorite shelf is tappable: Anime/Manga cards open that title's detail view (FR-52's in-library or not-yet-imported state, whichever applies); Character/Staff cards open the respective detail view (FR-38). No favorite card is a dead end. |
| FR-41 | Favorite hearts always have somewhere to live — FR-47's login gate means the app is never reachable in a disconnected state where a favorite toggle would be meaningless. |

## 4.10 Profile

A dedicated tab, not previously speced as its own screen.

| **ID** | **Requirement** |
| --- | --- |
| FR-42 | Profile shows stats — total anime, total manga, days watched, chapters read, followers, following — pulled live from AniList's viewer statistics. No disconnected fallback is needed — FR-47 guarantees Profile is only ever reached while connected. |
| FR-43 | A settings/gear icon on Profile opens Account/Sync management — connect/disconnect AniList (FR-22), manual Pull (FR-23), and connection status — rather than this living as a separate tab. |

## 4.11 Studios & Users (M9)

Same live pass-through model as 4.9: nothing here is stored in the local database, and nothing here can write to another user's data.

| **ID** | **Requirement** |
| --- | --- |
| FR-64 | Catalog Search (FR-1) has a category picker: Anime, Manga, Characters, Staff, Studios, Users. Only Anime and Manga results feed the import flow (FR-2); the other categories are live lookups that open the matching detail view. |
| FR-65 | Studio search returns AniList studios by name. Studio detail shows name, animation studio vs. production company, member-favorites count, a favorite heart (FR-39), and the studio's works. Each work opens that title's detail view (FR-52's in-library or not-yet-imported state). Favorite Studio cards on any Profile shelf open this view (extends FR-54). |
| FR-66 | User search returns AniList users by name. A user's public profile shows avatar, banner, about text, join year, anime/manga counts, mean scores, episodes watched, chapters read, and their favorite shelves (same shelf rules and click-through as FR-40/FR-54). |
| FR-67 | User profiles are read-only browsing of public AniList data. No follow/unfollow, messaging, or other social write features: nothing in this app ever writes to another user's AniList data. The only other-user data stored locally is Friend Comparison's (4.12): the names and avatars of the people you follow, and friends' public lists, all read without the token (FR-71, FR-79–FR-81), with one exception: the mutual-only friend sync reads your own following list with your token (FR-79). |

## 4.12 Friend Comparison

Compare your list against a friend's. Your friends are the AniList users you follow, read from your following list (FR-79: without the token, except one read for mutual follows), plus anyone you add by username. A friend is a local record, not an AniList follow: adding, hiding or removing one changes nothing on AniList, and nothing here follows or unfollows anyone. Unlike 4.8–4.11, a friend's list IS stored locally (`friend`, `friend_entry`), because a comparison needs the whole list and refetching it on every view would burn AniList's rate limit; lists are fetched by a slow background queue (FR-80), when a comparison needs one (FR-81), or on Refresh. Friend data is read-only and isolated: it never feeds rankings, anchors, Insights, or sync, and is never pushed anywhere.

| **ID** | **Requirement** |
| --- | --- |
| FR-70 | Profile (4.10) has a Friends section: a "Sync from AniList" button with when the friends list last synced, the friends setting (FR-79), an add-by-username field, and a list of friends. Each row shows avatar, name, where the friend came from ("From AniList" or "Added by you"), their list's status ("Not fetched yet", "Fetching their list…", "12 anime · 3 manga · fetched 3 days ago", or "Private list, can't compare"; a friend whose list turned private after it was fetched shows "Private list now · 12 anime · 3 manga · fetched 3 days ago" and can still be compared from that copy, FR-81), and Refresh, Remove or Hide, and Compare actions. The second action is "Remove" for a friend you added by username, which deletes them; for a friend from AniList it's "Hide", which hides them, so a sync doesn't bring them back while you still follow them. Compare is disabled for a private friend with no stored list. Hidden friends are listed under a collapsed "Hidden (N)", each with an Unhide action, which checks your following list right away (a friend sync): if you still follow them they're back on the spot, their list next in line; if you don't, the hidden record is simply removed. If AniList can't be reached they're unhidden and the next sync settles it. Adding someone by username always makes them a manual friend, visible and kept even if you unfollow them (this also un-hides a hidden one). A user's public profile (FR-66) has a "Compare lists" button that adds that user by username if they aren't a friend yet and opens the comparison. Your own account can't be added. |
| FR-71 | Fetching a friend's list reads their anime and manga lists from AniList without the access token (public data only, so the app can never act as or for them) and replaces that friend's stored entries in one transaction. It happens from the background queue (FR-80), when a comparison needs it (FR-81), when you add a friend by username, and on Refresh; a detail page never calls AniList and reads the stored copy. Each fetch is two AniList requests (the user lookup, which also gives their score format, then both lists in one query; for a user the lookup can't find, the second request asks whether they're private or missing), within AniList's shared 30 req/min limit. A 429 is handled as for every other AniList call: it waits for Retry-After (at most 30 seconds, at most twice) and retries; a longer wait fails the fetch with a "try again shortly" message. |
| FR-72 | Scores are normalized before comparing. For a friend on POINT_100, POINT_10_DECIMAL or POINT_10, AniList converts each entry's score to POINT_10_DECIMAL server-side, the same way Pull reads your own list. POINT_5 and POINT_3 are instead read in the friend's own format and converted by the app, because AniList's conversion tops them out below 10 (5 stars is 9.0, :) is 8.5): POINT_5 is stars × 2 (1 → 2 … 5 → 10); POINT_3 is 1 → 4, 2 → 7, 3 → 10. The friend's format comes with the user lookup (FR-71), so this costs no extra request. A score of 0 means "unscored" and is stored as NULL. Both sides are clamped to this app's 1.0–10.0 scale. Your side is always your local overall (FR-9), which already includes scores Pull brought in from AniList. |
| FR-73 | If AniList has no user by that name, an add by username fails with "No AniList user named X". If their list is private, the add fails with a clear "X's list is private on AniList" message and nothing is stored. A friend already in the list (from AniList, or added earlier) whose list turns out private stays, marked "Private list, can't compare"; the background queue doesn't ask again for 7 days, and Refresh always asks. A fetch that fails this way keeps the previously stored list and its last-fetched time. |
| FR-74 | A comparison covers one media type at a time (the Anime/Manga toggle) and never mixes the two. Titles match on anilist_id. "Rated" means a non-null score on that side. The comparison shows: overlap (titles both of you rated); taste match = 100 × (1 − mean \|yours − theirs\| / 9), rounded to a whole percent, where 9 is the largest possible gap on a 1–10 scale, hidden when overlap is under 5, with a tooltip explaining the formula; and skew = mean(yours − theirs) over the overlap, positive when you score higher. |
| FR-75 | Biggest disagreements: the 5 shared titles with the largest \|yours − theirs\|, however small that gap is. Strongest agreements: up to 5 of the remaining shared titles, smallest gap first. Ties in both go to the higher average of the two scores, then AniList id. A title never appears in both lists, and each media type uses its own pool. When every shared title is within 0.5 (each gap at most 0.5), the disagreements card says "You two rarely disagree" above its titles. Each shows cover, title, both scores, and the delta, and expands in place (FR-82). |
| FR-76 | A full table of shared rated titles (cover, title, your score, their score, delta), sortable by your score, their score, or delta, in either direction. Clicking a row expands it in place (FR-82) rather than leaving the page. |
| FR-77 | "They've completed, you haven't": titles the friend has Completed or Rewatching/Rereading with a score of 8.0 or higher that aren't in your library or are only on your Planning list. "You've completed, they haven't": the reverse, from your Completed/Rewatching/Rereading titles with an overall of 8.0 or higher that aren't on their list or are only on their Planning list. Each is ordered by that person's score, highest first, ties going to the more popular title (AniList popularity: how many members have it on their list; unknown counts as least popular), then AniList id. Each shows its top 10 in a wrapping grid, with "Show all (N)" to expand the full list and "Show less" to collapse it; each item shows cover, title, the score, and a note when the title is on a Planning list (yours in the first list, theirs in the second). Every title in these lists opens its detail view (FR-52's in-library or not-yet-imported state); Back returns to the comparison as you left it: the same scroll position, expanded lists and expanded row. |
| FR-78 | Genre comparison: per genre, your average overall and theirs, each over that side's own rated titles of this media type, with counts, side by side; a genre one side has no rated titles in shows a dimmed "— (0)" and an empty bar for that side. A title's detail view (FR-15) shows a chip for each friend who has scored that title, with their name and score; their status is in the chip's tooltip. Chips read the stored lists only: friends whose lists haven't been fetched yet don't appear, and hidden friends never do. |
| FR-79 | Friend sync reads the AniList users you follow (`Page.following` for your own user id, 50 per page, every page) without the access token and stores only their id, name and avatar; it fetches no lists (FR-80 does that). It runs after login, after every Pull (FR-23), and from the "Sync from AniList" button, and records when it last succeeded. A setting chooses who counts: "Everyone I follow" (the default) or "Only mutual follows" (people you follow who follow you back), with a tooltip saying so. For mutual only, the sync reads your own following list with your access token, the one exception to 4.12's no-token rule: a read query only, never a mutation, and for your own following list only, so that AniList fills each user's `isFollower` (whether they follow you) and no followers pages are needed. That answer counts only when the same response's `Viewer` is your account, because without a viewer AniList says false for everyone; if AniList refuses the token or doesn't confirm the viewer, the sync reads your followers (`Page.followers`) without the token instead. The everyone sync, friends' lists and everything else in 4.12 never send the token. Each friend records its source: `anilist` (from sync) or `manual` (added by username). A sync adds new follows, updates names and avatars, and removes `anilist` friends you no longer follow (or who are no longer mutual), with their stored lists. It never removes a `manual` friend, a `manual` friend who is also followed stays `manual`, and hidden friends (FR-70) stay hidden. A sync that fails part-way changes nothing. It costs one request per 50 follows either way, plus one per 50 followers only in the mutual-only fallback. A sync failing after login or Pull is logged and never fails them. |
| FR-80 | Friends' lists are fetched in the background, one friend (two requests, FR-71) at a time, at most one every 12 seconds: 5 friends or 10 requests a minute, a third of AniList's limit, so the rest of the app keeps the budget. Next in line: friends whose list has never been fetched first, then the oldest fetch, among lists older than 24 hours; a private friend waits 7 days after AniList last said so (FR-73); a hidden friend never. A friend whose fetch fails for any reason other than not reaching AniList (say, their account is gone) is skipped for an hour, so they can't hold up everyone behind them. Both waits (the hour and a private friend's 7 days) are stored, so restarting the app doesn't retry anyone early. The queue pauses while any other AniList call is in flight, and for 60 seconds after any 429 (longer if Retry-After says so). It runs only while logged in and is idle when every list is fresh. |
| FR-81 | Opening a comparison first fetches the friend's list, ahead of the queue, if it has never been fetched or is older than 24 hours, unless they're marked private (FR-73). If that fetch fails, the comparison uses the stored copy, or fails with the fetch's error when there isn't one. A private friend with no stored list can't be compared. |
| FR-82 | Clicking a title in Biggest disagreements, Strongest agreements, or the shared table expands it in place; clicking it again collapses it, and only one title is expanded at a time across the whole page. Expanding keeps the table's sort and the page's scroll position. The two cards are laid out as one grid of rows, so an item growing in one column keeps the other column's rows aligned. The expanded view shows your per-criterion scores, in the rating stepper's labels and order (Story, Characters, Animation, Audio, Enjoyment for anime; Story, Characters, Art, Enjoyment for manga), as mono numbers with small bars, with your highest and lowest criterion marked, plus your overall. A rating that came from AniList (rating.source = 'anilist', §5) has no real category scores, so it shows "Rated on AniList only, no category scores" and a "Rate it now" button that opens the rating stepper for that title and comes back to the refreshed comparison; saving it pushes through the usual sync (FR-24). The view also shows the friend's overall, their status and progress, a quiet note that AniList doesn't share their category scores, and an "Open details" link to the title's detail view. Nothing in an expanded row calls AniList. |
| FR-83 | The friend has a color of their own, `--friend` (amber, #f2a65a, with `--friend-dim` for gradients), next to your `--accent` blue: their column header and scores in the shared table, their scores in both cards and the completed lists, their overall in an expanded row, their genre bars (the same gradient style as yours), genre numbers and legend dot, and the score on a friend chip on a detail page. Delta green and red stay as they are. |

# 5. Data Model

SQLite, six tables. One row per title regardless of media type — this is the structural fix. Sync adds one small auth table; it does not touch the media/rating split. Friend Comparison (4.12) adds three tables of its own (`friend`, `friend_entry`, `friend_sync`) that nothing else reads; they are added with CREATE TABLE IF NOT EXISTS, so an existing library.db gains them on open with no change to its other tables.

A `friend` table from before FR-79 (one without `source`) is migrated on open, in one transaction, without changing any existing value: `fetched_at` is renamed to `added_at` (a schema-only change, no row is rewritten); `source` (default 'manual'), a new nullable `fetched_at`, `private_at` and `hidden` (default 0) are added; and the new `fetched_at` is filled from `added_at`. So every existing friend stays a manual friend with its stored list and its fetch time. A `friend` table without `retry_at` just gains that column (NULL). `friend_entry`, `media`, `rating` and `anilist_auth` are not touched by that migration.

FR-77 and FR-82 add four columns the same way (ALTER TABLE ADD COLUMN, no existing value changed): `media.popularity` and `friend_entry.popularity` (NULL until the next Pull or that friend's next list fetch, both of which already request it), `friend_entry.progress` (NULL until that friend's next fetch), and `rating.source` (below), whose existing rows are classified once by the rule given there.

## media

| **Field** | **Type** | **Notes** |
| --- | --- | --- |
| id | INTEGER PK | Local id |
| anilist_id | INTEGER UNIQUE | AniList's id — the only external id stored |
| media_type | TEXT | 'anime' \| 'manga', set once at import, NOT NULL |
| title / title_native / title_english | TEXT | Display + native + English title |
| cover_url | TEXT | Cached cover image URL |
| genres | TEXT (JSON) | List of genre strings |
| tags | TEXT (JSON) | List of tag strings (more granular than genres, e.g. "Kingdom Management") — stored the same way as genres |
| producers | TEXT (JSON) | List of producer/studio strings — stored the same way as genres |
| runtime_minutes | INTEGER | Average minutes per episode (anime only); NULL for manga |
| year / season / format | INTEGER / TEXT / TEXT | From AniList |
| episodes | INTEGER | Anime only; NULL for manga |
| chapters / volumes | INTEGER | Manga only; NULL for anime |
| synopsis | TEXT | |
| avg_score | REAL | AniList community average, normalized to /10 |
| status | TEXT | watching\|completed\|paused\|dropped\|planning\|rewatching (reading/rereading for manga), NOT NULL, default 'planning' |
| progress | INTEGER | Current episode/chapter, default 0. Independent of episodes/chapters (the series totals). |
| repeat_count | INTEGER | Number of completed rewatches/rereads, default 0. Mirrors AniList's own repeat field; displayed on entries/detail when > 0, matching AniList's own display. |
| anilist_list_entry_id | INTEGER | NULL until first synced. AniList's own id for this title's entry on your list — required to push updates instead of creating duplicates. |
| last_synced_at | TEXT | ISO timestamp of this entry's last successful sync. Compared against local updated_at and AniList's own list-entry updatedAt to detect one-sided vs. two-sided (conflicting) changes per FR-23. |
| pending_conflict | INTEGER | 0/1, default 0. Set to 1 when an automatic push (FR-58) detects AniList changed independently and skips pushing; cleared by sync/resolve. |
| popularity | INTEGER | AniList popularity (members with the title on their list), used only to break ties in FR-77. Written on import and refreshed by every Pull, for titles already in the library too; NULL until then. |

*Deliberately NOT stored on media: relations, this title's cast, community recommendations, favourites, trending, and all-time rankings (popularity is the one exception, kept only as FR-77's tie-break; the detail view's Members count is still fetched live). These are volatile — they change independently of anything the user does — so storing them would just be another staleness surface. They're fetched live via GET /media/{id}/community (Section 7) each time a detail view opens, the same live-pass-through pattern already used for Characters/Staff (FR-38).*

## rating

| **Field** | **Type** | **Notes** |
| --- | --- | --- |
| media_id | INTEGER PK/FK | References media.id, one rating per title |
| story / characters / enjoyment | REAL | 1.0–10.0 (0.5 steps), always required |
| animation | REAL | 1.0–10.0 (0.5 steps); UI labels this "Art" when media_type='manga' |
| audio | REAL | 1.0–10.0 (0.5 steps) for anime; always 0.0 for manga, never NULL |
| overall | REAL | Computed server-side from the active criteria set |
| updated_at | TEXT | ISO timestamp |
| source | TEXT | 'app' (rated in this app's stepper) or 'anilist' (seeded by Pull from AniList's overall score, FR-23, which sets every criterion to that one value). NOT NULL, default 'app', CHECK. Rating a title in the app sets 'app'; a Pull that only updates overall leaves it as it was. Drives FR-82's "Rated on AniList only". |

*`rating.source` was added by migration. Existing rows were classified once, by rule: a rating whose active criteria are all equal (story = characters = animation = enjoyment, and = audio for anime) is 'anilist', because that is exactly how Pull seeds them; every other rating is 'app'. A NULL animation (the nullable-column defect) never counts as equal, so such a row is 'app'. Known miss: a title you rated in the app with every criterion the same is classified 'anilist' and shows "Rated on AniList only" with "Rate it now" until you rate it again.*

*A media row can exist with no rating row at all (Planning/early Watching, per FR-21) — rating is created only once the user actually scores a title.*

## anilist_auth

Single row (id=1). Holds the local OAuth session — never synced anywhere, never exposed outside this app. There is no client_secret: the implicit grant (FR-22) needs only the public client_id, which is built into the app.

| **Field** | **Type** | **Notes** |
| --- | --- | --- |
| access_token | TEXT | AniList OAuth token, stored locally only |
| token_expires_at | TEXT | ISO timestamp — AniList tokens are valid ~1 year with no refresh token, so this is checked at launch, not per-request |
| anilist_user_id | INTEGER | The connected account's AniList id |
| anilist_username | TEXT | Displayed in the app so it's clear which account is connected |
| score_format | TEXT | Set to POINT_10_DECIMAL on the AniList account itself (FR-25/FR-28), so push needs no conversion; re-verified on every sync. |

## friend

One row per friend (FR-70, FR-79). A local record only; nothing here changes an AniList follow.

| **Field** | **Type** | **Notes** |
| --- | --- | --- |
| anilist_user_id | INTEGER PK | AniList's user id. Usernames can change; ids don't. |
| name | TEXT | AniList username, NOT NULL, refreshed on every fetch and sync |
| avatar_url | TEXT | |
| added_at | TEXT | ISO timestamp the friend was added, NOT NULL. For friends from before FR-79, their last fetch time at the migration. |
| source | TEXT | 'anilist' (friend sync, FR-79) or 'manual' (added by username), NOT NULL, default 'manual', CHECK |
| fetched_at | TEXT | ISO timestamp of the last successful fetch of their lists (FR-71); NULL = not fetched yet |
| private_at | TEXT | When AniList last said their list is private (FR-73); NULL = not known to be private. Cleared by a successful fetch. |
| hidden | INTEGER | 1 = a friend from AniList you removed (FR-70): not listed, compared, fetched or shown on detail pages, only under Hidden. NOT NULL, default 0 |
| retry_at | TEXT | The queue leaves them alone until then after a fetch failed for a reason other than reaching AniList (FR-80); NULL = no wait. Cleared by a successful fetch. |

## friend_sync

Single row (id=1): the friends setting and the last friend sync (FR-79).

| **Field** | **Type** | **Notes** |
| --- | --- | --- |
| id | INTEGER PK | Always 1 |
| mutual_only | INTEGER | 0 = everyone you follow (default), 1 = only mutual follows. NOT NULL |
| synced_at | TEXT | ISO timestamp of the last successful friend sync; NULL = never |

## friend_entry

A friend's public list as of `friend.fetched_at` (FR-71). Replaced wholesale on each fetch; deleted with the friend (`ON DELETE CASCADE`).

| **Field** | **Type** | **Notes** |
| --- | --- | --- |
| friend_id | INTEGER | References friend.anilist_user_id, ON DELETE CASCADE, NOT NULL |
| anilist_id | INTEGER | The title's AniList id, the match key against media.anilist_id (FR-74). PK is (friend_id, anilist_id). |
| media_type | TEXT | 'anime' \| 'manga', from AniList's own MediaType, NOT NULL, same CHECK as media |
| status | TEXT | Their status in this app's FR-17 vocabulary (mapped like Pull), NOT NULL |
| score | REAL | 1.0–10.0 after FR-72 normalization; NULL when they haven't scored it |
| title / title_english / cover_url | TEXT | For display in the comparison, since most of their titles aren't in your library |
| genres | TEXT (JSON) | List of genre strings, for FR-78 |
| progress | INTEGER | Their episode/chapter progress, shown in FR-82; NULL for lists fetched before this column |
| popularity | INTEGER | The title's AniList popularity, FR-77's tie-break; NULL for lists fetched before this column |

# 6. Technical Architecture

| **Layer** | **Choice** | **Notes** |
| --- | --- | --- |
| Backend | Rust (axum router, `src-tauri/src/api.rs`) | All routes; `src-tauri/src/anilist.rs` is the only caller of the AniList API |
| Database | SQLite (rusqlite, `src-tauri/src/db.rs`) | Single local file (`library.db`) in the OS app-data folder, or `MYANIMERANK_DB`; zero-config |
| Metadata source | AniList GraphQL only | `graphql.anilist.co` — search + by-id lookups |
| Frontend | Single-file vanilla HTML/JS (`frontend/index.html`) | Same page in the desktop window and in a browser under `--serve` |
| Desktop shell | Tauri 2 | Frameless native window with a custom titlebar (WebView2 / WebKit / WebKitGTK) |
| Hosting | In-process, no server | The page reaches the router through one Tauri command, so no port is open; `cargo run -- --serve <port>` binds it to localhost for browser development |
| Auth | AniList OAuth, Implicit Grant | Fixed dedicated local port for the callback only (`src-tauri/src/oauth.rs`); built-in public client_id, no client_secret — per FR-22 |

# 7. API Contract (for frontend integration)

This is the contract the separately-built frontend integrates against. Every response shape below is final for v2 — design the frontend's data layer to these shapes directly.

## GET /search?q=&type=anime|manga

```
Response: { "results": [ {
  anilist_id, media_type, title, title_native, title_english, cover_url,
  genres: [string], tags: [string], producers: [string], year, season, format,
  episodes, chapters, volumes, runtime_minutes, synopsis, avg_score
} ], "q": string }
```

## POST /media

```
Body: MediaImport (search result shape + status: one of the FR-17 values, default 'planning')
Response: { "id": number }   // local media.id
```

## PUT /media/{id}/status

```
Body: { "status": string }  // one of the FR-17 values for that media_type
Response: { "status": string }
```

## PUT /media/{id}/progress

```
Body: { "progress": number }
Response: { "progress": number }
```

## PUT /media/{id}/repeat

```
Body: { "repeat_count": number }
Response: { "repeat_count": number }
// FR-51; round-trips with Pull/Push same as status and progress.
```

## PUT /media/{id}/rating

```
Body: { story, characters, animation, audio, enjoyment }  // all 5 always sent, 0.5 steps
Response: { "overall": number, "media_type": "anime"|"manga" }
// Backend computes overall from media_type. Manga must send audio: 0.0 (422 otherwise, FR-6); anime audio follows the 1.0–10.0 / 0.5-step rule.
// Creates the rating row if one doesn't exist yet (see FR-21).
```

## GET /media/{id}

```
Response: { ...media fields (including tags, producers, title_english, runtime_minutes), status, progress, rating: {...} | null }
```

## GET /media/by-anilist/{anilist_id}

```
Same shape as GET /media/{id}, but keyed by anilist_id for titles reached via Discover that aren't imported yet (FR-52) — no local id exists for them.
Response: { in_library: false, ...AniList fields } | { in_library: true, ...media fields, id, status, progress, rating }.
```

## GET /media/{id}/community

```
Response: { relations: [ { anilist_id, type, title, cover_url } ], characters: [ { name, role } ], recommended: [ { anilist_id, title, cover_url, thumbs } ], popularity, favourites, trending, rank_rated, rank_popular }
// Live-fetched from AniList on every detail view open; never persisted (FR-49). This is also the source for Rate Series's chain (FR-44) via the relations array.
```

## DELETE /media/{id}

```
Response: { "deleted": true } | 502 if remote delete fails (local row untouched)
// FR-59: pushes to AniList first if anilist_list_entry_id exists; local delete only happens on that success. Never-synced entries (no anilist_list_entry_id) skip straight to local delete.
```

## GET /library?media_type=&status=&sort=

```
Response: { "results": [ { ...media fields, status, progress, rating: {...} | null } ], "count": number }
// status=all (or omitted) returns everything; otherwise one of the FR-17 values.
```

## GET /auth/anilist/login

```
Opens AniList's OAuth authorize page in the system browser; redirects locally on approval.
```

## GET /auth/anilist/callback?code=

```
Exchanges the code for a token, fetches viewer id/username/scoreFormat, stores in anilist_auth.
Response: { "connected": true, "username": string }
```

## GET /auth/status

```
Response: { "connected": boolean, "username": string | null }
```

## POST /auth/logout

```
Response: { "connected": false }   // clears anilist_auth only; library data untouched
```

## POST /sync/pull

```
Fetches the connected account's full AniList anime + manga lists.
Response: { "imported": number, "updated": number, "conflicts": [ { media_id, title, local: {status,progress,updated_at}, remote: {status,progress,updatedAt} } ] }
// conflicts = entries where BOTH sides changed since last_synced_at (FR-23); frontend prompts per-entry, then calls POST /sync/resolve for each choice.
// Afterwards runs a friend sync (FR-79); its failure is logged and doesn't change this response.
```

## POST /sync/resolve/{media_id}

```
Body: { "winner": "local"|"remote" }
Response: { "status": string, "progress": number }
// Applies the user's per-entry conflict choice from FR-23 and updates last_synced_at.
```

## GET /sync/pending-conflicts

```
Response: { "conflicts": [ { media_id, title, local: {status,progress,updated_at}, remote: {status,progress,updatedAt} } ] }
// Same shape as sync/pull's conflicts array (FR-58) — entries where an automatic push was skipped due to detected divergence. POST /sync/resolve/{media_id} resolves these identically to Pull-surfaced conflicts.
```

## POST /sync/push/{media_id}

```
Pushes current status/progress/overall for one entry to AniList.
Response: { "synced_at": string }
// Called automatically per FR-24/FR-25/FR-28; exposed here for manual retry after a failure.
```

## GET /rankings?media_type=&genre=&year=&sort=&q=

```
Response: { "results": [ { ...media fields, rating: {...} } ], "count": number }
// q filters results by title match; applied client-side or server-side, same effect either way.
```

## GET /media/{id}/anchors?criterion=story&scope=genre|all

```
Response: { "anchors": [ { id, title, cover_url, score } ] }
// Always same media_type as {id}, sorted desc by criterion. scope=genre (default) filters to shared genre; scope=all removes the genre filter entirely — driven by the pill switch in FR-8, not an automatic fallback.
```

## GET /insights

```
Response: {
  by_genre: [ { genre, media_type, avg_overall, count } ],
  top_criterion: { anime: { criterion, avg_score } | null, manga: { criterion, avg_score } | null },
  community_divergence: { avg_divergence, most_above: [ { media_id, title, your_score, avg_score, delta } ], most_below: [ ... ] },
  distribution: { anime: [ { bucket, count } ], manga: [ { bucket, count } ] }
}
// FR-60 through FR-63. Computed from local rating data (community_divergence uses avg_score already captured at import, no live AniList call needed).
```

## GET /discover?section=&media_type=

```
section: trending | current_season | upcoming_season | top_ranked | top_ranked_movies | popular
Response: { "results": [ { ...media fields, local_status: string | null } ], "count": number }
// local_status is set only if the title is already in the local library (FR-33). Cached server-side 60s per section+media_type (FR-50).
```

## GET /feed?type=&page=

```
type: your | following | global
Response: { "activities": [ { username, verb, title, anilist_id, progress_range, occurred_at } ], "has_more": boolean }
// Pass-through to AniList's Activity API, cached server-side 60s per type+page (FR-50); nothing here is persisted beyond that cache (FR-36).
```

## GET /characters/search?q=

```
Response: { "results": [ { anilist_id, name, image_url } ] }
```

## GET /characters/{anilist_id}

```
Response: { anilist_id, name, image_url, bio, favorites_count, voice_actors: [ { name, language, image_url } ], appearances: [ { anilist_id, title, cover_url } ] }
// Live-fetched, never persisted (FR-38).
```

## GET /staff/{anilist_id}

```
Response: { anilist_id, name, image_url, bio, favorites_count, characters_voiced: [...], works: [...] }
```

## POST /favorites/toggle

```
Body: { "type": "anime"|"manga"|"character"|"staff"|"studio", "anilist_id": number }
Response: { "favorited": boolean }
// Calls AniList's own favorite toggle on the connected account. Requires FR-22; no local state (FR-39).
```

## GET /favorites

```
Response: { "anime": [...], "manga": [...], "characters": [...], "staff": [...], "studios": [...] }
// Live from AniList's viewer favourites; powers Profile's favorite shelves (FR-40) and heart-icon state. Only non-empty arrays are surfaced on Profile per FR-40.
```

## GET /profile/stats

```
Response: { "total_anime": number, "total_manga": number, "days_watched": number, "chapters_read": number, "followers": number, "following": number, "avatar_url": string | null, "banner_url": string | null }
// No connected flag needed — this endpoint is only ever reachable while logged in, per FR-47.
```

## GET /studios/search?q=

```
Response: { "results": [ { anilist_id, name } ] }
// Live AniList lookup, nothing persisted (FR-65).
```

## GET /studios/{anilist_id}

```
Response: { anilist_id, name, is_animation_studio: boolean, favorites_count, works: [ { anilist_id, title, title_english, cover_url, year } ] }
// Live-fetched, never persisted (FR-65).
```

## GET /users/search?q=

```
Response: { "results": [ { anilist_id, name, avatar_url } ] }
// Live AniList lookup, nothing persisted (FR-66).
```

## GET /users/{anilist_id}

```
Response: { anilist_id, name, avatar_url, banner_url, about, site_url, created_at, anime_count, anime_mean_score, anime_episodes_watched, manga_count, manga_mean_score, manga_chapters_read, favorites: { anime, manga, characters, staff, studios } }
// Public profile data only, read-only, never persisted (FR-66/FR-67). favorites uses the same shape as GET /favorites.
```

## GET /friends

```
Response: { "friends": [ Friend ], "hidden": [ { anilist_user_id, name, avatar_url } ], "synced_at": string | null, "mutual_only": boolean }
Friend = { anilist_user_id, name, avatar_url, source: "anilist" | "manual",
           list_status: "pending" | "fetching" | "fetched" | "private", fetched_at: string | null,
           anime_count, manga_count }
// FR-70/FR-79. Local only, no AniList call. Hidden friends are only in `hidden`, by name. Counts are entries on their stored list, any status.
// list_status: "fetching" while a fetch of theirs runs, else "private" when private_at is set (FR-73), else "fetched" when fetched_at is set, else "pending".
```

## POST /friends

```
Body: { "username": string }
Response: Friend
// FR-70/FR-71: looks the user up on AniList, fetches both lists without the token, stores them as a manual friend. Adding someone who's already a friend (from AniList, hidden, or manual) makes them a visible manual friend and refreshes them.
// 400 if it's your own account; 404 "No AniList user named X"; 403 "X's list is private on AniList" (FR-73); 502 if AniList can't be reached. A new friend is stored on none of these failures.
```

## POST /friends/sync

```
Response: same as GET /friends
// FR-79: reads your following list, then adds, updates and removes friends from AniList. Fetches no lists. Without the token, except that mutual_only reads your own following list with it (isFollower), falling back to your followers list without it.
// 502 if AniList can't be reached part-way; nothing changes.
```

## PUT /friends/settings

```
Body: { "mutual_only": boolean }
Response: { "mutual_only": boolean }
// FR-79. Local only; the frontend runs POST /friends/sync right after, so the list follows the new setting.
```

## POST /friends/{anilist_user_id}/unhide

```
Response: same as GET /friends, plus "unhidden": boolean
// FR-70: un-hides, then runs a friend sync: unhidden is true if they're back (you still follow them), false if the sync removed the record. 404 if they aren't hidden. If AniList can't be reached, they stay unhidden (true).
```

## POST /friends/{anilist_user_id}/refresh

```
Response: Friend
// FR-71: refetches both lists and replaces the stored ones, even for a friend marked private. 404 if not a friend (or hidden); on 403/502 the previously stored list is kept (FR-73), and a 403 marks them private.
```

## DELETE /friends/{anilist_user_id}

```
Response: { "deleted": true, "hidden": boolean }   // 404 if not a friend (or already hidden)
// FR-70. Local only, nothing changes on AniList. A manual friend is removed with (ON DELETE CASCADE) their stored entries; a friend from AniList is hidden (hidden: true), and their stored entries are deleted.
```

## GET /friends/{anilist_user_id}/compare?media_type=anime|manga

```
Response: {
  friend: Friend, media_type,
  overlap: number, taste_match: number | null, skew: number | null, rarely_disagree: boolean,
  disagreements: [ Shared ], agreements: [ Shared ], shared: [ Shared ],
  they_completed: [ { anilist_id, media_type, title, title_english, cover_url, their_score, your_status: string | null } ],
  you_completed: [ { anilist_id, media_id, media_type, title, title_english, cover_url, your_score, their_status: string | null } ],
  genres: [ { genre, your_avg: number | null, your_count, their_avg: number | null, their_count } ]
}
Shared = { anilist_id, media_id, media_type, title, title_english, cover_url, your_score, their_score, delta,
           rated_in_app: boolean, your_criteria: Criteria | null, their_status: string, their_progress: number | null }
Criteria = { story, characters, animation, audio, enjoyment }   // anime; manga has no audio, and animation is Art
// delta = your_score − their_score. FR-82: your_criteria is null when rated_in_app is false (rating.source = 'anilist').
// rarely_disagree (FR-75): overlap > 0 and every shared title's gap is at most 0.5.
// disagreements: the 5 largest gaps whatever their size; agreements: up to 5 of the rest, smallest gap first.
// they_completed / you_completed: every qualifying title, ordered by that score, then popularity, then anilist_id (FR-77); the frontend shows the first 10.
// FR-72–FR-78, computed from the stored lists. FR-81: first fetches the friend's lists (two requests) if they've never been fetched or are older than 24 hours, unless the friend is marked private; if that fetch fails, the stored copy is used.
// 404 if not a friend (or hidden); 403 "X's list is private on AniList" for a private friend with no stored list; 502 if the fetch failed and nothing is stored. taste_match is null when overlap < 5; skew is null when overlap is 0.
// shared is ordered by your_score descending; the frontend re-sorts it (FR-76). genres is ordered by your_count + their_count descending.
```

## GET /media/by-anilist/{anilist_id}/friend-scores

```
Response: { "scores": [ { anilist_user_id, name, avatar_url, status, score } ] }
// FR-78: friends who have scored this title, highest score first, from the stored lists only (so friends whose lists aren't fetched yet, and hidden friends, never appear). Works for titles not in the library too.
```

# 8. Frontend Design Brief (handoff for Claude Design)

The existing app has an established dark visual identity — keep it, don't redesign from scratch. Carry these tokens forward:

- Typeface: Overpass (display/body) + Overpass Mono (data/labels).
- Palette: near-black background (#0c0d10 / #060709), surfaces in #131419–#1e212a, single blue accent (#34a8f0 family), muted text scale (#eef1f6 → #474d5a). Friend Comparison adds one more: `--friend`, a soft amber (#f2a65a, with `--friend-dim` for gradients), used only for the friend's side (FR-83).
- Frameless custom titlebar (minimize/maximize/close), since this runs inside a Tauri window, not a browser chrome.

**Bottom nav is five tabs, matching AniList's own client apps: Anime, Manga, Discover, Feed, Profile. Search (AniList catalog, for import) and Rankings (anchored-score sort) are NOT separate tabs — they live inside the Anime/Manga tabs as described below. None of these five tabs is reachable until login (FR-47) — the Login/Connect screen is what a fresh launch actually shows first. On a page reached from a tab (a title's detail view, a user, character, staff or studio page, Insights, the rating stepper) the tab you came from stays highlighted; a friend comparison, and any page opened from it, highlights Profile, where Friends lives. Back restores the tab that was highlighted on the page you return to.**

**Screens to design:**

- **Login / Connect (FR-47)** — the app's actual entry point. AniList branding, a single "Connect AniList" action that opens the OAuth flow in-window (FR-22), no way to reach any tab behind it. Shown on every fresh launch until login completes, and again immediately after logout (FR-48).

- **Anime tab / Manga tab** — each is that media type's Library: collapsible accordion sections by status (FR-18, FR-30), with a sort icon (by title/progress/score/updated — this is where the anchored-overall Rankings sort lives, FR-13) and a search-within-tab field (FR-31) at the top. Completed splits into format sub-sections (FR-29). Each entry shows progress ("12 / 24 eps") with a quick +1 control, and a score badge only if rated.
- **Discover tab** — horizontal shelves per FR-32: Currently Trending, Current/Upcoming Season, Top Ranked, Top Ranked Movies, Popular — scoped to whichever of Anime/Manga is active. This is also where AniList-catalog Search lives (a search icon opens it, per FR-1/FR-2) — separate from the search-within-tab field on Anime/Manga.
- **Feed tab** — Your/Following/Global picker (FR-34), activity list with cover thumbnails and relative timestamps (FR-35), tap-through to detail.
- **Rating stepper** — must visibly differ by media type: 5 steps with "Animation" + "Audio" for anime, 4 steps with "Art" and no Audio step for manga. Score control snaps to 0.5 steps. Anchors panel beside each step, headed "Your calibration — how you scored [Criterion] on similar anime/manga. Place this title against them," with a genre/all pill switch (FR-8). Opened from a title's score pill, not a separate tab. Rate Series mode (FR-44–46) chains this across a title's prequel/sequel entries, with a season-level back control distinct from the per-criterion back.
- **Profile tab** — avatar and banner (the banner across the top with the avatar overlapping its bottom edge; no banner area when the account has none), stats block (FR-42: total anime/manga, days watched, chapters read, followers/following), one favorite shelf per non-empty category — Anime, Manga, Characters, Staff, Studios (FR-40) — settings/gear icon opening Account/Sync (FR-43: connect/disconnect, Pull button, connection status).
- **Detail view (anime/manga)** — full criteria breakdown, series info stats, tags vs. genres, producers, relations, rankings/popularity, favorite heart, edit/delete. Two states: in-library shows quick status/progress/score pills; not-yet-imported (reached via Discover, FR-52) shows an "Add to Library" action with a status picker instead.
- **Character / Staff detail** — cover, favorite heart, member-favorites count, bio, voice-actors-by-language (characters) or filmography (staff), appearances (FR-38).
- **Catalog Search categories (FR-64)** — a category picker on the Search screen: Anime, Manga, Characters, Staff, Studios, Users.
- **Studio detail (FR-65)** — name, studio type, member-favorites count, favorite heart, list of works.
- **User profile (FR-66)** — avatar, banner, about, stats block, favorite shelves. Read-only (FR-67), plus a "Compare lists" button (FR-70).
- **Friends (FR-70, FR-79)** — a section on the Profile tab: "Sync from AniList" with last-synced time, the everyone / mutual-only setting with a tooltip, add-by-username field, one row per friend with avatar, name, source, list status, Refresh, Remove, Compare, and a collapsed "Hidden (N)" list with Unhide. Not a sixth bottom-nav tab.
- **Friend comparison (FR-74–FR-83)** — Anime/Manga toggle; overlap, taste match (with a tooltip explaining it) and skew; biggest disagreements and strongest agreements side by side; a sortable shared-titles table; rows in both cards and the table expand in place to your criteria and the friend's status (FR-82); "They've completed, you haven't" and "You've completed, they haven't" as top-10 grids with Show all; side-by-side genre averages. You in `--accent`, the friend in `--friend` (FR-83). The detail view shows friends' scores for that title.
- **Insights (optional)** — genre breakdown, score distribution, community-score divergence.

*Design deliverable: a single-file HTML/React mockup (or component set) using the API contract in Section 7 as the data shape, so wiring it to the real backend afterward is a drop-in, not a rewrite.*

# 9. Build Milestones

| **Phase** | **Deliverable** |
| --- | --- |
| M1 | Frontend design pass in Claude Design against Section 8, using mock data shaped per Section 7. |
| M2 | Backend skeleton + SQLite schema (Section 5); /search and /media working against AniList only. |
| M3 | Rating endpoints with server-side media-type-aware overall computation; anchors endpoint with media_type + genre filtering; status/progress endpoints (4.5). |
| M4 | AniList OAuth login, Pull, and automatic push sync (4.6). |
| M5 | Discover, Feed, and Characters/Staff/Favorites endpoints (4.7–4.9) — all live pass-throughs to AniList, no schema changes needed. |
| M6 | Wire the designed frontend (Anime/Manga tabs, Discover, Feed, Rating stepper, Profile, Account/Sync) to the real backend, replacing mock data with live API calls. |
| M7 | Accordion polish, format sub-splits, search/sort within tabs, detail view, delete, persistence polish. |
| M8 (optional) | Insights dashboard. |
| M9 | Studios & Users (4.11): /studios/search, /studios/{id}, /users/search, /users/{id}, and the Studios/Users categories in catalog Search — live pass-throughs to AniList, no schema changes. |
| M10 | Friend Comparison (4.12): `friend` / `friend_entry` / `friend_sync` tables (additive, CREATE TABLE IF NOT EXISTS, plus the additive `friend` migration in §5), /friends endpoints, friends synced from your AniList following (FR-79), the background list queue (FR-80), the comparison view, and friends' scores on the detail view. |

# 10. Appendix: Reference App Feature Set (MyAniList)

You asked for MyAniList (a third-party AniList client) to be used as a reference for "all the ways this app should be usable." Mapping its feature list against this PRD below — partly so Library (4.5) has a clear reference point, and partly to be explicit about what's staying out of v2 scope, since it was exactly this kind of quiet scope growth (adding capabilities without updating the data model) that caused the original manga sync bug.

| **MyAniList Feature** | **Status** | **Notes** |
| --- | --- | --- |
| Track anime/manga lists | In scope — v2 | This is Library (4.5): status + progress tracking, now kept in sync with your real AniList.co list via 4.6. |
| Log into AniList.co and sync your real lists | In scope — v2 | This is now Section 4.6 (AniList Account Sync) — OAuth login, Pull on demand, automatic push of status/progress/overall. |
| Discover/search series, characters, voice actors | In scope — v2 | Series browsing is Discover (4.7); catalog search stays the import Search flow. Character/staff search and detail is now 4.9. |
| Follow friends, see their lists/favorites | In scope — v2 | Activity is Feed (4.8) — live pass-through to AniList's Activity API, scoped to the connected account's own follow graph. Comparing your list against a friend's public list is 4.12. Following/unfollowing stays out of scope (FR-67). |
| Mark series/characters/VAs as favorites | In scope — v2 | Section 4.9 — favoriting is a live toggle on the real AniList account, not local state. |
| Custom app theme / list style / icon | Non-goal — v2 | The app has one established dark theme (Section 8) that's intentionally being carried forward, not made user-configurable, for this rebuild. |
| Live broadcast/episode-airing notifications | Non-goal — v2 | Needs background polling of AniList's airing schedule plus OS-level notifications — separate subsystem, not part of the Rust/SQLite/Tauri core being rebuilt here. |

*If any "Non-goal" row above is actually wanted for v2, say so explicitly and it goes into the data model and API contract before frontend work starts on it — that's the discipline this rebuild is meant to enforce.*

# 11. Open Questions

- Export to MAL XML: resolved — dropped entirely, not reshaped into an AniList-id-based export. AniList sync now does the real syncing job; the v1-era MAL export was never carried into this rebuild's codebase.
- FR-26 vs FR-47 boundary: resolved — AniList access tokens are valid for a full year with no refresh token, so a stored token doesn't expire from being offline; FR-47's gate only re-triggers on an actual logout (FR-48), never from a network gap alone, however long. The only remaining edge case: what happens on the rare occasion a year-old token finally does expire while the app is offline — confirm the app should surface a clear re-login prompt rather than a generic error.
