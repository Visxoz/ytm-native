//! Tracks and cards as the UI's row and card items.

use super::s;
use crate::{
    CardItem, RowItem, RowKind, TrackItem,
    api::{self, Card, CardKind, Track},
    images,
};

fn track_item(track: &Track, number: usize) -> TrackItem {
    TrackItem {
        id: s(&track.id),
        title: s(&track.title),
        artists: s(track.artist_names()),
        album: s(track.album_name()),
        duration: s(track.duration.map(api::format_duration).unwrap_or_default()),
        art: images::cached_or_empty(track.thumb.as_deref(), images::ROW),
        number: s(number.to_string()),
        has_artist: track.first_artist_id().is_some(),
        has_album: track.album.is_some(),
    }
}

pub(super) fn track_row(track: &Track, index: usize, numbered: bool, section: usize) -> RowItem {
    let number = if numbered {
        track.track_nr.map_or(index + 1, usize::from)
    } else {
        index + 1
    };
    RowItem {
        kind: RowKind::Track,
        track: track_item(track, number),
        numbered,
        show_album: !numbered,
        section: section as i32,
        index: index as i32,
        ..Default::default()
    }
}

pub(super) fn card_item(card: &Card, edge: u32) -> CardItem {
    let tint = card.color.map_or(slint::Color::default(), |c| {
        slint::Color::from_argb_encoded(0xFF00_0000 | (c & 0x00FF_FFFF))
    });
    CardItem {
        kind: s(card.kind.as_str()),
        id: s(&card.id),
        title: s(&card.title),
        subtitle: s(&card.subtitle),
        art: images::cached_or_empty(card.thumb.as_deref(), edge),
        round: card.kind == CardKind::Artist,
        tint,
    }
}
