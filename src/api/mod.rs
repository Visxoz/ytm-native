//! YouTube Music data through rustypipe, shaped into the pages the UI shows.

mod account;
mod convert;
mod format;
mod home_feed;
mod model;
mod pages;

use rustypipe::{
    client::{ClientType, RustyPipe, RustyPipeQuery},
    model::{MusicPlaylist, TrackItem, TrackType, paginator::Paginator},
};

pub use convert::resize_thumb;
use convert::tracks;
pub use format::format_duration;
pub use model::*;

/// Longest playlist loaded in one go.
const PLAYLIST_LIMIT: usize = 500;

pub type Result<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Only the same title by one of the same artists counts, so an edit is
/// never swapped for the original or a namesake by someone else.
fn is_song_version(video: &Track, song: &Track) -> bool {
    let same_artist = |a: &ArtistRef, b: &ArtistRef| {
        (a.id.is_some() && a.id == b.id) || a.name.eq_ignore_ascii_case(&b.name)
    };
    fn folded(title: &str) -> impl Iterator<Item = char> + '_ {
        title.trim().chars().flat_map(char::to_lowercase)
    }
    folded(&song.title).eq(folded(&video.title))
        && song
            .artists
            .iter()
            .any(|s| video.artists.iter().any(|v| same_artist(s, v)))
}

#[derive(Clone)]
pub struct Api {
    rp: RustyPipe,
}

impl Api {
    pub fn new() -> Result<Self> {
        let dir = crate::settings::data_dir();
        let rp = RustyPipe::builder()
            .storage(Box::new(crate::secret::EncryptedStorage::new(&dir)))
            .storage_dir(dir)
            .no_botguard()
            .build_with_client(crate::http::keep_alive(reqwest::ClientBuilder::new()))
            .map_err(err)?;
        Ok(Self { rp })
    }

    fn q(&self) -> RustyPipeQuery {
        self.rp.query()
    }

    /// A query that sends the sign-in cookie when there is one: private
    /// playlists, Liked music and radios made from them exist only for their
    /// owner.
    fn user_q(&self) -> RustyPipeQuery {
        let q = self.q();
        if q.auth_enabled(ClientType::DesktopMusic) {
            q.authenticated()
        } else {
            q
        }
    }

    /// A playlist as its owner sees it: its first page, or up to
    /// `PLAYLIST_LIMIT` tracks when `full`. Should the cookie have gone stale,
    /// public playlists still open signed out.
    async fn fetch_playlist(&self, id: &str, full: bool) -> Result<MusicPlaylist> {
        let fetch = |q: RustyPipeQuery| async move {
            let mut playlist = q.music_playlist(id).await?;
            if full {
                let _ = playlist.tracks.extend_limit(&q, PLAYLIST_LIMIT).await;
            }
            Ok::<_, rustypipe::error::Error>(playlist)
        };
        let q = self.user_q();
        let signed_in = q.auth_enabled(ClientType::DesktopMusic);
        match fetch(q).await {
            Ok(playlist) => Ok(playlist),
            Err(e) if signed_in => fetch(self.q()).await.map_err(|_| err(e)),
            Err(e) => Err(err(e)),
        }
    }

    /// Tracks of an album or playlist, for playing a card directly.
    pub async fn card_tracks(&self, kind: CardKind, id: &str) -> Result<Vec<Track>> {
        match kind {
            CardKind::Album => Ok(self.album(id).await?.header_tracks().to_vec()),
            CardKind::Playlist => self.playlist_tracks(id).await,
            // The top songs only, without the discography the page fetches.
            CardKind::Artist => {
                let artist = self.q().music_artist(id, false).await.map_err(err)?;
                Ok(tracks(artist.tracks))
            }
            CardKind::Genre | CardKind::Song => Ok(Vec::new()),
        }
    }

