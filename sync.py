"""FR-23 (Pull + per-entry conflict resolution) and FR-24/FR-26 (Push + retry
queue) orchestration. database.py stays pure data access, anilist.py stays a
pure external-API client -- this module is where the two meet.
"""
import asyncio
import logging
from datetime import datetime, timezone

import httpx

import anilist
import database

log = logging.getLogger(__name__)

_push_queue: set[int] = set()
_PUSH_RETRY_INTERVAL_SECONDS = 30


async def pull() -> dict:
    """FR-23. One-sided changes apply automatically; both-sides-changed entries
    come back as conflicts and are NOT auto-resolved."""
    auth = database.get_auth()
    if not auth:
        raise RuntimeError("not connected")

    imported = 0
    updated = 0
    conflicts = []
    now = datetime.now(timezone.utc).isoformat()

    for media_type in ("anime", "manga"):
        entries = await anilist.get_media_list_collection(
            auth["access_token"], auth["anilist_user_id"], media_type
        )
        for entry in entries:
            local = database.get_media_by_anilist_id(entry["anilist_id"])

            if not local:
                # New AniList entry, no local counterpart: import directly, no prompt.
                media_data = {
                    k: v
                    for k, v in entry.items()
                    if k not in ("list_entry_id", "status", "progress", "repeat_count", "score", "updated_at")
                }
                media_id = database.insert_or_update_media(media_data)
                # AniList's status/progress/repeat/score land exactly as-is,
                # with updated_at == last_synced_at, so the row starts out in
                # sync: not run through update_progress()'s local auto-complete
                # rules, and not stamped as a local edit (which made the next
                # remote change look like a conflict).
                database.apply_remote_entry(
                    media_id, entry["status"], entry["progress"], entry["repeat_count"], entry["score"], now
                )
                database.set_anilist_list_entry_id(media_id, entry["list_entry_id"])
                imported += 1
                continue

            # Backfill for rows imported before anilist_list_entry_id existed --
            # the id is stable once AniList creates the entry, so any prior Pull's
            # rows can be caught up here regardless of which branch they hit below.
            if local.get("anilist_list_entry_id") is None:
                database.set_anilist_list_entry_id(local["id"], entry["list_entry_id"])

            outcome = database.detect_conflict(
                local.get("updated_at"), entry["updated_at"], local.get("last_synced_at")
            )
            if outcome == "conflict":
                # FR-58: same pending_conflict flag push_now_guarded() sets, so
                # GET /sync/pending-conflicts and POST /sync/resolve work
                # identically regardless of which path found the conflict.
                database.set_pending_conflict(local["id"], True)
                conflicts.append(
                    {
                        "media_id": local["id"],
                        "title": local["title"],
                        "title_english": local["title_english"],
                        "local": {
                            "status": local["status"],
                            "progress": local["progress"],
                            "updated_at": local.get("updated_at"),
                        },
                        "remote": {
                            "status": entry["status"],
                            "progress": entry["progress"],
                            "updatedAt": entry["updated_at"],
                        },
                    }
                )
            elif outcome == "remote":
                database.apply_remote_entry(
                    local["id"], entry["status"], entry["progress"], entry["repeat_count"], entry["score"], now
                )
                updated += 1
            elif outcome == "local":
                # Local wins but isn't applied here -- FR-24 pushes it separately.
                # Just mark it seen so it doesn't look changed again next pull.
                database.set_last_synced(local["id"], now)
            # outcome == "none": nothing changed on either side, nothing to do.

    return {"imported": imported, "updated": updated, "conflicts": conflicts}


async def resolve(media_id: int, winner: str) -> dict:
    """FR-23's per-entry conflict resolution."""
    auth = database.get_auth()
    if not auth:
        raise RuntimeError("not connected")
    local = database.get_media_by_id(media_id)
    if not local:
        raise ValueError("media not found")

    now = datetime.now(timezone.utc).isoformat()
    if winner == "remote":
        entries = await anilist.get_media_list_collection(
            auth["access_token"], auth["anilist_user_id"], local["media_type"]
        )
        remote_entry = next((e for e in entries if e["anilist_id"] == local["anilist_id"]), None)
        if not remote_entry:
            raise ValueError("remote entry not found")
        database.apply_remote_entry(
            media_id,
            remote_entry["status"],
            remote_entry["progress"],
            remote_entry["repeat_count"],
            remote_entry["score"],
            now,
        )
        database.set_pending_conflict(media_id, False)
        return {"status": remote_entry["status"], "progress": remote_entry["progress"]}

    if winner == "local":
        database.set_last_synced(media_id, now)
        database.set_pending_conflict(media_id, False)
        # Explicit user choice already made -- push_now() directly, deliberately
        # bypassing push_now_guarded()'s conflict check (FR-58 plan point 3).
        await push_now(media_id)
        refreshed = database.get_media_by_id(media_id)
        return {"status": refreshed["status"], "progress": refreshed["progress"]}

    raise ValueError(f"invalid winner: {winner!r}")


