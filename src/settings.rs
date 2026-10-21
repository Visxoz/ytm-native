//! Preferences persisted as JSON in the app's data directory.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `%APPDATA%\ytm-native`, where settings and caches live.
pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ytm-native")
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub volume: f32,
    pub muted: bool,
    pub normalise: bool,
    pub discord: bool,
    pub autoplay: bool,
    pub shuffle: bool,
    pub repeat: i32,
    pub sidebar_collapsed: bool,
    pub window: Option<WindowGeometry>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.8,
            muted: false,
            normalise: true,
            discord: true,
            autoplay: true,
            shuffle: false,
            repeat: 0,
            sidebar_collapsed: false,
            window: None,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read(settings_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let result = std::fs::create_dir_all(data_dir()).and_then(|()| {
            let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
            std::fs::write(settings_path(), json)
        });
        if let Err(e) = result {
            log::warn!("could not save settings: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_missing_fields_with_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"volume":0.5}"#).unwrap();
        assert_eq!(settings.volume, 0.5);
        assert!(settings.normalise);
    }
}
