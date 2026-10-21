//! Premium audio (256 kbps Opus) for signed-in YouTube Premium accounts.
//!
//! YouTube offers Premium formats only to signed-in web clients, whose URLs
//! are scrambled by its player JavaScript. Once per player version the
//! player is cut down natively (see `trim`) and cached; each song then
//! unscrambles its URL in a throwaway QuickJS context, so nothing stays in
//! memory between songs.

mod trim;

use std::{
    io::Write,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use rquickjs::{Context, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha1::{Digest, Sha1};

use crate::streams::{self, ResolvedStream};

const IFRAME_API: &str = "https://www.youtube.com/iframe_api";
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
/// Formats only Premium accounts get: 256 kbps Opus and AAC.
const PREMIUM_ITAGS: &[u32] = &[774, 141];
const QUICKJS_STACK: usize = 2 * 1024 * 1024;
const LOG_LIMIT: u64 = 256 * 1024;

/// Web clients that skip PO tokens for Premium accounts, in order of trial.
struct Client {
    name: &'static str,
    id: &'static str,
    version: &'static str,
    host: &'static str,
}

const CLIENTS: &[Client] = &[
    Client {
        name: "WEB_REMIX",
        id: "67",
        version: "1.20260707.12.00",
        host: "music.youtube.com",
    },
    Client {
        name: "WEB_CREATOR",
        id: "62",
        version: "1.20260708.06.00",
        host: "www.youtube.com",
    },
];

/// The cut-down player for one YouTube player version.
#[derive(Serialize, Deserialize, Clone)]
pub struct Player {
    pub id: String,
    /// Signature timestamp, which player requests must quote.
    pub sts: u32,
    code: String,
}

pub enum Outcome {
    Premium(ResolvedStream),
    /// No Premium version of this song: music videos and uploads often
    /// have none, so this says nothing about the account on its own.
    NoPremiumFormats,
}

fn cache_path() -> std::path::PathBuf {
    crate::settings::data_dir().join("player.json")
}

fn log_path() -> std::path::PathBuf {
    crate::settings::data_dir().join("premium.log")
}

/// Appends a line to `premium.log`, for checking what the Premium path did.
/// Never records cookies or URLs.
pub fn diag(line: impl AsRef<str>) {
    let path = log_path();
    let truncate = std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_LIMIT);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(!truncate)
        .write(true)
        .truncate(truncate)
        .open(&path);
    if let Ok(mut file) = file {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = writeln!(file, "[{secs}] {}", line.as_ref());
    }
    log::info!("{}", line.as_ref());
}

fn player_id(iframe_api: &str) -> Option<String> {
    for marker in [r"player\/", "player/"] {
        if let Some(start) = iframe_api.find(marker) {
            let id: String = iframe_api[start + marker.len()..].chars().take(8).collect();
            if id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(id);
            }
        }
    }
    None
}

