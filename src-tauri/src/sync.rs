//! FR-23 (Pull + per-entry conflict resolution), FR-24/FR-26 (Push + retry
//! queue) and FR-79/FR-80 (friend sync + friend-list queue). Port of sync.py:
//! db.rs stays pure data access, anilist.rs stays a pure external-API client,
//! and this module is where the two meet.

use std::time::Duration;

use tokio::sync::watch;

use serde_json::{json, Value};

use crate::anilist::{AlError, AniList, FriendLookup, ListEntry};
use crate::db::{detect_conflict, now_iso, Media, Scores, SyncOutcome, CRITERIA_BACKFILL_DONE};
use crate::limiter::{Lane, Priority};
use crate::scoring::{self, ListOptions};
use crate::Shared;

const PUSH_RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// FR-86: within a retry round, one title at most this often.
pub const PUSH_RETRY_SPACING: Duration = Duration::from_secs(5);
/// FR-80: one queue turn (one friend, two requests) at most this often: at
/// most 10 requests a minute, on top of the shared limiter (FR-86).
pub const FRIEND_QUEUE_SPACING: Duration = Duration::from_secs(12);

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

/// What one pulled entry did locally.
enum Pulled {
    Imported,
    Updated,
    Conflict(Value),
    Unchanged,
}

/// FR-23. One-sided changes apply automatically; both-sides-changed entries
/// come back as conflicts and are NOT auto-resolved. At normal priority (FR-86).
///
/// Criteria come from AniList's advanced scores (see `criteria_step`). The
/// first pull that reads them is also the one-time backfill: until it has
/// gone through both lists, a title whose real local criteria differ from
/// AniList's is never changed on either side but listed in
/// `criteria_conflicts`, for the user to settle (`resolve_criteria`).
pub async fn pull(st: &Shared) -> SyncResult<Value> {
    let auth = auth(st)?;
    let (mut imported, mut updated) = (0, 0);
    let mut conflicts = Vec::new();
    let now = now_iso();
    let anilist = st.anilist.at(Priority::Normal);
    let ctx = PullContext {
        now: &now,
        format: auth.score_format.as_deref(),
        backfill: st.db.get_meta(CRITERIA_BACKFILL_DONE)?.is_none(),
    };

    for media_type in ["anime", "manga"] {
        let entries = anilist
            .get_media_list_collection(&auth.access_token, auth.anilist_user_id, media_type)
            .await?;
        for entry in entries {
            // One entry failing (say, a write that fails for a moment) must
            // not stop the rest. It's left untouched locally, so the next
            // pull tries it again.
            match pull_entry(st, &entry, &ctx) {
                Ok(Pulled::Imported) => imported += 1,
                Ok(Pulled::Updated) => updated += 1,
                Ok(Pulled::Conflict(c)) => conflicts.push(c),
                Ok(Pulled::Unchanged) => {}
                Err(e) => log::error!("pull skipped AniList media {} ({media_type}): {e}", entry.media.anilist_id),
            }
        }
    }
    // Both lists read: the backfill has seen every entry once.
    if ctx.backfill {
        st.db.set_meta(CRITERIA_BACKFILL_DONE, &now)?;
        log::info!("advanced-score backfill done");
    }
    Ok(json!({
        "imported": imported,
        "updated": updated,
        "conflicts": conflicts,
        "criteria_conflicts": st.db.get_criteria_conflicts()?,
    }))
}

struct PullContext<'a> {
    now: &'a str,
    format: Option<&'a str>,
    /// The one-time advanced-score backfill hasn't finished yet.
    backfill: bool,
}

/// What a pull does with AniList's criteria (advanced scores) for a title
/// already in the library.
#[derive(Debug, PartialEq)]
enum CriteriaStep {
    /// They become the title's real criteria.
    Take(Scores),
    /// Real local criteria differ, found by the backfill: neither side
    /// changes, and the user is asked.
    Ask(Scores),
    Leave,
}

/// `asking`: the backfill already asked about this title and the user
/// hasn't answered, so it keeps asking whatever AniList does meanwhile.
fn criteria_step(local: &Media, remote: Option<&Scores>, outcome: SyncOutcome, backfill: bool, asking: bool) -> CriteriaStep {
    let Some(remote) = remote.copied() else { return CriteriaStep::Leave };
    // Copied values (or no rating) carry nothing AniList's real ones don't.
    let Some(real) = local.rating.as_ref().filter(|r| r.source == "app") else {
        return CriteriaStep::Take(remote);
    };
    if scoring::same_criteria(&local.fields.media_type, real, &remote) {
        CriteriaStep::Leave
    } else if backfill || asking {
        // Local criteria never reached AniList before, so a later AniList
        // updatedAt doesn't mean these criteria changed there.
        CriteriaStep::Ask(remote)
    } else if outcome == SyncOutcome::Remote {
        CriteriaStep::Take(remote)
    } else {
        CriteriaStep::Leave
    }
}

