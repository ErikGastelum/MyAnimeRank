//! FR-23 (Pull + per-entry conflict resolution) and FR-24/FR-26 (Push + retry
//! queue). Port of sync.py: db.rs stays pure data access, anilist.rs stays a
//! pure external-API client, and this module is where the two meet.

use std::time::Duration;

use serde_json::{json, Value};

use crate::anilist::{AlError, ListEntry};
use crate::db::{detect_conflict, now_iso, Media, SyncOutcome};
use crate::Shared;

const PUSH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum SyncError {
    NotConnected,
    /// Python's ValueError: the media (or its remote entry) doesn't exist.
    NotFound(String),
    AniList(AlError),
    Db(rusqlite::Error),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::NotConnected => f.write_str("not connected"),
            SyncError::NotFound(m) => f.write_str(m),
            SyncError::AniList(e) => write!(f, "{e}"),
            SyncError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl From<AlError> for SyncError {
    fn from(e: AlError) -> Self {
        SyncError::AniList(e)
    }
}

impl From<rusqlite::Error> for SyncError {
    fn from(e: rusqlite::Error) -> Self {
        SyncError::Db(e)
    }
}

pub type SyncResult<T> = Result<T, SyncError>;

fn auth(st: &Shared) -> SyncResult<crate::db::Auth> {
    st.db.get_auth()?.ok_or(SyncError::NotConnected)
}

fn local(st: &Shared, media_id: i64) -> SyncResult<Media> {
    st.db.get_media_by_id(media_id)?.ok_or_else(|| SyncError::NotFound("media not found".into()))
}

fn conflict_entry(local: &Media, remote: Option<(&str, i64, Option<&str>)>) -> Value {
    json!({
        "media_id": local.id,
        "title": local.fields.title,
        "title_english": local.fields.title_english,
        "local": {"status": local.status, "progress": local.progress, "updated_at": local.updated_at},
        "remote": {
            "status": remote.map(|r| r.0),
            "progress": remote.map(|r| r.1),
            "updatedAt": remote.and_then(|r| r.2),
        },
    })
}

/// FR-23. One-sided changes apply automatically; both-sides-changed entries
/// come back as conflicts and are NOT auto-resolved.
pub async fn pull(st: &Shared) -> SyncResult<Value> {
    let auth = auth(st)?;
    let (mut imported, mut updated) = (0, 0);
    let mut conflicts = Vec::new();
    let now = now_iso();

    for media_type in ["anime", "manga"] {
        let entries = st
            .anilist
            .get_media_list_collection(&auth.access_token, auth.anilist_user_id, media_type)
            .await?;
        for entry in entries {
            let Some(local) = st.db.get_media_by_anilist_id(entry.media.anilist_id)? else {
                // New AniList entry with no local row: import directly. Its
                // status/progress/score land exactly as AniList has them, with
                // updated_at == last_synced_at, so the row starts in sync.
                let media_id = st.db.insert_or_update_media(&entry.media, None)?;
                st.db.apply_remote_entry(media_id, &entry.status, entry.progress, entry.repeat_count, entry.score, &now)?;
                st.db.set_anilist_list_entry_id(media_id, entry.list_entry_id)?;
                imported += 1;
                continue;
            };

            // Backfill for rows imported before anilist_list_entry_id existed.
            if local.anilist_list_entry_id.is_none() {
                st.db.set_anilist_list_entry_id(local.id, entry.list_entry_id)?;
            }

            match detect_conflict(local.updated_at.as_deref(), entry.updated_at.as_deref(), local.last_synced_at.as_deref()) {
                SyncOutcome::Conflict => {
                    // FR-58: same flag push_now_guarded() sets.
                    st.db.set_pending_conflict(local.id, true)?;
                    conflicts.push(conflict_entry(
                        &local,
                        Some((&entry.status, entry.progress, entry.updated_at.as_deref())),
                    ));
                }
                SyncOutcome::Remote => {
                    st.db.apply_remote_entry(local.id, &entry.status, entry.progress, entry.repeat_count, entry.score, &now)?;
                    updated += 1;
                }
                // Local wins but is pushed separately (FR-24); just mark seen.
                SyncOutcome::Local => st.db.set_last_synced(local.id, &now)?,
                SyncOutcome::None => {}
            }
        }
    }
    Ok(json!({"imported": imported, "updated": updated, "conflicts": conflicts}))
}

async fn find_remote(st: &Shared, auth: &crate::db::Auth, local: &Media) -> SyncResult<Option<ListEntry>> {
    let entries = st
        .anilist
        .get_media_list_collection(&auth.access_token, auth.anilist_user_id, &local.fields.media_type)
        .await?;
    Ok(entries.into_iter().find(|e| e.media.anilist_id == local.fields.anilist_id))
}

/// FR-23's per-entry conflict resolution.
pub async fn resolve(st: &Shared, media_id: i64, winner: &str) -> SyncResult<Value> {
    let auth = auth(st)?;
    let local = local(st, media_id)?;
    let now = now_iso();
    if winner == "remote" {
        let remote = find_remote(st, &auth, &local)
            .await?
            .ok_or_else(|| SyncError::NotFound("remote entry not found".into()))?;
        st.db.apply_remote_entry(media_id, &remote.status, remote.progress, remote.repeat_count, remote.score, &now)?;
        st.db.set_pending_conflict(media_id, false)?;
        return Ok(json!({"status": remote.status, "progress": remote.progress}));
    }
    st.db.set_last_synced(media_id, &now)?;
    st.db.set_pending_conflict(media_id, false)?;
    // Explicit user choice already made: push directly, deliberately
    // bypassing push_now_guarded()'s conflict check.
    push_now(st, media_id, None).await?;
    let refreshed = local_after(st, media_id)?;
    Ok(json!({"status": refreshed.status, "progress": refreshed.progress}))
}

fn local_after(st: &Shared, media_id: i64) -> SyncResult<Media> {
    local(st, media_id)
}

fn queue(st: &Shared) -> std::sync::MutexGuard<'_, std::collections::BTreeSet<i64>> {
    st.push_queue.lock().unwrap_or_else(|e| e.into_inner())
}

