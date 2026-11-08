//! blaze's wire schema for a distributed build ([docs/distributed.md](../../docs/distributed.md)):
//! its types, their encoding, their digests, their errors and limits, and a JSON rendering for
//! people.
//!
//! **The encoding** is deterministic CBOR, RFC 8949 section 4.2's core deterministic encoding:
//! shortest heads, definite lengths, map keys ascending by their encoded bytes, no floats and no
//! tags. A struct is a map of small integers, every key present; an enum is the two-element array
//! `[discriminant, payload]`. One value has one encoding, so a request or a result can be hashed
//! and kept under that hash.
//!
//! **The validator** ([`validate`]) refuses any message not in that form and names the first
//! offence: a non-shortest head, an indefinite length, unsorted keys, a duplicate key, a float, a
//! tag, an unknown key, an oversize item. [`validate_as`] also refuses a key the schema type does
//! not have. Nothing canonicalizes a bad message: a client that encodes wrong finds out.
//!
//! **Digests** are `sha256("blaze/" ‖ name ‖ "/v5" ‖ 0x00 ‖ canonical CBOR of the value)`, where
//! `name` is the type's name; [`digest_of`] writes it. The domain separation means the bytes of a
//! `WorkKey` and of a `JobContentInput` never collide even when their encodings do.
//!
//! **JSON** ([`to_json`]) is only a rendering for people; it is never the wire, and no value's
//! identity is taken from it.

#![deny(missing_docs)]

pub mod calls;
mod cbor;
pub mod job_terminal_v2;
pub mod json;
pub mod pack_list;
pub mod shapes;
pub mod vectors;

pub use calls::*;
pub use cbor::Limits;
pub use cbor::{
    Appender, Bytes, Encoder, Error, ErrorKind, MAX_DEPTH, Named, Reader, Set, Wire, decode,
    encode, validate, validate_as,
};
pub use json::to_json;
pub use shapes::*;
pub use vectors::{Gen, Sample, Vector, every};

use sha2::{Digest as _, Sha256};

/// A SHA-256 digest: 32 bytes, a byte string on the wire.
pub type Digest = [u8; 32];

/// The version of the schema this crate speaks. A change that adds a field is a new version.
pub const VERSION: u32 = 5;

/// The first eight bytes of a distributed pack: the store values one batch of work made
/// (`blaze_dist_local::pack`), under one content digest. `blaze-serve` refuses any other bytes
/// under a `packs/` name.
pub const DIST_PACK_MAGIC: [u8; 8] = *b"blzdpk\0\x01";

/// The format `PackRef::format` names for a pack that starts with [`DIST_PACK_MAGIC`].
pub const DIST_PACK_FORMAT: u32 = 1;

/// Whether `start`, a blob's first bytes, is a distributed pack of this format.
pub fn is_dist_pack(start: &[u8]) -> bool {
    start.starts_with(&DIST_PACK_MAGIC)
}

/// The word every digest's input starts with.
const DIGEST_PREFIX: &str = "blaze/";

/// The version word of every digest and signature's input.
const DOMAIN_VERSION: &str = "/v5";

/// The bytes a digest of `value` is taken over: `"blaze/" ‖ name ‖ "/v5" ‖ 0x00 ‖ CBOR`.
pub fn digest_input<T: Wire + Named>(value: &T) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(DIGEST_PREFIX.as_bytes());
    out.extend_from_slice(T::NAME.as_bytes());
    out.extend_from_slice(DOMAIN_VERSION.as_bytes());
    out.push(0);
    {
        let mut e = Encoder::new_into(&mut out);
        value.encode(&mut e);
    }
    out
}

/// The SHA-256 of type `name`'s `cbor`, for a name a golden file carries.
pub fn digest_of_encoded_name(name: &str, cbor: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(DIGEST_PREFIX.as_bytes());
    hasher.update(name.as_bytes());
    hasher.update(DOMAIN_VERSION.as_bytes());
    hasher.update([0u8]);
    hasher.update(cbor);
    hasher.finalize().into()
}

/// The SHA-256 of `value`'s domain-separated encoding.
pub fn digest_of<T: Wire + Named>(value: &T) -> Digest {
    digest_of_encoded::<T>(&encode(value))
}

/// The SHA-256 of `cbor` behind the domain separation of type `T`, for a value whose canonical
/// CBOR is already written: a digest newtype over a tuple hashes its array without a wrapper.
pub fn digest_of_encoded<T: Named>(cbor: &[u8]) -> Digest {
    digest_of_encoded_name(T::NAME, cbor)
}
