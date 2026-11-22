//! Real codec outputs belong to the allocator their caller chose.

use std::alloc::{AllocError, Allocator, Global, Layout};
use std::cell::Cell;
use std::ptr::NonNull;

use crate::{
    Attempt, DIST_PACK_FORMAT, Encoder, ExecutionId, Grant, Lease, Limits, PackRef, PrNamespace,
    Set, Wire, WorkName, decode, digest_input, digest_of, encode, pack_list,
};

#[derive(Clone, Copy)]
struct Counted<'a>(&'a Cell<usize>);

// SAFETY: this forwards every block and layout unchanged to Global; every clone has the
// same allocation behavior and may deallocate blocks returned by any other clone.
unsafe impl Allocator for Counted<'_> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let allocation = Global.allocate(layout)?;
        self.0.set(self.0.get() + 1);
        Ok(allocation)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        // SAFETY: all pointers returned by this allocator came from Global with this layout.
        unsafe { Global.deallocate(pointer, layout) };
    }
}

#[test]
fn pack_lists_use_the_callers_allocator() {
    let packs = [PackRef {
        pack: [0xab; 32],
        size: 80,
        keys: [0xcd; 32],
        packed: None,
        format: DIST_PACK_FORMAT,
    }];
    let encoded_allocations = Cell::new(0);
    let encoded = pack_list::encode(&packs, Counted(&encoded_allocations));
    assert!(encoded_allocations.get() > 0);
    assert_eq!(encoded.as_slice(), pack_list::encode(&packs, Global).as_slice());

    let decoded_allocations = Cell::new(0);
    let decoded = pack_list::decode(&encoded, Counted(&decoded_allocations)).unwrap();
    assert_eq!(decoded.as_slice(), packs.as_slice());
    assert_eq!(decoded_allocations.get(), 1, "one output allocation, no ordering index");

    // Control: the same real decoder with a different allocator yields the same references
    // while making no allocation through the monitored allocator.
    decoded_allocations.set(0);
    let control = pack_list::decode(&encoded, Global).unwrap();
    assert_eq!(decoded.as_slice(), control.as_slice());
    assert_eq!(decoded_allocations.get(), 0);
}

#[test]
fn nested_schema_encoding_and_digest_scratch_use_the_callers_allocator() {
    let value = Grant {
        lease: Lease {
            execution: ExecutionId(3),
            attempt: Attempt(1),
            holder: "holder".into(),
            generation: 3,
        },
        roots: [(24, 2), (1, 2)].into_iter().collect(),
        names: [WorkName([9; 32]), WorkName([2; 32])].into_iter().collect(),
    };
    let allocations = Cell::new(0);
    let encoded = encode(&value, Counted(&allocations));
    assert!(allocations.get() > 0);
    assert!(std::ptr::eq(encoded.allocator().0, &allocations));
    assert_eq!(decode::<Grant>(&encoded, &Limits::DOCUMENTED).unwrap(), value);

    // Control: the same schema through the default allocator must write the same bytes and
    // leave the monitored allocator untouched.
    allocations.set(0);
    let control = encode(&value, Global);
    assert_eq!(encoded.as_slice(), control.as_slice());
    assert_eq!(allocations.get(), 0);

    // The output is already large enough: any allocation during this encode is nested set
    // scratch. A codec that propagates the allocator only to its outer buffer fails here.
    let mut preallocated = Vec::with_capacity_in(control.len(), Counted(&allocations));
    let pointer = preallocated.as_ptr();
    allocations.set(0);
    value.encode(&mut Encoder::new_into(&mut preallocated));
    assert!(allocations.get() > 0, "nested sets must use the encoder's scratch allocator");
    assert_eq!(preallocated.as_ptr(), pointer);
    assert_eq!(preallocated.as_slice(), control.as_slice());

    allocations.set(0);
    let digest = digest_of(&value, Counted(&allocations));
    assert!(allocations.get() > 0);
    allocations.set(0);
    assert_eq!(digest, digest_of(&value, Global));
    assert_eq!(allocations.get(), 0);

    let preimage = digest_input(&value, Counted(&allocations));
    assert!(allocations.get() > 0);
    assert!(std::ptr::eq(preimage.allocator().0, &allocations));
    assert_eq!(preimage.as_slice(), digest_input(&value, Global).as_slice());
}

