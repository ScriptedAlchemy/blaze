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

/// The native pack requirement that a producer or reader refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackErrorKind {
    /// One immutable key has distinct values.
    ConflictingKey(Digest),
    /// The complete encoded pack cannot be represented in the host's address space.
    SizeOverflow,
    /// A complete pack exceeds the documented blob limit.
    BlobLimit,
    /// A producer's partition target cannot hold a header and one item.
    PartitionLimits,
    /// A value's encoded size overflows the host's address space.
    ValueSizeOverflow(Digest),
    /// One value cannot fit in a documented native pack.
    ValueLimit(Digest),
    /// The reference names an unsupported native format.
    Format(u32),
    /// The actual and committed byte lengths differ.
    Length {
        /// The bytes received.
        actual: u64,
        /// The length committed by the manifest.
        committed: u64,
    },
    /// The pack's bytes do not hash to the committed digest.
    Digest(Digest),
    /// The native header is absent.
    Header,
    /// A record header or payload is truncated.
    TruncatedValue,
    /// Records are not in strictly increasing key order.
    KeyOrder,
    /// The set of keys differs from the commitment.
    Keys,
}

/// A native pack refusal names the committed pack when one exists and a matchable cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackError {
    /// The committed pack being read; producers have no commitment yet.
    pub pack: Option<Digest>,
    /// The condition that refused the pack.
    pub kind: PackErrorKind,
}

impl PackError {
    fn producing(kind: PackErrorKind) -> Self {
        Self { pack: None, kind }
    }
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use PackErrorKind as Kind;
        if let Some(pack) = &self.pack {
            write!(f, "pack {} ", hex(pack))?;
        }
        match &self.kind {
            Kind::ConflictingKey(key) => write!(f, "two values under the key {}", hex(key)),
            Kind::SizeOverflow => f.write_str("the pack's total byte length overflows usize"),
            Kind::BlobLimit => write!(f, "exceeds the {}-byte blob limit", Limits::DOCUMENTED.blob),
            Kind::PartitionLimits => {
                f.write_str("native pack partition limits must hold at least a header and one key")
            }
            Kind::ValueSizeOverflow(key) => {
                write!(f, "native value {} has an overflowing size", hex(key))
            }
            Kind::ValueLimit(key) => write!(
                f,
                "native value {} exceeds the {}-byte pack limit",
                hex(key), Limits::DOCUMENTED.blob,
            ),
            Kind::Format(format) => write!(f, "is of format {format}"),
            Kind::Length { actual, committed } => {
                write!(f, "is {actual} bytes, committed as {committed}")
            }
            Kind::Digest(got) => write!(f, "hashes to {}", hex(got)),
            Kind::Header => f.write_str("has no header"),
            Kind::TruncatedValue => f.write_str("ends inside a value"),
            Kind::KeyOrder => f.write_str("is not in key order"),
            Kind::Keys => f.write_str("holds other keys than were committed"),
        }
    }
}

impl std::error::Error for PackError {}

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
fn canonical(mut values: Vec<Value>) -> Result<Vec<Value>, PackError> {
    values.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    values.dedup();
    for pair in values.windows(2) {
        if pair[0].key == pair[1].key {
            return Err(PackError::producing(PackErrorKind::ConflictingKey(pair[0].key)));
        }
    }
    Ok(values)
}

