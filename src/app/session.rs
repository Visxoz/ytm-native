use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::TimerMode;

use super::{App, State, TICK};
use crate::{
    api::{Page, Watched, Watchtime},
    settings,
};

/// How long after the last change the session and history are written.
/// Skipping through songs then costs one write, after the sound starts.
const PERSIST_AFTER: Duration = Duration::from_secs(2);
const SAVE_SESSION_FOR: Duration = Duration::from_secs(1);
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
    /// Set while the play is reported to YouTube.
    watch: Option<Watch>,
}

/// A play's watch-time pings.
pub(super) struct Watch {
    /// Signing in or out since stops them.
    account: u64,
    /// Also tells this play from a replay of the same song.
    started: Instant,
    /// Where they go, once the play's report is answered.
    ping: Option<Watchtime>,
    sent: usize,
    /// Since `started`.
    next_at: Duration,
    /// Stretches heard since the last ping, in song seconds.
    segments: Vec<(f64, f64)>,
    /// The end of the last stretch heard.
    position: f64,
}

#[derive(PartialEq)]
pub(super) enum Flush {
    /// On the web player's schedule.
    Due,
    /// On pausing.
    Now,
    /// The play ended.
    Last,
}

/// Adds a stretch played through, joining the last one it carries on from.
fn add_stretch(segments: &mut Vec<(f64, f64)>, from: f64, to: f64) {
    match segments.last_mut() {
        // `from` is the last tick's position, the very float that ended it.
        Some(last) if last.1 == from => last.1 = to,
        _ => segments.push((from, to)),
    }
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
        // Gapless, repeat and restarting start a play without stopping.
        self.report_watchtime(Flush::Last);
        let reported = {
            let mut state = self.state.borrow_mut();
            let State {
                history,
                queue,
                playback,
                signed_in,
                settings,
                account,
                ..
            } = &mut *state;
            let Some(track) = queue.current() else {
                return;
            };
            history.record(track);
            // As YouTube Music does, so the account's recommendations and
            // Listen again follow what plays here too.
            let reported = *signed_in && settings.youtube_history && !track.is_local();
            let watch = reported.then(|| Watch {
                account: *account,
                started: Instant::now(),
                ping: None,
                sent: 0,
                next_at: Duration::ZERO,
                segments: Vec::new(),
                position: 0.0,
            });
            let reported = watch.as_ref().map(|w| (track.id.clone(), w.started));
            playback.listening = Listening {
                unheard: Some(track.id.clone()),
                watch,
                ..Listening::default()
            };
            reported
        };
        self.persist_soon();
        self.scrobble_started();
        if let Some((id, started)) = reported {
            self.report_play(id, started);
        }
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
                let moved = played_through(position - last, now - at, speed);
                listening.heard += moved;
                if let Some(watch) = listening.watch.as_mut().filter(|_| !moved.is_zero()) {
                    add_stretch(&mut watch.segments, last, position);
                    watch.position = position;
                }
            }
            let heard = listening.heard;
            if let Some(id) = listening.unheard.take_if(|_| play_counts(heard, length)) {
                history.mark_heard(&id);
            }
            (heard, length)
        };
        self.scrobble_heard(heard, length);
        self.report_watchtime(Flush::Due);
    }

    fn report_play(self: &Rc<Self>, id: String, started: Instant) {
        let api = self.api.clone();
        self.spawn(
            async move { api.report_play(&id).await },
            move |app, result| {
                let listening = &mut app.state.borrow_mut().playback.listening;
                let Some(watch) = listening.watch.as_mut().filter(|w| w.started == started) else {
                    return;
                };
                match result {
                    Ok(Some(ping)) => {
                        watch.next_at = ping.due(0, Duration::ZERO);
                        watch.ping = Some(ping);
                    }
                    Ok(None) => listening.watch = None,
                    Err(e) => {
                        log::warn!("could not add a play to the YouTube history: {e}");
                        listening.watch = None;
                    }
                }
            },
        );
    }

    /// Tells YouTube how much of the play was heard since the last ping, as
    /// the web player does. shortcut: quitting mid-song loses what was heard
    /// since the last ping, as the runtime stops with the window; flush from
    /// `persist` should that matter.
    pub(super) fn report_watchtime(self: &Rc<Self>, flush: Flush) {
        let paused = self.player.is_paused();
        let sending = {
            let mut state = self.state.borrow_mut();
            let reporting = state.signed_in && state.settings.youtube_history;
            let account = state.account;
            let listening = &mut state.playback.listening;
            let Some(watch) = listening.watch.as_mut() else {
                return;
            };
            // An old play's URL never goes out with another account's cookie.
            if !reporting || watch.account != account {
                listening.watch = None;
                return;
            }
            let elapsed = watch.started.elapsed();
            let last = flush == Flush::Last;
            let heard = !watch.segments.is_empty();
            let due = match flush {
                Flush::Due => heard && elapsed >= watch.next_at,
                Flush::Now => heard,
                Flush::Last => true,
            };
            let url = match &watch.ping {
                Some(ping) if due => {
                    let watched = Watched {
                        segments: std::mem::take(&mut watch.segments),
                        position: watch.position,
                        elapsed,
                        paused,
                        last,
                    };
                    watch.sent += 1;
                    watch.next_at = ping.due(watch.sent, elapsed);
                    Some(ping.url(&watched))
                }
                _ => None,
            };
            if last {
                listening.watch = None;
            }
            url
        };
        let Some(url) = sending else {
            return;
        };
        let api = self.api.clone();
        self.spawn(
            async move { api.report_watchtime(url).await },
            |_, result| {
                if let Err(e) = result {
                    log::debug!("could not report the watch time: {e}");
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
        drop(state);
        // Bounded, as it waits on rustypipe's locks, which a request may hold.
        let api = self.api.clone();
        let _ = self.rt.block_on(async move {
            tokio::time::timeout(SAVE_SESSION_FOR, api.save_session()).await
        });
    }

    pub(super) fn resume_position(&self) -> f64 {
        let state = self.state.borrow();
        let current = state.queue.current().map(|t| t.id.as_str());
        match &state.playback.resume {
            Some((id, position)) if Some(id.as_str()) == current => *position,
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

    #[test]
    fn joins_stretches_played_on_and_splits_them_at_a_seek() {
        let mut segments = Vec::new();
        add_stretch(&mut segments, 0.0, 0.25);
        add_stretch(&mut segments, 0.25, 0.5);
        assert_eq!(segments, [(0.0, 0.5)], "played on");
        add_stretch(&mut segments, 60.0, 60.25);
        assert_eq!(segments, [(0.0, 0.5), (60.0, 60.25)], "after a seek");
    }
}
