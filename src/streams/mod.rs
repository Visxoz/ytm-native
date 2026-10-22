//! Resolves playable stream URLs straight from YouTube's player endpoint.
//!
//! Most clients get scrambled URLs that need YouTube's player JavaScript to
//! unlock. A few get plain URLs, so asking as one of those keeps the whole
//! pipeline native. They are tried in order: YouTube tends to change one
//! client at a time, so a song keeps playing on the next while the first
//! is fixed. Premium streams go through `premium`.

pub mod premium;

use serde::Deserialize;
use serde_json::json;

const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";

/// A client YouTube hands plain URLs to, as yt-dlp describes it.
pub(crate) struct Client {
    pub(crate) name: &'static str,
    id: &'static str,
    version: &'static str,
    user_agent: &'static str,
    device_make: &'static str,
    device_model: &'static str,
    os_name: &'static str,
    os_version: &'static str,
    android_sdk: Option<u32>,
}

/// In order of trial. The first is the usual one; the rest only answer
/// once it fails, so they cost nothing while it works.
pub(crate) const CLIENTS: &[Client] = &[
    Client {
        name: "VISIONOS",
        id: "101",
        version: "1.02",
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 \
            (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
        device_make: "Apple",
        device_model: "RealityDevice17,1",
        os_name: "visionOS",
        os_version: "26.5.23O471",
        android_sdk: None,
    },
    Client {
        name: "ANDROID_VR",
        id: "28",
        version: "1.65.10",
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; \
            eureka-user Build/SQ3A.220605.009.A1) gzip",
        device_make: "Oculus",
        device_model: "Quest 3",
        os_name: "Android",
        os_version: "12L",
        android_sdk: Some(32),
    },
    Client {
        name: "IOS",
        id: "5",
        version: "20.10.4",
        user_agent: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
        device_make: "Apple",
        device_model: "iPhone16,2",
        os_name: "iPhone",
        os_version: "18.3.2.22D82",
        android_sdk: None,
    },
];

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

