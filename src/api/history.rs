//! Reports plays and the time heard to YouTube, so they reach the
//! account's history and recommendations.

use std::{
    hash::{BuildHasher, Hasher, RandomState},
    time::Duration,
};

use rustypipe::client::ClientType;
use serde::Deserialize;
use url::Url;

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
    videostats_watchtime_url: Option<TrackingUrl>,
    #[serde(default)]
    videostats_scheduled_flush_walltime_seconds: Vec<u32>,
    videostats_default_flush_interval_seconds: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackingUrl {
    base_url: String,
}

fn tracking(json: &str) -> Option<Tracking> {
    serde_json::from_str::<Player>(json).ok()?.playback_tracking
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

/// A stats URL from the player response, for the play `cpn`.
fn stats_url(url: &str, cpn: &str) -> Result<Url> {
    let mut url = Url::parse(url).map_err(err)?;
    // It carries the sign-in cookie, so only to YouTube itself.
    if !crate::http::within(&url, &["youtube.com"]) {
        return Err("the stats URL is not on youtube.com".into());
    }
    url.query_pairs_mut()
        .append_pair("ver", "2")
        .append_pair("c", "WEB_REMIX")
        .append_pair("cpn", cpn);
    Ok(url)
}

/// Where a play's watch time goes, and when: the web player pings at the
/// scheduled seconds, then at the interval.
pub struct Watchtime {
    url: Url,
    scheduled: Vec<u32>,
    every: u32,
}

impl Watchtime {
    /// When ping `n` is due, since the play started, with ping `n - 1`
    /// sent at `sent`.
    pub fn due(&self, n: usize, sent: Duration) -> Duration {
        self.scheduled.get(n).map_or(
            sent + Duration::from_secs(self.every.max(1).into()),
            |&at| Duration::from_secs(at.into()),
        )
    }

    /// The ping reporting `watched`.
    pub fn url(&self, watched: &Watched) -> Url {
        let secs = |s: f64| format!("{s:.3}");
        let join = |end: fn(&(f64, f64)) -> f64| {
            let ends: Vec<String> = watched.segments.iter().map(|s| secs(end(s))).collect();
            ends.join(",")
        };
        let mut url = self.url.clone();
        {
            let mut query = url.query_pairs_mut();
            // The last ping may have nothing new to add.
            if !watched.segments.is_empty() {
                query
                    .append_pair("st", &join(|s| s.0))
                    .append_pair("et", &join(|s| s.1));
            }
            query
                .append_pair("cmt", &secs(watched.position))
                .append_pair("rt", &secs(watched.elapsed.as_secs_f64()))
                .append_pair("state", if watched.paused { "paused" } else { "playing" });
            if watched.last {
                query.append_pair("final", "1");
            }
        }
        url
    }
}

/// What a watch-time ping reports.
pub struct Watched {
    /// Stretches heard since the last ping, in song seconds.
    pub segments: Vec<(f64, f64)>,
    /// Where the sound got to.
    pub position: f64,
    /// Since the play started.
    pub elapsed: Duration,
    pub paused: bool,
    /// The play has ended.
    pub last: bool,
}

async fn ping(url: Url, cookie: &str) -> Result<()> {
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

impl Api {
    /// Adds `video_id` to the signed-in account's history, as YouTube
    /// Music's web player does: the account's player response for the song
    /// carries a stats URL, which is then requested with the session's
    /// cookie. Returns where the play's watch time goes, should it say.
    pub async fn report_play(&self, video_id: &str) -> Result<Option<Watchtime>> {
        let cookie = self.cookie().ok_or("signed out")?;
        let json = self
            .q()
            .authenticated()
            .raw(
                ClientType::DesktopMusic,
                "player",
                &serde_json::json!({ "videoId": super::video_id(video_id)? }),
            )
            .await
            .map_err(err)?;
        let tracking = tracking(&json).ok_or("the player response has no playback tracking")?;
        let playback = tracking
            .videostats_playback_url
            .ok_or("the player response has no playback stats URL")?;
        // Both pings carry the same nonce, which ties the watch time to the play.
        let cpn = cpn();
        ping(stats_url(&playback.base_url, &cpn)?, &cookie).await?;
        let Some(url) = tracking
            .videostats_watchtime_url
            .and_then(|u| stats_url(&u.base_url, &cpn).ok())
        else {
            return Ok(None);
        };
        let scheduled = match tracking.videostats_scheduled_flush_walltime_seconds {
            scheduled if scheduled.is_empty() => vec![10, 20, 30],
            scheduled => scheduled,
        };
        Ok(Some(Watchtime {
            url,
            scheduled,
            every: tracking
                .videostats_default_flush_interval_seconds
                .unwrap_or(40),
        }))
    }

    /// Sends a watch-time ping from `Watchtime::url`.
    pub async fn report_watchtime(&self, url: Url) -> Result<()> {
        let cookie = self.cookie().ok_or("signed out")?;
        ping(url, &cookie).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_stats_urls_and_schedule() {
        let json = r#"{"playabilityStatus":{"status":"OK"},"playbackTracking":{
            "videostatsPlaybackUrl":{"baseUrl":"https://s.youtube.com/api/stats/playback?ns=yt&docid=abc"},
            "videostatsWatchtimeUrl":{"baseUrl":"https://s.youtube.com/api/stats/watchtime?ns=yt"},
            "videostatsScheduledFlushWalltimeSeconds":[10,20,30],
            "videostatsDefaultFlushIntervalSeconds":40}}"#;
        let tracking = tracking(json).unwrap();
        assert_eq!(
            tracking.videostats_playback_url.unwrap().base_url,
            "https://s.youtube.com/api/stats/playback?ns=yt&docid=abc"
        );
        assert_eq!(
            tracking.videostats_watchtime_url.unwrap().base_url,
            "https://s.youtube.com/api/stats/watchtime?ns=yt"
        );
        assert_eq!(
            tracking.videostats_scheduled_flush_walltime_seconds,
            [10, 20, 30]
        );
        assert_eq!(tracking.videostats_default_flush_interval_seconds, Some(40));
        assert!(super::tracking(r#"{"playabilityStatus":{}}"#).is_none());
        assert!(super::tracking("not json").is_none());
    }

    #[test]
    fn sends_the_cookie_only_to_youtube() {
        assert!(stats_url("https://s.youtube.com/api/stats/playback", "x").is_ok());
        assert!(stats_url("https://evil.example/api/stats/playback", "x").is_err());
    }

    #[test]
    fn pings_on_the_schedule_then_at_the_interval() {
        let watch = Watchtime {
            url: Url::parse("https://s.youtube.com/api/stats/watchtime").unwrap(),
            scheduled: vec![10, 20, 30],
            every: 40,
        };
        let secs = Duration::from_secs;
        assert_eq!(watch.due(0, Duration::ZERO), secs(10));
        assert_eq!(
            watch.due(1, secs(5)),
            secs(20),
            "a pause ping keeps the schedule"
        );
        assert_eq!(watch.due(3, secs(31)), secs(71), "then 40 s after the last");
    }

    #[test]
    fn reports_each_stretch_heard() {
        let watch = Watchtime {
            url: stats_url("https://s.youtube.com/api/stats/watchtime?ns=yt", "nonce").unwrap(),
            scheduled: Vec::new(),
            every: 40,
        };
        let watched = |segments, last| Watched {
            segments,
            position: 64.25,
            elapsed: Duration::from_millis(20_500),
            paused: false,
            last,
        };
        let query = |watched| -> Vec<(String, String)> {
            watch.url(&watched).query_pairs().into_owned().collect()
        };
        let get = |query: &[(String, String)], k: &str| {
            query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        let ping = query(watched(vec![(0.0, 12.5), (60.0, 64.25)], false));
        assert_eq!(get(&ping, "cpn").as_deref(), Some("nonce"));
        assert_eq!(get(&ping, "st").as_deref(), Some("0.000,60.000"));
        assert_eq!(get(&ping, "et").as_deref(), Some("12.500,64.250"));
        assert_eq!(get(&ping, "cmt").as_deref(), Some("64.250"));
        assert_eq!(get(&ping, "rt").as_deref(), Some("20.500"));
        assert_eq!(get(&ping, "state").as_deref(), Some("playing"));
        assert_eq!(get(&ping, "final"), None);

        let last = query(watched(Vec::new(), true));
        assert_eq!(get(&last, "final").as_deref(), Some("1"));
        assert_eq!(get(&last, "st"), None, "nothing new heard");
        assert_eq!(get(&last, "cmt").as_deref(), Some("64.250"));
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
