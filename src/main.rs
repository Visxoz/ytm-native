#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Every unsafe block says why it is sound; CI fails one that does not.
#![deny(clippy::undocumented_unsafe_blocks)]

mod api;
mod app;
mod audio;
mod google_sign_in;
mod http;
mod images;
mod lastfm;
mod logger;
mod media_controls;
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
    winit_030::{
        EventResult, WinitWindowAccessor,
        winit::{
            dpi,
            event::{ElementState, TouchPhase, WindowEvent},
        },
    },
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

/// Closes the search box on a press anywhere outside it. Clicking a row or
/// card takes no focus from it, so the box would stay focused, its dropdown
/// over the page. Watching winit's events rather than catching the press in
/// the UI lets that press go on to do what it was meant to. A window keeps
/// one winit event filter, so anything else watching winit's events belongs
/// in this one; a second would replace it.
fn close_search_on_outside_press(ui: &AppWindow) {
    let weak = ui.as_weak();
    let mut cursor = dpi::PhysicalPosition::new(0.0_f64, 0.0);
    ui.window().on_winit_window_event(move |window, event| {
        let pressed_at = match event {
            WindowEvent::CursorMoved { position, .. } => {
                cursor = *position;
                None
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            } => Some(cursor),
            WindowEvent::Touch(touch) if touch.phase == TouchPhase::Started => Some(touch.location),
            _ => None,
        };
        if let (Some(at), Some(ui)) = (pressed_at, weak.upgrade()) {
            // Converted as Slint converts the same events, so both agree.
            let at = at.to_logical::<f32>(window.scale_factor().into());
            ui.invoke_close_search_outside(at.x, at.y);
        }
        EventResult::Propagate
    });
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
    // winit puts the system's theme back on the title bar at every
    // WM_SETTINGCHANGE unless the window was created with one, which
    // `set_theme` does not count as. Only the main window, made first: the
    // sign-in window's page is white in every look.
    let title_bar = if settings.look == settings::Look::Light {
        slint::winit_030::winit::window::Theme::Light
    } else {
        slint::winit_030::winit::window::Theme::Dark
    };
    let main_window = std::cell::Cell::new(true);
    // Skia on the GPU through Direct3D 12, which takes most of the work of
    // scrolling off the CPU for about 50 MB of driver memory. Without a usable
    // GPU, Skia falls back to drawing into a CPU buffer.
    if std::env::var_os("SLINT_BACKEND").is_none()
        && let Err(e) = slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("skia".into())
            .with_winit_window_attributes_hook(move |attributes| {
                if main_window.replace(false) {
                    attributes.with_theme(Some(title_bar))
                } else {
                    attributes
                }
            })
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
    close_search_on_outside_press(&ui);

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
                media_app.attach_window(hwnd);
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
            let was = was_minimized.replace(minimized);
            if minimized && !was {
                // Minimising asks for no frame, so ask for one: the patched
                // renderer drops its GPU surface on it (patches/).
                ui.window().request_redraw();
            }
            if was && !minimized {
                // Again after the restore animation, which can swallow frames.
                for delay in [0, 150, 500] {
                    let weak = ui.as_weak();
                    slint::Timer::single_shot(Duration::from_millis(delay), move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_repaint_flip(!ui.get_repaint_flip());
                            // Frames skipped while minimised leave Slint's
                            // redraw tracker dirty, so a property change alone
                            // asks for no frame.
                            ui.window().request_redraw();
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
