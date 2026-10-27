//! SQLite schema and data access (PRD Section 5). Port of database.py.
//!
//! One `media` table for both anime and manga, distinguished by `media_type`,
//! which is set once at import time and never inferred or defaulted. `rating`
//! is a separate table so a media row can exist with no rating yet.
//! `anilist_auth` is a single row (id=1) holding the OAuth session.
//!
//! The schema is byte-for-byte the one the Python app created, so an existing
//! `library.db` opens here unchanged (no migration).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{named_params, params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub type DbResult<T> = rusqlite::Result<T>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS media (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    anilist_id INTEGER UNIQUE NOT NULL,
    media_type TEXT NOT NULL CHECK (media_type IN ('anime','manga')),
    title TEXT,
    title_native TEXT,
    title_english TEXT,
    cover_url TEXT,
    genres TEXT,
    tags TEXT,
    producers TEXT,
    runtime_minutes INTEGER,
    year INTEGER,
    season TEXT,
    format TEXT,
    episodes INTEGER,
    chapters INTEGER,
    volumes INTEGER,
    synopsis TEXT,
    avg_score REAL,
    status TEXT NOT NULL DEFAULT 'planning',
    progress INTEGER NOT NULL DEFAULT 0,
    repeat_count INTEGER NOT NULL DEFAULT 0,
    anilist_list_entry_id INTEGER,
    updated_at TEXT,
    last_synced_at TEXT,
    pending_conflict INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS rating (
    media_id INTEGER PRIMARY KEY REFERENCES media(id),
    story REAL NOT NULL,
    characters REAL NOT NULL,
    animation REAL,
    audio REAL NOT NULL DEFAULT 0.0,
    enjoyment REAL NOT NULL,
    overall REAL NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS anilist_auth (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    access_token TEXT,
    token_expires_at TEXT,
    anilist_user_id INTEGER,
    anilist_username TEXT,
    score_format TEXT
);
";

pub const ANIME_CRITERIA: [&str; 5] = ["story", "characters", "animation", "audio", "enjoyment"];
/// Audio excluded; Art lives in the `animation` column.
pub const MANGA_CRITERIA: [&str; 4] = ["story", "characters", "animation", "enjoyment"];

pub fn criteria_for(media_type: &str) -> &'static [&'static str] {
    if media_type == "anime" {
        &ANIME_CRITERIA
    } else {
        &MANGA_CRITERIA
    }
}

/// Same string format Python's `datetime.now(UTC).isoformat()` wrote, so
/// timestamps already in library.db keep comparing correctly.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false)
}

/// The AniList/SearchResult shape: everything about a title that comes from
/// AniList, before it has any local list state.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct MediaFields {
    pub anilist_id: i64,
    pub media_type: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub title_native: Option<String>,
    #[serde(default)]
    pub title_english: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub producers: Vec<String>,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub season: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub episodes: Option<i64>,
    #[serde(default)]
    pub chapters: Option<i64>,
    #[serde(default)]
    pub volumes: Option<i64>,
    #[serde(default)]
    pub runtime_minutes: Option<i64>,
    #[serde(default)]
    pub synopsis: Option<String>,
    #[serde(default)]
    pub avg_score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rating {
    pub story: f64,
    pub characters: f64,
    pub animation: Option<f64>,
    pub audio: f64,
    pub enjoyment: f64,
    pub overall: f64,
    pub updated_at: String,
}

impl Rating {
    pub fn criterion(&self, name: &str) -> Option<f64> {
        match name {
            "story" => Some(self.story),
            "characters" => Some(self.characters),
            "animation" => self.animation,
            "audio" => Some(self.audio),
            "enjoyment" => Some(self.enjoyment),
            "overall" => Some(self.overall),
            _ => None,
        }
    }
}

