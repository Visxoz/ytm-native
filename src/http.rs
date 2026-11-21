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
/// Idle connections are kept, with HTTP/2 pings, so the next song's requests
/// minutes later skip the TCP and TLS handshakes.
const POOL_IDLE: Duration = Duration::from_secs(5 * 60);
const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(30);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Only web links, as the shell runs whatever it's given, and none with a `"`,
/// which the url crate keeps in hosts and could break out of the browser's
/// command-line quotes.
fn browsable(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && !url.as_str().contains('"')
}

/// HTTPS on an allowed domain. Every URL from YouTube's responses is checked
/// with this before it is fetched.
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

/// The user's own proxy, read once at startup by `load_proxy`, as the
/// clients are built once. Unset, they follow Windows' proxy setting.
// shortcut: reqwest reads only Windows' manual "Use a proxy server" address,
// not a setup script, automatic detection or per-protocol servers; PAC would
// need WinHTTP's WinHttpGetProxyForUrl.
static PROXY: OnceLock<(Url, reqwest::Proxy)> = OnceLock::new();

fn proxy_path() -> std::path::PathBuf {
    crate::settings::data_dir().join("proxy.bin")
}

/// Takes the saved proxy into use. Encrypted, as its address may hold a
/// password.
pub fn load_proxy() {
    let Some(saved) = crate::secret::read(&proxy_path()) else {
        return;
    };
    match parse_proxy(&saved) {
        Ok(proxy) => {
            log::info!(
                "connecting through the proxy at {}",
                proxy_address(&proxy.0)
            );
            let _ = PROXY.set(proxy);
        }
        Err(e) => log::warn!("ignoring the saved proxy: {e}"),
    }
}

/// Saves `text` as the proxy for the next start, or none when it is empty.
/// Returns its address without any password, and whether it differs from
/// the one in use, credentials included.
pub fn save_proxy(text: &str) -> Result<(Option<String>, bool), &'static str> {
    let url = if text.trim().is_empty() {
        match std::fs::remove_file(proxy_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err("Couldn't clear the saved proxy.");
            }
            _ => None,
        }
    } else {
        let (url, _) = parse_proxy(text)?;
        crate::secret::write(&proxy_path(), url.as_str())
            .map_err(|_| "Couldn't save the proxy.")?;
        Some(url)
    };
    let pending = PROXY.get().map(|(u, _)| u) != url.as_ref();
    Ok((url.as_ref().map(proxy_address), pending))
}

/// The proxy in use, without any password.
pub fn proxy_in_use() -> Option<String> {
    PROXY.get().map(|(url, _)| proxy_address(url))
}

/// WebView2's flag for the proxy in use.
pub fn proxy_server_arg() -> Option<String> {
    PROXY.get().map(|(url, _)| proxy_server_flag(url))
}

/// Built from the parsed parts, never the text typed, as it joins a command
/// line. Chromium takes no password, and resolves names through SOCKS5 itself.
fn proxy_server_flag(url: &Url) -> String {
    let address = proxy_address(url).replacen("socks5h://", "socks5://", 1);
    format!("--proxy-server={address}")
}

/// A proxy address as typed: HTTP, HTTPS or SOCKS5, with a host and a port.
/// Without a scheme, HTTP.
fn parse_proxy(text: &str) -> Result<(Url, reqwest::Proxy), &'static str> {
    let text = text.trim();
    // A sign-in's @, # or / otherwise end the user name or password early.
    let bad = if text.contains('@') {
        "That isn't a proxy address. Write @, # or / in a user name or password as %40, %23 or %2F."
    } else {
        "That isn't a proxy address, like http://127.0.0.1:8080."
    };
    let url = if text.contains("://") {
        Url::parse(text)
    } else {
        Url::parse(&format!("http://{text}"))
    }
    .map_err(|_| bad)?;
    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") {
        return Err("Use an http:// or socks5:// address.");
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("The address needs a host.");
    }
    if url.port_or_known_default().is_none() {
        return Err("The address needs a port, like :1080.");
    }
    // hyper-util parses it again as an `http::Uri`, and goes direct should
    // that fail.
    if url.as_str().parse::<::http::Uri>().is_err() {
        return Err(bad);
    }
    let proxy = reqwest::Proxy::all(url.as_str()).map_err(|_| bad)?;
    Ok((url, proxy))
}

/// Scheme, host and port, leaving out any user name and password.
fn proxy_address(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or_default();
    format!("{}://{host}:{port}", url.scheme())
}

/// The user's proxy, should they have set one; otherwise reqwest follows
/// Windows' setting and the `HTTPS_PROXY` variables.
pub fn proxied(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    match PROXY.get() {
        Some((_, proxy)) => builder.proxy(proxy.clone()),
        None => builder,
    }
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
        proxied(keep_alive(reqwest::Client::builder()))
            .timeout(Duration::from_secs(15))
            .redirect(redirects(ALLOWED_DOMAINS))
            .build()
            .expect("HTTP client builds")
    })
}

/// For the audio download thread. No compression, as byte ranges must be the
/// bytes on the wire; the long idle pool keeps the warm-up's connection.
pub fn blocking_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let builder = match PROXY.get() {
            Some((_, proxy)) => reqwest::blocking::Client::builder().proxy(proxy.clone()),
            None => reqwest::blocking::Client::builder(),
        };
        builder
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
    fn takes_proxy_addresses_with_a_host_and_port() {
        let address = |text| parse_proxy(text).map(|(url, _)| proxy_address(&url));
        assert_eq!(address("127.0.0.1:8080").unwrap(), "http://127.0.0.1:8080");
        assert_eq!(
            address(" socks5h://user:secret@proxy.example:1080 ").unwrap(),
            "socks5h://proxy.example:1080",
            "no password shown"
        );
        assert_eq!(address("https://[::1]:3128").unwrap(), "https://[::1]:3128");
        assert_eq!(address("http://proxy").unwrap(), "http://proxy:80");
        assert!(address("ftp://proxy:21").is_err());
        assert!(address("socks5://proxy").is_err(), "no port to assume");
        assert!(address("http://:8080").is_err());
        assert!(address("not a proxy").is_err());
        assert!(address("socks5://prøxy.lan:1080").is_err(), "not a URI");
        assert!(
            address("http://user:pa#ss@proxy:8080")
                .unwrap_err()
                .contains("%23"),
            "says how to write a password's #"
        );
        assert!(address("http://user:pa%23ss@proxy:8080").is_ok());
    }

    #[test]
    fn gives_webview2_the_proxy_without_its_password() {
        let flag = |text| proxy_server_flag(&parse_proxy(text).unwrap().0);
        assert_eq!(
            flag("socks5h://user:secret@127.0.0.1:1080"),
            "--proxy-server=socks5://127.0.0.1:1080"
        );
        assert_eq!(flag("10.0.0.1:3128"), "--proxy-server=http://10.0.0.1:3128");
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
