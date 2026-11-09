//! Lyrics line by line, timed where YouTube has them so.
//!
//! The web client is given lyrics as one block of text. YouTube Music's
//! Android app is given them as lines, each with its start time when the
//! lyrics are synced, so the timed ones are asked for as that app.

use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};

const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse?prettyPrint=false";
const VERSION: &str = "7.21.50";
const USER_AGENT: &str =
    "com.google.android.apps.youtube.music/7.21.50 (Linux; U; Android 14) gzip";
const LYRICS_DATA: &str =
    "/elementRenderer/newElement/type/componentType/model/timedLyricsModel/lyricsData";

#[derive(Debug, Default, PartialEq)]
pub struct Lyrics {
    pub lines: Vec<String>,
    /// Each line's start in seconds when the lyrics are synced, else empty.
    pub starts: Vec<f64>,
    /// Where the lyrics come from, such as "Source: Musixmatch".
    pub source: String,
}

impl Lyrics {
    pub fn plain(body: &str, source: String) -> Self {
        Self {
            lines: body.lines().map(str::to_owned).collect(),
            starts: Vec::new(),
            source,
        }
    }
}

#[derive(Deserialize)]
struct Response<'a> {
    /// Kept raw, so the large `frameworkUpdates` beside it is only scanned.
    #[serde(borrow)]
    contents: &'a RawValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsData {
    timed_lyrics_data: Vec<Line>,
    #[serde(default)]
    source_message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    lyric_line: String,
    cue_range: Option<CueRange>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CueRange {
    start_time_milliseconds: String,
}

/// The lyrics `browse_id` names as the Android app is given them, or
/// `None` when the answer is not in the shape expected.
pub(super) async fn timed(browse_id: &str) -> Result<Option<Lyrics>, String> {
    let body = json!({
        "context": { "client": {
            "clientName": "ANDROID_MUSIC",
            "clientVersion": VERSION,
            "androidSdkVersion": 34,
            "hl": "en",
        }},
        "browseId": browse_id,
    });
    let text = crate::http::client()
        .post(BROWSE_URL)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    Ok(parse(&text))
}

fn parse(json: &str) -> Option<Lyrics> {
    let response: Response = serde_json::from_str(json).ok()?;
    let contents: Value = serde_json::from_str(response.contents.get()).ok()?;
    let data = LyricsData::deserialize(contents.pointer(LYRICS_DATA)?).ok()?;
    if data.timed_lyrics_data.is_empty() {
        return None;
    }
    // Synced only if every line has its time.
    let starts: Option<Vec<f64>> = data
        .timed_lyrics_data
        .iter()
        .map(|line| {
            let ms: u64 = line
                .cue_range
                .as_ref()?
                .start_time_milliseconds
                .parse()
                .ok()?;
            Some(ms as f64 / 1000.0)
        })
        .collect();
    Some(Lyrics {
        lines: data
            .timed_lyrics_data
            .into_iter()
            .map(|line| line.lyric_line)
            .collect(),
        starts: starts.unwrap_or_default(),
        source: data.source_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(lines: &str) -> String {
        format!(
            r#"{{"contents":{{"elementRenderer":{{"newElement":{{"type":{{"componentType":{{"model":{{"timedLyricsModel":{{"lyricsData":{{"timedLyricsData":[{lines}],"sourceMessage":"Source: Musixmatch"}}}}}}}}}}}}}}}},"frameworkUpdates":{{"big":[1,2,3]}}}}"#
        )
    }

    #[test]
    fn reads_synced_lines_with_their_starts() {
        let json = response(
            r#"{"lyricLine":"One","cueRange":{"startTimeMilliseconds":"1500","endTimeMilliseconds":"3000","metadata":{"id":"1"}}},
               {"lyricLine":"Two","cueRange":{"startTimeMilliseconds":"3000","endTimeMilliseconds":"4000"}}"#,
        );
        assert_eq!(
            parse(&json),
            Some(Lyrics {
                lines: vec!["One".into(), "Two".into()],
                starts: vec![1.5, 3.0],
                source: "Source: Musixmatch".into(),
            })
        );
    }

    #[test]
    fn untimed_lines_have_no_starts() {
        let json = response(
            r#"{"lyricLine":"One"},{"lyricLine":"Two","cueRange":{"startTimeMilliseconds":"3000"}}"#,
        );
        let lyrics = parse(&json).unwrap();
        assert_eq!(lyrics.lines, ["One", "Two"]);
        assert!(lyrics.starts.is_empty(), "synced only when every line is");
    }

    #[test]
    fn other_shapes_are_none() {
        assert_eq!(parse(&response("")), None, "no lines");
        assert_eq!(parse(r#"{"contents":{"sectionListRenderer":{}}}"#), None);
        assert_eq!(parse("not json"), None);
    }
}
