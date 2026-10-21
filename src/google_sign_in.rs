//! Signing in on Google's own page, in a WebView2 window. Once that reaches
//! YouTube Music signed in, its cookies sign the app in as pasted ones do.

use raw_window_handle::HasWindowHandle;
use slint::ComponentHandle;
use wry::{
    PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    dpi::{LogicalPosition, PhysicalSize},
};

use crate::SignInWindow;

const START_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&passive=true\
    &continue=https%3A%2F%2Fmusic.youtube.com%2F";
const MUSIC_URL: &str = "https://music.youtube.com/";
/// A signed-in session has one of these.
const SESSION_COOKIES: &[&str] = &["SAPISID", "__Secure-3PAPISID"];

pub struct GoogleSignIn {
    window: SignInWindow,
    webview: Option<WebView>,
    context: WebContext,
}

impl GoogleSignIn {
    /// Shows the window; `attach` then places the page in it.
    pub fn open() -> Result<Self, String> {
        let window = SignInWindow::new().map_err(|e| e.to_string())?;
        window.show().map_err(|e| e.to_string())?;
        // In private mode the page's cookies stay in memory; the folder only
        // keeps WebView2's own files out of the exe's directory.
        let context = WebContext::new(Some(crate::settings::data_dir().join("webview")));
        Ok(Self {
            window,
            webview: None,
            context,
        })
    }

    /// Places Google's page in the window, once Windows has created it.
    /// Returns `false` until then. `on_page` runs on the UI thread as each
    /// page finishes loading, from inside WebView2's event, so it should
    /// only schedule work.
    pub fn attach(&mut self, on_page: impl Fn() + 'static) -> Result<bool, String> {
        let handle = self.window.window().window_handle();
        if handle.window_handle().is_err() {
            return Ok(false);
        }
        let webview = WebViewBuilder::new_with_web_context(&mut self.context)
            .with_url(START_URL)
            .with_incognito(true)
            .with_bounds(bounds(&self.window))
            .with_on_page_load_handler(move |event, _| {
                if matches!(event, PageLoadEvent::Finished) {
                    on_page();
                }
            })
            .build_as_child(&handle)
            .map_err(|e| format!("could not open Google's sign-in page: {e}"))?;
        self.webview = Some(webview);
        Ok(true)
    }

    pub fn window(&self) -> &SignInWindow {
        &self.window
    }

    /// Sizes the page to the window.
    pub fn fit(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.set_bounds(bounds(&self.window));
        }
    }

    /// The session's `Cookie` header, once the page is on YouTube Music
    /// signed in.
    pub fn cookie(&self) -> Option<String> {
        let webview = self.webview.as_ref()?;
        if !webview.url().ok()?.starts_with(MUSIC_URL) {
            return None;
        }
        let cookies = webview.cookies_for_url(MUSIC_URL).ok()?;
        if !cookies.iter().any(|c| SESSION_COOKIES.contains(&c.name())) {
            return None;
        }
        Some(
            cookies
                .iter()
                .map(|c| format!("{}={}", c.name(), c.value()))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

fn bounds(window: &SignInWindow) -> Rect {
    let size = window.window().size();
    Rect {
        position: LogicalPosition::new(0, 0).into(),
        size: PhysicalSize::new(size.width, size.height).into(),
    }
}
