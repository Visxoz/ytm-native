use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::TimerMode;

use super::{App, State, TICK};
use crate::{api::Page, settings};

/// How long after the last change the session and history are written.
/// Skipping through songs then costs one write, after the sound starts.
const PERSIST_AFTER: Duration = Duration::from_secs(2);
/// Heard this long, or half a shorter song, a play reaches Listen again.
const PLAY_COUNTS_AFTER: Duration = Duration::from_secs(30);

/// The song playing, and how much of it has been heard.
#[derive(Default)]
pub(super) struct Listening {
    /// The song's ID, until its play counts.
    unheard: Option<String>,
    heard: Duration,
    /// The position at the last tick, and when that was, so only time
    /// played through counts, not stalls or seeks.
    last_tick: Option<(f64, Instant)>,
}

/// What a tick adds to the time heard: forward movement no greater than the
/// song time elapsed at `speed`, so a seek adds nothing while a tick held up
/// by the UI thread loses nothing, as the sound plays on its own thread.
fn played_through(moved: f64, elapsed: Duration, speed: f64) -> Duration {
    if moved > 0.0 && moved <= (elapsed + TICK).as_secs_f64() * speed {
        Duration::from_secs_f64(moved)
    } else {
        Duration::ZERO
    }
}

/// A song of unknown length counts after the full 30 s.
fn play_counts(heard: Duration, length: Duration) -> bool {
    let needed = match length {
        Duration::ZERO => PLAY_COUNTS_AFTER,
        length => PLAY_COUNTS_AFTER.min(length / 2),
    };
    heard >= needed
}

impl App {
    /// Run as the track's sound starts: the one place a play is recorded.
    pub(super) fn record_play(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            let State {
                history,
                queue,
                playback,
                ..
            } = &mut *state;
            let Some(track) = queue.current() else {
                return;
            };
            history.record(track);
            playback.listening = Listening {
                unheard: Some(track.id.clone()),
                ..Listening::default()
            };
        }
        self.persist_soon();
        self.scrobble_started();
        self.report_play();
    }

    /// Counts what was heard since the last tick, letting the play into
    /// Listen again once it counts; saved with the next play or on exit.
    pub(super) fn listened(self: &Rc<Self>, position: f64) {
        let (heard, length) = {
            let mut state = self.state.borrow_mut();
            let State {
                playback,
                settings,
                history,
                ..
            } = &mut *state;
            let length = Duration::from_secs_f64(playback.duration.max(0.0));
            let listening = &mut playback.listening;
            let now = Instant::now();
            if let Some((last, at)) = listening.last_tick.replace((position, now)) {
                let speed = f64::from(settings.speed);
                listening.heard += played_through(position - last, now - at, speed);
            }
            let heard = listening.heard;
            if let Some(id) = listening.unheard.take_if(|_| play_counts(heard, length)) {
                history.mark_heard(&id);
            }
            (heard, length)
        };
        self.scrobble_heard(heard, length);
    }

    /// As YouTube Music does, so the account's recommendations and Listen
    /// again follow what plays here too.
    fn report_play(self: &Rc<Self>) {
        let id = {
            let state = self.state.borrow();
            if !(state.signed_in && state.settings.youtube_history) {
                return;
            }
            let Some(track) = state.queue.current().filter(|t| !t.is_local()) else {
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

    pub(super) fn history_page(&self) -> Option<Page> {
        let tracks = self.state.borrow().history.tracks().cloned().collect();
        super::page::track_list_page("Recently played", "History", tracks)
    }

    pub(super) fn persist_soon(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.persist_timer
            .start(TimerMode::SingleShot, PERSIST_AFTER, move || {
                if let Some(app) = weak.upgrade() {
                    app.persist_in_background();
                }
            });
    }

    /// The UI thread only takes copies, so a scroll never waits on the disk.
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

    /// As on exit; the recent searches are saved again in case an earlier
    /// save failed.
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

    pub(super) fn take_resume_position(&self) -> f64 {
        let mut state = self.state.borrow_mut();
        let current = state.queue.current().map(|t| t.id.clone());
        match state.playback.resume.take() {
            Some((id, position)) if Some(&id) == current.as_ref() => position,
            _ => 0.0,
        }
    }

    /// A long queue is cut to the part around the current track before copying.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_only_time_played_through() {
        let ms = Duration::from_millis;
        assert_eq!(played_through(0.25, ms(250), 1.0), ms(250), "a tick");
        assert_eq!(
            played_through(5.0, ms(5000), 1.0),
            ms(5000),
            "a tick held up by the UI thread"
        );
        assert_eq!(played_through(30.0, ms(250), 1.0), Duration::ZERO, "a seek");
        assert_eq!(played_through(-30.0, ms(250), 1.0), Duration::ZERO, "back");
        assert_eq!(played_through(0.0, ms(250), 1.0), Duration::ZERO, "paused");
        assert_eq!(
            played_through(0.5, ms(250), 2.0),
            ms(500),
            "a tick at twice the speed"
        );
    }

    #[test]
    fn a_play_counts_after_thirty_seconds_or_half_a_short_song() {
        let secs = Duration::from_secs;
        assert!(!play_counts(secs(1), secs(200)), "a skip");
        assert!(!play_counts(secs(29), secs(200)));
        assert!(play_counts(secs(30), secs(200)));
        assert!(play_counts(secs(10), secs(20)), "half a short song");
        assert!(!play_counts(secs(0), Duration::ZERO), "an unknown length");
        assert!(play_counts(secs(30), Duration::ZERO));
    }
}
