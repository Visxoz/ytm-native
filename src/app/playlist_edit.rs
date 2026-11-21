//! Edits to the account's own playlists, from the page showing one. Songs
//! removed or moved show so at once, and go back should YouTube refuse.

use std::{ops::RangeInclusive, rc::Rc, time::Instant};

use slint::{ComponentHandle, Model};

use super::{App, HistoryEntry, items::track_row, s};
use crate::{
    Api as ApiGlobal, RowItem, RowKind,
    api::{self, Card, Page, PlaylistPrivacy, Route, Section, Track},
    settings,
};

/// The playlist an edit is made to.
struct Target {
    id: String,
    account: u64,
}

fn songs(page: &mut Page) -> Option<&mut Vec<Track>> {
    match page.sections.first_mut() {
        Some(Section::Tracks { tracks, .. }) => Some(tracks),
        _ => None,
    }
}

/// Counts `change` more songs in the header, whose count takes in those past
/// the ones loaded.
fn recount(page: &mut Page, change: i64) {
    let count = song_count(page).map(|n| n.saturating_add_signed(change));
    let meta = api::playlist_meta(count, page.header_tracks());
    if let Some(header) = &mut page.header {
        header.count = count;
        header.meta = meta;
    }
}

fn song_count(page: &Page) -> Option<u64> {
    page.header.as_ref()?.count
}

/// Takes the entry `entry` out of the page's songs: where it was, the song,
/// and the entry that followed it.
fn take(page: &mut Page, entry: &str) -> Option<(usize, Track, Option<String>)> {
    let tracks = songs(page)?;
    let at = tracks
        .iter()
        .position(|t| t.set_video_id.as_deref() == Some(entry))?;
    let track = tracks.remove(at);
    let next = tracks.get(at).and_then(|t| t.set_video_id.clone());
    recount(page, -1);
    Some((at, track, next))
}

/// Puts `track` among the page's songs before the entry `before`, last
/// without one, or at `at` should that entry be gone too; answers where.
/// None should the songs hold it already, as a fresh copy would.
fn put(page: &mut Page, track: Track, before: Option<&str>, at: usize) -> Option<usize> {
    let tracks = songs(page)?;
    if tracks.iter().any(|t| t.set_video_id == track.set_video_id) {
        return None;
    }
    let at = match before {
        None => tracks.len(),
        Some(before) => tracks
            .iter()
            .position(|t| t.set_video_id.as_deref() == Some(before))
            .unwrap_or(at.min(tracks.len())),
    };
    tracks.insert(at, track);
    recount(page, 1);
    Some(at)
}

/// Moves the entry `entry` before `before`, as `put` places it: from where to
/// where, and the entry that followed it.
fn shift(
    page: &mut Page,
    entry: &str,
    before: Option<&str>,
    at: usize,
) -> Option<(usize, usize, Option<String>)> {
    let (from, track, next) = take(page, entry)?;
    Some((from, put(page, track, before, at)?, next))
}

/// The entry song `from` lands before at `to`, `Some(None)` for the end.
fn successor(page: &Page, from: usize, to: usize) -> Option<Option<String>> {
    let tracks = page.header_tracks();
    let mut rest = tracks
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != from)
        .map(|(_, t)| t);
    match rest.nth(to) {
        Some(t) => t.set_video_id.clone().map(Some),
        None => (song_count(page).is_none_or(|n| n <= tracks.len() as u64)).then_some(None),
    }
}

/// Takes `route` out of the history, and the repeats that leaves side by
/// side; answers the entry to go back to should `route` be the one `shown`.
fn forget(
    back: &mut Vec<HistoryEntry>,
    forward: &mut Vec<HistoryEntry>,
    route: &Route,
    shown: &Route,
) -> Option<HistoryEntry> {
    for stack in [&mut *back, &mut *forward] {
        stack.retain(|e| e.route != *route);
        stack.dedup_by(|a, b| a.route == b.route);
    }
    let to = (shown == route).then(|| back.pop()).flatten();
    let here = match &to {
        Some(entry) => entry.route.clone(),
        None if shown == route => Route::Library,
        None => shown.clone(),
    };
    for stack in [back, forward] {
        if stack.last().is_some_and(|e| e.route == here) {
            stack.pop();
        }
    }
    to
}

