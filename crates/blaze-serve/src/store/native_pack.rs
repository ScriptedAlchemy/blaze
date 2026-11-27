//! Pure `#![no_std]` native distributed pack codec used by `blaze-serve` tests.
//!
//! Layout: `DIST_PACK_MAGIC` (`b"blzdpk\0\x01"`), followed by records sorted strictly by 32-byte
//! key, each `(key: [u8; 32], kind: u8, len: u64 LE, bytes: [u8; len])`.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;
use blaze_wire::{
    DIST_PACK_FORMAT, DIST_PACK_MAGIC, Digest, Encoder, Limits, PackRef, WireAlloc,
    digest_of_encoded_name,
};

/// The native pack requirement that a producer or reader refused.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum PackErrorKind {
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
#[allow(dead_code)]
pub(crate) struct PackError {
    /// The committed pack being read; producers have no commitment yet.
    pub(crate) pack: Option<Digest>,
    /// The condition that refused the pack.
    pub(crate) kind: PackErrorKind,
}

impl PackError {
    const fn producing(kind: PackErrorKind) -> Self {
        Self { pack: None, kind }
    }
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            PackErrorKind::ConflictingKey(_) => f.write_str("two values under one key"),
            PackErrorKind::SizeOverflow => f.write_str("pack byte length overflows usize"),
            PackErrorKind::BlobLimit => write!(f, "exceeds {}-byte blob limit", Limits::DOCUMENTED.blob),
            PackErrorKind::PartitionLimits => f.write_str("partition limits too small"),
            PackErrorKind::ValueSizeOverflow(_) => f.write_str("native value size overflow"),
            PackErrorKind::ValueLimit(_) => f.write_str("native value exceeds blob limit"),
            PackErrorKind::Format(fmt_id) => write!(f, "unsupported format {fmt_id}"),
            PackErrorKind::Length { actual, committed } => {
                write!(f, "is {actual} bytes, committed as {committed}")
            }
            PackErrorKind::Digest(_) => f.write_str("pack digest mismatch"),
            PackErrorKind::Header => f.write_str("has no header"),
            PackErrorKind::TruncatedValue => f.write_str("ends inside a value"),
            PackErrorKind::KeyOrder => f.write_str("is not in key order"),
            PackErrorKind::Keys => f.write_str("holds other keys than were committed"),
        }
    }
}

/// One value: a store key, its kind's code, its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Value {
    /// The store key.
    pub(crate) key: Digest,
    /// The kind's code.
    pub(crate) kind: u8,
    /// The value bytes.
    pub(crate) bytes: Vec<u8, WireAlloc>,
}

/// One checked value borrowed from the complete native pack that contains it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct ValueRef<'a> {
    /// The store key.
    pub(crate) key: Digest,
    /// The kind's code.
    pub(crate) kind: u8,
    /// The value bytes.
    pub(crate) bytes: &'a [u8],
}

impl PartialEq<Value> for ValueRef<'_> {
    fn eq(&self, other: &Value) -> bool {
        self.key == other.key && self.kind == other.kind && self.bytes == other.bytes.as_slice()
    }
}

impl PartialEq<ValueRef<'_>> for Value {
    fn eq(&self, other: &ValueRef<'_>) -> bool {
        other == self
    }
}

/// A pack as it goes on the wire.
#[derive(Clone)]
pub(crate) struct Pack<A: Allocator> {
    /// The whole pack.
    pub(crate) bytes: Vec<u8, A>,
    /// What a result commits about it.
    pub(crate) reference: PackRef,
}

impl<A: Allocator> fmt::Debug for Pack<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pack")
            .field("bytes_len", &self.bytes.len())
            .field("reference", &self.reference)
            .finish()
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Pack<B>> for Pack<A> {
    fn eq(&self, other: &Pack<B>) -> bool {
        self.bytes.as_slice() == other.bytes.as_slice() && self.reference == other.reference
    }
}

