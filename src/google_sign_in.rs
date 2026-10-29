//! Signing in on Google's own page, in a WebView2 window. Once that reaches
//! YouTube Music signed in, its cookies sign the app in as pasted ones do.

use raw_window_handle::HasWindowHandle;
use slint::ComponentHandle;
use url::Url;
use wry::{
    NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    WebViewBuilderExtWindows,
    dpi::{LogicalPosition, PhysicalSize},
};

use crate::SignInWindow;

const START_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&passive=true\
    &continue=https%3A%2F%2Fmusic.youtube.com%2F";
const MUSIC_URL: &str = "https://music.youtube.com/";
/// A signed-in session has one of these.
const SESSION_COOKIES: &[&str] = &["SAPISID", "__Secure-3PAPISID"];
/// Google sets __Secure-1PSIDRTS and __Secure-3PSIDRTS, which last minutes,
/// only on a session bound to the browser.
const BOUND_SESSION_COOKIE: &str = "PSIDRTS";
/// Turns off Device Bound Session Credentials. With them, WebView2 binds the
/// session to a key only it holds, and Google signs out the copy the app
/// keeps once the window closes and nothing refreshes it. The runtime ignores
/// feature names it lacks, so the list names each variant. Passing arguments
/// replaces wry's own, so its msWebOOUI, msPdfOOUI and msSmartScreenProtection
/// are repeated here; its autoplay switch is left out, as sign-in plays
/// nothing.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,\
    DeviceBoundSessions,DeviceBoundSessionsForRestrictedSites,EnableBoundSessionCredentials";

const MUSIC_HOST: &str = "music.youtube.com";
/// Sign-in and cookie consent hosts, allowed on any path.
const SIGN_IN_HOSTS: &[&str] = &[
    "accounts.youtube.com",
    "consent.google.com",
    "consent.youtube.com",
    "gds.google.com",
];
/// Sign-in hands over to YouTube Music through www.youtube.com/signin.
const HANDOFF_HOST: &str = "www.youtube.com";
const HANDOFF_PATH: &str = "/signin";
/// Google also sets cookies on regional accounts hosts, such as
/// accounts.google.co.uk.
const ACCOUNTS_SUBDOMAIN: &str = "accounts.";
/// From https://www.google.com/supported_domains, one `.google.<tld>` per line.
const GOOGLE_DOMAINS: &str = include_str!("google_domains.txt");

fn is_google_accounts_host(host: &str) -> bool {
    host.strip_prefix(ACCOUNTS_SUBDOMAIN).is_some_and(|domain| {
        GOOGLE_DOMAINS
            .lines()
            .any(|d| d.trim().strip_prefix('.') == Some(domain))
    })
}

/// Pages the sign-in window may show. WebView2 reports only top-level
/// navigations, so the frames inside Google's pages are unaffected.
fn is_allowed(url: &Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    match url.host_str() {
        Some(MUSIC_HOST) => true,
        Some(HANDOFF_HOST) => url.path() == HANDOFF_PATH,
        Some(host) => SIGN_IN_HOSTS.contains(&host) || is_google_accounts_host(host),
        None => false,
    }
}

fn on_navigation(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let allowed = is_allowed(&parsed);
    if !allowed {
        // Origin and path only: sign-in query strings carry session tokens.
        log::info!(
            "sign-in window: blocked {}{}",
            parsed.origin().ascii_serialization(),
            parsed.path()
        );
        open_in_browser(url);
    }
    allowed
}

/// Opens web links such as Google's help pages in the browser. Other
/// schemes are dropped, and so are sign-in hops, whose URLs carry tokens.
fn open_in_browser(url: &str) {
    let Ok(url) = Url::parse(url) else { return };
    let is_web = matches!(url.scheme(), "http" | "https");
    let is_sign_in = url
        .host_str()
        .is_some_and(|host| host.starts_with(ACCOUNTS_SUBDOMAIN));
    if is_web && !is_sign_in {
        // explorer.exe hands a URL to the default browser.
        if let Err(e) = std::process::Command::new("explorer")
            .arg(url.as_str())
            .spawn()
        {
            log::warn!("could not open a link in the browser: {e}");
        }
    }
}

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
            .with_additional_browser_args(BROWSER_ARGS)
            .with_bounds(bounds(&self.window))
            .with_on_page_load_handler(move |event, _| {
                if matches!(event, PageLoadEvent::Finished) {
                    on_page();
                }
            })
            .with_navigation_handler(|url| on_navigation(&url))
            .with_new_window_req_handler(|url, _| {
                open_in_browser(&url);
                NewWindowResponse::Deny
            })
            .with_download_started_handler(|_, _| false)
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
        if cookies
            .iter()
            .any(|c| c.name().ends_with(BOUND_SESSION_COOKIE))
        {
            // BROWSER_ARGS did not take: a runtime that renamed the feature,
            // WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS or a policy replacing
            // them, or an account that insists on binding. Kept rather than
            // refused, as it works until the session goes unrefreshed.
            log::warn!("Google bound the sign-in to the sign-in window, so it will lapse");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(url: &str) -> bool {
        is_allowed(&Url::parse(url).unwrap())
    }

    #[test]
    fn allows_the_sign_in_chain() {
        assert!(allowed("https://accounts.google.com/v3/signin/identifier"));
        assert!(allowed("https://accounts.google.co.uk/accounts/SetSID"));
        assert!(allowed("https://accounts.google.de/accounts/SetSID"));
        assert!(allowed("https://accounts.youtube.com/accounts/SetSID"));
        assert!(allowed("https://consent.youtube.com/m?continue=x"));
        assert!(allowed(
            "https://www.youtube.com/signin?action_handle_signin=true"
        ));
        assert!(allowed("https://music.youtube.com/"));
    }

    #[test]
    fn blocks_lookalike_hosts() {
        assert!(!allowed("https://accounts.google.evil.com/accounts/SetSID"));
        assert!(!allowed("https://accounts.google.co.uk.evil.com/"));
        assert!(!allowed("https://accounts.google/"));
        assert!(!allowed("https://myaccounts.google.com/"));
        assert!(!allowed("https://accounts.evil.com/"));
        assert!(!allowed("https://music.youtube.com.evil.com/"));
    }

    #[test]
    fn blocks_everything_else() {
        assert!(!allowed("http://accounts.google.com/"), "plain HTTP");
        assert!(!allowed("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert!(!allowed("https://www.youtube.com/signin/extra"));
        assert!(!allowed("https://www.google.co.uk/"));
        assert!(!allowed("https://support.google.com/accounts"));
        assert!(!allowed("https://example.com/"));
        assert!(!allowed("file:///C:/Windows/System32/calc.exe"));
        assert!(!on_navigation("javascript:alert(1)"));
        assert!(!on_navigation("not a url"));
    }
}
