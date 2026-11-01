//! FR-86: the one request budget every AniList call in the app shares.
//!
//! AniList allows 30 requests a minute. This sends at most BUDGET in any
//! rolling window, by priority: interactive (someone clicked) may use all of
//! it, normal work (Pull, Discover, Feed, friend sync) leaves some for a
//! click, and background work (the friend-list queue, push retries) only
//! uses what's spare, and only while nothing else is waiting or running.
//! `AniList::graphql_within()` is its only caller.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::Notify;

/// AniList's own limit per window (`X-RateLimit-Limit`, notes #26).
pub const ANILIST_LIMIT: usize = 30;
/// What the app lets itself send per window: 3 under AniList's limit.
pub const BUDGET: usize = 27;
pub const WINDOW: Duration = Duration::from_secs(60);
/// Normal work stops this far short of BUDGET: one worst-case pick (FR-84).
pub const NORMAL_RESERVE: usize = 4;
/// Background work stops this far short of BUDGET.
pub const BACKGROUND_RESERVE: usize = 8;
/// A request never waits longer than this for a 429 pause to end: it fails
/// at once instead ("try again shortly").
pub const MAX_PAUSE_WAIT: Duration = Duration::from_secs(30);
/// No Retry-After is taken at more than an hour.
const MAX_PAUSE: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Background = 0,
    Normal = 1,
    Interactive = 2,
}

impl Priority {
    const ALL: [Priority; 3] = [Priority::Interactive, Priority::Normal, Priority::Background];

    /// The most of the window's budget this priority may use.
    pub fn share(self) -> usize {
        match self {
            Priority::Interactive => BUDGET,
            Priority::Normal => BUDGET - NORMAL_RESERVE,
            Priority::Background => BUDGET - BACKGROUND_RESERVE,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Priority::Interactive => "interactive",
            Priority::Normal => "normal",
            Priority::Background => "background",
        }
    }

    fn from_u8(n: u8) -> Self {
        match n {
            0 => Priority::Background,
            1 => Priority::Normal,
            _ => Priority::Interactive,
        }
    }
}

/// A priority shared by every request of one piece of work, which can be
/// raised while they wait: a comparison joining a queue fetch (FR-81).
#[derive(Clone)]
pub struct Lane(Arc<AtomicU8>);

impl Lane {
    pub fn new(priority: Priority) -> Self {
        Lane(Arc::new(AtomicU8::new(priority as u8)))
    }

    pub fn get(&self) -> Priority {
        Priority::from_u8(self.0.load(Ordering::SeqCst))
    }

    /// Whether this raised it.
    fn raise(&self, priority: Priority) -> bool {
        self.0.fetch_max(priority as u8, Ordering::SeqCst) < priority as u8
    }
}

pub struct Limiter {
    window: Duration,
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    /// When each request counted in the window stops counting: a window after
    /// its answer came back (a window after it was sent while it's still
    /// out), plus any AniList counted that this didn't (observe()).
    counted: Vec<Instant>,
    /// Requests waiting for their turn, in arrival order.
    waiting: Vec<(u64, Lane)>,
    in_flight: [usize; 3],
    /// A 429 stops everything until then, and background work until the later
    /// `background_until`.
    paused_until: Option<Instant>,
    background_until: Option<Instant>,
    next_seq: u64,
    sent: u64,
    anilist_remaining: Option<usize>,
}

enum Verdict {
    Go,
    /// Wait for a change, or until then.
    Wait(Option<Instant>),
    /// Paused for longer than MAX_PAUSE_WAIT: this much is left.
    GiveUp(Duration),
}

impl State {
    fn prune(&mut self, now: Instant) {
        self.counted.retain(|until| *until > now);
    }

    /// When no more than `n` will be counted, if nothing else is sent.
    fn when_at_most(&self, n: usize) -> Option<Instant> {
        if self.counted.len() <= n {
            return None;
        }
        let mut until = self.counted.clone();
        until.sort_unstable();
        Some(until[until.len() - n - 1])
    }

