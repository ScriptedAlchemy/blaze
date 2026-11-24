//! Object names and what kind of file each is.
//!
//! Two shapes on disk and on the wire:
//! - `packs/<64 lowercase hex>.pack` — a content-addressed pack of records, named by its SHA-256.
//! - `manifests/<64 lowercase hex>` — a unit's list of pack names, named by its unit key.
//!
//! No slashes beyond the one after the directory, no `..`, no uppercase hex, no other extension:
//! anything else is refused before any path is joined.

use core::alloc::Allocator;

use blaze_own::{String, format_in};
use blaze_wire::Digest;

use crate::error::Error;

/// Which of the two object kinds a name belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Content-addressed pack: `packs/<sha256>.pack`.
    Pack,
    /// Unit pack list: `manifests/<unit-key>`.
    Manifest,
}

impl Kind {
    /// The directory name on disk and first path segment on the wire.
    pub const fn directory(self) -> &'static str {
        match self {
            Kind::Pack => "packs",
            Kind::Manifest => "manifests",
        }
    }
}

/// A validated object name.
///
/// Constructing one parses the digest and proves the name has no traversal, no unexpected
/// characters and the right shape for its [`Kind`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ObjectName {
    kind: Kind,
    digest: Digest,
}

impl ObjectName {
    /// Parses a wire object name (`packs/<sha256>.pack` or `manifests/<sha256>`).
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let Some((dir, rest)) = raw.split_once('/') else {
            return Err(Error::malformed(
                "object name must be packs/<sha256>.pack or manifests/<sha256>",
            ));
        };
        if rest.contains('/') {
            return Err(Error::malformed("object name must have exactly two segments"));
        }
        let (kind, hex_part) = match dir {
            "packs" => {
                let Some(stem) = rest.strip_suffix(".pack") else {
                    return Err(Error::malformed("pack name must end in .pack"));
                };
                (Kind::Pack, stem)
            }
            "manifests" => (Kind::Manifest, rest),
            _ => {
                return Err(Error::malformed(
                    "object name must start with packs/ or manifests/",
                ));
            }
        };
        let digest = parse_digest(hex_part)?;
        Ok(Self { kind, digest })
    }

    /// Parses a name and requires it to be of `expected` kind.
    pub fn parse_kind(raw: &str, expected: Kind) -> Result<Self, Error> {
        let parsed = Self::parse(raw)?;
        if parsed.kind != expected {
            return Err(Error::malformed(match expected {
                Kind::Pack => "this route serves packs/<sha256>.pack only",
                Kind::Manifest => "this route serves manifests/<sha256> only",
            }));
        }
        Ok(parsed)
    }

    /// The kind this name belongs to.
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    /// The 32-byte digest carried in the name.
    pub const fn digest(&self) -> &Digest {
        &self.digest
    }

    /// The normalized relative path (`packs/<hex>.pack` or `manifests/<hex>`).
    pub fn as_string_in<A: Allocator>(&self, alloc: A) -> String<A> {
        let h = hex_bytes(&self.digest);
        let h_str = core::str::from_utf8(&h).unwrap_or("");
        match self.kind {
            Kind::Pack => format_in!(alloc, "packs/{h_str}.pack"),
            Kind::Manifest => format_in!(alloc, "manifests/{h_str}"),
        }
    }

    /// The file name inside its directory (`<hex>.pack` or `<hex>`).
    pub fn file_name_in<A: Allocator>(&self, alloc: A) -> String<A> {
        let h = hex_bytes(&self.digest);
        let h_str = core::str::from_utf8(&h).unwrap_or("");
        match self.kind {
            Kind::Pack => format_in!(alloc, "{h_str}.pack"),
            Kind::Manifest => format_in!(alloc, "{h_str}"),
        }
    }
}

/// Parses 64 lowercase hexadecimal characters into a 32-byte digest.
pub(crate) fn parse_digest(hex_str: &str) -> Result<Digest, Error> {
    let bytes = hex_str.as_bytes();
    if bytes.len() != 64 {
        return Err(Error::malformed("digest must be 64 lowercase hex characters"));
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        let (Some(&hi_b), Some(&lo_b)) = (pair.first(), pair.get(1)) else {
            return Err(Error::malformed("digest must be 64 lowercase hex characters"));
        };
        let hi = nibble(hi_b)?;
        let lo = nibble(lo_b)?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

fn nibble(byte: u8) -> Result<u8, Error> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(Error::malformed("digest must be 64 lowercase hex characters")),
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Encodes a 32-byte digest as 64 lowercase ASCII hex bytes on the stack.
pub(crate) fn hex_bytes(digest: &Digest) -> [u8; 64] {
    let mut out = [b'0'; 64];
    for (idx, &byte) in digest.iter().enumerate() {
        let hi = usize::from(byte >> 4);
        let lo = usize::from(byte & 0x0f);
        let pos = idx.saturating_mul(2);
        if let (Some(slot), Some(&ch)) = (out.get_mut(pos), HEX.get(hi)) {
            *slot = ch;
        }
        if let (Some(slot), Some(&ch)) = (out.get_mut(pos.saturating_add(1)), HEX.get(lo)) {
            *slot = ch;
        }
    }
    out
}

/// Formats a 32-byte digest as 64 lowercase hex characters in `alloc`.
pub(crate) fn hex_in<A: Allocator>(digest: &Digest, alloc: A) -> String<A> {
    let bytes = hex_bytes(digest);
    let text = core::str::from_utf8(&bytes).unwrap_or("");
    let mut out = String::with_capacity_in(64, alloc);
    out.push_str(text);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_pack_and_manifest_names_parse() {
        let alloc = blaze_own::testing::heap().shared();
        let pack_str = "packs/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.pack";
        let pack = ObjectName::parse(pack_str).ok();
        assert!(pack.is_some());
        if let Some(pack) = pack {
            assert_eq!(pack.kind(), Kind::Pack);
            assert_eq!(pack.as_string_in(alloc.clone()).as_str(), pack_str);
            assert_eq!(
                pack.file_name_in(alloc.clone()).as_str(),
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.pack"
            );
        }

        let manifest_str =
            "manifests/fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let manifest = ObjectName::parse(manifest_str).ok();
        assert!(manifest.is_some());
        if let Some(manifest) = manifest {
            assert_eq!(manifest.kind(), Kind::Manifest);
            assert_eq!(manifest.as_string_in(alloc).as_str(), manifest_str);
        }
    }

    #[test]
    fn malformed_and_traversal_names_are_refused() {
        for bad in [
            "",
            "packs/",
            "packs/abc.pack",
            "packs/0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef.pack",
            "packs/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "packs/../0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.pack",
            "manifests/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.pack",
            "other/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "manifests/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/extra",
        ] {
            assert!(ObjectName::parse(bad).is_err(), "expected refusal for {bad:?}");
        }
    }
}