fn signature_timestamp(player: &str) -> Option<u32> {
    ["signatureTimestamp:", "sts:"].iter().find_map(|marker| {
        let start = player.find(marker)? + marker.len();
        let digits: String = player[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

async fn text(url: &str) -> Result<String, String> {
    crate::http::client()
        .get(url)
        .header(reqwest::header::USER_AGENT, WEB_USER_AGENT)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("could not fetch {url}: {e}"))?
        .text()
        .await
        .map_err(|e| e.to_string())
}

/// The cut-down current player, from the cache unless YouTube updated it.
pub async fn load_player() -> Result<Player, String> {
    let iframe = text(IFRAME_API).await?;
    let id = player_id(&iframe).ok_or("could not find the player version")?;
    if let Some(cached) = std::fs::read(cache_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Player>(&bytes).ok())
        .filter(|p| p.id == id)
    {
        return Ok(cached);
    }

    let started = Instant::now();
    let source = text(&format!(
        "https://www.youtube.com/s/player/{id}/player_ias.vflset/en_US/base.js"
    ))
    .await?;
    let fetched = started.elapsed();
    let sts = signature_timestamp(&source).ok_or("could not find the signature timestamp")?;
    let (code, trimmed) = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        trim::trim(&source).map(|code| (code, started.elapsed()))
    })
    .await
    .map_err(|e| e.to_string())??;

    // Check the result works before trusting it.
    let player = Player { id, sts, code };
    let check = player.clone();
    let solved = tokio::task::spawn_blocking(move || check.solve("abcdefghijklmnop", None))
        .await
        .map_err(|e| e.to_string())??;
    if solved.0 == "abcdefghijklmnop" {
        return Err("the player's n function did nothing".into());
    }
    diag(format!(
        "player {}: fetched in {} ms, trimmed to {} KB in {} ms",
        player.id,
        fetched.as_millis(),
        player.code.len() / 1024,
        trimmed.as_millis()
    ));
    if let Ok(json) = serde_json::to_vec(&player) {
        let _ = std::fs::write(cache_path(), json);
    }
    Ok(player)
}

impl Player {
    /// Unscrambles an `n` value and optionally a signature, in a throwaway
    /// QuickJS context. Blocking; takes about 20 ms.
    pub fn solve(&self, n: &str, sig: Option<&str>) -> Result<(String, Option<String>), String> {
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        runtime.set_max_stack_size(QUICKJS_STACK);
        let context = Context::full(&runtime).map_err(|e| e.to_string())?;
        context.with(|ctx| {
            let fail = |e: rquickjs::Error| {
                let exception = ctx.catch();
                match exception.as_exception().and_then(|x| x.message()) {
                    Some(message) => format!("solver failed: {message}"),
                    None => format!("solver failed: {e}"),
                }
            };
            ctx.globals()
                .set("__code", self.code.as_str())
                .map_err(fail)?;
            ctx.eval::<(), _>(
                "globalThis.__s = {n: null, sig: null}; Function('_result', __code)(__s); delete globalThis.__code;",
            )
            .map_err(fail)?;
            let call = |f: &str, input: &str| -> Result<String, String> {
                let input = serde_json::to_string(input).map_err(|e| e.to_string())?;
                ctx.eval::<String, _>(format!("__s.{f}({input})")).map_err(fail)
            };
            let n = call("n", n)?;
            let sig = sig.map(|s| call("sig", s)).transpose()?;
            Ok((n, sig))
        })
    }
}

fn cookie_value<'c>(cookie: &'c str, name: &str) -> Option<&'c str> {
    cookie.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

