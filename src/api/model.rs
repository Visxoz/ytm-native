pub use rustypipe::model::PlaylistPrivacy;
use rustypipe::model::{TrackItem, paginator::Paginator};

use super::convert::{HEADER_THUMB, join_artists, resize_thumb};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtistRef {
    pub id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// The entry's own ID in the account's playlist it came from, which
    /// removing or moving it takes. Never saved, as it means nothing away
    /// from that playlist's page.
    #[serde(skip)]
    pub set_video_id: Option<String>,
}

impl crate::queue::Spread for Track {
    // By name, as rows don't all carry the artist's id.
    fn artist(&self) -> &str {
        self.artists.first().map_or("", |a| a.name.as_str())
    }

    fn album(&self) -> &str {
        self.album_name()
    }
}

impl Track {
    pub fn artist_names(&self) -> String {
        join_artists(&self.artists)
    }

    pub fn first_artist_id(&self) -> Option<&str> {
        self.artists.iter().find_map(|a| a.id.as_deref())
    }

    /// A file on this PC, whose ID is its path; it plays only from there.
    pub fn is_local(&self) -> bool {
        std::path::Path::new(&self.id).is_absolute()
    }

    /// The album's page on YouTube; a local file's album has none.
    pub fn album_id(&self) -> Option<&str> {
        self.album
            .as_ref()
            .filter(|_| !self.is_local())
            .map(|(id, _)| id.as_str())
    }

    pub fn album_name(&self) -> &str {
        self.album.as_ref().map_or("", |(_, name)| name.as_str())
    }

    /// The length in seconds, or zero when YouTube did not say.
    pub fn seconds(&self) -> f64 {
        self.duration.map_or(0.0, f64::from)
    }

    /// A larger rendition of the thumbnail for the player and Discord.
    pub fn large_thumb(&self) -> Option<String> {
        self.thumb
            .as_deref()
            .map(|url| resize_thumb(url, HEADER_THUMB))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueueMode {
    /// Queue the whole section from the clicked track.
    Section,
    /// Play the track, then a radio based on it.
    Radio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CardLayout {
    Carousel,
    Grid,
    Chips,
    /// One large card for the best search match.
    Top,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

impl Section {
    pub fn title(&self) -> &str {
        match self {
            Self::Tracks { title, .. } | Self::Cards { title, .. } => title,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Header {
    /// "Album", "Playlist", "Artist"…, shown above the title.
    pub label: String,
    pub title: String,
    pub subtitle: String,
    pub meta: String,
    pub thumb: Option<String>,
    pub round: bool,
    /// Artists show their wide header image behind the title.
    pub banner: bool,
    /// Playlist whose tracks Shuffle loads when the page's are incomplete.
    pub shuffle_playlist: Option<String>,
    pub radio_id: Option<String>,
    /// One of the account's own playlists, which it can edit.
    pub editable: bool,
    /// Who can see the account's own playlist.
    pub privacy: Option<PlaylistPrivacy>,
    /// Whether the album or playlist is in the library; unknown signed out
    /// and on the account's own playlists.
    pub saved: Option<bool>,
    /// The playlist saving it targets, an album's being its audio playlist.
    pub save_id: Option<String>,
    /// Whether the account is subscribed to the artist; unknown signed out.
    pub subscribed: Option<bool>,
    /// The channel subscribing to the artist takes.
    pub channel_id: Option<String>,
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
    Shelves(super::home_feed::Feed),
}

#[derive(Debug, Clone)]
pub struct MoreTracks {
    pub(super) tracks: Paginator<TrackItem>,
    pub(super) count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Route {
    Home,
    Explore,
    Library,
    /// The songs played: the account's YouTube Music history when signed
    /// in, otherwise the one kept locally.
    History,
    Downloads,
    /// Songs in the folders the user added.
    LocalFiles,
    Search(String),
    Album(String),
    Playlist(String),
    Artist(String),
    Genre(String),
}

/// A song's, album's, playlist's or artist's address on YouTube Music, by
/// the kind names cards use.
pub fn link(kind: &str, id: &str) -> Option<String> {
    let path = match kind {
        "song" => "watch?v=",
        // Albums go by their MPREb_ browse ID.
        "album" => "browse/",
        "playlist" => "playlist?list=",
        "artist" => "channel/",
        _ => return None,
    };
    Some(format!("https://music.youtube.com/{path}{id}"))
}

impl Route {
    /// The page's address on YouTube Music, for those that have one.
    pub fn link(&self) -> Option<String> {
        match self {
            Route::Album(id) => link("album", id),
            Route::Playlist(id) => link("playlist", id),
            Route::Artist(id) => link("artist", id),
            _ => None,
        }
    }

    pub fn nav_name(&self) -> &'static str {
        match self {
            Route::Home => "home",
            Route::Explore => "explore",
            Route::Library => "library",
            Route::History => "history",
            Route::Downloads => "downloads",
            Route::LocalFiles => "local",
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_name_each_kind_and_skip_pages_without_one() {
        assert_eq!(
            link("song", "dQw4w9WgXcQ").as_deref(),
            Some("https://music.youtube.com/watch?v=dQw4w9WgXcQ")
        );
        assert_eq!(
            Route::Album("MPREb_x".into()).link().as_deref(),
            Some("https://music.youtube.com/browse/MPREb_x")
        );
        assert_eq!(
            Route::Playlist("PLx".into()).link().as_deref(),
            Some("https://music.youtube.com/playlist?list=PLx")
        );
        assert_eq!(
            Route::Artist("UCx".into()).link().as_deref(),
            Some("https://music.youtube.com/channel/UCx")
        );
        assert_eq!(link("genre", "x"), None);
        assert_eq!(Route::Home.link(), None);
        assert_eq!(Route::Search("x".into()).link(), None);
    }
}
