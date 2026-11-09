//! Scrobbling to Last.fm: connecting the account, and logging each song
//! once enough of it has been heard.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::Timer;

use super::{App, TICK};
use crate::lastfm::{self, Error, Play};

/// How often to ask whether the user has allowed access in the browser,
/// and for how long before giving up.
const POLL_EVERY: Duration = Duration::from_secs(3);
const POLL_FOR: Duration = Duration::from_secs(5 * 60);

/// The song playing, until enough of it has been heard to count.
struct Listening {
    play: Play,
    heard: Duration,
    /// The position at the last tick, so only time played through counts,
    /// not stalls or seeks.
    position: Option<f64>,
}

#[derive(Default)]
pub(super) struct Scrobbler {
    session: Option<lastfm::Session>,
    pending: lastfm::Pending,
    playing: Option<Listening>,
    /// A batch is on its way to Last.fm.
    sending: bool,
    /// Last.fm refused a batch as invalid, so plays go one at a time until
    /// the one it refuses is found and dropped.
    one_at_a_time: bool,
    /// Bumped by connecting and disconnecting, so replies for an earlier
    /// account or attempt are ignored.
    generation: u64,
}

impl Scrobbler {
    pub(super) fn load() -> Self {
        Self {
            session: lastfm::load_session(),
            pending: lastfm::Pending::load(),
            ..Default::default()
        }
    }

    pub(super) fn save(&mut self) {
        self.pending.save();
    }
}

impl App {
    /// Shows the Last.fm account, or how to connect one.
    pub(super) fn show_lastfm(&self) {
        let ui = self.ui();
        ui.set_lastfm_available(lastfm::available());
        let state = self.state.borrow();
        let name = state.scrobbler.session.as_ref().map(|s| s.name.as_str());
        ui.set_lastfm_user(name.unwrap_or_default().into());
    }

    /// Starts logging the song that just started playing, and shows it on
    /// the profile as playing now.
    pub(super) fn scrobble_started(self: &Rc<Self>) {
        let (session, play, generation) = {
            let mut state = self.state.borrow_mut();
            state.scrobbler.playing = None;
            let Some(session) = state.scrobbler.session.clone() else {
                return;
            };
            let Some(track) = state.queue.current() else {
                return;
            };
            let play = Play {
                // The first artist, as Last.fm's catalogue lists features
                // in the title rather than as artists.
                artist: track
                    .artists
                    .first()
                    .map(|a| a.name.clone())
                    .unwrap_or_default(),
                track: track.title.clone(),
                album: track.album.as_ref().map(|(_, name)| name.clone()),
                duration: track.duration,
                timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
            };
            // Last.fm refuses a play without either.
            if play.artist.is_empty() || play.track.is_empty() {
                return;
            }
            state.scrobbler.playing = Some(Listening {
                play: play.clone(),
                heard: Duration::ZERO,
                position: None,
            });
            (session, play, state.scrobbler.generation)
        };
        self.spawn(
            async move { lastfm::now_playing(&session, &play).await },
            move |app, result| app.lastfm_failed(result, generation),
        );
    }

    /// Counts what was heard of the playing song since the last tick, now at
    /// `position`, and logs it once that is half of it or four minutes.
    pub(super) fn scrobble_heard(self: &Rc<Self>, position: f64) {
        {
            let mut state = self.state.borrow_mut();
            let length = Duration::from_secs_f64(state.playback.duration.max(0.0));
            let scrobbler = &mut state.scrobbler;
            let Some(listening) = scrobbler.playing.as_mut() else {
                return;
            };
            // Forward by about a tick: a seek, or a stall, adds nothing.
            let moved = position - listening.position.replace(position).unwrap_or(position);
            if moved > 0.0 && moved < 2.0 * TICK.as_secs_f64() {
                listening.heard += Duration::from_secs_f64(moved);
            }
            if !lastfm::counts(listening.heard, length) {
                return;
            }
            let Some(listening) = scrobbler.playing.take() else {
                return;
            };
            scrobbler.pending.push(listening.play);
        }
        self.send_scrobbles();
    }

