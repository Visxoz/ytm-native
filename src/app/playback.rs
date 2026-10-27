//! Loading tracks and the transport controls: play, pause, seek, skip,
//! volume, shuffle and repeat.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::SharedString;

use super::{
    App,
    resolve::{PREMIUM_NO, PREMIUM_YES, premium_dry_run},
};
use crate::{
    audio::{Opening, Source, StreamSpec},
    queue::Repeat,
    streams::{ResolvedStream, premium},
};

/// Pressing previous later than this restarts the track instead.
const RESTART_AFTER_SECS: f64 = 3.0;
/// Stream URLs last about six hours; refresh well before.
pub(super) const PREFETCH_FRESH: Duration = Duration::from_secs(60 * 60);
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

pub(super) fn stream_spec(stream: ResolvedStream, start_at: f64) -> StreamSpec {
    StreamSpec {
        source: Source {
            url: stream.url,
            size: stream.size,
            headers: stream.headers,
        },
        opening: Opening {
            mime: stream.mime,
            loudness_db: stream.loudness_db,
            start_at,
        },
    }
}

impl App {
    /// Loads and plays the queue's current track.
    pub(super) fn start_current(self: &Rc<Self>) {
        let Some(track) = self.state.borrow().queue.current().cloned() else {
            self.stop();
            return;
        };
        let generation = {
            let mut state = self.state.borrow_mut();
            let playback = &mut state.playback;
            playback.generation += 1;
            playback.started = false;
            playback.duration = track.seconds();
            playback.requested_at = Some(Instant::now());
            playback.forget_queued();
            playback.generation
        };
        self.player.stop();
        self.player.set_paused(false);

        let resume = match &self.state.borrow().playback.resume {
            Some((id, position)) if *id == track.id => *position,
            _ => 0.0,
        };
        self.present(&track, resume);
        let ui = self.ui();
        ui.set_np_loading(true);
        ui.set_np_quality(SharedString::new());
        ui.set_np_premium(false);

        let prefetched = self.take_prefetched(&track.id);
        let resolve = self.resolve_future(track.id.clone(), prefetched);
        self.spawn(resolve, move |app, result| {
            if app.state.borrow().playback.generation != generation {
                return;
            }
            match result {
                Ok(stream) => app.load_stream(stream),
                Err(e) => app.playback_failed(&track.title, &e),
            }
        });
    }

    /// A stream resolved ahead of time for `id`, if still fresh.
    pub(super) fn take_prefetched(&self, id: &str) -> Option<ResolvedStream> {
        self.state.borrow_mut().playback.take_prefetched(id)
    }

    fn load_stream(self: &Rc<Self>, stream: ResolvedStream) {
        self.show_stream(&stream);
        let start_at = self.take_resume_position();
        self.player.load(stream_spec(stream, start_at));
    }

    fn playback_failed(self: &Rc<Self>, title: &str, error: &str) {
        log::warn!("playback failed: {error}");
        // A Premium stream that will not play gets one retry on the free one.
        let retry_free = {
            let mut state = self.state.borrow_mut();
            let id = state.queue.current().map(|t| t.id.clone());
            if state.playback.current_premium && state.playback.force_free != id {
                state.playback.current_premium = false;
                state.playback.force_free.clone_from(&id);
                state.playback.prefetched.clear();
                id
            } else {
                None
            }
        };
        if let Some(id) = retry_free {
            premium::diag(format!(
                "{id}: Premium stream failed to play ({error}); retrying the free stream"
            ));
            self.start_current();
            return;
        }
        let failures = {
            let mut state = self.state.borrow_mut();
            state.playback.failures += 1;
            state.playback.failures
        };
        self.toast(format!("Couldn't play \u{201c}{title}\u{201d}: {error}"));
        let has_next = self.state.borrow().queue.peek_next().is_some();
        if failures < MAX_CONSECUTIVE_FAILURES && has_next {
            self.next(true);
        } else {
            self.ui().set_np_loading(false);
            self.show_playing(false);
        }
    }

