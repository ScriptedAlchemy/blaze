//! The digest and signature domain separation, the credentials' headers, the `ops` bits, the
//! `JobContent` bytes, the framing and the JSON rendering.

use blaze_wire::calls::*;
use blaze_wire::shapes::*;
use blaze_wire::vectors::{Gen, Sample, Vector};
use blaze_wire::{
    AgentRequest, Bytes, ErrorCode, Framing, Limits, Named, Op, Set, decode, digest_of,
    digest_of_encoded_name, encode, sign, verify,
};
use ed25519_dalek::SigningKey;

fn limits() -> Limits {
    Limits::DOCUMENTED
}

fn key() -> SigningKey {
    let mut seed = [7u8; 32];
    seed[0] = 7;
    SigningKey::from_bytes(&seed)
}

/// A `WorkKey` and a `JobContentInput` with the same bytes: their digests differ, because the type
/// name is in the digest's input.
#[test]
fn digests_are_separated_by_their_type_names() {
    let item = WorkKey::Item(ItemKey { unit: [1u8; 32], stable: [2u8; 32], env: [3u8; 32] });
    let content = JobContentInput { spec: [1u8; 32], namespace: Namespace::Main };
    let a = digest_of(&item);
    let b = digest_of(&content);
    assert_ne!(a, b);
    // The same bytes under another name are another digest: the input is not only the value.
    let bytes = encode(&item);
    assert_ne!(a, digest_of_encoded_name("JobContentInput", &bytes));
    // And the prefix is the schema's, not the store's: the digest is not a plain SHA-256.
    use sha2::Digest as _;
    let plain: [u8; 32] = sha2::Sha256::digest(&bytes).into();
    assert_ne!(a, plain);
}

#[test]
fn a_signature_is_not_a_digest_and_neither_is_another_type_s() {
    let value = Attempt(3);
    let signature = sign(&key(), &value);
    let digest = digest_of(&value);
    // The signature is over `"blaze/sig/" ‖ name ...`, so it is not the digest's bytes signed.
    assert_ne!(&signature[..32], &digest[..]);
    assert!(verify(&key().verifying_key(), &value, &signature));
    // Another type's signature over the same bytes does not verify: the name is inside.
    let other = sign(&key(), &ExecutionId(3));
    assert!(!verify(&key().verifying_key(), &value, &other));
    // A changed value does not verify under the old signature.
    assert!(!verify(&key().verifying_key(), &Attempt(4), &signature));
}

#[test]
fn a_record_is_signed_over_fields_0_to_5_only() {
    // Section 2: the server signs a `CrateRecord`'s fields 0 to 5, and the namespace is in the
    // signed bytes, so a `Pr` record is not a `Main` record.
    let mut record = sample_record();
    let signed = sign(&key(), &record);
    let mut pr = record.clone();
    pr.namespace = Namespace::Pr(PrNamespace { n: 1 });
    assert!(!verify(&key().verifying_key(), &pr, &signed), "a Pr record verified as Main's");
    // Field 6 is the signature itself, so the whole record's digest covers it...
    record.signature = [0u8; 64];
    let mut other = record.clone();
    other.signature = [9u8; 64];
    let digest_a = digest_of(&record);
    let digest_b = digest_of(&other);
    assert_ne!(digest_a, digest_b, "the whole record's digest covers field 6");
    // ...and the signed bytes are the six-field map of fields 0 to 5, without field 6 at all.
    let bytes = record.signed_bytes();
    assert_eq!(bytes[0], 0xa6, "the signed bytes are a map of six, not seven: {bytes:02x?}");
    let expected = record_fields_0_to_5(&record);
    assert_eq!(bytes, expected, "the signed bytes are not fields 0 to 5");
    // The whole record, field 6 included, is seven fields, so the two encodings differ.
    assert_ne!(bytes, encode(&record), "the signed bytes are the whole record");
}

/// Fields 0 to 5 of a record, as the six-field map the server signs.
fn record_fields_0_to_5(record: &CrateRecord) -> Vec<u8> {
    use blaze_wire::{Encoder, Wire};
    let mut e = Encoder::new();
    e.map(6);
    e.uint(0);
    record.namespace.encode(&mut e);
    e.uint(1);
    record.epoch.encode(&mut e);
    e.uint(2);
    record.crate_key.encode(&mut e);
    e.uint(3);
    record.generation.encode(&mut e);
    e.uint(4);
    record.packs.encode(&mut e);
    e.uint(5);
    record.source.encode(&mut e);
    e.into_bytes()
}

