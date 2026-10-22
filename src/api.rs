//! YouTube Music data through rustypipe, shaped into the pages the UI shows.

use rustypipe::{
    client::{ClientType, RustyPipe, RustyPipeQuery},
    model::{
        AlbumItem, AlbumType, ArtistId, ArtistItem, MusicItem, MusicPlaylist, MusicPlaylistItem,
        Thumbnail, TrackItem, paginator::Paginator, richtext::ToPlaintext,
    },
};

/// Thumbnail edge, in pixels, wanted for list rows and for cards.
pub const ROW_THUMB: u32 = 120;
pub const CARD_THUMB: u32 = 240;
const HEADER_THUMB: u32 = 544;
/// Longest playlist loaded in one go.
const PLAYLIST_LIMIT: usize = 500;
const HOME_SHELF: usize = 20;

pub type Result<T> = std::result::Result<T, String>;

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
    /// Tracks the Play and Shuffle buttons queue.
    pub tracks: Vec<Track>,
    /// Playlist whose tracks Shuffle loads when `tracks` is incomplete.
    pub shuffle_playlist: Option<String>,
    pub radio_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub header: Option<Header>,
    pub sections: Vec<Section>,
    /// Tracks of a long playlist still to fetch after its first page shows.
    pub more: Option<MoreTracks>,
}

#[derive(Debug, Clone)]
pub struct MoreTracks {
    tracks: Paginator<TrackItem>,
    count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Route {
    Home,
    Explore,
    Library,
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
            _ => "",
        }
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn join_artists(artists: &[ArtistRef]) -> String {
    artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Picks the smallest thumbnail at least `edge` pixels wide.
pub fn pick_thumb(thumbs: &[Thumbnail], edge: u32) -> Option<String> {
    let best = thumbs
        .iter()
        .filter(|t| t.width >= edge)
        .min_by_key(|t| t.width)
        .or_else(|| thumbs.iter().max_by_key(|t| t.width))?;
    Some(resize_thumb(&best.url, edge))
}

/// googleusercontent thumbnails render at any size given in the URL suffix.
pub fn resize_thumb(url: &str, edge: u32) -> String {
    if url.contains("googleusercontent.com")
        && let Some(eq) = url.rfind('=')
        && url[eq + 1..].starts_with('w')
    {
        return format!("{}=w{edge}-h{edge}-l90-rj", &url[..eq]);
    }
    url.to_owned()
}

fn artist_refs(artists: &[ArtistId]) -> Vec<ArtistRef> {
    artists
        .iter()
        .map(|a| ArtistRef {
            id: a.id.clone(),
            name: a.name.clone(),
        })
        .collect()
}

fn track(t: TrackItem) -> Track {
    Track {
        thumb: pick_thumb(&t.cover, ROW_THUMB),
        artists: artist_refs(&t.artists),
        album: t.album.map(|a| (a.id, a.name)),
        duration: t.duration,
        track_nr: t.track_nr,
        id: t.id,
        title: t.name,
    }
}

fn tracks(items: Vec<TrackItem>) -> Vec<Track> {
    items.into_iter().map(track).collect()
}

fn album_type(t: AlbumType) -> &'static str {
    match t {
        AlbumType::Album => "Album",
        AlbumType::Ep => "EP",
        AlbumType::Single => "Single",
        AlbumType::Audiobook => "Audiobook",
        AlbumType::Show => "Podcast",
        _ => "Album",
    }
}

fn album_card(a: AlbumItem) -> Card {
    let mut subtitle = vec![album_type(a.album_type).to_owned()];
    let artists = join_artists(&artist_refs(&a.artists));
    if !artists.is_empty() {
        subtitle.push(artists);
    }
    if let Some(year) = a.year {
        subtitle.push(year.to_string());
    }
    Card {
        kind: CardKind::Album,
        thumb: pick_thumb(&a.cover, CARD_THUMB),
        id: a.id,
        title: a.name,
        subtitle: subtitle.join(" • "),
        color: None,
        track: None,
    }
}

fn artist_card(a: ArtistItem) -> Card {
    Card {
        kind: CardKind::Artist,
        thumb: pick_thumb(&a.avatar, CARD_THUMB),
        id: a.id,
        title: a.name,
        subtitle: a.subscriber_count.map_or_else(
            || "Artist".to_owned(),
            |n| format!("{} subscribers", compact(n)),
        ),
        color: None,
        track: None,
    }
}

fn playlist_card(p: MusicPlaylistItem) -> Card {
    let mut subtitle = vec!["Playlist".to_owned()];
    if let Some(channel) = &p.channel {
        subtitle.push(channel.name.clone());
    }
    if let Some(n) = p.track_count {
        subtitle.push(songs(n));
    }
    Card {
        kind: CardKind::Playlist,
        thumb: pick_thumb(&p.thumbnail, CARD_THUMB),
        id: p.id,
        title: p.name,
        subtitle: subtitle.join(" • "),
        color: None,
        track: None,
    }
}

/// 1234567 → "1.2M".
pub fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => trim_decimal(n as f64 / 1e3, "K"),
        1_000_000..1_000_000_000 => trim_decimal(n as f64 / 1e6, "M"),
        _ => trim_decimal(n as f64 / 1e9, "B"),
    }
}

