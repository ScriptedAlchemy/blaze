//! The C1 verifier's failing cases, kept as regressions.
//!
//! Every test here failed against the first cut of the crate, and each asserts the behaviour the
//! schema is meant to have rather than the bug it had. The suite is the verifier's own scratch
//! file, reduced to the crate's tests, with two cases that the doc settled the other way: the
//! `Namespace::Pr` payload is the map `[1, {0: n}]` the code and golden vectors use, and an enum
//! renders as the wire's two-element array, which is what `json.rs` now documents.
//!
//! The tests write bytes by hand where the encoder never writes them, and reach into the crate's
//! own types where a public API is what the fix is about.

#![allow(missing_docs)]

use blaze_wire::*;
use ed25519_dalek::{Signature, Signer, SigningKey};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;

fn head(major: u8, n: u64) -> Vec<u8> {
    let mut b = vec![];
    let m = major << 5;
    if n < 24 {
        b.push(m | n as u8);
    } else if n <= 255 {
        b.extend([m | 24, n as u8]);
    } else if n <= 65535 {
        b.push(m | 25);
        b.extend((n as u16).to_be_bytes());
    } else if n <= u32::MAX as u64 {
        b.push(m | 26);
        b.extend((n as u32).to_be_bytes());
    } else {
        b.push(m | 27);
        b.extend(n.to_be_bytes());
    }
    b
}

const EDGES: [u64; 10] =
    [0, 23, 24, 255, 256, 65535, 65536, u32::MAX as u64, u32::MAX as u64 + 1, u64::MAX];

fn limits() -> Limits {
    Limits::DOCUMENTED
}

fn kind(bytes: &[u8]) -> ErrorKind {
    validate(bytes, &limits()).unwrap_err().kind
}

fn identity<T: Wire + std::fmt::Debug>(bytes: &[u8], l: &Limits) {
    validate(bytes, l).unwrap();
    let v = decode::<T>(bytes, l).unwrap();
    assert_eq!(encode(&v), bytes);
}

#[test]
fn uint_encoder_and_decoder_all_boundaries() {
    for n in EDGES {
        let b = head(0, n);
        assert_eq!(encode(&n), b);
        identity::<u64>(&b, &limits());
    }
}

#[test]
fn array_map_encoder_all_length_boundaries() {
    for n in EDGES.into_iter().filter(|n| *n <= usize::MAX as u64) {
        let mut e = Encoder::new();
        e.array(n as usize);
        assert_eq!(e.into_bytes(), head(4, n));
        let mut e = Encoder::new();
        e.map(n as usize);
        assert_eq!(e.into_bytes(), head(5, n));
    }
}

#[test]
fn bytes_and_text_encoder_length_boundaries_through_65536() {
    let l = Limits { string: 65536, ..limits() };
    for n in EDGES.into_iter().filter(|n| *n <= 65536) {
        let body = vec![b'a'; n as usize];
        let mut b = head(2, n);
        b.extend(&body);
        let v = Bytes::new(body.clone());
        assert_eq!(encode(&v), b);
        identity::<Bytes>(&b, &l);
        let mut t = head(3, n);
        t.extend(&body);
        let s = String::from_utf8(body).unwrap();
        assert_eq!(encode(&s), t);
        identity::<String>(&t, &l);
    }
}

#[test]
fn negative_integers_all_boundaries_and_nonminimal_forms() {
    // Major type 1 stores -1 - n, including -2^64.
    for n in EDGES {
        validate(&head(1, n), &limits()).unwrap();
    }
    for (ai, n) in [(24, 23u64), (25, 255), (26, 65535), (27, u32::MAX as u64)] {
        let mut b = vec![0x20 | ai];
        let width = 1usize << (ai - 24);
        b.extend_from_slice(&n.to_be_bytes()[8 - width..]);
        assert_eq!(kind(&b), ErrorKind::NonShortestHead);
    }
}

#[test]
fn all_major_heads_nonminimal_at_each_transition() {
    for major in 0..=5 {
        for (ai, n) in [(24, 23u64), (25, 255), (26, 65535), (27, u32::MAX as u64)] {
            let mut b = vec![(major << 5) | ai];
            let width = 1usize << (ai - 24);
            b.extend_from_slice(&n.to_be_bytes()[8 - width..]);
            assert_eq!(kind(&b), ErrorKind::NonShortestHead, "major={major}, n={n}");
        }
    }
}

