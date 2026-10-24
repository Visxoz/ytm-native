//! Smart Shuffle: while shuffling a playlist, album or other collection of
//! the user's own, recommended tracks are mixed in, one after every few of
//! theirs. They come from the radio of one of the user's tracks, a batch at
//! a time as the queue runs, and are marked in the queue so that turning
//! Smart Shuffle off takes them out again.

use std::{
    collections::{HashSet, VecDeque},
    rc::Rc,
};

use super::{App, State};
use crate::{api::Track, queue::Queue};

/// The user's own tracks between two recommendations.
const GAP_MIN: usize = 3;
const GAP_MAX: usize = 4;
/// How many of the user's own tracks ahead get their recommendations; the
/// rest follow as the queue runs, so a long playlist never asks for all.
const LOOKAHEAD: usize = 8;
/// Recently played songs that are not recommended again.
const RECENT: usize = 100;

#[derive(Default)]
pub(super) struct Smart {
    /// Bumped when the queue is replaced or Smart Shuffle turned off, so a
    /// batch asked for an earlier queue is dropped as it arrives.
    generation: u64,
    fetching: bool,
    /// Recommendations fetched and not yet placed.
    pool: VecDeque<Track>,
    /// Own tracks to pass before the next recommendation; zero picks anew.
    gap: usize,
    /// IDs in the queue or recently played, rebuilt when the queue changed
    /// under it, so each check is a lookup.
    known: HashSet<String>,
    known_revision: Option<u64>,
}

impl Smart {
    pub(super) fn reset(&mut self) {
        self.generation += 1;
        self.fetching = false;
        self.pool.clear();
        self.gap = 0;
    }

    /// The next pooled recommendation not already in the queue.
    fn fresh(&mut self) -> Option<Track> {
        while let Some(track) = self.pool.pop_front() {
            if !self.known.contains(&track.id) {
                return Some(track);
            }
        }
        None
    }

    fn fill_known(&mut self, queue: &Queue<Track>, recent: impl Iterator<Item = String>) {
        if self.known_revision == Some(queue.revision()) {
            return;
        }
        self.known = queue.items().map(|t| t.id.clone()).chain(recent).collect();
        self.known_revision = Some(queue.revision());
    }
}

/// A random one of the user's own tracks, to seed a batch from.
fn seed(queue: &mut Queue<Track>) -> Option<String> {
    let own = queue.entries().filter(|(_, r)| !r).count();
    let pick = queue.random_below(own);
    queue
        .entries()
        .filter(|(_, r)| !r)
        .nth(pick)
        .map(|(t, _)| t.id.clone())
}

impl App {
    /// Puts recommendations among the upcoming tracks where they are due,
    /// and asks for a batch when the pool runs dry.
    pub(super) fn smart_top_up(self: &Rc<Self>) {
        let (changed, fetch) = {
            let mut state = self.state.borrow_mut();
            if !(state.settings.smart_shuffle && state.queue_collection) {
                return;
            }
            let State {
                queue,
                smart,
                history,
                ..
            } = &mut *state;
            let Some(current) = queue.current_index() else {
                return;
            };
            if smart.fetching {
                return;
            }
            smart.fill_known(queue, history.tracks().take(RECENT).map(|t| t.id.clone()));
            let mut run = usize::from(!queue.is_recommended(current));
            let mut ahead = 0;
            let mut index = current + 1;
            let mut changed = false;
            let mut dry = false;
            loop {
                if index < queue.len() && queue.is_recommended(index) {
                    run = 0;
                    index += 1;
                    continue;
                }
                if smart.gap == 0 {
                    smart.gap = GAP_MIN + queue.random_below(GAP_MAX - GAP_MIN + 1);
                }
                if run >= smart.gap {
                    let Some(track) = smart.fresh() else {
                        dry = true;
                        break;
                    };
                    smart.known.insert(track.id.clone());
                    queue.insert_recommended(index, track);
                    smart.gap = 0;
                    run = 0;
                    changed = true;
                    index += 1;
                    continue;
                }
                if index >= queue.len() || ahead >= LOOKAHEAD {
                    break;
                }
                run += 1;
                ahead += 1;
                index += 1;
            }
            smart.known_revision = Some(queue.revision());
            let fetch = dry.then(|| seed(queue)).flatten().map(|seed| {
                smart.fetching = true;
                (seed, smart.generation)
            });
            (changed, fetch)
        };
        if changed {
            self.refresh_queue();
        }
        if let Some((seed, generation)) = fetch {
            self.smart_fetch(seed, generation);
        }
    }

    fn smart_fetch(self: &Rc<Self>, seed: String, generation: u64) {
        let api = self.api.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            move |app, result| {
                let fresh = {
                    let mut state = app.state.borrow_mut();
                    let smart = &mut state.smart;
                    if smart.generation != generation {
                        return;
                    }
                    smart.fetching = false;
                    match result {
                        Ok(tracks) => {
                            let known = &smart.known;
                            smart
                                .pool
                                .extend(tracks.into_iter().filter(|t| !known.contains(&t.id)));
                            !smart.pool.is_empty()
                        }
                        Err(e) => {
                            log::debug!("smart shuffle could not fetch recommendations: {e}");
                            false
                        }
                    }
                };
                // Nothing new means nothing to place; the next track start
                // asks again from another seed.
                if fresh {
                    app.prefetch_next();
                }
            },
        );
    }
}
