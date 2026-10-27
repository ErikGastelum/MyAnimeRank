# MyAnimeRank

A personal, local desktop app for ranking anime and manga by **anchored
relative scoring**: you rate each title on several criteria by comparing it
against titles you've already scored in the same genre and media type, instead
of picking an isolated 1–10.

It's also an AniList client. You log in with your AniList account, your lists
sync both ways (status, progress, overall score), and you can browse Discover,
your Feed, characters, staff, studios and user profiles. All metadata comes
from AniList's GraphQL API.

The product spec is [`docs/PRD.md`](docs/PRD.md).

## Desktop app (Tauri + Rust)

The app is being moved from Python to a single native Rust binary for lower
memory use. It lives in [`src-tauri/`](src-tauri/): the same API routes,
JSON shapes and SQLite schema, running in-process (no local web server),
with the same `frontend/index.html` shown in a frameless Tauri window. The
Python version below still works during the transition.

### Setup (Windows)

1. Install Rust from <https://rustup.rs> (it sets up the MSVC build tools it
   needs). WebView2 is already part of Windows 10/11.
2. AniList login: no setup needed. Log in with your own AniList account; the
   app's AniList client ID is built in and there is no client secret.

   To use a different AniList client instead, set its ID in the
   `ANILIST_CLIENT_ID` environment variable, or as `ANILIST_CLIENT_ID = "12345"`
   in `local_config.toml` in `%APPDATA%\com.erikgastelum.myanimerank\` (or
   `local_config.py` in the folder you run from, or its parent). Whichever
   client is used, its redirect URI on AniList must stay exactly
   `http://localhost:8931/callback`.
3. Your library: the app keeps its database at
   `%APPDATA%\com.erikgastelum.myanimerank\library.db`. To bring your
   existing library over, close the app and **copy** your current
   `library.db` there (the schema is identical, nothing is migrated). Or point
   it anywhere with the `MYANIMERANK_DB` environment variable.

### Run

```powershell
cd src-tauri
cargo run                      # desktop window (debug build)
cargo run --release            # optimized build
cargo run -- --serve 8420      # browser dev at http://127.0.0.1:8420 (uses ./library.db or MYANIMERANK_DB)
cargo test                     # tests; in-memory databases and a fake AniList, never the network
```

To build an installer (`.msi` / `.exe`): `cargo install tauri-cli --version "^2" --locked`,
then `cargo tauri build` from `src-tauri/`.

## Python version (being replaced)

Stack: Python (FastAPI) backend, SQLite, a single-file HTML/JS frontend, shown
in a frameless pywebview desktop window.

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