/// FR-23 with FR-25's conversion: when AniList's score is just what pushing
/// the local overall in `format` left there (7.9 pushed as POINT_10 reads
/// back 8.0), the local decimal stays.
fn remote_score(local: &Media, remote: Option<f64>, format: Option<&str>) -> Option<f64> {
    match (local.rating.as_ref(), remote) {
        (Some(r), Some(s)) if scoring::same_on_anilist(r.overall, s, format) => Some(r.overall),
        _ => remote,
    }
}

fn pull_entry(st: &Shared, entry: &ListEntry, ctx: &PullContext) -> SyncResult<Pulled> {
    let (now, format) = (ctx.now, ctx.format);
    let Some(local) = st.db.get_media_by_anilist_id(entry.media.anilist_id)? else {
        // New AniList entry with no local row: import directly. Its
        // status/progress/score land exactly as AniList has them, with
        // updated_at == last_synced_at, so the row starts in sync. Its
        // advanced scores, when complete, are its real criteria; otherwise
        // the criteria are copied from the overall.
        st.db.import_remote_entry(
            &entry.media,
            entry.list_entry_id,
            &entry.status,
            entry.progress,
            entry.repeat_count,
            entry.score,
            entry.criteria.as_ref(),
            now,
        )?;
        return Ok(Pulled::Imported);
    };

    // Backfill for rows imported before anilist_list_entry_id existed.
    if local.anilist_list_entry_id.is_none() {
        st.db.set_anilist_list_entry_id(local.id, entry.list_entry_id)?;
    }
    // FR-77's tie-break, from this same request.
    if let Some(popularity) = entry.media.popularity.filter(|p| local.fields.popularity != Some(*p)) {
        st.db.set_media_popularity(local.id, popularity)?;
    }

    let outcome = detect_conflict(local.updated_at.as_deref(), entry.updated_at.as_deref(), local.last_synced_at.as_deref());
    let asking = st.db.get_criteria_conflict(local.id)?.is_some();
    let step = criteria_step(&local, entry.criteria.as_ref(), outcome, ctx.backfill, asking);
    if let CriteriaStep::Ask(remote) = &step {
        st.db.save_criteria_conflict(local.id, remote)?;
    }
    Ok(match outcome {
        SyncOutcome::Remote => {
            let (score, criteria) = match &step {
                CriteriaStep::Take(c) => (entry.score, Some(c)),
                // Neither rating side changes while the user is asked.
                CriteriaStep::Ask(_) => (None, None),
                CriteriaStep::Leave => (remote_score(&local, entry.score, format), None),
            };
            st.db.apply_remote_entry(local.id, &entry.status, entry.progress, entry.repeat_count, score, criteria, now)?;
            Pulled::Updated
        }
        other => {
            // Only copied (or missing) criteria are taken outside a remote
            // change; it isn't a local edit, so nothing is pushed for it.
            let took = match &step {
                CriteriaStep::Take(c) => {
                    st.db.set_criteria_from_anilist(local.id, c)?;
                    true
                }
                _ => false,
            };
            match other {
                SyncOutcome::Conflict => {
                    // FR-58: same flag push_now_guarded() sets.
                    st.db.set_pending_conflict(local.id, true)?;
                    Pulled::Conflict(conflict_entry(&local, Some((&entry.status, entry.progress, entry.updated_at.as_deref()))))
                }
                // Local wins but is pushed separately (FR-24); just mark seen.
                SyncOutcome::Local => {
                    st.db.set_last_synced(local.id, now)?;
                    if took { Pulled::Updated } else { Pulled::Unchanged }
                }
                _ if took => Pulled::Updated,
                _ => Pulled::Unchanged,
            }
        }
    })
}

