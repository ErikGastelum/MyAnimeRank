# Plan: M7 — Accordion polish, format sub-splits, search/sort, detail view, delete, persistence

**Source PRD**: docs/PRD.md (Delivery Milestones, M7)
**Complexity**: Medium

## Summary

M7 is explicitly an audit-first milestone. Most of the listed items were already built during M6 against real data and are correct as-is; only three areas have confirmed, real bugs, plus one confirmed architectural gap (list-entry ID tracking) needed to satisfy the user's decision on delete semantics. This plan implements only what's actually broken/missing, verified against the real running backend and the user's real 35-title AniList library — nothing here is a rewrite of already-working code.

## Audit Findings (confirmed against real code + real data, not assumptions)

| Item | Status | Evidence |
|---|---|---|
| Accordion default-open (FR-18/FR-30) | **Already correct** | `defaultExpanded()` (index.html:488) opens watching/rewatching or reading/rereading independent of row count; `secExpanded()`/`toggleSec` persist per-key overrides via localStorage. No status-with-zero-entries edge case breaks it — `sections` always includes every status header with its count (index.html:1121). |
| Search/sort within tabs (FR-31) | **Already correct, needs live re-verify only** | `libraryView()` filters/sorts the real `library` array client-side (index.html:1088-1095); no mock dependency. |
| Tags vs. genres vs. producers | **Already correct** | `detailView()` reads `it.tags`/`it.genres`/`it.producers` directly (index.html:1683, 1687), all three are real `SearchResult`/`MediaOut` fields already populated by `anilist._map()`. |
| **Format sub-splits (FR-29)** | **CONFIRMED BROKEN** | `FORMAT_ORDER` (index.html:211) uses display strings (`'Movie'`, `'TV Short'`, `'One Shot'`); real `format` values are raw AniList enum strings (`'MOVIE'`, `'TV_SHORT'`, `'ONE_SHOT'`) plus `'MUSIC'` (missing from the anime list entirely). Confirmed live: `Kimi no Suizou wo Tabetai` and `Jujutsu Kaisen 0` (format `MOVIE`) are **silently absent** from the real Completed accordion right now — `groups.filter(g=>g.items.length)` drops any format group with zero string-matches, and `'MOVIE' !== 'Movie'`. |
| **Detail view community/rankings/relations (FR-49)** | **CONFIRMED entirely mock** | `detailView()` unconditionally calls the mock `community(it)` (index.html:441, called at 1617) — no `BACKEND_AVAILABLE` branch, never calls `GET /media/{id}/community` at all. Rankings, relations, and characters/recommended shelves shown for a real title are 100% fabricated. |
| **Favorite heart in detail view (FR-38)** | **CONFIRMED entirely mock** | `H.toggleFav` (index.html:648) only flips a local `it._fav` boolean and re-renders — never calls `api.toggleFavorite()`, and `_fav` is never initialized from the real `GET /favorites` list when a detail view opens. The heart currently lies about favorite state for every real title. |
| **Delete's AniList-side effect** | **CONFIRMED local-only; user decided it should also delete remotely** | `DELETE /media/{id}` (main.py:354) is a synchronous function calling only `database.delete_media()` — no AniList mutation. **User confirmed**: delete should also remove the entry from the real AniList list, not just locally. This surfaced a deeper gap: `anilist_list_entry_id` (the column that would let us call AniList's delete mutation) is a schema column that **is never actually populated anywhere** — `sync.pull()` explicitly excludes `list_entry_id` from imported data (sync.py:44), and `push_list_entry()`'s response is never persisted back. This needs building, not just calling an existing piece. |
| Persistence polish | **Very likely already correct by design, needs live re-verify only** | Auth (`anilist_auth` table), library rows, and `pending_conflict` flags are all SQLite-backed (survive process restart by construction); frontend just re-fetches everything at boot (`boot()`, index.html:1721) rather than caching real data in localStorage. `persistLibrary`/`LIB_KEY` localStorage functions are mock-mode-only artifacts from before M6 and don't affect real-mode persistence. |
| Quick status/progress/score pills in detail view | Uses the same `PUT /media/{id}/status\|progress\|rating` endpoints validated live in M6 | Needs a quick live click-through from the detail-view-specific entry points (`setStatus`, `dIncProgress`, `editCurrent`), not new code — included in validation, not implementation. |

## Patterns to Mirror

