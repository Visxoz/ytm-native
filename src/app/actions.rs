use std::{collections::HashSet, rc::Rc};

use super::App;
use crate::api::{CardKind, Page, QueueMode, Route, Section, Track};

fn card_kind(kind: &str) -> Option<CardKind> {
    match kind {
        "album" => Some(CardKind::Album),
        "playlist" => Some(CardKind::Playlist),
        "artist" => Some(CardKind::Artist),
        _ => None,
    }
}

impl App {
    fn section_track(&self, section: usize, index: usize) -> Option<Track> {
        match self.state.borrow().page.sections.get(section)? {
            Section::Tracks { tracks, .. } => tracks.get(index).cloned(),
            Section::Cards { .. } => None,
        }
    }

    pub(super) fn play_from_section(self: &Rc<Self>, section: usize, index: usize) {
        // Only a section played whole is copied; a radio seed is one track.
        let (track, whole) = {
            let state = self.state.borrow();
            let Some(Section::Tracks { tracks, mode, .. }) = state.page.sections.get(section)
            else {
                return;
            };
            let Some(track) = tracks.get(index) else {
                return;
            };
            match mode {
                QueueMode::Section => (track.clone(), Some(tracks.clone())),
                QueueMode::Radio => (track.clone(), None),
            }
        };
        match whole {
            Some(tracks) => self.play_queue(tracks, Some(index)),
            None => self.play_with_radio(track),
        }
    }

