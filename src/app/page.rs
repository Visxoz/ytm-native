//! Turns a loaded page into rows, and fills in its artwork and the rest of
//! a long playlist as they arrive.

use std::rc::Rc;

use slint::{Model, ModelRc, VecModel};

use super::{
    App,
    items::{card_item, track_row},
    s,
};
use crate::{
    CardItem, RowItem, RowKind,
    api::{CardLayout, Page, Route, Section},
    images,
};

type CardArt = (Rc<VecModel<CardItem>>, Vec<Option<String>>, u32);

impl App {
    pub(super) fn show_page(self: &Rc<Self>, page: Page) {
        let (generation, route) = {
            let state = self.state.borrow();
            (state.page_generation, state.route.clone())
        };
        let mut rows = Vec::new();
        // (row, thumbnail) for track artwork to fetch.
        let mut track_art = Vec::new();
        // Each card model with the thumbnails its cards still need.
        let mut card_art: Vec<CardArt> = Vec::new();

        if route == Route::Home {
            rows.push(RowItem {
                kind: RowKind::Greeting,
                title: s(greeting()),
                ..Default::default()
            });
        }

        let header_edge = match &page.header {
            Some(h) if h.banner => images::BANNER,
            _ => images::HEADER,
        };
        if let Some(header) = &page.header {
            rows.push(RowItem {
                kind: RowKind::Header,
                label: s(&header.label),
                title: s(&header.title),
                subtitle: s(&header.subtitle),
                meta: s(&header.meta),
                description: s(&header.description),
                art: header
                    .thumb
                    .as_deref()
                    .and_then(|url| images::cached(url, header_edge))
                    .unwrap_or_default(),
                round: header.round,
                banner: header.banner,
                can_play: !header.tracks.is_empty(),
                can_shuffle: !header.tracks.is_empty() || header.shuffle_playlist.is_some(),
                can_radio: header.radio_id.is_some(),
                ..Default::default()
            });
        }

        for (section_index, section) in page.sections.iter().enumerate() {
            let title = match section {
                Section::Tracks { title, .. } | Section::Cards { title, .. } => title,
            };
            if !title.is_empty() {
                rows.push(RowItem {
                    kind: RowKind::Title,
                    title: s(title),
                    ..Default::default()
                });
            }
            match section {
                Section::Tracks {
                    tracks, numbered, ..
                } => {
                    // Album and playlist track lists get column labels.
                    if title.is_empty() {
                        rows.push(RowItem {
                            kind: RowKind::Columns,
                            show_album: !*numbered,
                            ..Default::default()
                        });
                    }
                    for (i, track) in tracks.iter().enumerate() {
                        if track.thumb.is_some() && !*numbered {
                            track_art.push((rows.len(), track.thumb.clone()));
                        }
                        rows.push(track_row(track, i, *numbered, section_index));
                    }
                }
                Section::Cards { cards, layout, .. } => {
                    let edge = match layout {
                        CardLayout::Top => images::TOP,
                        _ => images::CARD,
                    };
                    let model = Rc::new(VecModel::from(
                        cards.iter().map(|c| card_item(c, edge)).collect::<Vec<_>>(),
                    ));
                    card_art.push((
                        Rc::clone(&model),
                        cards.iter().map(|c| c.thumb.clone()).collect(),
                        edge,
                    ));
                    rows.push(RowItem {
                        kind: match layout {
                            CardLayout::Carousel => RowKind::Carousel,
                            CardLayout::Grid => RowKind::Grid,
                            CardLayout::Chips => RowKind::Chips,
                            CardLayout::Top => RowKind::Top,
                        },
                        cards: ModelRc::from(model),
                        section: section_index as i32,
                        ..Default::default()
                    });
                }
            }
        }

        let ui = self.ui();
        let header_thumb = page.header.as_ref().and_then(|h| h.thumb.clone());
        ui.set_page_title(s(page.header.as_ref().map_or("", |h| h.title.as_str())));
        ui.set_page_can_play(page.header.as_ref().is_some_and(|h| !h.tracks.is_empty()));
        let tint = header_thumb
            .as_deref()
            .and_then(|url| images::accent(url, header_edge))
            .unwrap_or_else(|| route_tint(&route));
        ui.set_accent(tint);

        self.state.borrow_mut().page = page;
        self.rows.set_vec(rows);
        let scroll = self.state.borrow_mut().restore_scroll.take().unwrap_or(0.0);
        ui.set_page_scroll(scroll);

        let accent_url = header_thumb.clone();
        self.load_image(header_thumb, header_edge, move |app, image| {
            if app.state.borrow().page_generation != generation {
                return;
            }
            if let Some(color) = accent_url
                .as_deref()
                .and_then(|url| images::accent(url, header_edge))
            {
                app.ui().set_accent(color);
            }
            if let Some(index) = (0..app.rows.row_count()).find(|&i| {
                app.rows
                    .row_data(i)
                    .is_some_and(|r| r.kind == RowKind::Header)
            }) {
                let mut row = app.rows.row_data(index).unwrap_or_default();
                row.art = image;
                app.rows.set_row_data(index, row);
            }
        });
        for (row_index, url) in track_art {
            self.load_row_art(row_index, url, generation);
        }
        for (model, urls, edge) in card_art {
            for (i, url) in urls.into_iter().enumerate() {
                let model = Rc::clone(&model);
                self.load_image(url, edge, move |_, image| {
                    if let Some(mut card) = model.row_data(i) {
                        card.art = image;
                        model.set_row_data(i, card);
                    }
                });
            }
        }
        if self.state.borrow().page.more.is_some() {
            self.complete_page(route, generation);
        }
    }

