//! File slots kept when an address-ordered input list moves entries.

use crate::model::Splice;

/// Retained entries keep their slots; new entries take removed slots of the same storage
/// class, in input order within each class. `Some(None)` keeps the mapping as it is, without
/// allocation. An incompatible assignment returns `None` before changing any state.
pub(super) fn recycle<T: Copy + Default, K: Copy + Ord>(
    old: &[T],
    len: usize,
    splices: &[Splice],
    old_class: impl Fn(T) -> K,
    new_class: impl Fn(usize) -> K,
) -> Option<Option<Vec<T>>> {
    if old.len() != len {
        return None;
    }
    let (mut was, mut now) = (0, 0);
    for splice in splices {
        let (a, b) = (&splice.old, &splice.new);
        if a.start > a.end
            || b.start > b.end
            || a.end as usize > old.len()
            || b.end as usize > len
            || (a.start as usize) < was
            || (b.start as usize) < now
            || a.start as usize - was != b.start as usize - now
        {
            return None;
        }
        (was, now) = (a.end as usize, b.end as usize);
    }
    if old.len() - was != len - now {
        return None;
    }
    if splices.iter().all(Splice::same_len)
        && splices.iter().all(|splice| {
            (splice.new.start as usize..splice.new.end as usize)
                .all(|at| old_class(old[at]) == new_class(at))
        })
    {
        return Some(None);
    }
    // alloc: only when splices move entries between input positions; one removed/new entry
    let mut removed: Vec<_> = splices
        .iter()
        .flat_map(|splice| {
            (splice.old.start as usize..splice.old.end as usize)
                .map(|at| (old_class(old[at]), at, old[at]))
        })
        .collect();
    // alloc: new input positions, matched to the removed slots before publishing
    let mut added: Vec<_> = splices
        .iter()
        .flat_map(|splice| {
            (splice.new.start as usize..splice.new.end as usize).map(|at| (new_class(at), at))
        })
        .collect();
    removed.sort_unstable_by_key(|it| (it.0, it.1));
    added.sort_unstable();
    if removed.len() != added.len() || removed.iter().zip(&added).any(|(a, b)| a.0 != b.0) {
        return None;
    }
    // alloc: the new input-position mapping, published together with the encoded bytes
    let mut out = vec![T::default(); len];
    (was, now) = (0, 0);
    for splice in splices {
        out[now..splice.new.start as usize].copy_from_slice(&old[was..splice.old.start as usize]);
        (was, now) = (splice.old.end as usize, splice.new.end as usize);
    }
    out[now..].copy_from_slice(&old[was..]);
    for (old, new) in removed.iter().zip(added) {
        out[new.1] = old.2;
    }
    Some(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    fn moved(
        slots: &[usize],
        from: usize,
        to: usize,
        count: usize,
        plant: bool,
    ) -> Result<Vec<usize>, String> {
        let mut expected = slots.to_vec();
        let entries: Vec<_> = expected.drain(from..from + count).collect();
        expected.splice(to..to, entries);
        let count = count as u32;
        let splices = if from < to {
            vec![
                Splice { old: from as u32..from as u32 + count, new: from as u32..from as u32 },
                Splice {
                    old: to as u32 + count..to as u32 + count,
                    new: to as u32..to as u32 + count,
                },
            ]
        } else if from > to {
            vec![
                Splice { old: to as u32..to as u32, new: to as u32..to as u32 + count },
                Splice {
                    old: from as u32..from as u32 + count,
                    new: from as u32 + count..from as u32 + count,
                },
            ]
        } else {
            Vec::new()
        };
        let actual = recycle(slots, slots.len(), &splices, |slot| slot % 3, |at| expected[at] % 3)
            .ok_or("compatible slots refused")?;
        let actual = if plant { slots.to_vec() } else { actual.unwrap_or_else(|| slots.to_vec()) };
        if actual != expected {
            return Err("a moved entry changed another entry's slot".into());
        }
        Ok(actual)
    }

    /// The refinement: after any sequence of moves, each retained or moved entry still owns
    /// its original encoded slot, and each slot is owned exactly once.
    #[hegel::test]
    fn slots_follow_entries_when_splices_move_them(tc: hegel::TestCase) {
        let len = tc.draw(gs::integers::<u8>().min_value(2).max_value(64)) as usize;
        let moves = tc.draw(
            gs::vecs(gs::tuples!(gs::integers::<u8>(), gs::integers::<u8>(), gs::integers::<u8>()))
                .max_size(12),
        );
        let mut slots: Vec<_> = (0..len).collect();
        for (from, to, count) in moves {
            let count = 1 + count as usize % len.min(8);
            let positions = len - count + 1;
            slots = moved(&slots, from as usize % positions, to as usize % positions, count, false)
                .unwrap();
        }
    }

    #[test]
    fn the_oracle_catches_slots_left_in_input_order() {
        let slots = [0, 1, 2, 3];
        assert_eq!(moved(&slots, 0, 3, 1, false), Ok(vec![1, 2, 3, 0]));
        assert!(moved(&slots, 0, 3, 1, true).unwrap_err().contains("another entry's slot"));
    }

    #[test]
    fn incompatible_storage_or_splices_publish_no_mapping() {
        let old = [0usize, 1, 2];
        let all = [Splice { old: 0..3, new: 0..3 }];
        assert!(recycle(&old, 3, &all, |slot| slot, |_| 0).is_none());
        let gap = [Splice { old: 1..2, new: 0..1 }];
        assert!(recycle(&old, 3, &gap, |_| (), |_| ()).is_none());
        let past = [Splice { old: 0..4, new: 0..4 }];
        assert!(recycle(&old, 3, &past, |_| (), |_| ()).is_none());
        let empty_name = [true];
        let all = [Splice { old: 0..1, new: 0..1 }];
        assert!(recycle(&empty_name, 1, &all, |named| (named, 0), |_| (false, 0)).is_none());
    }
}
