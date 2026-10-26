//! The credential vectors of section 8 that are not a type of the schema: the `Blaze-Agent`
//! signature over a request, the `Blaze-Capability` header, the `ops` bits and `JobContent`'s
//! bytes, checked against the golden file `tests/golden/credentials.txt`.

use blaze_wire::credentials::{agent_request, capability, golden_text, key};
use blaze_wire::{Limits, encode};

#[test]
fn the_credentials_match_their_golden_file() {
    let path = format!("{}/tests/golden/credentials.txt", env!("CARGO_MANIFEST_DIR"));
    let checked_in = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no credentials golden file at {path}: {e}"));
    assert_eq!(
        checked_in,
        golden_text(),
        "the credentials' golden file is not what this build makes"
    );
}

#[test]
fn the_agent_vector_verifies() {
    let path = format!("{}/tests/golden/credentials.txt", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).unwrap();
    let signature = field(&text, "[agent]", "signature");
    let request = agent_request();
    assert!(request.verify(&key().verifying_key(), &signature), "the golden signature failed");
}

#[test]
fn the_capability_header_reads_back() {
    let path = format!("{}/tests/golden/credentials.txt", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).unwrap();
    let header = field(&text, "[capability]", "header");
    let signed = blaze_wire::capability_from_header(&header, &Limits::DOCUMENTED)
        .expect("the golden header did not read back");
    assert_eq!(signed.capability, capability());
    assert!(signed.capability.signature_is(&key().verifying_key(), &signed.signature));
}

#[test]
fn the_capability_s_ops_are_a_byte_of_bits() {
    // The `ops` field is field 6 of a `Capability`, an unsigned integer.
    let capability = capability();
    let bytes = encode(&capability);
    // lookup, fetch, publish, events and roots are bits 0 to 3 and 5: 47, one byte after a
    // one-byte head, because 47 is past the 23 a head holds on its own.
    assert_eq!(capability.ops, 47);
    assert!(bytes.windows(3).any(|w| w == [0x06, 0x18, 0x2f]), "ops is not field 6 as 47");
    // A capability with every operation set writes 0xff, still one byte after its head.
    let all = blaze_wire::Capability { ops: 0xff, ..capability };
    assert!(encode(&all).windows(3).any(|w| w == [0x06, 0x18, 0xff]));
}

/// The value of `name` in `text`'s `section`.
fn field(text: &str, section: &str, name: &str) -> String {
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with('[') {
            inside = line == section;
            continue;
        }
        if inside
            && let Some((key, value)) = line.split_once(": ")
            && key == name
        {
            return value.to_owned();
        }
    }
    panic!("no {name} in {section}")
}
