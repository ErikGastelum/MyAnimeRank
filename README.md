# MyAnimeRank

A personal, local desktop app for ranking anime and manga by **anchored
relative scoring**: you rate each title on several criteria by comparing it
against titles you've already scored in the same genre and media type, instead
of picking an isolated 1–10.

It's also an AniList client. You log in with your AniList account, your lists
sync both ways (status, progress, overall score), and you can browse Discover,
your Feed, characters, staff, studios and user profiles. All metadata comes
from AniList's GraphQL API.

Stack: Python (FastAPI) backend, SQLite, a single-file HTML/JS frontend, shown
in a frameless pywebview desktop window. The product spec is
[`docs/PRD.md`](docs/PRD.md).

## Setup

Requires Python 3.12+.

1. Create and activate a virtual environment (`.venv/` is gitignored):

   ```powershell
   python -m venv .venv
   .venv\Scripts\Activate.ps1      # Windows PowerShell
   ```

   ```bash
   source .venv/bin/activate        # macOS / Linux
   ```

2. Install dependencies:

   ```bash
   pip install -r requirements.txt       # to run the app
   pip install -r requirements-dev.txt   # to also run tests and lint (pytest, ruff)
   ```

   On Linux, pywebview also needs GTK3 + WebKit2GTK from your OS package
   manager (see the note in `requirements.txt`).

3. Register an AniList API client at <https://anilist.co/settings/developer>
   with this exact redirect URI:

   ```
   http://localhost:8931/callback
   ```

4. Create `local_config.py` in the project root with your client's
   credentials. It's gitignored; never commit it.

   ```python
   ANILIST_CLIENT_ID = "12345"
   ANILIST_CLIENT_SECRET = "your-client-secret"
   ```

## Run

```bash
python app.py
```

opens the desktop window. On first launch you'll see the login screen; the
app can't be used without an AniList login.

For frontend/backend work in a browser instead:

```bash
uvicorn main:app --port 8420
```

then open <http://127.0.0.1:8420>.

Your library lives in `library.db` (gitignored). Starting the server creates
it, and may migrate its schema, so copy it somewhere safe before upgrading.

## Test

With the dev dependencies installed (`pip install -r requirements-dev.txt`):

```bash
python -m pytest -q
```

Tests use a throwaway database per test and fake every AniList call; they
never touch `library.db` or the network.

```bash
ruff check .
```

runs the linter (configured in `pyproject.toml`).
