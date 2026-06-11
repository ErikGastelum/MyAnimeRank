from typing import Literal, Optional

from pydantic import BaseModel, Field


class AnimeCreate(BaseModel):
    title: str
    title_ro: Optional[str] = None
    api_id: Optional[str] = None
    cover_url: Optional[str] = None
    genres: list[str] = []
    year: Optional[int] = None
    season: Optional[str] = None
    episodes: Optional[int] = None
    format: Optional[str] = None
    synopsis: Optional[str] = None
    mal_score: Optional[float] = None
    status: Optional[Literal["completed","watching","planning","dropped","paused","rewatching"]] = None


class RatingIn(BaseModel):
    story:      float = Field(..., ge=1.0, le=10.0)
    characters: float = Field(..., ge=1.0, le=10.0)
    animation:  float = Field(..., ge=1.0, le=10.0)
    audio:      float = Field(..., ge=1.0, le=10.0)
    enjoyment:  float = Field(..., ge=1.0, le=10.0)


class AnimeUpdate(BaseModel):
    title:         Optional[str]       = None
    title_ro:      Optional[str]       = None
    cover_url:     Optional[str]       = None
    genres:        Optional[list[str]] = None
    year:          Optional[int]       = None
    season:        Optional[str]       = None
    format:        Optional[str]       = None
    synopsis:      Optional[str]       = None
    notes:         Optional[str]       = None
    status:        Optional[Literal["completed","watching","planning","dropped","paused","repeating","rewatching"]] = None
    rewatch_count:    Optional[int]    = None
    episodes_watched: Optional[int]    = None
