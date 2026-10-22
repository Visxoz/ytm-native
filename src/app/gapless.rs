//! Queues the next track in the player to follow the current one without a
//! gap, and moves the app on once it is heard.

use std::rc::Rc;

use super::{App, playback::stream_spec};

/// How long before a track ends the next one is queued to follow it with no
/// gap: time for its stream to start downloading and its decoder to open.
const GAPLESS_LEAD_SECS: f64 = 15.0;

impl App {
    /// Queues the next track in the player shortly before the current one
    /// ends, so it follows without a gap, and drops a queued track the
    /// queue no longer has next. Runs every tick, so it copies nothing
    /// until a track is due.
    pub(super) fn update_gapless(self: &Rc<Self>, position: f64) {
        let (stale, due, generation) = {
            let state = self.state.borrow();
            let playback = &state.playback;
            let next = state.queue.peek_next().map(|t| t.id.as_str());
            let stale = playback
                .queued
                .as_ref()
                .is_some_and(|(id, _)| next != Some(id.as_str()));
            let near_end = playback.started
                && playback.duration > 0.0
                && playback.duration - position <= GAPLESS_LEAD_SECS
                && (playback.queued.is_none() || stale);
            let due = next
                .filter(|id| near_end && playback.queue_tried.as_deref() != Some(*id))
                .map(str::to_owned);
            (stale, due, playback.generation)
        };
        if stale {
            self.state.borrow_mut().playback.queued = None;
            self.player.clear_next();
        }
        let Some(next_id) = due else {
            return;
        };
        self.state.borrow_mut().playback.queue_tried = Some(next_id.clone());
        let prefetched = self.take_prefetched(&next_id);
        let resolve = self.resolve_future(next_id.clone(), prefetched);
        self.spawn(resolve, move |app, result| {
            let still_next = {
                let state = app.state.borrow();
                state.playback.generation == generation
                    && state.queue.peek_next().is_some_and(|t| t.id == next_id)
            };
            match result {
                Ok(stream) if still_next => {
                    app.player.queue_next(stream_spec(stream.clone(), 0.0));
                    app.state.borrow_mut().playback.queued = Some((next_id, stream));
                }
                Ok(_) => {}
                Err(e) => log::debug!("could not queue the next track: {e}"),
            }
        });
    }

    /// The queued track became audible: move the queue on and show it,
    /// without loading anything.
    pub(super) fn advance_gapless(self: &Rc<Self>) {
        let Some((id, stream)) = self.state.borrow_mut().playback.queued.take() else {
            return;
        };
        let current = self.state.borrow_mut().queue.advance(false).cloned();
        let Some(track) = current.filter(|t| t.id == id) else {
            // The queue changed under the player; play what it says now.
            self.start_current();
            return;
        };
        {
            let mut state = self.state.borrow_mut();
            let playback = &mut state.playback;
            playback.generation += 1;
            playback.started = true;
            playback.failures = 0;
            playback.duration = track.duration.map_or(0.0, f64::from);
            playback.requested_at = None;
            playback.queue_tried = None;
            playback.resume = None;
        }
        self.present(&track, 0.0);
        self.record_play();
        self.show_stream(&stream);
        self.ui().set_np_loading(false);
        self.update_media_state();
        self.extend_with_radio_if_ending();
        self.prefetch_next();
    }
}
