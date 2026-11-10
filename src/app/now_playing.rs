//! What shows the playing track: the player bar, the queue and lyrics
//! panel, the media overlay and Discord.

use std::{rc::Rc, time::Duration};

use slint::{ComponentHandle, Model, ModelRc, SharedString, TimerMode, VecModel};

use super::{App, TICK, s};
use crate::{
    Api as ApiGlobal, AppWindow, QueueItem,
    api::{self, Track},
    images, presence,
    streams::ResolvedStream,
};

/// The player-bar badge text. Premium formats show YouTube's nominal 256
/// kbps; others show the stream's bitrate.
fn quality_label(stream: &ResolvedStream) -> String {
    let codec = stream.codec.to_uppercase();
    match (stream.itag, stream.bitrate_kbps) {
        (774 | 141, _) => format!("{codec} 256"),
        (_, Some(kbps)) => format!("{codec} {}", kbps.round()),
        (_, None) => codec,
    }
}

/// The queue row for `track`, and the artwork it still has to load.
pub(super) fn queue_item(
    track: &Track,
    current: bool,
    recommended: bool,
    selected: bool,
) -> (QueueItem, Option<String>) {
    let art = track
        .thumb
        .as_deref()
        .and_then(|u| images::cached(u, images::ROW));
    let missing = if art.is_none() {
        track.thumb.clone()
    } else {
        None
    };
    let item = QueueItem {
        id: s(&track.id),
        title: s(&track.title),
        artists: s(track.artist_names()),
        duration: s(track.duration.map(api::format_duration).unwrap_or_default()),
        art: art.unwrap_or_default(),
        current,
        recommended,
        selected,
    };
    (item, missing)
}

impl App {
    /// Shows `track` as the one playing: the player bar, its artwork, the
    /// queue, the media overlay, Discord, the lyrics and the saved session.
    pub(super) fn present(self: &Rc<Self>, track: &Track, position: f64) {
        let ui = self.ui();
        // Not a song shown again, as on repeat or a retry.
        let new_song = ui.global::<ApiGlobal>().get_playing_id() != track.id.as_str();
        self.show_track_details(&ui, track, position);
        ui.set_np_buffered(0.0);
        self.show_playing(true);
        ui.set_np_art(images::cached_or_empty(track.thumb.as_deref(), images::ROW));
        self.load_player_art(track);

        if let Some(media) = self.media.borrow_mut().as_mut() {
            let large = track.large_thumb();
            media.set_track(
                &track.title,
                &track.artist_names(),
                track.album_name(),
                large.as_deref(),
                track.seconds(),
            );
        }
        self.refresh_queue();
        self.persist_soon();
        self.update_presence();
        if new_song {
            self.load_rating(track.id.clone());
            let mut state = self.state.borrow_mut();
            state.lyrics_for = None;
            state.lyric_starts.clear();
        }
        if ui.get_panel_open() && ui.get_panel_tab() == 1 {
            self.load_lyrics();
        }
        // The same song again keeps its lyrics; light the line it starts at.
        self.show_lyric_line(position);
    }

    /// Loads `track`'s artwork into the player bar, with the colours taken
    /// from it, unless another track is playing by then.
    pub(super) fn load_player_art(self: &Rc<Self>, track: &Track) {
        let id = track.id.clone();
        let url = track.thumb.clone();
        self.load_image(url.clone(), images::PLAYER, move |app, image| {
            if !app
                .state
                .borrow()
                .queue
                .current()
                .is_some_and(|t| t.id == id)
            {
                return;
            }
            let ui = app.ui();
            ui.set_np_art(image);
            if let Some(color) = url
                .as_deref()
                .and_then(|url| images::accent(url, images::PLAYER))
            {
                ui.set_np_accent(color);
                app.set_artwork_accent(images::vivid(color));
            }
        });
        // Without artwork, the last song's colour would linger.
        if track.thumb.is_none() {
            self.set_artwork_accent(None);
        }
    }

    /// Puts `track` on the player bar at `position`, with the length the
    /// playback has for it.
    pub(super) fn show_track_details(&self, ui: &AppWindow, track: &Track, position: f64) {
        ui.set_has_track(true);
        ui.set_np_title(s(&track.title));
        ui.set_np_artists(s(track.artist_names()));
        ui.set_np_has_artist(track.first_artist_id().is_some());
        ui.set_np_has_album(track.album.is_some());
        ui.set_np_duration(self.state.borrow().playback.duration as f32);
        ui.set_np_position(position as f32);
        ui.global::<ApiGlobal>().set_playing_id(s(&track.id));
    }

    /// Shows the stream's quality, and fills in a length the track lacked.
    pub(super) fn show_stream(&self, stream: &ResolvedStream) {
        let ui = self.ui();
        ui.set_np_premium(stream.premium);
        ui.set_np_quality(quality_label(stream).into());
        if let Some(duration) = stream.duration {
            let mut state = self.state.borrow_mut();
            if state.playback.duration <= 0.0 {
                state.playback.duration = duration;
                drop(state);
                ui.set_np_duration(duration as f32);
            }
        }
        self.state.borrow_mut().playback.current_premium = stream.premium;
    }

