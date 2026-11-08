//! Editing the queue from its panel: dragging a row into a new place,
//! selecting several to remove or play next, and saving it all as a
//! playlist. Rows change in place, so a long queue is never rebuilt for one
//! move.

use std::rc::Rc;

use slint::Model;

use super::{App, State, now_playing::queue_item};
use crate::api;

/// The rows picked with Ctrl- and Shift-click are flagged on the rows
/// themselves; this is how many, and where a Shift-click selects from.
#[derive(Clone, Copy, Default)]
pub(super) struct Selection {
    count: usize,
    anchor: Option<usize>,
}

impl App {
    /// A click on a queue row: plain plays it, Ctrl toggles its selection,
    /// and Shift selects from the row last clicked up to it.
    pub(super) fn queue_clicked(self: &Rc<Self>, index: usize, control: bool, shift: bool) {
        if !(control || shift) {
            self.clear_selection();
            if self.state.borrow_mut().queue.jump(index).is_some() {
                self.start_current();
            }
            return;
        }
        match self.selection.get().anchor {
            Some(anchor) if shift => {
                for row in anchor.min(index)..=anchor.max(index) {
                    self.select(row, true);
                }
            }
            _ => {
                let on = !self
                    .queue_model
                    .row_data(index)
                    .is_some_and(|item| item.selected);
                self.select(index, on);
                self.selection.set(Selection {
                    anchor: Some(index),
                    ..self.selection.get()
                });
            }
        }
        self.show_selection();
    }

    fn select(&self, row: usize, on: bool) {
        let Some(mut item) = self.queue_model.row_data(row) else {
            return;
        };
        if item.selected == on {
            return;
        }
        item.selected = on;
        self.queue_model.set_row_data(row, item);
        let mut selection = self.selection.get();
        if on {
            selection.count += 1;
        } else {
            selection.count -= 1;
        }
        self.selection.set(selection);
    }

    fn show_selection(&self) {
        self.ui()
            .set_queue_selected(self.selection.get().count as i32);
    }

    pub(super) fn clear_selection(&self) {
        if self.selection.get().count > 0 {
            for row in 0..self.queue_model.row_count() {
                self.select(row, false);
            }
        }
        self.forget_selection();
    }

    /// The rows were rebuilt or cleared, and carry no flags any more.
    pub(super) fn forget_selection(&self) {
        self.selection.set(Selection::default());
        self.show_selection();
    }

    pub(super) fn selected_rows(&self) -> Vec<usize> {
        if self.selection.get().count == 0 {
            return Vec::new();
        }
        (0..self.queue_model.row_count())
            .filter(|&row| {
                self.queue_model
                    .row_data(row)
                    .is_some_and(|item| item.selected)
            })
            .collect()
    }

    /// Puts the row dragged from `from` down at `to`.
    pub(super) fn queue_move(self: &Rc<Self>, from: usize, to: usize) {
        if !self.state.borrow_mut().queue.move_to(from, to) {
            return;
        }
        if let Some(item) = self.queue_model.row_data(from) {
            self.queue_model.remove(from);
            self.queue_model.insert(to, item);
        }
        self.queue_edited();
    }

    pub(super) fn queue_remove(self: &Rc<Self>, rows: Vec<usize>) {
        let removed = self.state.borrow_mut().queue.remove_many(rows);
        if removed.is_empty() {
            return;
        }
        for &row in removed.iter().rev() {
            self.queue_model.remove(row);
        }
        self.queue_edited();
    }

    /// Moves `rows` to right after the current track, in their queue order.
    pub(super) fn queue_play_next(self: &Rc<Self>, mut rows: Vec<usize>) {
        let (current, moved) = {
            let mut state = self.state.borrow_mut();
            let Some(current) = state.queue.current_index() else {
                return;
            };
            // As the queue sees them: in order, once each, never the current.
            rows.sort_unstable();
            rows.dedup();
            rows.retain(|&row| row != current && row < state.queue.len());
            (current, state.queue.play_next_many(rows.iter().copied()))
        };
        if !moved {
            return;
        }
        let items: Vec<_> = rows
            .iter()
            .filter_map(|&row| self.queue_model.row_data(row))
            .collect();
        for &row in rows.iter().rev() {
            self.queue_model.remove(row);
        }
        let current = current - rows.iter().take_while(|&&row| row < current).count();
        for (offset, item) in items.into_iter().enumerate() {
            self.queue_model.insert(current + 1 + offset, item);
        }
        self.queue_edited();
    }

