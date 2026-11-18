//! The personal home feed (`FEmusic_home`), which rustypipe has no model for.
//! Types name only the fields used, so serde skips the rest without allocating.

use rustypipe::client::{ClientType, RustyPipeQuery};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::{
    ArtistRef, Card, CardKind, CardLayout, QueueMode, Section, Track,
    convert::{CARD_THUMB, ROW_THUMB, card_section, fit_thumb, resize_thumb, track_section},
};

/// Longest list shelf shown, such as Quick picks.
const LIST_LIMIT: usize = 10;
/// Continuations fetched at most after the first page; the whole feed is
/// five or six pages.
const CONTINUATIONS: usize = 8;
/// The shelves Home shows, in order: those made from the account's own
/// listening. Titles are in English, rustypipe's language.
const SHELVES: [&str; 6] = [
    "Listen again",
    "Quick picks",
    "Forgotten favorites",
    "Trending songs for you",
    "Mixed for you",
    "Fresh finds, old favorites",
];
const LISTEN_AGAIN: &str = SHELVES[0];
/// Songs played lately that join Listen again, which YouTube fills with
/// older favourites and seldom the latest plays, one after every
/// `RECENT_EVERY` of YouTube's own, so they never lead it.
const RECENT: usize = 5;
const RECENT_EVERY: usize = 3;

