from pydantic import BaseModel, Field
from typing import Optional


class AnimeCreate(BaseModel):
    title: str
    api_id: Optional[str] = None
    cover_url: Optional[str] = None
    genres: list[str] = []
    year: Optional[int] = None
    season: Optional[str] = None
    episodes: Optional[int] = None
    format: Optional[str] = None
    synopsis: Optional[str] = None
    mal_score: Optional[float] = None


class RatingIn(BaseModel):
    story:      float = Field(..., ge=1.0, le=10.0)
    characters: float = Field(..., ge=1.0, le=10.0)
    animation:  float = Field(..., ge=1.0, le=10.0)
    audio:      float = Field(..., ge=1.0, le=10.0)
    enjoyment:  float = Field(..., ge=1.0, le=10.0)