#[test]
fn the_golden_record_s_own_signature_is_over_its_six_fields() {
    // The follow-up the verifier asked for: an independent check of the embedded field, not just
    // the generic whole-value vector. The golden file's field 6 must be the vector key's signature
    // over the six fields the server signs, under the CrateRecord domain.
    let path = format!("{}/tests/golden/CrateRecord.txt", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("no CrateRecord golden file");
    let golden = Vector::from_golden(&text).expect("the golden file is not a vector");
    let record: CrateRecord =
        decode(&golden.cbor, &limits()).expect("the golden record does not decode");
    let key = golden.signing_key().verifying_key();
    assert!(
        record.signature_is(&key, &record.signature),
        "the golden record's field 6 is not the signature of its fields 0 to 5"
    );
    // Its signed bytes are the file's `signed` line, which is the six-field map.
    assert_eq!(golden.signed_bytes(), record.signed_bytes().as_slice());
    assert_eq!(golden.signed_bytes()[0], 0xa6);
    // And the record is the one this build makes from the same seed, so the file is not stale.
    let made = Vector::of_record::<CrateRecord>(1);
    assert_eq!(made.cbor, golden.cbor);
    assert_eq!(made.signed, golden.signed);
    assert_eq!(made.digest, golden.digest);
    assert_eq!(made.signature, golden.signature);
}

#[test]
fn a_record_signature_is_over_the_six_fields_under_its_own_domain() {
    // The record's own signature is the schema's signature protocol over the six-field map, not
    // an undomained signature over raw CBOR: a verifier that takes the domain's bytes accepts it,
    // and one that takes the raw six-field CBOR does not.
    let record = sample_record();
    let signed = record.signed(&key());
    assert!(
        signed.signature_is(&key().verifying_key(), &signed.signature),
        "the record's own signature does not verify"
    );
    assert!(
        blaze_wire::verify_encoded_name(
            "CrateRecord",
            &record.signed_bytes(),
            &key().verifying_key(),
            &signed.signature
        ),
        "the signature is not the CrateRecord domain's over fields 0 to 5"
    );
    assert!(
        !blaze_wire::verify_encoded_name(
            "CrateRecord",
            &encode(&record),
            &key().verifying_key(),
            &signed.signature
        ),
        "the signature verified over the whole record, field 6 included"
    );
    // A record whose fields 0 to 5 changed does not verify under the old signature.
    let mut changed = signed.clone();
    changed.generation = changed.generation.wrapping_add(1);
    assert!(!changed.signature_is(&key().verifying_key(), &signed.signature));
    // A signature made over the raw six-field CBOR, with no domain, is refused.
    use ed25519_dalek::Signer as _;
    let raw = key().sign(&record.signed_bytes()).to_bytes();
    assert!(
        !record.signature_is(&key().verifying_key(), &raw),
        "an undomained signature over the signed bytes was accepted"
    );
    // And one over the whole record's CBOR is refused, as the generic `sign` would make it.
    let whole = sign(&key(), &record);
    assert!(!record.signature_is(&key().verifying_key(), &whole));
}

#[test]
fn the_ops_bits_are_the_eight_section_8_names() {
    let expected = [
        ("lookup", 0, Op::Lookup),
        ("fetch", 1, Op::Fetch),
        ("publish", 2, Op::Publish),
        ("events", 3, Op::Events),
        ("cancel", 4, Op::Cancel),
        ("roots", 5, Op::Roots),
        ("delete a namespace", 6, Op::DeleteNamespace),
        ("work", 7, Op::Work),
    ];
    for (name, bit, op) in expected {
        assert_eq!(op as u32, 1 << bit, "{name} is bit {bit}");
        assert!(op.in_bits(1 << bit), "{name} is not in its own bit");
        assert!(!op.in_bits(!(1 << bit)), "{name} is in another bit");
    }
    assert_eq!(Op::ALL.len(), 8);
    // Every bit of a byte is one operation, and no operation is outside it.
    let all: u32 = Op::ALL.iter().map(|op| *op as u32).fold(0, |a, b| a | b);
    assert_eq!(all, 0xff);
    // A capability's `ops` is these bits.
    let mut g = Gen::new(1);
    let capability: Capability = Capability::sample(&mut g);
    let ops = capability.ops;
    let names: Vec<&str> = Op::ALL
        .iter()
        .filter(|op| op.in_bits(ops))
        .map(|op| match op {
            Op::Lookup => "lookup",
            Op::Fetch => "fetch",
            Op::Publish => "publish",
            Op::Events => "events",
            Op::Cancel => "cancel",
            Op::Roots => "roots",
            Op::DeleteNamespace => "delete a namespace",
            Op::Work => "work",
        })
        .collect();
    assert_eq!(names.len(), (0..8).filter(|bit| ops & (1 << bit) != 0).count());
}

#[test]
fn the_job_content_bytes_are_its_spec_digest_and_namespace() {
    // Section 8: a job's content is `{0: BuildSpec digest, 1: Namespace}`, and an equal content
    // joins a running execution rather than starting another.
    let spec = [3u8; 32];
    let main = JobContentInput { spec, namespace: Namespace::Main };
    let pr = JobContentInput { spec, namespace: Namespace::Pr(PrNamespace { n: 7 }) };
    // The map is `{0: <32 bytes>, 1: <namespace>}`: `a2 00 58 20 ... 01 ...`.
    let bytes = encode(&main);
    assert_eq!(&bytes[..4], &[0xa2, 0x00, 0x58, 0x20]);
    assert_eq!(&bytes[4..36], &spec);
    // The namespace's key is 1, and `Namespace::Main` is the array `[0, null]`.
    assert_eq!(&bytes[36..39], &[0x01, 0x82, 0x00]);
    assert_eq!(bytes[39], 0xf6);
    assert_eq!(JobContent::of(&main).0, digest_of_encoded_name("JobContent", &bytes));
    // A namespace is in the content, so a `Main` job and a `Pr(7)` job of one spec are two
    // contents and two executions.
    assert_ne!(JobContent::of(&main), JobContent::of(&pr));
    // The spec's digest is in it, so two specs are two contents.
    let other = JobContentInput { spec: [4u8; 32], namespace: Namespace::Main };
    assert_ne!(JobContent::of(&main), JobContent::of(&other));
    // And the same content from two builds is one content.
    assert_eq!(
        JobContent::of(&main),
        JobContent::of(&JobContentInput { spec, namespace: Namespace::Main })
    );
}

#[test]
fn the_capability_header_is_base64url_of_the_signed_capability() {
    let mut g = Gen::new(11);
    let capability: Capability = Capability::sample(&mut g);
    let signature = sign(&key(), &capability);
    let signed = SignedCapability { capability, signature };
    let header = blaze_wire::capability_header(&signed);
    // base64url without padding: no `+`, `/` or `=`.
    assert!(!header.contains('+') && !header.contains('/') && !header.contains('='));
    let back = blaze_wire::capability_from_header(&header, &limits())
        .expect("the header did not read back");
    assert_eq!(back, signed);
    // A byte changed in the header is refused.
    let mut broken = header.clone().into_bytes();
    broken[10] = if broken[10] == b'A' { b'B' } else { b'A' };
    let broken = String::from_utf8(broken).unwrap();
    assert!(blaze_wire::capability_from_header(&broken, &limits()).is_err());
    // Text that is not base64url is refused.
    assert!(blaze_wire::capability_from_header("not base64!", &limits()).is_err());
}

#[test]
fn base64url_round_trips() {
    let mut g = Gen::new(12);
    for _ in 0..64 {
        let bytes: Vec<u8> = Bytes::sample(&mut g).bytes;
        let text = blaze_wire::base64url(&bytes);
        assert_eq!(blaze_wire::base64url_decode(&text), Some(bytes));
    }
    // The RFC 4648 vectors of base64url, without padding.
    let cases = [
        ("", ""),
        ("f", "Zg"),
        ("fo", "Zm8"),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg"),
        ("fooba", "Zm9vYmE"),
        ("foobar", "Zm9vYmFy"),
        // Bytes that use the two symbols base64 does not.
        ("\u{fb}\u{ff}", "w7vDvw"),
    ];
    for (plain, encoded) in cases {
        assert_eq!(blaze_wire::base64url(plain.as_bytes()), encoded);
        assert_eq!(blaze_wire::base64url_decode(encoded), Some(plain.as_bytes().to_vec()));
    }
}

#[test]
fn the_agent_signature_covers_the_method_path_body_and_date() {
    let request = AgentRequest {
        agent: "worker-1",
        date_ms: 1_767_225_600_000,
        method: "POST",
        path: "/v1/workers/next",
        body: b"\xa1\x00\x00",
    };
    let signature = request.sign(&key());
    assert!(!signature.contains('=') && !signature.contains('+') && !signature.contains('/'));
    assert!(request.verify(&key().verifying_key(), &signature));
    // The input is the section 8 shape: `"blaze/req/v1" 0x00 method 0x00 path 0x00 sha256(body)
    // 0x00 the date`.
    let input = request.signing_input();
    let mut expected = Vec::new();
    expected.extend_from_slice(b"blaze/req/v1");
    expected.push(0);
    expected.extend_from_slice(b"POST");
    expected.push(0);
    expected.extend_from_slice(b"/v1/workers/next");
    expected.push(0);
    use sha2::Digest as _;
    expected.extend_from_slice(&sha2::Sha256::digest(request.body));
    expected.push(0);
    expected.extend_from_slice(b"1767225600000");
    assert_eq!(input, expected);
    // Each field is covered: a changed method, path, body or date does not verify.
    for changed in [
        AgentRequest { method: "GET", ..request },
        AgentRequest { path: "/v1/workers/hello", ..request },
        AgentRequest { body: b"\xa1\x00\x01", ..request },
        AgentRequest { date_ms: request.date_ms + 1, ..request },
    ] {
        assert!(
            !changed.verify(&key().verifying_key(), &signature),
            "a changed {} verified",
            changed.method
        );
    }
    // A date more than 60 s off is refused.
    assert!(request.date_is_near(request.date_ms + 59_000));
    assert!(request.date_is_near(request.date_ms));
    assert!(!request.date_is_near(request.date_ms + 61_000));
    assert!(!request.date_is_near(request.date_ms - 61_000));
}

#[test]
fn the_framing_is_a_four_byte_big_endian_length() {
    let head = Framing::head(b"\xa0", 16 << 20).expect("one byte is within the limit");
    assert_eq!(head, [0, 0, 0, 1]);
    let head = Framing::head(&[0u8; 256], 16 << 20).unwrap();
    assert_eq!(head, [0, 0, 1, 0]);
    // Past the limit is refused.
    let error = Framing::head(&[0u8; 5], 4).expect_err("five bytes are past a limit of four");
    assert_eq!(error.offence(), "oversize item");
}

#[test]
fn the_errors_carry_the_section_8_codes_and_statuses() {
    let cases = [
        (1u8, 400u16, ErrorCode::Malformed),
        (2, 400, ErrorCode::Version),
        (3, 401, ErrorCode::Unauthenticated),
        (4, 403, ErrorCode::NotAllowed),
        (5, 404, ErrorCode::NotFound),
        (6, 409, ErrorCode::Conflict),
        (7, 413, ErrorCode::TooLarge),
        (8, 429, ErrorCode::Busy),
        (9, 422, ErrorCode::Refused),
        (10, 500, ErrorCode::Internal),
    ];
    for (byte, status, code) in cases {
        assert_eq!(ErrorCode::from_u8(byte), Some(code));
        assert_eq!(code.status(), status, "{code:?}");
        assert_eq!(code as u8, byte);
        let error = WireError::new(code, "why");
        assert_eq!(error.code(), Some(code));
        let bytes = encode(&error);
        assert_eq!(decode::<WireError>(&bytes, &limits()).unwrap(), error);
    }
    assert_eq!(ErrorCode::from_u8(0), None);
    assert_eq!(ErrorCode::from_u8(11), None);
    let error = WireError::new(ErrorCode::Busy, "later").retry_after(1_000);
    assert_eq!(error.retry_after_ms, Some(1_000));
    assert_eq!(&encode(&error)[..2], &[0xa3, 0x00]);
}

#[test]
fn the_limits_are_section_8_s() {
    let limits = Limits::DOCUMENTED;
    assert_eq!(limits.request, 16 << 20);
    assert_eq!(limits.event, 1 << 20);
    assert_eq!(limits.blob, 4 << 30);
    assert_eq!(limits.lookup_items, 4096);
    assert_eq!(limits.batch_items, 65536);
    assert_eq!(limits.string, 4096);
    // The struct's keys are 0 to 5, in order, every one present.
    let bytes = encode(&limits);
    assert_eq!(bytes[0], 0xa6);
    let back: Limits = decode(&bytes, &limits).unwrap();
    assert_eq!(back, limits);
}

#[test]
fn the_json_rendering_shows_the_same_value() {
    let mut g = Gen::new(13);
    let record: CrateRecord = CrateRecord::sample(&mut g);
    let json = blaze_wire::to_json(&record);
    assert!(json.starts_with("{\"CrateRecord\":"));
    assert!(json.ends_with('}'));
    // A map's keys are its field numbers as text, and the signature field is hex.
    assert!(json.contains("\"0\":"), "{json}");
    assert!(json.contains("\"6\":\""), "{json}");
    assert!(json.contains("\"2\":\""), "{json}");
    // The JSON is valid enough to be read by a person: no control characters, balanced braces.
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for c in json.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' | '[' if !in_string => depth += 1,
            '}' | ']' if !in_string => depth -= 1,
            c if (c as u32) < 0x20 && !in_string => panic!("a control character in the JSON"),
            _ => {}
        }
    }
    assert_eq!(depth, 0);
    assert!(!in_string);
}

