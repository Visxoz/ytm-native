//! Reporting plays to YouTube, so songs played here show up in the
//! account's YouTube Music history and shape its recommendations.

use std::hash::{BuildHasher, Hasher, RandomState};

use rustypipe::client::ClientType;
use serde::Deserialize;

use super::{Api, Result, err};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Player {
    playback_tracking: Option<Tracking>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tracking {
    videostats_playback_url: Option<TrackingUrl>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackingUrl {
    base_url: String,
}

/// The stats URL the web player pings as a song starts.
fn playback_url(json: &str) -> Option<String> {
    let player: Player = serde_json::from_str(json).ok()?;
    Some(player.playback_tracking?.videostats_playback_url?.base_url)
}

/// A client playback nonce: 16 random characters the web player sends
/// with each ping, from the same alphabet.
fn cpn() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let state = RandomState::new();
    (0..16)
        .map(|i| {
            let mut hasher = state.build_hasher();
            hasher.write_usize(i);
            char::from(ALPHABET[(hasher.finish() % 64) as usize])
        })
        .collect()
}

impl Api {
    /// Adds `video_id` to the signed-in account's history, as YouTube
    /// Music's web player does: the account's player response for the song
    /// carries a stats URL, which is then requested with its `cookie`.
    pub async fn report_play(&self, video_id: &str, cookie: &str) -> Result<()> {
        let json = self
            .q()
            .authenticated()
            .raw(
                ClientType::DesktopMusic,
                "player",
                &serde_json::json!({ "videoId": video_id }),
            )
            .await
            .map_err(err)?;
        let url = playback_url(&json).ok_or("the player response has no stats URL")?;
        let mut url = url::Url::parse(&url).map_err(err)?;
        // It carries the sign-in cookie, so only to YouTube itself.
        if !crate::http::within(&url, &["youtube.com"]) {
            return Err("the stats URL is not on youtube.com".into());
        }
        url.query_pairs_mut()
            .append_pair("ver", "2")
            .append_pair("c", "WEB_REMIX")
            .append_pair("cpn", &cpn());
        crate::http::client()
            .get(url)
            .header(
                reqwest::header::USER_AGENT,
                crate::streams::premium::WEB_USER_AGENT,
            )
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            // Its URL carries the session's tracking tokens.
            .map_err(|e| err(e.without_url()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_stats_url() {
        let json = r#"{"playabilityStatus":{"status":"OK"},"playbackTracking":{
            "videostatsPlaybackUrl":{"baseUrl":"https://s.youtube.com/api/stats/playback?ns=yt&docid=abc"},
            "videostatsWatchtimeUrl":{"baseUrl":"https://s.youtube.com/api/stats/watchtime?ns=yt"}}}"#;
        assert_eq!(
            playback_url(json).as_deref(),
            Some("https://s.youtube.com/api/stats/playback?ns=yt&docid=abc")
        );
        assert_eq!(playback_url(r#"{"playabilityStatus":{}}"#), None);
        assert_eq!(playback_url("not json"), None);
    }

    #[test]
    fn makes_nonces_like_the_web_player() {
        let (a, b) = (cpn(), cpn());
        assert_eq!(a.len(), 16);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_ne!(a, b, "random each time");
    }
}