impl<A: Allocator> Eq for Pack<A> {}

/// Canonicalize the entire set BEFORE partitioning, so identical duplicates
/// coalesce and conflicting keys cannot be concealed at a pack boundary.
pub(crate) fn canonical<A: Allocator>(mut values: Vec<Value, A>) -> Result<Vec<Value, A>, PackError> {
    values.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    values.dedup();
    for pair in values.windows(2) {
        if let (Some(a), Some(b)) = (pair.first(), pair.get(1)) {
            if a.key == b.key {
                return Err(PackError::producing(PackErrorKind::ConflictingKey(a.key)));
            }
        }
    }
    Ok(values)
}

impl<A: Allocator + Clone> Pack<A> {
    /// Encodes `values` in canonical key order into a single native pack in `alloc`.
    pub(crate) fn of(values: Vec<Value, impl Allocator>, alloc: A) -> Result<Pack<A>, PackError> {
        let values = canonical(values)?;
        let size = values
            .iter()
            .try_fold(DIST_PACK_MAGIC.len(), |acc: usize, value| {
                acc.checked_add(41)?.checked_add(value.bytes.len())
            })
            .ok_or_else(|| PackError::producing(PackErrorKind::SizeOverflow))?;
        let size_u64 = u64::try_from(size)
            .map_err(|_| PackError::producing(PackErrorKind::SizeOverflow))?;
        if size_u64 > Limits::DOCUMENTED.blob {
            return Err(PackError::producing(PackErrorKind::BlobLimit));
        }
        let mut bytes = Vec::with_capacity_in(size, alloc.clone());
        bytes.extend_from_slice(&DIST_PACK_MAGIC);
        for value in &values {
            let vlen = u64::try_from(value.bytes.len())
                .map_err(|_| PackError::producing(PackErrorKind::ValueSizeOverflow(value.key)))?;
            bytes.extend_from_slice(&value.key);
            bytes.push(value.kind);
            bytes.extend_from_slice(&vlen.to_le_bytes());
            bytes.extend_from_slice(value.bytes.as_slice());
        }
        let reference = PackRef {
            pack: blaze_sha256::digest(bytes.as_slice()),
            size: size_u64,
            keys: keys_digest(values.iter().map(|it| it.key), alloc),
            packed: None,
            format: DIST_PACK_FORMAT,
        };
        Ok(Pack { bytes, reference })
    }

    /// Partitions `values` with explicit `max_items` and `target_bytes`.
    #[allow(dead_code)]
    pub(crate) fn batches_with_limits(
        values: Vec<Value, impl Allocator>,
        max_items: usize,
        target_bytes: usize,
        alloc: A,
    ) -> Result<Vec<Pack<A>, A>, PackError> {
        if max_items == 0 || target_bytes < DIST_PACK_MAGIC.len() {
            return Err(PackError::producing(PackErrorKind::PartitionLimits));
        }
        let values = canonical(values)?;
        let mut packs = Vec::new_in(alloc.clone());
        if values.is_empty() {
            packs.push(Self::of(values, alloc)?);
            return Ok(packs);
        }
        let blob_limit = usize::try_from(Limits::DOCUMENTED.blob).unwrap_or(usize::MAX);
        let mut group = Vec::new_in(alloc.clone());
        let mut size = DIST_PACK_MAGIC.len();
        for value in values {
            let entry = value
                .bytes
                .len()
                .checked_add(41)
                .ok_or_else(|| PackError::producing(PackErrorKind::ValueSizeOverflow(value.key)))?;
            if entry > blob_limit.saturating_sub(DIST_PACK_MAGIC.len()) {
                return Err(PackError::producing(PackErrorKind::ValueLimit(value.key)));
            }
            if !group.is_empty()
                && (group.len() >= max_items
                    || size.saturating_add(entry) > target_bytes
                    || size.saturating_add(entry) > blob_limit)
            {
                let complete = core::mem::replace(&mut group, Vec::new_in(alloc.clone()));
                packs.push(Self::of(complete, alloc.clone())?);
                size = DIST_PACK_MAGIC.len();
            }
            size = size
                .checked_add(entry)
                .ok_or_else(|| PackError::producing(PackErrorKind::SizeOverflow))?;
            group.push(value);
        }
        packs.push(Self::of(group, alloc)?);
        Ok(packs)
    }
}

