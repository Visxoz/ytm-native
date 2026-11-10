//! What is kept between runs: the queue and position, restored on the next
//! start, the history of songs played, the recent searches, and plays not
//! yet sent to Last.fm.

use std::{rc::Rc, time::Duration};

use slint::TimerMode;

use super::{App, State};
use crate::{
    api::{self, Header, Page, QueueMode, Section},
    settings,
};

/// How long after the last change the session and history are written.
/// Skipping through songs then costs one write, after the sound starts.
const PERSIST_AFTER: Duration = Duration::from_secs(2);

impl App {
    /// Adds the playing track to the history as its sound starts, starts
    /// logging it to Last.fm, and reports it to YouTube.
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
        self.scrobble_started();
        self.report_play();
    }

    /// Adds the playing song to the YouTube account's history, as YouTube
    /// Music does, so its recommendations and Listen again follow what
    /// plays here too.
    fn report_play(self: &Rc<Self>) {
        let id = {
            let state = self.state.borrow();
            if !(state.signed_in && state.settings.youtube_history) {
                return;
            }
            let Some(track) = state.queue.current() else {
                return;
            };
            track.id.clone()
        };
        let Some(cookie) = self.premium.cookie() else {
            return;
        };
        let api = self.api.clone();
        self.spawn(
            async move { api.report_play(&id, &cookie).await },
            |_, result| {
                if let Err(e) = result {
                    log::warn!("could not add a play to the YouTube history: {e}");
                }
            },
        );
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
                    app.persist_in_background();
                }
            });
    }

    /// Writes the session and history off the UI thread, which only takes
    /// copies of them, so a scroll never waits on the disk.
    fn persist_in_background(self: &Rc<Self>) {
        let session = self.session();
        let history = self.state.borrow().history.unsaved();
        let session_ticket = settings::SESSION_SAVES.ticket();
        let history_ticket = settings::HISTORY_SAVES.ticket();
        self.spawn(
            async move {
                if let Some(session) = session {
                    session.save(session_ticket);
                }
                history.and_then(|(history, revision)| {
                    history.save(history_ticket).then_some(revision)
                })
            },
            |app, saved| {
                if let Some(revision) = saved {
                    app.state.borrow_mut().history.saved_up_to(revision);
                }
            },
        );
    }

    /// Writes the session, history and unsent scrobbles now, as on exit,
    /// and the recent searches should saving them as they changed have
    /// failed.
    pub(super) fn persist(&self) {
        self.persist_timer.stop();
        if let Some(session) = self.session() {
            session.save(settings::SESSION_SAVES.ticket());
        }
        let mut state = self.state.borrow_mut();
        if let Some((history, revision)) = state.history.unsaved()
            && history.save(settings::HISTORY_SAVES.ticket())
        {
            state.history.saved_up_to(revision);
        }
        state.searches.save();
        state.scrobbler.save();
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

    /// The queue and position to save, so the next start can resume. A long
    /// queue is cut to the part around the current track before copying.
    fn session(&self) -> Option<settings::Session> {
        let state = self.state.borrow();
        let index = state.queue.current_index()?;
        let current = state.queue.current().map(|t| t.id.as_str());
        let position = match &state.playback.resume {
            Some((id, position)) if Some(id.as_str()) == current => *position,
            _ if state.playback.started => self.player.position(),
            _ => 0.0,
        };
        let window = settings::Session::window(state.queue.len(), index);
        Some(settings::Session {
            queue: state
                .queue
                .items()
                .skip(window.start)
                .take(window.len())
                .cloned()
                .collect(),
            index: index - window.start,
            position,
            duration: state.playback.duration,
            recommended: state
                .queue
                .recommended()
                .filter(|i| window.contains(i))
                .map(|i| i - window.start)
                .collect(),
        })
    }

    /// Shows the last run's track, paused, ready to resume where it was.
    pub(super) fn restore_session(self: &Rc<Self>) {
        let Some(session) = settings::Session::load() else {
            return;
        };
        let track = session.queue[session.index].clone();
        {
            let mut state = self.state.borrow_mut();
            state.set_queue(session.queue, session.index, false);
            state.queue.mark_recommended(session.recommended);
            state.playback.resume = Some((track.id.clone(), session.position));
            state.playback.duration = track.duration.map_or(session.duration, f64::from);
        }
        self.show_track_details(&self.ui(), &track, session.position);
        self.show_playing(false);
        self.load_player_art(&track);
        self.refresh_queue();
    }
}