    pub async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let playlist = self.fetch_playlist(id, true).await?;
        Ok(tracks(playlist.tracks.items))
    }

    pub async fn radio(&self, radio_id: &str) -> Result<Vec<Track>> {
        let items: Paginator<TrackItem> = self.user_q().music_radio(radio_id).await.map_err(err)?;
        Ok(tracks(items.items))
    }

    pub async fn track_radio(&self, video_id: &str) -> Result<Vec<Track>> {
        let items = self.q().music_radio_track(video_id).await.map_err(err)?;
        Ok(tracks(items.items))
    }

    /// The song a music video is the video of, found by its title and
    /// artists. Uploads, such as slowed or sped-up edits, have none.
    pub async fn song_version(&self, video: &Track) -> Result<Option<Track>> {
        let query = format!("{} {}", video.title, video.artist_names());
        let found = self.q().music_search_tracks(&query).await.map_err(err)?;
        Ok(found
            .items
            .items
            .into_iter()
            .filter(|t| t.track_type == TrackType::Track)
            .map(convert::track)
            .find(|song| is_song_version(video, song)))
    }

    pub async fn lyrics(&self, video_id: &str) -> Result<Option<String>> {
        let details = self.q().music_details(video_id).await.map_err(err)?;
        let Some(lyrics_id) = details.lyrics_id else {
            return Ok(None);
        };
        let lyrics = match self.q().music_lyrics(&lyrics_id).await {
            Ok(lyrics) => lyrics,
            Err(rustypipe::error::Error::Extraction(
                rustypipe::error::ExtractionError::NotFound { .. },
            )) => return Ok(None),
            Err(e) => return Err(err(e)),
        };
        let mut text = lyrics.body;
        if !lyrics.footer.is_empty() {
            text.push_str("\n\n");
            text.push_str(&lyrics.footer);
        }
        Ok(Some(text))
    }

    pub async fn suggestions(&self, query: &str) -> Vec<String> {
        self.q()
            .music_search_suggestion(query)
            .await
            .map(|s| s.terms)
            .unwrap_or_default()
    }

    /// A YouTube visitor ID, which the player endpoint wants.
    pub async fn visitor_data(&self, fresh: bool) -> Result<String> {
        self.q().get_visitor_data(fresh).await.map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, title: &str, artists: &[(&str, &str)]) -> Track {
        Track {
            id: id.into(),
            title: title.into(),
            artists: artists
                .iter()
                .map(|(id, name)| ArtistRef {
                    id: (!id.is_empty()).then(|| (*id).into()),
                    name: (*name).into(),
                })
                .collect(),
            album: None,
            duration: None,
            thumb: None,
            track_nr: None,
            video: false,
        }
    }

    #[test]
    fn matches_a_music_video_to_its_song() {
        let video = track(
            "mqEZcXdtAUA",
            "Fever",
            &[("UCbuck", "Buckshot"), ("UCfake", "fakemink")],
        );
        let song = track(
            "FDzYuXpyUTI",
            "Fever",
            &[("UCbuck", "Buckshot"), ("UCfake", "fakemink")],
        );
        assert!(is_song_version(&video, &song));
        let by_name = track("x", "fever ", &[("", "FAKEMINK")]);
        assert!(is_song_version(&video, &by_name));
    }

    #[test]
    fn keeps_edits_and_namesakes() {
        let video = track("mqEZcXdtAUA", "Fever", &[("UCbuck", "Buckshot")]);
        let solo = track("f61jlvSDYRc", "Fever (Solo)", &[("UCbuck", "Buckshot")]);
        assert!(!is_song_version(&video, &solo));
        let namesake = track("x", "Fever", &[("UCpeggy", "Peggy Lee")]);
        assert!(!is_song_version(&video, &namesake));

        let upload = track(
            "STX02ldQjJ4",
            "Cult Member - U Weren't Here I Really Miss You (ORIGINAL SONG + BEST PART + SLOWED)",
            &[("UCupload", "some uploader")],
        );
        let original = track(
            "x",
            "U Weren't Here I Really Miss You",
            &[("UCcult", "Cult Member")],
        );
        assert!(!is_song_version(&upload, &original));
    }
}
