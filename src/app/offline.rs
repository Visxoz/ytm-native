//! The user's own music, in the folders they add.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::PoisonError,
};

use slint::ComponentHandle;

use super::{App, page::track_list_page};
use crate::{api::Route, local, streams::ResolvedStream};

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
    /// The file `id` plays from, should it be a local file.
    pub(super) fn offline_stream(&self, id: &str) -> Option<ResolvedStream> {
        let path = Path::new(id);
        local::owns(path).then(|| local_stream(path))
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
