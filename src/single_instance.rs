//! Keeps one copy of the app running; a second launch focuses the first.

use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HWND},
    System::Threading::CreateMutexW,
    UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW, SW_RESTORE,
        SetForegroundWindow, ShowWindow,
    },
};

use crate::tray::{self, wide};

/// Kept from before the app was named Lily, so an older copy and a newer one
/// never run together, which would share the data folder as it moves.
const MUTEX_NAME: &str = "Local\\ytm-native-single-instance";

/// Holds the mutex until dropped, so an update can start the new copy
/// before this process has fully exited.
pub struct Guard(HANDLE);

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from CreateMutexW and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Returns `None` if another instance is running, after focusing it.
pub fn acquire(window_class: &str) -> Option<Guard> {
    acquire_named(MUTEX_NAME, window_class, tray::CLASS)
}

fn acquire_named(mutex: &str, window_class: &str, tray_class: &str) -> Option<Guard> {
    let name = wide(mutex);
    // SAFETY: `name` is a valid NUL-terminated UTF-16 string.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    let exists =
        std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32);
    let guard = Guard(handle);
    if handle.is_null() || !exists {
        return Some(guard);
    }
    // Let go of the other copy's mutex at once, rather than at exit.
    drop(guard);
    // The other copy shows itself: Slint takes a window in the tray as hidden
    // and would not draw it if shown from here.
    let found = find_window(tray_class);
    if !found.is_null() {
        let mut pid = 0;
        // SAFETY: `found` came from FindWindowW and `pid` is a valid out
        // pointer. The foreground grant lets that process come to the front.
        unsafe {
            GetWindowThreadProcessId(found, &mut pid);
            AllowSetForegroundWindow(pid);
            PostMessageW(found, tray::WM_SHOW, 0, 0);
        }
        return None;
    }
    // A copy from before the tray.
    let hwnd = find_window(window_class);
    if !hwnd.is_null() {
        // SAFETY: `hwnd` was just returned by FindWindowW.
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
    None
}

fn find_window(class: &str) -> HWND {
    let class = wide(class);
    // SAFETY: `class` is NUL-terminated; a null title matches any window.
    unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_instance_is_refused() {
        let mutex = format!("Local\\ytm-native-test-{}", std::process::id());
        let first = acquire_named(&mutex, "no such window", "no such window");
        assert!(first.is_some(), "the first instance runs");
        assert!(
            acquire_named(&mutex, "no such window", "no such window").is_none(),
            "a second instance stops"
        );
        drop(first);
        assert!(
            acquire_named(&mutex, "no such window", "no such window").is_some(),
            "a new copy runs once the first lets go, as after an update"
        );
    }
}
