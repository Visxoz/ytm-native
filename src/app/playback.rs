//! Loading tracks and the transport controls: play, pause, seek, skip,
//! volume, shuffle and repeat.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::{SharedString, Timer};

use super::{
    App, SongVersion, State,
    resolve::{PREMIUM_NO, PREMIUM_YES, premium_dry_run},
    session::Flush,
};
use crate::{
    images,
    queue::Repeat,
    streams::{ResolvedStream, premium},
};

/// Pressing previous later than this restarts the track instead.
const RESTART_AFTER_SECS: f64 = 3.0;
/// Stream URLs last about six hours; refresh well before.
pub(super) const PREFETCH_FRESH: Duration = Duration::from_secs(60 * 60);
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// Opening a song again after its connection dropped, each try waiting
/// `REOPEN_AFTER` longer than the last.
const MAX_REOPENS: u32 = 3;
const REOPEN_AFTER: Duration = Duration::from_secs(5);

impl App {
    pub(super) fn start_current(self: &Rc<Self>) {
        let Some(track) = self.state.borrow().queue.current().cloned() else {
            self.stop();
            return;
        };
        let generation = {
            let mut state = self.state.borrow_mut();
            let playback = &mut state.playback;
            let recorded = playback.play_recorded == Some(playback.generation);
            playback.generation += 1;
            playback.started = false;
            playback.duration = track.seconds();
            playback.requested_at = Some(Instant::now());
            // A skip to the track queued in the player plays its stream,
            // unless that is a Premium one being retried on the free path.
            if playback
                .queued
                .as_ref()
                .is_some_and(|(id, ..)| *id == track.id)
                && playback.force_free.as_ref() != Some(&track.id)
            {
                playback.unqueue();
            } else {
                playback.forget_queued();
            }
            // A song opened again carries on without a second play.
            if !playback.reopening(&track.id) {
                playback.reopen = None;
            } else if recorded {
                playback.play_recorded = Some(playback.generation);
            }
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

        let prefetched = self.state.borrow_mut().playback.take_prefetched(&track.id);
        let resolve = self.resolve_future(track.id.clone(), prefetched.map(|(stream, _)| stream));
        self.spawn(resolve, move |app, result| {
            if app.state.borrow().playback.generation != generation {
                return;
            }
            let reopening = app.state.borrow().playback.reopening(&track.id);
            match result {
                Ok(stream) => app.load_stream(&track.id, stream),
                // Still offline.
                Err(e) if reopening => app.connection_lost(&e),
                Err(e) => app.playback_failed(&track.title, &e),
            }
        });
    }

    fn load_stream(self: &Rc<Self>, id: &str, stream: ResolvedStream) {
        self.show_stream(&stream);
        let start_at = self.resume_position();
        self.player.load(self.stream_spec(id, stream, start_at));
    }

    /// Opens the song again where it stopped, a few times while the
    /// connection is gone, then stops on it, so Play carries on.
    fn connection_lost(self: &Rc<Self>, error: &str) {
        log::warn!("lost the connection: {error}");
        let position = self.player.position();
        let (id, sounded, tries, generation) = {
            let mut state = self.state.borrow_mut();
            let Some(id) = state.queue.current().map(|t| t.id.clone()) else {
                return;
            };
            let playback = &mut state.playback;
            let sounded = playback.started;
            let tries = playback.reopen_try(&id, position);
            playback.started = false;
            (id, sounded, tries, playback.generation)
        };
        if self.player.is_paused() || tries > MAX_REOPENS {
            if tries > MAX_REOPENS {
                self.toast("Lost the connection. Press play to carry on.");
            }
            self.stop();
            return;
        }
        if tries == 1 {
            self.toast("Lost the connection; trying again");
        }
        self.ui().set_np_loading(true);
        let weak = Rc::downgrade(self);
        Timer::single_shot(REOPEN_AFTER * tries, move || {
            let Some(app) = weak.upgrade() else { return };
            // A late event may have sent it after the song played on.
            let due = {
                let playback = &app.state.borrow().playback;
                playback.generation == generation && !playback.started && playback.reopen.is_some()
            };
            if !due {
                return;
            }
            {
                // The download gave up ahead of the sound, which played on
                // through what was buffered, unless sought since.
                let resume = &mut app.state.borrow_mut().playback.resume;
                if sounded && *resume == Some((id.clone(), position)) {
                    *resume = Some((id, app.player.position()));
                }
            }
            if app.player.is_paused() {
                // Play then opens it.
                app.ui().set_np_loading(false);
            } else {
                app.start_current();
            }
        });
    }

    fn playback_failed(self: &Rc<Self>, title: &str, error: &str) {
        log::warn!("playback failed: {error}");
        // A Premium stream that will not play gets one retry on the free one.
        let retry_free = {
            let mut state = self.state.borrow_mut();
            let State {
                queue, playback, ..
            } = &mut *state;
            match queue.current().map(|t| t.id.clone()) {
                Some(id)
                    if playback.current_premium && playback.force_free.as_ref() != Some(&id) =>
                {
                    playback.current_premium = false;
                    playback.force_free = Some(id.clone());
                    playback.prefetched.clear();
                    // Mid-song, carry on from where the sound got to, read before
                    // `start_current` stops the player; at the very end, start over.
                    let position = self.player.position();
                    let carry_on = playback.started
                        && (playback.duration <= 0.0 || position < playback.duration - 1.0);
                    if carry_on {
                        playback.resume = Some((id.clone(), position));
                    }
                    Some((id, carry_on))
                }
                _ => None,
            }
        };
        if let Some((id, carry_on)) = retry_free {
            self.premium.suspect_player();
            premium::diag(format!(
                "{id}: Premium stream failed to play ({error}); retrying the free stream"
            ));
            self.start_current();
            if carry_on {
                let playback = &mut self.state.borrow_mut().playback;
                playback.play_recorded = Some(playback.generation);
            }
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
            // Stopped like any other end, so the device does not run on.
            self.stop();
        }
    }

    pub(super) fn on_player_event(self: &Rc<Self>, kind: &str, message: &str) {
        match kind {
            "started" => {
                let (waited, premium_stream, recorded) = {
                    let mut state = self.state.borrow_mut();
                    let playback = &mut state.playback;
                    playback.started = true;
                    playback.failures = 0;
                    playback.reopen = None;
                    playback.resume = None;
                    let waited = playback.requested_at.take().map(|t| t.elapsed());
                    let recorded = playback.play_recorded.take() == Some(playback.generation);
                    (waited, playback.current_premium, recorded)
                };
                if !recorded {
                    self.record_play();
                }
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
            "next-dropped" => self.state.borrow_mut().playback.unqueue(),
            // A stream that dropped is kept, so its track rides out the
            // drop as it plays rather than failing to resolve offline.
            "next-failed" if message != "dropped" => {
                self.state.borrow_mut().playback.queued_failed = true;
            }
            "ended" => self.next(false),
            "dropped" => self.connection_lost(message),
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
        // At the sizes `present` shows the moment the track starts; the
        // queue panel loads its own while open.
        let art = self
            .state
            .borrow()
            .queue
            .peek_next()
            .and_then(|t| t.thumb.clone());
        if self.ui().get_full_player() {
            self.load_image(art.clone(), images::COVER, |_, _| {});
        }
        self.load_image(art, images::PLAYER, |_, _| {});
        let next = {
            let state = self.state.borrow();
            let Some(next) = state.queue.peek_next().cloned() else {
                return;
            };
            // A video whose song version is known has that resolving in its
            // place, unless it is playing, as on repeat, and stays. One still
            // being looked up resolves too, for a quick skip.
            let replaced = matches!(
                state.song_versions.get(&next.id),
                Some(SongVersion::Known(Some(_)))
            ) && state.queue.current().is_none_or(|t| t.id != next.id);
            if replaced
                || state.playback.has_prefetched(&next.id)
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
                Ok(stream) => {
                    app.warm_stream(&stream);
                    state.playback.store_prefetched(next.id, stream);
                }
                Err(e) => log::debug!("prefetch failed: {e}"),
            }
        });
    }

    /// Fetches a prefetched stream's first bytes on Tokio's blocking pool,
    /// as the download's blocking client must never run on its workers.
    pub(super) fn warm_stream(&self, stream: &ResolvedStream) {
        if stream.file.is_some() {
            return;
        }
        let (url, headers) = (stream.url.clone(), stream.headers.clone());
        self.rt
            .spawn_blocking(move || crate::streams::warm(&url, &headers));
    }

    /// Queues a radio when the last track starts, so playback carries on.
    pub(super) fn extend_with_radio_if_ending(self: &Rc<Self>) {
        let (seed, revision, generation) = {
            let state = self.state.borrow();
            if !state.settings.autoplay
                || state.queue.repeat != Repeat::Off
                || state.queue.peek_next().is_some()
                || state.playback.extending
            {
                return;
            }
            // A local file has no radio to carry on with.
            let Some(current) = state.queue.current().filter(|t| !t.is_local()) else {
                return;
            };
            (
                current.id.clone(),
                state.queue.revision(),
                state.playback.generation,
            )
        };
        self.state.borrow_mut().playback.extending = true;
        let api = self.api.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            move |app, result| {
                app.state.borrow_mut().playback.extending = false;
                // Both changing means another queue was started, which the
                // radio is not for; should that one end, `next` asks again.
                // One alone is an edit, or a skip, in the same queue.
                let replaced = {
                    let state = app.state.borrow();
                    state.queue.revision() != revision && state.playback.generation != generation
                };
                match result {
                    Ok(_) if replaced => {}
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
            let (seed, generation) = {
                let state = self.state.borrow();
                (
                    state
                        .queue
                        .current()
                        .filter(|t| !t.is_local())
                        .map(|t| t.id.clone()),
                    state.playback.generation,
                )
            };
            if let Some(seed) = seed {
                let api = self.api.clone();
                self.spawn(
                    async move { api.track_radio(&seed).await },
                    move |app, result| {
                        // Something else started meanwhile. Not the queue's
                        // revision: the radio queued as the last track started
                        // may have just extended it.
                        if app.state.borrow().playback.generation != generation {
                            return;
                        }
                        // Failed, it may still move on to what that radio added.
                        match result {
                            Ok(tracks) => app.append_unique(tracks),
                            Err(e) => log::warn!("autoplay radio failed: {e}"),
                        }
                        if app.state.borrow_mut().queue.advance(true).is_some() {
                            app.start_current();
                            return;
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
            // Starting over is a new play, as a replay through repeat-one is.
            if self.state.borrow().playback.started {
                self.record_play();
            }
            return;
        }
        if self.state.borrow_mut().queue.back().is_some() {
            self.start_current();
        }
    }

    pub(super) fn stop(self: &Rc<Self>) {
        self.state.borrow_mut().playback.started = false;
        self.player.stop();
        self.player.set_paused(true);
        let ui = self.ui();
        self.show_playing(false);
        ui.set_np_loading(false);
        self.report_playback();
        // Play carries on a song stopped on a dropped connection as the same play.
        let reopening = {
            let state = self.state.borrow();
            let current = state.queue.current();
            current.is_some_and(|t| state.playback.reopening(&t.id))
        };
        self.report_watchtime(if reopening { Flush::Now } else { Flush::Last });
    }

    pub(super) fn toggle_play(self: &Rc<Self>) {
        if !self.ui().get_has_track() {
            return;
        }
        if self.stopped() {
            // Replay the current track after it stopped at the queue's end.
            self.start_current();
            return;
        }
        self.set_paused(!self.player.is_paused());
    }

    /// Whether nothing is loaded for the track shown, as after a restart or
    /// once the queue has played out.
    fn stopped(&self) -> bool {
        !self.state.borrow().playback.started && !self.ui().get_np_loading()
    }

    pub(super) fn set_paused(self: &Rc<Self>, paused: bool) {
        self.player.set_paused(paused);
        self.show_playing(!paused);
        self.report_playback();
        if paused {
            self.report_watchtime(Flush::Now);
        }
    }

    pub(super) fn seek_to(self: &Rc<Self>, secs: f64) {
        let duration = self.state.borrow().playback.duration;
        let secs = if duration > 0.0 {
            secs.clamp(0.0, duration - 0.5)
        } else {
            secs.max(0.0)
        };
        let stopped = self.stopped();
        {
            // With nothing to seek in, or a song being opened again, the
            // track starts here.
            let mut state = self.state.borrow_mut();
            if let Some(id) = state.queue.current().map(|t| t.id.clone())
                && (stopped || state.playback.reopening(&id))
            {
                state.playback.resume = Some((id, secs));
            }
        }
        if !stopped {
            self.player.seek(secs);
        }
        self.ui().set_np_position(secs as f32);
        self.show_lyric_line(secs);
        self.report_playback_at(secs);
    }

    /// Plays at the speed Settings lists at `index`.
    pub(super) fn set_speed(&self, index: usize) {
        let Some(&speed) = super::SPEEDS.get(index) else {
            return;
        };
        self.update_settings(|s| s.speed = speed);
        self.player.set_speed(speed);
        // The overlay and Discord show clock time, which the speed scales.
        let track = self.state.borrow().queue.current().cloned();
        if let Some(track) = track {
            self.show_media_track(&track);
        }
        self.report_playback();
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
            // Remembered across restarts either way, so neither answer is
            // asked for again on the first songs.
            let (known, none) = (status == PREMIUM_YES, status == PREMIUM_NO);
            let saved = {
                let settings = &self.state.borrow().settings;
                (settings.premium_account, settings.no_premium)
            };
            if (known || none) && saved != (known, none) {
                self.update_settings(|s| {
                    s.premium_account = known;
                    s.no_premium = none;
                });
            }
        }
        let started = self.state.borrow().playback.started;
        if ui.get_has_track() && started {
            let position = self.player.position();
            ui.set_np_position(position as f32);
            self.show_lyric_line(position);
            self.listened(position);
            let buffered = self.player.buffered();
            ui.set_np_buffered(buffered);
            let stalled = self.player.is_buffering() && !self.player.is_paused();
            ui.set_np_loading(stalled && buffered < 1.0);
            self.update_gapless(position);
        }
        self.sync_mini();
    }
}
