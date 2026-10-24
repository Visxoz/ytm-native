//! The play queue. Shuffle reorders what comes after the current track, as
//! YouTube Music does, and turning it off restores the original order.
//! Smart Shuffle mixes recommended tracks in among the user's own, marked
//! so they can be taken out again.

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn next(self) -> Self {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }

    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Repeat::All,
            2 => Repeat::One,
            _ => Repeat::Off,
        }
    }
}

#[derive(Debug, Clone)]
struct Entry<T> {
    uid: u64,
    item: T,
    /// Mixed in by Smart Shuffle rather than chosen by the user.
    recommended: bool,
}

#[derive(Debug, Clone)]
pub struct Queue<T> {
    entries: Vec<Entry<T>>,
    current: Option<usize>,
    /// Entry order before shuffling, by uid.
    unshuffled: Option<Vec<u64>>,
    pub repeat: Repeat,
    next_uid: u64,
    rng: u64,
    /// Bumped whenever entries are added, removed or reordered.
    revision: u64,
}

impl<T: Clone> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> Queue<T> {
    pub fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0x9E37_79B9, |d| d.as_nanos() as u64);
        Self::with_seed(seed)
    }

    pub fn with_seed(seed: u64) -> Self {
        Self {
            entries: Vec::new(),
            current: None,
            unshuffled: None,
            repeat: Repeat::Off,
            next_uid: 0,
            rng: seed | 1,
            revision: 0,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn entry(&mut self, item: T) -> Entry<T> {
        self.revision += 1;
        self.next_uid += 1;
        Entry {
            uid: self.next_uid,
            item,
            recommended: false,
        }
    }

    fn random(&mut self) -> u64 {
        // xorshift64
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// A random index below `n`, for shuffled starts.
    pub fn random_below(&mut self, n: usize) -> usize {
        (self.random() % n.max(1) as u64) as usize
    }

    fn shuffle_after_current(&mut self) {
        let start = self.current.map_or(0, |c| c + 1);
        for i in (start + 1..self.entries.len()).rev() {
            let j = start + (self.random() % (i - start + 1) as u64) as usize;
            self.entries.swap(i, j);
        }
    }

    /// Replaces the queue, starting at `start`.
    pub fn set(&mut self, items: Vec<T>, start: usize, shuffle: bool) {
        self.revision += 1;
        self.entries = items.into_iter().map(|item| self.entry(item)).collect();
        self.current = (!self.entries.is_empty()).then(|| start.min(self.entries.len() - 1));
        self.unshuffled = None;
        if shuffle {
            self.set_shuffle(true);
        }
    }

    pub fn set_shuffle(&mut self, on: bool) {
        match (on, self.unshuffled.is_some()) {
            (true, false) => {
                self.revision += 1;
                self.unshuffled = Some(self.entries.iter().map(|e| e.uid).collect());
                self.shuffle_after_current();
            }
            (false, true) => {
                self.revision += 1;
                let order = self.unshuffled.take().unwrap_or_default();
                let current_uid = self.current.map(|c| self.entries[c].uid);
                let rank: HashMap<u64, usize> = order
                    .into_iter()
                    .enumerate()
                    .map(|(i, uid)| (uid, i))
                    .collect();
                // Entries added while shuffled have no place in the old
                // order, so they stay behind the entry they followed; the
                // one playing then never jumps to the end.
                let mut anchor = None;
                let mut keyed: Vec<_> = std::mem::take(&mut self.entries)
                    .into_iter()
                    .enumerate()
                    .map(|(i, e)| {
                        if let Some(&r) = rank.get(&e.uid) {
                            anchor = Some(r);
                        }
                        ((anchor, i), e)
                    })
                    .collect();
                keyed.sort_by_key(|(key, _)| *key);
                self.entries = keyed.into_iter().map(|(_, e)| e).collect();
                self.current =
                    current_uid.and_then(|uid| self.entries.iter().position(|e| e.uid == uid));
            }
            _ => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn current(&self) -> Option<&T> {
        self.current.map(|c| &self.entries[c].item)
    }

    pub fn items(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(|e| &e.item)
    }

    /// Every item, with whether Smart Shuffle put it there.
    pub fn entries(&self) -> impl Iterator<Item = (&T, bool)> {
        self.entries.iter().map(|e| (&e.item, e.recommended))
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.entries.get(index).map(|e| &e.item)
    }

    pub fn is_recommended(&self, index: usize) -> bool {
        self.entries.get(index).is_some_and(|e| e.recommended)
    }

    /// Indices of the entries Smart Shuffle put in.
    pub fn recommended(&self) -> impl Iterator<Item = usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.recommended.then_some(i))
    }

    /// Marks the entries at `indices` as Smart Shuffle's, as restored from
    /// a saved queue.
    pub fn mark_recommended(&mut self, indices: impl IntoIterator<Item = usize>) {
        for index in indices {
            if let Some(entry) = self.entries.get_mut(index) {
                entry.recommended = true;
            }
        }
    }

    /// Puts a recommendation at `index`, which must be after the current
    /// track.
    pub fn insert_recommended(&mut self, index: usize, item: T) {
        let Some(current) = self.current else { return };
        if index <= current || index > self.entries.len() {
            return;
        }
        let mut entry = self.entry(item);
        entry.recommended = true;
        self.entries.insert(index, entry);
    }

    /// Takes out every recommendation but the one playing, leaving the
    /// user's own order as it was. Returns whether any was removed.
    pub fn remove_recommended(&mut self) -> bool {
        let Some(current) = self.current else {
            return false;
        };
        let before = self.entries.len();
        let removed_before = self.entries[..current]
            .iter()
            .filter(|e| e.recommended)
            .count();
        let mut index = 0;
        self.entries.retain(|e| {
            let keep = !e.recommended || index == current;
            index += 1;
            keep
        });
        if self.entries.len() == before {
            return false;
        }
        self.revision += 1;
        self.current = Some(current - removed_before);
        true
    }

    /// The entry that `advance` would move to, without moving.
    pub fn peek_next(&self) -> Option<&T> {
        self.next_index(false).map(|i| &self.entries[i].item)
    }

    fn next_index(&self, user: bool) -> Option<usize> {
        let current = self.current?;
        if self.repeat == Repeat::One && !user {
            return Some(current);
        }
        if current + 1 < self.entries.len() {
            Some(current + 1)
        } else if self.repeat != Repeat::Off {
            Some(0)
        } else {
            None
        }
    }

    /// Moves to the next track. `user` skips ignore repeat-one. Returns
    /// `None` at the end of the queue.
    pub fn advance(&mut self, user: bool) -> Option<&T> {
        let next = self.next_index(user)?;
        self.current = Some(next);
        self.current()
    }

    pub fn back(&mut self) -> Option<&T> {
        let current = self.current?;
        let prev = if current > 0 {
            current - 1
        } else if self.repeat == Repeat::All {
            self.entries.len() - 1
        } else {
            0
        };
        self.current = Some(prev);
        self.current()
    }

    pub fn jump(&mut self, index: usize) -> Option<&T> {
        if index >= self.entries.len() {
            return None;
        }
        self.current = Some(index);
        self.current()
    }

    /// Inserts right after the current track.
    pub fn play_next(&mut self, item: T) {
        let entry = self.entry(item);
        let at = self.current.map_or(self.entries.len(), |c| c + 1);
        self.entries.insert(at, entry);
        if self.current.is_none() {
            self.current = Some(at);
        }
    }

    pub fn push(&mut self, item: T) {
        let entry = self.entry(item);
        self.entries.push(entry);
        if self.current.is_none() {
            self.current = Some(self.entries.len() - 1);
        }
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = T>) {
        for item in items {
            self.push(item);
        }
    }

    /// Removes an entry other than the current one.
    pub fn remove(&mut self, index: usize) -> bool {
        let Some(current) = self.current else {
            return false;
        };
        if index >= self.entries.len() || index == current {
            return false;
        }
        self.revision += 1;
        self.entries.remove(index);
        if index < current {
            self.current = Some(current - 1);
        }
        true
    }

    /// Puts `item` in place of every entry `matches` picks, keeping their
    /// places, so the revision stays. Returns the indices replaced.
    pub fn replace(&mut self, matches: impl Fn(&T) -> bool, item: &T) -> Vec<usize> {
        let mut replaced = Vec::new();
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if matches(&entry.item) {
                entry.item = item.clone();
                replaced.push(index);
            }
        }
        replaced
    }

    /// Drops everything after the current track.
    pub fn clear_upcoming(&mut self) {
        if let Some(current) = self.current {
            self.revision += 1;
            self.entries.truncate(current + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(n: u32) -> Queue<u32> {
        let mut q = Queue::with_seed(42);
        q.set((0..n).collect(), 0, false);
        q
    }

    fn items(q: &Queue<u32>) -> Vec<u32> {
        q.items().copied().collect()
    }

    #[test]
    fn advances_and_stops_at_end() {
        let mut q = queue(3);
        assert_eq!(q.advance(false), Some(&1));
        assert_eq!(q.advance(false), Some(&2));
        assert_eq!(q.advance(false), None);
        assert_eq!(q.current(), Some(&2));
    }

    #[test]
    fn repeat_modes() {
        let mut q = queue(2);
        q.repeat = Repeat::All;
        q.advance(false);
        assert_eq!(q.advance(false), Some(&0));

        q.repeat = Repeat::One;
        assert_eq!(q.advance(false), Some(&0));
        assert_eq!(q.advance(true), Some(&1), "skipping leaves repeat-one");
    }

    #[test]
    fn back_wraps_only_with_repeat_all() {
        let mut q = queue(3);
        assert_eq!(q.back(), Some(&0));
        q.repeat = Repeat::All;
        assert_eq!(q.back(), Some(&2));
    }

    #[test]
    fn replaces_in_place_through_shuffle() {
        let mut q = queue(10);
        q.jump(2);
        q.set_shuffle(true);
        let revision = q.revision();
        let at = q.items().position(|&i| i == 7).unwrap();
        assert_eq!(q.replace(|&i| i == 7, &70), [at]);
        assert_eq!(q.revision(), revision, "nothing moved");
        assert_eq!(q.current(), Some(&2));
        assert!(q.replace(|&i| i == 99, &0).is_empty(), "nothing to replace");

        q.set_shuffle(false);
        assert_eq!(items(&q), [0, 1, 2, 3, 4, 5, 6, 70, 8, 9]);
    }

    #[test]
    fn shuffle_keeps_current_and_restores_order() {
        let mut q = queue(20);
        q.jump(5);
        q.set_shuffle(true);
        let shuffled = items(&q);
        assert_eq!(&shuffled[..6], &[0, 1, 2, 3, 4, 5]);
        assert_ne!(shuffled, (0..20).collect::<Vec<_>>());
        let mut sorted = shuffled.clone();
        sorted.sort();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());

        q.advance(false);
        let playing = *q.current().unwrap();
        q.set_shuffle(false);
        assert_eq!(items(&q), (0..20).collect::<Vec<_>>());
        assert_eq!(q.current(), Some(&playing));
    }

    #[test]
    fn unshuffle_keeps_tracks_added_while_shuffled_behind_their_neighbour() {
        let mut q = queue(5);
        q.set_shuffle(true);
        q.push(99);
        let after = items(&q)[4];
        q.set_shuffle(false);
        let order = items(&q);
        assert_eq!(order.len(), 6);
        let at = order.iter().position(|&i| i == 99).unwrap();
        assert_eq!(
            order[at - 1],
            after,
            "still follows the track it was added after"
        );

        let mut q = queue(5);
        q.jump(2);
        q.set_shuffle(true);
        q.play_next(99);
        q.advance(false);
        assert_eq!(q.current(), Some(&99));
        q.set_shuffle(false);
        assert_eq!(items(&q), [0, 1, 2, 99, 3, 4]);
        assert_eq!(
            q.current(),
            Some(&99),
            "the playing track stays where it is"
        );
        assert_eq!(q.advance(false), Some(&3));
    }

    #[test]
    fn recommendations_go_after_the_current_track_only() {
        let mut q = queue(4);
        q.jump(1);
        let revision = q.revision();
        q.insert_recommended(1, 50);
        q.insert_recommended(0, 50);
        assert_eq!(items(&q), [0, 1, 2, 3], "never at or before the current");
        assert_eq!(q.revision(), revision);
        q.insert_recommended(2, 50);
        q.insert_recommended(5, 51);
        assert_eq!(items(&q), [0, 1, 50, 2, 3, 51]);
        assert_eq!(q.current(), Some(&1));
        assert!(q.is_recommended(2) && q.is_recommended(5));
        assert!(!q.is_recommended(1));
        assert_eq!(q.recommended().collect::<Vec<_>>(), [2, 5]);
        assert_ne!(q.revision(), revision);
    }

    #[test]
    fn removing_recommendations_restores_the_users_order() {
        let mut q = queue(6);
        q.jump(1);
        q.set_shuffle(true);
        let shuffled = items(&q);
        q.insert_recommended(2, 50);
        q.insert_recommended(5, 51);
        q.advance(false);
        q.advance(false);
        assert_eq!(q.current(), Some(&shuffled[2]));
        let revision = q.revision();
        assert!(q.remove_recommended());
        assert_eq!(items(&q), shuffled, "the user's shuffled order is back");
        assert_eq!(
            q.current(),
            Some(&shuffled[2]),
            "the index moved with the track"
        );
        assert_ne!(q.revision(), revision);
        assert!(!q.remove_recommended(), "nothing left to remove");

        q.set_shuffle(false);
        assert_eq!(items(&q), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_playing_recommendation_is_kept() {
        let mut q = queue(3);
        q.insert_recommended(1, 50);
        q.insert_recommended(3, 51);
        q.advance(false);
        assert_eq!(q.current(), Some(&50));
        assert!(q.remove_recommended());
        assert_eq!(items(&q), [0, 50, 1, 2]);
        assert_eq!(q.current(), Some(&50));
        assert!(q.is_recommended(1), "still marked as one");
    }

    #[test]
    fn marks_restored_recommendations() {
        let mut q = queue(4);
        q.mark_recommended([1, 3, 9]);
        assert_eq!(q.recommended().collect::<Vec<_>>(), [1, 3]);
        assert_eq!(
            q.entries()
                .filter(|(_, r)| *r)
                .map(|(i, _)| *i)
                .collect::<Vec<_>>(),
            [1, 3]
        );
    }

    #[test]
    fn play_next_and_remove() {
        let mut q = queue(3);
        q.play_next(7);
        assert_eq!(items(&q), [0, 7, 1, 2]);
        q.jump(2);
        assert!(q.remove(0));
        assert_eq!(q.current(), Some(&1));
        assert!(
            !q.remove(q.current_index().unwrap()),
            "cannot remove the current track"
        );
    }

    #[test]
    fn empty_queue_starts_with_first_push() {
        let mut q: Queue<u32> = Queue::with_seed(1);
        assert_eq!(q.advance(false), None);
        q.push(4);
        assert_eq!(q.current(), Some(&4));
    }

    #[test]
    fn revision_tracks_changes_to_entries_only() {
        let mut q = queue(3);
        let r = q.revision();
        q.advance(false);
        q.jump(0);
        q.back();
        assert_eq!(q.revision(), r, "moving through the queue changes nothing");
        q.push(9);
        assert_ne!(q.revision(), r);
        let r = q.revision();
        q.set_shuffle(true);
        assert_ne!(q.revision(), r);
    }

    #[test]
    fn clears_upcoming() {
        let mut q = queue(5);
        q.jump(1);
        q.clear_upcoming();
        assert_eq!(items(&q), [0, 1]);
    }
}
