//! Real codec outputs belong to the allocator their caller chose.

use core::alloc::{AllocError, Allocator, Layout};
use core::cell::Cell;
use core::cmp::Ordering;
use core::hash::{Hash, Hasher};
use core::ptr::{self, NonNull};

use blaze_own::{String, Vec};

use crate::{
    Attempt, DIST_PACK_FORMAT, Encoder, ExecutionId, Grant, Lease, Limits, PackRef, PrNamespace,
    Set, Wire, WireAlloc, WireString, WorkName, decode, digest_input, digest_of, encode, pack_list,
};

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

#[derive(Clone, Copy)]
struct Counted<'a> {
    base: WireAlloc,
    count: &'a Cell<usize>,
}

// SAFETY: this forwards every block and layout unchanged to `base`; every clone shares the
// same backing allocator and may deallocate blocks returned by any other clone.
unsafe impl Allocator for Counted<'_> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let allocation = self.base.allocate(layout)?;
        self.count.set(self.count.get() + 1);
        Ok(allocation)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        // SAFETY: all pointers returned by this allocator came from `base` with this layout.
        unsafe { self.base.deallocate(pointer, layout) };
    }
}

struct FnvHasher(u64);

impl FnvHasher {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }
}

impl Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

#[test]
fn pack_lists_use_the_callers_allocator() {
    let base = heap();
    let packs = [PackRef {
        pack: [0xab; 32],
        size: 80,
        keys: [0xcd; 32],
        packed: None,
        format: DIST_PACK_FORMAT,
    }];
    let encoded_allocations = Cell::new(0);
    let counted_enc = Counted { base, count: &encoded_allocations };
    let encoded = pack_list::encode(&packs, counted_enc);
    assert!(encoded_allocations.get() > 0);
    assert_eq!(encoded.as_slice(), pack_list::encode(&packs, base).as_slice());

    let decoded_allocations = Cell::new(0);
    let counted_dec = Counted { base, count: &decoded_allocations };
    let decoded = pack_list::decode(&encoded, counted_dec).unwrap();
    assert_eq!(decoded.as_slice(), packs.as_slice());
    assert_eq!(decoded_allocations.get(), 1, "one output allocation, no ordering index");

    decoded_allocations.set(0);
    let control = pack_list::decode(&encoded, base).unwrap();
    assert_eq!(decoded.as_slice(), control.as_slice());
    assert_eq!(decoded_allocations.get(), 0);
}

#[test]
fn nested_schema_encoding_and_digest_scratch_use_the_callers_allocator() {
    let base = heap();
    let value = Grant {
        lease: Lease {
            execution: ExecutionId(3),
            attempt: Attempt(1),
            holder: WireString::from_str_in("holder", base),
            generation: 3,
        },
        roots: Set::from_iter_in([(24, 2), (1, 2)], base),
        names: Set::from_iter_in([WorkName([9; 32]), WorkName([2; 32])], base),
    };
    let allocations = Cell::new(0);
    let counted = Counted { base, count: &allocations };
    let encoded = encode(&value, counted);
    assert!(allocations.get() > 0);
    assert!(ptr::eq(encoded.allocator().count, &allocations));
    assert_eq!(decode::<Grant>(&encoded, &Limits::DOCUMENTED, base).unwrap(), value);

    allocations.set(0);
    let control = encode(&value, base);
    assert_eq!(encoded.as_slice(), control.as_slice());
    assert_eq!(allocations.get(), 0);

    let mut preallocated = Vec::with_capacity_in(control.len(), counted);
    let pointer = preallocated.as_ptr();
    allocations.set(0);
    value.encode(&mut Encoder::new_into(&mut preallocated));
    assert!(allocations.get() > 0, "nested sets must use the encoder's scratch allocator");
    assert_eq!(preallocated.as_ptr(), pointer);
    assert_eq!(preallocated.as_slice(), control.as_slice());

    allocations.set(0);
    let digest = digest_of(&value, counted);
    assert!(allocations.get() > 0);
    allocations.set(0);
    assert_eq!(digest, digest_of(&value, base));
    assert_eq!(allocations.get(), 0);

    let preimage = digest_input(&value, counted);
    assert!(allocations.get() > 0);
    assert!(ptr::eq(preimage.allocator().count, &allocations));
    assert_eq!(preimage.as_slice(), digest_input(&value, base).as_slice());
}

