"""Pydantic models matching PRD Section 7's API contract shapes."""
from typing import Annotated

from pydantic import BaseModel, Field


class SearchResult(BaseModel):
    anilist_id: int
    media_type: str
    title: str
    title_native: str | None = None
    title_english: str | None = None
    cover_url: str | None = None
    genres: list[str] = []
    tags: list[str] = []
    producers: list[str] = []
    year: int | None = None
    season: str | None = None
    format: str | None = None
    episodes: int | None = None
    chapters: int | None = None
    volumes: int | None = None
    runtime_minutes: int | None = None
    synopsis: str | None = None
    avg_score: float | None = None


class SearchResponse(BaseModel):
    results: list[SearchResult]
    q: str


class MediaImport(SearchResult):
    status: str = "planning"


class RatingOut(BaseModel):
    story: float
    characters: float
    animation: float | None = None
    audio: float
    enjoyment: float
    overall: float
    updated_at: str


class MediaOut(SearchResult):
    id: int
    status: str
    progress: int
    repeat_count: int
    anilist_list_entry_id: int | None = None
    updated_at: str | None = None
    last_synced_at: str | None = None
    pending_conflict: bool = False
    rating: RatingOut | None = None


RatingScore = Annotated[float, Field(ge=1.0, le=10.0, multiple_of=0.5)]


class RatingIn(BaseModel):
    story: RatingScore
    characters: RatingScore
    animation: RatingScore
    audio: float
    enjoyment: RatingScore


class RatingResult(BaseModel):
    overall: float
    media_type: str


class StatusIn(BaseModel):
    status: str


class StatusOut(BaseModel):
    status: str


class ProgressIn(BaseModel):
    progress: int


class ProgressOut(BaseModel):
    progress: int


class RepeatIn(BaseModel):
    repeat_count: int


class RepeatOut(BaseModel):
    repeat_count: int


class AnchorItem(BaseModel):
    id: int
    title: str
    title_english: str | None = None
    cover_url: str | None = None
    score: float


class AnchorsResponse(BaseModel):
    anchors: list[AnchorItem]


class AuthStatus(BaseModel):
    connected: bool
    username: str | None = None


class LogoutResult(BaseModel):
    connected: bool


class ConflictSide(BaseModel):
    status: str
    progress: int
    updated_at: str | None = None
    updatedAt: str | None = None  # remote side only, matches AniList's own casing


class ConflictEntry(BaseModel):
    media_id: int
    title: str
    title_english: str | None = None
    local: ConflictSide
    remote: ConflictSide


class SyncPullResult(BaseModel):
    imported: int
    updated: int
    conflicts: list[ConflictEntry]


class PendingConflictsResult(BaseModel):
    conflicts: list[ConflictEntry]


# ---------------------------------------------------------------- M5 ----

class DiscoverItem(SearchResult):
    local_status: str | None = None


class DiscoverResponse(BaseModel):
    results: list[DiscoverItem]
    count: int


class FeedActivity(BaseModel):
    username: str | None = None
    verb: str | None = None
    title: str | None = None
    title_english: str | None = None
    anilist_id: int | None = None
    media_type: str | None = None
    cover_url: str | None = None
    progress_range: str | None = None
    occurred_at: str | None = None


class FeedResponse(BaseModel):
    activities: list[FeedActivity]
    has_more: bool


class CharacterSearchItem(BaseModel):
    anilist_id: int
    name: str
    image_url: str | None = None


class CharacterSearchResponse(BaseModel):
    results: list[CharacterSearchItem]


class StaffSearchItem(BaseModel):
    anilist_id: int
    name: str
    image_url: str | None = None


class StaffSearchResponse(BaseModel):
    results: list[StaffSearchItem]


class VoiceActor(BaseModel):
    anilist_id: int
    name: str
    language: str | None = None
    image_url: str | None = None


class CharacterAppearance(BaseModel):
    anilist_id: int | None = None
    title: str | None = None
    title_english: str | None = None
    cover_url: str | None = None
    year: int | None = None


class CharacterDetail(BaseModel):
    anilist_id: int
    name: str
    image_url: str | None = None
    bio: str | None = None
    favorites_count: int | None = None
    voice_actors: list[VoiceActor]
    appearances: list[CharacterAppearance]


class StaffCharacter(BaseModel):
    anilist_id: int | None = None
    name: str | None = None
    image_url: str | None = None
    role: str | None = None
    work_anilist_id: int | None = None
    work_title: str | None = None
    work_cover_url: str | None = None
    year: int | None = None
    work_anilist_ids: list[int] = []


class StaffWork(BaseModel):
    anilist_id: int
    title: str | None = None
    title_english: str | None = None
    cover_url: str | None = None
    year: int | None = None


