//! What is kept between runs: the queue and position, restored on the next
//! start, and the history of songs played.

use std::{rc::Rc, time::Duration};

use slint::{ComponentHandle, TimerMode};

use super::{App, State, s};
use crate::{
    Api as ApiGlobal,
    api::{self, Header, Page, QueueMode, Section},
    images, settings,
};

/// How long after the last change the session and history are written.
/// Skipping through songs then costs one write, after the sound starts.
const PERSIST_AFTER: Duration = Duration::from_secs(2);

impl App {
    /// Adds the playing track to the history, as its sound starts.
    pub(super) fn record_play(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            let State { history, queue, .. } = &mut *state;
            let Some(track) = queue.current() else {
                return;
            };
            history.record(track);
        }
        self.persist_soon();
    }

    /// The history as a page, newest first, or `None` when nothing has
    /// played yet.
    pub(super) fn history_page(&self) -> Option<Page> {
        let state = self.state.borrow();
        let history = &state.history;
        if history.is_empty() {
            return None;
        }
        Some(Page {
            header: Some(Header {
                label: "Recently played".to_owned(),
                title: "History".to_owned(),
                meta: api::songs(history.len() as u64),
                ..Default::default()
            }),
            sections: vec![Section::Tracks {
                title: String::new(),
                tracks: history.tracks().cloned().collect(),
                mode: QueueMode::Section,
                numbered: false,
            }],
            more: None,
        })
    }

    /// Writes the session and history shortly, once things settle.
    pub(super) fn persist_soon(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.persist_timer
            .start(TimerMode::SingleShot, PERSIST_AFTER, move || {
                if let Some(app) = weak.upgrade() {
                    app.persist();
                }
            });
    }

    /// Writes the session and history now, as on exit.
    pub(super) fn persist(&self) {
        self.persist_timer.stop();
        self.save_session();
        self.state.borrow_mut().history.save();
    }
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
    fn save_session(&self) {
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
            recommended: state.queue.recommended().collect(),
            collection: state.queue_collection,
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
            state.set_queue(session.queue, session.index, false, session.collection);
            state.queue.mark_recommended(session.recommended);
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
