//! What a rating looks like on AniList: the overall score converted to the
//! account's scoreFormat, and the criteria as AniList advanced scores,
//! matched BY NAME to the categories the account has set up.
//!
//! This is the one place that knows how app criteria and AniList's
//! advanced-scoring categories line up. Every push and pull of criteria
//! goes through it.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::db::{criteria_for, round1, snap_to_half_step, Rating, Scores};

/// App criterion key -> the AniList category names it fills, compared
/// case-insensitively. AniList's default categories are Story, Characters,
/// Visuals, Audio, Enjoyment for BOTH lists, so a manga list usually has an
/// "Audio" category this app has no criterion for; that one is left as
/// AniList has it (see `advanced_scores`).
const ANIME_CATEGORIES: &[(&str, &[&str])] = &[
    ("story", &["Story"]),
    ("characters", &["Characters"]),
    ("animation", &["Visuals", "Animation"]),
    ("audio", &["Audio"]),
    ("enjoyment", &["Enjoyment"]),
];

/// Manga: the `animation` column holds Art.
const MANGA_CATEGORIES: &[(&str, &[&str])] = &[
    ("story", &["Story"]),
    ("characters", &["Characters"]),
    ("animation", &["Visuals", "Art"]),
    ("enjoyment", &["Enjoyment"]),
];

/// The app criterion that fills AniList category `category`, if any.
pub fn criterion_for_category(media_type: &str, category: &str) -> Option<&'static str> {
    let table = if media_type == "anime" { ANIME_CATEGORIES } else { MANGA_CATEGORIES };
    let wanted = category.trim();
    table
        .iter()
        .find(|(_, names)| names.iter().any(|n| n.eq_ignore_ascii_case(wanted)))
        .map(|(key, _)| *key)
}

/// One list's advanced-scoring setup (`mediaListOptions.animeList` /
/// `mangaList`): the categories in AniList's order, and whether it's on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdvancedScoring {
    pub enabled: bool,
    pub categories: Vec<String>,
}

/// `Viewer.mediaListOptions`, the parts a push needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListOptions {
    pub score_format: Option<String>,
    pub anime: AdvancedScoring,
    pub manga: AdvancedScoring,
}

/// The GraphQL selection that `ListOptions::from_json` reads.
pub const LIST_OPTIONS_FIELDS: &str = "mediaListOptions { scoreFormat
    animeList { advancedScoring advancedScoringEnabled }
    mangaList { advancedScoring advancedScoringEnabled } }";

impl ListOptions {
    /// From a `mediaListOptions` node. A missing list reads as disabled.
    pub fn from_json(v: &Value) -> ListOptions {
        let list = |l: &Value| AdvancedScoring {
            enabled: l["advancedScoringEnabled"].as_bool().unwrap_or(false),
            categories: l["advancedScoring"]
                .as_array()
                .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        };
        ListOptions {
            score_format: v["scoreFormat"].as_str().map(str::to_string),
            anime: list(&v["animeList"]),
            manga: list(&v["mangaList"]),
        }
    }

    pub fn for_type(&self, media_type: &str) -> &AdvancedScoring {
        if media_type == "anime" {
            &self.anime
        } else {
            &self.manga
        }
    }
}

/// A 1.0-10.0 app score as AniList's `score` / advanced score takes it in
/// `format`. None for a format this app can't write without losing the
/// score's meaning (POINT_5, POINT_3, unknown): those pushes stop (FR-28).
///
/// - POINT_10_DECIMAL: one decimal, 7.9 -> 7.9
/// - POINT_10: whole points, 7.9 -> 8 (halves round up: 7.5 -> 8)
/// - POINT_100: 7.9 -> 79
pub fn to_anilist_score(score: f64, format: &str) -> Option<f64> {
    match format {
        "POINT_10_DECIMAL" => Some(round1(score)),
        "POINT_10" => Some(score.round()),
        "POINT_100" => Some((score * 10.0).round()),
        _ => None,
    }
}

/// Whether pushes can write a score in this format at all.
pub fn can_push_in(format: Option<&str>) -> bool {
    format.is_some_and(|f| to_anilist_score(5.0, f).is_some())
}

