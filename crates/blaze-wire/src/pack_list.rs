//! The native full-manifest format shared by the agent and the cache server: exactly
//! `<pack sha256> <byte length> <keys digest>\n` per pack, with an empty list valid.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::{DIST_PACK_FORMAT, DIST_PACK_MAGIC, Digest, Limits, PackRef};

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
pub fn decode(bytes: &[u8]) -> Result<Vec<PackRef>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.len() > Limits::DOCUMENTED.request as usize {
        return Err("the pack list exceeds the wire request limit".to_owned());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "the pack list is not UTF-8".to_owned())?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err("the pack list ends before a line terminator".to_owned());
    }
    let mut packs = Vec::new();
    let mut seen = BTreeSet::new();
    for (at, line) in text.split_terminator('\n').enumerate() {
        let bad = |why: &str| format!("pack list line {}: {why}", at + 1);
        if line.len() > 151 {
            return Err(bad("the fields exceed their maximum lengths"));
        }
        let mut fields = line.split(' ');
        let pack = digest(fields.next().ok_or_else(|| bad("no pack digest"))?)
            .ok_or_else(|| bad("the pack digest is not 64 lowercase hex digits"))?;
        let length = fields.next().ok_or_else(|| bad("no byte length"))?;
        if length.is_empty()
            || !length.bytes().all(|byte| byte.is_ascii_digit())
            || (length.len() > 1 && length.starts_with('0'))
        {
            return Err(bad("the byte length is not a canonical decimal integer"));
        }
        let size: u64 = length.parse().map_err(|_| bad("the byte length exceeds u64"))?;
        if size < DIST_PACK_MAGIC.len() as u64 {
            return Err(bad("the byte length is shorter than a pack header"));
        }
        let keys = digest(fields.next().ok_or_else(|| bad("no keys digest"))?)
            .ok_or_else(|| bad("the keys digest is not 64 lowercase hex digits"))?;
        if fields.next().is_some() {
            return Err(bad("unexpected fields after the keys digest"));
        }
        if seen.last().is_some_and(|previous| *previous > pack) {
            return Err(bad("the pack digests are not strictly sorted"));
        }
        if !seen.insert(pack) {
            return Err(bad("the pack digest occurs more than once"));
        }
        packs.push(PackRef { pack, size, keys, packed: None, format: DIST_PACK_FORMAT });
    }
    Ok(packs)
}
