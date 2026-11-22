//! The persisted namespace of a cached closure's code. This is the exact key constructor
//! used by `fncache::cache_code`, isolated from the parked front end so an active library
//! target can exercise its version boundary. Its tests are also compiled by
//! `crates/blaze-ids/tests/cached_code.rs` from this same source file.
//!
//! The format is part of the key, not a header read after a hit: a result filed under the
//! previous format must have another name, even when every other input is unchanged.

use blaze_ids::{ContentId, Hasher};

/// Changes when the meaning of the code identity changes. The parent retains its FORMAT
/// alias so the persisted-layout gate compares the old declaration with this one.
pub(super) const FORMAT: u32 = 2;
const CACHED_CODE_DOMAIN: &str = "blaze v2 cached code 1";

/// The first 128 bits of the closure's versioned behavior identity and compilation context.
/// The field order and framing are the pre-extraction format-2 formula, byte for byte.
pub(super) fn code(
    format: u32,
    target: &[u8],
    nightly: &[u8],
    checked: bool,
    behavior: &ContentId,
) -> u128 {
    let mut hasher = Hasher::new(CACHED_CODE_DOMAIN);
    hasher.update(&format.to_le_bytes()).part(target);
    hasher.part(nightly).update(&[checked as u8]).id(behavior);
    let id = hasher.finish().0;
    u128::from_le_bytes(id[..16].try_into().expect("a content ID's first 16 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &[u8] = b"aarch64-unknown-linux-gnu";
    const NIGHTLY: &[u8] = b"nightly-2026-10-27";
    const BEHAVIOR: ContentId = ContentId([0x2a; 32]);

    // The previous format's persisted key bytes, assembled independently of `code` from
    // the old field sequence. This fixture deliberately shares every input except format.
    fn previous_format_key() -> [u8; 16] {
        let mut old = Hasher::new(CACHED_CODE_DOMAIN);
        old.update(&1u32.to_le_bytes()).part(TARGET);
        old.part(NIGHTLY).update(&[1]).id(&BEHAVIOR);
        old.finish().0[..16].try_into().unwrap()
    }

    fn separate_versions(key: impl Fn(u32) -> u128) -> Result<(), &'static str> {
        if key(1) == key(FORMAT) {
            Err("a cached answer from the previous format is reachable under the new key")
        } else {
            Ok(())
        }
    }

    /// The invariant: changing the cached-code format separates the previous and current
    /// key namespaces while preserving every field of the existing format-2 identity.
    #[test]
    fn a_previous_format_has_a_different_cached_code_key() {
        let old = previous_format_key();
        let current = code(FORMAT, TARGET, NIGHTLY, true, &BEHAVIOR).to_le_bytes();
        assert_eq!(code(1, TARGET, NIGHTLY, true, &BEHAVIOR).to_le_bytes(), old);
        assert_ne!(current, old, "the previous namespace must not yield a current cache hit");
        assert_eq!(
            separate_versions(|format| code(format, TARGET, NIGHTLY, true, &BEHAVIOR)),
            Ok(()),
        );

        // The extraction preserves the shipped format-2 formula, not only inequality with
        // the previous version. An unrelated key churn would lose every existing hit.
        let mut existing = Hasher::new(CACHED_CODE_DOMAIN);
        existing.update(&2u32.to_le_bytes()).part(TARGET);
        existing.part(NIGHTLY).update(&[1]).id(&BEHAVIOR);
        assert_eq!(current.as_slice(), &existing.finish().0[..16]);
    }

    #[test]
    fn the_oracle_catches_a_format_left_out_of_the_key() {
        let planted = separate_versions(|_format| code(FORMAT, TARGET, NIGHTLY, true, &BEHAVIOR));
        assert_eq!(
            planted,
            Err("a cached answer from the previous format is reachable under the new key"),
        );
    }

    #[test]
    fn every_compilation_input_changes_the_cached_code_key() {
        let original = code(FORMAT, TARGET, NIGHTLY, true, &BEHAVIOR);
        let changed = [
            code(FORMAT, b"x86_64-unknown-linux-gnu", NIGHTLY, true, &BEHAVIOR),
            code(FORMAT, TARGET, b"nightly-2026-10-28", true, &BEHAVIOR),
            code(FORMAT, TARGET, NIGHTLY, false, &BEHAVIOR),
            code(FORMAT, TARGET, NIGHTLY, true, &ContentId([0x2b; 32])),
        ];
        for other in changed {
            assert_ne!(original, other);
        }
        assert_ne!(
            code(FORMAT, b"ab", b"c", true, &BEHAVIOR),
            code(FORMAT, b"a", b"bc", true, &BEHAVIOR),
            "field framing prevents a target/nightly boundary from aliasing",
        );
    }
}
