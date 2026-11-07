//! A distributed pack: the store values one batch of work made, under one content digest.
//!
//! `DIST_PACK_MAGIC`, then the values sorted by key, each its 32-byte key, a byte naming its
//! kind (`blaze_cas::Kind::code`, opaque here), its length as a little-endian `u64` and its
//! bytes. A pack is named by the SHA-256 of all of that ([`Pack::digest`]), and
//! [`PackRef::keys`] is the digest of its keys, so a reader checks it has what was committed
//! before it believes one value.

use blaze_wire::{
    DIST_PACK_FORMAT, DIST_PACK_MAGIC, Digest, PackRef, digest_of_encoded_name, encode,
};
use sha2::{Digest as _, Sha256};

/// One value: a store key, its kind's code, its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    /// The store key.
    pub key: Digest,
    /// The kind's code, which only the store reads.
    pub kind: u8,
    /// The value.
    pub bytes: Vec<u8>,
}

/// A pack as it goes on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pack {
    /// The whole pack.
    pub bytes: Vec<u8>,
    /// What a result commits about it.
    pub reference: PackRef,
}

impl Pack {
    /// The pack of `values`, in key order whatever order they came in, so two executors that
    /// made the same values name the same pack. Two values under one key with different bytes
    /// are an error: a key covers all its value depends on.
    pub fn of(mut values: Vec<Value>) -> Result<Pack, String> {
        values.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        values.dedup();
        for pair in values.windows(2) {
            if pair[0].key == pair[1].key {
                return Err(format!("two values under the key {}", hex(&pair[0].key)));
            }
        }
        let size: usize = values.iter().map(|it| 41 + it.bytes.len()).sum();
        let mut bytes = Vec::with_capacity(DIST_PACK_MAGIC.len() + size);
        bytes.extend_from_slice(&DIST_PACK_MAGIC);
        for value in &values {
            bytes.extend_from_slice(&value.key);
            bytes.push(value.kind);
            bytes.extend_from_slice(&(value.bytes.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&value.bytes);
        }
        let reference = PackRef {
            pack: Sha256::digest(&bytes).into(),
            size: bytes.len() as u64,
            keys: keys_digest(values.iter().map(|it| it.key)),
            packed: None,
            format: DIST_PACK_FORMAT,
        };
        Ok(Pack { bytes, reference })
    }
}

fn keys_digest(keys: impl Iterator<Item = Digest>) -> Digest {
    let keys: Vec<Digest> = keys.collect();
    digest_of_encoded_name("blaze/dist-pack-keys", &encode(&keys))
}

/// The values of `bytes`, which must be the pack `reference` names: its digest, size, format
/// and keys are all checked before a value is returned, and the values must be in key order.
pub fn read(bytes: &[u8], reference: &PackRef) -> Result<Vec<Value>, String> {
    let name = hex(&reference.pack);
    if reference.format != DIST_PACK_FORMAT {
        return Err(format!("pack {name} is of format {}", reference.format));
    }
    if bytes.len() as u64 != reference.size {
        return Err(format!("pack {name} is {} bytes, committed as {}", bytes.len(), reference.size));
    }
    let got: Digest = Sha256::digest(bytes).into();
    if got != reference.pack {
        return Err(format!("pack {name} hashes to {}", hex(&got)));
    }
    let mut rest =
        bytes.strip_prefix(&DIST_PACK_MAGIC).ok_or_else(|| format!("pack {name} has no header"))?;
    let mut values = Vec::new();
    while !rest.is_empty() {
        let short = || format!("pack {name} ends inside a value");
        let (key, after) = rest.split_first_chunk::<32>().ok_or_else(short)?;
        let (kind, after) = after.split_first().ok_or_else(short)?;
        let (len, after) = after.split_first_chunk::<8>().ok_or_else(short)?;
        let len = usize::try_from(u64::from_le_bytes(*len)).map_err(|_| short())?;
        if after.len() < len {
            return Err(short());
        }
        let (value, after) = after.split_at(len);
        if values.last().is_some_and(|last: &Value| last.key >= *key) {
            return Err(format!("pack {name} is not in key order"));
        }
        values.push(Value { key: *key, kind: *kind, bytes: value.to_vec() });
        rest = after;
    }
    if keys_digest(values.iter().map(|it| it.key)) != reference.keys {
        return Err(format!("pack {name} holds other keys than were committed"));
    }
    Ok(values)
}

/// Lower-case hexadecimal, as blob names spell a digest.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(key: u8, bytes: &[u8]) -> Value {
        Value { key: [key; 32], kind: 1, bytes: bytes.to_vec() }
    }

    #[test]
    fn a_pack_reads_back_whatever_order_it_was_made_in() {
        let one = Pack::of(vec![value(2, b"two"), value(1, b"one")]).unwrap();
        let other = Pack::of(vec![value(1, b"one"), value(2, b"two")]).unwrap();
        assert_eq!(one, other, "two executors with the same values name one pack");
        assert_eq!(
            read(&one.bytes, &one.reference).unwrap(),
            vec![value(1, b"one"), value(2, b"two")]
        );
    }

    #[test]
    fn two_values_under_one_key_are_refused() {
        let error = Pack::of(vec![value(1, b"a"), value(1, b"b")]).unwrap_err();
        assert!(error.contains("two values"), "{error}");
        assert!(Pack::of(vec![value(1, b"a"), value(1, b"a")]).is_ok(), "one value, twice");
    }

    #[test]
    fn a_reader_believes_nothing_the_commit_does_not_name() {
        let pack = Pack::of(vec![value(1, b"one")]).unwrap();
        let mut flipped = pack.bytes.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(read(&flipped, &pack.reference).unwrap_err().contains("hashes"));
        let mut short = pack.reference.clone();
        short.size -= 1;
        assert!(read(&pack.bytes, &short).unwrap_err().contains("committed"));
        let mut keys = pack.reference.clone();
        keys.keys = [9; 32];
        assert!(read(&pack.bytes, &keys).unwrap_err().contains("other keys"));
        // The right digest over bytes that are not a pack: refused by their shape.
        let junk = b"not a pack".to_vec();
        let reference = PackRef {
            pack: Sha256::digest(&junk).into(),
            size: junk.len() as u64,
            ..pack.reference.clone()
        };
        assert!(read(&junk, &reference).unwrap_err().contains("no header"));
    }
}