/// The `Authorization` header YouTube's web player sends: SHA-1 hashes of
/// the session cookies, the time and the origin.
fn authorization(cookie: &str, origin: &str, now: u64) -> Option<String> {
    let sapisid =
        cookie_value(cookie, "SAPISID").or_else(|| cookie_value(cookie, "__Secure-3PAPISID"));
    let schemes = [
        ("SAPISIDHASH", sapisid),
        ("SAPISID1PHASH", cookie_value(cookie, "__Secure-1PAPISID")),
        ("SAPISID3PHASH", cookie_value(cookie, "__Secure-3PAPISID")),
    ];
    let parts: Vec<String> = schemes
        .iter()
        .filter_map(|(scheme, sid)| {
            let sid = (*sid)?;
            let digest = Sha1::digest(format!("{now} {sid} {origin}").as_bytes());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            Some(format!("{scheme} {now}_{hex}"))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

async fn player_request(
    client: &Client,
    video_id: &str,
    cookie: Option<&str>,
    visitor_data: &str,
    sts: u32,
) -> Result<streams::PlayerResponse, String> {
    let origin = format!("https://{}", client.host);
    let body = json!({
        "context": {
            "client": {
                "clientName": client.name,
                "clientVersion": client.version,
                "hl": "en",
                "visitorData": visitor_data,
            }
        },
        "playbackContext": {
            "contentPlaybackContext": {
                "html5Preference": "HTML5_PREF_WANTS",
                "signatureTimestamp": sts,
            }
        },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let mut request = crate::http::client()
        .post(format!("{origin}/youtubei/v1/player?prettyPrint=false"))
        .header(reqwest::header::USER_AGENT, WEB_USER_AGENT)
        .header("Content-Type", "application/json")
        .header("Origin", &origin)
        .header("X-Origin", &origin)
        .header("X-YouTube-Client-Name", client.id)
        .header("X-YouTube-Client-Version", client.version)
        .header("X-Goog-Visitor-Id", visitor_data);
    if let Some(cookie) = cookie {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        request = request
            .header("Cookie", cookie)
            .header("X-Goog-AuthUser", "0");
        if let Some(auth) = authorization(cookie, &origin, now) {
            request = request.header("Authorization", auth);
        }
    }
    let text = request
        .body(body.to_string())
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("{} player request failed: {e}", client.name))?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    streams::parse_response(&text)
}

/// Rewrites a stream URL with its unscrambled `n` and signature.
fn unscramble(format: &streams::Format, player: &Player) -> Result<String, String> {
    let (mut url, sig) = match (&format.url, &format.signature_cipher) {
        (Some(url), _) => (url::Url::parse(url).map_err(|e| e.to_string())?, None),
        (None, Some(cipher)) => {
            let fields: std::collections::HashMap<String, String> =
                url::form_urlencoded::parse(cipher.as_bytes())
                    .into_owned()
                    .collect();
            let url = fields.get("url").ok_or("cipher without URL")?;
            let s = fields.get("s").ok_or("cipher without signature")?.clone();
            let sp = fields
                .get("sp")
                .cloned()
                .unwrap_or_else(|| "signature".into());
            (
                url::Url::parse(url).map_err(|e| e.to_string())?,
                Some((s, sp)),
            )
        }
        (None, None) => return Err("format without URL".into()),
    };
    let n = url
        .query_pairs()
        .find(|(k, _)| k == "n")
        .map(|(_, v)| v.into_owned());
    let (solved_n, solved_sig) = match &n {
        Some(n) => player.solve(n, sig.as_ref().map(|(s, _)| s.as_str()))?,
        None => (String::new(), None),
    };
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| {
            let value = if k == "n" {
                solved_n.clone()
            } else {
                v.into_owned()
            };
            (k.into_owned(), value)
        })
        .collect();
    url.query_pairs_mut().clear().extend_pairs(pairs);
    if let (Some((_, sp)), Some(signature)) = (sig, solved_sig) {
        url.query_pairs_mut().append_pair(&sp, &signature);
    }
    Ok(url.into())
}

/// Resolves a Premium stream. `cookie` is `None` only for dry runs, which
/// time the path without a Premium account.
pub async fn resolve(
    video_id: &str,
    cookie: Option<&str>,
    visitor_data: &str,
    player: &Arc<Player>,
) -> Result<Outcome, String> {
    let started = Instant::now();
    let mut last_error = String::from("no clients");
    for client in CLIENTS {
        let response =
            match player_request(client, video_id, cookie, visitor_data, player.sts).await {
                Ok(response) => response,
                Err(e) => {
                    diag(format!("{video_id}: {} failed: {e}", client.name));
                    last_error = e;
                    continue;
                }
            };
        let requested = started.elapsed();
        let itags: Vec<u32> = response.audio_formats().iter().map(|f| f.itag).collect();
        let premium = itags.iter().any(|i| PREMIUM_ITAGS.contains(i));
        if !premium && cookie.is_some() {
            diag(format!(
                "{video_id}: {} has no Premium version ({itags:?}); playing the free stream",
                client.name
            ));
            return Ok(Outcome::NoPremiumFormats);
        }
        let Some(format) = streams::best_audio(response.audio_formats()).cloned() else {
            last_error = "no audio formats".into();
            continue;
        };
        let solve_player = Arc::clone(player);
        let solve_format = format.clone();
        let solve_started = Instant::now();
        let url = tokio::task::spawn_blocking(move || unscramble(&solve_format, &solve_player))
            .await
            .map_err(|e| e.to_string())??;
        diag(format!(
            "{video_id}: {} itag {} ({} kbps) of {itags:?}; request {} ms, unscramble {} ms",
            client.name,
            format.itag,
            format.bitrate / 1000,
            requested.as_millis(),
            solve_started.elapsed().as_millis()
        ));
        return Ok(Outcome::Premium(response.stream(
            &format,
            url,
            WEB_USER_AGENT,
            true,
        )));
    }
    Err(last_error)
}

/// Turns a pasted `Cookie:` header or cookies.txt into a `Cookie` header.
pub fn cookie_header(pasted: &str) -> String {
    let pasted = pasted.trim();
    if pasted.contains('\t') {
        return pasted
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|line| {
                let fields: Vec<&str> = line.split('\t').collect();
                (fields.len() >= 7 && fields[0].contains("youtube.com"))
                    .then(|| format!("{}={}", fields[5], fields[6].trim()))
            })
            .collect::<Vec<_>>()
            .join("; ");
    }
    pasted
        .strip_prefix("Cookie:")
        .unwrap_or(pasted)
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_player_version_and_timestamp() {
        assert_eq!(
            player_id(r#"...trustedScriptURL("https:\/\/www.youtube.com\/s\/player\/67288f35\/www-widgetapi.vflset")"#).as_deref(),
            Some("67288f35")
        );
        assert_eq!(player_id("nothing here"), None);
        assert_eq!(
            signature_timestamp("x={signatureTimestamp:20744,y:1}"),
            Some(20744)
        );
    }

    #[test]
    fn builds_sapisid_authorization() {
        let cookie = "SID=a; SAPISID=abc; __Secure-3PAPISID=def";
        let auth = authorization(cookie, "https://music.youtube.com", 1_700_000_000).unwrap();
        // sha1("1700000000 abc https://music.youtube.com")
        assert!(auth.starts_with("SAPISIDHASH 1700000000_"));
        assert!(auth.contains(" SAPISID3PHASH 1700000000_"));
        assert!(!auth.contains("SAPISID1PHASH"), "no 1P cookie, no 1P hash");
        assert!(authorization("SID=a", "https://x", 0).is_none());
    }

    #[test]
    fn converts_pasted_cookies() {
        assert_eq!(cookie_header("Cookie: SID=a; HSID=b"), "SID=a; HSID=b");
        let txt = "# Netscape\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\ta\n.google.com\tTRUE\t/\tTRUE\t0\tNID\tz\n";
        assert_eq!(cookie_header(txt), "SID=a");
    }

    #[test]
    fn solves_with_a_trimmed_player() {
        let source = r#"var _yt_player={};(function(g){var window=this;
var helper = function(x) { return x.split("").reverse().join(""); };
function Url(base) { this.params = {}; }
Url.prototype.set = function(k, v) { this.params[k] = v; };
Url.prototype.get = function(k) { return this.params[k]; };
Url.prototype.clone = function() { return this; };
Url.prototype.unscramble = function() { if (this.params.n) this.params.n = helper(this.params.n); if (this.params.s) this.params.s = encodeURIComponent(helper(decodeURIComponent(this.params.s))); };
var marker = { mark: function() {} };
g.make = function(base, key, value) { marker.mark("alr", "yes"); var u = new Url(base); if (value !== undefined) u.set(key, value); return u; };
})(_yt_player);"#;
        let player = Player {
            id: "test".into(),
            sts: 1,
            code: trim::trim(source).unwrap(),
        };
        let (n, sig) = player.solve("abc", Some("x/y")).unwrap();
        assert_eq!(n, "cba");
        assert_eq!(sig.as_deref(), Some("y/x"));
    }
}