    /// Sends the oldest plays Last.fm has yet to take, then the rest. The
    /// plays are saved once it answers, as they are tiny and sent at once.
    pub(super) fn send_scrobbles(self: &Rc<Self>) {
        let (session, batch, generation) = {
            let mut state = self.state.borrow_mut();
            let scrobbler = &mut state.scrobbler;
            let Some(session) = scrobbler.session.clone() else {
                return;
            };
            if scrobbler.sending || scrobbler.pending.is_empty() {
                return;
            }
            scrobbler.sending = true;
            let batch = scrobbler.pending.batch(scrobbler.one_at_a_time);
            (session, batch, scrobbler.generation)
        };
        self.spawn(
            async move {
                let result = lastfm::scrobble(&session, &batch).await;
                (result, batch)
            },
            move |app, (result, batch)| {
                let more = {
                    let mut state = app.state.borrow_mut();
                    let scrobbler = &mut state.scrobbler;
                    scrobbler.sending = false;
                    if scrobbler.generation != generation {
                        return;
                    }
                    let more = match &result {
                        Ok(()) => {
                            scrobbler.pending.sent(&batch);
                            true
                        }
                        // One play spoils a batch; find it, and drop it alone.
                        Err(Error::Rejected(_)) if batch.len() > 1 => {
                            scrobbler.one_at_a_time = true;
                            true
                        }
                        Err(Error::Rejected(e)) => {
                            log::warn!("Last.fm refused a play: {e}");
                            scrobbler.pending.sent(&batch);
                            scrobbler.one_at_a_time = false;
                            true
                        }
                        Err(_) => false,
                    };
                    scrobbler.pending.save();
                    more && !scrobbler.pending.is_empty()
                };
                app.lastfm_failed(result, generation);
                if more {
                    app.send_scrobbles();
                }
            },
        );
    }

    /// Forgets an account whose access was revoked, keeping its plays for
    /// when it connects again; other failures keep the plays for the next
    /// attempt. Replies for an earlier account change nothing.
    fn lastfm_failed(self: &Rc<Self>, result: Result<(), Error>, generation: u64) {
        match result {
            Ok(()) | Err(Error::Rejected(_)) => {}
            Err(_) if self.state.borrow().scrobbler.generation != generation => {}
            Err(Error::SignedOut) => {
                self.end_lastfm();
                self.toast("Last.fm no longer allows access. Connect it again in Settings.");
            }
            Err(e) => log::warn!("Last.fm: {e}"),
        }
    }

    /// Opens Last.fm in the browser for the user to allow access, then
    /// waits for them to.
    pub(super) fn connect_lastfm(self: &Rc<Self>) {
        let generation = {
            let scrobbler = &mut self.state.borrow_mut().scrobbler;
            scrobbler.generation += 1;
            scrobbler.generation
        };
        self.ui().set_lastfm_connecting(true);
        self.spawn(lastfm::authorise(), move |app, result| {
            if app.state.borrow().scrobbler.generation != generation {
                return;
            }
            match result {
                Ok((page, token)) => {
                    crate::http::open_in_browser(&page);
                    app.poll_lastfm(token, generation, Instant::now());
                }
                Err(e) => app.lastfm_not_connected(&e),
            }
        });
    }

    /// Asks whether access was allowed, again every few seconds until it is,
    /// through any brief failure to reach Last.fm.
    fn poll_lastfm(self: &Rc<Self>, token: String, generation: u64, since: Instant) {
        let asked = token.clone();
        self.spawn(
            async move { lastfm::session(&asked).await },
            move |app, result| {
                if app.state.borrow().scrobbler.generation != generation {
                    return;
                }
                match result {
                    Ok(session) => {
                        lastfm::save_session(&session);
                        app.toast(format!("Scrobbling to Last.fm as {}", session.name));
                        app.state.borrow_mut().scrobbler.session = Some(session);
                        app.ui().set_lastfm_connecting(false);
                        app.show_lastfm();
                        app.send_scrobbles();
                    }
                    Err(Error::NotYetAllowed | Error::Failed(_)) if since.elapsed() < POLL_FOR => {
                        let weak = Rc::downgrade(app);
                        Timer::single_shot(POLL_EVERY, move || {
                            if let Some(app) = weak.upgrade() {
                                app.poll_lastfm(token, generation, since);
                            }
                        });
                    }
                    Err(e) => app.lastfm_not_connected(&e),
                }
            },
        );
    }

    fn lastfm_not_connected(self: &Rc<Self>, error: &dyn std::fmt::Display) {
        log::warn!("could not connect Last.fm: {error}");
        self.ui().set_lastfm_connecting(false);
        self.toast("Couldn't connect to Last.fm. Try again in Settings.");
    }

    /// Forgets the account, or stops waiting for one to connect. Plays not
    /// yet sent are dropped with it, so none reach the next account.
    pub(super) fn disconnect_lastfm(&self) {
        self.end_lastfm();
        let pending = &mut self.state.borrow_mut().scrobbler.pending;
        pending.clear();
        pending.save();
    }

    /// Forgets the account's session, and any reply on its way for it.
    fn end_lastfm(&self) {
        {
            let scrobbler = &mut self.state.borrow_mut().scrobbler;
            scrobbler.generation += 1;
            scrobbler.session = None;
            scrobbler.playing = None;
        }
        lastfm::remove_session();
        self.ui().set_lastfm_connecting(false);
        self.show_lastfm();
    }
}
