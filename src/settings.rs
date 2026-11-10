//! Preferences persisted as JSON in the app's data directory.

use std::{
    collections::VecDeque,
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// `%APPDATA%\ytm-native`, where settings and caches live.
pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ytm-native")
}

/// Writes `bytes` to a temporary file and renames it over `path`, so a
/// crash or a full disk midway leaves the old file whole. Not flushed, as
/// most saves run on the UI thread. Each
/// write has its own temporary file, as rustypipe saves its cache from
/// whichever worker refreshed it, and two writes could otherwise interleave.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let partial = path.with_extension(format!("{}.tmp", NEXT.fetch_add(1, Ordering::Relaxed)));
    let result = std::fs::write(&partial, bytes).and_then(|()| std::fs::rename(&partial, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Saves `value` as JSON, returning whether it was saved. A failure is
/// logged as `what` that could not be saved.
pub(crate) fn save_json(path: &Path, value: &impl Serialize, what: &str) -> bool {
    let written = serde_json::to_vec(value)
        .map_err(io::Error::other)
        .and_then(|json| write_atomic(path, &json));
    if let Err(e) = &written {
        log::warn!("could not save {what}: {e}");
    }
    written.is_ok()
}

/// Numbers one file's saves, which run off the UI thread and can finish
/// out of order, so an older one finishing late can't overwrite a newer.
pub struct Saves {
    issued: AtomicU64,
    written: Mutex<u64>,
}

impl Saves {
    const fn new() -> Self {
        Self {
            issued: AtomicU64::new(0),
            written: Mutex::new(0),
        }
    }

    /// Taken on the UI thread, in the order the saves are asked for.
    pub fn ticket(&self) -> u64 {
        self.issued.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Saves `value` unless a later ticket's save has been written, which
    /// counts as saved. Returns whether it was saved.
    fn save(&self, ticket: u64, path: &Path, value: &impl Serialize, what: &str) -> bool {
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        if ticket < *written {
            return true;
        }
        let saved = save_json(path, value, what);
        if saved {
            *written = ticket;
        }
        saved
    }
}

pub static SESSION_SAVES: Saves = Saves::new();
pub static HISTORY_SAVES: Saves = Saves::new();

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

/// The app's colours: dark, light, or pure black for OLED screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Look {
    #[default]
    Dark,
    Light,
    Black,
}

impl Look {
    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Look::Light,
            2 => Look::Black,
            _ => Look::Dark,
        }
    }
}

/// YouTube Music's red, the default accent.
pub const YOUTUBE_RED: u32 = 0xff0033;

/// The colour of active controls: one of a few, or one taken from the
/// playing song's artwork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    #[default]
    Red,
    Orange,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
    Artwork,
}