impl App {
    /// The account's own playlist on screen, loaded whole, which the page's
    /// menus and dragging offer to edit.
    fn own_playlist(&self) -> Option<Target> {
        if !self.ui().global::<ApiGlobal>().get_own_playlist() {
            return None;
        }
        let state = self.state.borrow();
        let Route::Playlist(id) = &state.route else {
            return None;
        };
        Some(Target {
            id: id.clone(),
            account: state.account,
        })
    }

    /// Offers the page's edits should it be one of the account's own
    /// playlists loaded whole, so every entry's place is known.
    pub(super) fn offer_edits(&self) {
        let state = self.state.borrow();
        let header = (state.signed_in && state.page.more.is_none())
            .then_some(state.page.header.as_ref())
            .flatten()
            .filter(|h| h.editable);
        let ui = self.ui();
        let api = ui.global::<ApiGlobal>();
        api.set_own_playlist(header.is_some());
        api.set_own_songs(state.page.header_tracks().len() as i32);
        api.set_own_privacy(match header.and_then(|h| h.privacy) {
            Some(PlaylistPrivacy::Public) => 0,
            Some(PlaylistPrivacy::Unlisted) => 1,
            _ => 2,
        });
    }

    /// Applies `edit` to the playlist's cached copy and, while it is on
    /// screen, to that, answering what it gave there.
    fn edit_page<R>(&self, target: &Target, edit: impl Fn(&mut Page) -> R) -> Option<R> {
        let shown = self.own_playlist().is_some_and(|t| t.id == target.id);
        let mut state = self.state.borrow_mut();
        // Signed in or out meanwhile, the playlist is the last account's.
        if state.account != target.account {
            return None;
        }
        let route = Route::Playlist(target.id.clone());
        *state.page_edits.entry(route.clone()).or_default() += 1;
        if let Some((page, at)) = state.page_cache.get_mut(&route) {
            edit(page);
            // As fresh as YouTube's, which may not show the edit yet.
            *at = Instant::now();
        }
        shown.then(|| edit(&mut state.page))
    }

    pub(super) fn header_changed(&self) {
        self.offer_edits();
        self.refresh_header();
    }

    /// The songs' rows follow their column labels.
    fn first_song_row(&self) -> Option<usize> {
        self.find_row(RowKind::Columns).map(|row| row + 1)
    }

    /// Numbers the song rows `songs` by where they now are, which their
    /// callbacks and artwork go by.
    fn renumber(&self, first: usize, songs: RangeInclusive<usize>) {
        for index in songs {
            let r = first + index;
            let Some(mut row) = self.rows.row_data(r).filter(|r| r.kind == RowKind::Track) else {
                break;
            };
            if row.index != index as i32 {
                row.index = index as i32;
                row.track.number = s((index + 1).to_string());
                self.rows.set_row_data(r, row);
            }
        }
    }

    fn take_row(&self, at: usize) {
        if let Some(first) = self.first_song_row()
            && first + at < self.rows.row_count()
        {
            self.rows.remove(first + at);
            self.renumber(first, at..=usize::MAX);
        }
    }

    fn put_row(&self, at: usize, row: RowItem) {
        if let Some(first) = self.first_song_row() {
            self.rows
                .insert((first + at).min(self.rows.row_count()), row);
            self.renumber(first, at..=usize::MAX);
        }
    }

    fn move_row(&self, from: usize, to: usize) {
        let Some(first) = self.first_song_row() else {
            return;
        };
        let Some(row) = self.rows.row_data(first + from) else {
            return;
        };
        self.rows.remove(first + from);
        self.rows
            .insert((first + to).min(self.rows.row_count()), row);
        self.renumber(first, from.min(to)..=from.max(to));
    }

    /// Shows a rename or deletion in the sidebar at once, as YouTube's list
    /// can take a moment to.
    fn edit_library(self: &Rc<Self>, edit: impl FnOnce(&mut Vec<Card>)) {
        let cards = {
            let mut state = self.state.borrow_mut();
            // A load already asked for may predate the edit.
            state.library_generation += 1;
            edit(&mut state.library);
            state.library.clone()
        };
        self.list_library();
        self.spawn(async move { settings::save_library(&cards) }, |_, ()| {});
    }

