//! The native full-manifest format shared by the agent and the cache server: exactly
//! `<pack sha256> <byte length> <keys digest>\n` per pack, with an empty list valid.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::{DIST_PACK_FORMAT, DIST_PACK_MAGIC, Digest, Limits, PackRef};

/// Why a native manifest cannot be read as a canonical list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeErrorKind {
    /// The entire manifest exceeds the advertised wire limit.
    RequestLimit,
    /// A manifest's bytes are not UTF-8.
    Utf8(std::str::Utf8Error),
    /// The final record has no line terminator.
    LineTerminator,
    /// A record exceeds the maximum lengths of its three fields.
    FieldLengths,
    /// The pack digest field is absent.
    MissingPack,
    /// The pack digest is not 64 lowercase hexadecimal digits.
    PackDigest,
    /// The byte-length field is absent.
    MissingLength,
    /// The byte length is not a canonical unsigned decimal.
    LengthSyntax,
    /// A decimal byte length exceeds its wire representation.
    LengthOverflow(std::num::ParseIntError),
    /// A pack cannot contain even its header.
    ShortPack,
    /// The keys digest field is absent.
    MissingKeys,
    /// The keys digest is not 64 lowercase hexadecimal digits.
    KeysDigest,
    /// A record has fields beyond its three required fields.
    ExtraFields,
    /// A pack digest is before its predecessor.
    PackOrder,
    /// Two records name the same pack.
    DuplicatePack,
}

/// A manifest refusal, with a one-based record number for record-local failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    /// The offending record, or `None` for an envelope failure.
    pub line: Option<usize>,
    /// The violated format requirement, with its original parsing error when present.
    pub kind: DecodeErrorKind,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(line) = self.line {
            write!(f, "pack list line {line}: ")?;
        } else {
            f.write_str("the pack list ")?;
        }
        f.write_str(match self.kind {
            DecodeErrorKind::RequestLimit => "exceeds the wire request limit",
            DecodeErrorKind::Utf8(_) => "is not UTF-8",
            DecodeErrorKind::LineTerminator => "ends before a line terminator",
            DecodeErrorKind::FieldLengths => "the fields exceed their maximum lengths",
            DecodeErrorKind::MissingPack => "no pack digest",
            DecodeErrorKind::PackDigest => "the pack digest is not 64 lowercase hex digits",
            DecodeErrorKind::MissingLength => "no byte length",
            DecodeErrorKind::LengthSyntax => "the byte length is not a canonical decimal integer",
            DecodeErrorKind::LengthOverflow(_) => "the byte length exceeds u64",
            DecodeErrorKind::ShortPack => "the byte length is shorter than a pack header",
            DecodeErrorKind::MissingKeys => "no keys digest",
            DecodeErrorKind::KeysDigest => "the keys digest is not 64 lowercase hex digits",
            DecodeErrorKind::ExtraFields => "unexpected fields after the keys digest",
            DecodeErrorKind::PackOrder => "the pack digests are not strictly sorted",
            DecodeErrorKind::DuplicatePack => "the pack digest occurs more than once",
        })
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            DecodeErrorKind::Utf8(source) => Some(source),
            DecodeErrorKind::LengthOverflow(source) => Some(source),
            _ => None,
        }
    }
}

/// Render the native list, whose references use the current raw pack format. Order is kept:
/// callers canonicalize their set before its attempt-owned commit.
pub fn encode(packs: &[PackRef]) -> Vec<u8> {
    let mut out = String::new();
    for pack in packs {
        writeln!(
            out,
            "{} {} {}",
            crate::json::hex(&pack.pack),
            pack.size,
            crate::json::hex(&pack.keys)
        )
        .expect("writing a string");
    }
    out.into_bytes()
}

fn digest(text: &str) -> Option<Digest> {
    let mut out = [0; 32];
    if text.len() != 64
        || !text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Read one bounded native list. Noncanonical fields, unknown fields, invalid lengths and
/// repeated pack digests are errors rather than an empty or partial cache entry.
pub fn decode(bytes: &[u8]) -> Result<Vec<PackRef>, DecodeError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.len() > Limits::DOCUMENTED.request as usize {
        return Err(DecodeError { line: None, kind: DecodeErrorKind::RequestLimit });
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|source| DecodeError { line: None, kind: DecodeErrorKind::Utf8(source) })?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(DecodeError { line: None, kind: DecodeErrorKind::LineTerminator });
    }
    let mut packs = Vec::new();
    let mut seen = BTreeSet::new();
    for (at, line) in text.split_terminator('\n').enumerate() {
        let bad = |kind| DecodeError { line: Some(at + 1), kind };
        if line.len() > 151 {
            return Err(bad(DecodeErrorKind::FieldLengths));
        }
        let mut fields = line.split(' ');
        let pack = digest(fields.next().ok_or_else(|| bad(DecodeErrorKind::MissingPack))?)
            .ok_or_else(|| bad(DecodeErrorKind::PackDigest))?;
        let length = fields.next().ok_or_else(|| bad(DecodeErrorKind::MissingLength))?;
        if length.is_empty()
            || !length.bytes().all(|byte| byte.is_ascii_digit())
            || (length.len() > 1 && length.starts_with('0'))
        {
            return Err(bad(DecodeErrorKind::LengthSyntax));
        }
        let size: u64 = length.parse()
            .map_err(|source| bad(DecodeErrorKind::LengthOverflow(source)))?;
        if size < DIST_PACK_MAGIC.len() as u64 {
            return Err(bad(DecodeErrorKind::ShortPack));
        }
        let keys = digest(fields.next().ok_or_else(|| bad(DecodeErrorKind::MissingKeys))?)
            .ok_or_else(|| bad(DecodeErrorKind::KeysDigest))?;
        if fields.next().is_some() {
            return Err(bad(DecodeErrorKind::ExtraFields));
        }
        if seen.last().is_some_and(|previous| *previous > pack) {
            return Err(bad(DecodeErrorKind::PackOrder));
        }
        if !seen.insert(pack) {
            return Err(bad(DecodeErrorKind::DuplicatePack));
        }
        packs.push(PackRef { pack, size, keys, packed: None, format: DIST_PACK_FORMAT });
    }
    Ok(packs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn refusals_retain_the_record_and_the_parsing_cause() {
        let invalid_utf8 = decode(&[0xff]).unwrap_err();
        assert_eq!(invalid_utf8.line, None);
        assert!(matches!(invalid_utf8.kind, DecodeErrorKind::Utf8(_)));
        assert!(invalid_utf8.source().is_some());

        let first = format!("{} 80 {}\n", "ab".repeat(32), "cd".repeat(32));
        let repeated = decode(format!("{first}{first}").as_bytes()).unwrap_err();
        assert_eq!(repeated.line, Some(2));
        assert_eq!(repeated.kind, DecodeErrorKind::DuplicatePack);
        let overflow = decode(first.replace(" 80 ", " 18446744073709551616 ").as_bytes())
            .unwrap_err();
        assert_eq!(overflow.line, Some(1));
        assert!(matches!(overflow.kind, DecodeErrorKind::LengthOverflow(_)));
        assert!(overflow.source().is_some());
        assert!(decode(first.as_bytes()).is_ok());
    }
}
