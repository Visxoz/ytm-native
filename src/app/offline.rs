//! Songs downloaded to play offline, and the user's own music in the
//! folders they add.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::PoisonError,
};

use slint::{ComponentHandle, Model};

use super::{App, page::track_list_page};
use crate::{
    RowKind,
    api::{self, Route, Track},
    downloads::Saved,
    local,
    streams::ResolvedStream,
};

/// A local file plays as the decoders find it, which the badge names by
/// its extension.
fn local_stream(path: &Path) -> ResolvedStream {
    ResolvedStream {
        codec: path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default(),
        file: Some(path.to_owned()),
        ..Default::default()
    }
}

impl App {
    /// The file `id` plays from: a local file's own path, or its download.
    pub(super) fn offline_stream(self: &Rc<Self>, id: &str) -> Option<ResolvedStream> {
        let path = Path::new(id);
        if local::owns(path) {
            return Some(local_stream(path));
        }
        let (stream, gone) = {
            let mut state = self.state.borrow_mut();
            let stream = state.downloads.stream(id);
            let gone = stream.is_none() && state.downloads.remove(id);
            (stream, gone)
        };
        if gone {
            log::warn!("a downloaded song's file has gone; streaming it instead");
            self.downloads_changed();
        }
        stream
    }

    /// Queues `tracks` to download one after another, skipping those
    /// downloaded or on their way.
    pub(super) fn download(self: &Rc<Self>, tracks: Vec<Track>) {
        let added = {
            let mut state = self.state.borrow_mut();
            let before = state.download_queue.len();
            for track in tracks {
                let known = state.downloads.contains(&track.id)
                    || state.download_queue.iter().any(|t| t.id == track.id);
                if !known && api::is_video_id(&track.id) {
                    state.download_queue.push_back(track);
                }
            }
            state.download_queue.len() - before
        };
        match added {
            0 => self.toast("Already downloaded"),
            1 => self.toast("Downloading the song"),
            n => self.toast(format!("Downloading {}", api::songs(n as u64))),
        }
        self.download_next();
    }

    /// Starts the first queued download unless one is under way. The queue's
    /// first track is the one downloading.
    fn download_next(self: &Rc<Self>) {
        let track = {
            let mut state = self.state.borrow_mut();
            if state.downloading {
                return;
            }
            let Some(track) = state.download_queue.front().cloned() else {
                return;
            };
            state.downloading = true;
            track
        };
        let resolve = self.keeping_future(track.id.clone());
        self.spawn(resolve, move |app, result| match result {
            Ok(stream) => app.save_download(track, stream),
            Err(e) => app.download_done(Err((track.title, e))),
        });
    }

    fn save_download(self: &Rc<Self>, track: Track, stream: ResolvedStream) {
        let (source, refresh) = self.remote_source(&track.id, stream.clone());
        let title = track.title.clone();
        // Blocking, as the player's download is, so off Tokio's workers.
        let save = self
            .rt
            .spawn_blocking(move || Saved::fetch(track, stream, source, refresh));
        self.spawn(save, move |app, result| {
            let result = result
                .unwrap_or_else(|_| Err("the download stopped".into()))
                .map_err(|e| (title, e));
            app.download_done(result);
        });
    }

    fn download_done(self: &Rc<Self>, result: Result<Saved, (String, String)>) {
        let finished = {
            let mut state = self.state.borrow_mut();
            state.download_queue.pop_front();
            state.downloading = false;
            state.download_queue.is_empty()
        };
        match result {
            Ok(saved) => {
                self.state.borrow_mut().downloads.insert(saved);
                self.downloads_changed();
                if finished {
                    self.toast("Downloads are ready to play offline");
                }
            }
            Err((title, e)) => {
                self.toast(format!("Couldn't download \u{201c}{title}\u{201d}: {e}"))
            }
        }
        self.download_next();
    }