fn trim_decimal(value: f64, suffix: &str) -> String {
    if value >= 100.0 {
        format!("{value:.0}{suffix}")
    } else {
        format!("{:.1}", value).trim_end_matches(".0").to_owned() + suffix
    }
}

/// "1 song", "12 songs".
fn songs(n: u64) -> String {
    if n == 1 {
        "1 song".to_owned()
    } else {
        format!("{n} songs")
    }
}

/// 3725 → "1:02:05".
pub fn format_duration(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Total running time as "1 hr 5 min" or "42 min".
fn total_time(tracks: &[Track]) -> Option<String> {
    let total: u32 = tracks.iter().filter_map(|t| t.duration).sum();
    if total == 0 {
        return None;
    }
    let minutes = total.div_ceil(60);
    Some(if minutes >= 60 {
        format!("{} hr {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    })
}

fn playlist_meta(count: Option<u64>, tracks: &[Track]) -> String {
    let mut meta = vec![songs(count.unwrap_or(tracks.len() as u64))];
    meta.extend(total_time(tracks));
    meta.join(" • ")
}

fn track_section(title: &str, tracks: Vec<Track>, mode: QueueMode) -> Option<Section> {
    (!tracks.is_empty()).then(|| Section::Tracks {
        title: title.to_owned(),
        tracks,
        mode,
        numbered: false,
    })
}

fn card_section(title: &str, cards: Vec<Card>, layout: CardLayout) -> Option<Section> {
    (!cards.is_empty()).then(|| Section::Cards {
        title: title.to_owned(),
        cards,
        layout,
    })
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
            .build()
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

    pub async fn page(&self, route: &Route, signed_in: bool) -> Result<Page> {
        match route {
            Route::Home => self.home(signed_in).await,
            Route::Explore => self.explore().await,
            Route::Library => self.library().await,
            Route::Search(query) => self.search(query).await,
            Route::Album(id) => self.album(id).await,
            Route::Playlist(id) => self.playlist(id).await,
            Route::Artist(id) => self.artist(id).await,
            Route::Genre(id, _) => self.genre(id).await,
        }
    }

    async fn home(&self, signed_in: bool) -> Result<Page> {
        let q = self.q();
        if signed_in {
            let (feed, genres) = tokio::join!(crate::home_feed::fetch(&q), q.music_genres());
            match feed {
                Ok(mut sections) if !sections.is_empty() => {
                    sections.extend(card_section(
                        "Browse moods & genres",
                        genres
                            .unwrap_or_default()
                            .into_iter()
                            .take(12)
                            .map(genre_card)
                            .collect(),
                        CardLayout::Chips,
                    ));
                    return Ok(Page {
                        header: None,
                        sections,
                        more: None,
                    });
                }
                Ok(_) => log::warn!("the personal home feed was empty"),
                Err(e) => log::warn!("could not load the personal home feed: {e}"),
            }
        }

        let (charts, new_albums, genres) =
            tokio::join!(q.music_charts(None), q.music_new_albums(), q.music_genres());
        let charts = charts.map_err(err)?;

        // Chart track lists are empty nowadays; the Trending playlist has them.
        let trending = match charts
            .playlists
            .iter()
            .find(|p| p.name.starts_with("Trending"))
        {
            Some(p) => q
                .music_playlist(&p.id)
                .await
                .map(|p| tracks(p.tracks.items))
                .unwrap_or_default(),
            None => tracks(charts.trending_tracks.clone()),
        };

        let mut sections = Vec::new();
        sections.extend(track_section(
            "Trending",
            trending.into_iter().take(10).collect(),
            QueueMode::Section,
        ));
        sections.extend(card_section(
            "New releases",
            new_albums
                .unwrap_or_default()
                .into_iter()
                .take(HOME_SHELF)
                .map(album_card)
                .collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Top artists",
            charts
                .artists
                .into_iter()
                .take(HOME_SHELF)
                .map(artist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Browse moods & genres",
            genres
                .unwrap_or_default()
                .into_iter()
                .take(12)
                .map(genre_card)
                .collect(),
            CardLayout::Chips,
        ));
        Ok(Page {
            header: None,
            sections,
            more: None,
        })
    }

    async fn explore(&self) -> Result<Page> {
        let q = self.q();
        let (genres, new_albums, new_videos) =
            tokio::join!(q.music_genres(), q.music_new_albums(), q.music_new_videos());
        let genres = genres.map_err(err)?;
        let (moods, genres): (Vec<_>, Vec<_>) = genres.into_iter().partition(|g| g.is_mood);
        let mut sections = Vec::new();
        sections.extend(card_section(
            "Moods & moments",
            moods.into_iter().map(genre_card).collect(),
            CardLayout::Chips,
        ));
        sections.extend(card_section(
            "Genres",
            genres.into_iter().map(genre_card).collect(),
            CardLayout::Chips,
        ));
        sections.extend(card_section(
            "New albums & singles",
            new_albums
                .unwrap_or_default()
                .into_iter()
                .map(album_card)
                .collect(),
            CardLayout::Grid,
        ));
        sections.extend(track_section(
            "New music videos",
            tracks(new_videos.unwrap_or_default())
                .into_iter()
                .take(HOME_SHELF)
                .collect(),
            QueueMode::Radio,
        ));
        Ok(Page {
            header: None,
            sections,
            more: None,
        })
    }

    async fn library(&self) -> Result<Page> {
        let q = self.q();
        let (playlist_cards, albums, artists) = tokio::join!(
            self.library_playlists(),
            q.music_saved_albums(),
            q.music_saved_artists(),
        );
        let playlist_cards = playlist_cards?;
        let mut sections = Vec::new();
        sections.extend(card_section("Playlists", playlist_cards, CardLayout::Grid));
        sections.extend(card_section(
            "Albums",
            albums
                .map(|p| p.items)
                .unwrap_or_default()
                .into_iter()
                .map(album_card)
                .collect(),
            CardLayout::Grid,
        ));
        sections.extend(card_section(
            "Artists",
            artists
                .map(|p| p.items)
                .unwrap_or_default()
                .into_iter()
                .map(artist_card)
                .collect(),
            CardLayout::Grid,
        ));
        Ok(Page {
            header: None,
            sections,
            more: None,
        })
    }

    /// Liked music followed by the saved playlists, for the sidebar.
    pub async fn library_playlists(&self) -> Result<Vec<Card>> {
        let q = self.q();
        let (playlists, liked) = tokio::join!(q.music_saved_playlists(), q.music_liked_tracks());
        let playlists = playlists.map_err(|e| format!("could not load your library: {e}"))?;
        let mut cards = Vec::new();
        if let Ok(liked) = &liked {
            cards.push(Card {
                kind: CardKind::Playlist,
                id: liked.id.clone(),
                title: "Liked music".into(),
                subtitle: liked.track_count.map_or_else(
                    || "Auto playlist".into(),
                    |n| format!("Auto playlist • {}", songs(n)),
                ),
                thumb: pick_thumb(&liked.thumbnail, CARD_THUMB),
                color: None,
                track: None,
            });
        }
        cards.extend(
            playlists
                .items
                .into_iter()
                .filter(|p| liked.as_ref().is_ok_and(|l| l.id != p.id) || liked.is_err())
                // Episodes for Later is a podcast page, which does not open
                // as a playlist.
                .filter(|p| p.id != "SE")
                .map(playlist_card),
        );
        Ok(cards)
    }

    async fn search(&self, query: &str) -> Result<Page> {
        let q = self.q();
        let (main, songs, albums, artists, playlists) = tokio::join!(
            q.music_search_main(query),
            q.music_search_tracks(query),
            q.music_search_albums(query),
            q.music_search_artists(query),
            q.music_search_playlists(query, false),
        );
        let songs = songs.map_err(err)?;

        let mut sections = Vec::new();
        // The top result leads, as on YouTube Music.
        match main.ok().and_then(|m| m.items.items.into_iter().next()) {
            Some(MusicItem::Artist(a)) => sections.extend(card_section(
                "Top result",
                vec![artist_card(a)],
                CardLayout::Top,
            )),
            Some(MusicItem::Album(a)) => sections.extend(card_section(
                "Top result",
                vec![album_card(a)],
                CardLayout::Top,
            )),
            Some(MusicItem::Playlist(p)) => sections.extend(card_section(
                "Top result",
                vec![playlist_card(p)],
                CardLayout::Top,
            )),
            _ => {}
        }
        sections.extend(track_section(
            "Songs",
            tracks(songs.items.items),
            QueueMode::Radio,
        ));
        sections.extend(card_section(
            "Artists",
            artists
                .map(|r| r.items.items)
                .unwrap_or_default()
                .into_iter()
                .map(artist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Albums",
            albums
                .map(|r| r.items.items)
                .unwrap_or_default()
                .into_iter()
                .map(album_card)
                .collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Playlists",
            playlists
                .map(|r| r.items.items)
                .unwrap_or_default()
                .into_iter()
                .map(playlist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        if sections.is_empty() {
            return Err(format!("No results for \u{201c}{query}\u{201d}"));
        }
        Ok(Page {
            header: None,
            sections,
            more: None,
        })
    }

    async fn album(&self, id: &str) -> Result<Page> {
        let album = self.q().music_album(id).await.map_err(err)?;
        let cover = pick_thumb(&album.cover, ROW_THUMB);
        let mut tracks = tracks(album.tracks);
        for t in &mut tracks {
            if t.thumb.is_none() {
                t.thumb.clone_from(&cover);
            }
            if t.album.is_none() {
                t.album = Some((album.id.clone(), album.name.clone()));
            }
        }
        let mut meta = Vec::new();
        if let Some(year) = album.year {
            meta.push(year.to_string());
        }
        meta.push(songs(tracks.len() as u64));
        meta.extend(total_time(&tracks));

        let header = Header {
            label: album_type(album.album_type).to_owned(),
            title: album.name,
            subtitle: join_artists(&artist_refs(&album.artists)),
            meta: meta.join(" • "),
            description: album
                .description
                .map(|d| d.to_plaintext())
                .unwrap_or_default(),
            thumb: pick_thumb(&album.cover, HEADER_THUMB),
            round: false,
            banner: false,
            tracks: tracks.clone(),
            shuffle_playlist: None,
            radio_id: album.playlist_id.map(|p| format!("RDAMPL{p}")),
        };
        let mut sections = vec![Section::Tracks {
            title: String::new(),
            tracks,
            mode: QueueMode::Section,
            numbered: true,
        }];
        sections.extend(card_section(
            "Other versions",
            album.variants.into_iter().map(album_card).collect(),
            CardLayout::Carousel,
        ));
        Ok(Page {
            header: Some(header),
            sections,
            more: None,
        })
    }

    /// A playlist's first page of tracks; `complete` fetches the rest.
    async fn playlist(&self, id: &str) -> Result<Page> {
        let playlist = self.fetch_playlist(id, false).await?;
        let tracks = tracks(playlist.tracks.items.clone());
        let more = (playlist.tracks.ctoken.is_some() && !tracks.is_empty()).then_some(MoreTracks {
            tracks: playlist.tracks,
            count: playlist.track_count,
        });
        let header = Header {
            label: "Playlist".to_owned(),
            title: playlist.name,
            subtitle: playlist.channel.map(|c| c.name).unwrap_or_default(),
            meta: playlist_meta(playlist.track_count, &tracks),
            description: playlist
                .description
                .map(|d| d.to_plaintext())
                .unwrap_or_default(),
            thumb: pick_thumb(&playlist.thumbnail, HEADER_THUMB),
            round: false,
            banner: false,
            tracks: tracks.clone(),
            shuffle_playlist: None,
            radio_id: Some(format!("RDAMPL{}", playlist.id)),
        };
        let mut sections = Vec::new();
        sections.extend(track_section("", tracks, QueueMode::Section));
        sections.extend(card_section(
            "Related playlists",
            playlist
                .related_playlists
                .items
                .into_iter()
                .map(playlist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        Ok(Page {
            header: Some(header),
            sections,
            more,
        })
    }

    /// Fetches the rest of a playlist page's tracks, up to `PLAYLIST_LIMIT`.
    /// Keeps what arrived should a later request fail.
    pub async fn complete(&self, mut page: Page) -> Page {
        let Some(MoreTracks { mut tracks, count }) = page.more.take() else {
            return page;
        };
        let shown = tracks.items.len();
        let _ = tracks.extend_limit(self.q(), PLAYLIST_LIMIT).await;
        let extra = self::tracks(tracks.items.split_off(shown));
        if let Some(Section::Tracks { tracks, .. }) = page.sections.first_mut() {
            tracks.extend_from_slice(&extra);
        }
        if let Some(header) = &mut page.header {
            header.tracks.extend(extra);
            header.meta = playlist_meta(count, &header.tracks);
        }
        page
    }

    async fn artist(&self, id: &str) -> Result<Page> {
        let artist = self.q().music_artist(id, true).await.map_err(err)?;
        let top = tracks(artist.tracks);
        let header = Header {
            label: "Artist".to_owned(),
            title: artist.name,
            subtitle: artist
                .subscriber_count
                .map(|n| format!("{} subscribers", compact(n)))
                .unwrap_or_default(),
            meta: String::new(),
            description: artist.description.unwrap_or_default(),
            // Wide banners must keep their aspect, so take the largest as is.
            thumb: artist
                .header_image
                .iter()
                .max_by_key(|t| t.width)
                .map(|t| t.url.clone()),
            round: true,
            banner: true,
            tracks: top.clone(),
            shuffle_playlist: artist.tracks_playlist_id,
            radio_id: artist.radio_id,
        };

        let (singles, albums): (Vec<_>, Vec<_>) = artist
            .albums
            .into_iter()
            .partition(|a| matches!(a.album_type, AlbumType::Single | AlbumType::Ep));
        let mut sections = Vec::new();
        sections.extend(track_section("Songs", top, QueueMode::Section));
        sections.extend(card_section(
            "Albums",
            albums.into_iter().map(album_card).collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Singles & EPs",
            singles.into_iter().map(album_card).collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Featured on",
            artist.playlists.into_iter().map(playlist_card).collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Fans might also like",
            artist
                .similar_artists
                .into_iter()
                .map(artist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        Ok(Page {
            header: Some(header),
            sections,
            more: None,
        })
    }

    async fn genre(&self, id: &str) -> Result<Page> {
        let genre = self.q().music_genre(id).await.map_err(err)?;
        let sections = genre
            .sections
            .into_iter()
            .filter_map(|s| {
                card_section(
                    &s.name,
                    s.playlists.into_iter().map(playlist_card).collect(),
                    CardLayout::Carousel,
                )
            })
            .collect();
        Ok(Page {
            header: Some(Header {
                label: "Genre".to_owned(),
                title: genre.name,
                ..Default::default()
            }),
            sections,
            more: None,
        })
    }

    /// Tracks of an album or playlist, for playing a card directly.
    pub async fn card_tracks(&self, kind: CardKind, id: &str) -> Result<Vec<Track>> {
        match kind {
            CardKind::Album => Ok(self
                .album(id)
                .await?
                .header
                .map(|h| h.tracks)
                .unwrap_or_default()),
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

    pub async fn sign_in(&self, cookie: &str) -> Result<()> {
        let cookie = cookie.trim();
        if cookie.contains('\t') {
            self.rp.user_auth_set_cookie_txt(cookie).await.map_err(err)
        } else {
            let cookie = cookie.strip_prefix("Cookie:").unwrap_or(cookie).trim();
            self.rp.user_auth_set_cookie(cookie).await.map_err(err)
        }
    }

    pub async fn sign_out(&self) {
        let _ = self.rp.user_auth_remove_cookie().await;
    }

    /// The signed-in account's profile picture, from YouTube Music's
    /// account menu.
    pub async fn account_photo(&self) -> Result<Option<String>> {
        let json = self
            .q()
            .authenticated()
            .raw(
                ClientType::DesktopMusic,
                "account/account_menu",
                &serde_json::json!({}),
            )
            .await
            .map_err(err)?;
        Ok(account_photo(&json))
    }

    /// Whether a sign-in cookie is stored, without asking YouTube.
    pub fn has_cookie(&self) -> bool {
        self.q().auth_enabled(ClientType::DesktopMusic)
    }

    /// Checks the stored cookie with YouTube, which costs a page load.
    /// `None` when YouTube could not be asked.
    pub async fn cookie_valid(&self) -> Option<bool> {
        match self.rp.user_auth_check_cookie().await {
            Ok(()) => Some(true),
            Err(rustypipe::error::Error::Auth(_)) => Some(false),
            Err(e) => {
                log::warn!("could not check the sign-in: {e}");
                None
            }
        }
    }
}

/// The largest profile picture in an account menu response.
fn account_photo(json: &str) -> Option<String> {
    let menu: serde_json::Value = serde_json::from_str(json).ok()?;
    let header = &menu["actions"][0]["openPopupAction"]["popup"]["multiPageMenuRenderer"]["header"]
        ["activeAccountHeaderRenderer"];
    header["accountPhoto"]["thumbnails"]
        .as_array()?
        .iter()
        .max_by_key(|t| t["width"].as_u64().unwrap_or(0))?["url"]
        .as_str()
        .map(str::to_owned)
}

fn genre_card(g: rustypipe::model::MusicGenreItem) -> Card {
    Card {
        kind: CardKind::Genre,
        id: g.id,
        title: g.name,
        subtitle: String::new(),
        thumb: None,
        color: Some(g.color),
        track: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Thumbnail is non-exhaustive, so build it the way rustypipe does.
    fn thumb(url: &str, width: u32) -> Thumbnail {
        serde_json::from_value(serde_json::json!({ "url": url, "width": width, "height": width }))
            .unwrap()
    }

    #[test]
    fn picks_smallest_sufficient_thumbnail() {
        let thumbs = [thumb("a", 60), thumb("b", 226), thumb("c", 544)];
        assert_eq!(pick_thumb(&thumbs, 120).as_deref(), Some("b"));
        assert_eq!(pick_thumb(&thumbs, 1000).as_deref(), Some("c"));
        assert_eq!(pick_thumb(&[], 120), None);
    }

    #[test]
    fn resizes_googleusercontent_thumbnails() {
        assert_eq!(
            resize_thumb("https://lh3.googleusercontent.com/abc=w60-h60-l90-rj", 360),
            "https://lh3.googleusercontent.com/abc=w360-h360-l90-rj"
        );
        let ytimg = "https://i.ytimg.com/vi/x/hqdefault.jpg";
        assert_eq!(resize_thumb(ytimg, 360), ytimg);
    }

    #[test]
    fn formats_numbers_and_durations() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_000), "1K");
        assert_eq!(compact(1_260_000), "1.3M");
        assert_eq!(compact(312_000), "312K");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(3725), "1:02:05");
    }

    #[test]
    fn reads_the_account_photo() {
        let menu = r#"{"actions":[{"openPopupAction":{"popup":{"multiPageMenuRenderer":{"header":
            {"activeAccountHeaderRenderer":{"accountName":{"runs":[{"text":"Alex"}]},
            "accountPhoto":{"thumbnails":[
                {"url":"https://yt3.ggpht.com/a=s88","width":88,"height":88},
                {"url":"https://yt3.ggpht.com/a=s176","width":176,"height":176}]}}}}}}}]}"#;
        assert_eq!(
            account_photo(menu).as_deref(),
            Some("https://yt3.ggpht.com/a=s176")
        );
        assert_eq!(account_photo("{}"), None);
        assert_eq!(account_photo("not json"), None);
    }

    #[test]
    fn totals_running_time() {
        let t = |d| Track {
            id: String::new(),
            title: String::new(),
            artists: Vec::new(),
            album: None,
            duration: Some(d),
            thumb: None,
            track_nr: None,
        };
        assert_eq!(
            total_time(&[t(1500), t(2401)]).as_deref(),
            Some("1 hr 6 min")
        );
        assert_eq!(total_time(&[t(61)]).as_deref(), Some("2 min"));
        assert_eq!(total_time(&[]), None);
    }
}
