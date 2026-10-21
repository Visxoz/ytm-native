//! Resolves playable stream URLs straight from YouTube's player endpoint.
//!
//! Most clients get scrambled URLs that need YouTube's player JavaScript to
//! unlock. The visionOS client gets plain URLs, so asking as that client keeps
//! the whole pipeline native. Premium streams go through `crate::premium`.

use std::{sync::OnceLock, time::Duration};

use serde::Deserialize;
use serde_json::json;

const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
const CLIENT_NAME: &str = "VISIONOS";
const CLIENT_ID: &str = "101";
const CLIENT_VERSION: &str = "1.02";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 \
    (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

#[derive(Debug, Clone)]
pub struct ResolvedStream {
    pub url: String,
    pub size: u64,
    pub mime: String,
    pub headers: Vec<(String, String)>,
    pub codec: String,
    pub bitrate_kbps: Option<f64>,
    pub duration: Option<f64>,
    pub loudness_db: Option<f32>,
    /// Came from the signed-in Premium path.
    pub premium: bool,
    /// YouTube's format number: 774 and 141 are the Premium ones.
    pub itag: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlayerResponse {
    playability_status: PlayabilityStatus,
    streaming_data: Option<StreamingData>,
    player_config: Option<PlayerConfig>,
}

#[derive(Deserialize)]
struct PlayabilityStatus {
    status: String,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamingData {
    #[serde(default)]
    adaptive_formats: Vec<Format>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayerConfig {
    audio_config: Option<AudioConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AudioConfig {
    loudness_db: Option<f32>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Format {
    pub itag: u32,
    pub url: Option<String>,
    /// `s`, `sp` and `url`, when the URL is scrambled.
    pub signature_cipher: Option<String>,
    mime_type: String,
    pub bitrate: u64,
    content_length: Option<String>,
    approx_duration_ms: Option<String>,
    loudness_db: Option<f32>,
    #[serde(default)]
    is_drc: bool,
    audio_track: Option<AudioTrack>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct AudioTrack {
    #[serde(default)]
    audio_is_default: bool,
}

impl Format {
    pub fn is_opus(&self) -> bool {
        self.mime_type.starts_with("audio/webm") && self.mime_type.contains("opus")
    }

    fn is_aac(&self) -> bool {
        self.mime_type.starts_with("audio/mp4")
    }

    fn has_url(&self) -> bool {
        self.url.is_some() || self.signature_cipher.is_some()
    }
}

impl PlayerResponse {
    pub fn audio_formats(&self) -> &[Format] {
        self.streaming_data
            .as_ref()
            .map_or(&[], |s| s.adaptive_formats.as_slice())
    }

    fn loudness_db(&self) -> Option<f32> {
        self.player_config
            .as_ref()
            .and_then(|c| c.audio_config.as_ref())
            .and_then(|a| a.loudness_db)
    }

    /// Builds the stream to play from one of this response's formats.
    pub fn stream(
        &self,
        format: &Format,
        url: String,
        user_agent: &str,
        premium: bool,
    ) -> ResolvedStream {
        ResolvedStream {
            url,
            size: format
                .content_length
                .as_deref()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
            mime: if format.is_opus() {
                "audio/webm"
            } else {
                "audio/mp4"
            }
            .into(),
            headers: vec![("User-Agent".into(), user_agent.into())],
            codec: if format.is_opus() { "Opus" } else { "AAC" }.into(),
            bitrate_kbps: Some(format.bitrate as f64 / 1000.0),
            duration: format
                .approx_duration_ms
                .as_deref()
                .and_then(|ms| ms.parse::<f64>().ok())
                .map(|ms| ms / 1000.0),
            loudness_db: format.loudness_db.or_else(|| self.loudness_db()),
            premium,
            itag: format.itag,
        }
    }
}

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
            .expect("HTTP client builds")
    })
}

/// Fetches the player response and picks the best audio stream.
pub async fn resolve(video_id: &str, visitor_data: &str) -> Result<ResolvedStream, String> {
    let body = json!({
        "context": {
            "client": {
                "clientName": CLIENT_NAME,
                "clientVersion": CLIENT_VERSION,
                "deviceMake": "Apple",
                "deviceModel": "RealityDevice17,1",
                "userAgent": USER_AGENT,
                "osName": "visionOS",
                "osVersion": "26.5.23O471",
                "hl": "en",
                "visitorData": visitor_data,
            }
        },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let text = http()
        .post(PLAYER_URL)
        .header("Content-Type", "application/json")
        .header("Origin", "https://www.youtube.com")
        .header("X-YouTube-Client-Name", CLIENT_ID)
        .header("X-YouTube-Client-Version", CLIENT_VERSION)
        .header("X-Goog-Visitor-Id", visitor_data)
        .body(body.to_string())
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("player request failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("player request failed: {e}"))?;
    parse(&text)
}

/// Parses a player response, failing with YouTube's reason if unplayable.
pub(crate) fn parse_response(json: &str) -> Result<PlayerResponse, String> {
    let response: PlayerResponse =
        serde_json::from_str(json).map_err(|e| format!("unexpected player response: {e}"))?;
    if response.playability_status.status != "OK" {
        return Err(response
            .playability_status
            .reason
            .clone()
            .unwrap_or_else(|| format!("not playable ({})", response.playability_status.status)));
    }
    Ok(response)
}

fn parse(json: &str) -> Result<ResolvedStream, String> {
    let response = parse_response(json)?;
    let format = best_audio(response.audio_formats()).ok_or("no playable audio stream")?;
    let url = format.url.clone().ok_or("no playable audio stream")?;
    Ok(response.stream(format, url, USER_AGENT, false))
}

/// The highest-bitrate Opus stream, else the best AAC. Skips dynamic-range
/// compressed variants and dubbed tracks.
pub(crate) fn best_audio(formats: &[Format]) -> Option<&Format> {
    let candidates = || {
        formats.iter().filter(|f| {
            f.has_url() && !f.is_drc && f.audio_track.as_ref().is_none_or(|t| t.audio_is_default)
        })
    };
    candidates()
        .filter(|f| f.is_opus())
        .max_by_key(|f| (f.bitrate, f.itag))
        .or_else(|| {
            candidates()
                .filter(|f| f.is_aac())
                .max_by_key(|f| f.bitrate)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(formats: &str) -> String {
        format!(
            r#"{{"playabilityStatus":{{"status":"OK"}},
                "streamingData":{{"adaptiveFormats":[{formats}]}},
                "playerConfig":{{"audioConfig":{{"loudnessDb":-2.5}}}}}}"#
        )
    }

    const AAC: &str = r#"{"itag":140,"url":"https://a/140","mimeType":"audio/mp4; codecs=\"mp4a.40.2\"",
        "bitrate":137064,"contentLength":"5983881","approxDurationMs":"369626"}"#;
    const OPUS: &str = r#"{"itag":251,"url":"https://a/251","mimeType":"audio/webm; codecs=\"opus\"",
        "bitrate":146460,"contentLength":"6214234","approxDurationMs":"369641","loudnessDb":4.5}"#;
    const OPUS_DRC: &str = r#"{"itag":251,"url":"https://a/drc","mimeType":"audio/webm; codecs=\"opus\"",
        "bitrate":150000,"isDrc":true}"#;
    const VIDEO: &str = r#"{"itag":248,"url":"https://a/248","mimeType":"video/webm; codecs=\"vp9\"",
        "bitrate":2000000}"#;
    const PREMIUM: &str = r#"{"itag":774,"signatureCipher":"s=abc&sp=sig&url=https%3A%2F%2Fa%2F774",
        "mimeType":"audio/webm; codecs=\"opus\"","bitrate":260000}"#;

    #[test]
    fn prefers_opus() {
        let stream = parse(&response(&[AAC, OPUS, OPUS_DRC, VIDEO].join(","))).unwrap();
        assert_eq!(stream.url, "https://a/251");
        assert_eq!(stream.mime, "audio/webm");
        assert_eq!(stream.size, 6214234);
        assert_eq!(stream.loudness_db, Some(4.5));
        assert_eq!(stream.duration, Some(369.641));
        assert!(!stream.premium);
    }

    #[test]
    fn falls_back_to_aac() {
        let stream = parse(&response(&[AAC, VIDEO].join(","))).unwrap();
        assert_eq!(stream.url, "https://a/140");
        assert_eq!(stream.codec, "AAC");
        assert_eq!(
            stream.loudness_db,
            Some(-2.5),
            "uses the player-wide loudness"
        );
    }

    #[test]
    fn picks_scrambled_premium_formats() {
        let response = parse_response(&response(&[OPUS, PREMIUM].join(","))).unwrap();
        let best = best_audio(response.audio_formats()).unwrap();
        assert_eq!(best.itag, 774);
        assert!(best.signature_cipher.is_some());
    }

    #[test]
    fn reports_unplayable_reason() {
        let json = r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Sign in to confirm your age"}}"#;
        assert_eq!(parse(json).unwrap_err(), "Sign in to confirm your age");
        assert_eq!(
            parse(&response(VIDEO)).unwrap_err(),
            "no playable audio stream"
        );
    }
}