    pub(super) fn update_media_state(&self) {
        self.update_media_state_at(self.player.position());
    }

    fn update_media_state_at(&self, position: f64) {
        let playing = if self.ui().get_has_track() {
            Some(!self.player.is_paused())
        } else {
            None
        };
        if let Some(media) = self.media.borrow_mut().as_mut() {
            media.set_state(playing, position);
        }
    }

    pub(super) fn update_presence(&self) {
        self.update_presence_at(self.player.position());
    }

    /// Tells the media overlay and Discord whether and where it is playing.
    pub(super) fn report_playback(&self) {
        self.report_playback_at(self.player.position());
    }

    /// Reports `position` rather than the player's, which still reports the
    /// old one until the engine has carried out a seek.
    pub(super) fn report_playback_at(&self, position: f64) {
        self.update_media_state_at(position);
        self.update_presence_at(position);
    }

    fn update_presence_at(&self, position: f64) {
        let state = self.state.borrow();
        let track = match state.queue.current() {
            Some(track) if state.settings.discord && self.ui().get_has_track() => {
                Some(presence::Track {
                    video_id: track.id.clone(),
                    title: track.title.clone(),
                    artist: track.artist_names(),
                    album: track.album.as_ref().map(|(_, name)| name.clone()),
                    artwork_url: track.large_thumb(),
                    duration: state.playback.duration,
                    position: position.min(state.playback.duration.max(0.0)),
                    paused: self.player.is_paused() || !state.playback.started,
                })
            }
            _ => None,
        };
        self.presence.update(track);
    }

    pub(super) fn refresh_queue(self: &Rc<Self>) {
        let (revision, current) = {
            let state = self.state.borrow();
            (state.queue.revision(), state.queue.current_index())
        };
        let current = current.map_or(-1, |i| i as i32);
        let ui = self.ui();
        let previous = ui.get_queue_current();
        ui.set_queue_current(current);
        if self.queue_shown.get() == Some(revision) {
            // Same entries: only the highlight moves.
            for (index, is_current) in [(previous, false), (current, true)] {
                if let Ok(i) = usize::try_from(index)
                    && let Some(mut item) = self.queue_model.row_data(i)
                    && item.current != is_current
                {
                    item.current = is_current;
                    self.queue_model.set_row_data(i, item);
                }
            }
            return;
        }
        self.queue_shown.set(Some(revision));

        let mut missing = Vec::new();
        let items: Vec<QueueItem> = self
            .state
            .borrow()
            .queue
            .entries()
            .enumerate()
            .map(|(i, (t, recommended))| {
                let (item, art) = queue_item(t, i as i32 == current, recommended, false);
                missing.extend(art.map(|url| (i, url)));
                item
            })
            .collect();
        self.queue_model.set_vec(items);
        self.forget_selection();
        for (i, url) in missing {
            self.load_queue_art(i, url);
        }
    }

    /// Shows the queue entries at `rows`, whose tracks were replaced in
    /// place, without rebuilding the rest.
    pub(super) fn refresh_queue_rows(self: &Rc<Self>, rows: &[usize]) {
        let revision = self.state.borrow().queue.revision();
        if self.queue_shown.get() != Some(revision) {
            self.refresh_queue();
            return;
        }
        let current = self.ui().get_queue_current();
        let mut missing = Vec::new();
        {
            let state = self.state.borrow();
            for &i in rows {
                let Some(track) = state.queue.get(i) else {
                    continue;
                };
                let selected = self
                    .queue_model
                    .row_data(i)
                    .is_some_and(|item| item.selected);
                let (item, art) = queue_item(
                    track,
                    i as i32 == current,
                    state.queue.is_recommended(i),
                    selected,
                );
                missing.extend(art.map(|url| (i, url)));
                self.queue_model.set_row_data(i, item);
            }
        }
        for (i, url) in missing {
            self.load_queue_art(i, url);
        }
    }

    /// Loads the artwork of the entry at `index` into its row. The entry may
    /// have moved meanwhile, or been replaced in place; the row that still
    /// wants this artwork gets it.
    pub(super) fn load_queue_art(self: &Rc<Self>, index: usize, url: String) {
        // Rows nobody can see aren't worth a download; `load_missing_queue_art`
        // catches up when Up next is shown.
        let ui = self.ui();
        if !(ui.get_panel_open() && ui.get_panel_tab() == 0) {
            return;
        }
        let expected = url.clone();
        self.load_image(Some(url), images::ROW, move |app, image| {
            let row = {
                let state = app.state.borrow();
                let wants = |i: usize| {
                    state.queue.get(i).and_then(|t| t.thumb.as_deref()) == Some(expected.as_str())
                        && app
                            .queue_model
                            .row_data(i)
                            .is_some_and(|item| item.art.size().width == 0)
                };
                if wants(index) {
                    Some(index)
                } else {
                    (0..state.queue.len()).find(|&i| wants(i))
                }
            };
            if let Some(row) = row
                && let Some(mut item) = app.queue_model.row_data(row)
            {
                item.art = image;
                app.queue_model.set_row_data(row, item);
            }
        });
    }