    /// The header's rename, privacy and delete; the first and last ask first.
    pub(super) fn edit_playlist(self: &Rc<Self>, action: &str) {
        let Some(target) = self.own_playlist() else {
            return;
        };
        let privacy = match action {
            "public" => PlaylistPrivacy::Public,
            "unlisted" => PlaylistPrivacy::Unlisted,
            "private" => PlaylistPrivacy::Private,
            "rename" | "delete" => {
                let ui = self.ui();
                let title = self
                    .state
                    .borrow()
                    .page
                    .header
                    .as_ref()
                    .map(|h| s(&h.title));
                ui.set_edit_playlist(s(&target.id));
                ui.set_edit_title(title.unwrap_or_default());
                if action == "rename" {
                    ui.set_rename_open(true);
                } else {
                    ui.set_delete_open(true);
                }
                return;
            }
            _ => return,
        };
        let current = self
            .state
            .borrow()
            .page
            .header
            .as_ref()
            .and_then(|h| h.privacy);
        if current == Some(privacy) {
            return;
        }
        let api = self.api.clone();
        let id = target.id.clone();
        self.spawn(
            async move { api.set_playlist_privacy(&id, privacy).await },
            move |app, result| match result {
                Ok(()) => {
                    let shown = app.edit_page(&target, |page| {
                        if let Some(header) = &mut page.header {
                            header.privacy = Some(privacy);
                        }
                    });
                    if shown.is_some() {
                        app.header_changed();
                    }
                    app.toast(match privacy {
                        PlaylistPrivacy::Public => "Anyone can find the playlist now",
                        PlaylistPrivacy::Unlisted => {
                            "Anyone with the link can see the playlist now"
                        }
                        PlaylistPrivacy::Private => "Only you can see the playlist now",
                    });
                }
                Err(e) => app.toast(format!("Couldn't change who can see the playlist: {e}")),
            },
        );
    }

    pub(super) fn rename_playlist(self: &Rc<Self>, id: String, name: &str) {
        let name = name.trim().to_owned();
        if name.is_empty() || name == self.ui().get_edit_title().as_str() {
            return;
        }
        let target = Target {
            id,
            account: self.state.borrow().account,
        };
        let api = self.api.clone();
        let (id, title) = (target.id.clone(), name.clone());
        self.spawn(
            async move { api.rename_playlist(&id, &title).await },
            move |app, result| match result {
                Ok(()) => {
                    let shown = app.edit_page(&target, |page| {
                        if let Some(header) = &mut page.header {
                            header.title.clone_from(&name);
                        }
                    });
                    if shown.is_some() {
                        app.header_changed();
                    }
                    if app.state.borrow().account != target.account {
                        return;
                    }
                    // The Library page and the sidebar show its name.
                    app.state.borrow_mut().page_cache.remove(&Route::Library);
                    app.edit_library(|cards| {
                        if let Some(card) = cards.iter_mut().find(|c| c.id == target.id) {
                            card.title.clone_from(&name);
                        }
                    });
                }
                Err(e) => app.toast(format!("Couldn't rename the playlist: {e}")),
            },
        );
    }

    /// Deletes the playlist and, should it be on screen, leaves it for the
    /// page before, or the library.
    pub(super) fn delete_playlist(self: &Rc<Self>, id: String) {
        let title = self.ui().get_edit_title().to_string();
        let account = self.state.borrow().account;
        let api = self.api.clone();
        let asked = id.clone();
        self.spawn(
            async move { api.delete_playlist(&asked).await },
            move |app, result| {
                if let Err(e) = result {
                    app.toast(format!("Couldn't delete \u{201c}{title}\u{201d}: {e}"));
                    return;
                }
                app.toast(format!("Deleted \u{201c}{title}\u{201d}"));
                if app.state.borrow().account != account {
                    return;
                }
                let route = Route::Playlist(id.clone());
                let (shown, back) = {
                    let mut state = app.state.borrow_mut();
                    let state = &mut *state;
                    state.page_cache.remove(&route);
                    state.page_cache.remove(&Route::Library);
                    // Neither Back nor Forward is to land on it.
                    let back = forget(&mut state.back, &mut state.forward, &route, &state.route);
                    (state.route == route, back)
                };
                if app.state.borrow().settings.pinned.contains(&id) {
                    app.update_settings(|s| s.pinned.retain(|p| *p != id));
                }
                app.edit_library(|cards| cards.retain(|c| c.id != id));
                if !shown {
                    app.update_nav(&app.ui());
                    return;
                }
                match back {
                    Some(entry) => {
                        app.state.borrow_mut().restore_scroll = Some(entry.scroll);
                        app.navigate(entry.route, false);
                    }
                    None => app.navigate(Route::Library, false),
                }
            },
        );
    }

