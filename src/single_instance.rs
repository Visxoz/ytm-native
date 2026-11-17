//! Keeps one copy of the app running; a second launch focuses the first.

use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE},
    System::Threading::CreateMutexW,
    UI::WindowsAndMessaging::{FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow},
};

/// Kept from before the app was named Lily, so an older copy and a newer one
/// never run together, which would share the data folder as it moves.
const MUTEX_NAME: &str = "Local\\ytm-native-single-instance";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

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
    acquire_named(MUTEX_NAME, window_class)
}

fn acquire_named(mutex: &str, window_class: &str) -> Option<Guard> {
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
    let class = wide(window_class);
    // SAFETY: `class` is NUL-terminated; a null title matches any window.
    let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    if !hwnd.is_null() {
        // SAFETY: `hwnd` was just returned by FindWindowW.
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_instance_is_refused() {
        let mutex = format!("Local\\ytm-native-test-{}", std::process::id());
        let first = acquire_named(&mutex, "no such window");
        assert!(first.is_some(), "the first instance runs");
        assert!(
            acquire_named(&mutex, "no such window").is_none(),
            "a second instance stops"
        );
        drop(first);
        assert!(
            acquire_named(&mutex, "no such window").is_some(),
            "a new copy runs once the first lets go, as after an update"
        );
    }
}
