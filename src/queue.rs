//! The play queue. Shuffle reorders what comes after the current track, as
//! YouTube Music does, and turning it off restores the original order.

use std::time::{SystemTime, UNIX_EPOCH};

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
        }
    }

    fn entry(&mut self, item: T) -> Entry<T> {
        self.next_uid += 1;
        Entry {
            uid: self.next_uid,
            item,
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
                self.unshuffled = Some(self.entries.iter().map(|e| e.uid).collect());
                self.shuffle_after_current();
            }
            (false, true) => {
                let order = self.unshuffled.take().unwrap_or_default();
                let current_uid = self.current.map(|c| self.entries[c].uid);
                // Entries added while shuffled keep their place at the end.
                let mut restored: Vec<Entry<T>> = Vec::with_capacity(self.entries.len());
                for uid in order {
                    if let Some(pos) = self.entries.iter().position(|e| e.uid == uid) {
                        restored.push(self.entries.remove(pos));
                    }
                }
                restored.append(&mut self.entries);
                self.entries = restored;
                self.current =
                    current_uid.and_then(|uid| self.entries.iter().position(|e| e.uid == uid));
            }
            _ => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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

    pub fn get(&self, index: usize) -> Option<&T> {
        self.entries.get(index).map(|e| &e.item)
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
        self.entries.remove(index);
        if index < current {
            self.current = Some(current - 1);
        }
        true
    }

    /// Drops everything after the current track.
    pub fn clear_upcoming(&mut self) {
        if let Some(current) = self.current {
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
    fn unshuffle_keeps_tracks_added_while_shuffled() {
        let mut q = queue(5);
        q.set_shuffle(true);
        q.push(99);
        q.set_shuffle(false);
        assert_eq!(items(&q), [0, 1, 2, 3, 4, 99]);
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
    fn clears_upcoming() {
        let mut q = queue(5);
        q.jump(1);
        q.clear_upcoming();
        assert_eq!(items(&q), [0, 1]);
    }
}