// YouTube leaves out fields a renderer lacks, so every struct defaults. Shelves
// and items are parsed one by one, so one in an odd shape is skipped, not fatal.

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
    fn pick(&self, edge: u32) -> Option<String> {
        let thumbs = &self.music_thumbnail_renderer.thumbnail.thumbnails;
        fit_thumb(thumbs.iter().map(|t| (t.width, t.url.as_str())), edge)
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
        track_section(&title, tracks, QueueMode::Section)
            .or_else(|| card_section(&title, cards, CardLayout::Carousel))
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

/// A page's shelves and the continuation token of the next page.
type Shelves = Result<(Vec<Section>, Option<String>), String>;

fn first_page(json: &str) -> Shelves {
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

fn next_page(json: &str) -> Shelves {
    let response: Response = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let list = response.continuation_contents.section_list_continuation;
    Ok((list.shelves(), list.continuation()))
}

const ATTEMPTS: u32 = 4;

/// YouTube refuses a share of signed-in feed requests, in bursts, with "400:
/// Request contains an invalid argument", and answers the same one moments later.
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

/// YouTube orders the shelves differently on every load, so each is held back
/// until every one above it in `SHELVES` has arrived, and nothing shown moves.
#[derive(Debug, Clone)]
pub struct Feed {
    held: Vec<Section>,
    /// The next page's, until the last page or `CONTINUATIONS` are read.
    token: Option<String>,
    pages_left: usize,
    /// The place in `SHELVES` of the next shelf to show.
    next: usize,
    /// A visitor ID set for the first page, kept for the rest, as their
    /// tokens may be tied to it.
    visitor: Option<String>,
}

impl Feed {
    /// The first page. `q` must send the sign-in cookie: the feed is personal.
    /// `visitor` replaces one from rustypipe's pool.
    pub async fn first(q: &RustyPipeQuery, visitor: Option<&str>) -> Result<Self, String> {
        let body = serde_json::json!({ "browseId": "FEmusic_home" });
        let q = q.clone().visitor_data_opt(visitor);
        let (held, token) = first_page(&browse(&q, &body, ATTEMPTS).await?)?;
        Ok(Self {
            held,
            token,
            pages_left: CONTINUATIONS,
            next: 0,
            visitor: visitor.map(str::to_owned),
        })
    }

    pub fn done(&self) -> bool {
        self.token.is_none()
    }

    /// Reads the next page. Should it fail, the feed ends with what was read.
    pub async fn next(&mut self, q: &RustyPipeQuery) -> Result<(), String> {
        let Some(token) = self.token.take() else {
            return Ok(());
        };
        let body = serde_json::json!({ "continuation": token });
        let q = q.clone().visitor_data_opt(self.visitor.as_deref());
        let (more, token) = next_page(&browse(&q, &body, ATTEMPTS).await?)?;
        self.held.extend(more);
        self.pages_left -= 1;
        self.token = token.filter(|_| self.pages_left > 0);
        Ok(())
    }

    /// The shelves whose betters in `SHELVES` are all in. Should none come, as
    /// when YouTube renames them, the feed is kept whole rather than left empty.
    pub fn release(&mut self) -> Vec<Section> {
        let done = self.done();
        let wanted = |s: &Section| SHELVES.contains(&s.title());
        let mut out = if done && self.next == 0 && !self.held.iter().any(wanted) {
            std::mem::take(&mut self.held)
        } else {
            let mut out = Vec::new();
            while let Some(name) = SHELVES.get(self.next) {
                match self.held.iter().position(|s| s.title() == *name) {
                    Some(i) => out.push(self.held.remove(i)),
                    None if !done => break,
                    None => {}
                }
                self.next += 1;
            }
            out
        };
        // Once one has shown the feed won't be kept whole, so hold only the
        // shelves still to come, and read no further page once all have.
        if self.next > 0 {
            let rest = &SHELVES[self.next..];
            self.held.retain(|s| rest.contains(&s.title()));
            if rest.is_empty() {
                self.token = None;
            }
        }
        // List shelves such as Quick picks play with a radio after them, as
        // YouTube Music does, rather than the shelf's ten on their own.
        for section in &mut out {
            if let Section::Tracks { mode, .. } = section {
                *mode = QueueMode::Radio;
            }
        }
        out
    }
}

/// A whole feed, such as the saved one, as `Feed::release` shows it.
pub fn arrange(sections: Vec<Section>) -> Vec<Section> {
    let mut feed = Feed {
        held: sections,
        token: None,
        pages_left: 0,
        next: 0,
        visitor: None,
    };
    feed.release()
}

/// Mixes the latest of `recent`, songs played newest first, into Listen
/// again, keeping its length, or makes the shelf of them when the feed has
/// none. A song YouTube shows already stays where it is.
pub fn add_recent<'a>(sections: &mut Vec<Section>, recent: impl Iterator<Item = &'a Track>) {
    fn card(track: &Track) -> Card {
        Card {
            kind: CardKind::Song,
            id: track.id.clone(),
            title: track.title.clone(),
            subtitle: track.artist_names(),
            thumb: track
                .thumb
                .as_deref()
                .map(|url| resize_thumb(url, CARD_THUMB)),
            color: None,
            track: Some(track.clone()),
        }
    }
    match sections.iter_mut().find(|s| s.title() == LISTEN_AGAIN) {
        Some(Section::Cards { cards, .. }) => {
            let len = cards.len();
            // Slots first, so a card is made only for a slot within the shelf.
            let slots = (RECENT_EVERY..len).step_by(RECENT_EVERY + 1).take(RECENT);
            let fresh: Vec<(usize, Card)> = slots
                .zip(recent.filter(|t| !cards.iter().any(|c| c.id == t.id)))
                .map(|(slot, track)| (slot, card(track)))
                .collect();
            for (slot, card) in fresh {
                cards.insert(slot, card);
            }
            cards.truncate(len);
        }
        Some(Section::Tracks { .. }) => {}
        None => {
            let cards: Vec<Card> = recent.take(RECENT).map(card).collect();
            if !cards.is_empty() {
                sections.insert(
                    0,
                    Section::Cards {
                        title: LISTEN_AGAIN.to_owned(),
                        cards,
                        layout: CardLayout::Carousel,
                    },
                );
            }
        }
    }
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

    fn track(id: &str) -> Track {
        Track {
            id: id.to_owned(),
            title: id.to_owned(),
            ..Track::default()
        }
    }

    fn shelf(title: &str, ids: &[&str]) -> Section {
        Section::Cards {
            title: title.to_owned(),
            cards: ids
                .iter()
                .map(|id| Card {
                    kind: CardKind::Song,
                    id: (*id).to_owned(),
                    title: (*id).to_owned(),
                    subtitle: String::new(),
                    thumb: None,
                    color: None,
                    track: Some(track(id)),
                })
                .collect(),
            layout: CardLayout::Carousel,
        }
    }

    fn ids(section: &Section) -> Vec<&str> {
        match section {
            Section::Cards { cards, .. } => cards.iter().map(|c| c.id.as_str()).collect(),
            Section::Tracks { tracks, .. } => tracks.iter().map(|t| t.id.as_str()).collect(),
        }
    }

    fn titles(sections: &[Section]) -> Vec<&str> {
        sections.iter().map(Section::title).collect()
    }

    fn feed(held: Vec<Section>) -> Feed {
        Feed {
            held,
            token: Some("next".to_owned()),
            pages_left: 3,
            next: 0,
            visitor: None,
        }
    }

    #[test]
    fn releases_shelves_from_the_top_down() {
        let mut feed = feed(vec![
            shelf("Forgotten favorites", &["f"]),
            shelf("Recaps", &["r"]),
            shelf("Listen again", &["a"]),
        ]);
        let mut shown = feed.release();
        assert_eq!(
            titles(&shown),
            ["Listen again"],
            "Forgotten favorites waits for Quick picks"
        );
        assert_eq!(
            titles(&feed.held),
            ["Forgotten favorites"],
            "a shelf Home will not show is let go once one has shown"
        );

        let quick_picks = Section::Tracks {
            title: "Quick picks".to_owned(),
            tracks: vec![track("q")],
            mode: QueueMode::Section,
            numbered: false,
        };
        feed.held.extend([
            shelf("Mixed for you", &["m"]),
            quick_picks,
            shelf("Listen again", &["b"]),
        ]);
        shown.extend(feed.release());
        assert_eq!(
            titles(&shown),
            ["Listen again", "Quick picks", "Forgotten favorites"],
            "Mixed for you waits for Trending songs for you, which may yet come"
        );
        assert!(
            matches!(
                shown[1],
                Section::Tracks {
                    mode: QueueMode::Radio,
                    ..
                }
            ),
            "a Quick pick plays with a radio after it"
        );

        feed.token = None;
        shown.extend(feed.release());
        assert_eq!(
            titles(&shown),
            [
                "Listen again",
                "Quick picks",
                "Forgotten favorites",
                "Mixed for you"
            ],
            "the rest once the feed ends"
        );
        assert_eq!(ids(&shown[0]), ["a"], "the first of two with one title");
    }

    #[test]
    fn keeps_a_renamed_feed_whole_once_it_ends() {
        let mut feed = feed(vec![shelf("Neu", &["n"]), shelf("Alt", &["o"])]);
        assert!(!feed.done());
        assert!(
            feed.release().is_empty(),
            "nothing yet: Listen again may come"
        );
        feed.token = None;
        assert_eq!(titles(&feed.release()), ["Neu", "Alt"]);
    }

    #[test]
    fn stops_reading_once_every_shelf_has_shown() {
        let mut feed = feed(SHELVES.iter().map(|name| shelf(name, &["x"])).collect());
        assert_eq!(feed.release().len(), SHELVES.len());
        assert!(feed.done(), "no page could add a shelf");
    }

    #[test]
    fn arranges_a_whole_feed() {
        let arranged = arrange(vec![
            shelf("Quick picks", &["q"]),
            shelf("Recaps", &["r"]),
            shelf("Listen again", &["a"]),
        ]);
        assert_eq!(titles(&arranged), ["Listen again", "Quick picks"]);
    }

    #[test]
    fn listen_again_mixes_in_recent_plays() {
        let mut sections = vec![
            shelf("Listen again", &["a", "b", "c", "d", "e", "f", "g"]),
            shelf("Mixed for you", &["m"]),
        ];
        let recent = [track("c"), track("x"), track("y"), track("z")];
        add_recent(&mut sections, recent.iter());
        assert_eq!(
            ids(&sections[0]),
            ["a", "b", "c", "x", "d", "e", "f"],
            "YouTube's lead, one recent play after every three, c left where it was, length kept"
        );
        assert_eq!(ids(&sections[1]), ["m"]);

        let youtube: Vec<String> = (0..19).map(|i| i.to_string()).collect();
        let youtube: Vec<&str> = youtube.iter().map(String::as_str).collect();
        let mut sections = vec![shelf("Listen again", &youtube)];
        let recent: Vec<Track> = ["r1", "r2", "r3", "r4", "r5"].map(track).into();
        add_recent(&mut sections, recent.iter());
        let shown = ids(&sections[0]);
        assert_eq!(shown.len(), 19);
        let at: Vec<usize> = (0..19).filter(|&i| shown[i].starts_with('r')).collect();
        assert_eq!(at, [3, 7, 11, 15], "as many as fit, never first");

        let mut sections = vec![shelf("Quick picks", &["q"])];
        let recent = [track("c"), track("d")];
        add_recent(&mut sections, recent.iter());
        assert_eq!(
            sections[0].title(),
            "Listen again",
            "made when the feed has none"
        );
        assert_eq!(ids(&sections[0]), ["c", "d"]);

        let mut sections = vec![shelf("Listen again", &["a"])];
        add_recent(&mut sections, std::iter::empty());
        assert_eq!(ids(&sections[0]), ["a"], "unchanged with nothing played");
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
