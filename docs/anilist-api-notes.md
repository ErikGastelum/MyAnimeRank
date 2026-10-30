# AniList API Notes

> Check this doc before assuming a field exists or a query shape works. If something isn't covered here, confirm via live introspection/testing as normal, then ADD the finding here before finishing the task — this file is only useful if it stays current.

Endpoint: `https://graphql.anilist.co`. Docs: https://anilist.gitbook.io/anilist-apiv2-docs/. Explorer: https://anilist.co/graphiql.

---

## A. Schema reference

Scoped to the types this app actually touches. Field name → type; notable args in parens.

### Media
```
id: Int!                    idMal: Int
title: MediaTitle           type: MediaType (ANIME | MANGA)
format: MediaFormat         status(version): MediaStatus
description(asHtml)         startDate/endDate: FuzzyDate
season: MediaSeason         seasonYear: Int
episodes: Int                duration: Int
chapters: Int                 volumes: Int
countryOfOrigin: CountryCode  isLicensed: Boolean
source(version): MediaSource  updatedAt: Int
coverImage: MediaCoverImage   bannerImage: String
genres: [String]              synonyms: [String]
averageScore: Int              meanScore: Int
popularity: Int                trending: Int
favourites: Int                 tags: [MediaTag]
relations: MediaConnection
characters(sort, role, page, perPage): CharacterConnection
staff(sort, page, perPage): StaffConnection
studios(sort, isMain): StudioConnection
isFavourite: Boolean!            isAdult: Boolean
nextAiringEpisode: AiringSchedule
mediaListEntry: MediaList        <- the CONNECTED VIEWER's own list entry, not arbitrary
rankings: [MediaRank]            siteUrl: String
```

### MediaTitle
```
romaji(stylised) / english(stylised) / native(stylised) / userPreferred: String
```

### MediaCoverImage
```
extraLarge / large / medium / color: String
```

### Character
```
id: Int!                  name: CharacterName
image: CharacterImage     description(asHtml): String
gender: String             dateOfBirth: FuzzyDate
age: String                <- STRING here, unlike Staff.age (Int) -- see gotcha #5
bloodType: String          isFavourite: Boolean!
siteUrl: String            favourites: Int
media(sort, type, onList, page, perPage): MediaConnection
```

### CharacterEdge (appears on CharacterConnection.edges)
```
node: Character            id: Int
role: CharacterRole        <- ENUM (MAIN | SUPPORTING | BACKGROUND)
voiceActors(language, sort): [Staff]
voiceActorRoles(language, sort): [StaffRoleType]
media: [Media]              <- FLAT list, NO pagination args -- see gotcha #3
favouriteOrder: Int
```

### CharacterConnection
```
edges: [CharacterEdge]     nodes: [Character]     pageInfo: PageInfo
```

### Staff
```
id: Int!                    name: StaffName
languageV2: String           image: StaffImage
description(asHtml): String  primaryOccupations: [String]
gender: String                dateOfBirth: FuzzyDate
dateOfDeath: FuzzyDate         age: Int          <- Int here, unlike Character.age (String)
yearsActive: [Int]             homeTown: String
bloodType: String              isFavourite: Boolean!
siteUrl: String                favourites: Int
staffMedia(sort, type, onList, page, perPage): MediaConnection
characters(sort, page, perPage): CharacterConnection
characterMedia(sort, onList, page, perPage): MediaConnection   <- exists, unexplored, see gotcha #16
```

### StaffEdge (appears on StaffConnection.edges)
```
node: Staff                id: Int
role: String                <- plain String here, unlike CharacterEdge.role (enum) -- see gotcha #4
favouriteOrder: Int
```

### Studio
```
id: Int!                    name: String!
isAnimationStudio: Boolean!  <- NO image/logo field anywhere -- see gotcha #6
media(sort, isMain, onList, page, perPage): MediaConnection
siteUrl: String              isFavourite: Boolean!
favourites: Int
```

### StudioEdge
```
node: Studio    id: Int    isMain: Boolean!    favouriteOrder: Int
```

