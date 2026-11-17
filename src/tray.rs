//! The tray icon, and the hidden window that hears its clicks and a second launch.

use std::{
    cell::{Cell, RefCell},
    ptr::{null, null_mut},
    rc::Rc,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Shell::{
            NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFY_ICON_MESSAGE,
            NOTIFYICONDATAW, Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
            DestroyMenu, DestroyWindow, GetCursorPos, GetSystemMetrics, HICON, IMAGE_ICON,
            LR_DEFAULTCOLOR, LoadImageW, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW,
            RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SetForegroundWindow, TPM_NONOTIFY,
            TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP, WM_CLOSE, WM_LBUTTONUP,
            WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
        },
    },
};

/// By which a second launch finds this copy.
pub const CLASS: &str = "LilyTray";
/// A second launch asking for the main window.
pub const WM_SHOW: u32 = WM_APP + 1;
const WM_ICON: u32 = WM_APP + 2;

#[derive(Clone, Copy)]
pub enum Action {
    Toggle,
    Show,
    MiniPlayer,
    PlayPause,
    Next,
    Previous,
    Quit,
}

/// Read as the menu opens.
#[derive(Default)]
pub struct MenuState {
    pub shown: bool,
    pub mini_open: bool,
    pub playing: bool,
    pub has_track: bool,
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Handler {
    on_action: Box<dyn Fn(Action)>,
    menu_state: Box<dyn Fn() -> MenuState>,
    icon: HICON,
    /// Broadcast as Explorer restarts, forgetting the icon.
    taskbar_created: u32,
    added: Cell<bool>,
}

thread_local! {
    // Shared borrows only, as the menu's modal loop re-enters the window proc.
    static HANDLER: RefCell<Option<Rc<Handler>>> = const { RefCell::new(None) };
}

impl Handler {
    fn notify(&self, hwnd: HWND, message: NOTIFY_ICON_MESSAGE) {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_ICON,
            hIcon: self.icon,
            ..Default::default()
        };
        for (to, from) in data.szTip.iter_mut().zip("Lily".encode_utf16()) {
            *to = from;
        }
        // SAFETY: `data` is initialised and outlives the call.
        unsafe { Shell_NotifyIconW(message, &data) };
    }

    /// Once the window procedure has returned, so never inside a modal loop
    /// that holds the app's state, such as the folder picker's.
    fn defer(self: &Rc<Self>, action: Action) {
        let handler = Rc::clone(self);
        slint::Timer::single_shot(Duration::ZERO, move || (handler.on_action)(action));
    }

    fn menu(self: &Rc<Self>, hwnd: HWND) {
        let state = (self.menu_state)();
        let items = [
            Some((
                Action::Toggle,
                true,
                if state.shown {
                    "Hide Lily"
                } else {
                    "Show Lily"
                },
            )),
            Some((
                Action::MiniPlayer,
                true,
                if state.mini_open {
                    "Close mini player"
                } else {
                    "Mini player"
                },
            )),
            None,
            Some((
                Action::PlayPause,
                state.has_track,
                if state.playing { "Pause" } else { "Play" },
            )),
            Some((Action::Next, state.has_track, "Next")),
            Some((Action::Previous, state.has_track, "Previous")),
            None,
            Some((Action::Quit, true, "Quit")),
        ];
        // SAFETY: the menu is made, shown and destroyed here; each string
        // lives through the call taking it.
        let chosen = unsafe {
            let menu = CreatePopupMenu();
            for (id, item) in items.iter().enumerate() {
                match item {
                    Some((_, enabled, label)) => {
                        let flags = if *enabled {
                            MF_STRING
                        } else {
                            MF_STRING | MF_GRAYED
                        };
                        AppendMenuW(menu, flags, id + 1, wide(label).as_ptr());
                    }
                    None => {
                        AppendMenuW(menu, MF_SEPARATOR, 0, null());
                    }
                }
            }
            let mut at = POINT { x: 0, y: 0 };
            GetCursorPos(&mut at);
            // Without this, a click elsewhere leaves the menu open.
            SetForegroundWindow(hwnd);
            let flags = TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY;
            let chosen = TrackPopupMenu(menu, flags, at.x, at.y, 0, hwnd, null());
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            chosen
        };
        let chosen = usize::try_from(chosen)
            .ok()
            .and_then(|id| items.get(id.checked_sub(1)?));
        if let Some(Some((action, ..))) = chosen {
            self.defer(*action);
        }
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handler = HANDLER.with_borrow(Clone::clone);
    if let Some(handler) = handler {
        match msg {
            WM_SHOW => {
                handler.defer(Action::Show);
                return 0;
            }
            // As `taskkill` sends; destroying this window would leave the app out of reach.
            WM_CLOSE => {
                handler.defer(Action::Quit);
                return 0;
            }
            WM_ICON => {
                // Showing rather than toggling, as a double-click is two clicks.
                match lparam as u32 {
                    WM_LBUTTONUP => handler.defer(Action::Show),
                    WM_RBUTTONUP => handler.menu(hwnd),
                    _ => {}
                }
                return 0;
            }
            // 0 if registering failed, which would match `menu`'s WM_NULL.
            _ if msg == handler.taskbar_created && msg != 0 && handler.added.get() => {
                handler.notify(hwnd, NIM_ADD);
            }
            _ => {}
        }
    }
    // SAFETY: the arguments are the ones Windows passed in.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub struct Tray {
    hwnd: HWND,
}

impl Tray {
    /// The icon shows once `set_visible` asks.
    pub fn new(
        on_action: impl Fn(Action) + 'static,
        menu_state: impl Fn() -> MenuState + 'static,
    ) -> Option<Self> {
        let class = wide(CLASS);
        // SAFETY: a null name asks for this exe's own module.
        let instance = unsafe { GetModuleHandleW(null()) };
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: `window_class` and the name it points to live through both
        // calls; the window is never shown.
        let hwnd = unsafe {
            RegisterClassW(&window_class);
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            )
        };
        if hwnd.is_null() {
            log::warn!("could not make the tray's window");
            return None;
        }
        // SAFETY: GetSystemMetrics takes no pointers; the name is resource 1,
        // the exe's icon, as MAKEINTRESOURCE passes it.
        let icon = unsafe {
            let size = GetSystemMetrics(SM_CXSMICON);
            LoadImageW(instance, 1 as _, IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
        };
        // SAFETY: the string is NUL-terminated and lives through the call.
        let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
        HANDLER.set(Some(Rc::new(Handler {
            on_action: Box::new(on_action),
            menu_state: Box::new(menu_state),
            icon,
            taskbar_created,
            added: Cell::new(false),
        })));
        Some(Self { hwnd })
    }

    pub fn set_visible(&self, visible: bool) {
        HANDLER.with_borrow(|handler| {
            if let Some(handler) = handler
                && handler.added.replace(visible) != visible
            {
                handler.notify(self.hwnd, if visible { NIM_ADD } else { NIM_DELETE });
            }
        });
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // Can run during thread-local teardown, as the app outlives `main`.
        let Ok(handler) = HANDLER.try_with(RefCell::take) else {
            return;
        };
        if let Some(handler) = handler {
            if handler.added.get() {
                handler.notify(self.hwnd, NIM_DELETE);
            }
            // SAFETY: the icon was loaded in `new` and is destroyed once.
            unsafe { DestroyIcon(handler.icon) };
        }
        // SAFETY: the window was made in `new` on this thread.
        unsafe { DestroyWindow(self.hwnd) };
    }
}
