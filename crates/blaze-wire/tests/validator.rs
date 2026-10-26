//! The validator refuses, naming each: non-shortest heads, indefinite lengths, unsorted keys,
//! duplicate keys, floats, tags, unknown keys and oversize items.
//!
//! Each case writes bytes by hand, because the encoder never writes what the validator refuses, so
//! the only way to see the refusal is to write the offence itself.

use blaze_wire::vectors::every;
use blaze_wire::{
    ErrorKind, Limits, Set, Wire, decode, encode, validate, validate_as, wire_struct,
};

fn limits() -> Limits {
    Limits::DOCUMENTED
}

/// The offence the validator names for `bytes`, as `(offence, what)`.
fn refused(bytes: &[u8]) -> (String, ErrorKind) {
    let error = validate(bytes, &limits()).expect_err("the validator accepted the bytes");
    (error.offence().to_owned(), error.kind)
}

/// The offence the schema decoder names for `bytes` read as `T`, after the bytes pass.
fn refused_as<T: Wire + std::fmt::Debug>(bytes: &[u8]) -> (String, ErrorKind) {
    let error = validate_as::<T>(bytes, &limits()).expect_err("the decoder accepted the bytes");
    (error.offence().to_owned(), error.kind)
}

#[test]
fn a_shortest_head_that_is_not_shortest_is_refused() {
    // 0 encoded in one byte of argument where the head's own form holds it.
    let cases: [(&[u8], &str); 4] = [
        // 1 as `0x18 0x01`, where `0x01` is the shortest.
        (&[0x18, 0x01], "non-shortest head"),
        // 24 as `0x19 0x00 0x18`, where `0x18 0x18` is.
        (&[0x19, 0x00, 0x18], "non-shortest head"),
        // 256 as `0x1a 0x00 0x00 0x01 0x00`, where `0x19 0x01 0x00` is.
        (&[0x1a, 0x00, 0x00, 0x01, 0x00], "non-shortest head"),
        // 65,536 as an eight-byte head, where four bytes hold it.
        (&[0x1b, 0, 0, 0, 0, 0, 1, 0, 0], "non-shortest head"),
    ];
    for (bytes, offence) in cases {
        let (named, kind) = refused(bytes);
        assert_eq!(named, offence);
        assert_eq!(kind, ErrorKind::NonShortestHead);
    }
    // The shortest form of each is accepted.
    for bytes in [&[0x01u8][..], &[0x18, 0x18], &[0x19, 0x01, 0x00], &[0x1a, 0x00, 0x01, 0x00, 0x00]] {
        validate(bytes, &limits()).expect("the shortest form was refused");
    }
}

