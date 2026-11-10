//! Wires the window's callbacks to the app.

use std::rc::Rc;

use slint::ComponentHandle;

use super::{App, updates::up_to_date_status};
use crate::{
    Api as ApiGlobal, AppWindow,
    api::{Rating, Route},
    audio::eq,
    settings::{Accent, Look, Quality},
};

impl App {
    pub(super) fn bind(self: &Rc<Self>, ui: &AppWindow) {
        // `on!(on_x, ...)` binds a window callback, `on!(api.on_x, ...)` one
        // of the `Api` global's.
        macro_rules! on {
            ($target:ident . $setter:ident, |$app:ident $(, $arg:ident)*| $body:expr) => {{
                let weak = Rc::downgrade(self);
                $target.$setter(move |$($arg),*| {
                    if let Some($app) = weak.upgrade() {
                        $body
                    }
                });
            }};
            ($setter:ident, $($rest:tt)*) => {
                on!(ui.$setter, $($rest)*)
            };
        }

        on!(on_go, |app, nav| match nav.as_str() {
            "home" => app.navigate(Route::Home, true),
            "explore" => app.navigate(Route::Explore, true),
            "library" => app.navigate(Route::Library, true),
            "history" => app.navigate(Route::History, true),
            _ => app.reload(),
        });
        on!(on_back, |app| app.go_back());
        on!(on_forward, |app| app.go_forward());
        on!(on_search, |app, query| app.search(&query));
        on!(on_search_edited, |app, query| app
            .suggest(query.to_string()));
        on!(on_forget_search, |app, query| app.forget_search(&query));
        on!(on_toggle_play, |app| app.toggle_play());
        on!(on_next, |app| app.next(true));
        on!(on_previous, |app| app.previous());
        on!(on_seek, |app, fraction| {
            let duration = app.state.borrow().playback.duration;
            app.seek_to(fraction as f64 * duration);
        });
        on!(on_set_volume, |app, volume| app.set_volume(volume));
        on!(on_toggle_mute, |app| app.toggle_mute());
        on!(on_toggle_shuffle, |app| app.toggle_shuffle());
        on!(on_cycle_repeat, |app| app.cycle_repeat());
        on!(on_queue_clicked, |app, index, control, shift| app
            .queue_clicked(index as usize, control, shift));
        on!(on_queue_move, |app, from, to| app
            .queue_move(from as usize, to as usize));
        on!(on_queue_remove, |app, index| app
            .queue_remove(vec![index as usize]));
        on!(on_queue_remove_selected, |app| app
            .queue_remove(app.selected_rows()));
        on!(on_queue_play_next, |app, index| app
            .queue_play_next(vec![index as usize]));
        on!(on_queue_play_next_selected, |app| app
            .queue_play_next(app.selected_rows()));
        on!(on_queue_clear_selection, |app| app.clear_selection());
        on!(on_queue_clear, |app| app.queue_clear());
        on!(on_queue_save, |app| app.save_queue_as_playlist());
        on!(on_panel_tab_changed, |app, tab| {
            if tab == 0 {
                app.load_missing_queue_art();
            } else if tab == 1 {
                app.load_lyrics();
            }
        });
        on!(on_lyric_clicked, |app, index| {
            let start = app.state.borrow().lyric_starts.get(index as usize).copied();
            if let Some(start) = start {
                app.seek_to(start);
            }
        });
        on!(on_np_go_artist, |app| {
            let id = app
                .state
                .borrow()
                .queue
                .current()
                .and_then(|t| t.first_artist_id().map(str::to_owned));
            if let Some(id) = id {
                app.navigate(Route::Artist(id), true);
            }
        });
        on!(on_np_go_album, |app| {
            let id = app
                .state
                .borrow()
                .queue
                .current()
                .and_then(|t| t.album.as_ref().map(|(id, _)| id.clone()));
            if let Some(id) = id {
                app.navigate(Route::Album(id), true);
            }
        });
        on!(on_sign_in, |app, cookie| app.sign_in(cookie.to_string()));
        on!(on_sign_in_browser, |app| app.sign_in_with_google());
        on!(on_sign_out, |app| app.sign_out());
        on!(on_set_normalise, |app, value| {
            app.update_settings(|s| s.normalise = value);
            app.player.set_normalise(value);
        });
        on!(on_set_discord, |app, value| {
            app.update_settings(|s| s.discord = value);
            app.update_presence();
        });
        on!(on_sidebar_changed, |app, collapsed| app
            .update_settings(|s| s.sidebar_collapsed = collapsed));
        on!(on_set_quality, |app, index| {
            let quality = Quality::from_index(index);
            app.update_settings(|s| s.set_quality(quality));
            // Streams resolved ahead at the old quality are dropped; the
            // playing song carries on as it is.
            {
                let playback = &mut app.state.borrow_mut().playback;
                playback.prefetched.clear();
                playback.forget_queued();
            }
            app.player.clear_next();
            app.warm_premium();
        });
        on!(on_set_crossfade, |app, value| {
            app.update_settings(|s| s.crossfade = value);
            // A track queued with the old setting is queued again.
            app.state.borrow_mut().playback.forget_queued();
            app.player.clear_next();
        });
        on!(on_set_equaliser, |app, on| app
            .change_equaliser(true, |s| s.equaliser = on));
        on!(on_set_eq_preset, |app, index| {
            if let Some(&(_, gains)) = eq::PRESETS.get(index as usize) {
                app.change_equaliser(true, |s| {
                    s.equaliser = true;
                    s.equaliser_gains = gains;
                });
            }
        });
        on!(on_set_eq_band, |app, band, value, done| {
            // Whole dB steps; saved once the slider is let go.
            // Adding 0 turns the -0 just left of centre into 0.
            let db = ((value * 2.0 - 1.0) * eq::MAX_DB).round() + 0.0;
            // A drag sends every pointer move, most of them within the same dB.
            let same = app
                .state
                .borrow()
                .settings
                .equaliser_gains
                .get(band as usize)
                == Some(&db);
            if done || !same {
                app.change_equaliser(done, |s| {
                    if let Some(gain) = s.equaliser_gains.get_mut(band as usize) {
                        *gain = db;
                    }
                });
            }
        });
        on!(on_set_look, |app, index| {
            app.update_settings(|s| s.look = Look::from_index(index));
            app.apply_theme(&app.ui());
        });
        on!(on_set_accent, |app, index| {
            app.update_settings(|s| s.accent = Accent::from_index(index));
            app.apply_accent(&app.ui());
        });
        on!(on_lastfm_connect, |app| app.connect_lastfm());
        on!(on_lastfm_disconnect, |app| app.disconnect_lastfm());
        on!(on_set_youtube_history, |app, value| app
            .update_settings(|s| s.youtube_history = value));
        on!(on_set_rating_buttons, |app, value| app
            .update_settings(|s| s.rating_buttons = value));
        on!(on_set_autoplay, |app, value| app
            .update_settings(|s| s.autoplay = value));
        on!(on_set_check_updates, |app, value| {
            app.update_settings(|s| s.check_updates = value);
            app.ui().set_update_status(up_to_date_status(value).into());
            if value {
                app.check_for_update();
            }
        });
        on!(on_install_update, |app| app.install_update());
        on!(on_player_event, |app, kind, message| app
            .on_player_event(&kind, &message));
        on!(
            on_media_action,
            |app, action, value| match action.as_str() {
                "toggle" => app.toggle_play(),
                "play" => app.set_paused(false),
                "pause" => app.set_paused(true),
                "next" => app.next(true),
                "previous" => app.previous(),
                "seek-by" => app.seek_to(app.player.position() + value as f64),
                "seek-to" => app.seek_to(value as f64),
                _ => {}
            }
        );

        let api = ui.global::<ApiGlobal>();
        on!(api.on_navigate, |app, kind, id| {
            let id = id.to_string();
            if kind == "song" {
                app.play_song_card(&id);
                return;
            }
            let route = match kind.as_str() {
                "album" => Route::Album(id),
                "playlist" => Route::Playlist(id),
                "artist" => Route::Artist(id),
                "genre" => Route::Genre(id),
                _ => return,
            };
            app.navigate(route, true);
        });
        on!(api.on_play_track, |app, section, index| app
            .play_from_section(section as usize, index as usize));
        on!(api.on_track_action, |app, action, section, index| app
            .track_action(&action, section as usize, index as usize));
        on!(api.on_play_card, |app, kind, id| app.play_card(
            &kind,
            id.to_string(),
            false
        ));
        on!(api.on_card_radio, |app, kind, id| app.play_card(
            &kind,
            id.to_string(),
            true
        ));
        on!(api.on_header_action, |app, action| app
            .header_action(&action));
        on!(api.on_rate, |app, id, rating| app
            .rate(id.to_string(), Rating::from_index(rating)));
        on!(api.on_save_to_playlist, |app, id| app
            .open_playlist_picker(id.to_string()));
        on!(on_pick_playlist, |app, index| app
            .pick_playlist(index as usize));
    }
}