class StaffDetail(BaseModel):
    anilist_id: int
    name: str
    image_url: str | None = None
    bio: str | None = None
    favorites_count: int | None = None
    gender: str | None = None
    birth_year: int | None = None
    birth_month: int | None = None
    birth_day: int | None = None
    age: int | None = None
    years_active: list[int] = []
    home_town: str | None = None
    blood_type: str | None = None
    characters_voiced: list[StaffCharacter]
    works: list[StaffWork]


class FavoriteToggleIn(BaseModel):
    type: str
    anilist_id: int


class FavoriteToggleOut(BaseModel):
    favorited: bool


class FavoriteMediaItem(BaseModel):
    anilist_id: int
    title: str | None = None
    title_english: str | None = None
    cover_url: str | None = None


class FavoritePersonItem(BaseModel):
    anilist_id: int
    name: str
    image_url: str | None = None


class FavoriteStudioItem(BaseModel):
    anilist_id: int
    name: str


class StudioSearchItem(BaseModel):
    anilist_id: int
    name: str


class StudioSearchResponse(BaseModel):
    results: list[StudioSearchItem]


class StudioWork(BaseModel):
    anilist_id: int
    title: str | None = None
    title_english: str | None = None
    cover_url: str | None = None
    year: int | None = None


class StudioDetail(BaseModel):
    anilist_id: int
    name: str
    is_animation_studio: bool
    favorites_count: int | None = None
    works: list[StudioWork]


class UserSearchItem(BaseModel):
    anilist_id: int
    name: str
    avatar_url: str | None = None


class UserSearchResponse(BaseModel):
    results: list[UserSearchItem]


class FavoritesResponse(BaseModel):
    anime: list[FavoriteMediaItem]
    manga: list[FavoriteMediaItem]
    characters: list[FavoritePersonItem]
    staff: list[FavoritePersonItem]
    studios: list[FavoriteStudioItem]


class PublicUserDetail(BaseModel):
    anilist_id: int
    name: str
    avatar_url: str | None = None
    banner_url: str | None = None
    about: str | None = None
    site_url: str | None = None
    created_at: int | None = None
    anime_count: int
    anime_mean_score: float
    anime_episodes_watched: int
    manga_count: int
    manga_mean_score: float
    manga_chapters_read: int
    favorites: FavoritesResponse


class CommunityRelation(BaseModel):
    anilist_id: int | None = None
    type: str | None = None
    title: str | None = None
    title_english: str | None = None
    cover_url: str | None = None
    year: int | None = None
    media_type: str | None = None


class CommunityCharacter(BaseModel):
    anilist_id: int | None = None
    name: str | None = None
    role: str | None = None
    image_url: str | None = None


class CommunityRecommended(BaseModel):
    anilist_id: int | None = None
    title: str | None = None
    title_english: str | None = None
    cover_url: str | None = None
    thumbs: int | None = None


class CommunityResponse(BaseModel):
    relations: list[CommunityRelation]
    characters: list[CommunityCharacter]
    recommended: list[CommunityRecommended]
    popularity: int | None = None
    favourites: int | None = None
    trending: int | None = None
    rank_rated: int | None = None
    rank_popular: int | None = None


class ResolveIn(BaseModel):
    winner: str  # "local" | "remote"


class ResolveOut(BaseModel):
    status: str
    progress: int


class PushResult(BaseModel):
    synced_at: str | None
    score_format_drift: bool


class ScoreFormatResult(BaseModel):
    score_format: str


# ---------------------------------------------------------------- M6 ----

class LibraryResponse(BaseModel):
    results: list[MediaOut]
    count: int


class RankingsResponse(BaseModel):
    results: list[MediaOut]
    count: int


class DeleteResult(BaseModel):
    deleted: bool


class ProfileStatsResponse(BaseModel):
    total_anime: int
    total_manga: int
    days_watched: float
    chapters_read: int
    followers: int
    following: int


# ---------------------------------------------------------------- M8 ----

class GenreInsight(BaseModel):
    genre: str
    media_type: str
    avg_overall: float
    count: int


class CriterionInsight(BaseModel):
    criterion: str
    avg_score: float


class TopCriterionResponse(BaseModel):
    anime: CriterionInsight | None = None
    manga: CriterionInsight | None = None


class DivergenceTitle(BaseModel):
    media_id: int
    title: str
    title_english: str | None = None
    your_score: float
    avg_score: float
    delta: float


class CommunityDivergence(BaseModel):
    avg_divergence: float
    most_above: list[DivergenceTitle]
    most_below: list[DivergenceTitle]


class DistributionBucket(BaseModel):
    bucket: float
    count: int


class DistributionResponse(BaseModel):
    anime: list[DistributionBucket]
    manga: list[DistributionBucket]


class InsightsResponse(BaseModel):
    by_genre: list[GenreInsight]
    top_criterion: TopCriterionResponse
    community_divergence: CommunityDivergence
    distribution: DistributionResponse