/// A library row (MediaOut in the PRD §7 contract).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Media {
    pub id: i64,
    #[serde(flatten)]
    pub fields: MediaFields,
    pub status: String,
    pub progress: i64,
    pub repeat_count: i64,
    pub anilist_list_entry_id: Option<i64>,
    pub updated_at: Option<String>,
    pub last_synced_at: Option<String>,
    pub pending_conflict: bool,
    pub rating: Option<Rating>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Auth {
    pub access_token: String,
    pub token_expires_at: Option<String>,
    pub anilist_user_id: i64,
    pub anilist_username: Option<String>,
    pub score_format: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Scores {
    pub story: f64,
    pub characters: f64,
    pub animation: f64,
    pub audio: f64,
    pub enjoyment: f64,
}

impl Scores {
    fn get(&self, name: &str) -> f64 {
        match name {
            "story" => self.story,
            "characters" => self.characters,
            "animation" => self.animation,
            "audio" => self.audio,
            "enjoyment" => self.enjoyment,
            _ => 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    None,
    Local,
    Remote,
    Conflict,
}

/// Python's `round(x, 1)`: round half to even. Matters for manga, whose
/// 4-criterion average can land exactly on a tie (e.g. 7.25 -> 7.2).
pub fn round1(x: f64) -> f64 {
    (x * 10.0).round_ties_even() / 10.0
}

/// Flat, deliberately UNWEIGHTED average of the active criteria for this
/// media type. Do not reintroduce per-criterion weights here.
pub fn compute_overall(media_type: &str, scores: &Scores) -> f64 {
    let keys = criteria_for(media_type);
    round1(keys.iter().map(|k| scores.get(k)).sum::<f64>() / keys.len() as f64)
}

/// Nearest valid criterion value: 1.0-10.0 in 0.5 steps, halves round up
/// (7.25 -> 7.5). AniList's POINT_10_DECIMAL allows e.g. 7.3, criteria don't.
pub fn snap_to_half_step(score: f64) -> f64 {
    ((score * 2.0 + 0.5).floor() / 2.0).clamp(1.0, 10.0)
}

fn ts_after(a: &str, b: &str) -> bool {
    match (DateTime::parse_from_rfc3339(a), DateTime::parse_from_rfc3339(b)) {
        (Ok(a), Ok(b)) => a > b,
        _ => a > b,
    }
}

/// FR-23's core comparison, pure logic. A never-synced local entry that has a
/// remote counterpart IS a conflict: with no synced baseline we can't tell
/// which side changed, so we ask instead of silently picking a winner.
pub fn detect_conflict(
    local_updated_at: Option<&str>,
    remote_updated_at: Option<&str>,
    last_synced_at: Option<&str>,
) -> SyncOutcome {
    let (local_changed, remote_changed) = match last_synced_at {
        None => (true, true),
        Some(synced) => (
            local_updated_at.is_some_and(|l| ts_after(l, synced)),
            remote_updated_at.is_some_and(|r| ts_after(r, synced)),
        ),
    };
    match (local_changed, remote_changed) {
        (true, true) => SyncOutcome::Conflict,
        (false, true) => SyncOutcome::Remote,
        (true, false) => SyncOutcome::Local,
        (false, false) => SyncOutcome::None,
    }
}

fn json_list(raw: Option<String>) -> Vec<String> {
    raw.filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn row_to_media(row: &Row) -> DbResult<Media> {
    Ok(Media {
        id: row.get("id")?,
        fields: MediaFields {
            anilist_id: row.get("anilist_id")?,
            media_type: row.get("media_type")?,
            title: row.get("title")?,
            title_native: row.get("title_native")?,
            title_english: row.get("title_english")?,
            cover_url: row.get("cover_url")?,
            genres: json_list(row.get("genres")?),
            tags: json_list(row.get("tags")?),
            producers: json_list(row.get("producers")?),
            year: row.get("year")?,
            season: row.get("season")?,
            format: row.get("format")?,
            episodes: row.get("episodes")?,
            chapters: row.get("chapters")?,
            volumes: row.get("volumes")?,
            runtime_minutes: row.get("runtime_minutes")?,
            synopsis: row.get("synopsis")?,
            avg_score: row.get("avg_score")?,
        },
        status: row.get("status")?,
        progress: row.get("progress")?,
        repeat_count: row.get("repeat_count")?,
        anilist_list_entry_id: row.get("anilist_list_entry_id")?,
        updated_at: row.get("updated_at")?,
        last_synced_at: row.get("last_synced_at")?,
        pending_conflict: row.get::<_, i64>("pending_conflict")? != 0,
        rating: None,
    })
}

fn row_to_rating(row: &Row) -> DbResult<Rating> {
    Ok(Rating {
        story: row.get("story")?,
        characters: row.get("characters")?,
        animation: row.get("animation")?,
        audio: row.get("audio")?,
        enjoyment: row.get("enjoyment")?,
        overall: row.get("overall")?,
        updated_at: row.get("updated_at")?,
    })
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> DbResult<Db> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> DbResult<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> DbResult<Db> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        migrate_existing_media_columns(&conn)?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock can't leave SQLite half-written
        // (statements are atomic), so a poisoned lock is safe to reuse.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------ media ----

    /// FR-4: same anilist_id updates the existing row instead of duplicating.
    /// media_type is written on insert only, never changed afterwards.
    /// `status` is None for Pull imports (status lands via apply_remote_entry).
    pub fn insert_or_update_media(&self, f: &MediaFields, status: Option<&str>) -> DbResult<i64> {
        let conn = self.conn();
        let existing: Option<i64> = conn
            .query_row("SELECT id FROM media WHERE anilist_id = ?", [f.anilist_id], |r| r.get(0))
            .optional()?;
        let genres = serde_json::to_string(&f.genres).unwrap_or_else(|_| "[]".into());
        let tags = serde_json::to_string(&f.tags).unwrap_or_else(|_| "[]".into());
        let producers = serde_json::to_string(&f.producers).unwrap_or_else(|_| "[]".into());

        if let Some(id) = existing {
            conn.execute(
                "UPDATE media SET title=:title, title_native=:title_native, title_english=:title_english,
                    cover_url=:cover_url, genres=:genres, tags=:tags, producers=:producers,
                    year=:year, season=:season, format=:format, episodes=:episodes, chapters=:chapters,
                    volumes=:volumes, runtime_minutes=:runtime_minutes, synopsis=:synopsis,
                    avg_score=:avg_score, status=COALESCE(:status, status)
                 WHERE anilist_id = :anilist_id",
                named_params! {
                    ":title": f.title, ":title_native": f.title_native, ":title_english": f.title_english,
                    ":cover_url": f.cover_url, ":genres": genres, ":tags": tags, ":producers": producers,
                    ":year": f.year, ":season": f.season, ":format": f.format, ":episodes": f.episodes,
                    ":chapters": f.chapters, ":volumes": f.volumes, ":runtime_minutes": f.runtime_minutes,
                    ":synopsis": f.synopsis, ":avg_score": f.avg_score, ":status": status,
                    ":anilist_id": f.anilist_id,
                },
            )?;
            return Ok(id);
        }

        conn.execute(
            "INSERT INTO media (anilist_id, media_type, title, title_native, title_english, cover_url,
                genres, tags, producers, year, season, format, episodes, chapters, volumes,
                runtime_minutes, synopsis, avg_score, status)
             VALUES (:anilist_id, :media_type, :title, :title_native, :title_english, :cover_url,
                :genres, :tags, :producers, :year, :season, :format, :episodes, :chapters, :volumes,
                :runtime_minutes, :synopsis, :avg_score, COALESCE(:status, 'planning'))",
            named_params! {
                ":anilist_id": f.anilist_id, ":media_type": f.media_type, ":title": f.title,
                ":title_native": f.title_native, ":title_english": f.title_english,
                ":cover_url": f.cover_url, ":genres": genres, ":tags": tags, ":producers": producers,
                ":year": f.year, ":season": f.season, ":format": f.format, ":episodes": f.episodes,
                ":chapters": f.chapters, ":volumes": f.volumes, ":runtime_minutes": f.runtime_minutes,
                ":synopsis": f.synopsis, ":avg_score": f.avg_score, ":status": status,
            },
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn rating_for(conn: &Connection, media_id: i64) -> DbResult<Option<Rating>> {
        conn.query_row("SELECT * FROM rating WHERE media_id = ?", [media_id], row_to_rating)
            .optional()
    }

    fn media_where(&self, clause: &str, id: i64) -> DbResult<Option<Media>> {
        let conn = self.conn();
        let sql = format!("SELECT * FROM media WHERE {clause} = ?");
        let Some(mut media) = conn.query_row(&sql, [id], row_to_media).optional()? else {
            return Ok(None);
        };
        media.rating = Self::rating_for(&conn, media.id)?;
        Ok(Some(media))
    }

    pub fn get_media_by_id(&self, media_id: i64) -> DbResult<Option<Media>> {
        self.media_where("id", media_id)
    }

    pub fn get_media_by_anilist_id(&self, anilist_id: i64) -> DbResult<Option<Media>> {
        self.media_where("anilist_id", anilist_id)
    }

    /// FR-21: creates the rating row if one doesn't exist yet. Overall is
    /// always computed here from media_type; audio is forced to 0.0 for manga.
    pub fn upsert_rating(&self, media_id: i64, scores: &Scores) -> DbResult<Option<(f64, String)>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let media_type: Option<String> = tx
            .query_row("SELECT media_type FROM media WHERE id = ?", [media_id], |r| r.get(0))
            .optional()?;
        let Some(media_type) = media_type else { return Ok(None) };
        let overall = compute_overall(&media_type, scores);
        let audio = if media_type == "anime" { scores.audio } else { 0.0 };
        let now = now_iso();
        tx.execute(
            "INSERT INTO rating (media_id, story, characters, animation, audio, enjoyment, overall, updated_at)
             VALUES (:media_id, :story, :characters, :animation, :audio, :enjoyment, :overall, :updated_at)
             ON CONFLICT(media_id) DO UPDATE SET
                story=:story, characters=:characters, animation=:animation,
                audio=:audio, enjoyment=:enjoyment, overall=:overall, updated_at=:updated_at",
            named_params! {
                ":media_id": media_id, ":story": scores.story, ":characters": scores.characters,
                ":animation": scores.animation, ":audio": audio, ":enjoyment": scores.enjoyment,
                ":overall": overall, ":updated_at": now,
            },
        )?;
        // FR-23/FR-24: score changes bump the same media.updated_at that
        // status/progress changes bump, so they're pushable too.
        tx.execute("UPDATE media SET updated_at = ? WHERE id = ?", params![now, media_id])?;
        tx.commit()?;
        Ok(Some((overall, media_type)))
    }

    pub fn update_status(&self, media_id: i64, status: &str) -> DbResult<Option<String>> {
        let n = self.conn().execute(
            "UPDATE media SET status = ?, updated_at = ? WHERE id = ?",
            params![status, now_iso(), media_id],
        )?;
        Ok((n > 0).then(|| status.to_string()))
    }

    /// FR-53 auto-complete + FR-57 null-total exception: clamp only when a
    /// total exists; auto-complete only when there IS a total, it's actually
    /// reached, and it wasn't already there.
    pub fn update_progress(&self, media_id: i64, new_progress: i64) -> DbResult<Option<i64>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                "SELECT media_type, episodes, chapters, status, progress, repeat_count FROM media WHERE id = ?",
                [media_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((media_type, episodes, chapters, mut status, old, mut repeat_count)) = row else {
            return Ok(None);
        };
        let total = if media_type == "anime" { episodes } else { chapters };
        let mut p = new_progress.max(0);
        if let Some(t) = total {
            p = p.min(t);
        }
        if let Some(t) = total {
            if t != 0 && p == t && old != t {
                match status.as_str() {
                    "watching" | "reading" => status = "completed".into(),
                    "rewatching" | "rereading" => {
                        repeat_count += 1;
                        status = "completed".into();
                    }
                    _ => {}
                }
            }
        }
        conn.execute(
            "UPDATE media SET progress = ?, status = ?, repeat_count = ?, updated_at = ? WHERE id = ?",
            params![p, status, repeat_count, now_iso(), media_id],
        )?;
        Ok(Some(p))
    }

    pub fn update_repeat_count(&self, media_id: i64, repeat_count: i64) -> DbResult<Option<i64>> {
        let n = self.conn().execute(
            "UPDATE media SET repeat_count = ?, updated_at = ? WHERE id = ?",
            params![repeat_count, now_iso(), media_id],
        )?;
        Ok((n > 0).then_some(repeat_count))
    }

    /// FR-8. `criterion` selects a SQL column, so it is whitelisted before
    /// use. Err(Some(msg)) = invalid criterion; Ok(None) = no such media.
    pub fn get_anchors(&self, media_id: i64, criterion: &str, scope: &str) -> Result<Option<Vec<Value>>, AnchorError> {
        if !ANIME_CRITERIA.contains(&criterion) {
            return Err(AnchorError::InvalidCriterion(criterion.to_string()));
        }
        let conn = self.conn();
        let target = conn
            .query_row("SELECT media_type, genres FROM media WHERE id = ?", [media_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .optional()?;
        let Some((media_type, genres)) = target else { return Ok(None) };
        let target_genres: HashSet<String> = json_list(genres).into_iter().collect();

        let sql = format!(
            "SELECT m.id, m.title, m.title_english, m.cover_url, m.genres, r.{criterion} AS score
             FROM media m JOIN rating r ON r.media_id = m.id
             WHERE m.media_type = ? AND m.id != ? AND r.{criterion} IS NOT NULL"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![media_type, media_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, f64>(5)?,
            ))
        })?;
        let mut results: Vec<(f64, Value)> = Vec::new();
        for row in rows {
            let (id, title, title_english, cover_url, genres, score) = row?;
            if scope == "genre" && !json_list(genres).iter().any(|g| target_genres.contains(g)) {
                continue;
            }
            results.push((
                score,
                json!({"id": id, "title": title, "title_english": title_english,
                       "cover_url": cover_url, "score": score}),
            ));
        }
        results.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(Some(results.into_iter().map(|(_, v)| v).collect()))
    }

    // ------------------------------------------------------------- auth ----

    pub fn get_auth(&self) -> DbResult<Option<Auth>> {
        let conn = self.conn();
        let row = conn
            .query_row("SELECT * FROM anilist_auth WHERE id = 1", [], |r| {
                Ok((
                    r.get::<_, Option<String>>("access_token")?,
                    r.get::<_, Option<String>>("token_expires_at")?,
                    r.get::<_, Option<i64>>("anilist_user_id")?,
                    r.get::<_, Option<String>>("anilist_username")?,
                    r.get::<_, Option<String>>("score_format")?,
                ))
            })
            .optional()?;
        Ok(row.and_then(|(token, expires, user_id, username, score_format)| {
            let token = token.filter(|t| !t.is_empty())?;
            Some(Auth {
                access_token: token,
                token_expires_at: expires,
                anilist_user_id: user_id.unwrap_or_default(),
                anilist_username: username,
                score_format,
            })
        }))
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.get_auth(), Ok(Some(_)))
    }

    pub fn save_auth(
        &self,
        access_token: &str,
        token_expires_at: Option<&str>,
        anilist_user_id: i64,
        anilist_username: &str,
        score_format: Option<&str>,
    ) -> DbResult<()> {
        self.conn().execute(
            "INSERT INTO anilist_auth (id, access_token, token_expires_at, anilist_user_id, anilist_username, score_format)
             VALUES (1, :access_token, :token_expires_at, :anilist_user_id, :anilist_username, :score_format)
             ON CONFLICT(id) DO UPDATE SET
                access_token=:access_token, token_expires_at=:token_expires_at,
                anilist_user_id=:anilist_user_id, anilist_username=:anilist_username,
                score_format=:score_format",
            named_params! {
                ":access_token": access_token, ":token_expires_at": token_expires_at,
                ":anilist_user_id": anilist_user_id, ":anilist_username": anilist_username,
                ":score_format": score_format,
            },
        )?;
        Ok(())
    }

    pub fn update_score_format(&self, score_format: Option<&str>) -> DbResult<()> {
        self.conn()
            .execute("UPDATE anilist_auth SET score_format = ? WHERE id = 1", [score_format])?;
        Ok(())
    }

    /// FR-27: clears the stored token only. No library data is touched.
    pub fn clear_auth(&self) -> DbResult<()> {
        self.conn().execute(
            "UPDATE anilist_auth SET access_token=NULL, token_expires_at=NULL,
             anilist_user_id=NULL, anilist_username=NULL, score_format=NULL WHERE id = 1",
            [],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------- sync ----

    pub fn set_last_synced(&self, media_id: i64, ts: &str) -> DbResult<()> {
        self.conn()
            .execute("UPDATE media SET last_synced_at = ? WHERE id = ?", params![ts, media_id])?;
        Ok(())
    }

    pub fn set_anilist_list_entry_id(&self, media_id: i64, entry_id: i64) -> DbResult<()> {
        self.conn().execute(
            "UPDATE media SET anilist_list_entry_id = ? WHERE id = ?",
            params![entry_id, media_id],
        )?;
        Ok(())
    }

    /// FR-23: AniList's value wins for this entry. Applied in one transaction
    /// with the timestamp stamp, so last_synced_at can never land ahead of the
    /// score. A nonzero remote score overwrites `overall` only (AniList never
    /// carries the criterion breakdown); with no local rating yet, every
    /// criterion seeds to that one value snapped to a 0.5 step, while overall
    /// keeps AniList's exact score so the next push doesn't rewrite it.
    pub fn apply_remote_entry(
        &self,
        media_id: i64,
        status: &str,
        progress: i64,
        repeat_count: i64,
        score: Option<f64>,
        ts: &str,
    ) -> DbResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE media SET status = ?, progress = ?, repeat_count = ?, updated_at = ?, last_synced_at = ? WHERE id = ?",
            params![status, progress, repeat_count, ts, ts, media_id],
        )?;
        // AniList's 0 means "no score entered", not a real rating.
        if let Some(score) = score.filter(|s| *s != 0.0) {
            let exists: bool = tx
                .query_row("SELECT 1 FROM rating WHERE media_id = ?", [media_id], |_| Ok(()))
                .optional()?
                .is_some();
            if exists {
                tx.execute(
                    "UPDATE rating SET overall = ?, updated_at = ? WHERE media_id = ?",
                    params![score, ts, media_id],
                )?;
            } else {
                let media_type: String =
                    tx.query_row("SELECT media_type FROM media WHERE id = ?", [media_id], |r| r.get(0))?;
                let seed = snap_to_half_step(score);
                let audio = if media_type == "anime" { seed } else { 0.0 };
                tx.execute(
                    "INSERT INTO rating (media_id, story, characters, animation, audio, enjoyment, overall, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    params![media_id, seed, seed, seed, audio, seed, score, ts],
                )?;
            }
        }
        tx.commit()
    }

    pub fn set_pending_conflict(&self, media_id: i64, value: bool) -> DbResult<()> {
        self.conn().execute(
            "UPDATE media SET pending_conflict = ? WHERE id = ?",
            params![value as i64, media_id],
        )?;
        Ok(())
    }

    pub fn get_pending_conflict_rows(&self) -> DbResult<Vec<Media>> {
        self.select_media("SELECT * FROM media WHERE pending_conflict = 1", &[])
    }

    // ------------------------------------------------ library / rankings ----

    fn select_media(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> DbResult<Vec<Media>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt
            .query_map(args, row_to_media)?
            .collect::<DbResult<Vec<Media>>>()?;
        for m in rows.iter_mut() {
            m.rating = Self::rating_for(&conn, m.id)?;
        }
        Ok(rows)
    }

    pub fn get_library(&self, media_type: Option<&str>, status: Option<&str>, sort: &str) -> DbResult<Vec<Media>> {
        let mut sql = String::from("SELECT * FROM media WHERE 1=1");
        let mut args: Vec<&dyn rusqlite::ToSql> = Vec::new();
        if let Some(mt) = media_type.as_ref() {
            sql.push_str(" AND media_type = ?");
            args.push(mt);
        }
        if let Some(st) = status.as_ref().filter(|s| !s.is_empty() && **s != "all") {
            sql.push_str(" AND status = ?");
            args.push(st);
        }
        let mut results = self.select_media(&sql, &args)?;
        sort_by_rating(&mut results, sort);
        Ok(results)
    }

    /// Only rated titles participate: rankings are anchored-score sort.
    pub fn get_rankings(
        &self,
        media_type: Option<&str>,
        genre: Option<&str>,
        year: Option<i64>,
        sort: &str,
        q: Option<&str>,
    ) -> DbResult<Vec<Media>> {
        let mut sql = String::from("SELECT m.* FROM media m JOIN rating r ON r.media_id = m.id WHERE 1=1");
        let mut args: Vec<&dyn rusqlite::ToSql> = Vec::new();
        if let Some(mt) = media_type.as_ref() {
            sql.push_str(" AND m.media_type = ?");
            args.push(mt);
        }
        let year = year.filter(|y| *y != 0);
        if let Some(y) = year.as_ref() {
            sql.push_str(" AND m.year = ?");
            args.push(y);
        }
        let like = q.filter(|q| !q.is_empty()).map(|q| format!("%{q}%"));
        if let Some(l) = like.as_ref() {
            sql.push_str(" AND m.title LIKE ?");
            args.push(l);
        }
        let mut results = self.select_media(&sql, &args)?;
        if let Some(g) = genre.filter(|g| !g.is_empty()) {
            results.retain(|r| r.fields.genres.iter().any(|x| x == g));
        }
        sort_by_rating(&mut results, sort);
        Ok(results)
    }

    pub fn delete_media(&self, media_id: i64) -> DbResult<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM rating WHERE media_id = ?", [media_id])?;
        let n = tx.execute("DELETE FROM media WHERE id = ?", [media_id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    // ---------------------------------------------------------- insights ----

    /// FR-60 through FR-63, computed from local rated titles only.
    pub fn get_insights(&self) -> DbResult<Value> {
        let rated = self.select_media("SELECT m.* FROM media m JOIN rating r ON r.media_id = m.id", &[])?;
        Ok(json!({
            "by_genre": insights_by_genre(&rated),
            "top_criterion": {
                "anime": insights_top_criterion(&rated, "anime"),
                "manga": insights_top_criterion(&rated, "manga"),
            },
            "community_divergence": insights_divergence(&rated),
            "distribution": {
                "anime": insights_distribution(&rated, "anime"),
                "manga": insights_distribution(&rated, "manga"),
            },
        }))
    }
}

#[derive(Debug)]
pub enum AnchorError {
    InvalidCriterion(String),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for AnchorError {
    fn from(e: rusqlite::Error) -> Self {
        AnchorError::Db(e)
    }
}

/// CREATE TABLE IF NOT EXISTS is a no-op on an existing table, so an older
/// library.db gets these columns added here, idempotently.
fn migrate_existing_media_columns(conn: &Connection) -> DbResult<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(media)")?;
    let cols: HashSet<String> = stmt
        .query_map([], |r| r.get::<_, String>("name"))?
        .collect::<DbResult<_>>()?;
    for col in ["updated_at", "last_synced_at"] {
        if !cols.contains(col) {
            conn.execute_batch(&format!("ALTER TABLE media ADD COLUMN {col} TEXT"))?;
            log::info!("migrated: added media.{col}");
        }
    }
    if !cols.contains("pending_conflict") {
        conn.execute_batch("ALTER TABLE media ADD COLUMN pending_conflict INTEGER NOT NULL DEFAULT 0")?;
        log::info!("migrated: added media.pending_conflict");
    }
    Ok(())
}

fn sort_by_rating(results: &mut [Media], sort: &str) {
    let key = if ["story", "characters", "animation", "audio", "enjoyment"].contains(&sort) {
        sort
    } else {
        "overall"
    };
    let value = |m: &Media| m.rating.as_ref().and_then(|r| r.criterion(key)).unwrap_or(0.0);
    // Stable, descending: equal scores keep insertion order (same as Python).
    results.sort_by(|a, b| value(b).total_cmp(&value(a)));
}

fn overall_of(m: &Media) -> f64 {
    m.rating.as_ref().map(|r| r.overall).unwrap_or(0.0)
}

fn insights_by_genre(rated: &[Media]) -> Vec<Value> {
    // (genre, media_type) are separate groups: FR-60 never blends them.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), Vec<f64>> = HashMap::new();
    for r in rated {
        for genre in &r.fields.genres {
            let key = (genre.clone(), r.fields.media_type.clone());
            groups
                .entry(key.clone())
                .or_insert_with(|| {
                    order.push(key);
                    Vec::new()
                })
                .push(overall_of(r));
        }
    }
    order
        .into_iter()
        .map(|key| {
            let scores = &groups[&key];
            json!({
                "genre": key.0, "media_type": key.1,
                "avg_overall": scores.iter().sum::<f64>() / scores.len() as f64,
                "count": scores.len(),
            })
        })
        .collect()
}

fn insights_top_criterion(rated: &[Media], media_type: &str) -> Value {
    // Audio is simply absent from manga's criteria, never averaged as zero.
    let subset: Vec<&Media> = rated.iter().filter(|r| r.fields.media_type == media_type).collect();
    if subset.is_empty() {
        return Value::Null;
    }
    let mut best: Option<(&str, f64)> = None;
    for c in criteria_for(media_type) {
        let avg = subset
            .iter()
            .map(|r| r.rating.as_ref().and_then(|x| x.criterion(c)).unwrap_or(0.0))
            .sum::<f64>()
            / subset.len() as f64;
        // First maximum wins on ties, like Python's max().
        if best.is_none_or(|(_, b)| avg > b) {
            best = Some((c, avg));
        }
    }
    let (criterion, avg_score) = best.expect("criteria list is non-empty");
    json!({"criterion": criterion, "avg_score": avg_score})
}

fn insights_divergence(rated: &[Media]) -> Value {
    let entries: Vec<(f64, Value)> = rated
        .iter()
        .filter_map(|r| {
            let avg = r.fields.avg_score?;
            let yours = overall_of(r);
            let delta = yours - avg;
            Some((
                delta,
                json!({
                    "media_id": r.id, "title": r.fields.title, "title_english": r.fields.title_english,
                    "your_score": yours, "avg_score": avg, "delta": delta,
                }),
            ))
        })
        .collect();
    let avg_divergence = if entries.is_empty() {
        0.0
    } else {
        entries.iter().map(|e| e.0).sum::<f64>() / entries.len() as f64
    };
    let mut above = entries.clone();
    above.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut below = entries;
    below.sort_by(|a, b| a.0.total_cmp(&b.0));
    let top5 = |v: Vec<(f64, Value)>| v.into_iter().take(5).map(|e| e.1).collect::<Vec<_>>();
    json!({"avg_divergence": avg_divergence, "most_above": top5(above), "most_below": top5(below)})
}

fn insights_distribution(rated: &[Media], media_type: &str) -> Vec<Value> {
    // 18 buckets, 0.5 wide, [1.0, 10.0]: real interval bucketing.
    let mut counts = [0usize; 18];
    for r in rated.iter().filter(|r| r.fields.media_type == media_type) {
        let idx = ((overall_of(r) - 1.0) / 0.5).trunc() as i64;
        counts[idx.clamp(0, 17) as usize] += 1;
    }
    (0..18)
        .map(|i| json!({"bucket": 1.0 + 0.5 * i as f64, "count": counts[i]}))
        .collect()
}