/// FR-23: a score Pull read (always `score(format: POINT_10_DECIMAL)`) is
/// what pushing `local` in `format` would have left on AniList. Then the
/// local decimal (7.9) stays instead of the rounded copy (8.0).
pub fn same_on_anilist(local: f64, remote_decimal: f64, format: Option<&str>) -> bool {
    let Some(format) = format else { return false };
    let Some(pushed) = to_anilist_score(local, format) else { return false };
    let as_decimal = if format == "POINT_100" { pushed / 10.0 } else { pushed };
    (as_decimal - remote_decimal).abs() < 1e-6
}

/// AniList's advanced scoring only exists for these two formats.
fn takes_advanced_scores(format: &str) -> bool {
    matches!(format, "POINT_100" | "POINT_10_DECIMAL")
}

/// `SaveMediaListEntry.advancedScores`: one value per category of the
/// account's list, in AniList's order, each app criterion placed BY NAME.
/// A category no criterion maps to keeps `remote`'s value (what AniList
/// has now), or 0 when there's none.
///
/// None (send nothing, leave AniList's as they are) when advanced scoring
/// is off for this list, the format has none, or the rating was seeded from
/// AniList's overall (FR-82): those criteria aren't real, and pushing them
/// would overwrite real advanced scores with the seed.
pub fn advanced_scores(
    media_type: &str,
    rating: &Rating,
    options: &ListOptions,
    remote: Option<&BTreeMap<String, f64>>,
) -> Option<Vec<f64>> {
    let format = options.score_format.as_deref()?;
    let scoring = options.for_type(media_type);
    if rating.source != "app" || !scoring.enabled || scoring.categories.is_empty() || !takes_advanced_scores(format) {
        return None;
    }
    Some(
        scoring
            .categories
            .iter()
            .map(|category| {
                criterion_for_category(media_type, category)
                    .and_then(|key| rating.criterion(key))
                    .and_then(|v| to_anilist_score(v, format))
                    .or_else(|| remote.and_then(|r| r.get(category).copied()))
                    .unwrap_or(0.0)
            })
            .collect(),
    )
}

/// `MediaList.advancedScores` (a JSON object, category -> score).
pub fn parse_advanced_scores(v: &Value) -> Option<BTreeMap<String, f64>> {
    let obj = v.as_object()?;
    Some(obj.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect())
}

/// An AniList advanced score (in `format`) on this app's 1.0-10.0 scale,
/// snapped to a 0.5 step as every criterion is. None for 0 ("not scored")
/// and for formats without advanced scoring.
fn from_anilist_advanced(score: f64, format: &str) -> Option<f64> {
    if score <= 0.0 {
        return None;
    }
    let s = match format {
        "POINT_100" => score / 10.0,
        "POINT_10_DECIMAL" => score,
        _ => return None,
    };
    Some(snap_to_half_step(s))
}

/// FR-23: AniList's advanced scores for one entry as this app's criteria,
/// each category matched BY NAME (`criterion_for_category`): Visuals fills
/// Animation for anime and Art for manga, Art fills Art, and manga's Audio
/// or any category the app doesn't know is ignored. All or nothing: None
/// unless advanced scoring is on for this list and every criterion of the
/// media type has a nonzero score, so a half-filled entry never passes as a
/// real rating.
pub fn criteria_from_advanced(media_type: &str, raw: &BTreeMap<String, f64>, options: &ListOptions) -> Option<Scores> {
    let format = options.score_format.as_deref()?;
    if !options.for_type(media_type).enabled || !takes_advanced_scores(format) {
        return None;
    }
    let value_for = |key: &str| {
        raw.iter()
            .filter(|(category, _)| criterion_for_category(media_type, category) == Some(key))
            .find_map(|(_, v)| from_anilist_advanced(*v, format))
    };
    let mut values = std::collections::HashMap::new();
    for key in criteria_for(media_type) {
        values.insert(*key, value_for(key)?);
    }
    Some(Scores {
        story: values["story"],
        characters: values["characters"],
        animation: values["animation"],
        audio: values.get("audio").copied().unwrap_or(0.0),
        enjoyment: values["enjoyment"],
    })
}

/// Whether a rating's criteria are the same as `other` for this media type.
pub fn same_criteria(media_type: &str, rating: &Rating, other: &Scores) -> bool {
    criteria_for(media_type)
        .iter()
        .all(|k| (rating.criterion(k).unwrap_or(0.0) - other.get(k)).abs() < 1e-9)
}
