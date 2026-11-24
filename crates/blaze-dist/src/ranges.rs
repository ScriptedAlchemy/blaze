//! Half-open ranges of indexes into the sorted roots, as a `Grant` and a `BatchResult` carry them.

use blaze_own::{BTreeMap, Vec};
use blaze_wire::{Set, WireAlloc};

/// A set of root indexes, held as disjoint, non-adjacent half-open ranges in ascending order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ranges {
    /// Start to end of each range.
    spans: BTreeMap<u64, u64, WireAlloc>,
}

impl Ranges {
    /// No index.
    pub fn new(alloc: WireAlloc) -> Ranges {
        Ranges { spans: BTreeMap::new_in(alloc) }
    }

    /// The indexes `0..n`.
    pub fn full(n: u64, alloc: WireAlloc) -> Ranges {
        let mut out = Ranges::new(alloc);
        out.insert(0, n);
        out
    }

    /// The ranges of a wire set, or `None` when one is empty or reversed or two overlap: a set
    /// the coordinator did not write.
    pub fn from_wire(set: &Set<(u64, u64)>, alloc: WireAlloc) -> Option<Ranges> {
        let mut out = Ranges::new(alloc);
        for &(start, end) in set.iter() {
            if start >= end || out.intersects(start, end) {
                return None;
            }
            out.insert(start, end);
        }
        Some(out)
    }

    /// The ranges as the wire carries them: a set of `(start, end)`.
    pub fn to_wire(&self) -> Set<(u64, u64)> {
        let alloc = *self.spans.allocator();
        Set::new(Vec::from_iter_in(self.iter(), alloc))
    }

    /// How many indexes the ranges hold.
    pub fn len(&self) -> u64 {
        self.spans.iter().map(|(start, end)| end - start).sum()
    }

    /// Whether there is no index.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The ranges, ascending.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.spans.iter().map(|(&start, &end)| (start, end))
    }

    /// Every index, ascending.
    pub fn indexes(&self) -> impl Iterator<Item = u64> + '_ {
        self.iter().flat_map(|(start, end)| start..end)
    }

    /// Adds `start..end`, merging what it touches.
    pub fn insert(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let alloc = *self.spans.allocator();
        let (mut start, mut end) = (start, end);
        // A range that starts before `start` and reaches it merges.
        if let Some((&s, &e)) = self.spans.range(..=start).next_back()
            && e >= start
        {
            start = s;
            end = end.max(e);
            self.spans.remove(&s);
        }
        // Ranges that start inside `start..=end` merge.
        let inside = Vec::from_iter_in(self.spans.range(start..=end).map(|(&s, &e)| (s, e)), alloc);
        for (s, e) in inside {
            end = end.max(e);
            self.spans.remove(&s);
        }
        self.spans.insert(start, end);
    }

    /// Removes `start..end`, splitting what it cuts.
    pub fn remove(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let alloc = *self.spans.allocator();
        let cut = Vec::from_iter_in(
            self.spans.range(..end).filter(|&(_, &e)| e > start).map(|(&s, &e)| (s, e)),
            alloc,
        );
        for (s, e) in cut {
            self.spans.remove(&s);
            if s < start {
                self.spans.insert(s, start);
            }
            if e > end {
                self.spans.insert(end, e);
            }
        }
    }

    /// Adds every range of `other`.
    pub fn extend(&mut self, other: &Ranges) {
        for (start, end) in other.iter() {
            self.insert(start, end);
        }
    }

    /// Whether every index of `start..end` is held.
    pub fn covers(&self, start: u64, end: u64) -> bool {
        start >= end || self.spans.range(..=start).next_back().is_some_and(|(_, &e)| e >= end)
    }

    /// Whether any index of `start..end` is held.
    pub fn intersects(&self, start: u64, end: u64) -> bool {
        start < end && self.spans.range(..end).next_back().is_some_and(|(_, &e)| e > start)
    }

    /// Whether every index of `other` is held here.
    pub fn contains_all(&self, other: &Ranges) -> bool {
        other.iter().all(|(start, end)| self.covers(start, end))
    }

    /// The indexes held here and not in `other`.
    pub fn minus(&self, other: &Ranges) -> Ranges {
        let mut out = self.clone();
        for (start, end) in other.iter() {
            out.remove(start, end);
        }
        out
    }

    /// Takes up to `n` indexes from the front.
    pub fn take_front(&mut self, n: u64) -> Ranges {
        let alloc = *self.spans.allocator();
        let mut out = Ranges::new(alloc);
        let mut left = n;
        while left > 0 {
            let Some((&start, &end)) = self.spans.iter().next() else { break };
            let take = (end - start).min(left);
            out.insert(start, start + take);
            self.remove(start, start + take);
            left -= take;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::{Vec, vec_in};

    use super::Ranges;

    #[test]
    fn insert_merges_and_remove_splits() {
        let alloc = blaze_own::testing::heap().shared();
        let mut r = Ranges::new(alloc);
        r.insert(5, 10);
        r.insert(0, 3);
        r.insert(3, 5);
        assert_eq!(Vec::from_iter_in(r.iter(), alloc).as_slice(), &[(0, 10)]);
        r.remove(2, 4);
        assert_eq!(Vec::from_iter_in(r.iter(), alloc).as_slice(), &[(0, 2), (4, 10)]);
        assert_eq!(r.len(), 8);
        assert!(r.covers(4, 10) && !r.covers(1, 5));
        assert!(r.intersects(1, 3) && !r.intersects(2, 4));
        let front = r.take_front(3);
        assert_eq!(Vec::from_iter_in(front.iter(), alloc).as_slice(), &[(0, 2), (4, 5)]);
        assert_eq!(Vec::from_iter_in(r.iter(), alloc).as_slice(), &[(5, 10)]);
    }

    #[test]
    fn wire_refuses_overlap_and_empty() {
        let alloc = blaze_own::testing::heap().shared();
        let ok = blaze_wire::Set::new(vec_in![alloc; (0, 2), (2, 4)]);
        assert_eq!(
            Vec::from_iter_in(Ranges::from_wire(&ok, alloc).unwrap().iter(), alloc).as_slice(),
            &[(0, 4)]
        );
        assert!(
            Ranges::from_wire(&blaze_wire::Set::new(vec_in![alloc; (0, 3), (2, 4)]), alloc)
                .is_none()
        );
        assert!(Ranges::from_wire(&blaze_wire::Set::new(vec_in![alloc; (3, 3)]), alloc).is_none());
    }
}
