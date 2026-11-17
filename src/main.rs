#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(clippy::undocumented_unsafe_blocks)]

mod api;
mod app;
mod audio;
mod downloads;
mod folder_picker;
mod google_sign_in;
mod http;
mod images;
mod lastfm;
mod local;
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
            platform::windows::{IconExtWindows, WindowAttributesExtWindows},
            window::{Fullscreen, Icon, Window, WindowAttributes},
        },
    },
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSMICON, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
};

use crate::{app::App, settings::WindowGeometry};

slint::include_modules!();

/// The main window's class, by which a second launch finds it; its title
/// alone would match an Explorer window on a folder named Lily.
const WINDOW_CLASS: &str = "Lily";

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
    // The full player's screen-sized window isn't one to reopen at.
    if window.is_minimized() || window.is_fullscreen() {
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

fn hwnd(window: &impl HasWindowHandle) -> Option<*mut std::ffi::c_void> {
    let handle = window.window_handle().ok()?;
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

/// Keeps the full player's window a pixel taller than the screen. One that
/// covers the screen exactly is handed to the display past the compositor, and
/// G-Sync then follows the app's frame rate, which swings between idle and a
/// lyric line's glide, flickering the screen and slowing the cursor.
fn keep_composed(window: &Window, size: dpi::PhysicalSize<u32>) {
    // winit names the monitor while making the window cover it.
    let Some(Fullscreen::Borderless(Some(monitor))) = window.fullscreen() else {
        return;
    };
    if size != monitor.size() {
        return;
    }
    let Some(hwnd) = hwnd(window) else { return };
    let at = monitor.position();
    // shortcut: the extra row shows on a monitor directly below; pick a free
    // edge if anyone stacks monitors vertically.
    // SAFETY: `hwnd` is this live window's handle and the call dereferences no
    // memory; winit buffers the events its re-entrant messages raise until
    // this handler returns, so Slint's handler isn't re-entered.
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            at.x,
            at.y,
            size.width as i32,
            size.height as i32 + 1,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
}

/// Keeps the full player composed, and closes the search box on a press
/// outside it, as rows and cards take no focus; watched in winit's events so
/// the press still does its own work. A window keeps one winit event filter,
/// so other watchers belong in this one.
fn watch_window_events(ui: &AppWindow) {
    let weak = ui.as_weak();
    let mut cursor = dpi::PhysicalPosition::new(0.0_f64, 0.0);
    ui.window().on_winit_window_event(move |window, event| {
        let pressed_at = match event {
            WindowEvent::Resized(size) => {
                window.with_winit_window(|w| keep_composed(w, *size));
                None
            }
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

/// Per-character fallbacks, as Slint's own picks one font per writing system
/// and decorative letters ("ꜱʟᴏᴡᴇᴅ") drew as boxes. Emoji come before Segoe UI
/// Symbol, whose emoji are black and white.
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

/// The exe's own icon (resource 1), so Windows picks the drawn frame for each
/// size rather than scaling one image down, as Slint's `icon` property would.
fn with_exe_icon(attributes: WindowAttributes) -> WindowAttributes {
    // SAFETY: GetSystemMetrics takes no pointers and only reads a system setting.
    let small = unsafe { GetSystemMetrics(SM_CXSMICON) } as u32;
    let icon = |size| Icon::from_resource(1, size).ok();
    attributes
        .with_window_icon(icon(Some(dpi::PhysicalSize::new(small, small))))
        .with_taskbar_icon(icon(None))
}

fn main() {
    add_fallback_fonts();
    if std::env::var_os("WGPU_DX12_PRESENTATION_SYSTEM").is_none() {
        // Slint 1.19's composition swap chain leaves the window see-through
        // while it remakes its surface; Lily has no translucent windows.
        // SAFETY: as in `add_fallback_fonts`, before any other thread starts.
        unsafe { std::env::set_var("WGPU_DX12_PRESENTATION_SYSTEM", "Hwnd") };
    }
    if cfg!(debug_assertions) && std::env::var_os("WGPU_VALIDATION").is_none() {
        // wgpu turns on D3D12's debug layer in debug builds, which shadows its
        // million-entry descriptor heap in about 150 MB of RAM.
        // SAFETY: as above.
        unsafe { std::env::set_var("WGPU_VALIDATION", "0") };
    }
    logger::init();
    let Some(instance) = single_instance::acquire(WINDOW_CLASS) else {
        return;
    };
    update::clean_up();
    settings::move_data_dir();
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
    // Every YouTube request waits for a visitor ID; fetching one now does it
    // while the window is made, and the first page's requests share it.
    let warm = api.clone();
    runtime.spawn(async move {
        if let Err(e) = warm.visitor_data(false).await {
            log::debug!("could not fetch a visitor ID ahead: {e}");
        }
    });

    // YouTube takes most of a second to build the signed-in Home, so it is
    // asked for now, while the window is made, rather than once it shows.
    let home_ahead = api.has_cookie().then(|| {
        let api = api.clone();
        runtime.spawn(async move { api.page(&api::Route::Home, true).await })
    });

    let settings = settings::Settings::load();
    // winit resets the title bar to the system theme on WM_SETTINGCHANGE unless
    // the window was created with one (`set_theme` doesn't count). Only the
    // main window, made first: the sign-in page is white in every look.
    let title_bar = if settings.look == settings::Look::Light {
        slint::winit_030::winit::window::Theme::Light
    } else {
        slint::winit_030::winit::window::Theme::Dark
    };
    let main_window = std::cell::Cell::new(true);
    // Skia on Direct3D 12 takes scrolling off the CPU for about 50 MB of
    // driver memory; without a usable GPU it draws into a CPU buffer.
    if std::env::var_os("SLINT_BACKEND").is_none()
        && let Err(e) = slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("skia".into())
            .with_winit_window_attributes_hook(move |attributes| {
                let attributes = with_exe_icon(attributes);
                if main_window.replace(false) {
                    attributes
                        .with_theme(Some(title_bar))
                        .with_class_name(WINDOW_CLASS)
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

    let app = App::new(&ui, runtime.handle().clone(), api, settings, home_ahead);

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
    watch_window_events(&ui);

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
            let found = hwnd(&ui.window().window_handle());
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
                // The patched renderer drops its GPU surface on this frame. Asked
                // of winit, as Slint drops its own requests while one is pending.
                ui.window()
                    .with_winit_window(|window| window.request_redraw());
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
