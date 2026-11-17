//! The mini player, made as it opens and dropped as it closes, so it holds no
//! GPU surface while closed.

use std::{cell::Cell, rc::Rc, time::Duration};

use slint::{
    CloseRequestResponse, ComponentHandle, PhysicalPosition, TimerMode,
    winit_030::{
        EventResult, WinitWindowAccessor,
        winit::{
            event::WindowEvent,
            platform::windows::{CornerPreference, WindowAttributesExtWindows},
            window::{WindowAttributes, WindowLevel},
        },
    },
};

use super::App;
use crate::{Api as ApiGlobal, MiniPlayer, Theme};

thread_local! {
    /// Set while the mini player's window is made, as `main`'s attributes
    /// hook can't otherwise tell it apart.
    static MAKING: Cell<bool> = const { Cell::new(false) };
}

/// Above other windows, off the taskbar, and rounded as framed windows are.
pub fn window_attributes(attributes: WindowAttributes) -> WindowAttributes {
    if !MAKING.get() {
        return attributes;
    }
    attributes
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_skip_taskbar(true)
        .with_corner_preference(CornerPreference::Round)
}

impl App {
    pub(super) fn toggle_mini_player(self: &Rc<Self>) {
        if self.mini.borrow().is_some() {
            self.close_mini_player();
        } else {
            self.open_mini();
        }
    }

    fn open_mini(self: &Rc<Self>) {
        MAKING.set(true);
        let mini = MiniPlayer::new().and_then(|mini| {
            if let Some((x, y)) = self.state.borrow().settings.mini_position
                && self.on_screen(x, y)
            {
                mini.window().set_position(PhysicalPosition::new(x, y));
            }
            mini.show().map(|()| mini)
        });
        MAKING.set(false);
        let mini = match mini {
            Ok(mini) => mini,
            Err(e) => {
                log::warn!("could not open the mini player: {e}");
                return;
            }
        };
        let action = |act: fn(&Rc<Self>)| {
            let weak = Rc::downgrade(self);
            move || {
                if let Some(app) = weak.upgrade() {
                    act(&app);
                }
            }
        };
        mini.on_toggle_play(action(|app| app.toggle_play()));
        mini.on_next(action(|app| app.next(true)));
        mini.on_previous(action(|app| app.previous()));
        mini.on_dismiss(action(|app| app.toggle_mini_player()));
        mini.on_expand(action(|app| {
            app.show_window();
            app.toggle_mini_player();
        }));
        // Alt+F4.
        let weak = Rc::downgrade(self);
        mini.window().on_close_requested(move || {
            if let Some(app) = weak.upgrade() {
                app.close_mini_player();
            }
            CloseRequestResponse::KeepWindowShown
        });
        // Saved once a move settles, as quitting may never come.
        let weak = Rc::downgrade(self);
        let settle = slint::Timer::default();
        mini.window().on_winit_window_event(move |_, event| {
            if let WindowEvent::Moved(at) = *event {
                let weak = weak.clone();
                settle.start(
                    TimerMode::SingleShot,
                    Duration::from_millis(500),
                    move || {
                        if let Some(app) = weak.upgrade() {
                            app.save_mini_position(at.x, at.y);
                        }
                    },
                );
            }
            EventResult::Propagate
        });
        let weak = mini.as_weak();
        mini.on_drag(move || {
            if let Some(mini) = weak.upgrade() {
                mini.window().with_winit_window(|window| {
                    let _ = window.drag_window();
                });
            }
        });
        *self.mini.borrow_mut() = Some(mini);
        self.ui().set_mini_open(true);
        self.theme_mini();
        self.sync_mini();
    }

    /// Dropped a tick later, as its own callback may be what closes it.
    pub(super) fn close_mini_player(&self) {
        let Some(mini) = self.mini.take() else {
            return;
        };
        let at = mini.window().position();
        self.save_mini_position(at.x, at.y);
        let _ = mini.hide();
        self.ui().set_mini_open(false);
        slint::Timer::single_shot(Duration::ZERO, move || drop(mini));
    }

    /// Each save writes settings.json, and `Moved` also fires as it opens.
    /// Off-screen ones are skipped, as minimised windows sit at -32000.
    fn save_mini_position(&self, x: i32, y: i32) {
        let at = Some((x, y));
        if self.state.borrow().settings.mini_position != at && self.on_screen(x, y) {
            self.update_settings(|s| s.mini_position = at);
        }
    }

    /// Whether a monitor shows enough of the mini player at `x`, `y` to grab
    /// it, as one unplugged or shrunk since could hide it.
    fn on_screen(&self, x: i32, y: i32) -> bool {
        const GRAB: (i32, i32) = (160, 60);
        self.ui()
            .window()
            .with_winit_window(|window| {
                window.available_monitors().any(|monitor| {
                    let (at, size) = (monitor.position(), monitor.size());
                    let (right, bottom) = (at.x + size.width as i32, at.y + size.height as i32);
                    x >= at.x && y >= at.y && x + GRAB.0 <= right && y + GRAB.1 <= bottom
                })
            })
            .unwrap_or(false)
    }

    pub(super) fn theme_mini(&self) {
        if let Some(mini) = &*self.mini.borrow() {
            let ui = self.ui();
            let (from, to) = (ui.global::<Theme>(), mini.global::<Theme>());
            to.set_look(from.get_look());
            to.set_accent(from.get_accent());
        }
    }

    /// Run every tick: setting a property to its value changes nothing.
    pub(super) fn sync_mini(&self) {
        let mini = self.mini.borrow();
        let Some(mini) = mini.as_ref() else {
            return;
        };
        let ui = self.ui();
        mini.set_has_track(ui.get_has_track());
        mini.set_song(ui.get_np_title());
        mini.set_artists(ui.get_np_artists());
        mini.set_art(ui.get_np_art());
        mini.set_playing(ui.global::<ApiGlobal>().get_is_playing());
        mini.set_loading(ui.get_np_loading());
        let duration = ui.get_np_duration();
        mini.set_progress(if duration > 0.0 {
            ui.get_np_position() / duration
        } else {
            0.0
        });
    }
}
