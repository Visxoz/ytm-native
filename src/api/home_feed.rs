//! Parses YouTube Music's personal home feed (`FEmusic_home`): shelves such
//! as Listen again, Quick picks and Mixed for you. rustypipe has no model
//! for it, so this reads the renderer JSON directly.

use serde_json::Value;

use super::{
    ArtistRef, Card, CardKind, CardLayout, QueueMode, Section, Track,
    convert::{CARD_THUMB, ROW_THUMB, resize_thumb},
};

/// Longest list shelf shown, such as Quick picks.
const LIST_LIMIT: usize = 10;

fn runs(v: &Value) -> impl Iterator<Item = &Value> {
    v["runs"].as_array().into_iter().flatten()
}

fn text(v: &Value) -> String {
    runs(v).filter_map(|r| r["text"].as_str()).collect()
}

fn browse_id(run: &Value) -> Option<&str> {
    run["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str()
}

/// Official music videos may have a song version; uploads have none.
fn is_music_video(watch: &Value) -> bool {
    watch["watchEndpointMusicSupportedConfigs"]["watchEndpointMusicConfig"]["musicVideoType"]
        .as_str()
        == Some("MUSIC_VIDEO_TYPE_OMV")
}

fn thumb(v: &Value, edge: u32) -> Option<String> {
    let thumbs = v["musicThumbnailRenderer"]["thumbnail"]["thumbnails"].as_array()?;
    let best = thumbs
        .iter()
        .filter(|t| t["width"].as_u64().unwrap_or(0) >= edge as u64)
        .min_by_key(|t| t["width"].as_u64())
        .or_else(|| thumbs.iter().max_by_key(|t| t["width"].as_u64()))?;
    Some(resize_thumb(best["url"].as_str()?, edge))
}

/// Artists from a subtitle such as "Song • Trippie Redd & Juice WRLD": the
/// runs linked to channels.
fn artists(subtitle: &Value) -> Vec<ArtistRef> {
    let linked: Vec<ArtistRef> = runs(subtitle)
        .filter(|r| browse_id(r).is_some_and(|id| id.starts_with("UC")))
        .map(|r| ArtistRef {
            id: browse_id(r).map(str::to_owned),
            name: r["text"].as_str().unwrap_or_default().to_owned(),
        })
        .collect();
    if !linked.is_empty() {
        return linked;
    }
    // Unlinked uploads: the first run that is not a type label.
    runs(subtitle)
        .filter_map(|r| r["text"].as_str())
        .find(|t| !matches!(*t, "Song" | "Video" | " • " | "Episode"))
        .map(|name| {
            vec![ArtistRef {
                id: None,
                name: name.to_owned(),
            }]
        })
        .unwrap_or_default()
}

fn two_row_card(r: &Value) -> Option<Card> {
    let title = text(&r["title"]);
    let subtitle = text(&r["subtitle"]);
    let card_thumb = thumb(&r["thumbnailRenderer"], CARD_THUMB);
    let nav = &r["navigationEndpoint"];
    if let Some(video_id) = nav["watchEndpoint"]["videoId"].as_str() {
        let track = Track {
            id: video_id.to_owned(),
            title: title.clone(),
            artists: artists(&r["subtitle"]),
            album: None,
            duration: None,
            thumb: thumb(&r["thumbnailRenderer"], ROW_THUMB),
            track_nr: None,
            video: is_music_video(&nav["watchEndpoint"]),
        };
        return Some(Card {
            kind: CardKind::Song,
            id: video_id.to_owned(),
            title,
            subtitle,
            thumb: card_thumb,
            color: None,
            track: Some(track),
        });
    }
    let browse = &nav["browseEndpoint"];
    let id = browse["browseId"].as_str()?;
    let page = browse["browseEndpointContextSupportedConfigs"]["browseEndpointContextMusicConfig"]
        ["pageType"]
        .as_str()
        .unwrap_or_default();
    let (kind, id) = match page {
        "MUSIC_PAGE_TYPE_ALBUM" => (CardKind::Album, id),
        "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL" => (CardKind::Artist, id),
        "MUSIC_PAGE_TYPE_PLAYLIST" => (CardKind::Playlist, id.strip_prefix("VL").unwrap_or(id)),
        _ => return None,
    };
    Some(Card {
        kind,
        id: id.to_owned(),
        title,
        subtitle,
        thumb: card_thumb,
        color: None,
        track: None,
    })
}

fn list_track(r: &Value) -> Option<Track> {
    let watch = &r["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]
        ["playNavigationEndpoint"]["watchEndpoint"];
    let video_id = r["playlistItemData"]["videoId"]
        .as_str()
        .or_else(|| watch["videoId"].as_str())?;
    let column =
        |i: usize| &r["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"];
    let album = runs(column(2))
        .find(|run| browse_id(run).is_some_and(|id| id.starts_with("MPRE")))
        .map(|run| {
            (
                browse_id(run).unwrap_or_default().to_owned(),
                run["text"].as_str().unwrap_or_default().to_owned(),
            )
        });
    Some(Track {
        id: video_id.to_owned(),
        title: text(column(0)),
        artists: artists(column(1)),
        album,
        duration: None,
        thumb: thumb(&r["thumbnail"], ROW_THUMB),
        track_nr: None,
        video: is_music_video(watch),
    })
}

/// Turns one shelf into a section: card rows for two-row items, a track list
/// for list items.
fn shelf(r: &Value) -> Option<Section> {
    let title = text(&r["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"]);
    let items = r["contents"].as_array()?;
    let tracks: Vec<Track> = items
        .iter()
        .filter_map(|i| i.get("musicResponsiveListItemRenderer"))
        .filter_map(list_track)
        .take(LIST_LIMIT)
        .collect();
    if !tracks.is_empty() {
        return Some(Section::Tracks {
            title,
            tracks,
            mode: QueueMode::Section,
            numbered: false,
        });
    }
    let cards: Vec<Card> = items
        .iter()
        .filter_map(|i| i.get("musicTwoRowItemRenderer"))
        .filter_map(two_row_card)
        .collect();
    (!cards.is_empty()).then_some(Section::Cards {
        title,
        cards,
        layout: CardLayout::Carousel,
    })
}

fn shelves(contents: &Value) -> Vec<Section> {
    contents
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("musicCarouselShelfRenderer"))
        .filter_map(shelf)
        .collect()
}

fn continuation(list: &Value) -> Option<String> {
    list["continuations"][0]["nextContinuationData"]["continuation"]
        .as_str()
        .map(str::to_owned)
}

/// Shelves from the first page and the token for the next one.
pub fn first_page(json: &str) -> Result<(Vec<Section>, Option<String>), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let list = &v["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
        ["sectionListRenderer"];
    if list.is_null() {
        return Err("unexpected home feed".into());
    }
    Ok((shelves(&list["contents"]), continuation(list)))
}

/// Shelves from a continuation page and the token for the next one.
pub fn next_page(json: &str) -> Result<(Vec<Section>, Option<String>), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let list = &v["continuationContents"]["sectionListContinuation"];
    Ok((shelves(&list["contents"]), continuation(list)))
}

