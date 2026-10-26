//! AniList client credentials. Port of local_config.py.
//!
//! Looked up in this order, first hit wins:
//! 1. `ANILIST_CLIENT_ID` / `ANILIST_CLIENT_SECRET` environment variables
//! 2. `local_config.toml` in the app's config folder
//! 3. `local_config.py` in the current folder or its parent (the existing
//!    Python file works unchanged during development)
//!
//! Both files use the same `NAME = "value"` lines, so the Python file can be
//! copied to `local_config.toml` as-is. The secret is never stored anywhere else.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Where credentials were looked for, for a useful error message.
    pub searched: Vec<PathBuf>,
}

impl Config {
    pub fn load(config_dir: Option<&Path>) -> Config {
        let mut cfg = Config {
            client_id: std::env::var("ANILIST_CLIENT_ID").ok().filter(|v| !v.is_empty()),
            client_secret: std::env::var("ANILIST_CLIENT_SECRET").ok().filter(|v| !v.is_empty()),
            searched: Vec::new(),
        };
        let mut candidates = Vec::new();
        if let Some(dir) = config_dir {
            candidates.push(dir.join("local_config.toml"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("local_config.py"));
            if let Some(parent) = cwd.parent() {
                candidates.push(parent.join("local_config.py"));
            }
        }
        for path in candidates {
            if cfg.is_complete() {
                break;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                cfg.client_id = cfg.client_id.or_else(|| read_value(&text, "ANILIST_CLIENT_ID"));
                cfg.client_secret = cfg.client_secret.or_else(|| read_value(&text, "ANILIST_CLIENT_SECRET"));
            }
            cfg.searched.push(path);
        }
        cfg
    }

    pub fn is_complete(&self) -> bool {
        self.client_id.is_some() && self.client_secret.is_some()
    }

    pub fn missing_message(&self) -> String {
        let places: Vec<String> = self.searched.iter().map(|p| p.display().to_string()).collect();
        format!(
            "AniList client credentials not found. Set ANILIST_CLIENT_ID and ANILIST_CLIENT_SECRET in {}",
            places.first().cloned().unwrap_or_else(|| "local_config.toml".into())
        )
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
    fn reads_python_and_toml_style_lines() {
        let text = "# creds\nANILIST_CLIENT_ID = \"12345\"\nANILIST_CLIENT_SECRET='s3cret'  # keep private\n";
        assert_eq!(read_value(text, "ANILIST_CLIENT_ID").as_deref(), Some("12345"));
        assert_eq!(read_value(text, "ANILIST_CLIENT_SECRET").as_deref(), Some("s3cret"));
        assert_eq!(read_value(text, "OTHER"), None);
    }
}
