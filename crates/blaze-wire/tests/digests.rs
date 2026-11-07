//! The digest domain separation, the `JobContent` bytes, the errors and the JSON rendering.

use blaze_wire::calls::*;
use blaze_wire::shapes::*;
use blaze_wire::vectors::{Gen, Sample, Vector};
use blaze_wire::{ErrorCode, Limits, Named, decode, digest_of, digest_of_encoded_name, encode};

fn limits() -> Limits {
    Limits::DOCUMENTED
}

/// A `WorkKey` and a `JobContentInput` with the same bytes: their digests differ, because the type
/// name is in the digest's input.
#[test]
fn digests_are_separated_by_their_type_names() {
    let item = WorkKey::Item(ItemKey { unit: [1u8; 32], stable: [2u8; 32], env: [3u8; 32] });
    let content =
        JobContentInput { unit: [1u8; 32], platform: [2u8; 32], namespace: Namespace::Main };
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
fn the_job_content_bytes_are_its_unit_platform_and_namespace() {
    // A job's content is `{0: unit, 1: platform, 2: Namespace}`: an equal content joins a running
    // execution rather than starting another. The namespace is a part of it, so a pull request's
    // execution is not another's.
    let (unit, platform) = ([3u8; 32], [4u8; 32]);
    let main = JobContentInput { unit, platform, namespace: Namespace::Main };
    let pr = JobContentInput { unit, platform, namespace: Namespace::Pr(PrNamespace { n: 7 }) };
    let bytes = encode(&main);
    let back: JobContentInput = decode(&bytes, &limits()).unwrap();
    assert_eq!(back, main);
    assert_eq!(bytes[0], 0xa3);
    assert_ne!(JobContent::of(&main), JobContent::of(&pr));
    let other = JobContentInput { unit, platform: [5u8; 32], namespace: Namespace::Main };
    assert_ne!(JobContent::of(&main), JobContent::of(&other));
    assert_eq!(JobContent::of(&main), JobContent::of(&main.clone()));
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
fn the_json_rendering_shows_the_same_value() {
    let mut g = Gen::new(13);
    let job = JobRequest::sample(&mut g);
    let json = blaze_wire::to_json(&job);
    assert!(json.starts_with("{\"JobRequest\":"));
    assert!(json.ends_with('}'));
    // A map's keys are its field numbers as text, and a digest is hex.
    assert!(json.contains("\"0\":\""), "{json}");
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
fn a_types_name_is_its_rust_name() {
    assert_eq!(JobRequest::NAME, "JobRequest");
    assert_eq!(<JobContentInput as Named>::NAME, "JobContentInput");
    assert_eq!(<Attempt as Named>::NAME, "Attempt");
    assert_eq!(Vector::of::<Attempt>(1).name, Attempt::NAME);
}

#[test]
fn the_version_is_in_every_digest() {
    assert_eq!(blaze_wire::VERSION, 5);
    let input = blaze_wire::digest_input(&PrNamespace { n: 1 });
    assert!(input.starts_with(b"blaze/PrNamespace/v5\0"));
}
