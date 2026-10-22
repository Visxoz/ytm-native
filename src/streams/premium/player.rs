//! YouTube's web player, cut down to its unscrambling code and run in
//! QuickJS.

use std::time::{Duration, Instant};

use rquickjs::{Context, Runtime};
use serde::{Deserialize, Serialize};

use super::{WEB_USER_AGENT, diag, trim};

const IFRAME_API: &str = "https://www.youtube.com/iframe_api";
const QUICKJS_STACK: usize = 2 * 1024 * 1024;
/// A solve takes about 25 ms and 1.6 MB.
const QUICKJS_MEMORY: usize = 32 * 1024 * 1024;
const QUICKJS_TIME: Duration = Duration::from_secs(2);

/// The cut-down player for one YouTube player version.
#[derive(Serialize, Deserialize, Clone)]
pub struct Player {
    pub id: String,
    /// Signature timestamp, which player requests must quote.
    pub sts: u32,
    code: String,
}

fn cache_path() -> std::path::PathBuf {
    crate::settings::data_dir().join("player.json")
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
        // YouTube's code gets about 20 times what a solve needs, so a
        // broken or hostile player fails instead of hanging or bloating.
        runtime.set_memory_limit(QUICKJS_MEMORY);
        let deadline = Instant::now() + QUICKJS_TIME;
        runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));
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

    #[test]
    fn stops_a_player_that_never_finishes() {
        let player = |code: &str| Player {
            id: "test".into(),
            sts: 1,
            code: code.into(),
        };
        let started = Instant::now();
        assert!(player("while (true) {}").solve("abc", None).is_err());
        assert!(
            started.elapsed() < QUICKJS_TIME * 2,
            "stopped at the time limit"
        );
        assert!(
            player("var a = []; while (true) a.push(new Array(1e5).fill(1));")
                .solve("abc", None)
                .is_err(),
            "stopped at the memory limit"
        );
    }
}