/// Pages fetched: the first and this many continuations, as YouTube Music
/// loads while scrolling.
pub const CONTINUATIONS: usize = 2;

pub async fn fetch(q: &rustypipe::client::RustyPipeQuery) -> Result<Vec<Section>, String> {
    let q = q.clone().authenticated();
    let first = q
        .raw(
            rustypipe::client::ClientType::DesktopMusic,
            "browse",
            &serde_json::json!({ "browseId": "FEmusic_home" }),
        )
        .await
        .map_err(|e| e.to_string())?;
    let (mut sections, mut token) = first_page(&first)?;
    for _ in 0..CONTINUATIONS {
        let Some(next) = token.take() else { break };
        let Ok(page) = q
            .raw(
                rustypipe::client::ClientType::DesktopMusic,
                "browse",
                &serde_json::json!({ "continuation": next }),
            )
            .await
        else {
            break;
        };
        let (more, next) = next_page(&page)?;
        sections.extend(more);
        token = next;
    }
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"{"contents":{"singleColumnBrowseResultsRenderer":{"tabs":[{"tabRenderer":{"content":{"sectionListRenderer":{
      "contents":[
        {"musicCarouselShelfRenderer":{"header":{"musicCarouselShelfBasicHeaderRenderer":{"title":{"runs":[{"text":"Listen again"}]}}},
          "contents":[
            {"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"Matt Hardy 999"}]},
              "subtitle":{"runs":[{"text":"Song"},{"text":" • "},{"text":"Trippie Redd","navigationEndpoint":{"browseEndpoint":{"browseId":"UCa"}}},{"text":" & "},{"text":"Juice WRLD","navigationEndpoint":{"browseEndpoint":{"browseId":"UCb"}}}]},
              "thumbnailRenderer":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[{"url":"https://lh3.googleusercontent.com/x=w226-h226","width":226}]}}},
              "navigationEndpoint":{"watchEndpoint":{"videoId":"44zDzdf08s4",
                "watchEndpointMusicSupportedConfigs":{"watchEndpointMusicConfig":{"musicVideoType":"MUSIC_VIDEO_TYPE_OMV"}}}}}},
            {"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"My Supermix"}]},"subtitle":{"runs":[{"text":"Juice WRLD, MXZI"}]},
              "navigationEndpoint":{"browseEndpoint":{"browseId":"VLRDTMAK5uy_kset","browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_PLAYLIST"}}}}}}
          ]}},
        {"musicCarouselShelfRenderer":{"header":{"musicCarouselShelfBasicHeaderRenderer":{"title":{"runs":[{"text":"Quick picks"}]}}},
          "contents":[
            {"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":"TjsfgUkT0eg"},
              "flexColumns":[
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Plug Walk"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Rich The Kid","navigationEndpoint":{"browseEndpoint":{"browseId":"UCr"}}},{"text":" • 565M plays"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"The World Is Yours","navigationEndpoint":{"browseEndpoint":{"browseId":"MPREb_1"}}}]}}}]}}
          ]}}
      ],
      "continuations":[{"nextContinuationData":{"continuation":"TOKEN"}}]}}}}]}}}"#;

    #[test]
    fn parses_cards_and_track_shelves() {
        let (sections, token) = first_page(PAGE).unwrap();
        assert_eq!(token.as_deref(), Some("TOKEN"));
        let Section::Cards { title, cards, .. } = &sections[0] else {
            panic!("expected cards")
        };
        assert_eq!(title, "Listen again");
        assert_eq!(cards[0].kind, CardKind::Song);
        let track = cards[0].track.as_ref().unwrap();
        assert_eq!(track.id, "44zDzdf08s4");
        assert_eq!(track.artist_names(), "Trippie Redd, Juice WRLD");
        assert!(track.video, "an official music video");
        assert_eq!(cards[1].kind, CardKind::Playlist);
        assert_eq!(cards[1].id, "RDTMAK5uy_kset", "VL prefix is dropped");

        let Section::Tracks { title, tracks, .. } = &sections[1] else {
            panic!("expected tracks")
        };
        assert_eq!(title, "Quick picks");
        assert_eq!(tracks[0].title, "Plug Walk");
        assert_eq!(tracks[0].artist_names(), "Rich The Kid");
        assert_eq!(tracks[0].album.as_ref().unwrap().0, "MPREb_1");
        assert!(!tracks[0].video);
    }

    #[test]
    fn rejects_other_pages() {
        assert!(first_page("{}").is_err());
        assert!(next_page("{}").unwrap().0.is_empty());
    }
}
