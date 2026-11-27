//! The validator refuses, naming each: non-shortest heads, indefinite lengths, unsorted keys,
//! duplicate keys, floats, tags, unknown keys and oversize items.

#![no_std]
#![feature(allocator_api)]

use core::fmt::Debug;

use blaze_own::Vec;
use blaze_wire::vectors::every;
use blaze_wire::{
    ErrorKind, Limits, Set, Wire, WireAlloc, WireString, WireVec, decode, encode, validate,
    validate_as, wire_struct,
};

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn limits() -> Limits {
    Limits::DOCUMENTED
}

fn refused(bytes: &[u8]) -> (&'static str, ErrorKind) {
    let error = validate(bytes, &limits()).expect_err("the validator accepted the bytes");
    (error.offence(), error.kind)
}

fn refused_as<T: Wire + Debug>(bytes: &[u8], alloc: WireAlloc) -> (&'static str, ErrorKind) {
    let error = validate_as::<T>(bytes, &limits(), alloc).expect_err("the decoder accepted the bytes");
    (error.offence(), error.kind)
}

#[test]
fn a_shortest_head_that_is_not_shortest_is_refused() {
    let cases: [(&[u8], &str); 4] = [
        (&[0x18, 0x01], "non-shortest head"),
        (&[0x19, 0x00, 0x18], "non-shortest head"),
        (&[0x1a, 0x00, 0x00, 0x01, 0x00], "non-shortest head"),
        (&[0x1b, 0, 0, 0, 0, 0, 1, 0, 0], "non-shortest head"),
    ];
    for (bytes, offence) in cases {
        let (named, kind) = refused(bytes);
        assert_eq!(named, offence);
        assert_eq!(kind, ErrorKind::NonShortestHead);
    }
    for bytes in
        [&[0x01u8][..], &[0x18, 0x18], &[0x19, 0x01, 0x00], &[0x1a, 0x00, 0x01, 0x00, 0x00]]
    {
        validate(bytes, &limits()).expect("the shortest form was refused");
    }
}

#[test]
fn an_indefinite_length_is_refused() {
    let cases: [&[u8]; 5] = [
        &[0x5f, 0x41, 0x01, 0xff],
        &[0x7f, 0x61, 0x61, 0xff],
        &[0x9f, 0x01, 0xff],
        &[0xbf, 0x01, 0x01, 0xff],
        &[0x81, 0x9f, 0xff],
    ];
    for bytes in cases {
        let (named, kind) = refused(bytes);
        assert_eq!(named, "indefinite length", "{bytes:02x?}");
        assert_eq!(kind, ErrorKind::IndefiniteLength);
    }
}

#[test]
fn unsorted_map_keys_are_refused() {
    let bytes = [0xa2, 0x01, 0x00, 0x00, 0x00];
    let (named, kind) = refused(&bytes);
    assert_eq!(named, "unsorted keys");
    assert_eq!(kind, ErrorKind::UnsortedKeys);
    validate(&[0xa2, 0x00, 0x00, 0x01, 0x00], &limits()).expect("sorted keys were refused");
}

#[test]
fn duplicate_map_keys_are_refused() {
    let bytes = [0xa2, 0x00, 0x00, 0x00, 0x00];
    let (named, kind) = refused(&bytes);
    assert_eq!(named, "duplicate key");
    assert_eq!(kind, ErrorKind::DuplicateKey(0));
}

#[test]
fn a_two_byte_simple_value_below_32_is_refused() {
    for n in 0..32u8 {
        let bytes = [0xf8, n];
        let (named, kind) = refused(&bytes);
        assert_eq!(named, "reserved simple value", "f8 {n:02x} was accepted");
        assert_eq!(kind, ErrorKind::ReservedSimpleValue(n));
    }
    for n in [0xf4u8, 0xf5, 0xf6] {
        validate(&[n], &limits()).expect("a one-byte simple value was refused");
    }
    for n in 32..=255u8 {
        validate(&[0xf8, n], &limits()).expect("a two-byte simple value of 32 or more was refused");
    }
    let error = validate(&[0xa1, 0xf8, 0x14, 0x00], &limits()).expect_err("a nonminimal key");
    assert_eq!(error.kind, ErrorKind::ReservedSimpleValue(0x14));
}

#[test]
fn the_decoder_refuses_what_the_validator_refuses() {
    let alloc = heap();
    let small = Limits { request: 1, ..limits() };
    assert!(validate(&[0x18, 0x18], &small).is_err());
    assert!(decode::<u64>(&[0x18, 0x18], &small, alloc).is_err(), "decode ignored the request limit");
    let small = Limits { string: 1, ..limits() };
    assert!(validate(&[0x62, b'a', b'b'], &small).is_err());
    assert!(
        decode::<WireString>(&[0x62, b'a', b'b'], &small, alloc).is_err(),
        "decode ignored the string limit"
    );
    let small = Limits { batch_items: 1, ..limits() };
    assert!(validate(&[0x82, 0x01, 0x02], &small).is_err());
    assert!(
        decode::<WireVec<u64>>(&[0x82, 0x01, 0x02], &small, alloc).is_err(),
        "decode ignored the array limit"
    );
}