#[test]
fn appending_keeps_the_callers_buffer_and_allocator_without_allocating() {
    let allocations = Cell::new(0);
    let mut bytes = Vec::with_capacity_in(64, Counted(&allocations));
    bytes.extend_from_slice(b"prefix");
    let pointer = bytes.as_ptr();
    let capacity = bytes.capacity();
    allocations.set(0);

    {
        let mut appender = Encoder::new_into(&mut bytes);
        PrNamespace { n: 24 }.encode(&mut appender);
        assert!(std::ptr::eq(appender.allocator().0, &allocations));
    }

    assert_eq!(bytes.as_slice(), b"prefix\xa1\x00\x18\x18");
    assert_eq!(bytes.as_ptr(), pointer);
    assert_eq!(bytes.capacity(), capacity);
    assert!(std::ptr::eq(bytes.allocator().0, &allocations));
    assert_eq!(allocations.get(), 0, "neither the temporary swap nor the append needs storage");
}

#[test]
fn sets_keep_their_storage_and_take_encoding_scratch_from_the_output_allocator() {
    let storage_allocations = Cell::new(0);
    let mut items = Vec::with_capacity_in(8, Counted(&storage_allocations));
    items.extend(["aa".to_owned(), "b".to_owned(), "aa".to_owned(), "a".to_owned()]);
    let pointer = items.as_ptr();
    let capacity = items.capacity();
    let first_duplicate = items[0].as_ptr();
    storage_allocations.set(0);

    let set = Set::new(items);
    assert!(storage_allocations.get() > 0, "canonicalization uses the input allocator");
    assert_eq!(set.as_slice(), ["a", "b", "aa"], "encoded order differs from lexical order");
    assert_eq!(set.as_slice()[2].as_ptr(), first_duplicate, "equal encodings keep the first item");
    let control: Set<String> = ["a", "b", "aa"].into_iter().map(str::to_owned).collect();
    // Counted deliberately has no comparison, formatting or hashing traits: those properties
    // belong to the items, independently of where the set keeps them.
    assert_eq!(set, control);
    assert_eq!(set.partial_cmp(&control), Some(std::cmp::Ordering::Equal));
    assert_eq!(set.cmp(&set), std::cmp::Ordering::Equal);
    let mut actual_hash = std::hash::DefaultHasher::new();
    let mut control_hash = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&set, &mut actual_hash);
    std::hash::Hash::hash(&control, &mut control_hash);
    assert_eq!(std::hash::Hasher::finish(&actual_hash), std::hash::Hasher::finish(&control_hash));

    let output_allocations = Cell::new(0);
    let mut encoded = Vec::with_capacity_in(64, Counted(&output_allocations));
    storage_allocations.set(0);
    output_allocations.set(0);
    set.encode(&mut Encoder::new_into(&mut encoded));
    assert_eq!(storage_allocations.get(), 0, "encoding must not allocate in the set's storage");
    assert!(output_allocations.get() > 0, "canonical encoding keys belong to the output allocator");
    assert_eq!(encoded.as_slice(), encode(&control, Global).as_slice());

    let items = set.into_vec();
    assert_eq!(items.as_ptr(), pointer);
    assert_eq!(items.capacity(), capacity);
    assert!(std::ptr::eq(items.allocator().0, &storage_allocations));
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

    let mut values = Vec::with_capacity_in(2, Global);
    values.extend([
        Changing { values: Cell::new((1, 9)), calls: Cell::new(0) },
        Changing { values: Cell::new((2, 0)), calls: Cell::new(0) },
    ]);
    let set = Set::new(values);
    assert!(set.iter().all(|item| item.calls.get() == 1));

    // Construction saw [1, 2]; the next snapshots are [9, 0]. Reencoding the items after
    // sorting those snapshots would emit [2, 1], which is not a canonical set.
    let bytes = encode(&set, Global);
    assert_eq!(bytes, [0x82, 0, 9]);
    assert!(set.iter().all(|item| item.calls.get() == 2));
    assert_eq!(decode::<Set<u64>>(&bytes, &Limits::DOCUMENTED).unwrap().as_slice(), &[0, 9]);
    assert!(decode::<Set<u64>>(&[0x82, 2, 1], &Limits::DOCUMENTED).is_err());
}
