use rustypipe::client::ClientType;
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};

use super::{Api, PlaylistPrivacy, Result, Track, err};

/// Why a playlist couldn't be made public or unlisted.
const NEEDS_CHANNEL: &str = "the account needs a YouTube channel to share playlists";

/// Songs or actions YouTube takes in one playlist call.
const PER_REQUEST: usize = 100;

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

fn all_video_ids(ids: &[String]) -> Result<()> {
    ids.iter().try_for_each(|id| super::video_id(id).map(drop))
}

/// `id`, should it be in the shape of a playlist's, channel's or playlist
/// entry's, so nothing else ever goes into a request that edits the account.
fn youtube_id(id: &str) -> Result<&str> {
    let valid = (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(id)
    } else {
        Err("not a YouTube ID".into())
    }
}

impl Api {
    async fn edit(&self, endpoint: &str, body: &Value) -> Result<String> {
        self.q()
            .authenticated()
            .raw(ClientType::DesktopMusic, endpoint, body)
            .await
            .map_err(err)
    }

    pub async fn rating(&self, video_id: &str) -> Result<Rating> {
        let json = self
            .edit("next", &json!({ "videoId": super::video_id(video_id)? }))
            .await?;
        Ok(parse_rating(&json))
    }

    pub async fn rate(&self, video_id: &str, rating: Rating) -> Result<()> {
        let endpoint = match rating {
            Rating::Like => "like/like",
            Rating::Dislike => "like/dislike",
            Rating::None => "like/removelike",
        };
        let video_id = super::video_id(video_id)?;
        self.edit(endpoint, &json!({ "target": { "videoId": video_id } }))
            .await
            .map(drop)
    }

    /// The account's own playlists, marking those that hold `video_id`.
    pub async fn playlists_for(&self, video_id: &str) -> Result<Vec<PlaylistOption>> {
        let json = self
            .edit(
                "playlist/get_add_to_playlist",
                &json!({ "videoIds": [super::video_id(video_id)?] }),
            )
            .await?;
        parse_playlists(&json).ok_or_else(|| "YouTube did not list your playlists".into())
    }

    /// Applies `actions` to one of the account's playlists, `PER_REQUEST` at a
    /// time.
    async fn edit_playlist(&self, playlist_id: &str, actions: &[Value]) -> Result<()> {
        let playlist_id = youtube_id(playlist_id)?;
        for chunk in actions.chunks(PER_REQUEST) {
            let reply = self
                .edit(
                    "browse/edit_playlist",
                    &json!({ "playlistId": playlist_id, "actions": chunk }),
                )
                .await?;
            // A refused edit, such as a full playlist, still answers 200.
            if !succeeded(&reply) {
                // An account without a channel is asked to make one first.
                return Err(if reply.contains("channelCreationFormEndpoint") {
                    NEEDS_CHANNEL.into()
                } else {
                    "YouTube didn't change the playlist".into()
                });
            }
        }
        Ok(())
    }

    /// Adds `video_ids` to the end of one of the account's playlists.
    pub async fn add_to_playlist(&self, playlist_id: &str, video_ids: &[String]) -> Result<()> {
        all_video_ids(video_ids)?;
        let actions: Vec<_> = video_ids
            .iter()
            .map(|v| json!({ "action": "ACTION_ADD_VIDEO", "addedVideoId": v }))
            .collect();
        self.edit_playlist(playlist_id, &actions).await
    }