/// FR-24/FR-25/FR-28. Transient network failures are queued for retry
/// (FR-26); permanent errors are not, since retrying them never succeeds.
/// `score_format` is one the caller already fetched fresh for this push;
/// when None it is fetched here. Either way it's re-checked every push.
pub async fn push_now(st: &Shared, media_id: i64, score_format: Option<String>) -> SyncResult<Value> {
    let auth = auth(st)?;
    let local = local(st, media_id)?;
    match push_inner(st, &auth, &local, score_format).await {
        Err(SyncError::AniList(e)) if e.is_network() => {
            log::warn!("push failed for media_id={media_id} (network), queued for retry");
            queue(st).insert(media_id);
            Err(SyncError::AniList(e))
        }
        other => other,
    }
}

async fn push_inner(st: &Shared, auth: &crate::db::Auth, local: &Media, score_format: Option<String>) -> SyncResult<Value> {
    let score_format = match score_format {
        Some(f) => Some(f),
        None => st.anilist.get_viewer(&auth.access_token).await?.score_format,
    };
    if auth.score_format != score_format {
        st.db.update_score_format(score_format.as_deref())?;
    }
    if score_format.as_deref() != Some("POINT_10_DECIMAL") {
        return Ok(json!({"synced_at": null, "score_format_drift": true}));
    }
    let overall = local.rating.as_ref().map(|r| r.overall);
    let entry_id = st
        .anilist
        .push_list_entry(&auth.access_token, local.fields.anilist_id, &local.status, local.progress, local.repeat_count, overall)
        .await?;
    let now = now_iso();
    st.db.set_last_synced(local.id, &now)?;
    // A brand-new local-only entry gets its list entry id from its first push.
    if local.anilist_list_entry_id.is_none() {
        st.db.set_anilist_list_entry_id(local.id, entry_id)?;
    }
    queue(st).remove(&local.id);
    Ok(json!({"synced_at": now, "score_format_drift": false}))
}

