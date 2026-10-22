//! Keeps one copy of the app running; a second launch focuses the first.

use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, HANDLE},
    System::Threading::CreateMutexW,
    UI::WindowsAndMessaging::{FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow},
};

const MUTEX_NAME: &str = "Local\\ytm-native-single-instance";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Held for the life of the process; the mutex closes when it exits.
pub struct Guard(#[allow(dead_code)] HANDLE);

/// Returns `None` if another instance is running, after focusing it.
pub fn acquire(window_title: &str) -> Option<Guard> {
    acquire_named(MUTEX_NAME, window_title)
}

fn acquire_named(mutex: &str, window_title: &str) -> Option<Guard> {
    let name = wide(mutex);
    // SAFETY: `name` is a valid NUL-terminated UTF-16 string.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    let exists =
        std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32);
    if handle.is_null() || !exists {
        return Some(Guard(handle));
    }
    let title = wide(window_title);
    // SAFETY: `title` is NUL-terminated; a null class matches any window.
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
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
    }
}
