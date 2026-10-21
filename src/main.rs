#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod app;
mod audio;
mod home_feed;
mod images;
mod logger;
mod media_controls;
mod premium;
mod presence;
mod queue;
mod settings;
mod single_instance;
mod streams;

use std::{rc::Rc, time::Duration};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{CloseRequestResponse, ComponentHandle, PhysicalPosition, PhysicalSize};

use crate::{app::App, settings::WindowGeometry};

slint::include_modules!();

const WINDOW_TITLE: &str = "YouTube Music";

fn restore_geometry(ui: &AppWindow, geometry: Option<WindowGeometry>) {
    let Some(g) = geometry else { return };
    if g.width >= 600 && g.height >= 400 {
        ui.window().set_size(PhysicalSize::new(g.width, g.height));
        ui.window().set_position(PhysicalPosition::new(g.x, g.y));
    }
    if g.maximized {
        ui.window().set_maximized(true);
    }
}

fn current_geometry(ui: &AppWindow, previous: Option<WindowGeometry>) -> Option<WindowGeometry> {
    let window = ui.window();
    if window.is_minimized() {
        return previous;
    }
    if window.is_maximized() {
        // Keep the restored size for when the window is unmaximised.
        return Some(WindowGeometry {
            maximized: true,
            ..previous.unwrap_or_default()
        });
    }
    let size = window.size();
    let position = window.position();
    Some(WindowGeometry {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
        maximized: false,
    })
}

fn hwnd(ui: &AppWindow) -> Option<*mut std::ffi::c_void> {
    let handle = ui.window().window_handle();
    let handle = handle.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut std::ffi::c_void),
        _ => None,
    }
}

/// Whether the window is minimised, asking Windows directly.
fn is_minimized(ui: &AppWindow) -> bool {
    match hwnd(ui) {
        // SAFETY: `hwnd` is this window's live handle.
        Some(hwnd) => unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsIconic(hwnd) != 0 },
        None => ui.window().is_minimized(),
    }
}

fn main() {
    logger::init();
    let Some(_instance) = single_instance::acquire(WINDOW_TITLE) else {
        return;
    };
    let _ = std::fs::create_dir_all(settings::data_dir());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Tokio runtime starts");

    let api = match api::Api::new() {
        Ok(api) => api,
        Err(e) => {
            log::error!("could not start the YouTube client: {e}");
            return;
        }
    };

    let settings = settings::Settings::load();
    // Skia drawing into a CPU buffer: crisp hinted text and smooth artwork
    // without the memory an OpenGL or Direct3D context costs.
    if std::env::var_os("SLINT_BACKEND").is_none()
        && let Err(e) = slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("skia-software".into())
            .select()
    {
        log::warn!("could not select the Skia software renderer: {e}");
    }
    let ui = AppWindow::new().expect("window opens");
    restore_geometry(&ui, settings.window);

    let app = App::new(&ui, runtime.handle().clone(), api, settings);

    let close_app = Rc::clone(&app);
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = weak.upgrade() {
            let mut settings = close_app.settings();
            settings.window = current_geometry(&ui, settings.window);
            settings.save();
        }
        close_app.shutdown();
        let _ = slint::quit_event_loop();
        CloseRequestResponse::HideWindow
    });

    ui.show().expect("window shows");

    let start_app = Rc::clone(&app);
    slint::Timer::single_shot(Duration::ZERO, move || start_app.start());

    // The native window appears some time after the event loop starts.
    let media_app = Rc::clone(&app);
    let weak = ui.as_weak();
    let attempts = std::cell::Cell::new(0);
    let media_timer = Rc::new(slint::Timer::default());
    let timer = Rc::downgrade(&media_timer);
    media_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            attempts.set(attempts.get() + 1);
            let Some(ui) = weak.upgrade() else { return };
            let found = hwnd(&ui);
            if let Some(hwnd) = found {
                media_app.attach_media_controls(hwnd);
            } else if attempts.get() < 50 {
                return;
            } else {
                log::warn!("no window handle for media controls");
            }
            if let Some(timer) = timer.upgrade() {
                timer.stop();
            }
        },
    );

    // Some drivers show a blank window after restoring from minimised until
    // something repaints, so repaint on restore, and once more shortly after.
    let restore_timer = slint::Timer::default();
    let weak = ui.as_weak();
    let was_minimized = std::cell::Cell::new(false);
    restore_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let minimized = is_minimized(&ui);
            if was_minimized.replace(minimized) && !minimized {
                // Again after the restore animation, which can swallow frames.
                for delay in [0, 150, 500] {
                    let weak = ui.as_weak();
                    slint::Timer::single_shot(Duration::from_millis(delay), move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_repaint_flip(!ui.get_repaint_flip());
                        }
                    });
                }
            }
        },
    );

    slint::run_event_loop_until_quit().expect("event loop runs");
    drop(media_timer);
    drop(app);
    runtime.shutdown_timeout(Duration::from_millis(500));
}
