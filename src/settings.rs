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

/// The signed-in session's `Cookie` header, for Premium stream requests.
pub fn load_cookie() -> Option<String> {
    let dir = data_dir();
    crate::secret::read_migrating(&dir.join("cookie.bin"), &dir.join("cookie"))
        .filter(|c| !c.trim().is_empty())
}

pub fn save_cookie(cookie: &str) {
    if let Err(e) = crate::secret::write(&data_dir().join("cookie.bin"), cookie) {
        log::warn!("could not save the session for Premium audio: {e}");
    }
}

pub fn remove_cookie() {
    let _ = std::fs::remove_file(data_dir().join("cookie.bin"));
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
    /// Use 256 kbps streams when signed in with YouTube Premium.
    pub premium_audio: bool,
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
            premium_audio: true,
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

/// The queue and position at exit, restored paused on the next start.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub queue: Vec<crate::api::Track>,
    pub index: usize,
    pub position: f64,
    /// Length of the current track, which feed cards do not carry.
    #[serde(default)]
    pub duration: f64,
}

/// Longest queue kept between runs.
const SESSION_QUEUE_LIMIT: usize = 500;

fn session_path() -> PathBuf {
    data_dir().join("session.json")
}

impl Session {
    pub fn load() -> Option<Self> {
        let bytes = std::fs::read(session_path()).ok()?;
        let session: Self = serde_json::from_slice(&bytes).ok()?;
        (session.index < session.queue.len()).then_some(session)
    }

    pub fn save(mut self) {
        // Keep the window around the current track if the queue is long.
        if self.queue.len() > SESSION_QUEUE_LIMIT {
            let start = self.index.saturating_sub(SESSION_QUEUE_LIMIT / 2);
            let end = (start + SESSION_QUEUE_LIMIT).min(self.queue.len());
            self.queue = self.queue[start..end].to_vec();
            self.index -= start;
        }
        match serde_json::to_vec(&self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(session_path(), json) {
                    log::warn!("could not save the session: {e}");
                }
            }
            Err(e) => log::warn!("could not save the session: {e}"),
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
