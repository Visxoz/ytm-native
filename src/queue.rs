//! Shuffle reorders only what follows the current track, as YouTube Music
//! does, and turning it off restores the original order.

use std::{
    collections::{HashMap, HashSet},
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

/// What shuffle spreads apart, so one artist's or album's songs don't bunch.
pub trait Spread {
    fn artist(&self) -> &str;
    fn album(&self) -> &str;
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

impl<T: Clone + Spread> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + Spread> Queue<T> {
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

    pub fn random_below(&mut self, n: usize) -> usize {
        (self.random() % n.max(1) as u64) as usize
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.random() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn shuffle_after_current(&mut self) {
        let start = self.current.map_or(0, |c| c + 1);
        let rest = self.entries.split_off(start);
        let items: Vec<&T> = rest.iter().map(|e| &e.item).collect();
        let order = self.spread(&items, &[T::artist, T::album]);
        let mut rest: Vec<_> = rest.into_iter().map(Some).collect();
        self.entries
            .extend(order.into_iter().filter_map(|i| rest[i].take()));
        // The spread doesn't see the current entry, so its artist may follow it.
        if let Some(current) = self.current {
            let artist = self.entries[current].item.artist();
            if let Some(other) = self.entries[start..]
                .iter()
                .position(|e| e.item.artist() != artist)
            {
                self.entries[start..=start + other].rotate_right(1);
            }
        }
    }

    /// A random order of `items` in which each key's items sit evenly apart
    /// from a random offset, with a little jitter, as a plain shuffle bunches
    /// them by chance; within a key, the next key is spread the same way.
    fn spread(&mut self, items: &[&T], keys: &[fn(&T) -> &str]) -> Vec<usize> {
        let Some((key, keys)) = keys.split_first() else {
            let mut order: Vec<usize> = (0..items.len()).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, self.random_below(i + 1));
            }
            return order;
        };
        // Groups in order of first appearance, as a map's order would make the
        // draws below differ from run to run.
        let mut index = HashMap::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let g = *index.entry(key(item)).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[g].push(i);
        }
        let mut placed = Vec::with_capacity(items.len());
        for group in groups {
            let members: Vec<&T> = group.iter().map(|&i| items[i]).collect();
            let order = self.spread(&members, keys);
            let gap = 1.0 / group.len() as f64;
            let offset = self.unit();
            for (n, m) in order.into_iter().enumerate() {
                let jitter = 0.4 * self.unit() - 0.2;
                placed.push(((n as f64 + offset + jitter) * gap, group[m]));
            }
        }
        placed.sort_by(|a, b| a.0.total_cmp(&b.0));
        placed.into_iter().map(|(_, i)| i).collect()
    }

    pub fn set(&mut self, items: Vec<T>, start: usize, shuffle: bool) {
        self.revision += 1;
        self.entries = items.into_iter().map(|item| self.entry(item)).collect();
        self.current = (!self.entries.is_empty()).then(|| start.min(self.entries.len() - 1));
        self.unshuffled = shuffle.then(|| self.entries.iter().map(|e| e.uid).collect());
        if shuffle {
            // Nothing has played yet, so those before the start are shuffled in too.
            if let Some(start) = self.current {
                self.entries[..=start].rotate_right(1);
                self.current = Some(0);
            }
            self.shuffle_after_current();
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
                // Entries pushed, played next or recommended while shuffled,
                // which the order doesn't know, stay behind the entry they
                // followed, so the one playing never jumps to the end.
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

    pub fn recommended(&self) -> impl Iterator<Item = usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.recommended.then_some(i))
    }

    pub fn mark_recommended(&mut self, indices: impl IntoIterator<Item = usize>) {
        for index in indices {
            if let Some(entry) = self.entries.get_mut(index) {
                entry.recommended = true;
            }
        }
    }

    /// Does nothing unless `index` is after the current track.
    pub fn insert_recommended(&mut self, index: usize, item: T) {
        let Some(current) = self.current else { return };
        if index <= current || index > self.entries.len() {
            return;
        }
        let mut entry = self.entry(item);
        entry.recommended = true;
        self.entries.insert(index, entry);
    }

    /// Keeps the one playing and the user's own order. Returns whether any was removed.
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

    /// `user` skips ignore repeat-one. Returns `None` at the end of the queue.
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

    /// While shuffled, each item goes in at random among the entries still to
    /// play, and goes last should shuffle be turned off.
    pub fn extend(&mut self, items: impl IntoIterator<Item = T>) {
        // Behind the entries played next or recommended right after the
        // current, which the order doesn't know, so they still play next.
        let start = self.unshuffled.as_ref().map(|order| {
            let known: HashSet<u64> = order.iter().copied().collect();
            let after = self.current.map_or(0, |c| c + 1);
            after
                + self.entries[after..]
                    .iter()
                    .take_while(|e| !known.contains(&e.uid))
                    .count()
        });
        for item in items {
            let entry = self.entry(item);
            let at = match (start, &mut self.unshuffled) {
                (Some(start), Some(order)) => {
                    order.push(entry.uid);
                    // A few draws for a place not beside the same artist, as
                    // spreading again would reorder what is already shown.
                    let mut at = start;
                    for _ in 0..4 {
                        at = start + self.random_below(self.entries.len() - start + 1);
                        if !self.beside(at, entry.item.artist()) {
                            break;
                        }
                    }
                    at
                }
                _ => self.entries.len(),
            };
            self.entries.insert(at, entry);
            self.current.get_or_insert(at);
        }
    }

    /// Whether an item put in at `at` would sit beside one by `artist`.
    fn beside(&self, at: usize, artist: &str) -> bool {
        let same = |i: usize| {
            self.entries
                .get(i)
                .is_some_and(|e| e.item.artist() == artist)
        };
        (at > 0 && same(at - 1)) || same(at)
    }

    /// Never removes the current entry. Returns the indices removed, in order.
    pub fn remove_many(&mut self, indices: impl IntoIterator<Item = usize>) -> Vec<usize> {
        let Some(current) = self.current else {
            return Vec::new();
        };
        let mut drop = vec![false; self.entries.len()];
        for index in indices {
            if index != current && index < drop.len() {
                drop[index] = true;
            }
        }
        let removed: Vec<usize> = (0..drop.len()).filter(|&i| drop[i]).collect();
        if removed.is_empty() {
            return removed;
        }
        self.revision += 1;
        let mut index = 0;
        self.entries.retain(|_| {
            let keep = !drop[index];
            index += 1;
            keep
        });
        let before = removed.iter().take_while(|&&i| i < current).count();
        self.current = Some(current - before);
        removed
    }

    /// The current index follows the current track, which may itself move.
    pub fn move_to(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.entries.len() || to >= self.entries.len() {
            return false;
        }
        self.revision += 1;
        if from < to {
            self.entries[from..=to].rotate_left(1);
        } else {
            self.entries[to..=from].rotate_right(1);
        }
        if let Some(current) = self.current {
            self.current = Some(if current == from {
                to
            } else if from < current && current <= to {
                current - 1
            } else if to <= current && current < from {
                current + 1
            } else {
                current
            });
        }
        true
    }

    /// Keeps the entries' queue order. Returns whether anything moved.
    pub fn play_next_many(&mut self, indices: impl IntoIterator<Item = usize>) -> bool {
        let Some(current) = self.current else {
            return false;
        };
        let mut pick = vec![false; self.entries.len()];
        for index in indices {
            if index != current && index < pick.len() {
                pick[index] = true;
            }
        }
        let picked = pick.iter().filter(|&&p| p).count();
        let already_next = pick[..current].iter().all(|&p| !p)
            && pick[current + 1..].iter().take(picked).all(|&p| p);
        if picked == 0 || already_next {
            return false;
        }
        self.revision += 1;
        let mut kept = Vec::with_capacity(self.entries.len());
        let mut moved = Vec::with_capacity(picked);
        let mut after = Vec::with_capacity(self.entries.len() - current - 1);
        for (index, entry) in std::mem::take(&mut self.entries).into_iter().enumerate() {
            if pick[index] {
                moved.push(entry);
            } else if index <= current {
                kept.push(entry);
            } else {
                after.push(entry);
            }
        }
        self.current = Some(kept.len() - 1);
        kept.append(&mut moved);
        kept.append(&mut after);
        self.entries = kept;
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

    const NAMES: [&str; 10] = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];

    impl Spread for u32 {
        fn artist(&self) -> &str {
            NAMES[*self as usize % 3]
        }

        fn album(&self) -> &str {
            ""
        }
    }

    /// An artist and an album, by index into `NAMES`.
    impl Spread for (usize, usize) {
        fn artist(&self) -> &str {
            NAMES[self.0]
        }

        fn album(&self) -> &str {
            NAMES[self.1]
        }
    }

    fn neighbours(
        order: &[(usize, usize)],
        same: impl Fn(&(usize, usize), &(usize, usize)) -> bool,
    ) -> usize {
        order.windows(2).filter(|w| same(&w[0], &w[1])).count()
    }

    /// Ten artists of uneven sizes, none much more than a fifth, each across three albums.
    fn library() -> Vec<(usize, usize)> {
        [60, 47, 41, 33, 29, 23, 19, 13, 11, 7]
            .into_iter()
            .enumerate()
            .flat_map(|(artist, n)| (0..n).map(move |i| (artist, i % 3)))
            .collect()
    }

    #[test]
    fn shuffle_spreads_artists_and_albums() {
        let songs = library();
        let n = songs.len() as f64;
        let mut by_artist = [0usize; 10];
        songs.iter().for_each(|s| by_artist[s.0] += 1);
        // What a plain shuffle averages: (n - 1) times the chance two songs share an artist.
        let plain: f64 = (n - 1.0)
            * by_artist
                .iter()
                .map(|&k| (k as f64 / n).powi(2))
                .sum::<f64>();
        let (mut artists, mut albums) = (0, 0);
        for seed in 0..20 {
            let mut q = Queue::with_seed(seed);
            q.set(songs.clone(), 0, true);
            let order: Vec<_> = q.items().copied().collect();
            artists += neighbours(&order[1..], |a, b| a.0 == b.0);
            for artist in 0..10 {
                let own: Vec<_> = order.iter().copied().filter(|s| s.0 == artist).collect();
                albums += neighbours(&own, |a, b| a.1 == b.1);
            }
        }
        assert!(
            artists as f64 * 10.0 < plain * 20.0,
            "{artists} same-artist neighbours, where a plain shuffle averages {}",
            plain * 20.0
        );
        // A plain shuffle puts two of an artist's songs from one album together a third of the time.
        let plain_albums = (songs.len() - 10) as f64 / 3.0 * 20.0;
        assert!(
            albums as f64 * 5.0 < plain_albums,
            "{albums} same-album neighbours, where a plain shuffle averages {plain_albums}"
        );
    }

    #[test]
    fn spreading_keeps_the_current_and_restores_the_order() {
        let songs = library();
        let mut q = Queue::with_seed(3);
        q.set(songs.clone(), 0, false);
        q.jump(40);
        q.set_shuffle(true);
        let order: Vec<_> = q.items().copied().collect();
        assert_eq!(&order[..41], &songs[..41], "what has played stays put");
        let mut sorted = order.clone();
        sorted.sort();
        let mut all = songs.clone();
        all.sort();
        assert_eq!(sorted, all, "nothing lost or doubled");
        q.advance(false);
        let playing = *q.current().unwrap();
        q.set_shuffle(false);
        assert_eq!(q.items().copied().collect::<Vec<_>>(), songs);
        assert_eq!(q.current(), Some(&playing));
    }

    #[test]
    fn a_playlist_arriving_in_pages_is_spread_too() {
        let songs = library();
        let mut same = 0;
        for seed in 0..20 {
            let mut q = Queue::with_seed(seed);
            q.set(songs[..100].to_vec(), 0, true);
            for page in songs[100..].chunks(100) {
                q.extend(page.iter().copied());
            }
            let order: Vec<_> = q.items().copied().collect();
            same += neighbours(&order, |a, b| a.0 == b.0);
        }
        // A plain shuffle of these averages about 37 per queue.
        assert!(same < 20 * 12, "{same} same-artist neighbours in 20 queues");
    }

    #[test]
    fn shuffle_is_the_same_for_the_same_seed() {
        let shuffled = || {
            let mut q = Queue::with_seed(7);
            q.set(library(), 3, true);
            q.extend((0..50).map(|i| (i % 10, 0)));
            q.items().copied().collect::<Vec<_>>()
        };
        assert_eq!(shuffled(), shuffled());
    }

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
    fn a_shuffled_start_part_way_through_plays_every_track() {
        let mut q = Queue::with_seed(42);
        q.set((0..20).collect(), 10, true);
        let mut heard = vec![*q.current().unwrap()];
        while let Some(&t) = q.advance(false) {
            heard.push(t);
        }
        assert_eq!(heard[0], 10);
        heard.sort();
        assert_eq!(heard, (0..20).collect::<Vec<_>>());
        q.set_shuffle(false);
        assert_eq!(items(&q), (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn tracks_added_while_shuffled_mix_in_and_go_last_unshuffled() {
        let mut q = queue(10);
        q.set_shuffle(true);
        q.advance(false);
        q.advance(false);
        let played = items(&q)[..3].to_vec();
        q.extend(10..20);
        let order = items(&q);
        assert_eq!(&order[..3], played, "what has played stays put");
        assert!(
            order[3..10].iter().any(|&t| t >= 10),
            "mixed in among those still to play"
        );
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
        q.set_shuffle(false);
        assert_eq!(items(&q), (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn tracks_added_while_shuffled_go_behind_one_played_next() {
        let mut q = queue(10);
        q.set_shuffle(true);
        q.play_next(99);
        q.extend(10..400);
        assert_eq!(q.peek_next(), Some(&99));
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
    fn moves_entries_and_the_current_index_follows() {
        let mut q = queue(6);
        q.jump(2);
        let revision = q.revision();
        assert!(q.move_to(4, 1), "a later entry to before the current");
        assert_eq!(items(&q), [0, 4, 1, 2, 3, 5]);
        assert_eq!(q.current(), Some(&2));
        assert_eq!(q.current_index(), Some(3));
        assert_ne!(q.revision(), revision);

        assert!(q.move_to(0, 5), "an earlier entry to after the current");
        assert_eq!(items(&q), [4, 1, 2, 3, 5, 0]);
        assert_eq!(q.current_index(), Some(2));

        assert!(q.move_to(2, 0), "the current track itself");
        assert_eq!(items(&q), [2, 4, 1, 3, 5, 0]);
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.current(), Some(&2));

        let revision = q.revision();
        assert!(!q.move_to(1, 1));
        assert!(!q.move_to(1, 9));
        assert_eq!(q.revision(), revision, "nothing to move changes nothing");
    }

    #[test]
    fn moving_while_shuffled_restores_the_original_order() {
        let mut q = queue(8);
        q.jump(1);
        q.set_shuffle(true);
        q.move_to(7, 2);
        q.move_to(3, 6);
        q.advance(false);
        let playing = *q.current().unwrap();
        q.set_shuffle(false);
        assert_eq!(items(&q), (0..8).collect::<Vec<_>>());
        assert_eq!(q.current(), Some(&playing));
    }

    #[test]
    fn removes_many_but_never_the_current() {
        let mut q = queue(7);
        q.jump(3);
        assert_eq!(q.remove_many([5, 0, 3, 1, 99, 5]), [0, 1, 5]);
        assert_eq!(items(&q), [2, 3, 4, 6]);
        assert_eq!(q.current(), Some(&3));
        assert_eq!(q.current_index(), Some(1));
        let revision = q.revision();
        assert!(q.remove_many([1]).is_empty(), "only the current: nothing");
        assert_eq!(q.revision(), revision);
    }

    #[test]
    fn plays_a_selection_next_in_queue_order() {
        let mut q = queue(8);
        q.jump(3);
        assert!(q.play_next_many([6, 1, 3, 7]));
        assert_eq!(items(&q), [0, 2, 3, 1, 6, 7, 4, 5]);
        assert_eq!(q.current(), Some(&3));
        assert_eq!(q.current_index(), Some(2));
        assert_eq!(q.peek_next(), Some(&1));

        let revision = q.revision();
        assert!(!q.play_next_many([3, 4, 5]), "already next, in that order");
        assert!(!q.play_next_many([2]), "the current track never moves");
        assert!(!q.play_next_many([]));
        assert_eq!(q.revision(), revision);
    }

    #[test]
    fn plays_a_selection_next_through_shuffle() {
        let mut q = queue(10);
        q.jump(4);
        q.set_shuffle(true);
        let shuffled = items(&q);
        assert!(q.play_next_many([8, 9]));
        assert_eq!(q.peek_next(), Some(&shuffled[8]));
        q.set_shuffle(false);
        assert_eq!(items(&q), (0..10).collect::<Vec<_>>());
        assert_eq!(q.current(), Some(&4));
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
        assert_eq!(q.remove_many([0]), [0]);
        assert_eq!(q.current(), Some(&1));
        assert!(
            q.remove_many([q.current_index().unwrap()]).is_empty(),
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