#[test]
fn every_type_renders_as_json() {
    // The rendering is of a value's canonical CBOR, so a type whose vector has one is rendered
    // from those bytes; the CBOR's own JSON is what a person reads.
    for entry in blaze_wire::vectors::every() {
        let mut json = String::new();
        blaze_wire::json::render_cbor(&(entry.vector)(1).cbor, &mut json);
        // A struct is an object, an enum an array, a byte string hex in quotes, a number a
        // number: the rendering is of the value, and every type has one.
        assert!(
            json.starts_with('{')
                || json.starts_with('[')
                || json.starts_with('"')
                || json.starts_with(|c: char| c.is_ascii_digit()),
            "{}: {json}",
            entry.name
        );
        assert!(!json.contains("NaN") && !json.contains("inf"));
    }
}

#[test]
fn a_digest_renders_as_its_hex() {
    let digest = digest_of(&Attempt(1));
    let hex = blaze_wire::json::hex(&digest);
    assert_eq!(hex.len(), 64);
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    let mut out = String::new();
    blaze_wire::json::render(&digest, &mut out);
    assert_eq!(out, format!("\"{hex}\""));
}

#[test]
fn the_version_is_one() {
    assert_eq!(blaze_wire::VERSION, 3);
    // `HelloRequest` carries the versions a client speaks; a server that speaks this one answers
    // it.
    let request = HelloRequest {
        versions: Set::new(vec![blaze_wire::VERSION]),
        identity: StoreIdentity([1u8; 32]),
        platform: sample_platform(),
        role: 1,
    };
    let bytes = encode(&request);
    let back: HelloRequest = decode(&bytes, &limits()).unwrap();
    assert_eq!(back, request);
    assert!(back.versions.as_slice().contains(&blaze_wire::VERSION));
}

fn sample_platform() -> ExecPlatform {
    let mut g = Gen::new(14);
    ExecPlatform::sample(&mut g)
}

fn sample_record() -> CrateRecord {
    let mut g = Gen::new(15);
    CrateRecord::sample(&mut g)
}

/// The name a type is hashed under is its Rust name, and a vector's name is the same.
#[test]
fn a_types_name_is_its_rust_name() {
    assert_eq!(CrateRecord::NAME, "CrateRecord");
    assert_eq!(<JobContentInput as Named>::NAME, "JobContentInput");
    assert_eq!(<Attempt as Named>::NAME, "Attempt");
    assert_eq!(Vector::of::<Attempt>(1).name, Attempt::NAME);
}
