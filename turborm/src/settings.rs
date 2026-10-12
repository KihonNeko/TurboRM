//! User settings, kept as a small JSON file in the XDG config directory.
//!
//! A missing, unreadable or partly unknown file never stops the app: every
//! field falls back to its default on its own.
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf};

const MIB: u64 = 1024 * 1024;

/// How much of the library is kept on disk: speed traded against space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheMode {
    /// Only the last few documents opened.
    Small,
    /// Recently opened documents, up to 500 MB.
    #[default]
    Standard,
    /// The whole library, downloaded in the background and kept up to date.
    Full,
}

impl CacheMode {
    /// Disk the downloaded-document cache may use in this mode.
    pub fn budget(self) -> u64 {
        match self {
            Self::Small => 100 * MIB,
            Self::Standard => 500 * MIB,
            // Bounded by the free-space reserve instead; see `service`.
            Self::Full => u64::MAX,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Standard => "standard",
            Self::Full => "full",
        }
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "small" => Some(Self::Small),
            "standard" => Some(Self::Standard),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// The library's order, kept so the app reopens sorted the same way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortOrder {
    #[default]
    Modified,
    Name,
}

impl SortOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Name => "name",
        }
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "modified" => Some(Self::Modified),
            "name" => Some(Self::Name),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub cache: CacheMode,
    pub sort: SortOrder,
}

impl Settings {
    /// `$XDG_CONFIG_HOME/turborm/settings.json`, else `~/.config/turborm/...`.
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or_else(|| anyhow!("The config directory is unavailable"))?;
        Ok(base.join("turborm").join("settings.json"))
    }

    /// The saved settings, or the defaults when there are none to read.
    pub fn load() -> Self {
        Self::path()
            .ok()
            .and_then(|path| fs::read(path).ok())
            .map(|bytes| Self::parse(&bytes))
            .unwrap_or_default()
    }

    /// Settings from JSON; an unknown value keeps that field's default.
    pub fn parse(bytes: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return Self::default();
        };
        let mut settings = Self::default();
        if let Some(cache) = value
            .get("cache")
            .and_then(|cache| cache.as_str())
            .and_then(CacheMode::from_name)
        {
            settings.cache = cache;
        }
        if let Some(sort) = value
            .get("sort")
            .and_then(|sort| sort.as_str())
            .and_then(SortOrder::from_name)
        {
            settings.sort = sort;
        }
        settings
    }

    /// Replace the settings file atomically.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("Invalid settings path"))?;
        fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.persist(&path)
            .map_err(|_| anyhow!("Could not save the settings"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_fall_back_field_by_field() {
        let full = Settings {
            cache: CacheMode::Full,
            sort: SortOrder::Name,
        };
        let bytes = serde_json::to_vec(&full).unwrap();
        assert_eq!(Settings::parse(&bytes), full);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"cache":"full","sort":"name"}"#
        );
        // A file saved before the sort was kept still loads, sorted by date.
        assert_eq!(
            Settings::parse(br#"{"cache":"small"}"#),
            Settings {
                cache: CacheMode::Small,
                sort: SortOrder::Modified,
            }
        );
        // Damaged files, unknown modes and unknown fields keep the defaults.
        assert_eq!(Settings::parse(b"not json"), Settings::default());
        assert_eq!(Settings::parse(br#"{"cache":"huge"}"#), Settings::default());
        assert_eq!(Settings::parse(br#"{"sort":"size"}"#), Settings::default());
        assert_eq!(
            Settings::parse(br#"{"cache":"small","later":1}"#).cache,
            CacheMode::Small
        );
        assert_eq!(Settings::default().cache, CacheMode::Standard);
    }

    #[test]
    fn modes_trade_space_for_speed() {
        assert!(CacheMode::Small.budget() < CacheMode::Standard.budget());
        assert!(CacheMode::Standard.budget() < CacheMode::Full.budget());
        assert_eq!(CacheMode::Standard.budget(), 500 * MIB);
        for mode in [CacheMode::Small, CacheMode::Standard, CacheMode::Full] {
            assert_eq!(CacheMode::from_name(mode.as_str()), Some(mode));
        }
    }
}