impl Accent {
    /// In the order the Settings dialog offers them, which is their index.
    pub const ALL: [Self; 8] = [
        Accent::Red,
        Accent::Orange,
        Accent::Green,
        Accent::Teal,
        Accent::Blue,
        Accent::Purple,
        Accent::Pink,
        Accent::Artwork,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Accent::Red => "Red",
            Accent::Orange => "Orange",
            Accent::Green => "Green",
            Accent::Teal => "Teal",
            Accent::Blue => "Blue",
            Accent::Purple => "Purple",
            Accent::Pink => "Pink",
            Accent::Artwork => "From the artwork",
        }
    }

    /// The colour as `0xRRGGBB`, or `None` for the artwork's, which changes
    /// with the song.
    pub fn rgb(self) -> Option<u32> {
        match self {
            Accent::Red => Some(YOUTUBE_RED),
            Accent::Orange => Some(0xff6a13),
            Accent::Green => Some(0x1db954),
            Accent::Teal => Some(0x00a89d),
            Accent::Blue => Some(0x3d7bff),
            Accent::Purple => Some(0x9b5cff),
            Accent::Pink => Some(0xf0458f),
            Accent::Artwork => None,
        }
    }

    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        usize::try_from(i)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
            .unwrap_or_default()
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
    /// Report songs played to the signed-in account's YouTube history.
    pub youtube_history: bool,
    /// Show like and dislike buttons beside the song playing.
    pub rating_buttons: bool,
    pub shuffle: bool,
    /// Shuffle with recommended tracks mixed in; a kind of shuffle, so it
    /// goes off with it.
    pub smart_shuffle: bool,
    pub repeat: i32,
    pub sidebar_collapsed: bool,
    pub quality: Quality,
    /// What older versions had in place of `quality`'s High, and still
    /// read. Kept in step for them, and read here only by `load`; the app
    /// itself goes by `quality`.
    pub premium_audio: bool,
    /// The signed-in account has been served Premium formats, so songs
    /// without them, such as music videos, never mark it as not Premium.
    pub premium_account: bool,
    /// Fade each track into the next, rather than playing them gaplessly.
    pub crossfade: bool,
    /// Look for a newer version on GitHub now and then.
    pub check_updates: bool,
    pub equaliser: bool,
    /// Each band's gain in dB, kept while the equaliser is off.
    pub equaliser_gains: crate::audio::eq::Gains,
    pub look: Look,
    pub accent: Accent,
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
            youtube_history: true,
            rating_buttons: true,
            shuffle: false,
            smart_shuffle: false,
            repeat: 0,
            sidebar_collapsed: false,
            quality: Quality::High,
            premium_audio: true,
            premium_account: false,
            crossfade: false,
            check_updates: true,
            equaliser: false,
            equaliser_gains: Default::default(),
            look: Look::Dark,
            accent: Accent::Red,
            window: None,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let mut settings: Self = read_json(&settings_path()).unwrap_or_default();
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

    /// The gains to play with, or `None` with the equaliser off.
    pub fn equaliser(&self) -> Option<crate::audio::eq::Gains> {
        self.equaliser.then_some(self.equaliser_gains)
    }

    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        if !on {
            self.smart_shuffle = false;
        }
    }

    pub fn save(&self) {
        // Pretty, as people open this file to look at it.
        let result = std::fs::create_dir_all(data_dir()).and_then(|()| {
            let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
            write_atomic(&settings_path(), &json)
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
    /// Queue entries Smart Shuffle put in.
    #[serde(default)]
    pub recommended: Vec<usize>,
}

/// Longest queue kept between runs.
const SESSION_QUEUE_LIMIT: usize = 500;

fn session_path() -> PathBuf {
    data_dir().join("session.json")
}

impl Session {
    pub fn load() -> Option<Self> {
        read_json::<Self>(&session_path()).filter(|s| s.index < s.queue.len())
    }

    /// The part of a queue `len` long that is kept: all of it, or the
    /// window around the track at `index` if it is long.
    pub fn window(len: usize, index: usize) -> Range<usize> {
        if len <= SESSION_QUEUE_LIMIT {
            return 0..len;
        }
        let start = index.saturating_sub(SESSION_QUEUE_LIMIT / 2);
        start..(start + SESSION_QUEUE_LIMIT).min(len)
    }

    pub fn save(&self, ticket: u64) {
        SESSION_SAVES.save(ticket, &session_path(), self, "the session");
    }
}

fn home_path() -> PathBuf {
    data_dir().join("home.json")
}

/// The signed-in home feed as it last loaded in full. YouTube refuses some
/// requests for the feed while the rest of the account works, so Home shows
/// this when a fresh copy cannot be had.
pub fn load_home() -> Option<Vec<crate::api::Section>> {
    read_json::<Vec<crate::api::Section>>(&home_path()).filter(|sections| !sections.is_empty())
}

pub fn save_home(sections: &[crate::api::Section]) {
    save_json(&home_path(), &sections, "the home feed");
}

/// Drops the saved feed, which belongs to the account signed in when it was
/// saved.
pub fn remove_home() {
    let _ = std::fs::remove_file(home_path());
}

/// The songs played, newest first, one entry per song. Changes are kept in
/// memory until `save`, which the app calls a moment after the last one
/// and on exit, so a song's start never waits on the disk.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    tracks: VecDeque<crate::api::Track>,
    /// Bumped with each play, and how far a save is known to have got: a
    /// copy handed to a background save counts only once it is written, so
    /// the save on exit still writes plays that one never did.
    #[serde(skip)]
    revision: u64,
    #[serde(skip)]
    saved: u64,
}

const HISTORY_LIMIT: usize = 500;

fn history_path() -> PathBuf {
    data_dir().join("history.json")
}

impl History {
    pub fn load() -> Self {
        read_json(&history_path()).unwrap_or_default()
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
        self.revision += 1;
    }

    /// A copy to save and the revision it holds, if there are plays not
    /// yet known to be saved.
    pub fn unsaved(&self) -> Option<(Self, u64)> {
        (self.revision != self.saved).then(|| {
            let copy = Self {
                tracks: self.tracks.clone(),
                ..Self::default()
            };
            (copy, self.revision)
        })
    }

    /// Records that a copy from `unsaved` was written.
    pub fn saved_up_to(&mut self, revision: u64) {
        self.saved = self.saved.max(revision);
    }

    /// Returns whether it was saved.
    pub fn save(&self, ticket: u64) -> bool {
        HISTORY_SAVES.save(ticket, &history_path(), self, "the history")
    }
}

/// The last few searches, newest first, offered from focusing the search
/// box until the user types. Saved as soon as they change, being a few
/// hundred bytes, and again on exit should that write have failed.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Searches {
    queries: VecDeque<String>,
    #[serde(skip)]
    changed: bool,
}

