//! The queue and position saved on exit and restored on the next start.

use std::rc::Rc;

use slint::ComponentHandle;

use super::{App, s};
use crate::{Api as ApiGlobal, images, settings};

impl App {
    /// The restored position, if the current track is the restored one.
    pub(super) fn take_resume_position(&self) -> f64 {
        let mut state = self.state.borrow_mut();
        let current = state.queue.current().map(|t| t.id.clone());
        match state.playback.resume.take() {
            Some((id, position)) if Some(&id) == current.as_ref() => position,
            _ => 0.0,
        }
    }

    /// Saves the queue and position so the next start can resume.
    pub(super) fn save_session(&self) {
        let state = self.state.borrow();
        let Some(index) = state.queue.current_index() else {
            return;
        };
        let current = state.queue.current().map(|t| t.id.as_str());
        let position = match &state.playback.resume {
            Some((id, position)) if Some(id.as_str()) == current => *position,
            _ if state.playback.started => self.player.position(),
            _ => 0.0,
        };
        settings::Session {
            queue: state.queue.items().cloned().collect(),
            index,
            position,
            duration: state.playback.duration,
        }
        .save();
    }

    /// Shows the last run's track, paused, ready to resume where it was.
    pub(super) fn restore_session(self: &Rc<Self>) {
        let Some(session) = settings::Session::load() else {
            return;
        };
        let track = session.queue[session.index].clone();
        {
            let mut state = self.state.borrow_mut();
            state.queue.set(session.queue, session.index, false);
            state.playback.resume = Some((track.id.clone(), session.position));
            state.playback.duration = track.duration.map_or(session.duration, f64::from);
        }
        let duration = self.state.borrow().playback.duration;
        let ui = self.ui();
        ui.set_has_track(true);
        ui.set_np_title(s(&track.title));
        ui.set_np_artists(s(track.artist_names()));
        ui.set_np_album(s(track.album.as_ref().map_or("", |(_, n)| n.as_str())));
        ui.set_np_has_artist(track.first_artist_id().is_some());
        ui.set_np_has_album(track.album.is_some());
        ui.set_np_duration(duration as f32);
        ui.set_np_position(session.position as f32);
        self.show_playing(false);
        ui.global::<ApiGlobal>().set_playing_id(s(&track.id));
        self.load_image(track.thumb.clone(), images::PLAYER, |app, image| {
            app.ui().set_np_art(image);
        });
        self.refresh_queue();
    }
}