pub(crate) fn keys_digest<A: Allocator + Clone>(
    keys: impl ExactSizeIterator<Item = Digest>,
    alloc: A,
) -> Digest {
    let mut encoded = Encoder::new(alloc);
    encoded.array(keys.len());
    for key in keys {
        encoded.bytes(&key);
    }
    digest_of_encoded_name("blaze/dist-pack-keys", encoded.into_bytes().as_slice())
}

/// Validates and parses `bytes` against `reference`, borrowing each payload slice.
#[allow(dead_code)]
pub(crate) fn read<'a, A: Allocator + Clone>(
    bytes: &'a [u8],
    reference: &PackRef,
    alloc: A,
) -> Result<Vec<ValueRef<'a>, A>, PackError> {
    let refused = |kind| PackError { pack: Some(reference.pack), kind };
    if reference.format != DIST_PACK_FORMAT {
        return Err(refused(PackErrorKind::Format(reference.format)));
    }
    if reference.size > Limits::DOCUMENTED.blob {
        return Err(refused(PackErrorKind::BlobLimit));
    }
    let actual = u64::try_from(bytes.len()).map_err(|_| refused(PackErrorKind::SizeOverflow))?;
    if actual != reference.size {
        return Err(refused(PackErrorKind::Length { actual, committed: reference.size }));
    }
    let got = blaze_sha256::digest(bytes);
    if got != reference.pack {
        return Err(refused(PackErrorKind::Digest(got)));
    }
    let mut rest = bytes
        .strip_prefix(&DIST_PACK_MAGIC)
        .ok_or_else(|| refused(PackErrorKind::Header))?;
    let mut values = Vec::new_in(alloc.clone());
    while !rest.is_empty() {
        let short = || refused(PackErrorKind::TruncatedValue);
        let (key, after) = rest.split_first_chunk::<32>().ok_or_else(short)?;
        let (kind, after) = after.split_first().ok_or_else(short)?;
        let (len, after) = after.split_first_chunk::<8>().ok_or_else(short)?;
        let len = usize::try_from(u64::from_le_bytes(*len)).map_err(|_| short())?;
        let Some((value, after)) = after.split_at_checked(len) else {
            return Err(short());
        };
        if values.last().is_some_and(|last: &ValueRef<'_>| last.key >= *key) {
            return Err(refused(PackErrorKind::KeyOrder));
        }
        values.push(ValueRef { key: *key, kind: *kind, bytes: value });
        rest = after;
    }
    if keys_digest(values.iter().map(|it| it.key), alloc) != reference.keys {
        return Err(refused(PackErrorKind::Keys));
    }
    Ok(values)
}

#[cfg(test)]
pub(crate) mod tests {
    use core::alloc::{AllocError, Allocator, Layout};
    use core::cell::Cell;
    use core::ptr::NonNull;

    use blaze_own::Vec;
    use blaze_wire::WireAlloc;

    use super::*;

    #[derive(Default)]
    struct Counts {
        allocations: Cell<usize>,
        deallocations: Cell<usize>,
    }

