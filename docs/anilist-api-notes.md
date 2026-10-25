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

14. **Local (non-AniList) fact:** manga rows always write `audio=0.0` (never `NULL`) in the local `rating` table — `database.py`'s `upsert_rating()` and `apply_remote_entry()` both explicitly override this server-side regardless of client input, since manga has no Audio criterion. Relevant for any per-criterion sort/filter/average logic — `NULL` never means "not applicable" for manga audio, `0.0` does.

15. **AniList rate-limits aggressively, even for unauthenticated introspection queries.** Hit real `429 Too Many Requests` responses this session after roughly 15 rapid sequential single-type introspection calls. Batch multiple type/enum lookups into one query using GraphQL aliases (`Foo: __type(name:"Foo"){...} Bar: __type(name:"Bar"){...}`) instead of firing many small requests back to back.

16. **`Staff.characterMedia`** (a `MediaConnection` distinct from `staffMedia`) exists per live introspection but was not tested or used this session — its exact semantics (character-derived media vs. staff's own production credits) are unconfirmed. Verify behavior before relying on it.
