//! Preferences persisted as JSON in the app's data directory.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `%APPDATA%\ytm-native`, where settings, caches and yt-dlp live.
pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ytm-native")
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

/// Netscape cookie file handed to yt-dlp while signed in.
pub fn cookies_path() -> PathBuf {
    data_dir().join("cookies.txt")
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
    /// Opus decodes in pure Rust at about ten times the CPU cost of AAC.
    pub prefer_aac: bool,
    pub discord: bool,
    pub autoplay: bool,
    pub shuffle: bool,
    pub repeat: i32,
    pub window: Option<WindowGeometry>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.8,
            muted: false,
            normalise: true,
            prefer_aac: true,
            discord: true,
            autoplay: true,
            shuffle: false,
            repeat: 0,
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

/// Converts a pasted `Cookie:` header or cookies.txt into a Netscape cookie
/// file for yt-dlp.
pub fn netscape_cookies(pasted: &str) -> String {
    let pasted = pasted.trim();
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    if pasted.contains('\t') {
        for line in pasted
            .lines()
            .filter(|l| !l.starts_with('#') && l.contains('\t'))
        {
            out.push_str(line.trim_end());
            out.push('\n');
        }
        return out;
    }
    let header = pasted.strip_prefix("Cookie:").unwrap_or(pasted);
    // A year out; YouTube rotates the session long before.
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        + 365 * 24 * 60 * 60;
    for pair in header.split(';') {
        if let Some((name, value)) = pair.trim().split_once('=') {
            out.push_str(&format!(
                ".youtube.com\tTRUE\t/\tTRUE\t{expiry}\t{}\t{}\n",
                name.trim(),
                value.trim()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_cookie_header() {
        let file = netscape_cookies("Cookie: SID=abc; HSID=def=g");
        let lines: Vec<_> = file.lines().collect();
        assert_eq!(lines[0], "# Netscape HTTP Cookie File");
        assert!(lines[1].starts_with(".youtube.com\tTRUE\t/\tTRUE\t"));
        assert!(lines[1].ends_with("\tSID\tabc"));
        assert!(lines[2].ends_with("\tHSID\tdef=g"));
    }

    #[test]
    fn keeps_cookie_file_lines() {
        let file = netscape_cookies("# comment\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tabc\n");
        assert_eq!(file.lines().count(), 2);
        assert!(file.ends_with("\tSID\tabc\n"));
    }

    #[test]
    fn fills_missing_fields_with_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"volume":0.5}"#).unwrap();
        assert_eq!(settings.volume, 0.5);
        assert!(settings.normalise);
    }
}
