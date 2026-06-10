# MyAnimeRank

A local-first desktop app for ranking anime through anchored relative scoring. Instead of pulling numbers out of thin air, every new rating is grounded in a comparison against anime you've already scored — filtered to the same genres, so a 7.5 story in Action means the same thing across all your Action titles.

No account. No cloud. No tracking. Runs entirely on your machine.

---

## How it works

When you add an anime, you score it across five criteria: **Story, Characters, Animation, Audio,** and **Enjoyment**. For each criterion, the app surfaces your already-rated anime that share at least one genre, sorted by that criterion's score, as anchors. You decide where the new title fits relative to those anchors and assign a score. The overall is the simple average of the five.

> *If Naruto's Story is anchored at 8.0, rating Jujutsu Kaisen's Story means deciding: is JJK better or worse than Naruto — and by how much?*

The master ranking re-sorts automatically every time a rating changes.

---

## Features

- **Anchored comparison workflow** — genre-matched anchors for each criterion keep scores internally consistent
- **Anime metadata autofill** — titles, covers, genres, year, and episode counts pulled from AniList / Jikan (MAL)
- **Import your existing list** — from AniList (username), MAL (username), GDPR export, CSV, or XML
- **Export to MAL-compatible XML** — take your ratings back to MyAnimeList
- **Backup & restore** — full library export/import as JSON
- **Seasonal recommendations** — a panel of upcoming/current anime based on your top genres
- **Personal insights** — average scores by genre, score distributions, divergence from community ratings
- **Frameless native-style window** — custom titlebar with minimize, maximize, and drag support
- **Fully offline** — no login, no sync, no telemetry

---

## Tech stack

| Layer | Choice |
|---|---|
| Desktop shell | Python + [pywebview](https://pywebview.flowrl.com/) |
| Backend | FastAPI + Uvicorn |
| Database | SQLite (single local file, zero config) |
| Frontend | Vanilla JS / React (served by FastAPI) |
| Anime metadata | Jikan v4 (MAL) |

---

## Requirements

- Python 3.10+
- pip

---

## Installation

```bash
# Clone the repo
git clone https://github.com/yourusername/MyAnimeRank.git
cd MyAnimeRank

# Install dependencies
pip install -r requirements.txt

# Launch the app
python app.py
```

The app opens in its own frameless window. Your library is stored in `myanimrank.db` in the project directory and persists across restarts.

> **Headless / browser mode:** if you'd rather run it in your browser without the desktop window, run `python main.py` and open `http://127.0.0.1:8000` directly.

---

## Project structure

```
MyAnimeRank/
├── app.py          # Desktop entry point (pywebview wrapper)
├── main.py         # FastAPI app and all API routes
├── database.py     # SQLite schema and connection helpers
├── models.py       # Pydantic request/response models
├── jikan.py        # Jikan API client + local search cache
├── MyAnimeRank.html  # Single-file frontend (React, served by FastAPI)
├── requirements.txt
└── myanimrank.db   # Created on first run (gitignored)
```

---

## Importing your list

| Source | How |
|---|---|
| AniList | Enter your AniList username in the Import panel |
| MyAnimeList | Enter your MAL username (fetched via Jikan) |
| MAL XML export | Upload the XML file from your MAL export |
| GDPR export | Upload the ZIP from AniList's data export |
| CSV | Upload a CSV with at minimum a `title` column |

Imported entries with an existing score are distributed evenly across all five criteria. You can re-rate any entry at any time.

---

## Exporting

- **MAL XML** — produces a `myanimelist.xml` compatible with MAL's import tool
- **JSON backup** — exports your full library (anime + ratings) for safekeeping or migration

---

## Data & privacy

Everything stays local. The app makes outbound requests only to the Jikan API for anime metadata and cover images; results are cached in the local database to minimize repeat calls. No usage data is collected or transmitted.

---

## Roadmap / v2 ideas

- Weighted criteria (user-defined per-genre weights)
- Multi-user profiles
- AniList GraphQL as a metadata source (richer genre data, no rate limits)
- Criterion-level comparison against community scores
- Exportable charts and taste profiles

---

## License

MIT