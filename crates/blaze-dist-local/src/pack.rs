//! A distributed pack: the store values one batch of work made, under one content digest.
//!
//! `DIST_PACK_MAGIC`, then the values sorted by key, each its 32-byte key, a byte naming its
//! kind (`blaze_cas::Kind::code`, opaque here), its length as a little-endian `u64` and its
//! bytes. A pack is named by the SHA-256 of all of that ([`Pack::digest`]), and
//! [`PackRef::keys`] is the digest of its keys, so a reader checks it has what was committed
//! before it believes one value.

use blaze_wire::{
    DIST_PACK_FORMAT, DIST_PACK_MAGIC, Digest, Limits, PackRef, digest_of_encoded_name, encode,
};
use sha2::{Digest as _, Sha256};

/// One value: a store key, its kind's code, its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    /// The store key.
    pub key: Digest,
    /// The kind's code, which only the store reads.
    pub kind: u8,
    /// The value.
    pub bytes: Vec<u8>,
}

/// A pack as it goes on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pack {
    /// The whole pack.
    pub bytes: Vec<u8>,
    /// What a result commits about it.
    pub reference: PackRef,
}

/// Canonicalize the entire set BEFORE partitioning, so identical duplicates
/// coalesce and conflicting keys cannot be concealed at a pack boundary.
fn canonical(mut values: Vec<Value>) -> Result<Vec<Value>, String> {
    values.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    values.dedup();
    for pair in values.windows(2) {
        if pair[0].key == pair[1].key {
            return Err(format!("two values under the key {}", hex(&pair[0].key)));
        }
    }
    Ok(values)
}