    /// Loads the artwork of every row still without it, from the current
    /// entry onwards, so the rows near what is playing are fetched first.
    pub(super) fn load_missing_queue_art(self: &Rc<Self>) {
        let len = self.queue_model.row_count();
        let current = usize::try_from(self.ui().get_queue_current()).map_or(0, |i| i.min(len));
        let missing: Vec<(usize, String)> = {
            let state = self.state.borrow();
            (current..len)
                .chain(0..current)
                .filter(|&i| {
                    self.queue_model
                        .row_data(i)
                        .is_some_and(|item| item.art.size().width == 0)
                })
                .filter_map(|i| Some((i, state.queue.get(i)?.thumb.clone()?)))
                .collect()
        };
        for (i, url) in missing {
            self.load_queue_art(i, url);
        }
    }

    pub(super) fn load_lyrics(self: &Rc<Self>) {
        let Some(id) = self.state.borrow().queue.current().map(|t| t.id.clone()) else {
            self.show_lyrics(api::Lyrics::plain(
                "Play something to see its lyrics.",
                String::new(),
            ));
            return;
        };
        if self.state.borrow().lyrics_for.as_deref() == Some(id.as_str()) {
            return;
        }
        self.state.borrow_mut().lyrics_for = Some(id.clone());
        let ui = self.ui();
        ui.set_lyrics_loading(true);
        let api = self.api.clone();
        let requested = id.clone();
        self.spawn(
            async move { api.lyrics(&requested).await },
            move |app, result| {
                if app.state.borrow().lyrics_for.as_deref() != Some(id.as_str()) {
                    return;
                }
                let lyrics = match result {
                    Ok(Some(lyrics)) => lyrics,
                    Ok(None) => {
                        api::Lyrics::plain("Lyrics aren't available for this song.", String::new())
                    }
                    Err(e) => {
                        app.state.borrow_mut().lyrics_for = None;
                        api::Lyrics::plain(&format!("Couldn't load lyrics: {e}"), String::new())
                    }
                };
                app.show_lyrics(lyrics);
                app.ui().set_lyrics_loading(false);
            },
        );
    }

    fn show_lyrics(self: &Rc<Self>, lyrics: api::Lyrics) {
        let ui = self.ui();
        let lines: Vec<SharedString> = lyrics.lines.into_iter().map(Into::into).collect();
        ui.set_lyrics(ModelRc::new(VecModel::from(lines)));
        ui.set_lyrics_synced(!lyrics.starts.is_empty());
        ui.set_lyrics_source(lyrics.source.into());
        ui.set_lyrics_current(-1);
        self.state.borrow_mut().lyric_starts = lyrics.starts;
        // The bar's position, which is the restored one before playing.
        self.show_lyric_line(f64::from(ui.get_np_position()));
    }

    /// Lights up the lyric line being sung at `position`, when synced lyrics
    /// are on screen. A line starting before the next tick is lit by a timer,
    /// so it lands on the beat.
    pub(super) fn show_lyric_line(self: &Rc<Self>, position: f64) {
        let ui = self.ui();
        if !(ui.get_panel_open() && ui.get_panel_tab() == 1) {
            return;
        }
        let (line, next) = {
            let starts = &self.state.borrow().lyric_starts;
            if starts.is_empty() {
                return;
            }
            let line = starts.partition_point(|&start| start <= position);
            (
                line as i32 - 1,
                starts.get(line).map(|start| start - position),
            )
        };
        ui.set_lyrics_current(line);
        if let Some(wait) = next.filter(|&wait| wait < TICK.as_secs_f64())
            && !self.player.is_paused()
            // Until a track has started, the engine's position is not the bar's.
            && self.state.borrow().playback.started
        {
            let weak = Rc::downgrade(self);
            self.lyric_timer.start(
                TimerMode::SingleShot,
                Duration::from_secs_f64(wait),
                move || {
                    if let Some(app) = weak.upgrade() {
                        app.show_lyric_line(app.player.position());
                    }
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_premium_formats_with_their_nominal_bitrate() {
        let stream = |itag, kbps| ResolvedStream {
            url: String::new(),
            size: 0,
            mime: String::new(),
            headers: Vec::new(),
            codec: "Opus".into(),
            bitrate_kbps: Some(kbps),
            duration: None,
            loudness_db: None,
            premium: itag == 774,
            itag,
        };
        assert_eq!(quality_label(&stream(774, 284.6)), "OPUS 256");
        assert_eq!(quality_label(&stream(251, 146.4)), "OPUS 146");
    }
}
