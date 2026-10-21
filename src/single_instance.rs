//! Keeps one copy of the app running; a second launch focuses the first.

use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE},
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
    let name = wide(MUTEX_NAME);
    // SAFETY: `name` is a valid NUL-terminated UTF-16 string.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    // SAFETY: reads the calling thread's last error.
    if handle.is_null() || unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
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
