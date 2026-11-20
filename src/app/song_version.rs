//! Music videos play as their song versions, which have Premium formats. The
//! song is swapped in only once its own stream is ready, so nothing waits for it.

use std::rc::Rc;

use super::App;
use crate::api::Track;

/// Most videos remembered; past it the cache starts over.
const SONG_VERSIONS_LIMIT: usize = 256;

pub(super) enum SongVersion {
    Pending,
    Known(Option<Track>),
}

impl App {
    /// Returns at once; the queue changes later.
    pub(super) fn swap_in_song_version(self: &Rc<Self>, next: &Track) {
        // A downloaded video plays as it was saved, online or not.
        if !next.video || self.state.borrow().downloads.contains(&next.id) {
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
        let id = video.id.clone();
        self.spawn(
            async move { api.song_version(&video).await },
            move |app, result| {
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
            },
        );
    }

    /// Until the song's stream resolves, the queue keeps the video and its stream.
    fn prefetch_song_version(self: &Rc<Self>, video_id: String, song: Track) {
        let ready = {
            let mut state = self.state.borrow_mut();
            let playback = &mut state.playback;
            if playback.prefetching.contains(&song.id) {
                return;
            }
            let ready = playback.has_prefetched(&song.id);
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
            app.state.borrow_mut().playback.prefetching.remove(&song.id);
            let stream = match result {
                Ok(stream) => stream,
                // Tried again at the next track start, not from here; until
                // then the video resolves as it is queued or starts.
                Err(e) => {
                    log::debug!("{}: the song version did not resolve: {e}", song.id);
                    return;
                }
            };
            // Kept only once the song is in the queue, which the video may
            // have left, or started playing, meanwhile.
            if app.put_song_version(&video_id, &song) {
                app.warm_stream(&stream);
                app.state
                    .borrow_mut()
                    .playback
                    .store_prefetched(song.id.clone(), stream);
            }
        });
    }

    /// Never swaps a video already playing. Returns whether it swapped.
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