/// Fetches the player response and picks the best audio stream, from the
/// first client that gives one. The first client's reason is reported
/// when none does, as it is the usual one.
pub async fn resolve(video_id: &str, visitor_data: &str) -> Result<ResolvedStream, String> {
    let mut first_error = None;
    for client in CLIENTS {
        match resolve_with(client, video_id, visitor_data).await {
            Ok(stream) => {
                if first_error.is_some() {
                    log::info!("{video_id}: the {} client served the stream", client.name);
                }
                return Ok(stream);
            }
            Err(e) => {
                log::debug!("{video_id}: {} client: {e}", client.name);
                first_error.get_or_insert(e);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "no clients".into()))
}

/// Asks the player endpoint as one client.
pub(crate) async fn resolve_with(
    client: &Client,
    video_id: &str,
    visitor_data: &str,
) -> Result<ResolvedStream, String> {
    let mut context = json!({
        "clientName": client.name,
        "clientVersion": client.version,
        "deviceMake": client.device_make,
        "deviceModel": client.device_model,
        "userAgent": client.user_agent,
        "osName": client.os_name,
        "osVersion": client.os_version,
        "hl": "en",
        "visitorData": visitor_data,
    });
    if let Some(sdk) = client.android_sdk {
        context["androidSdkVersion"] = sdk.into();
    }
    let body = json!({
        "context": { "client": context },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let text = crate::http::client()
        .post(PLAYER_URL)
        .header(reqwest::header::USER_AGENT, client.user_agent)
        .header("Content-Type", "application/json")
        .header("Origin", "https://www.youtube.com")
        .header("X-YouTube-Client-Name", client.id)
        .header("X-YouTube-Client-Version", client.version)
        .header("X-Goog-Visitor-Id", visitor_data)
        .body(body.to_string())
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("player request failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("player request failed: {e}"))?;
    parse(&text, client.user_agent)
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

/// The best plain-URL stream of a player response. A client that answers
/// with scrambled URLs only counts as having none.
fn parse(json: &str, user_agent: &str) -> Result<ResolvedStream, String> {
    let response = parse_response(json)?;
    let format = best_audio(response.audio_formats()).ok_or("no playable audio stream")?;
    let url = format.url.clone().ok_or("no playable audio stream")?;
    Ok(response.stream(format, url, user_agent, false))
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

    const UA: &str = "test";

    #[test]
    fn prefers_opus() {
        let stream = parse(&response(&[AAC, OPUS, OPUS_DRC, VIDEO].join(",")), UA).unwrap();
        assert_eq!(stream.url, "https://a/251");
        assert_eq!(stream.mime, "audio/webm");
        assert_eq!(stream.size, 6214234);
        assert_eq!(stream.loudness_db, Some(4.5));
        assert_eq!(stream.duration, Some(369.641));
        assert!(!stream.premium);
        assert_eq!(stream.headers[0].1, UA, "downloads as the same client");
    }

    #[test]
    fn falls_back_to_aac() {
        let stream = parse(&response(&[AAC, VIDEO].join(",")), UA).unwrap();
        assert_eq!(stream.url, "https://a/140");
        assert_eq!(stream.codec, "AAC");
        assert_eq!(
            stream.loudness_db,
            Some(-2.5),
            "uses the player-wide loudness"
        );
    }

    #[test]
    fn scrambled_urls_count_as_none() {
        assert_eq!(
            parse(&response(PREMIUM), UA).unwrap_err(),
            "no playable audio stream"
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
        assert_eq!(parse(json, UA).unwrap_err(), "Sign in to confirm your age");
        assert_eq!(
            parse(&response(VIDEO), UA).unwrap_err(),
            "no playable audio stream"
        );
    }

    /// Asks YouTube itself whether every stream path still works: each
    /// plain-URL client, a byte of what it serves, and the player trimming
    /// the Premium path relies on. The canary workflow runs it daily, so a
    /// change on YouTube's side shows up before users notice.
    #[test]
    #[ignore = "talks to YouTube; run by the canary workflow"]
    fn canary_every_stream_path_still_works() {
        // Never going away.
        const VIDEO: &str = "dQw4w9WgXcQ";
        let rt = tokio::runtime::Runtime::new().expect("runtime starts");
        let failures = rt.block_on(async {
            let mut failures = Vec::new();
            let api = crate::api::Api::new().expect("client starts");
            let visitor = match api.visitor_data(true).await {
                Ok(visitor) => visitor,
                Err(e) => return vec![format!("visitor data: {e}")],
            };
            for client in CLIENTS {
                match resolve_with(client, VIDEO, &visitor).await {
                    Ok(stream) => match probe(&stream).await {
                        Ok(()) => eprintln!(
                            "{}: {} {} kbps, serves bytes",
                            client.name,
                            stream.codec,
                            stream.bitrate_kbps.unwrap_or(0.0).round()
                        ),
                        Err(e) => failures.push(format!("{}: resolved but {e}", client.name)),
                    },
                    Err(e) => failures.push(format!("{}: {e}", client.name)),
                }
            }
            match premium::load_player().await {
                Ok(player) => eprintln!("player {}: trims and solves", player.id),
                Err(e) => failures.push(format!("player: {e}")),
            }
            failures
        });
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    /// Fetches the first bytes of a stream, as a URL can resolve and still
    /// be refused by the CDN.
    async fn probe(stream: &ResolvedStream) -> Result<(), String> {
        let mut request = crate::http::client()
            .get(&stream.url)
            .header(reqwest::header::RANGE, "bytes=0-1023");
        for (name, value) in &stream.headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("the CDN answered {status}"));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        (!bytes.is_empty())
            .then_some(())
            .ok_or_else(|| "the CDN sent no bytes".to_owned())
    }
}