    pub(super) fn remove_download(self: &Rc<Self>, id: &str) {
        let removed = {
            let mut state = self.state.borrow_mut();
            // Its file is going, so it's prefetched afresh.
            state.playback.prefetched.remove(id);
            state.downloads.remove(id)
        };
        if removed {
            self.downloads_changed();
        }
    }

    fn downloads_changed(self: &Rc<Self>) {
        self.state.borrow().downloads.save();
        self.show_downloaded();
        self.refresh_downloads_page();
    }

    /// Marks the rows shown as downloaded or not, after one changed.
    fn show_downloaded(&self) {
        let state = self.state.borrow();
        let header = state.page.header_tracks();
        let all = !header.is_empty() && header.iter().all(|t| state.downloads.contains(&t.id));
        for i in 0..self.rows.row_count() {
            let Some(mut row) = self.rows.row_data(i) else {
                continue;
            };
            let changed = match row.kind {
                RowKind::Track => {
                    let downloaded = state.downloads.contains(&row.track.id);
                    std::mem::replace(&mut row.track.downloaded, downloaded) != downloaded
                }
                RowKind::Header => std::mem::replace(&mut row.downloaded, all) != all,
                _ => false,
            };
            if changed {
                self.rows.set_row_data(i, row);
            }
        }
    }

    pub(super) fn show_downloads(self: &Rc<Self>) {
        let tracks = self.state.borrow().downloads.tracks();
        self.show_or_empty(
            track_list_page("Offline", "Downloads", tracks),
            "Songs you download will show up here.",
        );
    }

    /// Shows the Downloads page again where it was scrolled, should it be open.
    fn refresh_downloads_page(self: &Rc<Self>) {
        let scroll = self.ui().get_page_scroll();
        {
            let mut state = self.state.borrow_mut();
            if state.route != Route::Downloads {
                return;
            }
            state.restore_scroll = Some(scroll);
            // Rows move, so art still on its way to their old places is dropped.
            state.page_generation += 1;
        }
        self.show_downloads();
    }

    /// Lists the folders' music, reading only files new since the last time.
    pub(super) fn load_local_files(self: &Rc<Self>, generation: u64) {
        let folders = self.state.borrow().settings.local_folders.clone();
        if folders.is_empty() {
            self.show_or_empty(None, "Add a folder of your music to play it here.");
            return;
        }
        let library = std::sync::Arc::clone(&self.local);
        let scan = self.rt.spawn_blocking(move || {
            let mut library = library.lock().unwrap_or_else(PoisonError::into_inner);
            let library = library.get_or_insert_with(local::Library::load);
            let (tracks, changed) = library.scan(&folders);
            if changed {
                library.save();
            }
            tracks
        });
        self.spawn(scan, move |app, tracks| {
            if app.state.borrow().page_generation != generation {
                return;
            }
            let page = track_list_page("On this PC", "Local files", tracks.unwrap_or_default());
            app.show_or_empty(page, "No music was found in your folders.");
        });
    }

    /// Asks for a folder of music to add, and shows Local files with it.
    pub(super) fn add_folder(self: &Rc<Self>) {
        let Some(owner) = crate::hwnd(&self.ui().window().window_handle()) else {
            return;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        crate::folder_picker::pick(owner as isize, move |folder| {
            let _ = tx.send(folder);
        });
        let app = Rc::clone(self);
        let _ = slint::spawn_local(async move {
            if let Ok(Some(folder)) = rx.await {
                app.folder_added(folder);
            }
        });
    }

    fn folder_added(self: &Rc<Self>, folder: PathBuf) {
        if !self.state.borrow().settings.local_folders.contains(&folder) {
            self.update_settings(|s| s.local_folders.push(folder));
            self.show_folders();
        }
        self.show_fresh(Route::LocalFiles);
    }

    pub(super) fn remove_folder(self: &Rc<Self>, index: usize) {
        if index >= self.state.borrow().settings.local_folders.len() {
            return;
        }
        self.update_settings(|s| {
            s.local_folders.remove(index);
        });
        self.show_folders();
        if self.state.borrow().route == Route::LocalFiles {
            self.reload();
        }
    }
}