| Category | Source | Pattern |
|---|---|---|
| Backend mutation gated by auth | `main.py:354` (`Depends(require_auth)`) | New delete flow keeps the same dependency |
| GraphQL mutation with omitted-vs-null care | `anilist.py:296` `push_list_entry()` docstring | Any new mutation variables dict follows the same "omit rather than null" rule where relevant |
| Frontend real/mock branch | `frontend/index.html` `H.openFavTitle`, `H.openPerson` (this session) | New `community` fetch and `toggleFav` follow the same `if(BACKEND_AVAILABLE){ try{...}catch{...} } else { mock }` shape |
| Backend field never populated → fix at the source that should set it | `sync.py` FR-24 gap found in M4 (push wiring) | `anilist_list_entry_id` gap fixed the same way: capture it where the ID first becomes known (Pull import, and Push's create-response) |

## Files to Change

| File | Action | Why |
|---|---|---|
| `anilist.py` | UPDATE | Add `delete_list_entry()` mutation; make `push_list_entry()` return the list entry id from AniList's response |
| `sync.py` | UPDATE | `pull()`'s import path persists `anilist_list_entry_id` instead of excluding it; `push_now()` persists the id AniList returns after a successful push; add `delete_remote_entry()` |
| `database.py` | UPDATE | Add `set_anilist_list_entry_id(media_id, entry_id)`; `delete_media()` unchanged (still pure local delete, just called later in the flow now) |
| `main.py` | UPDATE | `delete_media` becomes `async`; calls `sync.delete_remote_entry(media_id)` before `database.delete_media()`; returns an error (not a silent partial success) if the AniList call fails so local/remote never silently diverge |
| `models.py` | No change expected | `DeleteResult` shape (`{"deleted": bool}`) stays as-is |
| `frontend/index.html` | UPDATE | Fix `FORMAT_ORDER` matching (map raw enum → display label instead of string identity); wire `detailView()`'s community block to real `GET /media/{id}/community`, cached by anilist_id; wire `H.toggleFav` to real `api.toggleFavorite()` + initialize `_fav` from loaded favorites on `openDetail` |

## Tasks

### Task 1: Fix format sub-split matching (frontend-only)
- **Action**: Replace `FORMAT_ORDER`'s Title-Case-string-identity matching with a raw-enum → label lookup (e.g. `FORMAT_LABEL = {TV:'TV', TV_SHORT:'TV Short', MOVIE:'Movie', SPECIAL:'Special', OVA:'OVA', ONA:'ONA', MUSIC:'Music', MANGA:'Manga', NOVEL:'Novel', ONE_SHOT:'One Shot'}`); group by raw `r.format`, display via the lookup, keep AniList's canonical ordering, fall back to an "Other" group for any unmapped value.
- **Mirror**: Existing `STATUS_LABEL` raw-key → display-label dict pattern already used one line above `FORMAT_ORDER`.
- **Validate**: Re-render Completed/anime accordion against the real library; confirm `Kimi no Suizou wo Tabetai` and `Jujutsu Kaisen 0` (both `MOVIE`) now appear under a "Movie" group.

### Task 2: Wire detail view's community data to the real endpoint
- **Action**: On `openDetail`, if `BACKEND_AVAILABLE` and the item has an `anilist_id`, fetch `GET /media/{id}/community` (cache by anilist_id so repeat opens don't re-fetch) and map `CommunityResponse`'s fields (`relations`, `characters`, `recommended`, `popularity`, `favourites`, `rank_popular`) onto the shape `detailView()`/`comm.*` already expects, replacing the `community(it)` mock call in real mode only.
- **Mirror**: `H.openPerson`'s real/mock branch structure (this session's M6 work).
- **Validate**: Open a real title's detail view, confirm rankings/relations show real AniList data (not the `120+(item.id%400)` mock formula), confirm a title with no relations doesn't crash (empty array, not null).

### Task 3: Wire the favorite heart to the real toggle
- **Action**: On `openDetail`, initialize `it._fav` from the already-loaded favorites list (membership by `anilist_id` + `media_type`, calling `api.getFavorites()` once and caching, same as Profile's favorites). On `H.toggleFav`, call `api.toggleFavorite(type, anilist_id)` and update the cached favorites list on success; keep local-only fallback when `!BACKEND_AVAILABLE`.
- **Mirror**: `loadProfileData()`'s existing favorites fetch/cache.
- **Validate**: Toggle a real title's heart on, confirm `GET /favorites` reflects it; toggle off, confirm it's removed; re-open the detail view, confirm the heart's initial state matches the real backend without needing a toggle first.

### Task 4: Populate `anilist_list_entry_id` (prerequisite for Task 5)
- **Action**: `sync.pull()` writes `entry["list_entry_id"]` into `anilist_list_entry_id` on import instead of excluding it. `anilist.push_list_entry()`'s mutation response includes the entry `id`; return it; `sync.push_now()` calls a new `database.set_anilist_list_entry_id(media_id, entry_id)` after a successful push.
- **Mirror**: `sync.py`'s existing `database.set_last_synced()` call-after-success pattern in `push_now()`.
- **Validate**: Directly inspect `library.db` after a fresh `POST /sync/pull` — confirm `anilist_list_entry_id` is non-null for all 35 real titles. Push a brand-new local entry, confirm its `anilist_list_entry_id` gets populated after the first successful push (was NULL before).

### Task 5: Delete also removes the AniList list entry
- **Action**: Add `anilist.delete_list_entry(access_token, list_entry_id)` (calls `mutation($id: Int){ DeleteMediaListEntry(id: $id){ deleted } }`). Add `sync.delete_remote_entry(media_id)`: looks up the local row, if `anilist_list_entry_id` is set calls the AniList delete and raises on failure (so `main.py` can 502/500 rather than silently local-delete on a failed remote call); if it's `None` (never synced), skips the AniList call entirely (nothing to delete remotely) and proceeds. `main.py`'s `delete_media` becomes `async def`, calls `sync.delete_remote_entry` first, then `database.delete_media`.
- **Mirror**: `sync.push_now()`'s `httpx.HTTPError` catch-and-reraise pattern (main.py's route layer decides the HTTP status, sync.py just propagates).
- **Validate**: **Live-test against a real, disposable test import** (add a throwaway title to planning status, confirm it appears on the real AniList list via a raw GraphQL query, delete it through the app, confirm it's gone from both `library.db` AND the real AniList account). Also test the "never-synced local entry" path doesn't error.

### Task 6: Live-verify (no code expected) — accordion, search/sort, persistence, quick pills
- **Action**: No changes anticipated; run through each against the real 35-title library and report pass/fail. If something's actually broken here despite the audit, fix it as a small addendum, not a redesign.
- **Validate**: Manual jsdom-driven click-throughs (same technique as M6) plus a real server-restart to confirm auth/library/conflict-flag persistence.

## Validation

```bash
# backend
python -c "import main"  # syntax/import check
# live checks against the running server + real account, via python3 -c httpx one-liners
# frontend
node --check <extracted-script>  # syntax check after each edit
# jsdom click-through scripts per task, same methodology as M6 (see project_jsdom_frontend_testing memory)
```

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| Delete-from-AniList mutation fails after local delete already happened (local/remote diverge) | Medium if not sequenced carefully | Explicitly sequence AniList delete *before* local delete; propagate failure as an error, never partial-succeed |
| A pre-M7 real title has no `anilist_list_entry_id` yet (never synced) and user tries to delete it | Medium — likely true for locally-imported-but-never-pushed titles | Treat `anilist_list_entry_id IS NULL` as "nothing to delete remotely," proceed with local-only delete — this is not a bug, it's correct (there's genuinely nothing on AniList to remove) |
| Testing delete-from-AniList against the real account risks losing real data if something goes wrong | High impact if mishandled | Use a throwaway test import (not an existing real title) for the live validation in Task 5, exactly as done for OAuth/Pull/Push testing in M4 |
| `FORMAT_ORDER` fix might miss an AniList format enum value not yet seen in the real library | Low | Fall back to an "Other" group for any unmapped raw format value, rather than silently dropping it again |

## Acceptance
- [ ] Format sub-split shows all real Completed titles, including non-TV formats
- [ ] Detail view's rankings/relations reflect real `GET /media/{id}/community` data
- [ ] Favorite heart reflects and updates real `GET/POST /favorites` state
- [ ] `anilist_list_entry_id` populated for all synced real titles
- [ ] Delete removes the entry from both local DB and the real AniList account, tested against a disposable test entry only
- [ ] Accordion/search/sort/persistence/quick-pills confirmed correct against real data (or fixed if not)