const SEARCHES_LIMIT: usize = 5;

fn searches_path() -> PathBuf {
    data_dir().join("searches.json")
}

impl Searches {
    pub fn load() -> Self {
        read_json::<Self>(&searches_path())
            .unwrap_or_default()
            .cleaned()
    }

    /// Replays the searches oldest first through `record`, so a file edited
    /// by hand cannot bring in blanks, repeats or more than the limit.
    fn cleaned(self) -> Self {
        let mut searches = Self::default();
        for query in self.queries.iter().rev() {
            searches.record(query);
        }
        searches.changed = false;
        searches
    }

    /// Newest first.
    pub fn queries(&self) -> impl Iterator<Item = &str> {
        self.queries.iter().map(String::as_str)
    }

    /// Puts `query` first, dropping an earlier search for it whatever its
    /// capitals. Returns whether the searches changed.
    pub fn record(&mut self, query: &str) -> bool {
        let query = query.trim();
        if query.is_empty() || self.queries.front().is_some_and(|newest| newest == query) {
            return false;
        }
        self.remove(query);
        self.queries.push_front(query.to_owned());
        self.queries.truncate(SEARCHES_LIMIT);
        self.changed = true;
        true
    }

    /// Drops `query`, whatever its capitals, returning whether it was there.
    pub fn remove(&mut self, query: &str) -> bool {
        let before = self.queries.len();
        let query = query.trim().to_lowercase();
        self.queries.retain(|q| q.to_lowercase() != query);
        let removed = self.queries.len() != before;
        self.changed |= removed;
        removed
    }