/// Settles one criteria conflict the backfill found. "anilist": AniList's
/// criteria replace the local ones (nothing is pushed). "local": the local
/// criteria stay and are pushed to AniList as advanced scores right away
/// (an explicit choice, so without push_now_guarded's conflict check).
pub async fn resolve_criteria(st: &Shared, media_id: i64, winner: &str) -> SyncResult<Value> {
    let anilist = st
        .db
        .get_criteria_conflict(media_id)?
        .ok_or_else(|| SyncError::NotFound("no criteria conflict for this title".into()))?;
    if winner == "anilist" {
        st.db.set_criteria_from_anilist(media_id, &anilist)?;
        return Ok(json!({"pushed": false}));
    }
    st.db.keep_local_criteria(media_id)?;
    push_now(st, media_id, None, Priority::Interactive).await?;
    Ok(json!({"pushed": true}))
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
        let score = remote_score(&local, remote.score, auth.score_format.as_deref());
        // The user chose AniList's side, its criteria included.
        st.db.apply_remote_entry(media_id, &remote.status, remote.progress, remote.repeat_count, score, remote.criteria.as_ref(), &now)?;
        st.db.set_pending_conflict(media_id, false)?;
        return Ok(json!({"status": remote.status, "progress": remote.progress}));
    }
    st.db.set_last_synced(media_id, &now)?;
    st.db.set_pending_conflict(media_id, false)?;
    // Explicit user choice already made: push directly, deliberately
    // bypassing push_now_guarded()'s conflict check.
    push_now(st, media_id, None, Priority::Interactive).await?;
    let refreshed = local_after(st, media_id)?;
    Ok(json!({"status": refreshed.status, "progress": refreshed.progress}))
}

fn local_after(st: &Shared, media_id: i64) -> SyncResult<Media> {
    local(st, media_id)
}

fn queue(st: &Shared) -> std::sync::MutexGuard<'_, std::collections::BTreeSet<i64>> {
    st.push_queue.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a push needs to know about the account and the entry on AniList,
/// fetched fresh for every push (FR-28).
#[derive(Debug, Clone, Default)]
pub struct PushLookup {
    /// scoreFormat and the advanced-scoring categories.
    pub options: ListOptions,
    /// The entry's advanced scores as AniList has them now, if it's on the list.
    pub remote_advanced: Option<std::collections::BTreeMap<String, f64>>,
}

async fn lookup(anilist: &AniList, auth: &crate::db::Auth, local: &Media) -> SyncResult<PushLookup> {
    let (remote, options) = anilist
        .get_list_entry(&auth.access_token, auth.anilist_user_id, local.fields.anilist_id, &local.fields.media_type)
        .await?;
    let options = match options {
        Some(o) => o,
        // AniList nulls Viewer when the title isn't on the list.
        None => anilist.get_viewer(&auth.access_token).await?.options,
    };
    Ok(PushLookup { options, remote_advanced: remote.and_then(|r| r.advanced_scores) })
}

/// FR-24/FR-25/FR-28. Transient network failures are queued for retry
/// (FR-26); permanent errors are not, since retrying them never succeeds.
/// `lookup` is one the caller already fetched fresh for this push; when
/// None it is fetched here. Either way the format is re-checked every push.
pub async fn push_now(st: &Shared, media_id: i64, lookup: Option<PushLookup>, priority: Priority) -> SyncResult<Value> {
    let auth = auth(st)?;
    let local = local(st, media_id)?;
    match push_inner(st, &st.anilist.at(priority), &auth, &local, lookup).await {
        Err(SyncError::AniList(e)) if e.is_network() => {
            log::warn!("push failed for media_id={media_id} (network), queued for retry");
            queue(st).insert(media_id);
            Err(SyncError::AniList(e))
        }
        other => other,
    }
}

