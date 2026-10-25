# Plan: M8 — Insights Dashboard (FR-60 through FR-63)

**Source PRD**: docs/PRD.md (Section 4.4, Section 7 `GET /insights`, Section 8 IA)
**Complexity**: Small–Medium

## Summary

Section 4.4 was rewritten from a vague bullet list into four concrete FRs with an explicit response shape in Section 7. The frontend already has an `Insights` entry point wired (accessible via the gear icon in Profile → Account/Sync, `H.openInsights`/`insightsView()` already exist) but it currently renders **mock data that doesn't match the new spec at all**: it has no `top_criterion` or `community_divergence` sections whatsoever (net-new UI, not a data swap), and its genre/distribution sections are flat, unsplit-by-media-type arrays where the real spec requires anime/manga kept strictly separate (FR-60, FR-63). This is a single new read-only endpoint over existing tables (`media` + `rating`, joined) plus a real rebuild of the Insights screen's content — no schema changes, no AniList calls, no sync concerns, matching the milestone's explicit "keep it simple" framing.

## Patterns to Mirror

| Category | Source | Pattern |
|---|---|---|
| Read-only aggregation over media+rating | `database.py` `get_rankings()` | JOIN media/rating, filter/sort in Python after fetch, no ORM |
| Criteria set exclusion (manga has no Audio) | `database.py:173-174` `ANIME_CRITERIA`/`MANGA_CRITERIA` | Reuse these exact tuples — never invent a new criteria list |
| Nested Pydantic response shapes | `models.py` `CommunityResponse`/`CommunityRelation`/etc (M5) | Same nested-model style for `InsightsResponse` |
| Route pattern | `main.py` `GET /library`, `GET /rankings` | `Depends(require_auth)`, plain `def` (no `await`, pure local reads) |
| Frontend real/mock branch | `frontend/index.html` `H.openPerson`, `loadCommunity()` (M6/M7) | Same `if(BACKEND_AVAILABLE){ fetch, cache, map onto render shape } else { mock }` structure |

## Files to Change

| File | Action | Why |
|---|---|---|
| `database.py` | UPDATE | Add `get_insights()` — one JOIN query, then four pure-Python aggregations (by_genre, top_criterion, community_divergence, distribution) |
| `models.py` | UPDATE | Add `InsightsResponse` + nested models (`GenreInsight`, `CriterionInsight`, `DivergenceTitle`, `CommunityDivergence`, `DistributionBucket`) matching Section 7's shape exactly |
| `main.py` | UPDATE | Add `GET /insights` route, `Depends(require_auth)` |
| `frontend/index.html` | UPDATE | Rebuild `insightsView()`'s content to render all four sections split correctly by media type where required; wire real fetch via a new `api.getInsights()` + cache; keep the mock `INSIGHTS` object as the offline fallback shape (restructured to match, so the mock/real branch doesn't need two different render paths) |

## Tasks

