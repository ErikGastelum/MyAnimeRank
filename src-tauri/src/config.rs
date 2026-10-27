//! AniList client ID. Login uses AniList's implicit grant, which needs only
//! the client ID. That ID is public by design (it's in the login URL anyway),
//! so it's built in and there is no client secret anywhere.
//!
//! Overrides, first hit wins:
//! 1. the `ANILIST_CLIENT_ID` environment variable
//! 2. `local_config.toml` in the app's config folder (`ANILIST_CLIENT_ID = "..."`)
//!
//! With neither set, the built-in ID is used, so login never fails for
//! missing config.

use std::path::Path;

/// This app's AniList API client. Its redirect URI must stay
/// `http://localhost:8931/callback` (see oauth.rs).
pub const BUILTIN_CLIENT_ID: &str = "45069";

#[derive(Debug, Clone)]
pub struct Config {
    pub client_id: String,
}

impl Config {
    pub fn load(config_dir: Option<&Path>) -> Config {
        let toml = config_dir.map(|dir| dir.join("local_config.toml"));
        Config::from_sources(std::env::var("ANILIST_CLIENT_ID").ok(), toml.as_deref())
    }

    /// `load` minus the process environment, so tests can drive the lookup
    /// order directly.
    pub fn from_sources(env_value: Option<String>, toml: Option<&Path>) -> Config {
        let client_id = env_value
            .filter(|v| !v.is_empty())
            .or_else(|| read_value(&std::fs::read_to_string(toml?).ok()?, "ANILIST_CLIENT_ID"))
            .unwrap_or_else(|| BUILTIN_CLIENT_ID.to_string());
        Config { client_id }
    }
}

/// Reads `NAME = "value"` (or 'value', or a bare number) from a simple file.
fn read_value(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim() != name {
            return None;
        }
        let value = value.split('#').next()?.trim().trim_matches(|c| c == '"' || c == '\'');
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::read_value;

    #[test]
    fn reads_quoted_and_commented_lines() {
        let text = "# config\nANILIST_CLIENT_ID = \"12345\"\nOTHER_NAME='abc'  # a comment\n";
        assert_eq!(read_value(text, "ANILIST_CLIENT_ID").as_deref(), Some("12345"));
        assert_eq!(read_value(text, "OTHER_NAME").as_deref(), Some("abc"));
        assert_eq!(read_value(text, "MISSING"), None);
    }
}