### User
```
id: Int!                    name: String!
about(asHtml): String        avatar: UserAvatar
bannerImage: String           isFollowing/isFollower/isBlocked: Boolean  <- VIEWER-relationship, not target's data
favourites(page): Favourites  <- resolves for ARBITRARY users, not just Viewer -- see gotcha #8
statistics: UserStatisticTypes  <- has separate .anime / .manga sub-objects, see gotcha #9
siteUrl: String                createdAt: Int    updatedAt: Int
moderatorRoles: [ModRole]      previousNames: [UserPreviousName]
```
`Page.users(search: String)` and `Page.studios(search: String)` both exist and work.

### UserAvatar
```
large / medium: String
```

### MediaList
*(NOT "MediaListEntry" — that type name doesn't exist. See gotcha #12.)*
```
id: Int!                    userId: Int!            mediaId: Int!
status: MediaListStatus      score(format): Float
progress: Int                 progressVolumes: Int
repeat: Int                    priority: Int
private: Boolean               notes: String
startedAt / completedAt: FuzzyDate
updatedAt: Int                 <- two-way sync comparison field
createdAt: Int
media: Media                    user: User
```

### FuzzyDate
```
year / month / day: Int   (all independently nullable)
```

### MediaTag (on Media.tags) and the top-level collections
```
Media.tags: name, rank (Int, 0-100), isMediaSpoiler, isGeneralSpoiler, isAdult
GenreCollection: [String]                       <- 19 genres; "Hentai" is the adult one (no flag)
MediaTagCollection: name, category, isGeneralSpoiler, isAdult   <- 430 tags, 24 categories (2026-10-30)
```

### Page.media filter arguments used by Pick an anime (FR-70)
```
type, id_in, id_not_in, genre_in, genre_not_in, tag_in, tag_not_in,
popularity_greater/_lesser, averageScore_greater, format_in,
startDate_greater/_lesser (FuzzyDateInt), season, episodes_greater/_lesser,
status_in, isAdult, sort      <- semantics confirmed live: see gotchas #17-#25
```

### Enums actually used by this app (confirmed via live introspection)
```
MediaType:        ANIME, MANGA
MediaFormat:       TV, TV_SHORT, MOVIE, SPECIAL, OVA, ONA, MUSIC, MANGA, NOVEL, ONE_SHOT
MediaStatus:       FINISHED, RELEASING, NOT_YET_RELEASED, CANCELLED, HIATUS
MediaSeason:       WINTER, SPRING, SUMMER, FALL
MediaSort:         (many) includes START_DATE, START_DATE_DESC, END_DATE, END_DATE_DESC, UPDATED_AT, UPDATED_AT_DESC, POPULARITY_DESC, FAVOURITES_DESC
MediaRelation:     ADAPTATION, PREQUEL, SEQUEL, PARENT, SIDE_STORY, CHARACTER, SUMMARY, ALTERNATIVE, SPIN_OFF, OTHER, SOURCE, COMPILATION, CONTAINS, SAME_UNIVERSE
CharacterRole:     MAIN, SUPPORTING, BACKGROUND
MediaListStatus:   CURRENT, PLANNING, COMPLETED, DROPPED, PAUSED, REPEATING
ScoreFormat:       POINT_100, POINT_10_DECIMAL, POINT_10, POINT_5, POINT_3
```

---

## B. Gotchas and confirmed behavior

1. **`MediaRelation` enum values** are `ADAPTATION/PREQUEL/SEQUEL/PARENT/SIDE_STORY/CHARACTER/SUMMARY/ALTERNATIVE/SPIN_OFF/OTHER/SOURCE/COMPILATION/CONTAINS/SAME_UNIVERSE` — confirmed via live introspection. A single title can carry **multiple edges of the same relation type** (e.g. two `PREQUEL` edges — a real season plus an unrelated recap special) — confirmed via live testing during Rate Series multi-hop chain-walking. Don't `.find()` the first same-type match; check all candidates.

2. **AniList's relation graph is adjacency-only** — there's no built-in transitive/multi-hop traversal in a single query. Franchise-spanning features (e.g. "Rate the whole series") require client-side chain-walking, hop by hop.

3. **`CharacterEdge.media` (flat `[Media]`, zero args) works correctly at any size. `Staff.characters.nodes.media` (a nested `MediaConnection`) returns a confirmed AniList server-side 500 at ANY size, including the minimal 1x1 case.** Isolated by testing three ways: `Character.media` queried directly — works; `Staff.characters` queried without nested media — works; the nested combination — 500s every time a character actually has media, for any real staff member. Use the flat edge field (`Staff.characters.edges[].media`), never nest a connection under `Staff.characters`.

4. **`CharacterEdge.role` is the `CharacterRole` enum** (`MAIN`/`SUPPORTING`/`BACKGROUND`). **`StaffEdge.role` is a plain `String`** (free text like "Director", "Music"). Same field name, different type, on sibling edge types — easy to conflate when writing a query from memory.

5. **`Character.age` is a `String`; `Staff.age` is an `Int`.** Same field name, different scalar type across the two entity types.

6. **`Studio` has no image/logo field at all** — confirmed via live introspection (`id, name, isAnimationStudio, media, siteUrl, isFavourite, favourites` is the complete field list). A Studio detail page can never show a studio image or logo. This is not a gap to fix.

7. **`Staff` has no `height` field.** Don't attempt to add it. Real structured fields that DO exist and were used to build the bio section: `gender, dateOfBirth{year,month,day}, dateOfDeath{...}, age, yearsActive:[Int], homeTown, bloodType, siteUrl, primaryOccupations:[String]`. All are independently nullable — many staff have `bloodType`/`homeTown` unset.

8. **`Page.studios(search: String)` and `Page.users(search: String)` both exist and work.** `User.favourites` and `User.statistics` resolve correctly for *arbitrary* users found via `Page.users(search:)`, not just the connected `Viewer` — this is what makes public user-profile pages possible without extra auth.

9. **`UserStatisticTypes` splits into separate `anime` and `manga` sub-objects**, each a full `UserStatistics` (`count, meanScore, episodesWatched, chaptersRead, genres, ...`) — never blended.

10. **`MediaSort` includes `START_DATE_DESC`** (also `START_DATE`, `END_DATE`, `END_DATE_DESC`, `UPDATED_AT`, `UPDATED_AT_DESC`) — used to get media pre-sorted newest-first for date-grouped list UIs, avoiding client-side date sorting.

11. **The original `voiceActors` sub-query never fetched `id` at all** — a genuine backend gap (not a frontend wiring issue). Character detail's Voice Actors shelf had no way to click through to a Staff page until `id` was added to the query and to the `VoiceActor` model.

12. **There is no `MediaListEntry` type in AniList's schema.** The real type is `MediaList` (confirmed: introspecting `MediaListEntry` returns `null`). Fields include `status, score(format), progress, repeat, startedAt/completedAt (FuzzyDate), updatedAt: Int, createdAt: Int, media, user`. `updatedAt` is the field two-way sync conflict detection compares against.

13. **`FuzzyDate` is `{year, month, day}`, all independently nullable `Int`.** Used for `dateOfBirth`/`dateOfDeath` (Character, Staff), `startDate`/`endDate` (Media), and `startedAt`/`completedAt` (MediaList). Format defensively — many real records only have `year` populated.

14. **Local (non-AniList) fact:** manga rows always write `audio=0.0` (never `NULL`) in the local `rating` table — `db.rs`'s `upsert_rating()` and `apply_remote_entry()` both explicitly override this server-side regardless of client input, since manga has no Audio criterion. Relevant for any per-criterion sort/filter/average logic — `NULL` never means "not applicable" for manga audio, `0.0` does.

15. **AniList rate-limits aggressively, even for unauthenticated introspection queries.** Hit real `429 Too Many Requests` responses this session after roughly 15 rapid sequential single-type introspection calls. Batch multiple type/enum lookups into one query using GraphQL aliases (`Foo: __type(name:"Foo"){...} Bar: __type(name:"Bar"){...}`) instead of firing many small requests back to back.

16. **`Staff.characterMedia`** (a `MediaConnection` distinct from `staffMedia`) exists per live introspection but was not tested or used this session — its exact semantics (character-derived media vs. staff's own production credits) are unconfirmed. Verify behavior before relying on it.

17. **Friend lists (PRD 4.12): confirmed against the live API on 2026-10-30, logged out (no token), with the exact queries in `anilist.rs`.** `friend_anilist()` in `tests/backend.rs` answers with the response shapes recorded here.
    - `User(id: $id, name: $name)` with only one of the two variables set looks a user up by id or exact name without a token. An unknown name or id: HTTP 404, `{"errors":[{"message":"Not Found.","status":404}],"data":{"User":null}}`.
    - **A private user looks exactly like a missing one to `User`**: HTTP 404 `"Not Found."` (checked by id; private users don't show up in user search, so no private username was available to check by name). Only `MediaListCollection` tells them apart: HTTP 404 `"Private User"` for a private user, `"User not found"` for a missing one, one error per aliased list. So after a `User` 404, `get_friend_list()` asks `MediaListCollection(userId:/userName:, type: ANIME) { hasNextChunk }` which of FR-73's two messages applies; still two requests.
    - A public user with nothing listed: HTTP 200, `{"anime":{"lists":[]},"manga":{"lists":[]}}`. Added with 0 entries, never reported as private.
    - `score(format: POINT_10_DECIMAL)` converts another user's scores from every format without a token; whole numbers come back as JSON integers (`9`, `0`). AniList's mapping on real lists: POINT_100 86 → 8.6; POINT_10 6 → 6; POINT_5 ★1–★5 → 1/3/5/7/9 (five stars is 9, not 10); POINT_3 :( / :| / :) → 3.5 / 6 / 8.5. 0 still means unscored. A bare `score` (no `format:`) is NOT reliably in the owner's format: one POINT_10_DECIMAL user had entries stored as both `8` and `80`. Always pass `format:`.
    - **So POINT_5 and POINT_3 friends are read in their own format and converted by the app (FR-72)**: ★ × 2, and 1/2/3 → 4/7/10. `User.mediaListOptions.scoreFormat` is public, so the lookup brings the format and the list query passes it as `score(format: $format)` (an enum variable works); still two requests. Verified live on 2026-10-30: `score(format: POINT_5)` gives the stars the user sees (POINT_100 10/30/50/70/90 → ★1–★5; entries scored before switching format round to the nearest star, 60 → ★3, 85 → ★4), and `score(format: POINT_3)` gives 35/60/85 → 1/2/3. Through the app: all 337 entries of user 5878220 (POINT_5) and 104 of user 6878852 (POINT_3) stored as mapped, and ★5 now compares as 10.0 instead of 9.0. FR-77's 8.0 cut-off now takes a POINT_5 friend's ★4 titles (8.0, was 7.0).
    - The list query also reads each entry's `progress` and each title's `popularity` (FR-77's tie-break, FR-82's "Completed · 26 episodes"); both logged out. Live on 2026-10-30 (user 5878220, 337 entries): every entry had an integer `progress` (0 when unstarted, chapters for manga) and every title a `popularity` (AniList's count of users with it listed). Still two requests, and the comparison itself makes none.
    - No chunking without `chunk`/`perChunk`: `hasNextChunk` is false and every entry comes back in one response. Both lists in the one friend query: 11,905 entries (user 30403) in 3.2 s / 4.3 MB, 19,944 entries (user 76852) in 5.4 s / 7.5 MB, well under the 30 s list timeout. Adding the 11,905-entry friend took 4.0 s end to end in a debug build.
    - `statistics.anime.count` / `.manga.count` leave out Planning entries (30403: 3,059 vs 4,168 listed), so a friend's stored count can be well above their profile's.
    - No media id appeared twice on any list checked (337, 11,905 and 19,944 entries, with custom lists); the first-copy dedup in `get_friend_list()` stays as a guard.
    - Requests, counted through a logging proxy: add and refresh are 2 (lookup, both lists); unknown and private are 2 (lookup, the one-list query). None carried an Authorization header.
    - Every response says `X-RateLimit-Limit: 30` (per minute), down from the ~90 the PRD used to assume (FR-50/FR-71 now say 30). No real 429 was seen: that session's egress spread requests over several IPs (`X-RateLimit-Remaining` jumped between 22 and 29 across 40 quick requests). 429 handling is tested only against the fake server; whether a real `Retry-After` exceeds `MAX_RETRY_WAIT_SECONDS` (30) is unconfirmed. If it does, the call fails at once instead of waiting, which is the intended trade-off for a request someone is waiting on.
    - For finding test users: `Page.users(sort: WATCHED_TIME_DESC)` returns HTTP 500 after ~10 s at any `perPage`; `CHAPTERS_READ_DESC` doesn't actually order by chapters read; `Page` depth is capped at 5,000 entries (`page × perPage`). `Page.mediaList(mediaId:)` on an obscure old title is a good way to find very large lists; private users' rows there have `user: null`.
    - Cloudflare in front of `graphql.anilist.co` rejects some client User-Agents (Python urllib's: 403, error 1010). No User-Agent (reqwest's default, what the app sends) and curl's are fine.

18. **Following and followers (PRD FR-79): confirmed live on 2026-10-30, logged out.** `Page(page:, perPage: 50) { pageInfo { hasNextPage } following(userId:, sort: [ID]) { id name avatar { large } } }` (and the same with `followers`) works without a token; `sort: [ID]` is accepted and keeps paging stable. `pageInfo.total` / `lastPage` are filled in for these lists (user 30403: 56 following, 40 followers). No null entries showed up on the lists checked, so whether a private user you follow appears there at all is unconfirmed; the app skips any entry without an id. `User.mediaListOptions.scoreFormat` is public on these nodes too, but the app takes it from the per-friend lookup (FR-71) instead.
    - Reading followers pages for mutuals is costly: real accounts 30403 = 2 following + 1 followers pages, 76852 = 4 + 13, 95746 = 9 + 22 (31 requests, more than a minute's budget). So mutual only reads your own following list with your token and uses `User.isFollower` instead (same request count as an everyone sync).
    - **`isFollower` is relative to the logged-in viewer, and logged out it is `false`, not null** (checked live: 4 of the first 5 users 30403 follows are mutuals, and all 5 came back `false` without a token). So a `false` proves nothing on its own: the app asks `viewer: Viewer { id }` in the same request and trusts `isFollower` only when that is your id, otherwise it reads your followers pages logged out. **With a real token it is unverified** (no token in that session): check on desktop that a mutual-only sync makes only following requests and keeps the right people.
    - Checked through the app (logged-out, on a temp DB logged in as 30403, through a logging proxy): a sync imported all 56 in 2 requests with no list fetched; mutual only kept exactly the 28 in both lists (3 requests); the background queue fetched never-fetched friends in id order, one every 13–14 s (12 s spacing plus the fetch), 10 requests in its first minute; opening a comparison for the last friend in line fetched them at once (2 requests, 0.8 s); hiding a followed friend survived the next sync. No friend request carried an Authorization header.
17. **`pageInfo.total` and `lastPage` are not counts.** While `hasNextPage` is true, AniList reports `total: 5000` (and `lastPage: 5000 / perPage`) whatever the real size. Confirmed 2026-10-30: Action+Comedy movies reported `total: 5000` on page 1, yet page 6 at `perPage: 50` was already empty. Only a page with `hasNextPage: false` carries the exact count, and a page past the end reports `total = (page - 1) * perPage`. To size a result set, probe single entries (`Page(page: n + 1, perPage: 1)` exists iff there are more than n matches), several at once with aliases.

18. **Paging stops at 5,000 entries.** Any `page * perPage` past 5000 is an HTTP 400: `Page depth exceeds maximum allowed for API requests (5000 entries)`. A sort order only ever exposes its first 5,000 matches. Pick an anime alternates `sort: ID` and `ID_DESC` for result sets that big (the oldest or newest 5,000 AniList entries).

19. **`genre_in` and `tag_in` mean "has all of these"; `genre_not_in` and `tag_not_in` mean "has none of these".** Confirmed on Winter 2024 TV: Action 22, Comedy 21, `genre_in: ["Action","Comedy"]` returned exactly the 5 titles with both; `tag_in: ["School","Male Protagonist"]` returned exactly the 9 with both; the `_not_in` results contained neither.

20. **An explicit `null` for some list arguments is an HTTP 500** (`Internal Server Error`): confirmed for `id_in`, `id_not_in` and `format_in` (`genre_in: null` happens to work). A variable that's declared but left out of `variables` is fine, so send only the filters that are set.

21. **Empty lists: `id_in: []` matches nothing** (0 results, not "no filter"); `id_not_in: []` excludes nothing.

22. **A long `id_not_in` is fine:** 10,000 ids (a 70 KB request) answered in 0.5 s.

23. **Range bounds.** `popularity_greater/_lesser` and `episodes_greater/_lesser` are strict (`>`/`<`), confirmed on one title at its exact value. `averageScore_greater: G` returns titles whose `averageScore` shows G or more (it behaves as `>=`); `averageScore_lesser` is `<`. `startDate_greater/_lesser` take a FuzzyDateInt (`YYYYMMDD`), and year-only dates are `YYYY0000`, so a year range Y1-Y2 is `startDate_greater: Y1*10000 - 1, startDate_lesser: Y2*10000 + 9999` either way the bound is read.

24. **The rate limit is 30 requests a minute right now,** not the documented 90 (`X-RateLimit-Limit: 30`, 2026-10-30). Aliases inside one request count as one request.

25. **Aliased deep probes cost depth, and sort order matters.** 25 single-entry probes spread to entry 4950 with a 2,000-id `id_not_in`: `sort: ID` 0.7 s, `ID_DESC` 2.5 s, `TITLE_ROMAJI` 3.7 s; 100 such probes with `TITLE_ROMAJI` took 14 s. A probe past the end of the matches is the expensive kind: AniList scans the whole filtered set to find nothing there. With genre, tag and range filters on a 10-title result, page 1 alone took 0.6 s, one probe past the end 0.4 s, four 1.2 s and 22 of them 6.1 s, so Pick an anime sends no probes when page 1 holds every match. AniList also caches repeated identical queries (a repeat came back in 0.2 s), which doesn't help rerolls: their `id_not_in` grows each time.

26. **How a franchise's seasons are linked: Re:Zero, checked live 2026-10-30.** Every season *and every split cour* is its own entry, chained one hop at a time with `PREQUEL`/`SEQUEL`: 1st Season (21355, TV) -> 2nd Season (108632, TV) -> 2nd Season Part 2 (119661, TV) -> 3rd Season (163134, TV) -> 4th Season (189046, TV). So a season's direct `PREQUEL` is the cour right before it (3rd Season's is 2nd Season *Part 2*), and "have I seen the earlier seasons" takes a multi-hop walk (gotcha #2). Off the main line: the OVAs (Memory Snow and Frozen Bond together, 100049, OVA) hang off the 1st Season as `PARENT`/`SIDE_STORY`, not as anyone's prequel; the chibi "Break Time" shorts (ONA/TV_SHORT) chain among themselves with `PREQUEL` and reach the main show only through `PARENT`; every light novel/manga link is `ALTERNATIVE` to a `MANGA`-type node. Pick an anime (FR-71) counts only `PREQUEL` edges to `ANIME` nodes in TV, TV_SHORT, MOVIE or ONA format, which keeps all of those side links from blocking.