    /// Makes a private playlist and returns its ID. The create call takes only
    /// so many songs, so the rest are added after.
    pub async fn create_playlist(&self, title: &str, video_ids: &[String]) -> Result<String> {
        all_video_ids(video_ids)?;
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

impl Api {
    /// Removes `tracks`, as its page has them, from one of the account's
    /// playlists.
    pub async fn remove_from_playlist(&self, playlist_id: &str, tracks: &[Track]) -> Result<()> {
        let actions = tracks
            .iter()
            .map(|t| {
                let entry = t.set_video_id.as_deref().ok_or("not in your playlist")?;
                Ok(json!({
                    "action": "ACTION_REMOVE_VIDEO",
                    "setVideoId": youtube_id(entry)?,
                    "removedVideoId": super::video_id(&t.id)?,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        self.edit_playlist(playlist_id, &actions).await
    }

    /// Moves the entry `set_video_id` of one of the account's playlists to
    /// before the entry `before`, or to the end.
    pub async fn move_in_playlist(
        &self,
        playlist_id: &str,
        set_video_id: &str,
        before: Option<&str>,
    ) -> Result<()> {
        let mut action = json!({
            "action": "ACTION_MOVE_VIDEO_BEFORE",
            "setVideoId": youtube_id(set_video_id)?,
        });
        // Without a successor YouTube moves it last (ytmusicapi's edit_playlist).
        if let Some(before) = before {
            action["movedSetVideoIdSuccessor"] = youtube_id(before)?.into();
        }
        self.edit_playlist(playlist_id, &[action]).await
    }

    pub async fn rename_playlist(&self, playlist_id: &str, title: &str) -> Result<()> {
        let title = title.trim();
        if title.is_empty() {
            return Err("a playlist needs a name".into());
        }
        let action = json!({ "action": "ACTION_SET_PLAYLIST_NAME", "playlistName": title });
        self.edit_playlist(playlist_id, &[action]).await
    }

    pub async fn set_playlist_privacy(
        &self,
        playlist_id: &str,
        privacy: PlaylistPrivacy,
    ) -> Result<()> {
        let privacy = match privacy {
            PlaylistPrivacy::Public => "PUBLIC",
            PlaylistPrivacy::Unlisted => "UNLISTED",
            PlaylistPrivacy::Private => "PRIVATE",
        };
        let action = json!({ "action": "ACTION_SET_PLAYLIST_PRIVACY", "playlistPrivacy": privacy });
        self.edit_playlist(playlist_id, &[action]).await
    }

    pub async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        let body = json!({ "playlistId": youtube_id(playlist_id)? });
        performed(&self.edit("playlist/delete", &body).await?)
    }

    /// Saves an album or playlist to the library, by the playlist ID its
    /// header carries, or removes it.
    pub async fn save_to_library(&self, playlist_id: &str, saved: bool) -> Result<()> {
        let endpoint = if saved {
            "like/like"
        } else {
            "like/removelike"
        };
        let body = json!({ "target": { "playlistId": youtube_id(playlist_id)? } });
        performed(&self.edit(endpoint, &body).await?)
    }

    /// Subscribes to an artist by the channel its header carries, or
    /// unsubscribes.
    pub async fn subscribe(&self, channel_id: &str, subscribed: bool) -> Result<()> {
        let endpoint = if subscribed {
            "subscription/subscribe"
        } else {
            "subscription/unsubscribe"
        };
        let body = json!({ "channelIds": [youtube_id(channel_id)?] });
        performed(&self.edit(endpoint, &body).await?)
    }

    pub async fn set_in_library(&self, subscribe: bool, id: &str, on: bool) -> Result<()> {
        if subscribe {
            self.subscribe(id, on).await
        } else {
            self.save_to_library(id, on).await
        }
    }
}

fn succeeded(reply: &str) -> bool {
    serde_json::from_str::<Value>(reply).is_ok_and(|v| v["status"] == "STATUS_SUCCEEDED")
}

/// For answers without a status: a change YouTube held back comes with a
/// dialog instead (ytmusicapi's validate_write_response).
fn performed(reply: &str) -> Result<()> {
    let v: Value = serde_json::from_str(reply).map_err(err)?;
    match v.pointer("/actions/0/showEngagementPanelEndpoint") {
        Some(_) => Err("YouTube didn't make the change".into()),
        None => Ok(()),
    }
}

/// The like button's state in a `next` response.
fn parse_rating(json: &str) -> Rating {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Next<'a> {
        /// Kept raw, so the large `contents` beside it is only scanned.
        #[serde(borrow)]
        player_overlays: &'a RawValue,
    }
    let overlays = serde_json::from_str::<Next>(json)
        .ok()
        .and_then(|next| serde_json::from_str::<Value>(next.player_overlays.get()).ok());
    let status = overlays.as_ref().and_then(|v| {
        v.pointer("/playerOverlayRenderer/actions/0/likeButtonRenderer/likeStatus")?
            .as_str()
    });
    match status {
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
    fn reads_whether_a_change_was_made() {
        assert!(performed(r#"{"responseContext":{},"actions":[{"addToToastAction":{}}]}"#).is_ok());
        assert!(performed(r#"{"responseContext":{}}"#).is_ok());
        let held = r#"{"actions":[{"showEngagementPanelEndpoint":{"identifier":{"tag":"x"}}}]}"#;
        assert!(performed(held).is_err());
        assert!(performed("<html>").is_err());
    }

    #[test]
    fn takes_only_youtube_ids() {
        for id in [
            "PLaZPMsuQNCsWn0iVMtGbaUXO6z-EdZaZm",
            "UC0C-w0YjGpqDXGB8IHb662A",
            "56B44F6D10557CC6",
        ] {
            assert_eq!(youtube_id(id), Ok(id));
        }
        let long = "a".repeat(65);
        for id in ["", r"C:\Music\song.mp3", r#"PL1","x"#, "PL 1", &long] {
            assert!(youtube_id(id).is_err(), "{id}");
        }
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

    // Taken from rustypipe's own tests: public, and long-lived.
    const ALBUMS: &[&str] = &[
        "MPREb_nlBWQROfvjo",
        "MPREb_8QkDeEIawvX",
        "MPREb_bHfHGoy7vuv",
        "MPREb_PiyfuVl6aYd",
        "MPREb_u1I69lSAe5v",
    ];
    const PLAYLISTS: &[&str] = &[
        "RDCLAK5uy_kFQXdnqMaQCVx2wpUM4ZfbsGCDibZtkJk",
        "RDCLAK5uy_kb7EBi6y3GrtJri4_ZH56Ms786DFEimbM",
        "PL5dDx681T4bR7ZF1IuWzOv1omlRbE7PiJ",
        "PLT2w2oBf1TZKyvY_M6JsASs73m-wjLzH5",
    ];
    const ARTISTS: &[&str] = &[
        "UClmXPfaYhXOYsNn_QUyheWQ",
        "UCfwCE5VhPMGxNPFxtVv7lRw",
        "UC0aXrjVxG5pZr99v77wZdPQ",
        "UCOR4_bSVIXPsGa4BbCSt60Q",
        "UC0C-w0YjGpqDXGB8IHb662A",
    ];

    /// Whether `route`'s page reads as saved or subscribed, and the ID that
    /// changing it takes.
    async fn library_state(
        api: &Api,
        route: &crate::api::Route,
        subscribe: bool,
    ) -> Result<(Option<bool>, Option<String>)> {
        let header = api.page(route, true).await?.header.ok_or("no header")?;
        Ok(if subscribe {
            (header.subscribed, header.channel_id)
        } else {
            (header.saved, header.save_id)
        })
    }

    /// The state read once it is `want`, asking a few times, as a change can
    /// take a moment to show.
    async fn read_until(
        api: Api,
        route: crate::api::Route,
        subscribe: bool,
        want: bool,
    ) -> Result<Option<bool>> {
        let mut last = Err("not read".to_owned());
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            last = library_state(&api, &route, subscribe).await.map(|(s, _)| s);
            if last == Ok(Some(want)) {
                break;
            }
        }
        last
    }

    /// On a copy of the app's sign-in, saves an album and a playlist and
    /// subscribes to an artist, each found neither saved nor subscribed to,
    /// checks each reads back, and undoes it whatever happened.
    #[test]
    #[ignore = "changes the signed-in account, then puts it back"]
    fn live_saves_and_subscribes() {
        use crate::api::Route;
        let source = crate::settings::data_dir().join("rustypipe_cache.bin");
        let dir = std::env::temp_dir().join(format!("lily-live-library-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _remove = Scratch(dir.clone());
        if std::fs::copy(&source, dir.join("rustypipe_cache.bin")).is_err() {
            eprintln!("skipped: no sign-in at {}", source.display());
            return;
        }
        let rt = tokio::runtime::Runtime::new().expect("runtime starts");
        let failures = rt.block_on(async {
            let api = match Api::in_dir(dir.clone(), None) {
                Ok(api) if api.has_cookie() => api,
                Ok(_) => return vec!["the copied sign-in holds no cookie".to_owned()],
                Err(e) => return vec![format!("client: {e}")],
            };
            if api.cookie_valid().await == Some(false) {
                return vec!["the sign-in looks stale: YouTube refused the cookie".to_owned()];
            }
            let kinds = [
                ("album", ALBUMS, false, Route::Album as fn(String) -> Route),
                ("playlist", PLAYLISTS, false, Route::Playlist),
                ("artist", ARTISTS, true, Route::Artist),
            ];
            let mut failures = Vec::new();
            for (kind, candidates, subscribe, make) in kinds {
                let mut found = None;
                for id in candidates {
                    let route = make((*id).to_owned());
                    match library_state(&api, &route, subscribe).await {
                        Ok((Some(false), Some(target))) => {
                            found = Some((*id, route, target));
                            break;
                        }
                        other => eprintln!("{kind} {id}: passed over, reads {other:?}"),
                    }
                }
                let Some((id, route, target)) = found else {
                    failures.push(format!("{kind}: no candidate read as not in the library"));
                    continue;
                };
                // Nothing between here and the undo may return or panic.
                let done = api.set_in_library(subscribe, &target, true).await;
                let after = tokio::spawn(read_until(api.clone(), route.clone(), subscribe, true));
                let after = after.await.map_err(|e| e.to_string()).and_then(|r| r);
                let mut undone = Err(String::new());
                for _ in 0..3 {
                    undone = api.set_in_library(subscribe, &target, false).await;
                    if undone.is_ok() {
                        break;
                    }
                }
                let restored = tokio::spawn(read_until(api.clone(), route, subscribe, false));
                let restored = restored.await.map_err(|e| e.to_string()).and_then(|r| r);
                eprintln!(
                    "{kind} {id} (by {target}): set {done:?}, then read {after:?}; \
                     undone {undone:?}, then read {restored:?}"
                );
                if done.is_err() || after != Ok(Some(true)) {
                    failures.push(format!(
                        "{kind} {id}: didn't read as set: {done:?}, {after:?}"
                    ));
                }
                if undone.is_err() || restored != Ok(Some(false)) {
                    let message = format!(
                        "{kind} {id} MAY BE LEFT CHANGED, undo by hand: {undone:?}, {restored:?}"
                    );
                    eprintln!("{message}");
                    failures.push(message);
                }
            }
            failures
        });
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    /// Removes the folder, which holds a copy of the sign-in, however the
    /// test ends.
    struct Scratch(std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Public videos that are never going away.
    const VIDEOS: [&str; 3] = ["dQw4w9WgXcQ", "kJQP7kiw5Fk", "9bZkp7q19f0"];

    /// The scratch playlist as its page has it, after a pause, as YouTube
    /// takes a moment to show an edit.
    async fn load(api: &Api, id: &str) -> Result<crate::api::Page> {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        api.page(&crate::api::Route::Playlist(id.to_owned()), true)
            .await
    }

    fn ids(page: &crate::api::Page) -> Vec<&str> {
        page.header_tracks().iter().map(|t| t.id.as_str()).collect()
    }

    async fn edit_scratch(api: Api, id: String, title: String) {
        let page = load(&api, &id).await.expect("the new playlist opens");
        let header = page.header.as_ref().expect("a header");
        assert!(header.editable, "it is the account's own");
        assert_eq!(header.privacy, Some(PlaylistPrivacy::Private));
        // YouTube Music swaps music videos for their song versions, so the
        // order is checked by what the page holds rather than what was sent.
        let first = ids(&page);
        assert_eq!(first.len(), VIDEOS.len(), "every song was added");
        let tracks = page.header_tracks();
        assert!(
            tracks.iter().all(|t| t.set_video_id.is_some()),
            "every entry has its ID"
        );
        let entry = |i: usize| tracks[i].set_video_id.as_deref();

        api.move_in_playlist(&id, entry(2).unwrap(), entry(0))
            .await
            .expect("moved");
        eprintln!("moved the last song first");
        api.remove_from_playlist(&id, &tracks[1..2])
            .await
            .expect("removed");
        eprintln!("removed {}", first[1]);
        let renamed = format!("{title} renamed");
        api.rename_playlist(&id, &renamed).await.expect("renamed");
        eprintln!("renamed it {renamed:?}");
        let privacy = match api
            .set_playlist_privacy(&id, PlaylistPrivacy::Unlisted)
            .await
        {
            Err(e) if e == NEEDS_CHANNEL => {
                eprintln!("not made unlisted: {NEEDS_CHANNEL}");
                PlaylistPrivacy::Private
            }
            done => {
                done.expect("made unlisted");
                eprintln!("made it unlisted");
                PlaylistPrivacy::Unlisted
            }
        };

        let page = load(&api, &id).await.expect("the playlist opens again");
        let header = page.header.as_ref().expect("a header");
        assert_eq!(ids(&page), [first[2], first[0]]);
        assert_eq!(header.title, renamed);
        assert_eq!(header.privacy, Some(privacy));
        eprintln!("the reloaded page has the edits");
    }

    /// Makes a private playlist on the signed-in account, edits it every way
    /// the app does, and deletes it however that went. Signed in through a
    /// copy of the app's sign-in, so the app's own files are never written.
    #[test]
    #[ignore = "edits the signed-in account; run by hand"]
    fn live_edits_a_scratch_playlist() {
        let signed_in = crate::settings::data_dir().join("rustypipe_cache.bin");
        if !signed_in.exists() {
            eprintln!("skipped: no sign-in at {}", signed_in.display());
            return;
        }
        let dir = Scratch(std::env::temp_dir().join(format!("lily-live-{}", std::process::id())));
        std::fs::create_dir_all(&dir.0).expect("temporary folder made");
        std::fs::copy(&signed_in, dir.0.join("rustypipe_cache.bin")).expect("sign-in copied");
        let api = Api::in_dir(dir.0.clone(), None).expect("client starts");
        if !api.has_cookie() {
            eprintln!("skipped: the sign-in has no cookie");
            return;
        }
        let rt = tokio::runtime::Runtime::new().expect("runtime starts");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let title = format!("Lily live test {stamp}");
        let videos = VIDEOS.map(String::from);
        // Named first, so one made by a call that then failed can be found.
        eprintln!("making the private playlist {title:?}");
        let id = rt
            .block_on(api.create_playlist(&title, &videos))
            .expect("scratch playlist made");
        eprintln!("made it: {id}");

        // Spawned, so a failed assertion comes back here to clean up after.
        let edited = rt.block_on(rt.spawn(edit_scratch(api.clone(), id.clone(), title)));
        let deleted = rt.block_on(api.delete_playlist(&id));
        eprintln!("deleted it: {deleted:?}");
        let gone = rt.block_on(async {
            for _ in 0..3 {
                if load(&api, &id).await.is_err() {
                    return true;
                }
            }
            false
        });
        eprintln!("gone: {gone}");

        if let Err(e) = edited {
            match e.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                Err(e) => panic!("{e}"),
            }
        }
        deleted.expect("scratch playlist deleted");
        assert!(gone, "the deleted playlist {id} still opens");
    }
}
