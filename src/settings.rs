//! Preferences persisted as JSON in the app's data directory.

use std::{collections::VecDeque, path::PathBuf};

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

/// How much of a stream to ask for. Low and Normal cap the bitrate, for
/// metered connections; High takes the best YouTube offers, which is 256
/// kbps when signed in with Premium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Low,
    Normal,
    #[default]
    High,
}

impl Quality {
    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            0 => Quality::Low,
            1 => Quality::Normal,
            _ => Quality::High,
        }
    }

    /// The highest bitrate wanted, in kbps, or `None` for the best there
    /// is. YouTube's Opus streams come at about 50, 70 and 130 kbps.
    pub fn max_kbps(self) -> Option<u64> {
        match self {
            Quality::Low => Some(64),
            Quality::Normal => Some(192),
            Quality::High => None,
        }
    }
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
    pub quality: Quality,
    /// Use 256 kbps streams when signed in with YouTube Premium: High
    /// quality. Kept alongside `quality`, as the code and older settings
    /// files use it.
    pub premium_audio: bool,
    /// The signed-in account has been served Premium formats, so songs
    /// without them, such as music videos, never mark it as not Premium.
    pub premium_account: bool,
    /// Look for a newer version on GitHub now and then.
    pub check_updates: bool,
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
            quality: Quality::High,
            premium_audio: true,
            premium_account: false,
            check_updates: true,
            window: None,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let mut settings: Self = std::fs::read(settings_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        // Premium audio turned off by an older version means Normal.
        if !settings.premium_audio && settings.quality == Quality::High {
            settings.quality = Quality::Normal;
        }
        settings
    }

    pub fn set_quality(&mut self, quality: Quality) {
        self.quality = quality;
        self.premium_audio = quality == Quality::High;
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

/// The songs played, newest first, one entry per song. Changes are kept in
/// memory until `save`, which the app calls a moment after the last one
/// and on exit, so a song's start never waits on the disk.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    tracks: VecDeque<crate::api::Track>,
    #[serde(skip)]
    changed: bool,
}

const HISTORY_LIMIT: usize = 500;

fn history_path() -> PathBuf {
    data_dir().join("history.json")
}

impl History {
    pub fn load() -> Self {
        std::fs::read(history_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Newest first.
    pub fn tracks(&self) -> impl Iterator<Item = &crate::api::Track> {
        self.tracks.iter()
    }

    /// Puts `track` first, dropping an earlier play of it.
    pub fn record(&mut self, track: &crate::api::Track) {
        self.tracks.retain(|t| t.id != track.id);
        self.tracks.push_front(track.clone());
        self.tracks.truncate(HISTORY_LIMIT);
        self.changed = true;
    }

    /// Writes the history, if it changed since the last save.
    pub fn save(&mut self) {
        if !self.changed {
            return;
        }
        let written = serde_json::to_vec(self)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(history_path(), json));
        match written {
            Ok(()) => self.changed = false,
            Err(e) => log::warn!("could not save the history: {e}"),
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
        assert_eq!(settings.quality, Quality::High);
    }

    #[test]
    fn quality_and_premium_audio_agree() {
        let mut settings = Settings::default();
        settings.set_quality(Quality::Low);
        assert!(!settings.premium_audio);
        settings.set_quality(Quality::High);
        assert!(settings.premium_audio);
        assert_eq!(
            Quality::from_index(Quality::Normal.index()),
            Quality::Normal
        );
        assert_eq!(Quality::High.max_kbps(), None);
    }
}
