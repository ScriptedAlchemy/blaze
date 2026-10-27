//! blaze's wire schema: every type of [docs/distributed.md](../../docs/distributed.md), its
//! encoding, its digest, its signature, its errors and limits, and a JSON rendering for people.
//!
//! **The encoding** is deterministic CBOR, RFC 8949 section 4.2's core deterministic encoding:
//! shortest heads, definite lengths, map keys ascending by their encoded bytes, no floats and no
//! tags. A struct is a map of small integers, every key present; an enum is the two-element array
//! `[discriminant, payload]`. One value has one encoding, so a request, a record or a test result
//! can be hashed and kept under that hash.
//!
//! **The validator** ([`validate`]) refuses any message not in that form and names the first
//! offence: a non-shortest head, an indefinite length, unsorted keys, a duplicate key, a float, a
//! tag, an unknown key, an oversize item. [`validate_as`] also refuses a key the schema type does
//! not have. Nothing canonicalizes a bad message: a client that encodes wrong finds out.
//!
//! **Digests** are `sha256("blaze/" ‖ name ‖ "/v4" ‖ 0x00 ‖ canonical CBOR of the value)`, where
//! `name` is the type's name; [`digest_of`] writes it. The domain separation means the bytes of a
//! `WorkKey` and of a `JobContentInput` never collide even when their encodings do.
//!
//! **Signatures** are Ed25519 over the same bytes with `sig/` in front of the name
//! ([`sign`], [`verify`]), so a signature can never be replayed as a digest or as another type's.
//!
//! **JSON** ([`to_json`]) is only a rendering for people (`Accept: application/json`, `--json`);
//! it is never the wire, and no value's identity is taken from it.

#![deny(missing_docs)]

pub mod calls;
mod cbor;
pub mod credentials;
pub mod job_terminal_v2;
pub mod json;
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
pub use vectors::{Gen, Sample, SelfSigned, Vector, every};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest as _, Sha256};

/// A SHA-256 digest: 32 bytes, a byte string on the wire.
pub type Digest = [u8; 32];

/// The version of the schema this crate speaks. A change that adds a field is a new version.
pub const VERSION: u32 = 4;

/// The word every digest's input starts with.
const DIGEST_PREFIX: &str = "blaze/";

/// The word every signature's input starts with, before the digest's own.
const SIGNATURE_PREFIX: &str = "blaze/sig/";

/// The version word of every digest and signature's input.
const DOMAIN_VERSION: &str = "/v4";

/// The bytes a digest of `value` is taken over: `"blaze/" ‖ name ‖ "/v4" ‖ 0x00 ‖ CBOR`.
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

/// The bytes a signature of `value` is taken over: [`digest_input`] with `sig/` in the name.
pub fn signature_input<T: Wire + Named>(value: &T) -> Vec<u8> {
    signature_input_encoded::<T>(&encode(value))
}

/// The bytes a signature of type `T` over `cbor` is taken over, for bytes already written.
pub fn signature_input_encoded<T: Named>(cbor: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + cbor.len());
    out.extend_from_slice(SIGNATURE_PREFIX.as_bytes());
    out.extend_from_slice(T::NAME.as_bytes());
    out.extend_from_slice(DOMAIN_VERSION.as_bytes());
    out.push(0);
    out.extend_from_slice(cbor);
    out
}

/// The signature of type `name` over `cbor`, for a name a golden file carries rather than a type.
pub fn sign_encoded(name: &str, cbor: &[u8], key: &SigningKey) -> [u8; 64] {
    key.sign(&signature_input_name(name, cbor)).to_bytes()
}