    #[derive(Clone, Copy)]
    struct Counted<'a> {
        counts: &'a Counts,
        inner: WireAlloc,
    }

    unsafe impl Allocator for Counted<'_> {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            let ptr = self.inner.allocate(layout)?;
            self.counts.allocations.set(self.counts.allocations.get().saturating_add(1));
            Ok(ptr)
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            self.counts.deallocations.set(self.counts.deallocations.get().saturating_add(1));
            // SAFETY: delegates to the same `inner` allocator that allocated `ptr`.
            unsafe { self.inner.deallocate(ptr, layout) };
        }
    }

    fn value(alloc: WireAlloc, key: u8, bytes: &[u8]) -> Value {
        let mut buf = Vec::with_capacity_in(bytes.len(), alloc);
        buf.extend_from_slice(bytes);
        Value { key: [key; 32], kind: 1, bytes: buf }
    }

    #[test]
    fn packs_and_record_views_use_the_callers_allocator() {
        let base = blaze_own::testing::heap().shared();
        let counts = Counts::default();
        let counted = Counted { counts: &counts, inner: base.clone() };

        let mut input = Vec::new_in(counted);
        input.push(value(base.clone(), 2, b"two"));
        input.push(value(base.clone(), 1, b"one"));
        input.push(value(base.clone(), 1, b"one"));
        let pointer = input.as_ptr();
        let Ok(input) = canonical(input) else {
            panic!("canonicalization failed");
        };
        assert_eq!(input.as_ptr(), pointer, "canonicalization must retain the input allocation");
        assert!(core::ptr::eq(input.allocator().counts, &counts));
        assert_eq!(input.len(), 2);

        let Ok(pack) = Pack::of(input, counted) else {
            panic!("Pack::of failed");
        };
        let mut control_in = Vec::new_in(base.clone());
        control_in.push(value(base.clone(), 1, b"one"));
        control_in.push(value(base.clone(), 2, b"two"));
        let Ok(control) = Pack::of(control_in, base.clone()) else {
            panic!("control Pack::of failed");
        };
        assert_eq!(pack, control);
        assert!(core::ptr::eq(pack.bytes.allocator().counts, &counts));

        let Ok(views) = read(pack.bytes.as_slice(), &pack.reference, counted) else {
            panic!("read failed");
        };
        assert!(core::ptr::eq(views.allocator().counts, &counts));
        assert_eq!(views.len(), 2);
        assert_eq!(views.first(), Some(&ValueRef { key: [1; 32], kind: 1, bytes: b"one" }));
        assert_eq!(views.get(1), Some(&ValueRef { key: [2; 32], kind: 1, bytes: b"two" }));

        let start = pack.bytes.as_ptr().addr();
        let end = start.saturating_add(pack.bytes.len());
        for view in &views {
            let ptr = view.bytes.as_ptr().addr();
            assert!(ptr >= start && ptr.saturating_add(view.bytes.len()) <= end);
        }

        let mut keys = Vec::new_in(base.clone());
        keys.push([1u8; 32]);
        keys.push([2u8; 32]);
        let encoded_keys = blaze_wire::encode(&keys, base.clone());
        assert_eq!(
            pack.reference.keys,
            digest_of_encoded_name("blaze/dist-pack-keys", encoded_keys.as_slice())
        );
        drop(views);
        drop(pack);
        assert_eq!(counts.allocations.get(), counts.deallocations.get());

        let make_source = || {
            let mut v = Vec::new_in(base.clone());
            v.push(value(base.clone(), 3, b"three"));
            v.push(value(base.clone(), 1, b"one"));
            v.push(value(base.clone(), 2, b"two"));
            v
        };
        let Ok(packs) = Pack::batches_with_limits(make_source(), 2, usize::MAX, counted) else {
            panic!("batches_with_limits failed");
        };
        let Ok(control_packs) = Pack::batches_with_limits(make_source(), 2, usize::MAX, base.clone()) else {
            panic!("control batches_with_limits failed");
        };
        assert_eq!(packs, control_packs);
        assert_eq!(packs.len(), 2);
        assert!(core::ptr::eq(packs.allocator().counts, &counts));
        for p in &packs {
            assert!(core::ptr::eq(p.bytes.allocator().counts, &counts));
        }
        drop(packs);
        assert_eq!(counts.allocations.get(), counts.deallocations.get());
    }
}