#[test]
fn length_prefixes_truncated_or_oversize_no_allocation_bombs() {
    let wide = Limits {
        request: u32::MAX,
        string: u32::MAX,
        blob: u64::MAX,
        batch_items: u32::MAX,
        ..limits()
    };
    for major in 2..=5 {
        for n in EDGES.into_iter().filter(|n| *n > 0) {
            let b = head(major, n);
            let e = validate(&b, &wide).unwrap_err();
            assert!(
                matches!(e.kind, ErrorKind::Truncated | ErrorKind::OversizeItem { .. }),
                "{major}, {n}: {e}"
            );
            let e = validate(&b, &limits()).unwrap_err();
            assert!(matches!(e.kind, ErrorKind::Truncated | ErrorKind::OversizeItem { .. }));
        }
    }
    for b in [head(2, u64::MAX), head(3, u64::MAX), head(4, u64::MAX)] {
        assert!(decode::<Bytes>(&b, &wide).is_err());
        assert!(decode::<String>(&b, &wide).is_err());
        assert!(decode::<Vec<u64>>(&b, &wide).is_err());
    }
}

#[test]
fn every_proper_prefix_of_representative_items_is_truncated() {
    let values = [
        encode(&u64::MAX),
        encode(&"hello".to_owned()),
        encode(&Bytes::new(vec![0; 24])),
        encode(&vec![u64::MAX, 24]),
    ];
    for b in values {
        for end in 0..b.len() {
            assert_eq!(kind(&b[..end]), ErrorKind::Truncated);
        }
    }
}

#[test]
fn utf8_and_bytes_are_distinct() {
    for bad in [
        vec![0xff],
        vec![0xc0, 0x80],
        vec![0xed, 0xa0, 0x80],
        vec![0xf4, 0x90, 0x80, 0x80],
        vec![0xe2, 0x82],
    ] {
        let mut t = head(3, bad.len() as u64);
        t.extend(&bad);
        assert_eq!(kind(&t), ErrorKind::BadUtf8);
        assert!(decode::<String>(&t, &limits()).is_err());
        let mut b = head(2, bad.len() as u64);
        b.extend(&bad);
        identity::<Bytes>(&b, &limits());
    }
    identity::<String>(&encode(&"é😀".to_owned()), &limits());
    assert!(decode::<String>(&[0x41, 0x61], &limits()).is_err());
    assert!(decode::<Bytes>(&[0x61, 0x61], &limits()).is_err());
}

#[test]
fn mixed_map_keys_sort_encoded_bytes_not_numeric_or_length_first() {
    validate(&[0xa2, 0x18, 0x18, 0, 0x20, 0], &limits()).unwrap();
    assert_eq!(kind(&[0xa2, 0x20, 0, 0x18, 0x18, 0]), ErrorKind::UnsortedKeys);
    validate(&[0xa2, 0x41, 0, 0, 0x60, 0], &limits()).unwrap();
    assert_eq!(kind(&[0xa2, 0x60, 0, 0x41, 0, 0]), ErrorKind::UnsortedKeys);
    assert!(matches!(kind(&[0xa2, 0x61, b'a', 0, 0x61, b'a', 0]), ErrorKind::DuplicateKey(_)));
    assert_eq!(kind(&[0xa1, 0x18, 0, 0]), ErrorKind::NonShortestHead);
    assert!(decode::<Ack>(&[0xa1, 0x18, 0, 0], &limits()).is_err());
}

#[test]
fn indefinite_floats_tags_reserved_and_trailing_named() {
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
    assert!(decode::<u64>(&[0, 0], &limits()).is_err());
}

/// P1: `f8` and a byte below 32 is not well-formed, RFC 8949 section 3.3.
#[test]
fn validator_rejects_nonminimal_simple_values() {
    for n in 0..=23 {
        let b = [0xf8, n];
        assert_eq!(kind(&b), ErrorKind::ReservedSimpleValue(n), "accepted {b:02x?}");
    }
}