/// Whether `signature` is type `name`'s over `cbor`.
pub fn verify_encoded_name(
    name: &str,
    cbor: &[u8],
    key: &VerifyingKey,
    signature: &[u8; 64],
) -> bool {
    key.verify_strict(&signature_input_name(name, cbor), &Signature::from_bytes(signature)).is_ok()
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

fn signature_input_name(name: &str, cbor: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + cbor.len());
    out.extend_from_slice(SIGNATURE_PREFIX.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(DOMAIN_VERSION.as_bytes());
    out.push(0);
    out.extend_from_slice(cbor);
    out
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

/// The Ed25519 signature of `value` under `key`, over its domain-separated encoding.
pub fn sign<T: Wire + Named>(key: &SigningKey, value: &T) -> [u8; 64] {
    key.sign(&signature_input(value)).to_bytes()
}

/// Whether `signature` is `value`'s under `key`.
pub fn verify<T: Wire + Named>(key: &VerifyingKey, value: &T, signature: &[u8; 64]) -> bool {
    key.verify_strict(&signature_input(value), &Signature::from_bytes(signature)).is_ok()
}

/// A message from an agent, as section 8's `Blaze-Agent` headers sign it.
///
/// The signature is Ed25519 over
/// `"blaze/req/v1" ‖ 0x00 ‖ method ‖ 0x00 ‖ path ‖ 0x00 ‖ sha256(body) ‖ 0x00 ‖ the date`, and the
/// server refuses a date more than 60 s off and a signature it has seen.
pub struct AgentRequest<'a> {
    /// The agent's name, the `Blaze-Agent` header.
    pub agent: &'a str,
    /// The unix milliseconds of the `Blaze-Date` header.
    pub date_ms: u64,
    /// The HTTP method, uppercase as section 8 writes it.
    pub method: &'a str,
    /// The path, with its query when it has one.
    pub path: &'a str,
    /// The request's body.
    pub body: &'a [u8],
}

impl AgentRequest<'_> {
    /// How far a date may be from the server's clock, in milliseconds.
    pub const MAX_SKEW_MS: u64 = 60_000;

    /// The bytes the `Blaze-Signature` header signs.
    pub fn signing_input(&self) -> Vec<u8> {
        let body = Sha256::digest(self.body);
        let date = self.date_ms.to_string();
        let mut out = Vec::with_capacity(64 + self.method.len() + self.path.len());
        for field in [
            &b"blaze/req/v1"[..],
            self.method.as_bytes(),
            self.path.as_bytes(),
            &body,
            date.as_bytes(),
        ] {
            out.extend_from_slice(field);
            out.push(0);
        }
        // The date ends the input, so its separator is not written.
        out.pop();
        out
    }

    /// The `Blaze-Signature` value: base64url, without padding.
    pub fn sign(&self, key: &SigningKey) -> String {
        base64url(&key.sign(&self.signing_input()).to_bytes())
    }

    /// Whether `signature` is this request's under `key`.
    pub fn verify(&self, key: &VerifyingKey, signature: &str) -> bool {
        let Some(bytes) = base64url_decode(signature) else {
            return false;
        };
        let Ok(signature) = <[u8; 64]>::try_from(&bytes[..]) else {
            return false;
        };
        key.verify_strict(&self.signing_input(), &Signature::from_bytes(&signature)).is_ok()
    }

    /// Whether `now_ms` is within [`AgentRequest::MAX_SKEW_MS`] of this request's date.
    pub fn date_is_near(&self, now_ms: u64) -> bool {
        self.date_ms.abs_diff(now_ms) <= AgentRequest::MAX_SKEW_MS
    }
}

/// The `Blaze-Capability` header: a [`SignedCapability`]'s canonical CBOR, base64url, no padding.
pub fn capability_header(signed: &SignedCapability) -> String {
    base64url(&encode(signed))
}

/// The [`SignedCapability`] of a `Blaze-Capability` header, refused as the validator refuses.
pub fn capability_from_header(header: &str, limits: &Limits) -> Result<SignedCapability, Error> {
    let bytes = base64url_decode(header).ok_or(Error {
        kind: ErrorKind::UnexpectedType { expected: "base64url in the header" },
        at: 0,
    })?;
    validate_as(&bytes, limits)
}

/// `bytes` in base64url without padding, RFC 4648 section 5.
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// The bytes of base64url text without padding, or `None` when it is not.
///
/// The text must be canonical: the unused low bits of the last character are zero, so one byte
/// string has one text, and a signature or a capability read from a header has no textual alias.
pub fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    // A leftover of six bits is a byte's worth of text that is not there; a leftover of one to
    // five bits must be zero, or two texts would name the same bytes.
    if bits >= 6 || buffer != 0 { None } else { Some(out) }
}

/// A frame: a four-byte big-endian length and then that many bytes, as the socket and an event
/// stream carry one item.
pub struct Framing;

impl Framing {
    /// The four-byte head of `item`, which must be within `limit`.
    pub fn head(item: &[u8], limit: u32) -> Result<[u8; 4], Error> {
        let len = u32::try_from(item.len()).map_err(|_| Error {
            kind: ErrorKind::OversizeItem {
                what: "item",
                len: item.len() as u64,
                limit: u64::from(limit),
            },
            at: 0,
        })?;
        if len > limit {
            return Err(Error {
                kind: ErrorKind::OversizeItem {
                    what: "item",
                    len: u64::from(len),
                    limit: u64::from(limit),
                },
                at: 0,
            });
        }
        Ok(len.to_be_bytes())
    }
}
