//! Starting playback from the page: tracks, cards, header buttons and
//! track menus.

use std::{collections::HashSet, rc::Rc};

use super::App;
use crate::api::{CardKind, QueueMode, Route, Section, Track};

impl App {
    /// One track of the page.
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
            Some(tracks) => {
                let mut state = self.state.borrow_mut();
                let shuffle = state.settings.shuffle;
                state.set_queue(tracks, index, shuffle, true);
                drop(state);
                self.start_current();
            }
            None => self.play_with_radio(track),
        }
    }

    /// Plays `track` now and queues a radio based on it, like YouTube Music.
    fn play_with_radio(self: &Rc<Self>, track: Track) {
        let id = track.id.clone();
        {
            let mut state = self.state.borrow_mut();
            state.set_queue(vec![track], 0, false, false);
            // Autoplay would otherwise fetch the same radio once this starts.
            state.playback.extending = true;
        }
        self.start_current();
        let api = self.api.clone();
        let seed = id.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            move |app, result| {
                app.state.borrow_mut().playback.extending = false;
                match result {
                    Ok(tracks) => {
                        let still_first =
                            app.state.borrow().queue.get(0).is_some_and(|t| t.id == id);
                        if still_first {
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

    pub(super) fn append_unique(self: &Rc<Self>, tracks: Vec<Track>) {
        {
            let mut state = self.state.borrow_mut();
            let fresh: Vec<Track> = {
                let known: HashSet<&str> = state.queue.items().map(|t| t.id.as_str()).collect();
                tracks
                    .into_iter()
                    .filter(|t| !known.contains(t.id.as_str()))
                    .collect()
            };
            let shuffle = state.settings.shuffle;
            state.queue.extend(fresh);
            // Keep newly added tracks in the shuffle. Recommendations are
            // taken out first, as they are placed again where they are due.
            if shuffle {
                state.queue.remove_recommended();
                state.queue.set_shuffle(false);
                state.queue.set_shuffle(true);
            }
        }
        self.refresh_queue();
        self.prefetch_next();
    }

    fn play_tracks(self: &Rc<Self>, tracks: Vec<Track>, shuffle: bool) {
        if tracks.is_empty() {
            self.toast("Nothing to play here");
            return;
        }
        let start = if shuffle {
            self.state.borrow_mut().queue.random_below(tracks.len())
        } else {
            0
        };
        self.update_settings(|s| s.set_shuffle(shuffle));
        self.state
            .borrow_mut()
            .set_queue(tracks, start, shuffle, true);
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
        let (tracks, shuffle_playlist, radio_id) = {
            let state = self.state.borrow();
            let Some(header) = &state.page.header else {
                return;
            };
            (
                state.page.header_tracks().to_vec(),
                header.shuffle_playlist.clone(),
                header.radio_id.clone(),
            )
        };
        match action {
            "play" => self.play_tracks(tracks, false),
            "shuffle" => match shuffle_playlist {
                Some(playlist) => {
                    let api = self.api.clone();
                    self.spawn(
                        async move { api.playlist_tracks(&playlist).await },
                        move |app, result| match result {
                            Ok(found) if !found.is_empty() => app.play_tracks(found, true),
                            _ => app.play_tracks(tracks, true),
                        },
                    );
                }
                None => self.play_tracks(tracks, true),
            },
            "radio" => {
                if let Some(radio) = radio_id {
                    self.play_radio(radio);
                }
            }
            _ => {}
        }
    }

    /// Plays a song card from the current page, with a radio after it.
    pub(super) fn play_song_card(self: &Rc<Self>, id: &str) {
        let track = self
            .state
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
            });
        if let Some(track) = track {
            self.play_with_radio(track);
        }
    }

    pub(super) fn play_card(self: &Rc<Self>, kind: &str, id: String, radio: bool) {
        if kind == "song" {
            self.play_song_card(&id);
            return;
        }
        let kind = match kind {
            "album" => CardKind::Album,
            "playlist" => CardKind::Playlist,
            "artist" => CardKind::Artist,
            _ => return,
        };
        if radio && kind == CardKind::Playlist {
            self.play_radio(format!("RDAMPL{id}"));
            return;
        }
        let api = self.api.clone();
        self.spawn(
            async move { api.card_tracks(kind, &id).await },
            move |app, result| match result {
                Ok(tracks) if radio => match tracks.into_iter().next() {
                    Some(first) => app.play_with_radio(first),
                    None => app.toast("Nothing to play here"),
                },
                Ok(tracks) => app.play_tracks(tracks, false),
                Err(e) => app.toast(format!("Couldn't load: {e}")),
            },
        );
    }

    pub(super) fn track_action(self: &Rc<Self>, action: &str, section: usize, index: usize) {
        let Some(track) = self.section_track(section, index) else {
            return;
        };
        match action {
            "next" | "queue" => {
                let empty = self.state.borrow().queue.is_empty();
                {
                    let mut state = self.state.borrow_mut();
                    if action == "next" {
                        state.queue.play_next(track.clone());
                    } else {
                        state.queue.push(track.clone());
                    }
                }
                if empty {
                    self.start_current();
                } else {
                    self.refresh_queue();
                    self.toast(if action == "next" {
                        format!("\u{201c}{}\u{201d} plays next", track.title)
                    } else {
                        format!("Added \u{201c}{}\u{201d} to the queue", track.title)
                    });
                }
            }
            "radio" => self.play_with_radio(track),
            "artist" => {
                if let Some(id) = track.first_artist_id() {
                    self.navigate(Route::Artist(id.to_owned()), true);
                }
            }
            "album" => {
                if let Some((id, _)) = track.album {
                    self.navigate(Route::Album(id), true);
                }
            }
            _ => {}
        }
    }
}
