# Plan: app.py — pywebview Desktop Shell (Section 6)

**Source PRD**: docs/PRD.md (Section 6: Technical Architecture)
**Complexity**: Medium

## Summary

`app.py` doesn't exist yet — this is a new file wrapping the already-working `main.py` FastAPI app in a frameless pywebview window. `main.py` itself doesn't change: it must keep running standalone via plain `uvicorn main:app --reload` for browser dev, exactly as it has for every milestone this session. The titlebar's three window-control buttons currently render as pure static divs with **zero click wiring at all** (not stubs — literally no `data-act` attribute), so this is genuinely new wiring, not fixing broken wiring. Environment check found this container has neither `pywebview` nor the system GTK/WebKit libraries it needs on Linux — per your confirmation, I'll install both and attempt a real window launch, with `ps aux | grep uvicorn` as the actual verification method for clean shutdown (not just "the window disappeared").

## Patterns to Mirror

| Category | Source | Pattern |
|---|---|---|
| Fixed vs. dynamic port separation | `oauth_listener.py:20` `OAUTH_PORT = 8931` | app.py's port must never collide with this; found dynamically, never hardcoded |
| Idempotent listener lifecycle w/ explicit teardown | `oauth_listener.py` `start_listener()`/self-shutdown-after-one-request | Same "explicit lifecycle, no orphaned resources" discipline applies to the uvicorn subprocess |
| gitignored local-only config | `local_config.py` | Not directly reused, but same "don't hardcode secrets/ports" spirit |
| Frontend real/mock branch pattern | `frontend/index.html` `BACKEND_AVAILABLE` checks (M6-M8) | Window-control handlers similarly no-op gracefully when `window.pywebview` doesn't exist (browser dev mode) |

## Files to Change

