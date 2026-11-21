use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use super::App;
use crate::{
    Api as ApiGlobal, PlaylistChoice, RowKind,
    api::{Header, Rating, Route},
};

impl App {
    pub(super) fn playing_id(&self) -> Option<String> {
        self.state.borrow().queue.current().map(|t| t.id.clone())
    }

    pub(super) fn load_rating(self: &Rc<Self>, id: String) {
        let generation = {
            let mut state = self.state.borrow_mut();
            state.rating_generation += 1;
            state.signed_in.then_some(state.rating_generation)
        };
        self.ui().global::<ApiGlobal>().set_playing_rating(0);
        let Some(generation) = generation else {
            return;
        };
        let api = self.api.clone();
        self.spawn(async move { api.rating(&id).await }, move |app, result| {
            if app.state.borrow().rating_generation != generation {
                return;
            }
            match result {
                Ok(rating) => app
                    .ui()
                    .global::<ApiGlobal>()
                    .set_playing_rating(rating.index()),
                Err(e) => log::debug!("could not load the song's rating: {e}"),
            }
        });
    }

    /// Rating the playing song as it already is takes that back. Disliking it
    /// skips it. The UI offers this only when signed in.
    pub(super) fn rate(self: &Rc<Self>, id: String, rating: Rating) {
        let ui = self.ui();
        let global = ui.global::<ApiGlobal>();
        let playing = self.playing_id().as_deref() == Some(id.as_str());
        let before = if playing {
            Rating::from_index(global.get_playing_rating())
        } else {
            Rating::None
        };
        let rating = if rating == before {
            Rating::None
        } else {
            rating
        };
        if playing {
            self.state.borrow_mut().rating_generation += 1;
            global.set_playing_rating(rating.index());
            if rating == Rating::Dislike {
                self.skip();
                // Nothing to skip to: the queue's end, or a repeat of it.
                if self.playing_id().as_deref() == Some(id.as_str()) {
                    self.stop();
                }
            }
        }
        let api = self.api.clone();
        let rated = id.clone();
        self.spawn(
            async move { api.rate(&rated, rating).await },
            move |app, result| match result {
                Ok(()) => {
                    // Liked music, the "LM" playlist, is shown afresh when next opened.
                    app.state
                        .borrow_mut()
                        .page_cache
                        .remove(&Route::Playlist("LM".into()));
                    app.toast(match (rating, before) {
                        (Rating::Like, _) => "Added to your liked songs",
                        (Rating::Dislike, _) => "Disliked",
                        (Rating::None, Rating::Dislike) => "Dislike removed",
                        (Rating::None, Rating::None) => "Rating removed",
                        (Rating::None, _) => "Removed from your liked songs",
                    });
                }
                Err(e) => {
                    // Undone on the player, unless it has moved on.
                    let ui = app.ui();
                    let global = ui.global::<ApiGlobal>();
                    if app.playing_id().as_deref() == Some(id.as_str())
                        && global.get_playing_rating() == rating.index()
                    {
                        global.set_playing_rating(before.index());
                    }
                    app.toast(format!("Couldn't rate the song: {e}"));
                }
            },
        );
    }

    /// Saves the page's album or playlist to the library, or subscribes to
    /// its artist, or undoes that: shown at once, and put back should YouTube
    /// refuse.
    pub(super) fn toggle_in_library(self: &Rc<Self>, subscribe: bool) {
        let ui = self.ui();
        // The last page's header stays on screen while the next loads or fails.
        if ui.get_page_loading() || !ui.get_page_error().is_empty() {
            return;
        }
        let (route, id, on, done, what, account) = {
            let mut state = self.state.borrow_mut();
            let Some(header) = &state.page.header else {
                return;
            };
            let (now, id) = if subscribe {
                (header.subscribed, &header.channel_id)
            } else {
                (header.saved, &header.save_id)
            };
            let (Some(now), Some(id)) = (now, id.clone()) else {
                return;
            };
            let (done, what) = match (subscribe, !now) {
                (false, true) => (
                    "Saved to your library".to_owned(),
                    "save it to your library",
                ),
                (false, false) => (
                    "Removed from your library".to_owned(),
                    "remove it from your library",
                ),
                (true, true) => (format!("Subscribed to {}", header.title), "subscribe"),
                (true, false) => (format!("Unsubscribed from {}", header.title), "unsubscribe"),
            };
            if !state.library_edits.insert(id.clone()) {
                return;
            }
            (state.route.clone(), id, !now, done, what, state.account)
        };
        self.show_in_library(&route, subscribe, &id, on);
        let api = self.api.clone();
        let target = id.clone();
        self.spawn(
            async move { api.set_in_library(subscribe, &target, on).await },
            move |app, result| {
                app.state.borrow_mut().library_edits.remove(&id);
                // Signed in or out meanwhile: the pages shown are another account's.
                if app.state.borrow().account != account {
                    return;
                }
                app.show_in_library(
                    &route,
                    subscribe,
                    &id,
                    if result.is_ok() { on } else { !on },
                );
                match result {
                    Ok(()) => {
                        // The Library page lists what was saved, so it loads afresh.
                        app.state.borrow_mut().page_cache.remove(&Route::Library);
                        if matches!(route, Route::Playlist(_)) {
                            app.load_library();
                        }
                        app.toast(done);
                    }
                    Err(e) => app.toast(format!("Couldn't {what}: {e}")),
                }
            },
        );
    }