### Task 1: `database.get_insights()`
- **Action**: One query — `SELECT m.id, m.media_type, m.genres, m.avg_score, m.title, r.story, r.characters, r.animation, r.audio, r.enjoyment, r.overall FROM media m JOIN rating r ON r.media_id = m.id` (only rated titles participate, same precondition `get_rankings()` already uses). Then in Python:
  - **by_genre**: parse each row's `genres` JSON list; accumulate `(genre, media_type) -> [overall,...]`; emit `{genre, media_type, avg_overall, count}` per group. Never merge anime/manga sums for the same genre name.
  - **top_criterion**: split rows by media_type; for anime use `ANIME_CRITERIA`, for manga use `MANGA_CRITERIA` (imported from the existing tuples — audio is never included in the manga average, not treated as 0); average each criterion across that media type's rows; pick the max; return `null` for a media type with zero rated titles (not a crash, not a 0.0 placeholder).
  - **community_divergence**: for every row where `avg_score is not None` (a title can lack a community score — skip it, don't treat missing as 0), `delta = overall - avg_score`; average all deltas -> `avg_divergence`; sort descending for `most_above` (top 5), ascending for `most_below` (top 5). Fewer than 5 rated+scored titles just returns however many exist, not padded.
  - **distribution**: bucket each row's `overall` into an 18-bucket 0.5-wide scale from 1.0 to <10.0 plus a final bucket for exactly 10.0 (`overall` is a computed average of 0.5-stepped criteria, so it is *not* itself naturally 0.5-aligned — this is genuine bucketing, not a group-by-exact-value). Compute separately for anime rows and manga rows; always emit all 18 buckets (count 0 where empty), never omit empty buckets.
- **Mirror**: `get_rankings()`'s JOIN-then-Python-aggregate style; `ANIME_CRITERIA`/`MANGA_CRITERIA` reuse (no new criteria list).
- **Validate**: Direct Python call against the real `library.db`, sanity-checked against a manual `sqlite3` query for at least one genre and the anime top criterion.

### Task 2: `models.py` — `InsightsResponse`
- **Action**: Add models matching Section 7's exact shape: `GenreInsight{genre, media_type, avg_overall, count}`, `CriterionInsight{criterion, avg_score}`, `DivergenceTitle{media_id, title, your_score, avg_score, delta}`, `CommunityDivergence{avg_divergence, most_above: list[DivergenceTitle], most_below: list[DivergenceTitle]}`, `TopCriterionResponse{anime: CriterionInsight|None, manga: CriterionInsight|None}`, `DistributionBucket{bucket, count}`, `InsightsResponse{by_genre: list[GenreInsight], top_criterion: TopCriterionResponse, community_divergence: CommunityDivergence, distribution: dict-like {anime: list[DistributionBucket], manga: list[DistributionBucket]}}`.
- **Mirror**: `CommunityResponse`'s nested-model style (M5, models.py).
- **Validate**: `python3 -c "import models"` + response_model round-trips cleanly against `get_insights()`'s dict output.

### Task 3: `GET /insights` route
- **Action**: `@app.get("/insights", response_model=InsightsResponse, dependencies=[Depends(require_auth)])` calling `database.get_insights()` directly (plain `def`, no `await` — pure local read, no AniList call per the milestone's explicit scope).
- **Mirror**: `GET /library`/`GET /rankings` route style.
- **Validate**: Live `httpx.get` against the running server with the real 35-title account; manually eyeball the genre breakdown and divergence outliers against titles I know the real ratings for (e.g., confirm a title I rated much higher than its `avg_score` shows up in `most_above`).

### Task 4: Frontend — real Insights screen
- **Action**: Add `api.getInsights()`; rebuild `insightsView()`'s markup to add the two currently-missing sections (Top Criterion: two small cards, Anime/Manga side by side, "no ratings yet" state when null; Community Divergence: aggregate delta number + two 5-row lists, Most Above/Most Below, each showing title/your score/community score/delta) and restructure the existing genre-breakdown and distribution sections to split by media type (two sub-lists / two bar charts) instead of one blended list. Keep `INSIGHTS` mock object as the `!BACKEND_AVAILABLE` fallback, reshaped to the same by-media-type structure so `insightsView()` has one render path, not two.
- **Mirror**: `loadCommunity()`/`H.openPerson`'s real/mock branch + cache pattern (M6/M7).
- **Validate**: jsdom click-through opening Insights against the real running backend; confirm all four sections render with real numbers, confirm the mock fallback still renders standalone with no backend.

## Validation

```bash
python -c "import main"   # syntax/import check
# direct httpx GET /insights against the real running server + real 35-title account
# manual sqlite3 cross-check of at least one genre average and one divergence outlier
node --check <extracted-script>   # frontend syntax check
# jsdom click-through: nav to Profile -> gear -> Insights, both real and mock-fallback modes
```

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| Manga has very few (or zero) rated titles, producing a `null` top_criterion or an empty divergence list | Medium — real account may be anime-heavy | Explicitly handle `null`/empty as valid states in both backend (never crash on empty aggregation) and frontend (show "not enough data yet" rather than blank/broken layout) |
| `overall` isn't 0.5-aligned, easy to mis-bucket with naive `round()` | Medium if not careful | Use proper interval bucketing (floor-based bucket index), not exact-value grouping |
| Titles with no `avg_score` (rare AniList data gaps) skewing divergence | Low | Explicitly filter `avg_score is not None` before computing deltas, same defensive pattern as FR-57's null-episode handling elsewhere in the app |

## Acceptance
- [ ] `GET /insights` returns real, correctly-shaped data with zero AniList calls
- [ ] Genre breakdown and score distribution are never blended across anime/manga
- [ ] Top criterion computed separately per media type, manga never includes Audio
- [ ] Community divergence shows aggregate + real top-5-above/top-5-below outliers
- [ ] Insights screen renders all four sections against the real 35-title account, verified as plausible (not just HTTP 200)
- [ ] Mock fallback still works standalone with the restructured shape
