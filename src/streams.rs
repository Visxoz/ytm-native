//! Resolves playable stream URLs with yt-dlp.
//!
//! Everything else talks to YouTube directly, but stream URLs need YouTube's
//! player JavaScript solved, which changes every few weeks. yt-dlp keeps up
//! with that, so the app keeps a private copy and updates it.

use std::{
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

use serde::Deserialize;

const DOWNLOAD_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const UPDATE_EVERY: Duration = Duration::from_secs(3 * 24 * 60 * 60);
/// Opus is YouTube's best-quality audio; AAC is cheaper to decode.
const FORMAT_OPUS: &str = "bestaudio[acodec=opus]/bestaudio[ext=m4a]/bestaudio";
const FORMAT_AAC: &str = "bestaudio[ext=m4a]/bestaudio";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone)]
pub struct ResolvedStream {
    pub url: String,
    pub size: u64,
    pub mime: String,
    pub headers: Vec<(String, String)>,
    pub codec: String,
    pub bitrate_kbps: Option<f64>,
    pub duration: Option<f64>,
}

#[derive(Deserialize)]
struct Info {
    url: Option<String>,
    filesize: Option<u64>,
    ext: Option<String>,
    acodec: Option<String>,
    abr: Option<f64>,
    duration: Option<f64>,
    #[serde(default)]
    http_headers: std::collections::BTreeMap<String, String>,
}

fn bin_dir() -> PathBuf {
    crate::settings::data_dir().join("bin")
}

/// Path of the yt-dlp binary, preferring the app's own copy.
fn ytdlp_path() -> PathBuf {
    bin_dir().join("yt-dlp.exe")
}

/// Makes sure yt-dlp is installed and reasonably fresh. Blocking.
pub fn ensure_ytdlp() -> Result<(), String> {
    let path = ytdlp_path();
    if !path.exists() {
        log::info!("downloading yt-dlp");
        std::fs::create_dir_all(bin_dir()).map_err(|e| e.to_string())?;
        let bytes = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .and_then(|c| c.get(DOWNLOAD_URL).send())
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map_err(|e| format!("could not download yt-dlp: {e}"))?;
        let partial = path.with_extension("part");
        std::fs::write(&partial, &bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&partial, &path).map_err(|e| e.to_string())?;
        return Ok(());
    }

    let stale = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age > UPDATE_EVERY);
    if stale {
        log::info!("updating yt-dlp");
        let status = Command::new(&path)
            .arg("-U")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_ok() {
            // Touch so the next check waits another interval even when
            // yt-dlp was already current.
            let _ = std::fs::File::options()
                .append(true)
                .open(&path)
                .and_then(|f| f.set_modified(SystemTime::now()));
        }
    }
    Ok(())
}

/// Resolves the best audio stream of a video. Blocking, takes a few seconds.
pub fn resolve(
    video_id: &str,
    cookies: Option<&Path>,
    prefer_aac: bool,
) -> Result<ResolvedStream, String> {
    let mut command = Command::new(ytdlp_path());
    command
        .args(["-J", "--no-playlist", "--no-warnings", "-f"])
        .arg(if prefer_aac { FORMAT_AAC } else { FORMAT_OPUS })
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null());
    if let Some(cookies) = cookies {
        command.arg("--cookies").arg(cookies);
    }
    command.arg(format!("https://music.youtube.com/watch?v={video_id}"));

    let output = command
        .output()
        .map_err(|e| format!("could not run yt-dlp: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .rev()
            .find(|l| l.contains("ERROR"))
            .unwrap_or("yt-dlp failed")
            .trim_start_matches("ERROR: ")
            .to_owned();
        return Err(reason);
    }
    parse(&output.stdout)
}

fn parse(json: &[u8]) -> Result<ResolvedStream, String> {
    let info: Info =
        serde_json::from_slice(json).map_err(|e| format!("unexpected yt-dlp output: {e}"))?;
    let url = info.url.ok_or("yt-dlp returned no stream URL")?;
    let mime = match info.ext.as_deref() {
        Some("webm") => "audio/webm",
        Some("m4a" | "mp4") => "audio/mp4",
        _ => return Err("unsupported stream container".into()),
    };
    Ok(ResolvedStream {
        url,
        size: info.filesize.unwrap_or(0),
        mime: mime.into(),
        headers: info.http_headers.into_iter().collect(),
        codec: info.acodec.unwrap_or_default(),
        bitrate_kbps: info.abr,
        duration: info.duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ytdlp_json() {
        let json = br#"{"url":"https://rr1.googlevideo.com/videoplayback?x=1","filesize":6214234,
            "ext":"webm","acodec":"opus","abr":146.4,"http_headers":{"User-Agent":"UA"}}"#;
        let stream = parse(json).unwrap();
        assert_eq!(stream.size, 6214234);
        assert_eq!(stream.mime, "audio/webm");
        assert_eq!(stream.codec, "opus");
        assert_eq!(stream.headers, [("User-Agent".to_owned(), "UA".to_owned())]);
    }

    #[test]
    fn rejects_unknown_containers() {
        assert!(parse(br#"{"url":"https://x","ext":"mhtml"}"#).is_err());
        assert!(parse(br#"{"ext":"webm"}"#).is_err());
    }
}