async fn push_inner(st: &Shared, anilist: &AniList, auth: &crate::db::Auth, local: &Media, lookup: Option<PushLookup>) -> SyncResult<Value> {
    let lookup = match lookup {
        Some(l) => l,
        None => self::lookup(anilist, auth, local).await?,
    };
    let score_format = lookup.options.score_format.as_deref();
    if auth.score_format.as_deref() != score_format {
        st.db.update_score_format(score_format)?;
    }
    // FR-28: only formats with an explicit conversion are written to.
    let Some(format) = score_format.filter(|f| scoring::can_push_in(Some(f))) else {
        return Ok(json!({"synced_at": null, "score_format_drift": true}));
    };
    // The local overall keeps its decimal; only the pushed copy is converted.
    let score = local.rating.as_ref().and_then(|r| scoring::to_anilist_score(r.overall, format));
    let advanced = local
        .rating
        .as_ref()
        .and_then(|r| scoring::advanced_scores(&local.fields.media_type, r, &lookup.options, lookup.remote_advanced.as_ref()));
    let entry_id = anilist
        .push_list_entry(
            &auth.access_token,
            local.fields.anilist_id,
            &local.status,
            local.progress,
            local.repeat_count,
            score,
            advanced.as_deref(),
        )
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
/// Your edits push at interactive priority, retries in the background (FR-86).
pub async fn push_now_guarded(st: &Shared, media_id: i64, priority: Priority) -> SyncResult<Value> {
    let auth = auth(st)?;
    let local = local(st, media_id)?;
    let (remote, options) = match st
        .anilist
        .at(priority)
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
        // Not on the list: AniList nulled Viewer too, so fetch it alone.
        let options = st.anilist.at(priority).get_viewer(&auth.access_token).await.map_err(|e| {
            if e.is_network() {
                queue(st).insert(media_id);
            }
            SyncError::from(e)
        })?;
        return push_now(st, media_id, Some(PushLookup { options: options.options, remote_advanced: None }), priority).await;
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
    let lookup = PushLookup { options: options.unwrap_or_default(), remote_advanced: remote.advanced_scores };
    push_now(st, media_id, Some(lookup), priority).await
}

/// Background push for a local edit: never awaited by the request, so a slow
/// or unreachable AniList never blocks the response.
pub fn spawn_push(st: &Shared, media_id: i64) {
    let st = st.clone();
    tokio::spawn(async move {
        if let Err(e) = push_now_guarded(&st, media_id, Priority::Interactive).await {
            log::warn!("background push for media_id={media_id} failed: {e}");
        }
    });
}

/// FR-26: runs for the app's lifetime. The queue is in memory, same as before.
pub async fn retry_loop(st: Shared) {
    loop {
        tokio::time::sleep(PUSH_RETRY_INTERVAL).await;
        if st.db.is_connected() {
            retry_pushes(&st, PUSH_RETRY_SPACING).await;
        }
    }
}

/// One round of FR-26 retries at background priority (FR-86): one title at
/// a time, `spacing` apart, stopping for this round as soon as background
/// work can't go (no spare budget, or a 429 paused requests). Returns how
/// many titles it tried.
pub async fn retry_pushes(st: &Shared, spacing: Duration) -> usize {
    let ids: Vec<i64> = queue(st).iter().copied().collect();
    let mut tried = 0;
    for media_id in ids {
        if tried > 0 {
            tokio::time::sleep(spacing).await;
        }
        if !st.anilist.is_quiet() {
            log::info!("push retries wait for spare AniList budget; {} still queued", queue(st).len());
            break;
        }
        tried += 1;
        match push_now_guarded(st, media_id, Priority::Background).await {
            Ok(_) => log::info!("retry succeeded for media_id={media_id}"),
            Err(e) => log::error!("retry failed for media_id={media_id}; still queued: {e}"),
        }
    }
    tried
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

// ---------------------------------------------------------------- friends ----

/// FR-79: imports who you follow (everyone, or mutual follows only, per the
/// setting), and fetches no lists. Logged out, except that mutual only reads
/// your own following list with your token (see AniList::get_following).
/// At normal priority (FR-86).
pub async fn sync_friends(st: &Shared) -> SyncResult<()> {
    let auth = auth(st)?;
    let (mutual_only, _) = st.db.friend_sync_state()?;
    let token = mutual_only.then_some(auth.access_token.as_str());
    let followed = st.anilist.at(Priority::Normal).get_following(auth.anilist_user_id, mutual_only, token).await?;
    st.db.sync_followed(&followed, &now_iso())?;
    Ok(())
}

/// FR-79 after a Pull (login included: the page Pulls right after it), in the
/// background, and a failure is only logged.
pub fn sync_friends_later(st: &Shared) {
    let st = st.clone();
    tokio::spawn(async move {
        if let Err(e) = sync_friends(&st).await {
            log::warn!("friend sync failed: {e}");
        }
    });
}

/// FR-71/FR-73: fetches one friend's lists without the token and stores
/// them. `id` is set for someone who's already a friend (Refresh, the queue,
/// a comparison): they show as "fetching" meanwhile, keep their source, and
/// are marked private if AniList says so. Only one such fetch per friend
/// runs at a time: a caller arriving during one waits for it and gets its
/// answer, without a request of its own, after raising that fetch to its
/// own `priority` (FR-86). Without `id`, it's an add by username: stored as
/// a manual friend. Your own account is never stored.
pub async fn fetch_friend(st: &Shared, name: &str, id: Option<i64>, priority: Priority) -> SyncResult<FriendLookup> {
    let auth = auth(st)?;
    let anilist = st.anilist.at(priority);
    let fetching = match id {
        Some(id) => match Fetching::join(st, id, anilist.lane()).await {
            Joined::Running(fetching) => Some(fetching),
            Joined::Answered(answer) => return Ok(answer?),
        },
        None => None,
    };
    let answer = anilist.get_friend_list(name, id).await;
    match (&answer, id) {
        (Ok(FriendLookup::Found(list)), _) if list.anilist_user_id == auth.anilist_user_id => {}
        (Ok(FriendLookup::Found(list)), None) => st.db.replace_friend_list(list, &now_iso())?,
        (Ok(FriendLookup::Found(list)), Some(_)) => {
            st.db.store_friend_list(list, &now_iso())?;
        }
        (Ok(FriendLookup::Private(_)), Some(id)) => st.db.mark_friend_private(id, &now_iso())?,
        _ => {}
    }
    // Stored first, so whoever waited reads the new list.
    if let Some(fetching) = &fetching {
        fetching.answer.send_replace(Some(answer.clone()));
    }
    Ok(answer?)
}

/// Where a running friend fetch's answer arrives: None until it's in.
pub type FetchAnswer = watch::Receiver<Option<Result<FriendLookup, AlError>>>;

/// A running friend fetch: its answer, and the lane its requests go in, for
/// a caller who joins it to raise.
#[derive(Clone)]
pub struct RunningFetch {
    answer: FetchAnswer,
    lane: Lane,
}

/// A friend shows as "fetching" (GET /friends) until this is dropped, and
/// anyone fetching them meanwhile gets this fetch's answer.
struct Fetching<'a> {
    st: &'a Shared,
    id: i64,
    answer: watch::Sender<Option<Result<FriendLookup, AlError>>>,
}

enum Joined<'a> {
    Running(Fetching<'a>),
    Answered(Result<FriendLookup, AlError>),
}

impl<'a> Fetching<'a> {
    /// Starts fetching `id` in `lane`, or, if that's already running, raises
    /// it to `lane`'s priority and waits for its answer. One dropped half-way
    /// answers nothing, so this one runs then.
    async fn join(st: &'a Shared, id: i64, lane: &Lane) -> Joined<'a> {
        loop {
            let RunningFetch { answer: mut running, lane: theirs } = {
                let mut all = st.friend_fetching.lock().unwrap_or_else(|e| e.into_inner());
                match all.get(&id) {
                    Some(running) => running.clone(),
                    None => {
                        let (answer, running) = watch::channel(None);
                        all.insert(id, RunningFetch { answer: running, lane: lane.clone() });
                        return Joined::Running(Fetching { st, id, answer });
                    }
                }
            };
            st.anilist.limiter().raise(&theirs, lane.get());
            let answer = running.wait_for(Option::is_some).await.ok().and_then(|answer| answer.clone());
            if let Some(answer) = answer {
                return Joined::Answered(answer);
            }
        }
    }
}

impl Drop for Fetching<'_> {
    fn drop(&mut self) {
        self.st.friend_fetching.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
    }
}

/// FR-80: the background friend-list queue, one turn per `spacing`.
pub async fn friend_queue_loop(st: Shared, spacing: Duration) {
    loop {
        tokio::time::sleep(spacing).await;
        if let Err(e) = friend_queue_step(&st).await {
            log::warn!("friend list queue: {e}");
        }
    }
}

/// One queue turn: fetches at most one friend's lists (two requests) at
/// background priority, and only while logged in, when background work could
/// go at once (FR-86: nothing else waiting or running, no 429 in the last
/// minute, spare budget). Returns whose fetch it tried.
pub async fn friend_queue_step(st: &Shared) -> SyncResult<Option<i64>> {
    if !st.db.is_connected() || !st.anilist.is_quiet() {
        return Ok(None);
    }
    let Some((id, name)) = st.db.next_friend_to_fetch()? else { return Ok(None) };
    let failed = match fetch_friend(st, &name, Some(id), Priority::Background).await {
        Ok(FriendLookup::NoSuchUser) => true,
        Ok(_) => false,
        // Not reaching AniList is the queue's problem, not this friend's:
        // the next turn tries again (after a 429, a minute later).
        Err(SyncError::AniList(e)) if e.is_network() => return Err(SyncError::AniList(e)),
        Err(SyncError::AniList(_)) => true,
        Err(e) => return Err(e),
    };
    if failed {
        st.db.skip_friend(id)?;
    }
    Ok(Some(id))
}
