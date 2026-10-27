//! Queues the next track in the player to follow the current one without a
//! gap or fading into it, and moves the app on once it is heard.

use std::rc::Rc;

use super::App;
use crate::{api::Track, audio::SILENCE_TRIM_SECS};

/// How long before a track ends the next one is queued to follow it with no
/// gap: time for its stream to start downloading and its decoder to open.
/// A crossfade adds its own length and the silence it may cut, as the
/// player holds that much of the current track back.
const GAPLESS_LEAD_SECS: f64 = 15.0;
/// Long enough to blend two songs, short enough to keep most of an ending.
const CROSSFADE_SECS: f64 = 6.0;

/// Whether `next` follows `current` on its album, which is often written to
/// run straight on, so it is never faded into.
fn in_album_order(current: Option<&Track>, next: &Track) -> bool {
    current.is_some_and(|c| match (&c.album, &next.album) {
        (Some((a, _)), Some((b, _))) if a == b => match (c.track_nr, next.track_nr) {
            (Some(a), Some(b)) => a.checked_add(1) == Some(b),
            _ => true,
        },
        _ => false,
    })
}

impl App {
    /// Queues the next track in the player shortly before the current one
    /// ends, so it follows without a gap or fades in, and drops a queued
    /// track the queue no longer has next. Runs every tick, so it copies
    /// nothing until a track is due.
    pub(super) fn update_gapless(self: &Rc<Self>, position: f64) {
        let (stale, due, generation) = {
            let state = self.state.borrow();
            let playback = &state.playback;
            let next = state.queue.peek_next();
            let crossfade = if state.settings.crossfade
                && next.is_some_and(|next| !in_album_order(state.queue.current(), next))
            {
                CROSSFADE_SECS
            } else {
                0.0
            };
            let lead = if crossfade > 0.0 {
                GAPLESS_LEAD_SECS + crossfade + SILENCE_TRIM_SECS
            } else {
                GAPLESS_LEAD_SECS
            };
            let next = next.map(|t| t.id.as_str());
            let stale = playback
                .queued
                .as_ref()
                .is_some_and(|(id, _)| next != Some(id.as_str()));
            let near_end = playback.started
                && playback.duration > 0.0
                && playback.duration - position <= lead
                && (playback.queued.is_none() || stale);
            let due = next
                .filter(|id| near_end && playback.queue_tried.as_deref() != Some(*id))
                .map(|id| (id.to_owned(), crossfade));
            (stale, due, playback.generation)
        };
        if stale {
            self.state.borrow_mut().playback.queued = None;
            self.player.clear_next();
        }
        let Some((next_id, crossfade)) = due else {
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
                    app.player
                        .queue_next(app.stream_spec(&next_id, stream.clone(), 0.0), crossfade);
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
            playback.duration = track.seconds();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn track(album: Option<&str>, track_nr: Option<u16>) -> Track {
        Track {
            album: album.map(|id| (id.into(), "Album".into())),
            track_nr,
            ..Default::default()
        }
    }

    #[test]
    fn runs_an_album_on_without_fading() {
        let first = track(Some("a"), Some(1));
        assert!(in_album_order(Some(&first), &track(Some("a"), Some(2))));
        assert!(
            in_album_order(Some(&first), &track(Some("a"), None)),
            "numbers unknown, assumed in order"
        );
    }

    #[test]
    fn fades_between_anything_else() {
        let first = track(Some("a"), Some(1));
        assert!(
            !in_album_order(Some(&first), &track(Some("a"), Some(5))),
            "shuffled"
        );
        assert!(!in_album_order(Some(&first), &track(Some("b"), Some(2))));
        assert!(!in_album_order(
            Some(&track(None, None)),
            &track(None, None)
        ));
        assert!(!in_album_order(None, &first));
        assert!(!in_album_order(
            Some(&track(Some("a"), Some(u16::MAX))),
            &track(Some("a"), Some(0))
        ));
    }
}