    pub(super) fn remove_from_playlist(self: &Rc<Self>, section: usize, track: Track) {
        let Some(target) = self.own_playlist().filter(|_| section == 0) else {
            return;
        };
        let Some(entry) = track.set_video_id.clone() else {
            self.toast("Can't remove the song");
            return;
        };
        let Some((at, _, next)) = self.edit_page(&target, |page| take(page, &entry)).flatten()
        else {
            return;
        };
        self.take_row(at);
        self.header_changed();
        let api = self.api.clone();
        let (id, removed) = (target.id.clone(), [track.clone()]);
        self.spawn(
            async move { api.remove_from_playlist(&id, &removed).await },
            move |app, result| match result {
                // The Library page shows each playlist's song count.
                Ok(()) => {
                    app.state.borrow_mut().page_cache.remove(&Route::Library);
                }
                Err(e) => {
                    let back = app.edit_page(&target, |page| {
                        put(page, track.clone(), next.as_deref(), at)
                    });
                    if let Some(Some(at)) = back {
                        let downloaded = app.state.borrow().downloads.contains(&track.id);
                        app.put_row(at, track_row(&track, at, false, 0, downloaded));
                        app.header_changed();
                    }
                    app.toast(format!(
                        "Couldn't remove \u{201c}{}\u{201d}: {e}",
                        track.title
                    ));
                }
            },
        );
    }