/// P1: the reserved simple values 24 to 31 after `f8` are not well-formed either.
#[test]
fn validator_rejects_reserved_simple_values() {
    for n in 24..=31 {
        let b = [0xf8, n];
        assert_eq!(kind(&b), ErrorKind::ReservedSimpleValue(n), "accepted {b:02x?}");
    }
}

#[test]
fn deep_input_is_bounded_without_stack_overflow() {
    let mut b = vec![0x81; 100_000];
    b.push(0);
    assert_eq!(kind(&b), ErrorKind::TooDeep);
}

#[derive(Debug)]
enum Node {
    Array(Vec<Node>),
    Leaf(u64),
}

impl Wire for Node {
    fn encode(&self, e: &mut Encoder) {
        match self {
            Self::Array(a) => a.encode(e),
            Self::Leaf(n) => n.encode(e),
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        if r.position() == 32 {
            return u64::decode(r).map(Self::Leaf);
        }
        Vec::<Node>::decode(r).map(Self::Array)
    }
}

/// P1: one depth definition. 32 arrays plus a scalar is 33 items deep, which the validator
/// refuses, and the decoder must refuse it too.
#[test]
fn decode_never_accepts_validator_rejected_depth() {
    let mut b = vec![0x81; 32];
    b.push(0);
    assert_eq!(kind(&b), ErrorKind::TooDeep);
    assert!(
        decode::<Node>(&b, &limits()).is_err(),
        "decode accepted 32 arrays plus scalar, rejected by validator"
    );
}

/// P1: the ordinary decode path holds the message to the request limit.
#[test]
fn decode_enforces_whole_request_size() {
    let l = Limits { request: 0, ..limits() };
    assert!(validate(&[0], &l).is_err());
    assert!(decode::<u64>(&[0], &l).is_err(), "decode ignored request limit");
}

/// P1: a real 16 MiB blob plus its head is past the default request limit, and decode says so.
#[test]
fn ordinary_decoder_enforces_real_documented_16mib_request_limit() {
    let b = encode(&Bytes::new(vec![0; limits().request as usize]));
    assert!(matches!(
        validate(&b, &limits()).unwrap_err().kind,
        ErrorKind::OversizeItem { what: "message", .. }
    ));
    assert!(
        decode::<Bytes>(&b, &limits()).is_err(),
        "default decoder accepts 16 MiB blob plus 5-byte CBOR head, over request limit"
    );
}

/// The digest of the `n`th of a lookup's items: 32 bytes with `n` big-endian in the first eight.
fn nth_digest(n: u64) -> Digest {
    let mut d = [0; 32];
    d[..8].copy_from_slice(&n.to_be_bytes());
    d
}

/// P2: a lookup's collection is 4,096 items and one more is refused, at both edges.
#[test]
fn schema_lookup_limit_enforced() {
    let at_limit = CacheMissingRequest { blobs: Set::new((0..4096u64).map(nth_digest).collect()) };
    let b = encode(&at_limit);
    assert_eq!(
        validate_as::<CacheMissingRequest>(&b, &limits()).unwrap().blobs.len(),
        4096,
        "exactly the lookup limit of 4096 items was refused"
    );
    let over = CacheMissingRequest { blobs: Set::new((0..4097u64).map(nth_digest).collect()) };
    let b = encode(&over);
    let error = validate_as::<CacheMissingRequest>(&b, &limits()).unwrap_err();
    assert_eq!(error.offence(), "oversize item", "accepted 4097 lookup items (limit 4096)");
    assert!(
        matches!(error.kind, ErrorKind::OversizeItem { what: "array", len: 4097, limit: 4096 }),
        "{error:?}"
    );
    // The answer's per-key list and the crate lookup's keys are the same limit.
    let answer = CacheMissingAnswer { missing: Set::new((0..4097u64).map(nth_digest).collect()) };
    assert!(validate_as::<CacheMissingAnswer>(&encode(&answer), &limits()).is_err());
    let keys: Vec<CrateKey> = (0..4097u64).map(|n| CrateKey(nth_digest(n))).collect();
    let lookup = CacheLookupRequest { namespaces: vec![Namespace::Main], keys: Set::new(keys) };
    assert!(validate_as::<CacheLookupRequest>(&encode(&lookup), &limits()).is_err());
    let keys: Vec<CrateKey> = (0..4096u64).map(|n| CrateKey(nth_digest(n))).collect();
    let lookup = CacheLookupRequest { namespaces: vec![Namespace::Main], keys: Set::new(keys) };
    assert!(validate_as::<CacheLookupRequest>(&encode(&lookup), &limits()).is_ok());
    let bases: Vec<TestBaseKey> = (0..4097u64).map(|n| TestBaseKey(nth_digest(n))).collect();
    let lookup = ResultsLookupRequest { namespaces: vec![Namespace::Main], keys: Set::new(bases) };
    assert!(validate_as::<ResultsLookupRequest>(&encode(&lookup), &limits()).is_err());
}

/// P2: the lookup limit is a lookup's, not every array's: a batch's collection keeps 65,536.
#[test]
fn a_batch_keeps_the_batch_limit_not_the_lookup_limit() {
    let roots: Vec<(u64, u64)> = (0..4097u64).map(|n| (n, 0)).collect();
    let grant = Grant {
        lease: Lease {
            execution: ExecutionId(1),
            attempt: Attempt(1),
            holder: "executor".into(),
            generation: 1,
        },
        roots: Set::new(roots),
        names: Set::new(vec![]),
    };
    assert!(
        validate_as::<Grant>(&encode(&grant), &limits()).is_ok(),
        "a batch's set of 4097 was narrowed to the lookup limit"
    );
    // And a batch's array is still held to its own limit: 65,536 is accepted, one more is not.
    let roots: Vec<(u64, u64)> = (0..65537u64).map(|n| (n, 0)).collect();
    let grant = Grant { roots: Set::new(roots), ..grant };
    assert!(validate_as::<Grant>(&encode(&grant), &limits()).is_err());
}

/// P1: the public digest input is the hash preimage, and it is not empty.
#[test]
fn digest_input_matches_documented_formula() {
    let v = Attempt(24);
    let mut expected = b"blaze/Attempt/v2\0".to_vec();
    expected.extend(encode(&v));
    assert_eq!(digest_of(&v), <[u8; 32]>::from(Sha256::digest(&expected)));
    assert_eq!(digest_input(&v), expected, "public digest_input must return actual hash preimage");
}

/// P1: `digest_input` is the encoding the golden digest is taken over, for every type.
#[test]
fn every_digest_input_is_the_encoding_the_golden_digest_uses() {
    // Every type's golden digest is its name's domain over its CBOR, and no type's CBOR is empty,
    // so no digest input is the bare prefix.
    for entry in every() {
        let vector = (entry.vector)(1);
        assert!(!vector.cbor.is_empty(), "{}: the vector's CBOR is empty", entry.name);
        assert_eq!(
            vector.digest,
            digest_of_encoded_name(entry.name, &vector.cbor),
            "{}: the golden digest is not its CBOR's",
            entry.name
        );
    }
    // The public function's output for a type whose value is written here: non-empty, and the
    // preimage of `digest_of`.
    let v = Attempt(24);
    let input = digest_input(&v);
    assert!(!input.is_empty(), "digest_input returned no bytes");
    assert_eq!(<[u8; 32]>::from(Sha256::digest(&input)), digest_of(&v));
    // It is exactly the bytes the golden digest is taken over for the same value.
    let mut expected = b"blaze/Attempt/v2\0".to_vec();
    expected.extend(encode(&v));
    assert_eq!(input, expected);
    assert_eq!(<[u8; 32]>::from(Sha256::digest(&expected)), digest_of(&v));
}

/// P1: the `f8` offence is named, not just refused.
#[test]
fn the_reserved_simple_value_offence_is_named() {
    for n in [0u8, 20, 23, 24, 31] {
        let error = validate(&[0xf8, n], &limits()).unwrap_err();
        assert_eq!(error.offence(), "reserved simple value", "f8 {n:02x}");
        assert_eq!(error.kind, ErrorKind::ReservedSimpleValue(n));
        // The same bytes inside a map key are refused the same way.
        let error = validate(&[0xa1, 0xf8, n, 0], &limits()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::ReservedSimpleValue(n));
    }
}

/// P1: `Encoder::new_into` writes onto the caller's buffer instead of taking and dropping it.
#[test]
fn new_into_really_appends_to_caller_output() {
    let mut b = vec![42];
    {
        let mut e = Encoder::new_into(&mut b);
        e.uint(1);
    }
    assert_eq!(b, [42, 1]);
}

#[test]
fn all_domains_are_unique_and_prefix_free() {
    let entries = every();
    assert_eq!(entries.len(), 100);
    let mut names = BTreeSet::new();
    let mut domains = vec![];
    for e in entries {
        assert!(names.insert(e.name));
        assert!(!e.name.contains('\0'));
        domains.push(format!("blaze/{}/v2\0", e.name).into_bytes());
        domains.push(format!("blaze/sig/{}/v2\0", e.name).into_bytes());
    }
    domains.push(b"blaze/req/v1\0".to_vec());
    for (i, a) in domains.iter().enumerate() {
        for (j, b) in domains.iter().enumerate() {
            if i != j {
                assert!(!a.starts_with(b));
            }
        }
    }
}

#[test]
fn generic_signature_covers_documented_domain_cbor_not_digest() {
    let k = SigningKey::from_bytes(&[7; 32]);
    let v = Attempt(24);
    let sig = sign(&k, &v);
    let mut input = b"blaze/sig/Attempt/v2\0".to_vec();
    input.extend(encode(&v));
    assert_eq!(signature_input(&v), input);
    assert_eq!(sig, k.sign(&input).to_bytes());
    assert!(k.verifying_key().verify_strict(&input, &Signature::from_bytes(&sig)).is_ok());
    assert!(k.verifying_key().verify_strict(&digest_of(&v), &Signature::from_bytes(&sig)).is_err());
}

fn record() -> CrateRecord {
    CrateRecord {
        namespace: Namespace::Main,
        epoch: 1,
        crate_key: CrateKey([0; 32]),
        generation: 1,
        packs: Set::new(vec![]),
        source: SourceRef::Root(RootRef { root: [0; 32] }),
        signature: [0; 64],
    }
}

fn record_fields_0_to_5(r: &CrateRecord) -> Vec<u8> {
    let mut e = Encoder::new();
    e.map(6);
    e.uint(0);
    r.namespace.encode(&mut e);
    e.uint(1);
    r.epoch.encode(&mut e);
    e.uint(2);
    r.crate_key.encode(&mut e);
    e.uint(3);
    r.generation.encode(&mut e);
    e.uint(4);
    r.packs.encode(&mut e);
    e.uint(5);
    r.source.encode(&mut e);
    e.into_bytes()
}

/// P1: the signed bytes are the documented six-field map, not seven fields with field 6 zeroed.
#[test]
fn record_signs_only_fields_0_to_5() {
    let r = record();
    assert_eq!(r.signed_bytes(), record_fields_0_to_5(&r));
}

/// P1: the record's signature is over the six fields under `blaze/sig/CrateRecord/v2`.
#[test]
fn record_signature_uses_documented_domain_separation() {
    let r = record();
    let k = SigningKey::from_bytes(&[7; 32]);
    let mut preimage = b"blaze/sig/CrateRecord/v2\0".to_vec();
    preimage.extend(record_fields_0_to_5(&r));
    let sig = k.sign(&preimage).to_bytes();
    assert!(r.signature_is(&k.verifying_key(), &sig), "valid documented signature rejected");
}

/// P1: a signature over raw CBOR, without the domain, is not the record's signature.
#[test]
fn record_rejects_raw_undomained_signature() {
    let r = record();
    let k = SigningKey::from_bytes(&[7; 32]);
    let sig = k.sign(&r.signed_bytes()).to_bytes();
    assert!(
        !r.signature_is(&k.verifying_key(), &sig),
        "raw CBOR signature accepted without domain"
    );
}

/// P2: the doc and the code agree that `Pr` is the payload map `[1, {0: n}]`.
///
/// The doc's own payload list at section 8 defines `Namespace::Pr { n: u64 }` as a map, so the
/// prose's `[1, n]` was the stale form; the code and the golden vectors are the map's.
#[test]
fn namespace_pr_agrees_with_doc_payload_map() {
    assert_eq!(
        encode(&Namespace::Pr(PrNamespace { n: 1 })),
        [0x82, 1, 0xa1, 0x00, 0x01],
        "a Pr payload is the map [1, {{0: n}}], as the doc's payload list says"
    );
}

/// Lower priority: base64url text has one form per byte string, so a signature header has no
/// alias that verifies.
#[test]
fn base64url_rejects_nonzero_pad_bits() {
    assert_eq!(base64url_decode("AA"), Some(vec![0]));
    assert!(base64url_decode("AB").is_none(), "nonzero pad bits accepted");
}

/// Lower priority: -2^64 is a well-formed CBOR item and renders without overflow.
#[test]
fn accepted_negative_u64_max_json_does_not_overflow() {
    let b = head(1, u64::MAX);
    validate(&b, &limits()).unwrap();
    let mut out = String::new();
    json::render_cbor(&b, &mut out);
    assert_eq!(out, "-18446744073709551616");
}

#[test]
fn unknown_schema_field_and_missing_field_named() {
    assert!(matches!(
        validate_as::<Ack>(&[0xa1, 0, 0], &limits()).unwrap_err().kind,
        ErrorKind::UnknownKey(0)
    ));
    assert!(matches!(
        validate_as::<AuthRequest>(&[0xa0], &limits()).unwrap_err().kind,
        ErrorKind::MissingKey(0)
    ));
}

#[test]
fn accepted_schema_input_reencodes_identically_via_validate_as() {
    for e in every() {
        for seed in 1..=8 {
            let b = (e.vector)(seed).cbor;
            validate(&b, &limits()).unwrap();
            assert_eq!((e.reencode)(&b).unwrap(), b);
        }
    }
}

// Built-in nested Vec type, without a custom decoder, confirms depth mismatch.
type Nest0 = u64;
type Nest1 = Vec<Nest0>;
type Nest2 = Vec<Nest1>;
type Nest3 = Vec<Nest2>;
type Nest4 = Vec<Nest3>;
type Nest5 = Vec<Nest4>;
type Nest6 = Vec<Nest5>;
type Nest7 = Vec<Nest6>;
type Nest8 = Vec<Nest7>;
type Nest9 = Vec<Nest8>;
type Nest10 = Vec<Nest9>;
type Nest11 = Vec<Nest10>;
type Nest12 = Vec<Nest11>;
type Nest13 = Vec<Nest12>;
type Nest14 = Vec<Nest13>;
type Nest15 = Vec<Nest14>;
type Nest16 = Vec<Nest15>;
type Nest17 = Vec<Nest16>;
type Nest18 = Vec<Nest17>;
type Nest19 = Vec<Nest18>;
type Nest20 = Vec<Nest19>;
type Nest21 = Vec<Nest20>;
type Nest22 = Vec<Nest21>;
type Nest23 = Vec<Nest22>;
type Nest24 = Vec<Nest23>;
type Nest25 = Vec<Nest24>;
type Nest26 = Vec<Nest25>;
type Nest27 = Vec<Nest26>;
type Nest28 = Vec<Nest27>;
type Nest29 = Vec<Nest28>;
type Nest30 = Vec<Nest29>;
type Nest31 = Vec<Nest30>;
type Nest32 = Vec<Nest31>;

#[test]
fn builtin_vec_decoder_never_accepts_validator_rejected_depth() {
    let mut b = vec![0x81; 32];
    b.push(0);
    assert_eq!(kind(&b), ErrorKind::TooDeep);
    assert!(
        decode::<Nest32>(&b, &limits()).is_err(),
        "built-in Vec<...<u64>> decoder accepts depth 33"
    );
}

fn ordinary_audit<T: Wire + Named + Sample>(seed: u64) {
    let mut g = Gen::new(seed);
    let original = encode(&T::sample(&mut g));
    assert!(validate(&original, &limits()).is_ok());
    let back = decode::<T>(&original, &limits()).unwrap();
    assert_eq!(encode(&back), original);
    for i in 0..original.len().min(300) {
        for mask in [1, 0x80, 0xff] {
            let mut bytes = original.clone();
            bytes[i] ^= mask;
            if let Ok(value) = decode::<T>(&bytes, &limits()) {
                assert!(
                    validate(&bytes, &limits()).is_ok(),
                    "{} decoded validator-rejected bytes {bytes:02x?}",
                    T::NAME
                );
                assert_eq!(encode(&value), bytes, "{} accepted nonidentity", T::NAME);
            }
        }
    }
}

#[test]
fn ordinary_decoder_schema_identity_and_mutations_all_98_types() {
    ordinary_audit::<ExecPlatform>(1);
    ordinary_audit::<SourceRef>(1);
    ordinary_audit::<GitRef>(1);
    ordinary_audit::<RootRef>(1);
    ordinary_audit::<BuildSpec>(1);
    ordinary_audit::<UnitSpec>(1);
    ordinary_audit::<ProfileSpec>(1);
    ordinary_audit::<LocalPolicy>(1);
    ordinary_audit::<Command>(1);
    ordinary_audit::<RunSpec>(1);
    ordinary_audit::<Namespace>(1);
    ordinary_audit::<PrNamespace>(1);
    ordinary_audit::<CrateRecord>(1);
    ordinary_audit::<PackRef>(1);
    ordinary_audit::<RootManifest>(1);
    ordinary_audit::<Capability>(1);
    ordinary_audit::<WorkKey>(1);
    ordinary_audit::<ItemKey>(1);
    ordinary_audit::<EdgeKey>(1);
    ordinary_audit::<Found>(1);
    ordinary_audit::<RootSet>(1);
    ordinary_audit::<Lease>(1);
    ordinary_audit::<Grant>(1);
    ordinary_audit::<ItemOutcome>(1);
    ordinary_audit::<EntryRef>(1);
    ordinary_audit::<Why>(1);
    ordinary_audit::<ItemResult>(1);
    ordinary_audit::<BatchResult>(1);
    ordinary_audit::<InputManifest>(1);
    ordinary_audit::<Observation>(1);
    ordinary_audit::<FileObservation>(1);
    ordinary_audit::<DirObservation>(1);
    ordinary_audit::<AbsentObservation>(1);
    ordinary_audit::<LinkObservation>(1);
    ordinary_audit::<TestVerdict>(1);
    ordinary_audit::<RuntimeBundle>(1);
    ordinary_audit::<Reservation>(1);
    ordinary_audit::<Stage>(1);
    ordinary_audit::<JobEvent>(1);
    ordinary_audit::<StageEvent>(1);
    ordinary_audit::<AssignedEvent>(1);
    ordinary_audit::<PublishedEvent>(1);
    ordinary_audit::<ProgramEvent>(1);
    ordinary_audit::<RefusedEvent>(1);
    ordinary_audit::<OutputEvent>(1);
    ordinary_audit::<DoneEvent>(1);
    ordinary_audit::<Outcome>(1);
    ordinary_audit::<ExecutorState>(1);
    ordinary_audit::<StoreIdentity>(1);
    ordinary_audit::<EntryKey>(1);
    ordinary_audit::<CrateKey>(1);
    ordinary_audit::<CrateKeyInput>(1);
    ordinary_audit::<FrontEndKey>(1);
    ordinary_audit::<FrontEndUnit>(1);
    ordinary_audit::<FrontEndKeyInput>(1);
    ordinary_audit::<ConfigKey>(1);
    ordinary_audit::<ConfigKeyInput>(1);
    ordinary_audit::<JobContent>(1);
    ordinary_audit::<JobContentInput>(1);
    ordinary_audit::<TestBaseKey>(1);
    ordinary_audit::<TestBaseKeyInput>(1);
    ordinary_audit::<TestKey>(1);
    ordinary_audit::<WorkName>(1);
    ordinary_audit::<ExecutionId>(1);
    ordinary_audit::<Attempt>(1);
    ordinary_audit::<HelloRequest>(1);
    ordinary_audit::<HelloAnswer>(1);
    ordinary_audit::<Limits>(1);
    ordinary_audit::<AuthRequest>(1);
    ordinary_audit::<SignedCapability>(1);
    ordinary_audit::<CacheLookupRequest>(1);
    ordinary_audit::<CacheLookupAnswer>(1);
    ordinary_audit::<CacheMissingRequest>(1);
    ordinary_audit::<CacheMissingAnswer>(1);
    ordinary_audit::<CachePublishRequest>(1);
    ordinary_audit::<CachePublishAnswer>(1);
    ordinary_audit::<ResultsLookupRequest>(1);
    ordinary_audit::<ResultsLookupAnswer>(1);
    ordinary_audit::<TestPass>(1);
    ordinary_audit::<ResultsPublishRequest>(1);
    ordinary_audit::<ResultsPublishAnswer>(1);
    ordinary_audit::<Ack>(1);
    ordinary_audit::<JobRequest>(1);
    ordinary_audit::<JobAnswer>(1);
    ordinary_audit::<EventItem>(1);
    ordinary_audit::<WorkersHelloRequest>(1);
    ordinary_audit::<WorkersHelloAnswer>(1);
    ordinary_audit::<WorkersNextRequest>(1);
    ordinary_audit::<WorkersNextAnswer>(1);
    ordinary_audit::<Assignment>(1);
    ordinary_audit::<WorkJoinRequest>(1);
    ordinary_audit::<WorkJoinAnswer>(1);
    ordinary_audit::<WorkLeaseRequest>(1);
    ordinary_audit::<WorkLeaseAnswer>(1);
    ordinary_audit::<WorkResultAnswer>(1);
    ordinary_audit::<WorkersEventsRequest>(1);
    ordinary_audit::<WorkersEventsAnswer>(1);
    ordinary_audit::<WireError>(1);
}

#[test]
fn full_array_and_map_lengths_at_65535_65536() {
    for n in [23, 24, 255, 256, 65535, 65536] {
        let mut e = Encoder::new();
        e.array(n);
        for _ in 0..n {
            e.uint(0);
        }
        let b = e.into_bytes();
        validate(&b, &limits()).unwrap();
        identity::<Vec<u64>>(&b, &limits());
        let mut e = Encoder::new();
        e.map(n);
        for i in 0..n {
            e.uint(i as u64);
            e.uint(0);
        }
        validate(&e.into_bytes(), &limits()).unwrap();
    }
}

#[test]
fn simple_value_sweep_reports_all_illegal_encodings() {
    let wrongly_accepted: Vec<u8> =
        (0..32).filter(|n| validate(&[0xf8, *n], &limits()).is_ok()).collect();
    assert!(
        wrongly_accepted.is_empty(),
        "validator wrongly accepts all of {wrongly_accepted:?} after f8"
    );
    for n in 32..=255 {
        validate(&[0xf8, n], &limits()).unwrap();
    }
}

/// Lower priority: a signature header's text is canonical, so a string-keyed replay check cannot
/// be fooled by an alias of the same bytes.
#[test]
fn agent_signature_header_is_not_malleable() {
    let k = SigningKey::from_bytes(&[7; 32]);
    let r = credentials::agent_request();
    let sig = r.sign(&k);
    assert!(r.verify(&k.verifying_key(), &sig));
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut alias = sig.clone().into_bytes();
    let last = alias.last_mut().unwrap();
    let index = alphabet.iter().position(|c| c == last).unwrap();
    assert_eq!(index & 15, 0);
    *last = alphabet[index + 1];
    let alias = String::from_utf8(alias).unwrap();
    assert_ne!(sig, alias);
    assert!(
        !r.verify(&k.verifying_key(), &alias),
        "noncanonical signature string verifies; string-keyed replay detection would be unsafe"
    );
}

/// Lower priority: an enum renders as the wire's two-element array, which `json.rs` documents.
#[test]
fn json_enum_rendering_agrees_with_public_docs() {
    assert_eq!(
        to_json(&Namespace::Main),
        r#"{"Namespace":[0,null]}"#,
        "json.rs documents the wire's [discriminant, payload] array"
    );
}

#[test]
fn illegal_simple_map_key_is_rejected_and_named() {
    let b = [0xa1, 0xf8, 0x14, 0];
    assert!(validate(&b, &limits()).is_err(), "nonminimal false map key accepted");
}
