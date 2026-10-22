//! The pages the UI shows, independent of rustypipe's models.

use rustypipe::model::{TrackItem, paginator::Paginator};

use super::convert::{HEADER_THUMB, join_artists, resize_thumb};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtistRef {
    pub id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artists: Vec<ArtistRef>,
    pub album: Option<(String, String)>,
    pub duration: Option<u32>,
    pub thumb: Option<String>,
    pub track_nr: Option<u16>,
    /// A music video, which may have a song version: audio only, and with
    /// Premium formats.
    #[serde(default)]
    pub video: bool,
}

impl Track {
    pub fn artist_names(&self) -> String {
        join_artists(&self.artists)
    }

    pub fn first_artist_id(&self) -> Option<&str> {
        self.artists.iter().find_map(|a| a.id.as_deref())
    }

    /// A larger rendition of the thumbnail for the player and Discord.
    pub fn large_thumb(&self) -> Option<String> {
        self.thumb
            .as_deref()
            .map(|url| resize_thumb(url, HEADER_THUMB))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    Album,
    Playlist,
    Artist,
    Genre,
    /// A single song, played with a radio after it.
    Song,
}

impl CardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CardKind::Album => "album",
            CardKind::Playlist => "playlist",
            CardKind::Artist => "artist",
            CardKind::Genre => "genre",
            CardKind::Song => "song",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Card {
    pub kind: CardKind,
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub thumb: Option<String>,
    /// Genre chips carry YouTube's accent colour.
    pub color: Option<u32>,
    /// The song a song card plays.
    pub track: Option<Track>,
}

/// How a click on one track of a section fills the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    /// Queue the whole section from the clicked track.
    Section,
    /// Play the track, then a radio based on it.
    Radio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardLayout {
    Carousel,
    Grid,
    Chips,
    /// One large card for the best search match.
    Top,
}

#[derive(Debug, Clone)]
pub enum Section {
    Tracks {
        title: String,
        tracks: Vec<Track>,
        mode: QueueMode,
        /// Album pages show track numbers instead of artwork.
        numbered: bool,
    },
    Cards {
        title: String,
        cards: Vec<Card>,
        layout: CardLayout,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Header {
    /// "Album", "Playlist", "Artist"…, shown above the title.
    pub label: String,
    pub title: String,
    pub subtitle: String,
    pub meta: String,
    pub description: String,
    pub thumb: Option<String>,
    pub round: bool,
    /// Artists show their wide header image behind the title.
    pub banner: bool,
    /// Playlist whose tracks Shuffle loads when the page's are incomplete.
    pub shuffle_playlist: Option<String>,
    pub radio_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub header: Option<Header>,
    pub sections: Vec<Section>,
    /// What the page still has to fetch after it shows; `Api::complete`
    /// fetches a step of it at a time.
    pub more: Option<More>,
}

impl Page {
    /// The tracks the header's Play and Shuffle buttons queue: the page's
    /// track list, which comes first.
    pub fn header_tracks(&self) -> &[Track] {
        match self.sections.first() {
            Some(Section::Tracks { tracks, .. }) if self.header.is_some() => tracks,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone)]
pub enum More {
    /// The rest of a long playlist's tracks.
    Tracks(MoreTracks),
    /// The home feed's further shelves.
    Shelves(MoreShelves),
}

#[derive(Debug, Clone)]
pub struct MoreTracks {
    pub(super) tracks: Paginator<TrackItem>,
    pub(super) count: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct MoreShelves {
    /// Continuation token of the next page.
    pub(super) token: String,
    /// Pages still to fetch.
    pub(super) pages_left: usize,
    /// Sections shown after the last shelf, such as the genre chips.
    pub(super) trailing: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Route {
    Home,
    Explore,
    Library,
    /// The songs played, kept locally.
    History,
    Search(String),
    Album(String),
    Playlist(String),
    Artist(String),
    Genre(String, String),
}

impl Route {
    pub fn nav_name(&self) -> &'static str {
        match self {
            Route::Home => "home",
            Route::Explore => "explore",
            Route::Library => "library",
            Route::History => "history",
            _ => "",
        }
    }
}