    pub(super) fn on_player_event(self: &Rc<Self>, kind: &str, message: &str) {
        match kind {
            "started" => {
                let (waited, premium_stream) = {
                    let mut state = self.state.borrow_mut();
                    state.playback.started = true;
                    state.playback.failures = 0;
                    let waited = state.playback.requested_at.take().map(|t| t.elapsed());
                    (waited, state.playback.current_premium)
                };
                self.record_play();
                if let Some(waited) = waited {
                    let line = format!(
                        "first sound after {} ms ({}, {})",
                        waited.as_millis(),
                        if premium_stream { "Premium" } else { "free" },
                        self.ui().get_np_quality()
                    );
                    if premium_stream || premium_dry_run() {
                        premium::diag(&line);
                    } else {
                        log::info!("{line}");
                    }
                }
                self.ui().set_np_loading(false);
                self.report_playback();
                self.extend_with_radio_if_ending();
                self.prefetch_next();
            }
            "advanced" => self.advance_gapless(),
            "next-dropped" => self.state.borrow_mut().playback.forget_queued(),
            "ended" => self.next(false),
            "error" => {
                let title = self
                    .state
                    .borrow()
                    .queue
                    .current()
                    .map(|t| t.title.clone())
                    .unwrap_or_default();
                self.playback_failed(&title, message);
            }
            _ => {}
        }
    }

    /// Resolves the next track's stream ahead of time to skip the wait.
    pub(super) fn prefetch_next(self: &Rc<Self>) {
        // Smart Shuffle places its recommendations first, as the next track
        // may be one of them.
        self.smart_top_up();
        // Never waits: a music video is looked up alongside, and its song
        // version swapped in once found and prefetched.
        let video = self
            .state
            .borrow()
            .queue
            .peek_next()
            .filter(|t| t.video)
            .cloned();
        if let Some(video) = video {
            self.swap_in_song_version(&video);
        }
        let next = {
            let state = self.state.borrow();
            let Some(next) = state.queue.peek_next().cloned() else {
                return;
            };
            if state.playback.has_prefetched(&next.id)
                || state.playback.prefetching.contains(&next.id)
            {
                return;
            }
            next
        };
        self.state
            .borrow_mut()
            .playback
            .prefetching
            .insert(next.id.clone());
        let resolve = self.resolve_future(next.id.clone(), None);
        self.spawn(resolve, move |app, result| {
            let mut state = app.state.borrow_mut();
            state.playback.prefetching.remove(&next.id);
            match result {
                Ok(stream) => state.playback.store_prefetched(next.id, stream),
                Err(e) => log::debug!("prefetch failed: {e}"),
            }
        });
    }