    fn play_with_radio(self: &Rc<Self>, track: Track) {
        let seed = track.id.clone();
        let queue_generation = {
            let mut state = self.state.borrow_mut();
            // Shuffled, if on, so the radio is shuffled in as it arrives.
            let shuffle = state.settings.shuffle;
            state.set_queue(vec![track], 0, shuffle);
            // Autoplay would otherwise fetch the same radio once this starts.
            state.playback.extending = true;
            state.queue_generation
        };
        self.start_current();
        let api = self.api.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            move |app, result| {
                app.state.borrow_mut().playback.extending = false;
                match result {
                    Ok(tracks) => {
                        if app.state.borrow().queue_generation == queue_generation {
                            app.append_unique(tracks);
                        }
                    }
                    Err(e) => {
                        log::warn!("radio failed: {e}");
                        app.extend_with_radio_if_ending();
                    }
                }
            },
        );
    }

    /// For radios, which may repeat what is queued.
    pub(super) fn append_unique(self: &Rc<Self>, tracks: Vec<Track>) {
        let fresh: Vec<Track> = {
            let state = self.state.borrow();
            let known: HashSet<&str> = state.queue.items().map(|t| t.id.as_str()).collect();
            tracks
                .into_iter()
                .filter(|t| !known.contains(t.id.as_str()))
                .collect()
        };
        self.append(fresh);
    }

    fn append(self: &Rc<Self>, tracks: Vec<Track>) {
        if tracks.is_empty() {
            return;
        }
        self.state.borrow_mut().queue.extend(tracks);
        self.refresh_queue();
        // Shuffled in, one may now be next.
        self.drop_stale_next();
        self.prefetch_next();
    }

    /// A Shuffle button turns shuffle on; Play leaves it, and Smart Shuffle,
    /// as they are.
    fn play_tracks(self: &Rc<Self>, tracks: Vec<Track>, shuffle: bool) {
        if tracks.is_empty() {
            self.toast("Nothing to play here");
            return;
        }
        // Saving the settings writes to disk, so only for a change.
        if shuffle && !self.state.borrow().settings.shuffle {
            self.update_settings(|s| s.set_shuffle(true));
        }
        self.play_queue(tracks, None);
    }

    /// A `start` of `None` starts from a random track if shuffling, else the first.
    fn play_queue(self: &Rc<Self>, tracks: Vec<Track>, start: Option<usize>) {
        {
            let mut state = self.state.borrow_mut();
            let shuffle = state.settings.shuffle;
            let start = match start {
                Some(start) => start,
                None if shuffle => state.queue.random_below(tracks.len()),
                None => 0,
            };
            state.set_queue(tracks, start, shuffle);
        }
        self.start_current();
    }

    fn play_radio(self: &Rc<Self>, radio_id: String) {
        let api = self.api.clone();
        self.spawn(
            async move { api.radio(&radio_id).await },
            |app, result| match result {
                Ok(tracks) => app.play_tracks(tracks, false),
                Err(e) => app.toast(format!("Couldn't start the radio: {e}")),
            },
        );
    }

    pub(super) fn header_action(self: &Rc<Self>, action: &str) {
        let (tracks, shuffle_playlist, unloaded, radio_id) = {
            let state = self.state.borrow();
            let Some(header) = &state.page.header else {
                return;
            };
            // A long playlist whose rest is still loading.
            let unloaded = match (&state.route, &state.page.more) {
                (Route::Playlist(id), Some(_)) => Some(id.clone()),
                _ => None,
            };
            (
                state.page.header_tracks().to_vec(),
                header.shuffle_playlist.clone(),
                unloaded,
                header.radio_id.clone(),
            )
        };
        match action {
            // Played from what has loaded, with the rest queued as it arrives.
            "play" | "shuffle" if unloaded.is_some() => {
                if action == "shuffle" && !self.state.borrow().settings.shuffle {
                    self.update_settings(|s| s.set_shuffle(true));
                }
                let (route, page) = {
                    let state = self.state.borrow();
                    (state.route.clone(), state.page.clone())
                };
                self.play_page(route, page, false);
            }
            "play" => self.play_tracks(tracks, false),
            "shuffle" => self.with_all_tracks(tracks, shuffle_playlist, |app, tracks| {
                app.play_tracks(tracks, true);
            }),
            "download" => {
                self.with_all_tracks(tracks, unloaded, |app, tracks| app.download(tracks))
            }
            "radio" => {
                if let Some(radio) = radio_id {
                    self.play_radio(radio);
                }
            }
            _ => {}
        }
    }

    /// Runs `then` with the page's tracks, or the whole playlist when the
    /// page shows only part of it.
    fn with_all_tracks(
        self: &Rc<Self>,
        shown: Vec<Track>,
        playlist: Option<String>,
        then: impl FnOnce(&Rc<Self>, Vec<Track>) + 'static,
    ) {
        let Some(playlist) = playlist else {
            then(self, shown);
            return;
        };
        let api = self.api.clone();
        self.spawn(
            async move { api.playlist_tracks(&playlist).await },
            move |app, result| match result {
                Ok(found) if !found.is_empty() => then(app, found),
                _ => then(app, shown),
            },
        );
    }

    fn song_card(&self, id: &str) -> Option<Track> {
        self.state
            .borrow()
            .page
            .sections
            .iter()
            .find_map(|section| match section {
                Section::Cards { cards, .. } => cards
                    .iter()
                    .find(|c| c.id == id)
                    .and_then(|c| c.track.clone()),
                Section::Tracks { .. } => None,
            })
    }

    pub(super) fn play_song_card(self: &Rc<Self>, id: &str) {
        if let Some(track) = self.song_card(id) {
            self.play_with_radio(track);
        }
    }

    pub(super) fn play_card(self: &Rc<Self>, kind: &str, id: String, radio: bool) {
        if kind == "song" {
            self.play_song_card(&id);
            return;
        }
        let Some(kind) = card_kind(kind) else {
            return;
        };
        if radio && kind == CardKind::Playlist {
            self.play_radio(format!("RDAMPL{id}"));
            return;
        }
        let route = match kind {
            CardKind::Album => Route::Album(id),
            CardKind::Playlist => Route::Playlist(id),
            _ => {
                let api = self.api.clone();
                self.spawn(
                    async move { api.card_tracks(kind, &id).await },
                    move |app, result| match result {
                        Ok(tracks) => app.play_card_tracks(tracks, radio),
                        Err(e) => app.toast(format!("Couldn't load: {e}")),
                    },
                );
                return;
            }
        };
        // An expired copy may be missing songs added since.
        if let Some((page, true, _)) = self.cached_page(&route) {
            self.play_page(route, page, radio);
            return;
        }
        let api = self.api.clone();
        let (signed_in, account) = {
            let state = self.state.borrow();
            (state.signed_in, state.account)
        };
        let asked = route.clone();
        self.spawn(
            async move { api.page(&asked, signed_in).await },
            move |app, result| match result {
                Ok(page) => {
                    // So opening it next shows it at once.
                    if app.state.borrow().account == account {
                        app.cache_page(route.clone(), page.clone());
                    }
                    app.play_page(route, page, radio);
                }
                Err(e) => app.toast(format!("Couldn't load: {e}")),
            },
        );
    }

    /// Plays `tracks`, or with `radio` the first one's radio.
    fn play_card_tracks(self: &Rc<Self>, tracks: Vec<Track>, radio: bool) {
        if !radio {
            self.play_tracks(tracks, false);
            return;
        }
        match tracks.into_iter().next() {
            Some(first) => self.play_with_radio(first),
            None => self.toast("Nothing to play here"),
        }
    }

    /// Plays an album or playlist page's tracks, or its first one's radio, at
    /// once, and queues the rest of a long playlist as it arrives.
    fn play_page(self: &Rc<Self>, route: Route, mut page: Page, radio: bool) {
        let more = page.more.is_some() && !radio;
        let tracks = match page.sections.first_mut() {
            // Taken rather than copied when the page is done with.
            Some(Section::Tracks { tracks, .. }) if !more => std::mem::take(tracks),
            Some(Section::Tracks { tracks, .. }) => tracks.clone(),
            Some(Section::Cards { .. }) | None => Vec::new(),
        };
        let shown = tracks.len();
        self.play_card_tracks(tracks, radio);
        if !more || shown == 0 {
            return;
        }
        // Read once the queue is set, so it is this queue's.
        let (queue_generation, account) = {
            let state = self.state.borrow();
            (state.queue_generation, state.account)
        };
        let api = self.api.clone();
        self.spawn(
            async move { api.complete(page).await },
            move |app, (mut page, whole)| {
                let keep = whole && app.state.borrow().account == account;
                if keep && let Some((cached, _)) = app.state.borrow_mut().page_cache.get_mut(&route)
                {
                    *cached = page.clone();
                }
                if app.state.borrow().queue_generation != queue_generation {
                    return;
                }
                // Not deduplicated, as a playlist may list a song twice.
                if let Some(Section::Tracks { tracks, .. }) = page.sections.first_mut()
                    && tracks.len() > shown
                {
                    app.append(tracks.split_off(shown));
                }
            },
        );
    }

    pub(super) fn download_card(self: &Rc<Self>, kind: &str, id: String) {
        if kind == "song" {
            if let Some(track) = self.song_card(&id) {
                self.download(vec![track]);
            }
            return;
        }
        let Some(kind) = card_kind(kind) else {
            return;
        };
        let api = self.api.clone();
        self.spawn(
            async move { api.card_tracks(kind, &id).await },
            |app, result| match result {
                Ok(tracks) => app.download(tracks),
                Err(e) => app.toast(format!("Couldn't load: {e}")),
            },
        );
    }

    /// A song in the queue, from its row or the player bar.
    pub(super) fn download_queued(self: &Rc<Self>, id: &str, remove: bool) {
        if remove {
            self.remove_download(id);
            return;
        }
        let track = self
            .state
            .borrow()
            .queue
            .items()
            .find(|t| t.id == id)
            .cloned();
        if let Some(track) = track {
            self.download(vec![track]);
        }
    }

    pub(super) fn track_action(self: &Rc<Self>, action: &str, section: usize, index: usize) {
        let Some(track) = self.section_track(section, index) else {
            return;
        };
        match action {
            "next" | "queue" => {
                let empty = self.state.borrow().queue.is_empty();
                let at = {
                    let mut state = self.state.borrow_mut();
                    if action == "next" {
                        state.queue.play_next(track.clone());
                        state.queue.current_index().map_or(0, |c| c + 1)
                    } else {
                        state.queue.push(track.clone());
                        state.queue.len() - 1
                    }
                };
                if empty {
                    self.start_current();
                } else {
                    self.queue_inserted(at);
                    self.toast(if action == "next" {
                        format!("\u{201c}{}\u{201d} plays next", track.title)
                    } else {
                        format!("Added \u{201c}{}\u{201d} to the queue", track.title)
                    });
                }
            }
            "radio" => self.play_with_radio(track),
            "download" => self.download(vec![track]),
            "remove-download" => self.remove_download(&track.id),
            "artist" => {
                if let Some(id) = track.first_artist_id() {
                    self.navigate(Route::Artist(id.to_owned()), true);
                }
            }
            "album" => {
                if let Some(id) = track.album_id() {
                    self.navigate(Route::Album(id.to_owned()), true);
                }
            }
            _ => {}
        }
    }
}
