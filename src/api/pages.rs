//! The pages behind each route.

use rustypipe::model::{AlbumType, MusicItem, richtext::ToPlaintext};

use super::{
    Api, Card, CardKind, CardLayout, Header, MoreTracks, PLAYLIST_LIMIT, Page, QueueMode, Result,
    Route, Section,
    convert::{
        CARD_THUMB, HEADER_THUMB, ROW_THUMB, album_card, album_type, artist_card, artist_refs,
        card_section, genre_card, join_artists, pick_thumb, playlist_card, track_section, tracks,
    },
    err,
    format::{compact, playlist_meta, songs, total_time},
};

const HOME_SHELF: usize = 20;

impl Api {
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
            let (feed, genres) = tokio::join!(super::home_feed::fetch(&q), q.music_genres());
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

    pub(super) async fn album(&self, id: &str) -> Result<Page> {
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
        let extra = super::convert::tracks(tracks.items.split_off(shown));
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
}
