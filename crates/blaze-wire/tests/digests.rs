//! The digest domain separation, the `JobContent` bytes, the errors and the JSON rendering.

#![no_std]
#![feature(allocator_api)]

use blaze_own::{String, format_in};
use blaze_wire::calls::*;
use blaze_wire::shapes::*;
use blaze_wire::vectors::{Gen, Sample, Vector};
use blaze_wire::{
    ErrorCode, Limits, Named, WireAlloc, decode, digest_of, digest_of_encoded_name, encode,
};

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn limits() -> Limits {
    Limits::DOCUMENTED
}

/// A `WorkKey` and a `JobContentInput` with the same bytes: their digests differ, because the type
/// name is in the digest's input.
#[test]
fn digests_are_separated_by_their_type_names() {
    let alloc = heap();
    let item = WorkKey::Item(ItemKey { unit: [1u8; 32], stable: [2u8; 32], env: [3u8; 32] });
    let content =
        JobContentInput { unit: [1u8; 32], platform: [2u8; 32], namespace: Namespace::Main };
    let a = digest_of(&item, alloc);
    let b = digest_of(&content, alloc);
    assert_ne!(a, b);
    let bytes = encode(&item, alloc);
    assert_ne!(a, digest_of_encoded_name("JobContentInput", &bytes));
    let plain = blaze_sha256::digest(&bytes);
    assert_ne!(a, plain);
}

#[test]
fn the_job_content_bytes_are_its_unit_platform_and_namespace() {
    let alloc = heap();
    let (unit, platform) = ([3u8; 32], [4u8; 32]);
    let main = JobContentInput { unit, platform, namespace: Namespace::Main };
    let pr = JobContentInput { unit, platform, namespace: Namespace::Pr(PrNamespace { n: 7 }) };
    let bytes = encode(&main, alloc);
    let back: JobContentInput = decode(&bytes, &limits(), alloc).unwrap();
    assert_eq!(back, main);
    assert_eq!(bytes[0], 0xa3);
    assert_ne!(JobContent::of(&main, alloc), JobContent::of(&pr, alloc));
    let other = JobContentInput { unit, platform: [5u8; 32], namespace: Namespace::Main };
    assert_ne!(JobContent::of(&main, alloc), JobContent::of(&other, alloc));
    assert_eq!(JobContent::of(&main, alloc), JobContent::of(&main.clone(), alloc));
}

#[test]
fn the_errors_carry_the_section_8_codes_and_statuses() {
    let alloc = heap();
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
        assert_eq!(code.as_u8(), byte);
        let error = WireError::new(code, format_args!("why"), alloc);
        assert_eq!(error.code(), Some(code));
        let bytes = encode(&error, alloc);
        assert_eq!(decode::<WireError>(&bytes, &limits(), alloc).unwrap(), error);
    }
    assert_eq!(ErrorCode::from_u8(0), None);
    assert_eq!(ErrorCode::from_u8(11), None);
    let error = WireError::new(ErrorCode::Busy, format_args!("later"), alloc).retry_after(1_000);
    assert_eq!(error.retry_after_ms, Some(1_000));
    assert_eq!(&encode(&error, alloc)[..2], &[0xa3, 0x00]);
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
}

#[test]
fn every_type_renders_as_json() {
    let alloc = heap();
    for entry in blaze_wire::vectors::every() {
        let mut json = String::new_in(alloc);
        blaze_wire::json::render_cbor(&(entry.vector)(1, alloc).cbor, &mut json).unwrap();
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
    let alloc = heap();
    let digest = digest_of(&Attempt(1), alloc);
    let hex = blaze_wire::json::hex(&digest);
    assert_eq!(hex.len(), 64);
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    let mut out = String::new_in(alloc);
    blaze_wire::json::render(&digest, &mut out, alloc).unwrap();
    assert_eq!(out, format_in!(alloc, "\"{hex}\""));
}

#[test]
fn the_json_rendering_shows_the_same_value() {
    let alloc = heap();
    let mut g = Gen::new(13);
    let job = JobRequest::sample(&mut g, alloc);
    let mut json = String::new_in(alloc);
    blaze_wire::to_json(&job, &mut json, alloc).unwrap();
    assert!(json.starts_with("{\"JobRequest\":"));
    assert!(json.ends_with('}'));
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
            c if u32::from(c) < 0x20 && !in_string => panic!("a control character in the JSON"),
            _ => {}
        }
    }
    assert_eq!(depth, 0);
    assert!(!in_string);
}

#[test]
fn a_types_name_is_its_rust_name() {
    let alloc = heap();
    assert_eq!(JobRequest::NAME, "JobRequest");
    assert_eq!(<JobContentInput as Named>::NAME, "JobContentInput");
    assert_eq!(<Attempt as Named>::NAME, "Attempt");
    assert_eq!(Vector::of::<Attempt>(1, alloc).name, Attempt::NAME);
}

#[test]
fn the_version_is_in_every_digest() {
    let alloc = heap();
    assert_eq!(blaze_wire::VERSION, 5);
    let input = blaze_wire::digest_input(&PrNamespace { n: 1 }, alloc);
    assert!(input.starts_with(b"blaze/PrNamespace/v5\0"));
}