    /// Queues a radio when the last track starts, so playback carries on.
    pub(super) fn extend_with_radio_if_ending(self: &Rc<Self>) {
        let seed = {
            let state = self.state.borrow();
            if !state.settings.autoplay
                || state.queue.repeat != Repeat::Off
                || state.queue.peek_next().is_some()
                || state.playback.extending
            {
                return;
            }
            let Some(current) = state.queue.current() else {
                return;
            };
            current.id.clone()
        };
        self.state.borrow_mut().playback.extending = true;
        let api = self.api.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            |app, result| {
                app.state.borrow_mut().playback.extending = false;
                match result {
                    Ok(tracks) => app.append_unique(tracks),
                    Err(e) => log::warn!("autoplay radio failed: {e}"),
                }
            },
        );
    }

    pub(super) fn next(self: &Rc<Self>, user: bool) {
        let moved = self.state.borrow_mut().queue.advance(user).is_some();
        if moved {
            self.start_current();
            return;
        }
        if self.state.borrow().settings.autoplay && !self.state.borrow().queue.is_empty() {
            // The radio may still be loading; queue it and move on.
            let seed = self.state.borrow().queue.current().map(|t| t.id.clone());
            if let Some(seed) = seed {
                let api = self.api.clone();
                self.spawn(
                    async move { api.track_radio(&seed).await },
                    |app, result| {
                        if let Ok(tracks) = result {
                            app.append_unique(tracks);
                            if app.state.borrow_mut().queue.advance(true).is_some() {
                                app.start_current();
                                return;
                            }
                        }
                        app.stop();
                    },
                );
                return;
            }
        }
        if !user {
            self.stop();
        }
    }

    pub(super) fn previous(self: &Rc<Self>) {
        if self.player.position() > RESTART_AFTER_SECS {
            self.seek_to(0.0);
            return;
        }
        if self.state.borrow_mut().queue.back().is_some() {
            self.start_current();
        }
    }

    fn stop(self: &Rc<Self>) {
        self.state.borrow_mut().playback.started = false;
        self.player.stop();
        self.player.set_paused(true);
        let ui = self.ui();
        self.show_playing(false);
        ui.set_np_loading(false);
        self.report_playback();
    }

    pub(super) fn toggle_play(self: &Rc<Self>) {
        if !self.ui().get_has_track() {
            return;
        }
        let ended = !self.state.borrow().playback.started && !self.ui().get_np_loading();
        if ended {
            // Replay the current track after it stopped at the queue's end.
            self.start_current();
            return;
        }
        self.set_paused(!self.player.is_paused());
    }

    pub(super) fn set_paused(self: &Rc<Self>, paused: bool) {
        self.player.set_paused(paused);
        self.show_playing(!paused);
        self.report_playback();
    }

    pub(super) fn seek_to(self: &Rc<Self>, secs: f64) {
        let duration = self.state.borrow().playback.duration;
        let secs = if duration > 0.0 {
            secs.clamp(0.0, duration - 0.5)
        } else {
            secs.max(0.0)
        };
        self.player.seek(secs);
        self.ui().set_np_position(secs as f32);
        self.report_playback_at(secs);
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.player.set_volume(volume);
        let mut state = self.state.borrow_mut();
        state.settings.volume = volume;
        state.settings.muted = false;
        drop(state);
        let ui = self.ui();
        ui.set_volume(volume);
        ui.set_muted(false);
    }

    pub(super) fn toggle_mute(&self) {
        let (muted, volume) = {
            let mut state = self.state.borrow_mut();
            state.settings.muted = !state.settings.muted;
            (state.settings.muted, state.settings.volume)
        };
        self.player.set_volume(if muted { 0.0 } else { volume });
        self.ui().set_muted(muted);
    }

    /// Cycles the shuffle button: off, shuffle, Smart Shuffle.
    pub(super) fn toggle_shuffle(self: &Rc<Self>) {
        let (shuffle, smart) = {
            let settings = &self.state.borrow().settings;
            match (settings.shuffle, settings.smart_shuffle) {
                (false, _) => (true, false),
                (true, false) => (true, true),
                (true, true) => (false, false),
            }
        };
        {
            let mut state = self.state.borrow_mut();
            if !smart {
                state.queue.remove_recommended();
                state.smart.reset();
            }
            state.queue.set_shuffle(shuffle);
        }
        self.update_settings(|s| {
            s.set_shuffle(shuffle);
            s.smart_shuffle = smart;
        });
        self.refresh_queue();
        self.prefetch_next();
    }

    pub(super) fn cycle_repeat(self: &Rc<Self>) {
        let repeat = {
            let mut state = self.state.borrow_mut();
            state.queue.repeat = state.queue.repeat.next();
            state.queue.repeat
        };
        self.update_settings(|s| s.repeat = repeat.index());
        self.prefetch_next();
    }

    pub(super) fn on_tick(self: &Rc<Self>) {
        let ui = self.ui();
        let status = self.premium.status();
        if self.premium_shown.replace(status) != status {
            ui.set_premium_status(
                match status {
                    PREMIUM_YES => "Active: playing 256 kbps Premium streams.",
                    PREMIUM_NO => "This account has no Premium; playing standard streams.",
                    _ => "When signed in with YouTube Premium, plays 256 kbps Opus.",
                }
                .into(),
            );
            if status == PREMIUM_YES && !self.state.borrow().settings.premium_account {
                self.update_settings(|s| s.premium_account = true);
            }
        }
        if !ui.get_has_track() {
            return;
        }
        let started = self.state.borrow().playback.started;
        if started {
            let position = self.player.position();
            ui.set_np_position(position as f32);
            let buffered = self.player.buffered();
            ui.set_np_buffered(buffered);
            let stalled = self.player.is_buffering() && !self.player.is_paused();
            ui.set_np_loading(stalled && buffered < 1.0);
            self.update_gapless(position);
        }
    }
}
