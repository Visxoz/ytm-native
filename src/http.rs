//! The HTTP clients the app's own requests use, limited to the hosts YouTube
//! Music needs, and sharing connections per host.

use std::{ptr::null, sync::OnceLock, time::Duration};

use reqwest::{Url, redirect};
use windows_sys::{
    Win32::{
        System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
        },
        UI::{
            Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
    core::w,
};

/// Domains the app talks to, with their subdomains: YouTube's API and
/// player, the audio streams, and artwork.
const ALLOWED_DOMAINS: &[&str] = &[
    "youtube.com",
    "googlevideo.com",
    "ytimg.com",
    "googleusercontent.com",
    "ggpht.com",
    // Built-in artwork, such as Liked music's.
    "gstatic.com",
];
const MAX_REDIRECTS: usize = 10;
/// Idle connections are kept this long, with HTTP/2 pings meanwhile, so the
/// next song's requests a few minutes after the last skip the TCP and TLS
/// handshakes. A ping that goes unanswered drops the connection.
const POOL_IDLE: Duration = Duration::from_secs(5 * 60);
const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(30);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Opens a web link in the default browser, as the Windows shell does.
pub fn open_in_browser(url: &Url) {
    if !browsable(url) {
        log::warn!("refused to open a link in the browser");
        return;
    }
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let url = wide(url.as_str());
    // A browser this starts would otherwise keep the app's folder open as
    // its working directory, so the folder couldn't be removed.
    let dir =
        wide(&std::env::var("SystemRoot").map_or_else(|_| String::new(), |r| r + r"\System32"));
    // On its own thread, as the shell can wait on the browser to start.
    std::thread::spawn(move || {
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            // Asked for on threads without a message loop, such as this one.
            fMask: SEE_MASK_NOASYNC,
            lpVerb: w!("open"),
            lpFile: url.as_ptr(),
            lpDirectory: dir.as_ptr(),
            nShow: SW_SHOWNORMAL,
            ..Default::default()
        };
        let com = (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32;
        // SAFETY: COM is set up for this thread around the call, as
        // ShellExecuteEx asks, and taken down only if that succeeded. `info`
        // and the NUL-terminated strings it points at outlive the call.
        let error = unsafe {
            let started = CoInitializeEx(null(), com) >= 0;
            let error = (ShellExecuteExW(&mut info) == 0).then(std::io::Error::last_os_error);
            if started {
                CoUninitialize();
            }
            error
        };
        if let Some(e) = error {
            log::warn!("could not open a link in the browser: {e}");
        }
    });
}

/// Whether `url` may go to the browser. Only web links, as the shell runs
/// whatever it's given, and none with a `"`, which the url crate keeps in
/// hosts and which could break out of the quotes the shell puts around the
/// link in the browser's command line.
fn browsable(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && !url.as_str().contains('"')
}

/// Whether the app may request `url`: HTTPS on an allowed domain. URLs
/// from YouTube's responses, such as artwork and streams, are checked with
/// this before they are fetched, and every redirect is too.
pub fn allowed(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| within(&url, ALLOWED_DOMAINS))
}

/// Whether `url` is HTTPS on one of `domains` or a subdomain of one.
pub fn within(url: &Url, domains: &[&str]) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|host| {
            domains.iter().any(|domain| {
                host == *domain
                    || host
                        .strip_suffix(domain)
                        .is_some_and(|sub| sub.ends_with('.'))
            })
        })
}

/// Follows redirects only within `domains`; any other fails the request.
pub fn redirects(domains: &'static [&'static str]) -> redirect::Policy {
    redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if within(attempt.url(), domains) {
            attempt.follow()
        } else {
            let origin = attempt.url().origin().ascii_serialization();
            attempt.error(format!("blocked a redirect to {origin}"))
        }
    })
}

/// Connection reuse settings shared with rustypipe's client, which talks to
/// the same hosts.
pub fn keep_alive(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .pool_idle_timeout(POOL_IDLE)
        .http2_keep_alive_interval(KEEP_ALIVE_EVERY)
        .http2_keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
        .http2_keep_alive_while_idle(true)
}

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        keep_alive(reqwest::Client::builder())
            .timeout(Duration::from_secs(15))
            .redirect(redirects(ALLOWED_DOMAINS))
            .build()
            .expect("HTTP client builds")
    })
}

/// For the audio download thread. Audio is fetched in byte ranges, whose
/// lengths must be the bytes on the wire, so it asks for no compression.
/// Idle connections are kept as long as the async client's, so one the
/// next stream's warm-up opened is still there when it plays.
pub fn blocking_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .pool_idle_timeout(POOL_IDLE)
            .timeout(Duration::from_secs(30))
            .no_gzip()
            .no_brotli()
            .redirect(redirects(ALLOWED_DOMAINS))
            .build()
            .expect("HTTP client builds")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_youtube_hosts_over_https() {
        assert!(allowed("https://music.youtube.com/youtubei/v1/player"));
        assert!(allowed("https://www.youtube.com/s/player/abc/base.js"));
        assert!(allowed(
            "https://rr3---sn-abc.googlevideo.com/videoplayback?x=1"
        ));
        assert!(allowed("https://i.ytimg.com/vi/x/hqdefault.jpg"));
        assert!(allowed("https://lh3.googleusercontent.com/abc=w120-h120"));
        assert!(allowed("https://yt3.ggpht.com/abc=s88"));
        assert!(allowed(
            "https://www.gstatic.com/youtube/media/ytm/images/pbg/liked-music-@576.png"
        ));
    }

    #[test]
    fn blocks_everything_else() {
        assert!(!allowed("http://music.youtube.com/"), "plain HTTP");
        assert!(!allowed("https://youtube.com.evil.com/"));
        assert!(!allowed("https://evilyoutube.com/"));
        assert!(!allowed("https://example.com/image.jpg"));
        assert!(!allowed("https://accounts.google.com/"));
        assert!(!allowed("file:///C:/Windows/System32/calc.exe"));
        assert!(!allowed("not a url"));
    }

    #[test]
    fn opens_only_plain_web_links() {
        let browsable = |s: &str| browsable(&Url::parse(s).unwrap());
        assert!(browsable("https://www.last.fm/api/auth/?api_key=k&token=t"));
        assert!(browsable("http://example.com/a b?q=\"x\""), "escaped");
        assert!(!browsable("https://a\"b.com/"), "kept in the host");
        assert!(!browsable("file:///C:/Windows/System32/calc.exe"));
        assert!(!browsable("ms-settings:privacy"));
        assert!(!browsable("javascript:alert(1)"));
    }
}