| File | Action | Why |
|---|---|---|
| `app.py` | CREATE | Frameless pywebview shell: find free port, launch `main:app` as a subprocess, open window, wire close→terminate |
| `frontend/index.html` | UPDATE | Wire the 3 titlebar buttons (`titlebar()`, currently inert divs) to real `H.winMinimize`/`H.winMaximize`/`H.winClose` handlers calling `window.pywebview.api.*` |
| `requirements.txt` | UPDATE | Add `pywebview` (regenerate via `pip freeze` per the file's own documented convention) |
| System packages | INSTALL | GTK3 + WebKit2GTK bindings (`python3-gi`, `gir1.2-webkit2-4.0` or equivalent) — pywebview's Linux backend, not currently present in this container |

## Tasks

### Task 1: Install pywebview + system GUI dependencies
- **Action**: `apt-get install` the GTK/WebKit2 bindings pywebview needs on Linux, then `pip install pywebview` into the project's `.venv`. Confirm `python3 -c "import webview"` succeeds before writing any app.py code that depends on it.
- **Mirror**: N/A — first GUI dependency in this project.
- **Validate**: `python3 -c "import webview; print(webview.__version__)"` succeeds.

### Task 2: `app.py` — dynamic port + subprocess-managed uvicorn
- **Action**: `_find_free_port()` via a throwaway `socket.bind(("127.0.0.1", 0))`. Launch `subprocess.Popen([sys.executable, "-m", "uvicorn", "main:app", "--host", "127.0.0.1", "--port", str(port)])` — a real subprocess (not a thread), specifically so shutdown is a real, verifiable `process.terminate()`/`.kill()` rather than relying on daemon-thread-dies-with-parent behavior. Poll the port briefly before creating the window so it doesn't load a blank/connection-refused page on first paint.
- **Mirror**: `oauth_listener.py`'s "never hardcode the free port, always resolve it at runtime" discipline, applied at app.py's process-launch level instead of a listener level.
- **Validate**: Run `app.py` headless-import-only first (`python3 -c "import app"`, guarded so the actual window doesn't launch on bare import) to catch syntax/import errors before the full GUI test.

### Task 3: Frameless window + real `js_api` wiring
- **Action**: `webview.create_window("MyAnimeRank", f"http://127.0.0.1:{port}", js_api=WindowApi(...), frameless=True, ...)`. `WindowApi` class exposes `minimize()`, a maximize/restore toggle, and `close()` (`window.destroy()`) — exact pywebview method names confirmed via direct introspection once installed (Task 1), not guessed at plan-time. Wire `window.events.closed` (or whatever the installed version's actual event API is) to explicitly terminate the uvicorn subprocess before the Python process exits.
- **Mirror**: N/A — new integration surface.
- **Validate**: Real launch, visually confirm frameless (no OS titlebar), confirm the window renders the app's own HTML titlebar.

### Task 4: Frontend — wire the 3 titlebar buttons
- **Action**: Add `data-act="winMinimize"`/`"winMaximize"`/`"winClose"` to `titlebar()`'s three button divs (frontend/index.html, currently plain `<div>`s with zero `data-act`). Add `H.winMinimize`/`H.winMaximize`/`H.winClose`, each calling `window.pywebview.api.<method>()` guarded by `if(window.pywebview&&window.pywebview.api)` so the browser-tab dev workflow (no pywebview present) stays a harmless no-op, not a thrown error.
- **Mirror**: `BACKEND_AVAILABLE`-style graceful-degradation checks already used throughout the frontend (M6-M8).
- **Validate**: Click all three inside the real pywebview window; confirm minimize/maximize/restore/close all work; confirm clicking them in a plain browser tab (`main.py` standalone) does nothing and throws no console error.

### Task 5: Verify `main.py` standalone workflow is unaffected
- **Action**: No code change expected — just confirm.
- **Validate**: `uvicorn main:app --reload` still starts cleanly and every screen still works in a browser tab, exactly as tested throughout M6-M8.

### Task 6: End-to-end desktop validation
- **Action**: Launch `python3 app.py` for real. Confirm: frameless window opens; titlebar buttons all work; real AniList OAuth login completes **from inside the pywebview window** (not a browser tab) — this needs explicit verification since pywebview embeds a real browser engine and FR-22's flow redirects to `graphql.anilist.co`'s domain and back to the fixed `:8931` callback, a genuinely different runtime context than every prior test this session; Library/Discover/Feed/Profile all work identically to the browser; log out; close via the custom close button; then run a real `ps aux | grep uvicorn` (or `pgrep -af uvicorn`) and confirm **zero** matching processes remain.
- **Mirror**: Every milestone this session's "confirm it works by actually running it against real AniList data" standard.
- **Validate**: The `ps aux`/`pgrep` output itself is the proof, not "the window closed."

## Validation

```bash
python3 -c "import webview; print(webview.__version__)"   # Task 1
python3 -c "import app"                                     # Task 2 syntax/import check (window launch guarded)
node --check <extracted-script>                             # Task 4 frontend syntax check
uvicorn main:app --reload                                   # Task 5 -- standalone workflow unaffected
python3 app.py                                               # Task 6 -- real end-to-end launch
pgrep -af uvicorn                                            # Task 6 -- must show nothing after closing the window
```

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| This container's pywebview Linux backend (GTK+WebKit2) fails to install or render in a headless/constrained container even after installing system packages | Medium — this is genuinely new territory for this environment, unlike every prior HTTP-only milestone | Install and attempt for real per your confirmation; if the window genuinely can't render here, fall back to validating everything except the live render (port logic, subprocess lifecycle, frontend wiring) and say so explicitly rather than claiming a launch that didn't happen |
| Uvicorn subprocess doesn't terminate cleanly on `terminate()` (SIGTERM ignored/slow) | Low-Medium | Add a bounded wait then `.kill()` (SIGKILL) fallback; verify with `pgrep`, not just assuming the call succeeded |
| pywebview's exact method/event names differ from assumed (`minimize()`, `events.closed`, etc.) across versions | Low-Medium | Introspect the actually-installed version directly (`help(webview.Window)`) before writing `WindowApi`, don't guess from memory |
| OAuth redirect behaves differently inside pywebview's embedded engine vs. a full browser (popup blocking, cookie/session quirks, window-open behavior) | Low-Medium | Explicit dedicated test in Task 6, not assumed from prior browser-based OAuth tests |

## Acceptance
- [ ] `app.py` launches a frameless pywebview window pointed at a dynamically-found free port, never colliding with 8931
- [ ] Minimize/maximize/close buttons all work for real, calling actual `js_api` methods
- [ ] Closing the window leaves zero orphaned uvicorn processes, confirmed via `ps`/`pgrep`, not visual inspection
- [ ] `main.py` standalone browser workflow (`uvicorn main:app --reload`) still works unchanged
- [ ] Real AniList OAuth login verified working from inside the pywebview window specifically
- [ ] Library/Discover/Feed/Profile confirmed working identically to the browser-tested version