    /// Moves song `from` of the playlist on screen, the song `id`, to `to`, as
    /// dragged; not should the rows have changed under the drag.
    pub(super) fn move_in_playlist(self: &Rc<Self>, from: usize, to: usize, id: &str) {
        let Some(target) = self.own_playlist() else {
            return;
        };
        let (entry, before, to) = {
            let state = self.state.borrow();
            let tracks = state.page.header_tracks();
            let to = to.min(tracks.len().saturating_sub(1));
            let Some(entry) = tracks
                .get(from)
                .filter(|t| t.id == id)
                .and_then(|t| t.set_video_id.clone())
            else {
                return;
            };
            if from == to {
                return;
            }
            let Some(before) = successor(&state.page, from, to) else {
                drop(state);
                self.toast("Can't move the song there");
                return;
            };
            (entry, before, to)
        };
        let moved = self.edit_page(&target, |page| shift(page, &entry, before.as_deref(), to));
        let Some(Some((from, to, after))) = moved else {
            return;
        };
        self.move_row(from, to);
        let api = self.api.clone();
        let (id, moving) = (target.id.clone(), entry.clone());
        self.spawn(
            async move { api.move_in_playlist(&id, &moving, before.as_deref()).await },
            move |app, result| {
                let Err(e) = result else { return };
                // Back before the entry it followed.
                let back =
                    app.edit_page(&target, |page| shift(page, &entry, after.as_deref(), from));
                if let Some(Some((now, back, _))) = back {
                    app.move_row(now, back);
                }
                app.toast(format!("Couldn't move the song: {e}"));
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Header, QueueMode};

    fn page(entries: &[&str]) -> Page {
        let tracks: Vec<Track> = entries
            .iter()
            .map(|e| Track {
                id: format!("video{e}"),
                set_video_id: Some((*e).to_owned()),
                duration: Some(60),
                ..Default::default()
            })
            .collect();
        Page {
            header: Some(Header {
                count: Some(entries.len() as u64 + 500),
                meta: api::playlist_meta(Some(entries.len() as u64 + 500), &tracks),
                ..Default::default()
            }),
            sections: vec![Section::Tracks {
                title: String::new(),
                tracks,
                mode: QueueMode::Section,
                numbered: false,
            }],
            more: None,
        }
    }

    fn order(page: &Page) -> Vec<&str> {
        page.header_tracks()
            .iter()
            .filter_map(|t| t.set_video_id.as_deref())
            .collect()
    }

    #[test]
    fn moves_before_the_song_then_there() {
        let mut p = page(&["a", "b", "c", "d"]);
        assert_eq!(successor(&p, 3, 0), Some(Some("a".into())));
        assert_eq!(successor(&p, 0, 2), Some(Some("d".into())));
        // The page's end isn't the playlist's: it has 500 more.
        assert_eq!(successor(&p, 0, 3), None);
        let mut whole = page(&["a", "b"]);
        whole.header = Some(Header {
            count: Some(2),
            ..Default::default()
        });
        assert_eq!(successor(&whole, 0, 1), Some(None), "last");
        if let Some(Section::Tracks { tracks, .. }) = whole.sections.first_mut() {
            tracks[0].set_video_id = None;
        }
        assert_eq!(successor(&whole, 1, 0), None, "no entry ID to go by");

        let before = successor(&p, 3, 0).flatten();
        assert_eq!(shift(&mut p, "d", before.as_deref(), 0), Some((3, 0, None)));
        assert_eq!(order(&p), ["d", "a", "b", "c"]);
        // Undone, it goes back to the end, where nothing followed it.
        assert_eq!(shift(&mut p, "d", None, 3), Some((0, 3, Some("a".into()))));
        assert_eq!(order(&p), ["a", "b", "c", "d"]);
        assert!(p.header.unwrap().meta.starts_with("504 songs"));
    }

    #[test]
    fn forgets_a_deleted_page_and_the_repeats_it_leaves() {
        let entries = |routes: &[&str]| -> Vec<HistoryEntry> {
            routes
                .iter()
                .map(|r| HistoryEntry {
                    route: Route::Playlist((*r).to_owned()),
                    scroll: 0.0,
                })
                .collect()
        };
        let ids = |stack: &[HistoryEntry]| -> Vec<Route> {
            stack.iter().map(|e| e.route.clone()).collect()
        };
        let gone = Route::Playlist("p".into());
        // A p A, shown A: Back steps over neither p nor A.
        let (mut back, mut forward) = (entries(&["x", "a", "p", "a"]), entries(&["b", "p"]));
        let a = Route::Playlist("a".into());
        assert!(forget(&mut back, &mut forward, &gone, &a).is_none());
        assert_eq!(ids(&back), ids(&entries(&["x"])));
        assert_eq!(ids(&forward), ids(&entries(&["b"])));
        // Shown, it goes back to the page before, which Forward doesn't repeat.
        let (mut back, mut forward) = (entries(&["x", "a"]), entries(&["a"]));
        let to = forget(&mut back, &mut forward, &gone, &gone);
        assert_eq!(to.map(|e| e.route), Some(a));
        assert_eq!(ids(&back), ids(&entries(&["x"])));
        assert!(forward.is_empty());
    }

    #[test]
    fn removes_and_puts_back_by_entry() {
        let mut p = page(&["a", "b", "c"]);
        let (at, track, next) = take(&mut p, "b").unwrap();
        assert_eq!((at, next.as_deref()), (1, Some("c")));
        assert_eq!(order(&p), ["a", "c"]);
        assert!(p.header.as_ref().unwrap().meta.starts_with("502 songs"));
        assert!(take(&mut p, "b").is_none(), "already gone");

        // A song removed before it meanwhile doesn't put it out of place.
        take(&mut p, "a").unwrap();
        assert_eq!(put(&mut p, track.clone(), next.as_deref(), at), Some(0));
        assert_eq!(order(&p), ["b", "c"]);
        // With what followed it gone too, it goes back where it was.
        let mut q = page(&["a", "c"]);
        take(&mut q, "c").unwrap();
        assert_eq!(put(&mut q, track.clone(), Some("c"), 1), Some(1));
        assert_eq!(order(&q), ["a", "b"]);
        // A fresh copy that still holds it isn't given it twice.
        assert_eq!(put(&mut q, track, None, 0), None);
        assert_eq!(order(&q), ["a", "b"]);
    }
}
