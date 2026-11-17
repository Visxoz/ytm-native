//! Windows' folder picker, for adding a folder of music.

use std::path::PathBuf;

use windows::Win32::{
    Foundation::HWND,
    System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize,
    },
    UI::Shell::{
        FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
    },
};

/// Asks for a folder on a thread of its own, as the dialog runs its own
/// message loop, owned by the window `owner` so it stays in front of it.
/// `done` gets the folder, or `None` should the dialog be cancelled.
pub fn pick(owner: isize, done: impl FnOnce(Option<PathBuf>) + Send + 'static) {
    std::thread::Builder::new()
        .name("folder-picker".into())
        .spawn(move || done(show(HWND(owner as *mut _))))
        .expect("folder picker thread spawns");
}

fn show(owner: HWND) -> Option<PathBuf> {
    // SAFETY: COM is set up on this thread for the dialog's life, and the
    // dialog and its result are released before COM is shut down again. The
    // path is copied out of the string the dialog allocated before that is
    // freed, once.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
            .ok()
            .ok()?;
        let picked = (|| {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            dialog
                .SetOptions(dialog.GetOptions().ok()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
                .ok()?;
            // Cancelling answers with an error.
            dialog.Show(Some(owner)).ok()?;
            let name = dialog
                .GetResult()
                .ok()?
                .GetDisplayName(SIGDN_FILESYSPATH)
                .ok()?;
            let path = name.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(name.0 as *const _));
            path
        })();
        CoUninitialize();
        picked
    }
}
