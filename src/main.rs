#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Every unsafe block says why it is sound; CI fails one that does not.
#![deny(clippy::undocumented_unsafe_blocks)]

mod api;
mod app;
mod audio;
mod google_sign_in;
mod home_feed;
mod http;
mod images;
mod logger;
mod media_controls;
mod premium;
mod presence;
mod queue;
mod secret;
mod settings;
mod single_instance;
mod streams;
mod update;

use std::{rc::Rc, time::Duration};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{
    CloseRequestResponse, ComponentHandle, PhysicalPosition, PhysicalSize,
    winit_030::WinitWindowAccessor,
};

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

/// Whether the window is minimised, as winit asks Windows directly.
fn is_minimized(ui: &AppWindow) -> bool {
    ui.window()
        .with_winit_window(|window| window.is_minimized())
        .flatten()
        .unwrap_or_else(|| ui.window().is_minimized())
}

/// Fonts Slint tries, character by character, for anything the UI font
/// lacks. Its own fallback picks one font per writing system, so letters
/// outside any, as in "𝓙𝓪𝓼𝓹𝓮𝓻", "ⓝⓘⓖⓗⓣⓒⓞⓡⓔ" or "ꜱʟᴏᴡᴇᴅ", drew as boxes. Emoji
/// come before Segoe UI Symbol, whose emoji are black and white.
const FALLBACK_FONTS: &[&str] = &["segoeui.ttf", "seguiemj.ttf", "seguisym.ttf", "cambria.ttc"];

fn add_fallback_fonts() {
    if std::env::var_os("SLINT_FONT_PATH").is_some() {
        return;
    }
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let fonts = std::path::Path::new(&windir).join("Fonts");
    let paths = FALLBACK_FONTS
        .iter()
        .map(|name| fonts.join(name))
        .filter(|path| path.exists());
    if let Ok(list) = std::env::join_paths(paths) {
        // SAFETY: called first thing in `main`, before this program starts any
        // other thread; Windows also serialises environment access itself.
        unsafe { std::env::set_var("SLINT_FONT_PATH", list) };
    }
}

fn main() {
    add_fallback_fonts();
    logger::init();
    let Some(instance) = single_instance::acquire(WINDOW_TITLE) else {
        return;
    };
    update::clean_up();
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
    // Skia on the GPU through Direct3D 12, which takes most of the work of
    // scrolling off the CPU for about 50 MB of driver memory. Without a usable
    // GPU, Skia falls back to drawing into a CPU buffer.
    if std::env::var_os("SLINT_BACKEND").is_none()
        && let Err(e) = slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("skia".into())
            .select()
    {
        log::warn!("could not select the Skia renderer: {e}");
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
    // A staged update starts only once this copy no longer holds the lock.
    drop(instance);
    update::launch_pending();
}
