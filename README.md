# MyAnimeRank

A personal, local desktop app for ranking anime and manga by **anchored
relative scoring**: you rate each title on several criteria by comparing it
against titles you've already scored in the same genre and media type, instead
of picking an isolated 1–10.

It's also an AniList client. You log in with your AniList account, your lists
sync both ways (status, progress, overall score), and you can browse Discover,
your Feed, characters, staff, studios and user profiles, or have it pick an
anime to watch next. All metadata comes from AniList's GraphQL API.

The product spec is [`docs/PRD.md`](docs/PRD.md).

## How it's built

A single native Rust binary built with Tauri 2, in [`src-tauri/`](src-tauri/).
The API runs in-process (no local web server, no open port), SQLite holds the
library, and the UI is the single-file `frontend/index.html`, shown in a
frameless Tauri window.

## Setup (Windows)

1. Install Rust from <https://rustup.rs> (it sets up the MSVC build tools it
   needs). WebView2 is already part of Windows 10/11.
2. AniList login: no setup needed. Log in with your own AniList account; the
   app's AniList client ID is built in and there is no client secret.

   To use a different AniList client instead, set its ID in the
   `ANILIST_CLIENT_ID` environment variable, or as `ANILIST_CLIENT_ID = "12345"`
   in `local_config.toml` in `%APPDATA%\com.erikgastelum.myanimerank\` (the
   environment variable wins if both are set). Whichever client is used, its
   redirect URI on AniList must stay exactly
   `http://localhost:8931/callback`.
3. Your library: the app keeps its database at
   `%APPDATA%\com.erikgastelum.myanimerank\library.db`. To bring an existing
   library over, close the app and **copy** your `library.db` there. Or point
   it anywhere with the `MYANIMERANK_DB` environment variable.

## Setup (macOS / Linux)

Login works the same way: no setup needed.

- **macOS:** run `xcode-select --install`, then install Rust from
  <https://rustup.rs>. The app's files live in
  `~/Library/Application Support/com.erikgastelum.myanimerank/`.
- **Linux:** install Rust from <https://rustup.rs>, plus the WebKitGTK
  libraries Tauri builds against. On Debian/Ubuntu (tested):

  ```bash
  sudo apt install build-essential pkg-config libssl-dev libgtk-3-dev \
    libwebkit2gtk-4.1-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
    librsvg2-dev libayatana-appindicator3-dev
  ```

  For other distros, see Tauri's list at
  <https://v2.tauri.app/start/prerequisites/#linux>. The library database
  lives in `~/.local/share/com.erikgastelum.myanimerank/` and an optional
  `local_config.toml` in `~/.config/com.erikgastelum.myanimerank/`.

The commands below are the same on every platform (use `export
MYANIMERANK_DB=...` instead of PowerShell's `$env:` syntax).

## Run

```powershell
cd src-tauri
cargo run                      # desktop window (debug build)
cargo run --release            # optimized build
cargo run -- --serve 8420      # browser dev at http://127.0.0.1:8420 (uses ./library.db or MYANIMERANK_DB)
```

Opening a database may migrate its schema, so copy `library.db` somewhere safe
before trying a new build against it.

To build an installer (`.msi` / `.exe`): `cargo install tauri-cli --version "^2" --locked`,
then `cargo tauri build` from `src-tauri/`.

## Test

```powershell
cd src-tauri
cargo test                     # in-memory databases and a fake AniList, never the network
cargo clippy --all-targets     # lint; kept warning-free
```