impl Pack {
    /// The pack of `values`, in key order whatever order they came in, so two executors that
    /// made the same values name the same pack. Two values under one key with different bytes
    /// are an error: a key covers all its value depends on.
    pub fn of(values: Vec<Value>) -> Result<Pack, String> {
        let values = canonical(values)?;
        if values.len() > Limits::DOCUMENTED.batch_items as usize {
            return Err(format!(
                "a native pack has {} keys, exceeding the {}-key batch limit",
                values.len(),
                Limits::DOCUMENTED.batch_items
            ));
        }
        let size = values.iter().try_fold(DIST_PACK_MAGIC.len(), |size: usize, value| {
            size.checked_add(41)?.checked_add(value.bytes.len())
        }).ok_or_else(|| "the pack's total byte length overflows usize".to_owned())?;
        if u64::try_from(size).map_or(true, |size| size > Limits::DOCUMENTED.blob) {
            return Err(format!("a native pack exceeds the {}-byte blob limit", Limits::DOCUMENTED.blob));
        }
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(&DIST_PACK_MAGIC);
        for value in &values {
            bytes.extend_from_slice(&value.key);
            bytes.push(value.kind);
            bytes.extend_from_slice(&(value.bytes.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&value.bytes);
        }
        let reference = PackRef {
            pack: Sha256::digest(&bytes).into(),
            size: bytes.len() as u64,
            keys: keys_digest(values.iter().map(|it| it.key)),
            packed: None,
            format: DIST_PACK_FORMAT,
        };
        Ok(Pack { bytes, reference })
    }

    /// Publish all of a grant or tail without exceeding a native pack's key or
    /// byte limits. This partitions the same work; it never drops or recompiles a
    /// value. The manifest already supports multiple sorted pack references.
    pub fn batches(values: Vec<Value>) -> Result<Vec<Pack>, String> {
        Self::batches_with_limits(values, Limits::DOCUMENTED.batch_items as usize, 64 << 20)
    }

    // A small limit makes the real partitioning path testable without allocating
    // 65,537 keys or several GiB. A single large value remains in its own pack,
    // provided it fits the documented blob limit.
    fn batches_with_limits(
        values: Vec<Value>,
        max_items: usize,
        target_bytes: usize,
    ) -> Result<Vec<Pack>, String> {
        if max_items == 0 || target_bytes < DIST_PACK_MAGIC.len() {
            return Err("native pack partition limits must hold at least a header and one key".to_owned());
        }
        let values = canonical(values)?;
        if values.is_empty() {
            return Ok(vec![Pack::of(Vec::new())?]);
        }
        let blob_limit = usize::try_from(Limits::DOCUMENTED.blob).unwrap_or(usize::MAX);
        let mut packs = Vec::new(); // alloc: one referenceable immutable pack per bounded partition
        let mut group = Vec::new(); // alloc: a bounded subset of the already owned values
        let mut size = DIST_PACK_MAGIC.len();
        for value in values {
            let entry = value.bytes.len().checked_add(41)
                .ok_or_else(|| format!("native value {} has an overflowing size", hex(&value.key)))?;
            if entry > blob_limit.saturating_sub(DIST_PACK_MAGIC.len()) {
                return Err(format!("native value {} exceeds the {}-byte pack limit", hex(&value.key), Limits::DOCUMENTED.blob));
            }
            if !group.is_empty()
                && (group.len() >= max_items
                    || size.saturating_add(entry) > target_bytes
                    || size.saturating_add(entry) > blob_limit)
            {
                packs.push(Pack::of(std::mem::take(&mut group))?);
                size = DIST_PACK_MAGIC.len();
            }
            size = size.checked_add(entry)
                .ok_or_else(|| "native partition byte length overflows usize".to_owned())?;
            group.push(value);
        }
        packs.push(Pack::of(group)?);
        Ok(packs)
    }
}

fn keys_digest(keys: impl Iterator<Item = Digest>) -> Digest {
    let keys: Vec<Digest> = keys.collect();
    digest_of_encoded_name("blaze/dist-pack-keys", &encode(&keys))
}

/// The values of `bytes`, which must be the pack `reference` names: its digest, size, format
/// and keys are all checked before a value is returned, and the values must be in key order.
pub fn read(bytes: &[u8], reference: &PackRef) -> Result<Vec<Value>, String> {
    let name = hex(&reference.pack);
    if reference.format != DIST_PACK_FORMAT {
        return Err(format!("pack {name} is of format {}", reference.format));
    }
    if reference.size > Limits::DOCUMENTED.blob {
        return Err(format!("pack {name} exceeds the {}-byte blob limit", Limits::DOCUMENTED.blob));
    }
    if bytes.len() as u64 != reference.size {
        return Err(format!(
            "pack {name} is {} bytes, committed as {}",
            bytes.len(),
            reference.size
        ));
    }
    let got: Digest = Sha256::digest(bytes).into();
    if got != reference.pack {
        return Err(format!("pack {name} hashes to {}", hex(&got)));
    }
    let mut rest =
        bytes.strip_prefix(&DIST_PACK_MAGIC).ok_or_else(|| format!("pack {name} has no header"))?;
    let mut values = Vec::new();
    while !rest.is_empty() {
        let short = || format!("pack {name} ends inside a value");
        if values.len() >= Limits::DOCUMENTED.batch_items as usize {
            return Err(format!("pack {name} exceeds the {}-key batch limit", Limits::DOCUMENTED.batch_items));
        }
        let (key, after) = rest.split_first_chunk::<32>().ok_or_else(short)?;
        let (kind, after) = after.split_first().ok_or_else(short)?;
        let (len, after) = after.split_first_chunk::<8>().ok_or_else(short)?;
        let len = usize::try_from(u64::from_le_bytes(*len)).map_err(|_| short())?;
        if after.len() < len {
            return Err(short());
        }
        let (value, after) = after.split_at(len);
        if values.last().is_some_and(|last: &Value| last.key >= *key) {
            return Err(format!("pack {name} is not in key order"));
        }
        values.push(Value { key: *key, kind: *kind, bytes: value.to_vec() });
        rest = after;
    }
    if keys_digest(values.iter().map(|it| it.key)) != reference.keys {
        return Err(format!("pack {name} holds other keys than were committed"));
    }
    Ok(values)
}

/// Lower-case hexadecimal, as blob names spell a digest.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(key: u8, bytes: &[u8]) -> Value {
        Value { key: [key; 32], kind: 1, bytes: bytes.to_vec() }
    }

    #[test]
    fn a_pack_reads_back_whatever_order_it_was_made_in() {
        let one = Pack::of(vec![value(2, b"two"), value(1, b"one")]).unwrap();
        let other = Pack::of(vec![value(1, b"one"), value(2, b"two")]).unwrap();
        assert_eq!(one, other, "two executors with the same values name one pack");
        assert_eq!(
            read(&one.bytes, &one.reference).unwrap(),
            vec![value(1, b"one"), value(2, b"two")]
        );
    }

    #[test]
    fn bounded_packs_keep_every_key_and_refuse_conflicts_across_boundaries() {
        let source = vec![value(3, b"three"), value(1, b"one"), value(2, b"two"),
            value(2, b"two")];
        let by_items = Pack::batches_with_limits(source.clone(), 2, usize::MAX).unwrap();
        assert_eq!(by_items.len(), 2, "three distinct keys must span two item-bounded packs");
        let recovered = by_items.iter().flat_map(|pack| read(&pack.bytes, &pack.reference).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(recovered, vec![value(1, b"one"), value(2, b"two"), value(3, b"three")]);
        let by_bytes = Pack::batches_with_limits(source.clone(), 5, DIST_PACK_MAGIC.len() + 45).unwrap();
        assert_eq!(by_bytes.len(), 3, "byte-bounded groups must retain every value");
        let recovered = by_bytes.iter().flat_map(|pack| read(&pack.bytes, &pack.reference).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(recovered, vec![value(1, b"one"), value(2, b"two"), value(3, b"three")]);
        let reordered = Pack::batches_with_limits(source.into_iter().rev().collect(), 2, usize::MAX).unwrap();
        assert_eq!(reordered, by_items, "partition names may not depend on producer order");
        assert_eq!(Pack::batches(Vec::new()).unwrap(), vec![Pack::of(Vec::new()).unwrap()]);
        assert!(Pack::batches_with_limits(vec![value(1, b"a"), value(1, b"b")], 1, usize::MAX)
            .unwrap_err().contains("two values"), "conflicting keys on different partitions refuse");
        assert!(Pack::batches_with_limits(vec![value(1, b"a")], 0, usize::MAX).is_err());
    }

    #[test]
    fn two_values_under_one_key_are_refused() {
        let error = Pack::of(vec![value(1, b"a"), value(1, b"b")]).unwrap_err();
        assert!(error.contains("two values"), "{error}");
        assert!(Pack::of(vec![value(1, b"a"), value(1, b"a")]).is_ok(), "one value, twice");
    }

    #[test]
    fn a_reader_believes_nothing_the_commit_does_not_name() {
        let pack = Pack::of(vec![value(1, b"one")]).unwrap();
        let mut flipped = pack.bytes.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(read(&flipped, &pack.reference).unwrap_err().contains("hashes"));
        let mut short = pack.reference.clone();
        short.size -= 1;
        assert!(read(&pack.bytes, &short).unwrap_err().contains("committed"));
        let mut keys = pack.reference.clone();
        keys.keys = [9; 32];
        assert!(read(&pack.bytes, &keys).unwrap_err().contains("other keys"));
        // The right digest over bytes that are not a pack: refused by their shape.
        let junk = b"not a pack".to_vec();
        let reference = PackRef {
            pack: Sha256::digest(&junk).into(),
            size: junk.len() as u64,
            ..pack.reference.clone()
        };
        assert!(read(&junk, &reference).unwrap_err().contains("no header"));
    }
}
