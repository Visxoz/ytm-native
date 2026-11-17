//! The main window going to the tray and back, and quitting.

use std::rc::Rc;

use slint::{
    CloseRequestResponse, ComponentHandle,
    winit_030::{WinitWindowAccessor, winit::window::Window},
};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};

use super::App;
use crate::{
    Api as ApiGlobal, AppWindow,
    tray::{Action, MenuState, Tray},
};

/// Off, going to the tray and back is immediate.
fn set_animated(window: &Window, on: bool) {
    let Some(hwnd) = crate::hwnd(window) else {
        return;
    };
    let disabled = i32::from(!on);
    // SAFETY: `hwnd` is this live window's; the BOOL is read from `disabled`,
    // which outlives the call.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED as u32,
            (&raw const disabled).cast(),
            size_of::<i32>() as u32,
        )
    };
}

fn is_shown(ui: &AppWindow) -> bool {
    ui.window().is_visible() && !crate::is_minimized(ui)
}

impl App {
    pub(super) fn attach_tray(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let ui = self.ui.clone();
        let tray = Tray::new(
            move |action| {
                if let Some(app) = weak.upgrade() {
                    app.tray_action(action);
                }
            },
            move || {
                ui.upgrade()
                    .map(|ui| MenuState {
                        shown: is_shown(&ui),
                        mini_open: ui.get_mini_open(),
                        playing: ui.global::<ApiGlobal>().get_is_playing(),
                        has_track: ui.get_has_track(),
                    })
                    .unwrap_or_default()
            },
        );
        if let Some(tray) = tray {
            tray.set_visible(self.state.borrow().settings.close_to_tray);
            let _ = self.tray.set(tray);
        }
    }

    pub(super) fn set_close_to_tray(&self, on: bool) {
        self.update_settings(|s| s.close_to_tray = on);
        if let Some(tray) = self.tray.get() {
            tray.set_visible(on);
        }
    }

    fn tray_action(self: &Rc<Self>, action: Action) {
        match action {
            Action::Toggle if is_shown(&self.ui()) => self.hide_to_tray(),
            Action::Toggle | Action::Show => self.show_window(),
            Action::MiniPlayer => self.toggle_mini_player(),
            Action::PlayPause => self.toggle_play(),
            Action::Next => self.next(true),
            Action::Previous => self.previous(),
            Action::Quit => self.quit(),
        }
    }

    pub fn close_requested(&self) -> CloseRequestResponse {
        if self.state.borrow().settings.close_to_tray && self.tray.get().is_some() {
            self.hide_to_tray();
            CloseRequestResponse::KeepWindowShown
        } else {
            self.quit();
            CloseRequestResponse::HideWindow
        }
    }

    /// Minimised first, as a hidden window draws no frame for the renderer to
    /// drop its GPU surface on; `main` hides it after that frame.
    fn hide_to_tray(&self) {
        let ui = self.ui();
        // Now, as quitting may never come, as when Windows shuts down.
        self.save_geometry(&ui);
        ui.window().with_winit_window(|window| {
            set_animated(window, false);
            window.set_minimized(true);
        });
        self.hide_pending.set(true);
    }

    pub fn take_hide_pending(&self) -> bool {
        self.hide_pending.take()
    }

    /// The window came back before reaching the tray, as from its taskbar button.
    pub fn cancel_hide(&self) {
        if self.hide_pending.take() {
            self.ui()
                .window()
                .with_winit_window(|window| set_animated(window, true));
        }
    }

    pub(super) fn show_window(&self) {
        let ui = self.ui();
        self.hide_pending.set(false);
        if !ui.window().is_visible() {
            let _ = ui.show();
        }
        ui.window().with_winit_window(|window| {
            window.set_minimized(false);
            window.focus_window();
            set_animated(window, true);
        });
    }

    pub fn quit(&self) {
        self.close_mini_player();
        let ui = self.ui();
        self.save_geometry(&ui);
        // Gone at once, rather than left frozen while the app winds down.
        let _ = ui.hide();
        self.shutdown();
        let _ = slint::quit_event_loop();
    }

    fn save_geometry(&self, ui: &AppWindow) {
        let window = crate::current_geometry(ui, self.state.borrow().settings.window);
        self.update_settings(|s| s.window = window);
    }
}
