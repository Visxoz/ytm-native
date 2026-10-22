//! What shows the playing track: the player bar, the queue and lyrics
//! panel, the media overlay and Discord.

use std::rc::Rc;

use slint::{ComponentHandle, Model};

use super::{App, s};
use crate::{
    Api as ApiGlobal, QueueItem,
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

/// The queue row for `track`, and whether its artwork still has to load.
fn queue_item(track: &Track, current: bool) -> (QueueItem, bool) {
    let art = track
        .thumb
        .as_deref()
        .and_then(|u| images::cached(u, images::ROW));
    let needs_art = art.is_none() && track.thumb.is_some();
    let item = QueueItem {
        title: s(&track.title),
        artists: s(track.artist_names()),
        duration: s(track.duration.map(api::format_duration).unwrap_or_default()),
        art: art.unwrap_or_default(),
        current,
    };
    (item, needs_art)
}

impl App {
    /// Shows `track` as the one playing: the player bar, its artwork, the
    /// queue, the media overlay, Discord, the lyrics and the saved session.
    pub(super) fn present(self: &Rc<Self>, track: &Track, position: f64) {
        let ui = self.ui();
        ui.set_has_track(true);
        ui.set_np_title(s(&track.title));
        ui.set_np_artists(s(track.artist_names()));
        ui.set_np_album(s(track.album.as_ref().map_or("", |(_, n)| n.as_str())));
        ui.set_np_has_artist(track.first_artist_id().is_some());
        ui.set_np_has_album(track.album.is_some());
        ui.set_np_duration(self.state.borrow().playback.duration as f32);
        ui.set_np_position(position as f32);
        ui.set_np_buffered(0.0);
        self.show_playing(true);
        ui.set_np_art(
            track
                .thumb
                .as_deref()
                .and_then(|u| images::cached(u, images::ROW))
                .unwrap_or_default(),
        );
        ui.global::<ApiGlobal>().set_playing_id(s(&track.id));
        let id = track.id.clone();
        let art = track.thumb.clone();
        self.load_image(art.clone(), images::PLAYER, move |app, image| {
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
            if let Some(color) = art
                .as_deref()
                .and_then(|url| images::accent(url, images::PLAYER))
            {
                ui.set_np_accent(color);
            }
        });

        if let Some(media) = self.media.borrow_mut().as_mut() {
            let large = track.large_thumb();
            media.set_track(
                &track.title,
                &track.artist_names(),
                track.album.as_ref().map_or("", |(_, n)| n.as_str()),
                large.as_deref(),
                track.duration.unwrap_or(0) as f64,
            );
        }
        self.refresh_queue();
        self.save_session();
        self.update_presence();
        self.state.borrow_mut().lyrics_for = None;
        if ui.get_panel_open() && ui.get_panel_tab() == 1 {
            self.load_lyrics();
        }
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
        let playing = if self.ui().get_has_track() {
            Some(!self.player.is_paused())
        } else {
            None
        };
        if let Some(media) = self.media.borrow_mut().as_mut() {
            media.set_state(playing, self.player.position());
        }
    }

    pub(super) fn update_presence(&self) {
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
                    position: self.player.position().min(state.playback.duration.max(0.0)),
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
            .items()
            .enumerate()
            .map(|(i, t)| {
                let (item, needs_art) = queue_item(t, i as i32 == current);
                if needs_art {
                    missing.push((i, t.thumb.clone()));
                }
                item
            })
            .collect();
        self.queue_model.set_vec(items);
        for (i, url) in missing {
            self.load_queue_art(i, url, revision);
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
                let (item, needs_art) = queue_item(track, i as i32 == current);
                if needs_art {
                    missing.push((i, track.thumb.clone()));
                }
                self.queue_model.set_row_data(i, item);
            }
        }
        for (i, url) in missing {
            self.load_queue_art(i, url, revision);
        }
    }

    fn load_queue_art(self: &Rc<Self>, index: usize, url: Option<String>, revision: u64) {
        let Some(url) = url else { return };
        let expected = url.clone();
        self.load_image(Some(url), images::ROW, move |app, image| {
            // The entry may have moved, or been replaced in place.
            let same = app.queue_shown.get() == Some(revision)
                && app
                    .state
                    .borrow()
                    .queue
                    .get(index)
                    .and_then(|t| t.thumb.as_deref())
                    == Some(expected.as_str());
            if same && let Some(mut item) = app.queue_model.row_data(index) {
                item.art = image;
                app.queue_model.set_row_data(index, item);
            }
        });
    }

    pub(super) fn load_lyrics(self: &Rc<Self>) {
        let Some(id) = self.state.borrow().queue.current().map(|t| t.id.clone()) else {
            self.ui()
                .set_lyrics("Play something to see its lyrics.".into());
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
                let text = match result {
                    Ok(Some(text)) => text,
                    Ok(None) => "Lyrics aren't available for this song.".to_owned(),
                    Err(e) => {
                        app.state.borrow_mut().lyrics_for = None;
                        format!("Couldn't load lyrics: {e}")
                    }
                };
                let ui = app.ui();
                ui.set_lyrics(text.into());
                ui.set_lyrics_loading(false);
            },
        );
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