    /// The waiting request that goes next: highest priority, then oldest.
    fn head(&self) -> Option<u64> {
        self.waiting.iter().max_by_key(|(seq, lane)| (lane.get(), std::cmp::Reverse(*seq))).map(|(seq, _)| *seq)
    }

    fn higher_than_background(&self) -> bool {
        self.in_flight[Priority::Normal as usize] + self.in_flight[Priority::Interactive as usize] > 0
            || self.waiting.iter().any(|(_, lane)| lane.get() > Priority::Background)
    }

    fn verdict(&self, seq: u64, priority: Priority, now: Instant) -> Verdict {
        if let Some(until) = self.paused_until.filter(|t| *t > now) {
            if until - now > MAX_PAUSE_WAIT {
                return Verdict::GiveUp(until - now);
            }
            return Verdict::Wait(Some(until));
        }
        if self.head() != Some(seq) {
            return Verdict::Wait(None);
        }
        if priority == Priority::Background {
            if let Some(until) = self.background_until.filter(|t| *t > now) {
                return Verdict::Wait(Some(until));
            }
            if self.higher_than_background() {
                return Verdict::Wait(None);
            }
        }
        let share = priority.share();
        if self.counted.len() >= share {
            return Verdict::Wait(self.when_at_most(share - 1));
        }
        Verdict::Go
    }
}

/// Counts a request as in flight until dropped, once its answer is in.
/// AniList counts a request when it arrives, which is some time after it was
/// sent and before its answer comes back, so the request goes on counting
/// for a whole window from then: two requests the window keeps apart reach
/// AniList at least a window apart, however long the connection took.
pub struct Permit<'a> {
    limiter: &'a Limiter,
    priority: Priority,
    counted_until: Instant,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut s = self.limiter.lock();
        s.in_flight[self.priority as usize] -= 1;
        if let Some(slot) = s.counted.iter_mut().find(|until| **until == self.counted_until) {
            *slot = Instant::now() + self.limiter.window;
        }
        drop(s);
        self.limiter.changed.notify_waiters();
    }
}

/// A request's place in line, given up when dropped, however acquire() ends.
struct Queued<'a> {
    limiter: &'a Limiter,
    seq: u64,
}

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        self.limiter.lock().waiting.retain(|(seq, _)| *seq != self.seq);
        self.limiter.changed.notify_waiters();
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter::new(WINDOW)
    }
}

