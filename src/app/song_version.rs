//! Plays music videos as their song versions, which are audio only and have
//! Premium formats. The next track is looked up in the background while its
//! stream is prefetched, and the song is swapped in only once its own stream
//! is ready, so nothing ever waits for it.

use std::{rc::Rc, time::Instant};

use super::{App, playback::PREFETCH_FRESH};
use crate::api::Track;

/// Most videos remembered; past it the cache starts over.
const SONG_VERSIONS_LIMIT: usize = 256;

pub(super) enum SongVersion {
    Pending,
    Known(Option<Track>),
}

impl App {
    /// Starts swapping `next`, the next track, for its song version if it is
    /// a music video with one. Returns at once; the queue changes later.
    pub(super) fn swap_in_song_version(self: &Rc<Self>, next: &Track) {
        if !next.video {
            return;
        }
        let known = {
            let mut state = self.state.borrow_mut();
            let versions = &mut state.song_versions;
            match versions.get(&next.id) {
                Some(SongVersion::Known(Some(song))) => song.clone(),
                Some(_) => return,
                None => {
                    if versions.len() >= SONG_VERSIONS_LIMIT {
                        versions.retain(|_, v| matches!(v, SongVersion::Pending));
                    }
                    versions.insert(next.id.clone(), SongVersion::Pending);
                    drop(state);
                    self.look_up_song_version(next.clone());
                    return;
                }
            }
        };
        self.prefetch_song_version(next.id.clone(), known);
    }

    fn look_up_song_version(self: &Rc<Self>, video: Track) {
        let api = self.api.clone();
        let lookup = async move {
            let song = api.song_version(&video).await;
            (video.id, song)
        };
        self.spawn(lookup, |app, (id, result)| {
            // A failed lookup counts as none, so it is not retried per track.
            let song = result.unwrap_or_else(|e| {
                log::debug!("{id}: could not look for a song version: {e}");
                None
            });
            app.state
                .borrow_mut()
                .song_versions
                .insert(id.clone(), SongVersion::Known(song.clone()));
            if let Some(song) = song {
                app.prefetch_song_version(id, song);
            }
        });
    }

    /// Resolves the song's stream, then swaps the song in. Until then the
    /// queue keeps the video and its prefetched stream.
    fn prefetch_song_version(self: &Rc<Self>, video_id: String, song: Track) {
        let ready = {
            let mut state = self.state.borrow_mut();
            let playback = &mut state.playback;
            if playback.prefetching.contains(&song.id) {
                return;
            }
            let ready = playback
                .prefetched
                .get(&song.id)
                .is_some_and(|(_, at)| at.elapsed() < PREFETCH_FRESH);
            if !ready {
                playback.prefetching.insert(song.id.clone());
            }
            ready
        };
        if ready {
            self.put_song_version(&video_id, &song);
            return;
        }
        let resolve = self.resolve_future(song.id.clone(), None);
        self.spawn(resolve, move |app, result| {
            {
                let mut state = app.state.borrow_mut();
                let playback = &mut state.playback;
                playback.prefetching.remove(&song.id);
                match result {
                    Ok(stream) => {
                        playback
                            .prefetched
                            .insert(song.id.clone(), (stream, Instant::now()));
                    }
                    Err(e) => {
                        log::debug!("{}: the song version did not resolve: {e}", song.id);
                        return;
                    }
                }
            }
            if !app.put_song_version(&video_id, &song) {
                app.state.borrow_mut().playback.prefetched.remove(&song.id);
            }
        });
    }

    /// Puts `song` in place of the video `video_id` in the queue, unless the
    /// video is already playing, and drops the video's prefetched stream.
    /// Returns whether it did.
    fn put_song_version(self: &Rc<Self>, video_id: &str, song: &Track) -> bool {
        let rows = {
            let mut state = self.state.borrow_mut();
            if state.queue.current().is_some_and(|t| t.id == video_id) {
                return false;
            }
            let rows = state.queue.replace(|t| t.id == video_id, song);
            if !rows.is_empty() {
                state.playback.prefetched.remove(video_id);
            }
            rows
        };
        if rows.is_empty() {
            return false;
        }
        log::info!("{video_id}: playing its song version, {}", song.id);
        self.refresh_queue_rows(&rows);
        true
    }
}