/// Deletes the entry from the real AniList list BEFORE the caller deletes the
/// local row, so a failed AniList call never leaves the two diverged. A row
/// that was never synced has nothing on AniList to remove.
pub async fn delete_remote_entry(st: &Shared, media_id: i64) -> SyncResult<()> {
    let auth = auth(st)?;
    let local = local(st, media_id)?;
    if let Some(entry_id) = local.anilist_list_entry_id {
        st.anilist.delete_list_entry(&auth.access_token, entry_id).await?;
    }
    Ok(())
}

/// FR-58. Automatic pushes (background on local edits, and retries) must not
/// blindly overwrite a remote change made in the same window. A local entry
/// with NO remote counterpart is the ordinary first push, not a conflict.
pub async fn push_now_guarded(st: &Shared, media_id: i64) -> SyncResult<Value> {
    let auth = auth(st)?;
    let local = local(st, media_id)?;
    let (remote, score_format) = match st
        .anilist
        .get_list_entry(&auth.access_token, auth.anilist_user_id, local.fields.anilist_id, &local.fields.media_type)
        .await
    {
        Ok(r) => r,
        Err(e) if e.is_network() => {
            log::warn!("pre-push lookup failed for media_id={media_id} (network), queued for retry");
            queue(st).insert(media_id);
            return Err(e.into());
        }
        Err(e) => return Err(e.into()),
    };
    let Some(remote) = remote else {
        return push_now(st, media_id, None).await;
    };
    if detect_conflict(local.updated_at.as_deref(), remote.updated_at.as_deref(), local.last_synced_at.as_deref())
        == SyncOutcome::Conflict
    {
        st.db.set_pending_conflict(media_id, true)?;
        // Only an explicit POST /sync/resolve can settle a conflict.
        queue(st).remove(&media_id);
        log::warn!("automatic push skipped for media_id={media_id}: conflict with remote change");
        return Ok(json!({"synced_at": null, "score_format_drift": false, "conflict": true}));
    }
    push_now(st, media_id, score_format).await
}

/// Background push for a local edit: never awaited by the request, so a slow
/// or unreachable AniList never blocks the response.
pub fn spawn_push(st: &Shared, media_id: i64) {
    let st = st.clone();
    tokio::spawn(async move {
        if let Err(e) = push_now_guarded(&st, media_id).await {
            log::warn!("background push for media_id={media_id} failed: {e}");
        }
    });
}

/// FR-26: runs for the app's lifetime. The queue is in memory, same as before.
pub async fn retry_loop(st: Shared) {
    loop {
        tokio::time::sleep(PUSH_RETRY_INTERVAL).await;
        if !st.db.is_connected() {
            continue;
        }
        let ids: Vec<i64> = queue(&st).iter().copied().collect();
        for media_id in ids {
            match push_now_guarded(&st, media_id).await {
                Ok(_) => log::info!("retry succeeded for media_id={media_id}"),
                Err(e) => log::error!("retry failed for media_id={media_id}; still queued: {e}"),
            }
        }
    }
}

/// FR-58. Same shape as Pull's conflicts, built from a live remote fetch.
pub async fn get_pending_conflicts(st: &Shared) -> SyncResult<Vec<Value>> {
    let auth = auth(st)?;
    let rows = st.db.get_pending_conflict_rows()?;
    let mut by_type: std::collections::HashMap<String, Vec<ListEntry>> = Default::default();
    let mut conflicts = Vec::new();
    for local in rows {
        let mt = local.fields.media_type.clone();
        if !by_type.contains_key(&mt) {
            let entries = st.anilist.get_media_list_collection(&auth.access_token, auth.anilist_user_id, &mt).await?;
            by_type.insert(mt.clone(), entries);
        }
        let remote = by_type[&mt].iter().find(|e| e.media.anilist_id == local.fields.anilist_id);
        conflicts.push(conflict_entry(
            &local,
            remote.map(|r| (r.status.as_str(), r.progress, r.updated_at.as_deref())),
        ));
    }
    Ok(conflicts)
}