async def push_now(media_id: int) -> dict:
    """FR-24/FR-25/FR-28. Transient network failures get queued for retry
    (FR-26); permanent errors (not connected, media doesn't exist) are NOT
    queued -- retrying those forever would never succeed."""
    auth = database.get_auth()
    if not auth:
        raise RuntimeError("not connected")
    local = database.get_media_by_id(media_id)
    if not local:
        raise ValueError("media not found")

    try:
        # FR-28: re-check scoreFormat on every sync, never silently reapply.
        viewer = await anilist.get_viewer(auth["access_token"])
        current_format = viewer["score_format"]
        if auth.get("score_format") != current_format:
            database.update_score_format(current_format)
        if current_format != "POINT_10_DECIMAL":
            return {"synced_at": None, "score_format_drift": True}

        overall = (local.get("rating") or {}).get("overall")
        result = await anilist.push_list_entry(
            auth["access_token"],
            local["anilist_id"],
            local["status"],
            local["progress"],
            local["repeat_count"],
            overall,
        )
        now = datetime.now(timezone.utc).isoformat()
        database.set_last_synced(media_id, now)
        # A brand-new local-only entry has no anilist_list_entry_id until its
        # first successful push creates one on AniList's side.
        if local.get("anilist_list_entry_id") is None:
            database.set_anilist_list_entry_id(media_id, result["id"])
        _push_queue.discard(media_id)
        return {"synced_at": now, "score_format_drift": False}
    except httpx.HTTPError:
        log.warning("push failed for media_id=%s (network), queued for retry", media_id)
        _push_queue.add(media_id)
        raise


async def delete_remote_entry(media_id: int) -> None:
    """Deletes the entry from the account's real AniList list before the
    caller deletes the local row -- sequenced this way, and left to raise on
    failure, so a failed AniList call never leaves local and remote diverged
    (no local delete happens unless this either succeeds or has nothing to do).
    A local row with no anilist_list_entry_id was never actually synced, so
    there's genuinely nothing on AniList to remove -- not an error."""
    auth = database.get_auth()
    if not auth:
        raise RuntimeError("not connected")
    local = database.get_media_by_id(media_id)
    if not local:
        raise ValueError("media not found")
    entry_id = local.get("anilist_list_entry_id")
    if entry_id is None:
        return
    await anilist.delete_list_entry(auth["access_token"], entry_id)


async def push_now_guarded(media_id: int) -> dict:
    """FR-58. The automatic push paths (background tasks on local edits, and
    the retry queue) must not blindly overwrite a remote change made during
    the same window -- unlike push_now() itself, which sync.resolve()'s
    winner=="local" branch calls directly and deliberately un-guarded, since
    that path only runs after the user has already been shown both sides and
    made an explicit choice.

    A local entry with NO remote counterpart at all is NOT a conflict -- it's
    the ordinary "first push creates the AniList entry" case FR-24 describes,
    and detect_conflict() (built for Pull, where the remote side always
    exists by construction) would incorrectly flag it as one if fed a bare
    None for the remote timestamp. So existence is checked before ever
    calling detect_conflict().
    """
    auth = database.get_auth()
    if not auth:
        raise RuntimeError("not connected")
    local = database.get_media_by_id(media_id)
    if not local:
        raise ValueError("media not found")

    entries = await anilist.get_media_list_collection(
        auth["access_token"], auth["anilist_user_id"], local["media_type"]
    )
    remote_entry = next((e for e in entries if e["anilist_id"] == local["anilist_id"]), None)

    if remote_entry is None:
        return await push_now(media_id)

    outcome = database.detect_conflict(
        local.get("updated_at"), remote_entry["updated_at"], local.get("last_synced_at")
    )
    if outcome == "conflict":
        database.set_pending_conflict(media_id, True)
        # Retrying automatically won't resolve a conflict -- only an explicit
        # POST /sync/resolve can. Stop the retry loop from hammering it.
        _push_queue.discard(media_id)
        log.warning("automatic push skipped for media_id=%s: conflict with remote change", media_id)
        return {"synced_at": None, "score_format_drift": False, "conflict": True}

    return await push_now(media_id)


async def retry_loop() -> None:
    """FR-26: background task started at FastAPI startup. Scoped to this
    process's lifetime -- persistence across restarts is an app.py concern
    for a later milestone."""
    while True:
        await asyncio.sleep(_PUSH_RETRY_INTERVAL_SECONDS)
        if not database.is_connected():
            continue
        for media_id in list(_push_queue):
            try:
                await push_now_guarded(media_id)
                log.info("retry succeeded for media_id=%s", media_id)
            except Exception:
                pass  # stays queued, tries again next interval


async def get_pending_conflicts() -> list[dict]:
    """FR-58. Same response shape as sync/pull's conflicts array, built the
    same way (live remote fetch, not a stale snapshot from detection time) --
    covers conflicts from either source (Pull or push_now_guarded), since
    both set the same pending_conflict flag."""
    auth = database.get_auth()
    if not auth:
        raise RuntimeError("not connected")

    rows = database.get_pending_conflict_rows()
    if not rows:
        return []

    entries_by_type: dict[str, list[dict]] = {}
    conflicts = []
    for local in rows:
        mt = local["media_type"]
        if mt not in entries_by_type:
            entries_by_type[mt] = await anilist.get_media_list_collection(
                auth["access_token"], auth["anilist_user_id"], mt
            )
        remote_entry = next(
            (e for e in entries_by_type[mt] if e["anilist_id"] == local["anilist_id"]), None
        )
        conflicts.append(
            {
                "media_id": local["id"],
                "title": local["title"],
                "title_english": local["title_english"],
                "local": {
                    "status": local["status"],
                    "progress": local["progress"],
                    "updated_at": local.get("updated_at"),
                },
                "remote": {
                    "status": remote_entry["status"] if remote_entry else None,
                    "progress": remote_entry["progress"] if remote_entry else None,
                    "updatedAt": remote_entry["updated_at"] if remote_entry else None,
                },
            }
        )
    return conflicts
