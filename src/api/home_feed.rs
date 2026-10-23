//! Parses YouTube Music's personal home feed (`FEmusic_home`): shelves such
//! as Listen again, Quick picks and Mixed for you. rustypipe has no model
//! for it, so this reads the renderer JSON directly, through types naming
//! only the fields used: serde skips the rest without allocating, where a
//! generic JSON tree would build every object and string of the response.

use rustypipe::client::{ClientType, RustyPipeQuery};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::{
    ArtistRef, Card, CardKind, CardLayout, QueueMode, Section, Track,
    convert::{CARD_THUMB, ROW_THUMB, resize_thumb},
};

/// Longest list shelf shown, such as Quick picks.
const LIST_LIMIT: usize = 10;
/// Pages fetched: the first and this many continuations, as YouTube Music
/// loads while scrolling.
pub const CONTINUATIONS: usize = 2;

// Every struct defaults its missing fields, as YouTube leaves out what a
// renderer does not have. Shelves and their items are kept as raw text and
// parsed one by one, so one in an unexpected shape is skipped rather than
// failing the page.

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Response {
    contents: Contents,
    continuation_contents: ContinuationContents,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Contents {
    single_column_browse_results_renderer: BrowseResults,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BrowseResults {
    tabs: Vec<Tab>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Tab {
    tab_renderer: TabRenderer,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TabRenderer {
    content: TabContent,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct TabContent {
    section_list_renderer: Option<SectionList>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ContinuationContents {
    section_list_continuation: SectionList,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SectionList {
    contents: Vec<Box<RawValue>>,
    continuations: Vec<Continuation>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Continuation {
    next_continuation_data: NextContinuation,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NextContinuation {
    continuation: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct SectionItem {
    music_carousel_shelf_renderer: Option<Shelf>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Shelf {
    header: ShelfHeader,
    contents: Vec<Box<RawValue>>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ShelfHeader {
    music_carousel_shelf_basic_header_renderer: BasicHeader,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BasicHeader {
    title: Runs,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ShelfItem {
    music_two_row_item_renderer: Option<TwoRow>,
    music_responsive_list_item_renderer: Option<ListItem>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Runs {
    runs: Vec<Run>,
}

impl Runs {
    fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// Artists from a subtitle such as "Song • Trippie Redd & Juice WRLD":
    /// the runs linked to channels.
    fn artists(&self) -> Vec<ArtistRef> {
        let linked: Vec<ArtistRef> = self
            .runs
            .iter()
            .filter_map(|r| {
                let id = r.browse_id().filter(|id| id.starts_with("UC"))?;
                Some(ArtistRef {
                    id: Some(id.to_owned()),
                    name: r.text.clone(),
                })
            })
            .collect();
        if !linked.is_empty() {
            return linked;
        }
        // Unlinked uploads: the first run that is not a type label.
        self.runs
            .iter()
            .map(|r| r.text.as_str())
            .find(|t| !matches!(*t, "Song" | "Video" | " • " | "Episode"))
            .map(|name| {
                vec![ArtistRef {
                    id: None,
                    name: name.to_owned(),
                }]
            })
            .unwrap_or_default()
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Run {
    text: String,
    navigation_endpoint: Navigation,
}

impl Run {
    fn browse_id(&self) -> Option<&str> {
        self.navigation_endpoint
            .browse_endpoint
            .as_ref()
            .map(|b| b.browse_id.as_str())
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Navigation {
    watch_endpoint: Option<Watch>,
    browse_endpoint: Option<Browse>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Watch {
    video_id: String,
    watch_endpoint_music_supported_configs: WatchConfigs,
}

impl Watch {
    /// Official music videos may have a song version; uploads have none.
    fn is_music_video(&self) -> bool {
        self.watch_endpoint_music_supported_configs
            .watch_endpoint_music_config
            .music_video_type
            == "MUSIC_VIDEO_TYPE_OMV"
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct WatchConfigs {
    watch_endpoint_music_config: WatchConfig,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct WatchConfig {
    music_video_type: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Browse {
    browse_id: String,
    browse_endpoint_context_supported_configs: BrowseConfigs,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct BrowseConfigs {
    browse_endpoint_context_music_config: BrowseConfig,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct BrowseConfig {
    page_type: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ThumbnailRenderer {
    music_thumbnail_renderer: MusicThumbnail,
}

impl ThumbnailRenderer {
    /// The smallest rendition at least `edge` wide, sized to `edge`.
    fn pick(&self, edge: u32) -> Option<String> {
        let thumbs = &self.music_thumbnail_renderer.thumbnail.thumbnails;
        let best = thumbs
            .iter()
            .filter(|t| t.width >= edge)
            .min_by_key(|t| t.width)
            .or_else(|| thumbs.iter().max_by_key(|t| t.width))?;
        Some(resize_thumb(&best.url, edge))
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MusicThumbnail {
    thumbnail: Thumbnails,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Thumbnails {
    thumbnails: Vec<Thumb>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Thumb {
    url: String,
    width: u32,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct TwoRow {
    title: Runs,
    subtitle: Runs,
    thumbnail_renderer: ThumbnailRenderer,
    navigation_endpoint: Navigation,
}

impl TwoRow {
    fn card(self) -> Option<Card> {
        let title = self.title.text();
        let subtitle = self.subtitle.text();
        let card_thumb = self.thumbnail_renderer.pick(CARD_THUMB);
        if let Some(watch) = &self.navigation_endpoint.watch_endpoint {
            let track = Track {
                id: watch.video_id.clone(),
                title: title.clone(),
                artists: self.subtitle.artists(),
                album: None,
                duration: None,
                thumb: self.thumbnail_renderer.pick(ROW_THUMB),
                track_nr: None,
                video: watch.is_music_video(),
            };
            return Some(Card {
                kind: CardKind::Song,
                id: watch.video_id.clone(),
                title,
                subtitle,
                thumb: card_thumb,
                color: None,
                track: Some(track),
            });
        }
        let browse = self.navigation_endpoint.browse_endpoint?;
        let id = browse.browse_id;
        let page = browse
            .browse_endpoint_context_supported_configs
            .browse_endpoint_context_music_config
            .page_type;
        let (kind, id) = match page.as_str() {
            "MUSIC_PAGE_TYPE_ALBUM" => (CardKind::Album, id),
            "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL" => (CardKind::Artist, id),
            "MUSIC_PAGE_TYPE_PLAYLIST" => (
                CardKind::Playlist,
                id.strip_prefix("VL").unwrap_or(&id).to_owned(),
            ),
            _ => return None,
        };
        Some(Card {
            kind,
            id,
            title,
            subtitle,
            thumb: card_thumb,
            color: None,
            track: None,
        })
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ListItem {
    playlist_item_data: PlaylistItemData,
    overlay: Overlay,
    flex_columns: Vec<FlexColumn>,
    thumbnail: ThumbnailRenderer,
}

impl ListItem {
    fn track(self) -> Option<Track> {
        let watch = self
            .overlay
            .music_item_thumbnail_overlay_renderer
            .content
            .music_play_button_renderer
            .play_navigation_endpoint
            .watch_endpoint;
        let video_id = if self.playlist_item_data.video_id.is_empty() {
            watch.as_ref()?.video_id.clone()
        } else {
            self.playlist_item_data.video_id
        };
        let column = |i: usize| {
            self.flex_columns
                .get(i)
                .map(|c| &c.music_responsive_list_item_flex_column_renderer.text)
        };
        let album = column(2).and_then(|runs| {
            runs.runs.iter().find_map(|run| {
                let id = run.browse_id().filter(|id| id.starts_with("MPRE"))?;
                Some((id.to_owned(), run.text.clone()))
            })
        });
        Some(Track {
            id: video_id,
            title: column(0).map(Runs::text).unwrap_or_default(),
            artists: column(1).map(Runs::artists).unwrap_or_default(),
            album,
            duration: None,
            thumb: self.thumbnail.pick(ROW_THUMB),
            track_nr: None,
            video: watch.as_ref().is_some_and(Watch::is_music_video),
        })
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PlaylistItemData {
    video_id: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Overlay {
    music_item_thumbnail_overlay_renderer: OverlayRenderer,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OverlayRenderer {
    content: OverlayContent,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct OverlayContent {
    music_play_button_renderer: PlayButton,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PlayButton {
    play_navigation_endpoint: Navigation,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct FlexColumn {
    music_responsive_list_item_flex_column_renderer: FlexColumnRenderer,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FlexColumnRenderer {
    text: Runs,
}

/// Parses each raw item, dropping those in an unexpected shape.
fn each<'a, T: Deserialize<'a>>(raw: &'a [Box<RawValue>]) -> impl Iterator<Item = T> + 'a {
    raw.iter().filter_map(|item| {
        serde_json::from_str(item.get())
            .map_err(|e| log::debug!("skipped a home feed item: {e}"))
            .ok()
    })
}

impl Shelf {
    /// A section: card rows for two-row items, a track list for list items.
    fn section(self) -> Option<Section> {
        let title = self
            .header
            .music_carousel_shelf_basic_header_renderer
            .title
            .text();
        let mut tracks = Vec::new();
        let mut cards = Vec::new();
        for item in each::<ShelfItem>(&self.contents) {
            if let Some(list) = item.music_responsive_list_item_renderer {
                if tracks.len() < LIST_LIMIT {
                    tracks.extend(list.track());
                }
            } else if let Some(two_row) = item.music_two_row_item_renderer {
                cards.extend(two_row.card());
            }
        }
        if !tracks.is_empty() {
            return Some(Section::Tracks {
                title,
                tracks,
                mode: QueueMode::Section,
                numbered: false,
            });
        }
        (!cards.is_empty()).then_some(Section::Cards {
            title,
            cards,
            layout: CardLayout::Carousel,
        })
    }
}

impl SectionList {
    fn shelves(&self) -> Vec<Section> {
        each::<SectionItem>(&self.contents)
            .filter_map(|s| s.music_carousel_shelf_renderer)
            .filter_map(Shelf::section)
            .collect()
    }

    fn continuation(&self) -> Option<String> {
        self.continuations
            .first()
            .map(|c| c.next_continuation_data.continuation.clone())
            .filter(|token| !token.is_empty())
    }
}

/// Shelves from the first page and the token for the next one.
pub fn first_page(json: &str) -> Result<(Vec<Section>, Option<String>), String> {
    let response: Response = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let list = response
        .contents
        .single_column_browse_results_renderer
        .tabs
        .into_iter()
        .next()
        .and_then(|tab| tab.tab_renderer.content.section_list_renderer)
        .ok_or("unexpected home feed")?;
    Ok((list.shelves(), list.continuation()))
}

/// Shelves from a continuation page and the token for the next one.
pub fn next_page(json: &str) -> Result<(Vec<Section>, Option<String>), String> {
    let response: Response = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let list = response.continuation_contents.section_list_continuation;
    Ok((list.shelves(), list.continuation()))
}

/// Requests made for one page of the feed before giving up.
pub const ATTEMPTS: u32 = 4;

/// YouTube refuses a share of requests for the signed-in feed, in bursts,
/// with "400: Request contains an invalid argument", and answers the same
/// request a moment later. The rest of the account, and the feed signed
/// out, are unaffected, so it is YouTube building the feed that fails.
fn refused(e: &rustypipe::error::Error) -> bool {
    matches!(e, rustypipe::error::Error::HttpStatus(400, _))
}

async fn browse(
    q: &RustyPipeQuery,
    body: &serde_json::Value,
    attempts: u32,
) -> Result<String, String> {
    let mut attempt = 1;
    loop {
        match q.raw(ClientType::DesktopMusic, "browse", body).await {
            Ok(json) => return Ok(json),
            Err(e) if attempt < attempts && refused(&e) => {
                log::debug!("the home feed was refused, asking again: {e}");
                let wait = std::time::Duration::from_millis(300 * u64::from(attempt));
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The feed's first page, with the token of the next, in up to `attempts`
/// requests. `q` must send the sign-in cookie: the feed is personal.
pub async fn first(
    q: &RustyPipeQuery,
    attempts: u32,
) -> Result<(Vec<Section>, Option<String>), String> {
    let body = serde_json::json!({ "browseId": "FEmusic_home" });
    let json = browse(q, &body, attempts).await?;
    first_page(&json)
}

/// The page `token` continues with, and the token of the one after.
pub async fn next(
    q: &RustyPipeQuery,
    token: &str,
) -> Result<(Vec<Section>, Option<String>), String> {
    let json = browse(q, &serde_json::json!({ "continuation": token }), ATTEMPTS).await?;
    next_page(&json)
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
            {"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"Broken"}]},
              "thumbnailRenderer":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[{"url":"x","width":"wide"}]}}},
              "navigationEndpoint":{"watchEndpoint":{"videoId":"broken"}}}},
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
        assert_eq!(cards.len(), 2, "the item in an unexpected shape is skipped");
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
    fn retries_only_refusals() {
        use rustypipe::error::Error;
        assert!(refused(&Error::HttpStatus(400, "invalid argument".into())));
        assert!(!refused(&Error::HttpStatus(401, "".into())));
        assert!(!refused(&Error::HttpStatus(404, "".into())));
        assert!(!refused(&Error::Other("network".into())));
    }

    #[test]
    fn rejects_other_pages() {
        assert!(first_page("{}").is_err());
        assert!(next_page("{}").unwrap().0.is_empty());
        assert!(first_page("not json").is_err());
    }

    #[test]
    fn reads_continuation_pages() {
        let json = r#"{"continuationContents":{"sectionListContinuation":{
            "contents":[{"musicCarouselShelfRenderer":{"header":{"musicCarouselShelfBasicHeaderRenderer":{"title":{"runs":[{"text":"Mixed for you"}]}}},
              "contents":[{"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"My Mix 1"}]},
                "navigationEndpoint":{"browseEndpoint":{"browseId":"VLRDTMAK5uy_a","browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_PLAYLIST"}}}}}}]}}],
            "continuations":[{"nextContinuationData":{"continuation":""}}]}}}"#;
        let (sections, token) = next_page(json).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(token, None, "an empty token ends the feed");
    }
}