impl Limiter {
    /// Tests shorten `window`; the budget and the shares stay as they are.
    pub fn new(window: Duration) -> Self {
        Limiter { window, state: Mutex::new(State::default()), changed: Notify::new() }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits for this request's turn and counts it as sent. Err is how long
    /// a 429 pause still has to run when that's more than MAX_PAUSE_WAIT.
    pub async fn acquire(&self, lane: &Lane) -> Result<Permit<'_>, Duration> {
        let seq = {
            let mut s = self.lock();
            let seq = s.next_seq;
            s.next_seq += 1;
            s.waiting.push((seq, lane.clone()));
            seq
        };
        let _queued = Queued { limiter: self, seq };
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wake = {
                let mut s = self.lock();
                let now = Instant::now();
                s.prune(now);
                let priority = lane.get();
                match s.verdict(seq, priority, now) {
                    Verdict::Go => {
                        let counted_until = now + self.window;
                        s.counted.push(counted_until);
                        s.sent += 1;
                        s.in_flight[priority as usize] += 1;
                        log::debug!("AniList request #{} ({}, {}/{BUDGET} in the window)", s.sent, priority.name(), s.counted.len());
                        return Ok(Permit { limiter: self, priority, counted_until });
                    }
                    Verdict::GiveUp(left) => return Err(left),
                    Verdict::Wait(until) => until,
                }
            };
            match wake {
                Some(until) => {
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = tokio::time::sleep_until(until.into()) => {}
                    }
                }
                None => notified.await,
            }
        }
    }

    /// Raises a waiting or future request's priority (FR-81).
    pub fn raise(&self, lane: &Lane, priority: Priority) {
        if lane.raise(priority) {
            self.changed.notify_waiters();
        }
    }

    /// A response's rate-limit headers. When AniList has counted more
    /// requests than this has (another client on the same connection, an
    /// earlier run), the difference counts as sent now: for a window, or
    /// until `reset` (Unix seconds) when AniList says when its count resets.
    pub fn observe(&self, limit: Option<usize>, remaining: Option<usize>, reset: Option<u64>) {
        let Some(remaining) = remaining else { return };
        let now = Instant::now();
        let until = reset
            .and_then(|r| r.checked_sub(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs()))
            .map(|secs| now + Duration::from_secs(secs).min(self.window))
            .filter(|t| *t > now)
            .unwrap_or(now + self.window);
        let mut s = self.lock();
        s.anilist_remaining = Some(remaining);
        s.prune(now);
        let theirs = limit.unwrap_or(ANILIST_LIMIT).saturating_sub(remaining);
        let missing = theirs.saturating_sub(s.counted.len());
        if missing > 0 {
            log::info!("AniList counts {theirs} requests in its window, {missing} more than sent here: slowing down");
            s.counted.extend(std::iter::repeat_n(until, missing));
        }
    }

    /// A 429: nothing is sent for `retry_after`, and nothing in the
    /// background for a window, or `retry_after` if that's longer (FR-80).
    pub fn pause(&self, retry_after: Duration) {
        let now = Instant::now();
        let retry_after = retry_after.min(MAX_PAUSE);
        let mut s = self.lock();
        s.paused_until = s.paused_until.max(Some(now + retry_after));
        s.background_until = s.background_until.max(Some(now + retry_after.max(self.window)));
    }

    /// Whether a background request would go at once, with room for `n` of
    /// them: no pause, nothing else waiting or running, and spare budget.
    pub fn background_ready(&self, n: usize) -> bool {
        let now = Instant::now();
        let mut s = self.lock();
        s.prune(now);
        let paused = [s.paused_until, s.background_until].iter().flatten().any(|t| *t > now);
        !paused && s.waiting.is_empty() && !s.higher_than_background() && s.counted.len() + n <= Priority::Background.share()
    }

    /// How long until `n` interactive requests could all go at once (zero
    /// when they could now), counting the interactive ones already waiting.
    pub fn wait_for(&self, n: usize) -> Duration {
        let now = Instant::now();
        let mut s = self.lock();
        s.prune(now);
        let ahead = s.waiting.iter().filter(|(_, lane)| lane.get() == Priority::Interactive).count();
        let mut at = s.paused_until.filter(|t| *t > now).unwrap_or(now);
        match BUDGET.checked_sub(n + ahead) {
            Some(room) => at = at.max(s.when_at_most(room).unwrap_or(now)),
            None => at = at.max(now + self.window),
        }
        at - now
    }

    /// GET /debug/rate-limit's shape.
    pub fn snapshot(&self) -> Value {
        let now = Instant::now();
        let mut s = self.lock();
        s.prune(now);
        let left = |t: Option<Instant>| t.map_or(0, |t| t.saturating_duration_since(now).as_millis() as u64);
        let by_priority = |count: &dyn Fn(Priority) -> usize| {
            Value::Object(Priority::ALL.iter().map(|p| (p.name().to_string(), json!(count(*p)))).collect())
        };
        json!({
            "budget": BUDGET,
            "window_seconds": self.window.as_secs_f64(),
            "used": s.counted.len(),
            "remaining": BUDGET.saturating_sub(s.counted.len()),
            "queued": by_priority(&|p| s.waiting.iter().filter(|(_, lane)| lane.get() == p).count()),
            "in_flight": by_priority(&|p| s.in_flight[p as usize]),
            "paused_ms": left(s.paused_until),
            "background_paused_ms": left(s.background_until),
            "anilist_remaining": s.anilist_remaining,
            "sent": s.sent,
        })
    }
}
