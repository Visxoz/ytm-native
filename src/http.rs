//! The HTTP clients the app's own requests use, limited to the hosts YouTube
//! Music needs, and sharing connections per host.

use std::{sync::OnceLock, time::Duration};

use reqwest::{Url, redirect};

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

/// Whether the app may request `url`: HTTPS on an allowed domain. URLs
/// from YouTube's responses, such as artwork and streams, are checked with
/// this before they are fetched, and every redirect is too.
pub fn allowed(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| allowed_url(&url))
}

fn allowed_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|host| {
            ALLOWED_DOMAINS.iter().any(|domain| {
                host == *domain
                    || host
                        .strip_suffix(domain)
                        .is_some_and(|sub| sub.ends_with('.'))
            })
        })
}

fn redirects() -> redirect::Policy {
    redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if allowed_url(attempt.url()) {
            attempt.follow()
        } else {
            let origin = attempt.url().origin().ascii_serialization();
            attempt.error(format!("blocked a redirect to {origin}"))
        }
    })
}

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(redirects())
            .build()
            .expect("HTTP client builds")
    })
}

/// For the audio download thread.
pub fn blocking_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(redirects())
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
}
