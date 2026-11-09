//! Changes to the signed-in account's music: likes, dislikes and its own
//! playlists, through the same calls YouTube Music's web player makes.

use rustypipe::client::ClientType;
use serde_json::{Value, json};

use super::{Api, Result, err};

/// Video IDs YouTube takes in one playlist call.
const PER_REQUEST: usize = 100;

/// How the account rated a song.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Rating {
    #[default]
    None,
    Like,
    Dislike,
}

impl Rating {
    /// 0 none, 1 like, 2 dislike, as the UI has it.
    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Rating::Like,
            2 => Rating::Dislike,
            _ => Rating::None,
        }
    }
}

/// One of the account's own playlists, offered for adding a song to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistOption {
    pub id: String,
    pub title: String,
    /// Already holds the song.
    pub contains: bool,
}

impl Api {
    async fn edit(&self, endpoint: &str, body: &Value) -> Result<String> {
        self.q()
            .authenticated()
            .raw(ClientType::DesktopMusic, endpoint, body)
            .await
            .map_err(err)
    }

    /// The account's rating of `video_id`.
    pub async fn rating(&self, video_id: &str) -> Result<Rating> {
        let json = self.edit("next", &json!({ "videoId": video_id })).await?;
        Ok(parse_rating(&json))
    }

    pub async fn rate(&self, video_id: &str, rating: Rating) -> Result<()> {
        let endpoint = match rating {
            Rating::Like => "like/like",
            Rating::Dislike => "like/dislike",
            Rating::None => "like/removelike",
        };
        self.edit(endpoint, &json!({ "target": { "videoId": video_id } }))
            .await
            .map(drop)
    }

    /// The account's own playlists, marking those that hold `video_id`.
    pub async fn playlists_for(&self, video_id: &str) -> Result<Vec<PlaylistOption>> {
        let json = self
            .edit(
                "playlist/get_add_to_playlist",
                &json!({ "videoIds": [video_id] }),
            )
            .await?;
        parse_playlists(&json).ok_or_else(|| "YouTube did not list your playlists".into())
    }

    /// Adds `video_ids` to the end of one of the account's playlists.
    pub async fn add_to_playlist(&self, playlist_id: &str, video_ids: &[String]) -> Result<()> {
        for chunk in video_ids.chunks(PER_REQUEST) {
            let actions: Vec<_> = chunk
                .iter()
                .map(|v| json!({ "action": "ACTION_ADD_VIDEO", "addedVideoId": v }))
                .collect();
            let reply = self
                .edit(
                    "browse/edit_playlist",
                    &json!({ "playlistId": playlist_id, "actions": actions }),
                )
                .await?;
            // A refused edit, such as a full playlist, still answers 200.
            if !succeeded(&reply) {
                return Err("YouTube didn't change the playlist".into());
            }
        }
        Ok(())
    }

    /// Makes a private playlist of `video_ids` for the signed-in account,
    /// and returns its ID. Large playlists are filled in after creation,
    /// as the create call takes only so many.
    pub async fn create_playlist(&self, title: &str, video_ids: &[String]) -> Result<String> {
        let (first, rest) = video_ids.split_at(video_ids.len().min(PER_REQUEST));
        let created = self
            .edit(
                "playlist/create",
                &json!({
                    "title": title,
                    "privacyStatus": "PRIVATE",
                    "videoIds": first,
                }),
            )
            .await?;
        let id = serde_json::from_str::<Value>(&created)
            .ok()
            .and_then(|v| v["playlistId"].as_str().map(str::to_owned))
            .ok_or("YouTube did not return the new playlist")?;
        self.add_to_playlist(&id, rest).await?;
        Ok(id)
    }
}

fn succeeded(reply: &str) -> bool {
    serde_json::from_str::<Value>(reply).is_ok_and(|v| v["status"] == "STATUS_SUCCEEDED")
}

/// The like button's state in a `next` response.
fn parse_rating(json: &str) -> Rating {
    let status = serde_json::from_str::<Value>(json).ok().and_then(|v| {
        v.pointer("/playerOverlays/playerOverlayRenderer/actions/0/likeButtonRenderer/likeStatus")?
            .as_str()
            .map(str::to_owned)
    });
    match status.as_deref() {
        Some("LIKE") => Rating::Like,
        Some("DISLIKE") => Rating::Dislike,
        _ => Rating::None,
    }
}

fn parse_playlists(json: &str) -> Option<Vec<PlaylistOption>> {
    let v: Value = serde_json::from_str(json).ok()?;
    let playlists = v
        .pointer("/contents/0/addToPlaylistRenderer/playlists")?
        .as_array()?;
    Some(
        playlists
            .iter()
            .filter_map(|p| {
                let option = &p["playlistAddToOptionRenderer"];
                let title = &option["title"];
                Some(PlaylistOption {
                    id: option["playlistId"].as_str()?.to_owned(),
                    title: title["simpleText"]
                        .as_str()
                        .or_else(|| title["runs"][0]["text"].as_str())?
                        .to_owned(),
                    contains: option["containsSelectedVideos"] == "ALL",
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_like_buttons_state() {
        let next = |status: &str| {
            format!(
                r#"{{"playerOverlays":{{"playerOverlayRenderer":{{"actions":[{{"likeButtonRenderer":
                {{"target":{{"videoId":"x"}},"likeStatus":"{status}","likesAllowed":true}}}}]}}}}}}"#
            )
        };
        assert_eq!(parse_rating(&next("LIKE")), Rating::Like);
        assert_eq!(parse_rating(&next("DISLIKE")), Rating::Dislike);
        assert_eq!(parse_rating(&next("INDIFFERENT")), Rating::None);
        assert_eq!(parse_rating("{}"), Rating::None);
        assert_eq!(Rating::from_index(Rating::Dislike.index()), Rating::Dislike);
    }

    #[test]
    fn reads_whether_an_edit_succeeded() {
        assert!(succeeded(
            r#"{"status":"STATUS_SUCCEEDED","playlistEditResults":[]}"#
        ));
        assert!(!succeeded(r#"{"status":"STATUS_FAILED"}"#));
        assert!(!succeeded("{}"));
    }

    #[test]
    fn lists_own_playlists() {
        let json = r#"{"contents":[{"addToPlaylistRenderer":{"playlists":[
            {"playlistAddToOptionRenderer":{"playlistId":"PL1","title":{"simpleText":"Road trip"},
                "containsSelectedVideos":"NONE"}},
            {"playlistAddToOptionRenderer":{"playlistId":"PL2","title":{"runs":[{"text":"Gym"}]},
                "containsSelectedVideos":"ALL"}},
            {"playlistAddToOptionRenderer":{"title":{"simpleText":"No ID"}}}]}}]}"#;
        assert_eq!(
            parse_playlists(json),
            Some(vec![
                PlaylistOption {
                    id: "PL1".into(),
                    title: "Road trip".into(),
                    contains: false
                },
                PlaylistOption {
                    id: "PL2".into(),
                    title: "Gym".into(),
                    contains: true
                },
            ])
        );
        assert_eq!(parse_playlists("{}"), None);
    }
}