    /// Shows whether the page of `route` that saves or subscribes by `id` is
    /// in the library, on screen and in its cached copy.
    fn show_in_library(&self, route: &Route, subscribe: bool, id: &str, on: bool) {
        let set = |header: Option<&mut Header>| {
            let Some(h) = header else { return false };
            let (flag, target) = if subscribe {
                (&mut h.subscribed, &h.channel_id)
            } else {
                (&mut h.saved, &h.save_id)
            };
            let ours = flag.is_some() && target.as_deref() == Some(id);
            if ours {
                *flag = Some(on);
            }
            ours
        };
        let mut state = self.state.borrow_mut();
        state.library_revision += 1;
        if let Some((cached, _)) = state.page_cache.get_mut(route) {
            set(cached.header.as_mut());
        }
        // `set` matches the ID too, as the last page stays while one loads.
        let shown = state.route == *route && set(state.page.header.as_mut());
        drop(state);
        if shown
            && let Some(i) = self.find_row(RowKind::Header)
            && let Some(mut row) = self.rows.row_data(i)
        {
            if subscribe {
                row.subscribed = on;
            } else {
                row.saved = on;
            }
            self.rows.set_row_data(i, row);
        }
    }

    pub(super) fn open_playlist_picker(self: &Rc<Self>, id: String) {
        let ui = self.ui();
        ui.set_playlist_choices(ModelRc::default());
        ui.set_picker_loading(true);
        ui.set_picker_open(true);
        self.state.borrow_mut().saving = Some((id.clone(), Vec::new()));
        let api = self.api.clone();
        let requested = id.clone();
        self.spawn(
            async move { api.playlists_for(&requested).await },
            move |app, result| {
                let ui = app.ui();
                let mut state = app.state.borrow_mut();
                let Some((_, offered)) = state.saving.as_mut().filter(|(saving, _)| *saving == id)
                else {
                    return;
                };
                match result {
                    Ok(playlists) => {
                        let choices: Vec<PlaylistChoice> = playlists
                            .iter()
                            .map(|p| PlaylistChoice {
                                title: p.title.as_str().into(),
                                contains: p.contains,
                            })
                            .collect();
                        *offered = playlists;
                        ui.set_playlist_choices(ModelRc::new(VecModel::from(choices)));
                        ui.set_picker_loading(false);
                    }
                    Err(e) => {
                        state.saving = None;
                        drop(state);
                        ui.set_picker_open(false);
                        app.toast(format!("Couldn't load your playlists: {e}"));
                    }
                }
            },
        );
    }

    pub(super) fn pick_playlist(self: &Rc<Self>, index: usize) {
        self.ui().set_picker_open(false);
        let Some((id, offered)) = self.state.borrow_mut().saving.take() else {
            return;
        };
        let Some(playlist) = offered.into_iter().nth(index) else {
            return;
        };
        if playlist.contains {
            self.toast(format!("Already in \u{201c}{}\u{201d}", playlist.title));
            return;
        }
        let api = self.api.clone();
        let target = playlist.id.clone();
        self.spawn(
            async move { api.add_to_playlist(&target, &[id]).await },
            move |app, result| match result {
                Ok(()) => {
                    // Shown afresh, with the song, when next opened.
                    app.state
                        .borrow_mut()
                        .page_cache
                        .remove(&Route::Playlist(playlist.id));
                    app.toast(format!("Saved to \u{201c}{}\u{201d}", playlist.title));
                }
                Err(e) => app.toast(format!(
                    "Couldn't save to \u{201c}{}\u{201d}: {e}",
                    playlist.title
                )),
            },
        );
    }
}
