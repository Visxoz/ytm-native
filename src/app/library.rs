//! Liking and disliking songs, and saving them to the account's playlists.

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

use super::App;
use crate::{
    Api as ApiGlobal, PlaylistChoice,
    api::{Rating, Route},
};

impl App {
    pub(super) fn playing_id(&self) -> Option<String> {
        self.state.borrow().queue.current().map(|t| t.id.clone())
    }

    /// Shows how the account rated `id`, the playing song, once YouTube says.
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

    /// Likes or dislikes `id`, or takes that back if the playing song
    /// already had it; `Rating::None` takes back either. Disliking the playing song skips it. The UI offers
    /// this only when signed in.
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
                self.next(true);
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

    /// Offers the account's playlists to save `id` to; only when signed in.
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

    /// Saves the song being saved to the playlist offered at `index`.
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