#[test]
fn a_float_is_refused() {
    let cases: [&[u8]; 3] = [
        &[0xf9, 0x00, 0x00],
        &[0xfa, 0x00, 0x00, 0x00, 0x00],
        &[0xfb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    ];
    for bytes in cases {
        let (named, kind) = refused(bytes);
        assert_eq!(named, "float");
        assert_eq!(kind, ErrorKind::Float);
    }
}

#[test]
fn a_tag_is_refused() {
    let bytes = [0xc0, 0x61, 0x61];
    let (named, kind) = refused(&bytes);
    assert_eq!(named, "tag");
    assert_eq!(kind, ErrorKind::Tag);
    let bytes = [0xc1, 0x01];
    assert_eq!(refused(&bytes).0, "tag");
}

#[test]
fn an_oversize_item_is_refused() {
    let limits = Limits { string: 4, ..Limits::DOCUMENTED };
    let bytes = [0x65, b'a', b'b', b'c', b'd', b'e'];
    let error = validate(&bytes, &limits).expect_err("the validator accepted an oversize string");
    assert_eq!(error.offence(), "oversize item");
    match error.kind {
        ErrorKind::OversizeItem { what, len, limit } => {
            assert_eq!(what, "text string");
            assert_eq!(len, 5);
            assert_eq!(limit, 4);
        }
        other => panic!("{other:?}"),
    }
    let limits = Limits { blob: 3, ..Limits::DOCUMENTED };
    let error = validate(&[0x44, 1, 2, 3, 4], &limits).expect_err("accepted");
    assert_eq!(error.offence(), "oversize item");
    let limits = Limits { request: 2, ..Limits::DOCUMENTED };
    let error = validate(&[0x83, 0x01, 0x02, 0x03], &limits).expect_err("accepted");
    assert_eq!(error.offence(), "oversize item");
    let limits = Limits { batch_items: 2, ..Limits::DOCUMENTED };
    let error = validate(&[0x83, 0x01, 0x02, 0x03], &limits).expect_err("accepted");
    assert_eq!(error.offence(), "oversize item");
}

#[test]
fn an_unsorted_set_is_refused() {
    let alloc = heap();
    let bytes = [0x82, 0x02, 0x01];
    let error = decode::<Set<u32>>(&bytes, &limits(), alloc).expect_err("the set decoder accepted");
    assert_eq!(error.offence(), "unsorted set");
    assert_eq!(error.kind, ErrorKind::UnsortedSet);
    assert_eq!(decode::<Set<u32>>(&[0x82, 0x01, 0x02], &limits(), alloc).unwrap().as_slice(), &[1, 2]);
}

#[test]
fn a_duplicate_set_item_is_refused() {
    let alloc = heap();
    let bytes = [0x82, 0x01, 0x01];
    let error = decode::<Set<u32>>(&bytes, &limits(), alloc).expect_err("the set decoder accepted");
    assert_eq!(error.offence(), "duplicate item");
    assert_eq!(error.kind, ErrorKind::DuplicateItem);
}

#[test]
fn bytes_after_the_item_are_refused() {
    let bytes = [0x01, 0x02];
    let error = validate(&bytes, &limits()).expect_err("the validator accepted trailing bytes");
    assert_eq!(error.offence(), "bytes after the item");
    assert_eq!(error.kind, ErrorKind::TrailingBytes);
}

#[test]
fn nesting_past_the_limit_is_refused() {
    let alloc = heap();
    let mut bytes = Vec::new_in(alloc);
    bytes.resize(usize::try_from(blaze_wire::MAX_DEPTH).unwrap() - 1, 0x81u8);
    bytes.push(0x01);
    validate(&bytes, &limits()).expect("the deepest array was refused");
    bytes.insert(0, 0x81);
    let error = validate(&bytes, &limits()).expect_err("deeper nesting was accepted");
    assert_eq!(error.offence(), "nesting too deep");
}

#[test]
fn truncated_bytes_are_refused() {
    let cases: [&[u8]; 4] = [
        &[],
        &[0x42, 0x01],
        &[0xa1, 0x00],
        &[0x82, 0x01],
    ];
    for bytes in cases {
        assert_eq!(refused(bytes).0, "truncated", "{bytes:02x?}");
    }
}

#[test]
fn text_that_is_not_utf8_is_refused() {
    let bytes = [0x62, 0xff, 0xfe];
    let error = validate(&bytes, &limits()).expect_err("invalid UTF-8 was accepted");
    assert_eq!(error.offence(), "text that is not UTF-8");
}

#[test]
fn the_validator_accepts_what_the_encoder_writes_on_random_values() {
    let alloc = heap();
    for entry in every() {
        for seed in 1..=256u64 {
            let cbor = (entry.vector)(seed, alloc).cbor;
            if let Err(error) = validate(&cbor, &limits()) {
                panic!("{} seed {seed}: {error}", entry.name);
            }
        }
    }
}

#[test]
fn every_offence_is_named() {
    let kinds = [
        ErrorKind::Truncated,
        ErrorKind::ReservedAdditional(28),
        ErrorKind::NonShortestHead,
        ErrorKind::ReservedSimpleValue(0),
        ErrorKind::IndefiniteLength,
        ErrorKind::Float,
        ErrorKind::Tag,
        ErrorKind::UnsortedKeys,
        ErrorKind::DuplicateKey(1),
        ErrorKind::UnsortedSet,
        ErrorKind::DuplicateItem,
        ErrorKind::UnknownKey(9),
        ErrorKind::MissingKey(9),
        ErrorKind::OversizeItem { what: "map", len: 2, limit: 1 },
        ErrorKind::UnexpectedType { expected: "a map" },
        ErrorKind::BadUtf8,
        ErrorKind::TrailingBytes,
        ErrorKind::TooDeep,
    ];
    for kind in kinds {
        assert!(!kind.offence().is_empty(), "{kind:?} has no offence");
    }
}

#[test]
fn an_unexpected_type_is_refused_by_name() {
    let alloc = heap();
    let error = validate_as::<ExecPlatformForTest>(&[0x61, 0x61], &limits(), alloc)
        .expect_err("a text string was read as a map");
    assert_eq!(error.offence(), "unexpected type");
    assert_eq!(error.kind, ErrorKind::UnexpectedType { expected: "a map" });
}

wire_struct! {
    /// A struct of one field, for the type check.
    ExecPlatformForTest {
        0 => only: u8,
    }
}

#[test]
fn a_negative_integer_is_refused_where_an_unsigned_one_is_asked_for() {
    let alloc = heap();
    let error = decode::<u32>(&[0x20], &limits(), alloc).expect_err("a negative integer read as u32");
    assert_eq!(error.kind, ErrorKind::UnexpectedType { expected: "an integer" });
}

#[test]
fn a_key_of_another_type_is_refused() {
    let alloc = heap();
    let bytes = [0xa1, 0x61, 0x61, 0x00];
    let error = validate_as::<ExecPlatformForTest>(&bytes, &limits(), alloc)
        .expect_err("a text key was read as a field number");
    assert_eq!(error.kind, ErrorKind::UnexpectedType { expected: "an integer key" });
}

#[test]
fn an_unknown_enum_discriminant_is_refused() {
    let alloc = heap();
    let bytes = [0x82, 0x09, 0xf6];
    let error = decode::<blaze_wire::Outcome>(&bytes, &limits(), alloc)
        .expect_err("an unknown discriminant was read");
    assert_eq!(error.kind, ErrorKind::UnknownKey(9));
}

#[test]
fn an_enum_of_the_wrong_length_is_refused() {
    let alloc = heap();
    let bytes = [0x83, 0x00, 0xf6, 0x00];
    let error = decode::<blaze_wire::Outcome>(&bytes, &limits(), alloc).expect_err("three elements");
    assert_eq!(error.kind, ErrorKind::UnexpectedType { expected: "an array of its length" });
}

#[test]
fn a_unit_variant_with_a_payload_is_refused() {
    let alloc = heap();
    let error = decode::<blaze_wire::Outcome>(&[0x82, 0x00, 0x00], &limits(), alloc)
        .expect_err("a unit variant took a payload");
    assert_eq!(error.kind, ErrorKind::UnexpectedType { expected: "null" });
}

#[test]
fn an_unknown_key_is_refused() {
    let alloc = heap();
    let v = blaze_wire::Reservation { memory: 1, cpus: 1, disk: 1 };
    let mut bytes = encode(&v, alloc);
    bytes[0] = 0xa4;
    bytes.extend_from_slice(&[0x03, 0x00]);
    let (named, kind) = refused_as::<blaze_wire::Reservation>(&bytes, alloc);
    assert_eq!(named, "unknown key");
    assert_eq!(kind, ErrorKind::UnknownKey(3));
}

#[test]
fn a_missing_key_is_refused() {
    let alloc = heap();
    let v = blaze_wire::Reservation { memory: 1, cpus: 1, disk: 1 };
    let mut bytes = encode(&v, alloc);
    bytes.truncate(bytes.len() - 2);
    bytes[0] = 0xa2;
    let (named, kind) = refused_as::<blaze_wire::Reservation>(&bytes, alloc);
    assert_eq!(named, "missing key");
    assert_eq!(kind, ErrorKind::MissingKey(2));
}