    fn load_row_art(self: &Rc<Self>, row_index: usize, url: Option<String>, generation: u64) {
        self.load_image(url, images::ROW, move |app, image| {
            if app.state.borrow().page_generation != generation {
                return;
            }
            if let Some(mut row) = app.rows.row_data(row_index) {
                row.track.art = image;
                app.rows.set_row_data(row_index, row);
            }
        });
    }

    /// Fetches the rest of a long playlist after its first page shows.
    fn complete_page(self: &Rc<Self>, route: Route, generation: u64) {
        let page = self.state.borrow().page.clone();
        let api = self.api.clone();
        self.spawn(async move { api.complete(page).await }, move |app, page| {
            if let Some((cached, _)) = app.state.borrow_mut().page_cache.get_mut(&route) {
                *cached = page.clone();
            }
            if app.state.borrow().page_generation == generation {
                app.append_tracks(page, generation);
            }
        });
    }

    /// Shows the tracks `page` has beyond those of the page on screen.
    fn append_tracks(self: &Rc<Self>, page: Page, generation: u64) {
        let shown = match self.state.borrow().page.sections.first() {
            Some(Section::Tracks { tracks, .. }) => tracks.len(),
            _ => return,
        };
        let Some(Section::Tracks {
            tracks, numbered, ..
        }) = page.sections.first()
        else {
            return;
        };
        let last_row = (0..self.rows.row_count()).rev().find(|&i| {
            self.rows
                .row_data(i)
                .is_some_and(|r| r.kind == RowKind::Track && r.section == 0)
        });
        let Some(last_row) = last_row else { return };
        for (index, track) in tracks.iter().enumerate().skip(shown) {
            let row_index = last_row + 1 + index - shown;
            self.rows
                .insert(row_index, track_row(track, index, *numbered, 0));
            if !*numbered {
                self.load_row_art(row_index, track.thumb.clone(), generation);
            }
        }
        if let Some(header) = &page.header
            && let Some(i) = (0..self.rows.row_count()).find(|&i| {
                self.rows
                    .row_data(i)
                    .is_some_and(|r| r.kind == RowKind::Header)
            })
            && let Some(mut row) = self.rows.row_data(i)
        {
            row.meta = s(&header.meta);
            self.rows.set_row_data(i, row);
        }
        self.state.borrow_mut().page = page;
    }
}

/// "Good morning" and so on, by the local time.
fn greeting() -> &'static str {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    greeting_for(now.hour())
}

fn greeting_for(hour: u8) -> &'static str {
    match hour {
        4..12 => "Good morning",
        12..18 => "Good afternoon",
        18..23 => "Good evening",
        _ => "Good night",
    }
}

/// The header tint for pages without artwork.
fn route_tint(route: &Route) -> slint::Color {
    let rgb = match route {
        Route::Home => 0x5c0f1c,
        Route::Explore => 0x1e3264,
        Route::Library => 0x3d1f5c,
        Route::Search(_) => 0x2a2a2a,
        _ => 0x333333,
    };
    slint::Color::from_argb_encoded(0xff00_0000 | rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets_by_hour() {
        assert_eq!(greeting_for(4), "Good morning");
        assert_eq!(greeting_for(11), "Good morning");
        assert_eq!(greeting_for(12), "Good afternoon");
        assert_eq!(greeting_for(18), "Good evening");
        assert_eq!(greeting_for(23), "Good night");
        assert_eq!(greeting_for(2), "Good night");
    }
}