    /// Writes the searches, if they changed since the last save.
    pub fn save(&mut self) {
        if self.changed && save_json(&searches_path(), self, "the recent searches") {
            self.changed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_keeps_short_queues_whole_and_long_ones_around_the_track() {
        assert_eq!(Session::window(400, 350), 0..400);
        assert_eq!(Session::window(1000, 100), 0..500);
        assert_eq!(Session::window(1000, 600), 350..850);
        assert_eq!(Session::window(1000, 990), 740..1000);
    }

    #[test]
    fn history_stays_unsaved_until_its_latest_play_is_written() {
        let track = |id: &str| crate::api::Track {
            id: id.into(),
            ..Default::default()
        };
        let mut history = History::default();
        assert!(history.unsaved().is_none());
        history.record(&track("a"));
        let (_, first) = history.unsaved().unwrap();
        // A play arriving while that copy is being written.
        history.record(&track("b"));
        history.saved_up_to(first);
        let (copy, second) = history.unsaved().unwrap();
        assert_eq!(copy.len(), 2);
        history.saved_up_to(second);
        history.saved_up_to(first);
        assert!(history.unsaved().is_none());
    }

    #[test]
    fn a_late_older_save_does_not_overwrite_a_newer_one() {
        let saves = Saves::new();
        let path = std::env::temp_dir().join(format!("ytm-saves-{}.json", std::process::id()));
        let (older, newer) = (saves.ticket(), saves.ticket());
        assert!(saves.save(newer, &path, &"newer", "a test"));
        assert!(saves.save(older, &path, &"older", "a test"));
        assert_eq!(read_json::<String>(&path).as_deref(), Some("newer"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn searches_keep_the_newest_five_once_each() {
        let mut searches = Searches::default();
        for q in ["a", "b", "c", "d", "e", "f"] {
            searches.record(q);
        }
        assert_eq!(
            searches.queries().collect::<Vec<_>>(),
            ["f", "e", "d", "c", "b"]
        );
        searches.record("  C ");
        assert_eq!(
            searches.queries().collect::<Vec<_>>(),
            ["C", "f", "e", "d", "b"]
        );
        assert!(!searches.record("C"), "searching again changes nothing");
        assert!(!searches.record("   "), "blank searches are not kept");
        assert!(!searches.remove("missing"));
        assert!(searches.remove("f"));
        assert_eq!(searches.queries().collect::<Vec<_>>(), ["C", "e", "d", "b"]);
        let json = serde_json::to_vec(&searches).unwrap();
        let back: Searches = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.queries().collect::<Vec<_>>(), ["C", "e", "d", "b"]);

        let edited: Searches =
            serde_json::from_str(r#"{"queries":["x","  ","X","y ","a","b","c","d"]}"#).unwrap();
        assert_eq!(
            edited.cleaned().queries().collect::<Vec<_>>(),
            ["x", "y", "a", "b", "c"],
            "trimmed, without blanks or repeats, the newest five"
        );
    }

    #[test]
    fn fills_missing_fields_with_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"volume":0.5}"#).unwrap();
        assert_eq!(settings.volume, 0.5);
        assert!(settings.normalise);
        assert!(!settings.crossfade, "off until chosen");
        assert_eq!(settings.quality, Quality::High);
    }

    #[test]
    fn replaces_files_whole() {
        let dir = std::env::temp_dir().join(format!("ytm-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        write_atomic(&path, b"[1]").unwrap();
        write_atomic(&path, b"[1,2]").unwrap();
        assert_eq!(read_json::<Vec<u8>>(&path), Some(vec![1, 2]));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "the partial files are renamed into place"
        );
        assert_eq!(read_json::<Vec<u8>>(&dir.join("missing.json")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn home_feed_survives_saving() {
        use crate::api::{Card, CardKind, CardLayout, QueueMode, Section, Track};
        let track = Track {
            id: "44zDzdf08s4".into(),
            title: "Matt Hardy 999".into(),
            thumb: Some("https://lh3.googleusercontent.com/x".into()),
            video: true,
            ..Default::default()
        };
        let sections = vec![
            Section::Cards {
                title: "Listen again".into(),
                cards: vec![Card {
                    kind: CardKind::Song,
                    id: track.id.clone(),
                    title: track.title.clone(),
                    subtitle: "Song".into(),
                    thumb: None,
                    color: Some(0x5c0f1c),
                    track: Some(track.clone()),
                }],
                layout: CardLayout::Carousel,
            },
            Section::Tracks {
                title: "Quick picks".into(),
                tracks: vec![track],
                mode: QueueMode::Section,
                numbered: false,
            },
        ];
        let json = serde_json::to_vec(&sections).unwrap();
        let back: Vec<Section> = serde_json::from_slice(&json).unwrap();
        let Section::Cards {
            title,
            cards,
            layout,
        } = &back[0]
        else {
            panic!("expected cards")
        };
        assert_eq!(title, "Listen again");
        assert_eq!(*layout, CardLayout::Carousel);
        assert_eq!(cards[0].kind, CardKind::Song);
        assert_eq!(cards[0].color, Some(0x5c0f1c));
        assert!(cards[0].track.as_ref().is_some_and(|t| t.video));
        assert!(matches!(
            &back[1],
            Section::Tracks {
                mode: QueueMode::Section,
                ..
            }
        ));
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

    #[test]
    fn looks_and_accents_round_trip_their_index() {
        for (i, accent) in Accent::ALL.into_iter().enumerate() {
            assert_eq!(accent.index(), i as i32, "ALL is in declaration order");
            assert_eq!(Accent::from_index(accent.index()), accent);
        }
        assert_eq!(Accent::from_index(-1), Accent::Red);
        assert_eq!(Accent::from_index(99), Accent::Red);
        assert_eq!(Accent::Artwork.rgb(), None);
        for look in [Look::Dark, Look::Light, Look::Black] {
            assert_eq!(Look::from_index(look.index()), look);
        }
        let settings: Settings =
            serde_json::from_str(r#"{"look":"black","accent":"artwork"}"#).unwrap();
        assert_eq!(
            (settings.look, settings.accent),
            (Look::Black, Accent::Artwork)
        );
    }
}
