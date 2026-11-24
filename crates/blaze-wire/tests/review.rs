//! C1 schema regressions, including malformed encodings the encoder cannot produce.
//!
//! `Namespace::Pr` encodes as `[1, {0: n}]`, matching the golden vectors. JSON renders
//! enums as the same two-element array, as documented in `json.rs`.

#![no_std]
#![allow(missing_docs)]
#![feature(allocator_api)]

use core::alloc::Allocator;
use core::fmt::Debug;

use blaze_own::{SortedSet, String, Vec, format_in};
use blaze_wire::*;

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn head(major: u8, n: u64, alloc: WireAlloc) -> WireVec<u8> {
    let mut b = Vec::new_in(alloc);
    let m = major << 5;
    if let Ok(small) = u8::try_from(n) {
        if small < 24 {
            b.push(m | small);
        } else {
            b.extend([m | 24, small]);
        }
    } else if let Ok(v) = u16::try_from(n) {
        b.push(m | 25);
        b.extend_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = u32::try_from(n) {
        b.push(m | 26);
        b.extend_from_slice(&v.to_be_bytes());
    } else {
        b.push(m | 27);
        b.extend_from_slice(&n.to_be_bytes());
    }
    b
}

const EDGES: [u64; 10] = [
    0,
    23,
    24,
    255,
    256,
    65535,
    65536,
    4_294_967_295,
    4_294_967_296,
    u64::MAX,
];

fn limits() -> Limits {
    Limits::DOCUMENTED
}

fn kind(bytes: &[u8]) -> ErrorKind {
    validate(bytes, &limits()).unwrap_err().kind
}

fn identity<T: Wire + Debug>(bytes: &[u8], l: &Limits, alloc: WireAlloc) {
    validate(bytes, l).unwrap();
    let v = decode::<T>(bytes, l, alloc).unwrap();
    assert_eq!(encode(&v, alloc).as_slice(), bytes);
}

#[test]
fn uint_encoder_and_decoder_all_boundaries() {
    let alloc = heap();
    for n in EDGES {
        let b = head(0, n, alloc);
        assert_eq!(encode(&n, alloc).as_slice(), b.as_slice());
        identity::<u64>(&b, &limits(), alloc);
    }
}

#[test]
fn array_map_encoder_all_length_boundaries() {
    let alloc = heap();
    for n in EDGES.into_iter().filter(|n| usize::try_from(*n).is_ok()) {
        let count = usize::try_from(n).unwrap();
        let mut e = Encoder::new(alloc);
        e.array(count);
        assert_eq!(e.into_bytes().as_slice(), head(4, n, alloc).as_slice());
        let mut e = Encoder::new(alloc);
        e.map(count);
        assert_eq!(e.into_bytes().as_slice(), head(5, n, alloc).as_slice());
    }
}

#[test]
fn bytes_and_text_encoder_length_boundaries_through_65536() {
    let alloc = heap();
    let l = Limits { string: 65536, ..limits() };
    for n in EDGES.into_iter().filter(|n| *n <= 65536) {
        let count = usize::try_from(n).unwrap();
        let mut body = Vec::new_in(alloc);
        body.resize(count, b'a');
        let mut b = head(2, n, alloc);
        b.extend_from_slice(&body);
        let v = Bytes { bytes: body.clone() };
        assert_eq!(encode(&v, alloc).as_slice(), b.as_slice());
        identity::<Bytes>(&b, &l, alloc);
        let mut t = head(3, n, alloc);
        t.extend_from_slice(&body);
        let s = String::from_utf8(body).unwrap();
        assert_eq!(encode(&s, alloc).as_slice(), t.as_slice());
        identity::<WireString>(&t, &l, alloc);
    }
}

#[test]
fn negative_integers_all_boundaries_and_nonminimal_forms() {
    let alloc = heap();
    for n in EDGES {
        validate(&head(1, n, alloc), &limits()).unwrap();
    }
    for (ai, n) in [(24u8, 23u64), (25, 255), (26, 65535), (27, 4_294_967_295)] {
        let mut b = Vec::new_in(alloc);
        b.push(0x20 | ai);
        let width = 1usize << usize::from(ai - 24);
        b.extend_from_slice(&n.to_be_bytes()[8 - width..]);
        assert_eq!(kind(&b), ErrorKind::NonShortestHead);
    }
}

#[test]
fn all_major_heads_nonminimal_at_each_transition() {
    let alloc = heap();
    for major in 0..=5u8 {
        for (ai, n) in [(24u8, 23u64), (25, 255), (26, 65535), (27, 4_294_967_295)] {
            let mut b = Vec::new_in(alloc);
            b.push((major << 5) | ai);
            let width = 1usize << usize::from(ai - 24);
            b.extend_from_slice(&n.to_be_bytes()[8 - width..]);
            assert_eq!(kind(&b), ErrorKind::NonShortestHead, "major={major}, n={n}");
        }
    }
}

#[test]
fn length_prefixes_truncated_or_oversize_no_allocation_bombs() {
    let alloc = heap();
    let wide = Limits {
        request: u32::MAX,
        string: u32::MAX,
        blob: u64::MAX,
        batch_items: u32::MAX,
        ..limits()
    };
    for major in 2..=5 {
        for n in EDGES.into_iter().filter(|n| *n > 0) {
            let b = head(major, n, alloc);
            let e = validate(&b, &wide).unwrap_err();
            assert!(
                matches!(e.kind, ErrorKind::Truncated | ErrorKind::OversizeItem { .. }),
                "{major}, {n}: {e}"
            );
            let e = validate(&b, &limits()).unwrap_err();
            assert!(matches!(e.kind, ErrorKind::Truncated | ErrorKind::OversizeItem { .. }));
        }
    }
    for b in [head(2, u64::MAX, alloc), head(3, u64::MAX, alloc), head(4, u64::MAX, alloc)] {
        assert!(decode::<Bytes>(&b, &wide, alloc).is_err());
        assert!(decode::<WireString>(&b, &wide, alloc).is_err());
        assert!(decode::<WireVec<u64>>(&b, &wide, alloc).is_err());
    }
}

#[test]
fn every_proper_prefix_of_representative_items_is_truncated() {
    let alloc = heap();
    let mut zeros = Vec::new_in(alloc);
    zeros.resize(24, 0u8);
    let mut nums = Vec::new_in(alloc);
    nums.extend([u64::MAX, 24]);
    let values = [
        encode(&u64::MAX, alloc),
        encode(&String::from_str_in("hello", alloc), alloc),
        encode(&Bytes { bytes: zeros }, alloc),
        encode(&nums, alloc),
    ];
    for b in values {
        for end in 0..b.len() {
            assert_eq!(kind(&b[..end]), ErrorKind::Truncated);
        }
    }
}

#[test]
fn utf8_and_bytes_are_distinct() {
    let alloc = heap();
    let bad_slices: [&[u8]; 5] = [
        &[0xff],
        &[0xc0, 0x80],
        &[0xed, 0xa0, 0x80],
        &[0xf4, 0x90, 0x80, 0x80],
        &[0xe2, 0x82],
    ];
    for bad in bad_slices {
        let len = u64::try_from(bad.len()).unwrap();
        let mut t = head(3, len, alloc);
        t.extend_from_slice(bad);
        assert_eq!(kind(&t), ErrorKind::BadUtf8);
        assert!(decode::<WireString>(&t, &limits(), alloc).is_err());
        let mut b = head(2, len, alloc);
        b.extend_from_slice(bad);
        identity::<Bytes>(&b, &limits(), alloc);
    }
    identity::<WireString>(&encode(&String::from_str_in("é😀", alloc), alloc), &limits(), alloc);
    assert!(decode::<WireString>(&[0x41, 0x61], &limits(), alloc).is_err());
    assert!(decode::<Bytes>(&[0x61, 0x61], &limits(), alloc).is_err());
}

#[test]
fn mixed_map_keys_sort_encoded_bytes_not_numeric_or_length_first() {
    let alloc = heap();
    validate(&[0xa2, 0x18, 0x18, 0, 0x20, 0], &limits()).unwrap();
    assert_eq!(kind(&[0xa2, 0x20, 0, 0x18, 0x18, 0]), ErrorKind::UnsortedKeys);
    validate(&[0xa2, 0x41, 0, 0, 0x60, 0], &limits()).unwrap();
    assert_eq!(kind(&[0xa2, 0x60, 0, 0x41, 0, 0]), ErrorKind::UnsortedKeys);
    assert!(matches!(kind(&[0xa2, 0x61, b'a', 0, 0x61, b'a', 0]), ErrorKind::DuplicateKey(_)));
    assert_eq!(kind(&[0xa1, 0x18, 0, 0]), ErrorKind::NonShortestHead);
    assert!(decode::<Ack>(&[0xa1, 0x18, 0, 0], &limits(), alloc).is_err());
}

#[test]
fn indefinite_floats_tags_reserved_and_trailing_named() {
    let alloc = heap();
    for b in [0x5f, 0x7f, 0x9f, 0xbf, 0xff] {
        assert_eq!(kind(&[b]), ErrorKind::IndefiniteLength);
    }
    for b in [0xf9, 0xfa, 0xfb] {
        assert_eq!(kind(&[b]), ErrorKind::Float);
    }
    for b in [0xc0, 0xd8] {
        assert_eq!(kind(&[b]), ErrorKind::Tag);
    }
    for m in 0..=5 {
        for ai in 28..=30 {
            assert_eq!(kind(&[(m << 5) | ai]), ErrorKind::ReservedAdditional(ai));
        }
    }
    assert_eq!(kind(&[0, 0]), ErrorKind::TrailingBytes);
    assert!(decode::<u64>(&[0, 0], &limits(), alloc).is_err());
}

#[test]
fn validator_rejects_nonminimal_simple_values() {
    for n in 0..=23 {
        let b = [0xf8, n];
        assert_eq!(kind(&b), ErrorKind::ReservedSimpleValue(n), "accepted {b:02x?}");
    }
}

#[test]
fn validator_rejects_reserved_simple_values() {
    for n in 24..=31 {
        let b = [0xf8, n];
        assert_eq!(kind(&b), ErrorKind::ReservedSimpleValue(n), "accepted {b:02x?}");
    }
}

#[test]
fn deep_input_is_bounded_without_stack_overflow() {
    let alloc = heap();
    let mut b = Vec::new_in(alloc);
    b.resize(100_000, 0x81u8);
    b.push(0);
    assert_eq!(kind(&b), ErrorKind::TooDeep);
}

#[derive(Debug)]
enum Node {
    Array(WireVec<Node>),
    Leaf(u64),
}

impl Wire for Node {
    fn encode<A: Allocator + Clone>(&self, e: &mut Encoder<A>) {
        match self {
            Self::Array(a) => a.encode(e),
            Self::Leaf(n) => n.encode(e),
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        if r.position() == 32 {
            return u64::decode(r).map(Self::Leaf);
        }
        WireVec::<Node>::decode(r).map(Self::Array)
    }
}

#[test]
fn decode_never_accepts_validator_rejected_depth() {
    let alloc = heap();
    let mut b = Vec::new_in(alloc);
    b.resize(32, 0x81u8);
    b.push(0);
    assert_eq!(kind(&b), ErrorKind::TooDeep);
    assert!(
        decode::<Node>(&b, &limits(), alloc).is_err(),
        "decode accepted 32 arrays plus scalar, rejected by validator"
    );
}

#[test]
fn decode_enforces_whole_request_size() {
    let alloc = heap();
    let l = Limits { request: 0, ..limits() };
    assert!(validate(&[0], &l).is_err());
    assert!(decode::<u64>(&[0], &l, alloc).is_err(), "decode ignored request limit");
}

#[test]
fn ordinary_decoder_enforces_real_documented_16mib_request_limit() {
    let alloc = heap();
    let mut payload = Vec::new_in(alloc);
    payload.resize(usize::try_from(limits().request).unwrap(), 0u8);
    let b = encode(&Bytes { bytes: payload }, alloc);
    assert!(matches!(
        validate(&b, &limits()).unwrap_err().kind,
        ErrorKind::OversizeItem { what: "message", .. }
    ));
    assert!(
        decode::<Bytes>(&b, &limits(), alloc).is_err(),
        "default decoder accepts 16 MiB blob plus 5-byte CBOR head, over request limit"
    );
}

#[test]
fn a_batch_keeps_the_batch_limit_not_the_lookup_limit() {
    let alloc = heap();
    let mut roots = Vec::with_capacity_in(4097, alloc);
    for n in 0..4097u64 {
        roots.push((n, 0));
    }
    let grant = Grant {
        lease: Lease {
            execution: ExecutionId(1),
            attempt: Attempt(1),
            holder: String::from_str_in("executor", alloc),
            generation: 1,
        },
        roots: Set::new(roots),
        names: Set::empty_in(alloc),
    };
    assert!(
        validate_as::<Grant>(&encode(&grant, alloc), &limits(), alloc).is_ok(),
        "a batch's set of 4097 was narrowed to the lookup limit"
    );
    let mut roots = Vec::with_capacity_in(65537, alloc);
    for n in 0..65537u64 {
        roots.push((n, 0));
    }
    let grant = Grant { roots: Set::new(roots), ..grant };
    assert!(validate_as::<Grant>(&encode(&grant, alloc), &limits(), alloc).is_err());
}

#[test]
fn digest_input_matches_documented_formula() {
    let alloc = heap();
    let v = Attempt(24);
    let mut expected = Vec::new_in(alloc);
    expected.extend_from_slice(b"blaze/Attempt/v5\0");
    expected.extend_from_slice(&encode(&v, alloc));
    assert_eq!(digest_of(&v, alloc), blaze_sha256::digest(&expected));
    assert_eq!(digest_input(&v, alloc).as_slice(), expected.as_slice());
}

#[test]
fn every_digest_input_is_the_encoding_the_golden_digest_uses() {
    let alloc = heap();
    for entry in every() {
        let vector = (entry.vector)(1, alloc);
        assert!(!vector.cbor.is_empty(), "{}: the vector's CBOR is empty", entry.name);
        assert_eq!(
            vector.digest,
            digest_of_encoded_name(entry.name, &vector.cbor),
            "{}: the golden digest is not its CBOR's",
            entry.name
        );
    }
    let v = Attempt(24);
    let input = digest_input(&v, alloc);
    assert!(!input.is_empty(), "digest_input returned no bytes");
    assert_eq!(blaze_sha256::digest(&input), digest_of(&v, alloc));
    let mut expected = Vec::new_in(alloc);
    expected.extend_from_slice(b"blaze/Attempt/v5\0");
    expected.extend_from_slice(&encode(&v, alloc));
    assert_eq!(input.as_slice(), expected.as_slice());
    assert_eq!(blaze_sha256::digest(&expected), digest_of(&v, alloc));
}

#[test]
fn the_reserved_simple_value_offence_is_named() {
    for n in [0u8, 20, 23, 24, 31] {
        let error = validate(&[0xf8, n], &limits()).unwrap_err();
        assert_eq!(error.offence(), "reserved simple value", "f8 {n:02x}");
        assert_eq!(error.kind, ErrorKind::ReservedSimpleValue(n));
        let error = validate(&[0xa1, 0xf8, n, 0], &limits()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::ReservedSimpleValue(n));
    }
}

#[test]
fn new_into_really_appends_to_caller_output() {
    let alloc = heap();
    let mut b = Vec::new_in(alloc);
    b.push(42);
    {
        let mut e = Encoder::new_into(&mut b);
        e.uint(1);
    }
    assert_eq!(b.as_slice(), [42, 1]);
}

#[test]
fn all_domains_are_unique_and_prefix_free() {
    let alloc = heap();
    let entries = every();
    assert_eq!(entries.len(), 43);
    let mut names = SortedSet::new_in(alloc);
    let mut domains = Vec::new_in(alloc);
    for e in entries {
        assert!(names.insert(e.name));
        assert!(!e.name.contains('\0'));
        domains.push(format_in!(alloc, "blaze/{}/v5\0", e.name).into_bytes());
    }
    for (i, a) in domains.iter().enumerate() {
        for (j, b) in domains.iter().enumerate() {
            if i != j {
                assert!(!a.starts_with(b));
            }
        }
    }
}

#[test]
fn namespace_pr_agrees_with_doc_payload_map() {
    let alloc = heap();
    assert_eq!(
        encode(&Namespace::Pr(PrNamespace { n: 1 }), alloc).as_slice(),
        [0x82, 1, 0xa1, 0x00, 0x01],
        "a Pr payload is the map [1, {{0: n}}], as the doc's payload list says"
    );
}

#[test]
fn accepted_negative_u64_max_json_does_not_overflow() {
    let alloc = heap();
    let b = head(1, u64::MAX, alloc);
    validate(&b, &limits()).unwrap();
    let mut out = String::new_in(alloc);
    json::render_cbor(&b, &mut out).unwrap();
    assert_eq!(out.as_str(), "-18446744073709551616");
}

#[test]
fn accepted_schema_input_reencodes_identically_via_validate_as() {
    let alloc = heap();
    for e in every() {
        for seed in 1..=8 {
            let b = (e.vector)(seed, alloc).cbor;
            validate(&b, &limits()).unwrap();
            assert_eq!((e.reencode)(&b, alloc).unwrap().as_slice(), b.as_slice());
        }
    }
}

type Nest0 = u64;
type Nest1 = WireVec<Nest0>;
type Nest2 = WireVec<Nest1>;
type Nest3 = WireVec<Nest2>;
type Nest4 = WireVec<Nest3>;
type Nest5 = WireVec<Nest4>;
type Nest6 = WireVec<Nest5>;
type Nest7 = WireVec<Nest6>;
type Nest8 = WireVec<Nest7>;
type Nest9 = WireVec<Nest8>;
type Nest10 = WireVec<Nest9>;
type Nest11 = WireVec<Nest10>;
type Nest12 = WireVec<Nest11>;
type Nest13 = WireVec<Nest12>;
type Nest14 = WireVec<Nest13>;
type Nest15 = WireVec<Nest14>;
type Nest16 = WireVec<Nest15>;
type Nest17 = WireVec<Nest16>;
type Nest18 = WireVec<Nest17>;
type Nest19 = WireVec<Nest18>;
type Nest20 = WireVec<Nest19>;
type Nest21 = WireVec<Nest20>;
type Nest22 = WireVec<Nest21>;
type Nest23 = WireVec<Nest22>;
type Nest24 = WireVec<Nest23>;
type Nest25 = WireVec<Nest24>;
type Nest26 = WireVec<Nest25>;
type Nest27 = WireVec<Nest26>;
type Nest28 = WireVec<Nest27>;
type Nest29 = WireVec<Nest28>;
type Nest30 = WireVec<Nest29>;
type Nest31 = WireVec<Nest30>;
type Nest32 = WireVec<Nest31>;

#[test]
fn builtin_vec_decoder_never_accepts_validator_rejected_depth() {
    let alloc = heap();
    let mut b = Vec::new_in(alloc);
    b.resize(32, 0x81u8);
    b.push(0);
    assert_eq!(kind(&b), ErrorKind::TooDeep);
    assert!(
        decode::<Nest32>(&b, &limits(), alloc).is_err(),
        "built-in Vec<...<u64>> decoder accepts depth 33"
    );
}

fn ordinary_audit<T: Wire + Named + Sample>(seed: u64, alloc: WireAlloc) {
    let mut g = Gen::new(seed);
    let original = encode(&T::sample(&mut g, alloc), alloc);
    assert!(validate(&original, &limits()).is_ok());
    let back = decode::<T>(&original, &limits(), alloc).unwrap();
    assert_eq!(encode(&back, alloc).as_slice(), original.as_slice());
    for i in 0..original.len().min(300) {
        for mask in [1, 0x80, 0xff] {
            let mut bytes = original.clone();
            bytes[i] ^= mask;
            if let Ok(value) = decode::<T>(&bytes, &limits(), alloc) {
                assert!(
                    validate(&bytes, &limits()).is_ok(),
                    "{} decoded validator-rejected bytes {bytes:02x?}",
                    T::NAME
                );
                assert_eq!(
                    encode(&value, alloc).as_slice(),
                    bytes.as_slice(),
                    "{} accepted nonidentity",
                    T::NAME
                );
            }
        }
    }
}

#[test]
fn full_array_and_map_lengths_at_65535_65536() {
    let alloc = heap();
    for n in [23usize, 24, 255, 256, 65535, 65536] {
        let mut e = Encoder::new(alloc);
        e.array(n);
        for _ in 0..n {
            e.uint(0);
        }
        let b = e.into_bytes();
        validate(&b, &limits()).unwrap();
        identity::<WireVec<u64>>(&b, &limits(), alloc);
        let mut e = Encoder::new(alloc);
        e.map(n);
        for i in 0..n {
            e.uint(u64::try_from(i).unwrap());
            e.uint(0);
        }
        validate(&e.into_bytes(), &limits()).unwrap();
    }
}

#[test]
fn simple_value_sweep_reports_all_illegal_encodings() {
    let alloc = heap();
    let mut wrongly_accepted = Vec::new_in(alloc);
    for n in 0..32u8 {
        if validate(&[0xf8, n], &limits()).is_ok() {
            wrongly_accepted.push(n);
        }
    }
    assert!(
        wrongly_accepted.is_empty(),
        "validator wrongly accepts all of {wrongly_accepted:?} after f8"
    );
    for n in 32..=255 {
        validate(&[0xf8, n], &limits()).unwrap();
    }
}

#[test]
fn json_enum_rendering_agrees_with_public_docs() {
    let alloc = heap();
    let mut json = String::new_in(alloc);
    to_json(&Namespace::Main, &mut json, alloc).unwrap();
    assert_eq!(
        json.as_str(),
        r#"{"Namespace":[0,null]}"#,
        "json.rs documents the wire's [discriminant, payload] array"
    );
}

#[test]
fn illegal_simple_map_key_is_rejected_and_named() {
    let b = [0xa1, 0xf8, 0x14, 0];
    assert!(validate(&b, &limits()).is_err(), "nonminimal false map key accepted");
}

#[test]
fn unknown_schema_field_and_missing_field_named() {
    let alloc = heap();
    assert!(matches!(
        validate_as::<Ack>(&[0xa1, 0, 0], &limits(), alloc).unwrap_err().kind,
        ErrorKind::UnknownKey(0)
    ));
    assert!(matches!(
        validate_as::<PrNamespace>(&[0xa0], &limits(), alloc).unwrap_err().kind,
        ErrorKind::MissingKey(0)
    ));
}

macro_rules! audit_each {
    ($($ty:ty),* $(,)?) => {
        #[test]
        fn ordinary_decoder_schema_identity_and_mutations_of_every_type() {
            let alloc = heap();
            let mut names = SortedSet::new_in(alloc);
            $(ordinary_audit::<$ty>(1, alloc); names.insert(<$ty as Named>::NAME);)*
            let mut listed = SortedSet::new_in(alloc);
            for it in blaze_wire::vectors::every() {
                listed.insert(it.name);
            }
            assert_eq!(names, listed, "the audit does not cover the schema's list");
        }
    };
}

audit_each!(
    LocalPolicy,
    Namespace,
    PrNamespace,
    PackRef,
    WorkKey,
    ItemKey,
    EdgeKey,
    Found,
    RootSet,
    Lease,
    Grant,
    ItemOutcome,
    EntryRef,
    Why,
    ItemResult,
    BatchResult,
    Reservation,
    Stage,
    JobEvent,
    StageEvent,
    AssignedEvent,
    RefusedEvent,
    OutputEvent,
    DoneEvent,
    Outcome,
    EntryKey,
    JobContent,
    JobContentInput,
    WorkName,
    ExecutionId,
    Attempt,
    Ack,
    JobRequest,
    JobAnswer,
    EventItem,
    WorkJoinRequest,
    WorkJoinAnswer,
    WorkLeaseRequest,
    WorkLeaseAnswer,
    WorkResultAnswer,
    WorkersEventsRequest,
    WorkersEventsAnswer,
    WireError,
);