    /// Drops everything after the current track.
    pub(super) fn queue_clear(self: &Rc<Self>) {
        self.state.borrow_mut().queue.clear_upcoming();
        self.refresh_queue();
        self.queue_edited();
    }

    /// Shows the entry just added at `index` without rebuilding the rest.
    pub(super) fn queue_inserted(self: &Rc<Self>, index: usize) {
        let (item, art) = {
            let state = self.state.borrow();
            let Some(track) = state.queue.get(index) else {
                return;
            };
            let current = state.queue.current_index() == Some(index);
            queue_item(track, current, false, false)
        };
        self.queue_model.insert(index, item);
        if let Some(url) = art {
            self.load_queue_art(index, url);
        }
        self.queue_edited();
    }

    /// Shows the recommendations Smart Shuffle put in at `rows`, in the
    /// order it put them in, without rebuilding the rest, so a selection or
    /// a drag survives. Each index already counts the rows put in before it,
    /// and all are after the current track, so the highlight stays put.
    pub(super) fn queue_recommended(self: &Rc<Self>, before: u64, rows: &[usize]) {
        let (current, len) = {
            let state = self.state.borrow();
            (state.queue.current_index(), state.queue.len())
        };
        // A model behind the queue, or a highlight left elsewhere, is rebuilt,
        // as is any row out of range, which would leave the model short.
        if self.queue_shown.get() != Some(before)
            || self.ui().get_queue_current() != current.map_or(-1, |i| i as i32)
            || rows.iter().any(|&i| i >= len)
        {
            self.refresh_queue();
            return;
        }
        let mut selection = self.selection.get();
        let mut missing = Vec::new();
        {
            let state = self.state.borrow();
            for &i in rows {
                let Some(track) = state.queue.get(i) else {
                    continue;
                };
                let (item, art) = queue_item(track, false, true, false);
                missing.extend(art.map(|url| (i, url)));
                self.queue_model.insert(i, item);
                if let Some(anchor) = selection.anchor.as_mut()
                    && *anchor >= i
                {
                    *anchor += 1;
                }
            }
            self.queue_shown.set(Some(state.queue.revision()));
        }
        self.selection.set(selection);
        for (i, url) in missing {
            self.load_queue_art(i, url);
        }
    }

    /// After rows changed in place: the model is at the queue's revision,
    /// a track queued in the player that is no longer next is dropped, and
    /// the new next one is prefetched.
    fn queue_edited(self: &Rc<Self>) {
        self.clear_selection();
        let (revision, current, stale) = {
            let mut state = self.state.borrow_mut();
            let State {
                queue, playback, ..
            } = &mut *state;
            let next = queue.peek_next().map(|t| t.id.as_str());
            let stale = playback
                .queued
                .as_ref()
                .is_some_and(|(id, _)| Some(id.as_str()) != next);
            if stale {
                playback.forget_queued();
            }
            (queue.revision(), queue.current_index(), stale)
        };
        self.queue_shown.set(Some(revision));
        self.ui()
            .set_queue_current(current.map_or(-1, |i| i as i32));
        if stale {
            self.player.clear_next();
        }
        self.persist_soon();
        self.prefetch_next();
    }

    /// Saves the queue as a private playlist named after today.
    pub(super) fn save_queue_as_playlist(self: &Rc<Self>) {
        let ids: Vec<String> = self
            .state
            .borrow()
            .queue
            .items()
            .map(|t| t.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let today = super::local_now();
        let title = format!("Queue {} {} {}", today.day(), today.month(), today.year());
        self.toast(format!(
            "Saving {} as a playlist…",
            api::songs(ids.len() as u64)
        ));
        let api = self.api.clone();
        let name = title.clone();
        self.spawn(
            async move { api.create_playlist(&name, &ids).await },
            move |app, result| match result {
                Ok(_) => {
                    app.toast(format!("Saved as \u{201c}{title}\u{201d}"));
                    app.load_library();
                }
                Err(e) => app.toast(format!("Couldn't save the playlist: {e}")),
            },
        );
    }
}
