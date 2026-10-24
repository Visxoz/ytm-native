//! Shapes rustypipe's items into tracks, cards and sections.

use rustypipe::model::{
    AlbumItem, AlbumType, ArtistId, ArtistItem, MusicGenreItem, MusicPlaylistItem, Thumbnail,
    TrackItem, TrackType,
};

use super::{
    ArtistRef, Card, CardKind, CardLayout, QueueMode, Section, Track,
    format::{compact, songs},
};

/// Thumbnail edge, in pixels, wanted for list rows and for cards.
pub const ROW_THUMB: u32 = 120;
pub const CARD_THUMB: u32 = 240;
pub const HEADER_THUMB: u32 = 544;

pub fn join_artists(artists: &[ArtistRef]) -> String {
    artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Picks the smallest thumbnail at least `edge` pixels wide.
pub fn pick_thumb(thumbs: &[Thumbnail], edge: u32) -> Option<String> {
    fit_thumb(thumbs.iter().map(|t| (t.width, t.url.as_str())), edge)
}

/// Of thumbnails given as (width, URL), the smallest at least `edge`
/// pixels wide, else the largest, sized to `edge`.
pub fn fit_thumb<'a>(
    thumbs: impl Iterator<Item = (u32, &'a str)> + Clone,
    edge: u32,
) -> Option<String> {
    let (_, url) = thumbs
        .clone()
        .filter(|(width, _)| *width >= edge)
        .min_by_key(|(width, _)| *width)
        .or_else(|| thumbs.max_by_key(|(width, _)| *width))?;
    Some(resize_thumb(url, edge))
}

/// googleusercontent thumbnails render at any size given in the URL suffix.
pub fn resize_thumb(url: &str, edge: u32) -> String {
    if url.contains("googleusercontent.com")
        && let Some(eq) = url.rfind('=')
        && url[eq + 1..].starts_with('w')
    {
        return format!("{}=w{edge}-h{edge}-l90-rj", &url[..eq]);
    }
    url.to_owned()
}

pub fn artist_refs(artists: &[ArtistId]) -> Vec<ArtistRef> {
    artists
        .iter()
        .map(|a| ArtistRef {
            id: a.id.clone(),
            name: a.name.clone(),
        })
        .collect()
}

pub fn track(t: TrackItem) -> Track {
    Track {
        video: t.track_type == TrackType::Video,
        thumb: pick_thumb(&t.cover, ROW_THUMB),
        artists: artist_refs(&t.artists),
        album: t.album.map(|a| (a.id, a.name)),
        duration: t.duration,
        track_nr: t.track_nr,
        id: t.id,
        title: t.name,
    }
}

pub fn tracks(items: Vec<TrackItem>) -> Vec<Track> {
    items.into_iter().map(track).collect()
}

pub fn album_type(t: AlbumType) -> &'static str {
    match t {
        AlbumType::Album => "Album",
        AlbumType::Ep => "EP",
        AlbumType::Single => "Single",
        AlbumType::Audiobook => "Audiobook",
        AlbumType::Show => "Podcast",
        _ => "Album",
    }
}

pub fn album_card(a: AlbumItem) -> Card {
    let mut subtitle = vec![album_type(a.album_type).to_owned()];
    let artists = join_artists(&artist_refs(&a.artists));
    if !artists.is_empty() {
        subtitle.push(artists);
    }
    if let Some(year) = a.year {
        subtitle.push(year.to_string());
    }
    Card {
        kind: CardKind::Album,
        thumb: pick_thumb(&a.cover, CARD_THUMB),
        id: a.id,
        title: a.name,
        subtitle: subtitle.join(" • "),
        color: None,
        track: None,
    }
}

pub fn artist_card(a: ArtistItem) -> Card {
    Card {
        kind: CardKind::Artist,
        thumb: pick_thumb(&a.avatar, CARD_THUMB),
        id: a.id,
        title: a.name,
        subtitle: a.subscriber_count.map_or_else(
            || "Artist".to_owned(),
            |n| format!("{} subscribers", compact(n)),
        ),
        color: None,
        track: None,
    }
}

pub fn playlist_card(p: MusicPlaylistItem) -> Card {
    let mut subtitle = vec!["Playlist".to_owned()];
    if let Some(channel) = &p.channel {
        subtitle.push(channel.name.clone());
    }
    if let Some(n) = p.track_count {
        subtitle.push(songs(n));
    }
    Card {
        kind: CardKind::Playlist,
        thumb: pick_thumb(&p.thumbnail, CARD_THUMB),
        id: p.id,
        title: p.name,
        subtitle: subtitle.join(" • "),
        color: None,
        track: None,
    }
}

pub fn genre_card(g: MusicGenreItem) -> Card {
    Card {
        kind: CardKind::Genre,
        id: g.id,
        title: g.name,
        subtitle: String::new(),
        thumb: None,
        color: Some(g.color),
        track: None,
    }
}

pub fn track_section(title: &str, tracks: Vec<Track>, mode: QueueMode) -> Option<Section> {
    (!tracks.is_empty()).then(|| Section::Tracks {
        title: title.to_owned(),
        tracks,
        mode,
        numbered: false,
    })
}

pub fn card_section(title: &str, cards: Vec<Card>, layout: CardLayout) -> Option<Section> {
    (!cards.is_empty()).then(|| Section::Cards {
        title: title.to_owned(),
        cards,
        layout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Thumbnail is non-exhaustive, so build it the way rustypipe does.
    fn thumb(url: &str, width: u32) -> Thumbnail {
        serde_json::from_value(serde_json::json!({ "url": url, "width": width, "height": width }))
            .unwrap()
    }

    #[test]
    fn picks_smallest_sufficient_thumbnail() {
        let thumbs = [thumb("a", 60), thumb("b", 226), thumb("c", 544)];
        assert_eq!(pick_thumb(&thumbs, 120).as_deref(), Some("b"));
        assert_eq!(pick_thumb(&thumbs, 1000).as_deref(), Some("c"));
        assert_eq!(pick_thumb(&[], 120), None);
    }

    #[test]
    fn resizes_googleusercontent_thumbnails() {
        assert_eq!(
            resize_thumb("https://lh3.googleusercontent.com/abc=w60-h60-l90-rj", 360),
            "https://lh3.googleusercontent.com/abc=w360-h360-l90-rj"
        );
        let ytimg = "https://i.ytimg.com/vi/x/hqdefault.jpg";
        assert_eq!(resize_thumb(ytimg, 360), ytimg);
    }
}