#[test]
fn appending_keeps_the_callers_buffer_and_allocator_without_allocating() {
    let base = heap();
    let allocations = Cell::new(0);
    let counted = Counted { base, count: &allocations };
    let mut bytes = Vec::with_capacity_in(64, counted);
    bytes.extend_from_slice(b"prefix");
    let pointer = bytes.as_ptr();
    let capacity = bytes.capacity();
    allocations.set(0);

    {
        let mut appender = Encoder::new_into(&mut bytes);
        PrNamespace { n: 24 }.encode(&mut appender);
        assert!(ptr::eq(appender.allocator().count, &allocations));
    }

    assert_eq!(bytes.as_slice(), b"prefix\xa1\x00\x18\x18");
    assert_eq!(bytes.as_ptr(), pointer);
    assert_eq!(bytes.capacity(), capacity);
    assert!(ptr::eq(bytes.allocator().count, &allocations));
    assert_eq!(allocations.get(), 0, "neither the temporary swap nor the append needs storage");
}

#[test]
fn sets_keep_their_storage_and_take_encoding_scratch_from_the_output_allocator() {
    let base = heap();
    let storage_allocations = Cell::new(0);
    let storage = Counted { base, count: &storage_allocations };
    let mut items = Vec::with_capacity_in(8, storage);
    for text in ["aa", "b", "aa", "a"] {
        items.push(String::from_str_in(text, base));
    }
    let pointer = items.as_ptr();
    let capacity = items.capacity();
    let first_duplicate = items[0].as_ptr();
    storage_allocations.set(0);

    let set = Set::new(items);
    assert!(storage_allocations.get() > 0, "canonicalization uses the input allocator");
    assert_eq!(set.as_slice()[0].as_str(), "a");
    assert_eq!(set.as_slice()[1].as_str(), "b");
    assert_eq!(set.as_slice()[2].as_str(), "aa");
    assert_eq!(set.as_slice()[2].as_ptr(), first_duplicate, "equal encodings keep the first item");

    let control: Set<WireString> =
        Set::from_iter_in(["a", "b", "aa"].into_iter().map(|s| String::from_str_in(s, base)), base);
    assert_eq!(set, control);
    assert_eq!(set.partial_cmp(&control), Some(Ordering::Equal));
    assert_eq!(set.cmp(&set), Ordering::Equal);
    let mut actual_hash = FnvHasher::new();
    let mut control_hash = FnvHasher::new();
    Hash::hash(&set, &mut actual_hash);
    Hash::hash(&control, &mut control_hash);
    assert_eq!(actual_hash.finish(), control_hash.finish());

    let output_allocations = Cell::new(0);
    let output = Counted { base, count: &output_allocations };
    let mut encoded = Vec::with_capacity_in(64, output);
    storage_allocations.set(0);
    output_allocations.set(0);
    set.encode(&mut Encoder::new_into(&mut encoded));
    assert_eq!(storage_allocations.get(), 0, "encoding must not allocate in the set's storage");
    assert!(output_allocations.get() > 0, "canonical encoding keys belong to the output allocator");
    assert_eq!(encoded.as_slice(), encode(&control, base).as_slice());

    let items = set.into_vec();
    assert_eq!(items.as_ptr(), pointer);
    assert_eq!(items.capacity(), capacity);
    assert!(ptr::eq(items.allocator().count, &storage_allocations));
}

#[test]
fn set_encoding_writes_each_canonical_sort_key_once() {
    struct Changing {
        values: Cell<(u64, u64)>,
        calls: Cell<usize>,
    }

    impl Wire for Changing {
        fn encode<A: Allocator + Clone>(&self, encoder: &mut Encoder<A>) {
            let (now, next) = self.values.get();
            self.values.set((next, now));
            self.calls.set(self.calls.get() + 1);
            encoder.uint(now);
        }

        fn decode(reader: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
            let value = reader.uint()?;
            Ok(Self { values: Cell::new((value, value)), calls: Cell::new(0) })
        }
    }

    let base = heap();
    let mut values = Vec::with_capacity_in(2, base);
    values.push(Changing { values: Cell::new((1, 9)), calls: Cell::new(0) });
    values.push(Changing { values: Cell::new((2, 0)), calls: Cell::new(0) });
    let set = Set::new(values);
    assert!(set.iter().all(|item| item.calls.get() == 1));

    let bytes = encode(&set, base);
    assert_eq!(bytes.as_slice(), [0x82, 0, 9]);
    assert!(set.iter().all(|item| item.calls.get() == 2));
    assert_eq!(decode::<Set<u64>>(&bytes, &Limits::DOCUMENTED, base).unwrap().as_slice(), &[0, 9]);
    assert!(decode::<Set<u64>>(&[0x82, 2, 1], &Limits::DOCUMENTED, base).is_err());
}