impl Pack {
    /// The pack of `values`, in key order whatever order they came in, so two executors that
    /// made the same values name the same pack. Two values under one key with different bytes
    /// are an error: a key covers all its value depends on. Native records are not a
    /// CBOR array: the blob byte limit, not the wire batch item limit, bounds this format.
    /// Use [`Self::batches`] to partition new uploads without refusing older whole packs.
    pub fn of(values: Vec<Value>) -> Result<Pack, PackError> {
        let values = canonical(values)?;
        let size = values.iter().try_fold(DIST_PACK_MAGIC.len(), |size: usize, value| {
            size.checked_add(41)?.checked_add(value.bytes.len())
        }).ok_or_else(|| PackError::producing(PackErrorKind::SizeOverflow))?;
        if u64::try_from(size).map_or(true, |size| size > Limits::DOCUMENTED.blob) {
            return Err(PackError::producing(PackErrorKind::BlobLimit));
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

    /// Publish all of a grant or tail within the producer's item and byte targets.
    /// These partitioning targets do not change the persisted native format's limits.
    /// This partitions the same work; it never drops or recompiles a
    /// value. The manifest already supports multiple sorted pack references.
    pub fn batches(values: Vec<Value>) -> Result<Vec<Pack>, PackError> {
        Self::batches_with_limits(values, Limits::DOCUMENTED.batch_items as usize, 64 << 20)
    }

    // A small limit makes the real partitioning path testable without allocating
    // 65,537 keys or several GiB. A single large value remains in its own pack,
    // provided it fits the documented blob limit.
    fn batches_with_limits(
        values: Vec<Value>,
        max_items: usize,
        target_bytes: usize,
    ) -> Result<Vec<Pack>, PackError> {
        if max_items == 0 || target_bytes < DIST_PACK_MAGIC.len() {
            return Err(PackError::producing(PackErrorKind::PartitionLimits));
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
                .ok_or_else(|| PackError::producing(PackErrorKind::ValueSizeOverflow(value.key)))?;
            if entry > blob_limit.saturating_sub(DIST_PACK_MAGIC.len()) {
                return Err(PackError::producing(PackErrorKind::ValueLimit(value.key)));
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
                .ok_or_else(|| PackError::producing(PackErrorKind::SizeOverflow))?;
            group.push(value);
        }
        packs.push(Pack::of(group)?);
        Ok(packs)
    }
}

fn keys_digest(keys: impl Iterator<Item = Digest>) -> Digest {
    let keys: Vec<Digest> = keys.collect();
    digest_of_encoded_name("blaze/dist-pack-keys", &encode(&keys, std::alloc::Global))
}

/// The values of `bytes`, which must be the pack `reference` names: its digest, size, format
/// and keys are all checked before a value is returned, and the values must be in key order.
/// A native pack is an opaque byte-bounded blob, not a CBOR batch: older producers may
/// have committed more records than the current upload partitioning target permits.
pub fn read(bytes: &[u8], reference: &PackRef) -> Result<Vec<Value>, PackError> {
    let refused = |kind| PackError { pack: Some(reference.pack), kind };
    if reference.format != DIST_PACK_FORMAT {
        return Err(refused(PackErrorKind::Format(reference.format)));
    }
    if reference.size > Limits::DOCUMENTED.blob {
        return Err(refused(PackErrorKind::BlobLimit));
    }
    if bytes.len() as u64 != reference.size {
        return Err(refused(PackErrorKind::Length {
            actual: bytes.len() as u64,
            committed: reference.size,
        }));
    }
    let got: Digest = Sha256::digest(bytes).into();
    if got != reference.pack {
        return Err(refused(PackErrorKind::Digest(got)));
    }
    let mut rest =
        bytes.strip_prefix(&DIST_PACK_MAGIC).ok_or_else(|| refused(PackErrorKind::Header))?;
    let mut values = Vec::new();
    while !rest.is_empty() {
        let short = || refused(PackErrorKind::TruncatedValue);
        let (key, after) = rest.split_first_chunk::<32>().ok_or_else(short)?;
        let (kind, after) = after.split_first().ok_or_else(short)?;
        let (len, after) = after.split_first_chunk::<8>().ok_or_else(short)?;
        let len = usize::try_from(u64::from_le_bytes(*len)).map_err(|_| short())?;
        if after.len() < len {
            return Err(short());
        }
        let (value, after) = after.split_at(len);
        if values.last().is_some_and(|last: &Value| last.key >= *key) {
            return Err(refused(PackErrorKind::KeyOrder));
        }
        values.push(Value { key: *key, kind: *kind, bytes: value.to_vec() });
        rest = after;
    }
    if keys_digest(values.iter().map(|it| it.key)) != reference.keys {
        return Err(refused(PackErrorKind::Keys));
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
        assert!(matches!(Pack::batches_with_limits(vec![value(1, b"a"), value(1, b"b")], 1, usize::MAX)
            .unwrap_err().kind, PackErrorKind::ConflictingKey(_)),
            "conflicting keys on different partitions refuse");
        assert!(Pack::batches_with_limits(vec![value(1, b"a")], 0, usize::MAX).is_err());
    }

    #[test]
    fn large_native_packs_remain_readable_while_new_uploads_stay_partitioned() {
        let target = Limits::DOCUMENTED.batch_items as usize;
        let values: Vec<_> = (0..=target)
            .map(|i| {
                let mut key = [0; 32];
                key[24..].copy_from_slice(&(i as u64).to_be_bytes());
                Value { key, kind: (i % 256) as u8, bytes: vec![i as u8] } // alloc: bounded fixture
            })
            .collect(); // alloc: the full 65,537-record compatibility fixture
        // Encode the historical whole-pack layout independently of Pack::of, so
        // restoring either the reader's or producer's erroneous item cap fails.
        let mut bytes = DIST_PACK_MAGIC.to_vec(); // alloc: one historical native blob
        for value in &values {
            bytes.extend_from_slice(&value.key);
            bytes.push(value.kind);
            bytes.extend_from_slice(&(value.bytes.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&value.bytes);
        }
        let reference = PackRef {
            pack: Sha256::digest(&bytes).into(),
            size: bytes.len() as u64,
            keys: keys_digest(values.iter().map(|value| value.key)),
            packed: None,
            format: DIST_PACK_FORMAT,
        };
        let legacy = Pack { bytes, reference };
        assert_eq!(read(&legacy.bytes, &legacy.reference).unwrap(), values);
        assert_eq!(Pack::of(values.clone()).unwrap(), legacy);

        let batches = Pack::batches(values.clone()).unwrap();
        assert_eq!(batches.len(), 2, "new uploads still obey their item target");
        let mut recovered = Vec::new(); // alloc: full logical-output comparison
        for batch in &batches {
            assert!(batch.reference.size <= Limits::DOCUMENTED.blob);
            let decoded = read(&batch.bytes, &batch.reference).unwrap();
            assert!(decoded.len() <= target);
            recovered.extend(decoded);
        }
        assert_eq!(recovered, values, "partitioning cannot drop or change native values");

        // Removing the unrelated item cap must not remove the native byte cap.
        let mut oversized = legacy.reference.clone();
        oversized.size = Limits::DOCUMENTED.blob + 1;
        assert_eq!(read(&legacy.bytes, &oversized).unwrap_err().kind, PackErrorKind::BlobLimit);
    }

    #[test]
    fn two_values_under_one_key_are_refused() {
        let error = Pack::of(vec![value(1, b"a"), value(1, b"b")]).unwrap_err();
        assert_eq!(error.kind, PackErrorKind::ConflictingKey([1; 32]), "{error}");
        assert!(Pack::of(vec![value(1, b"a"), value(1, b"a")]).is_ok(), "one value, twice");
    }

    #[test]
    fn a_reader_believes_nothing_the_commit_does_not_name() {
        let pack = Pack::of(vec![value(1, b"one")]).unwrap();
        let mut flipped = pack.bytes.clone();
        *flipped.last_mut().unwrap() ^= 1;
        let refused = read(&flipped, &pack.reference).unwrap_err();
        assert_eq!(refused.pack, Some(pack.reference.pack));
        assert!(matches!(refused.kind, PackErrorKind::Digest(_)));
        let mut short = pack.reference.clone();
        short.size -= 1;
        assert!(matches!(read(&pack.bytes, &short).unwrap_err().kind, PackErrorKind::Length { .. }));
        let mut keys = pack.reference.clone();
        keys.keys = [9; 32];
        assert_eq!(read(&pack.bytes, &keys).unwrap_err().kind, PackErrorKind::Keys);
        // The right digest over bytes that are not a pack: refused by their shape.
        let junk = b"not a pack".to_vec();
        let reference = PackRef {
            pack: Sha256::digest(&junk).into(),
            size: junk.len() as u64,
            ..pack.reference.clone()
        };
        assert_eq!(read(&junk, &reference).unwrap_err().kind, PackErrorKind::Header);
    }
}