#[test]
fn an_indefinite_length_is_refused() {
    let cases: [&[u8]; 5] = [
        // An indefinite byte string: `0x5f`, then chunks, then the break.
        &[0x5f, 0x41, 0x01, 0xff],
        // An indefinite text string.
        &[0x7f, 0x61, 0x61, 0xff],
        // An indefinite array.
        &[0x9f, 0x01, 0xff],
        // An indefinite map.
        &[0xbf, 0x01, 0x01, 0xff],
        // A definite array of one item that is an indefinite array.
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
    // `{1: 0, 0: 0}`: key 1 before key 0.
    let bytes = [0xa2, 0x01, 0x00, 0x00, 0x00];
    let (named, kind) = refused(&bytes);
    assert_eq!(named, "unsorted keys");
    assert_eq!(kind, ErrorKind::UnsortedKeys);
    // The sorted form of the same map is accepted.
    validate(&[0xa2, 0x00, 0x00, 0x01, 0x00], &limits()).expect("sorted keys were refused");
}

#[test]
fn duplicate_map_keys_are_refused() {
    // `{0: 0, 0: 0}`.
    let bytes = [0xa2, 0x00, 0x00, 0x00, 0x00];
    let (named, kind) = refused(&bytes);
    assert_eq!(named, "duplicate key");
    assert_eq!(kind, ErrorKind::DuplicateKey(0));
}

#[test]
fn a_float_is_refused() {
    let cases: [&[u8]; 3] = [
        // A half-precision 0.0, a single-precision and a double-precision.
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
    // Tag 0 (a date string) around a text string.
    let bytes = [0xc0, 0x61, 0x61];
    let (named, kind) = refused(&bytes);
    assert_eq!(named, "tag");
    assert_eq!(kind, ErrorKind::Tag);
    // Tag 1 (an epoch time) around an integer.
    let bytes = [0xc1, 0x01];
    assert_eq!(refused(&bytes).0, "tag");
}

#[test]
fn an_unknown_key_is_refused() {
    // `{0: ".", 1: ".", 2: null, 3: null, 4: ["", "", <32 zeros>], 5: ".", 6: 0}`: key 6 is not
    // `ExecPlatform`'s.
    let mut bytes = Vec::new();
    let v = blaze_wire::ExecPlatform {
        triple: ".".into(),
        os: ".".into(),
        sdk: None,
        libc: None,
        linker: (String::new(), String::new(), [0u8; 32]),
        cpu: ".".into(),
    };
    bytes.extend_from_slice(&encode(&v));
    // Replace the map's six keys with seven: a map of six is `a6`.
    bytes[0] = 0xa7;
    bytes.extend_from_slice(&[0x06, 0x00]);
    let (named, kind) = refused_as::<blaze_wire::ExecPlatform>(&bytes);
    assert_eq!(named, "unknown key");
    assert_eq!(kind, ErrorKind::UnknownKey(6));
}

#[test]
fn a_missing_key_is_refused() {
    // `ExecPlatform` without key 5: the map has five keys.
    let v = blaze_wire::ExecPlatform {
        triple: ".".into(),
        os: ".".into(),
        sdk: None,
        libc: None,
        linker: (String::new(), String::new(), [0u8; 32]),
        cpu: ".".into(),
    };
    let mut bytes = encode(&v);
    // The last field is key 5, one byte, and its value `"."`, two, so drop three and count five.
    bytes.truncate(bytes.len() - 3);
    bytes[0] = 0xa5;
    let (named, kind) = refused_as::<blaze_wire::ExecPlatform>(&bytes);
    assert_eq!(named, "missing key");
    assert_eq!(kind, ErrorKind::MissingKey(5));
}

#[test]
fn an_oversize_item_is_refused() {
    // A text string one byte past the limit, with its head.
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
    // A byte string past the blob limit.
    let limits = Limits { blob: 3, ..Limits::DOCUMENTED };
    let error = validate(&[0x44, 1, 2, 3, 4], &limits).expect_err("accepted");
    assert_eq!(error.offence(), "oversize item");
    // A whole message past the request limit.
    let limits = Limits { request: 2, ..Limits::DOCUMENTED };
    let error = validate(&[0x83, 0x01, 0x02, 0x03], &limits).expect_err("accepted");
    assert_eq!(error.offence(), "oversize item");
    // An array past the batch limit.
    let limits = Limits { batch_items: 2, ..Limits::DOCUMENTED };
    let error = validate(&[0x83, 0x01, 0x02, 0x03], &limits).expect_err("accepted");
    assert_eq!(error.offence(), "oversize item");
}

#[test]
fn an_unsorted_set_is_refused() {
    // A `Set<u32>` of `[2, 1]`, whose canonical order is `[1, 2]`.
    let bytes = [0x82, 0x02, 0x01];
    let error = decode::<Set<u32>>(&bytes, &limits()).expect_err("the set decoder accepted");
    assert_eq!(error.offence(), "unsorted set");
    assert_eq!(error.kind, ErrorKind::UnsortedSet);
    // The sorted form is taken.
    assert_eq!(decode::<Set<u32>>(&[0x82, 0x01, 0x02], &limits()).unwrap().as_slice(), &[1, 2]);
}

#[test]
fn a_duplicate_set_item_is_refused() {
    let bytes = [0x82, 0x01, 0x01];
    let error = decode::<Set<u32>>(&bytes, &limits()).expect_err("the set decoder accepted");
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
    // `MAX_DEPTH` arrays inside one another, and one more.
    let mut bytes = vec![0x81u8; blaze_wire::MAX_DEPTH as usize - 1];
    bytes.push(0x01);
    validate(&bytes, &limits()).expect("the deepest array was refused");
    bytes.insert(0, 0x81);
    let error = validate(&bytes, &limits()).expect_err("deeper nesting was accepted");
    assert_eq!(error.offence(), "nesting too deep");
}

#[test]
fn truncated_bytes_are_refused() {
    let cases: [&[u8]; 4] = [
        // Nothing at all.
        &[],
        // A head that says a byte string of two bytes, with one.
        &[0x42, 0x01],
        // A map that says one pair and has a key and no value.
        &[0xa1, 0x00],
        // An array that says two items and has one.
        &[0x82, 0x01],
    ];
    for bytes in cases {
        assert_eq!(refused(bytes).0, "truncated", "{bytes:02x?}");
    }
}

#[test]
fn text_that_is_not_utf8_is_refused() {
    // A two-byte text string whose bytes are not UTF-8.
    let bytes = [0x62, 0xff, 0xfe];
    let error = validate(&bytes, &limits()).expect_err("invalid UTF-8 was accepted");
    assert_eq!(error.offence(), "text that is not UTF-8");
}

#[test]
fn the_validator_accepts_what_the_encoder_writes_on_random_values() {
    // The same check as `roundtrip.rs`'s, from the validator's side: every type, many seeds.
    for entry in every() {
        for seed in 1..=256u64 {
            let cbor = (entry.vector)(seed).cbor;
            if let Err(error) = validate(&cbor, &limits()) {
                panic!("{} seed {seed}: {error}", entry.name);
            }
        }
    }
}

#[test]
fn every_offence_is_named() {
    // A test of the vocabulary itself: each kind has an offence and none is empty.
    let kinds = [
        ErrorKind::Truncated,
        ErrorKind::ReservedAdditional(28),
        ErrorKind::NonShortestHead,
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
    // A text string where a struct's map is asked for.
    let error = validate_as::<ExecPlatformForTest>(&[0x61, 0x61], &limits())
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
    // `-1`, which is `0x20`.
    let error = decode::<u32>(&[0x20], &limits()).expect_err("a negative integer read as u32");
    assert_eq!(error.kind, ErrorKind::UnexpectedType { expected: "an integer" });
}

#[test]
fn a_key_of_another_type_is_refused() {
    // A map whose key is a text string, where a schema map's key is an integer.
    let bytes = [0xa1, 0x61, 0x61, 0x00];
    let error = validate_as::<ExecPlatformForTest>(&bytes, &limits())
        .expect_err("a text key was read as a field number");
    assert_eq!(error.kind, ErrorKind::UnexpectedType { expected: "an integer key" });
}

#[test]
fn an_unknown_enum_discriminant_is_refused() {
    // `[9, null]` is no `Outcome`.
    let bytes = [0x82, 0x09, 0xf6];
    let error = decode::<blaze_wire::Outcome>(&bytes, &limits())
        .expect_err("an unknown discriminant was read");
    assert_eq!(error.kind, ErrorKind::UnknownKey(9));
}

#[test]
fn an_enum_of_the_wrong_length_is_refused() {
    // A three-element array, where an enum is two.
    let bytes = [0x83, 0x00, 0xf6, 0x00];
    let error = decode::<blaze_wire::Outcome>(&bytes, &limits()).expect_err("three elements");
    assert_eq!(error.kind, ErrorKind::UnexpectedType { expected: "an array of its length" });
}

#[test]
fn a_unit_variant_with_a_payload_is_refused() {
    // `[0, 0]` is no `Outcome::Passed`, whose payload is `null`.
    let error = decode::<blaze_wire::Outcome>(&[0x82, 0x00, 0x00], &limits())
        .expect_err("a unit variant took a payload");
    assert_eq!(error.kind, ErrorKind::UnexpectedType { expected: "null" });
}
