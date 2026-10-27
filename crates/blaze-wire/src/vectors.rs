//! Random values of every wire type, and the golden vectors they make.
//!
//! A test that checks a type round-trips byte-identically needs values of it that are not the
//! handful a person writes; [`Sample`] gives one from a seed, and [`Gen`] is the seed's stream.
//! The values are drawn with the edges of the encoding in mind: integers at 23, 24, 255, 256,
//! 65,535, 65,536 and the ends of their range, strings and byte strings at the lengths whose heads
//! change width, empty and long arrays, and sets whose input is not sorted.
//!
//! [`every`] lists one entry per schema type, so a test can walk all of them and the golden
//! vectors can be generated and checked in one place.

use ed25519_dalek::{SigningKey, VerifyingKey};
use sha2::{Digest as _, Sha256};

use crate::calls::*;
use crate::cbor::Limits;
use crate::cbor::{Named, Wire};
use crate::shapes::*;
use crate::{Digest, Set, digest_of, encode, sign};

/// A deterministic stream of random values.
#[derive(Clone, Debug)]
pub struct Gen {
    state: u64,
}

impl Gen {
    /// The stream of `seed`. A seed of 0 is taken as 1, which xorshift needs.
    pub fn new(seed: u64) -> Gen {
        Gen { state: if seed == 0 { 1 } else { seed } }
    }

    /// The stream seeded from a type's name, so a type's random values are the same in every run.
    pub fn named(name: &str) -> Gen {
        let digest = Sha256::digest(name.as_bytes());
        let mut seed = [0u8; 8];
        seed.copy_from_slice(&digest[..8]);
        Gen::new(u64::from_be_bytes(seed))
    }

    /// The next 64 bits: xorshift64*.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number below `n`, which must not be 0.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// One of the values that sits at a boundary of the encoding, or a random one.
    pub fn edge_u64(&mut self) -> u64 {
        const EDGES: [u64; 12] = [
            0,
            1,
            23,
            24,
            255,
            256,
            65_535,
            65_536,
            u32::MAX as u64 - 1,
            u32::MAX as u64,
            u32::MAX as u64 + 1,
            u64::MAX,
        ];
        if self.below(2) == 0 {
            EDGES[self.below(EDGES.len() as u64) as usize]
        } else {
            self.next_u64() >> self.below(64)
        }
    }

    /// A length at a boundary of the head's width, or a small random one.
    pub fn len(&mut self, most: usize) -> usize {
        const EDGES: [usize; 6] = [0, 1, 22, 23, 24, 255];
        let edge = EDGES[self.below(EDGES.len() as u64) as usize];
        if edge <= most { edge } else { self.below(most as u64 + 1) as usize }
    }

    /// One value of `T`.
    pub fn value<T: Sample>(&mut self) -> T {
        T::sample(self)
    }

    /// A text string of printable ASCII, which keeps a JSON rendering simple to read.
    pub fn text(&mut self) -> String {
        const ALPHABET: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 -_./:";
        let len = self.len(40);
        let mut out = String::with_capacity(len);
        for _ in 0..len {
            out.push(ALPHABET[self.below(ALPHABET.len() as u64) as usize] as char);
        }
        out
    }

    /// Bytes at a length whose head changes width.
    pub fn bytes(&mut self) -> Vec<u8> {
        let len = self.len(300);
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

/// A type a random value can be made of.
pub trait Sample: Sized {
    /// One value of this type from `g`.
    fn sample(g: &mut Gen) -> Self;
}

macro_rules! sample_uint {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Sample for $ty {
                fn sample(g: &mut Gen) -> Self {
                    let v = g.edge_u64();
                    // A value in the type's range, keeping the edges where they fit.
                    let max = <$ty>::MAX as u64;
                    if v <= max { v as $ty } else { (v % (max + 1)) as $ty }
                }
            }
        )*
    };
}

sample_uint!(u8, u16, u32, u64);

impl Sample for bool {
    fn sample(g: &mut Gen) -> Self {
        g.below(2) == 0
    }
}

impl Sample for String {
    fn sample(g: &mut Gen) -> Self {
        g.text()
    }
}

impl<const N: usize> Sample for [u8; N] {
    fn sample(g: &mut Gen) -> Self {
        let mut out = [0u8; N];
        for b in &mut out {
            *b = g.next_u64() as u8;
        }
        out
    }
}

impl<T: Sample> Sample for Option<T> {
    fn sample(g: &mut Gen) -> Self {
        if g.below(2) == 0 { None } else { Some(T::sample(g)) }
    }
}

impl<T: Sample> Sample for Vec<T> {
    fn sample(g: &mut Gen) -> Self {
        let len = g.len(4);
        (0..len).map(|_| T::sample(g)).collect()
    }
}

impl<T: Sample + Wire> Sample for Set<T> {
    fn sample(g: &mut Gen) -> Self {
        // The input is deliberately unsorted and may repeat, so the set's own order is what a
        // test sees the encoder write.
        let len = g.len(4);
        let items: Vec<T> = (0..len).map(|_| T::sample(g)).collect();
        Set::new(items)
    }
}

impl<A: Sample, B: Sample> Sample for (A, B) {
    fn sample(g: &mut Gen) -> Self {
        (A::sample(g), B::sample(g))
    }
}

impl<A: Sample, B: Sample, C: Sample> Sample for (A, B, C) {
    fn sample(g: &mut Gen) -> Self {
        (A::sample(g), B::sample(g), C::sample(g))
    }
}

/// One value of a type, its canonical CBOR, its digest and its signature under a key derived from
/// the type's name.
///
/// The key is `sha256(name)`, so a golden vector carries no secret and every run of the test makes
/// the same signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vector {
    /// The type's name, as the domain separation spells it.
    pub name: &'static str,
    /// The value's canonical CBOR.
    pub cbor: Vec<u8>,
    /// `sha256("blaze/" ‖ name ‖ "/v1" ‖ 0x00 ‖ cbor)`.
    pub digest: Digest,
    /// Ed25519 over `"blaze/sig/" ‖ name ‖ "/v1" ‖ 0x00 ‖ ` the signed bytes.
    pub signature: [u8; 64],
    /// The bytes the signature is over, when they are not the whole `cbor`: a `CrateRecord`'s
    /// fields 0 to 5, which its field 6 is the signature of.
    pub signed: Option<Vec<u8>>,
    /// The public half of the key the signature is under.
    pub key: [u8; 32],
}

impl Vector {
    /// The vector of one value of `T`, and of that value's encoding, digest and signature.
    pub fn of<T: Wire + Named + Sample>(seed: u64) -> Vector {
        let mut g = Gen::new(seed);
        let value: T = T::sample(&mut g);
        Vector::of_value(&value)
    }

    /// The vector of `value`.
    pub fn of_value<T: Wire + Named>(value: &T) -> Vector {
        let key = key_for(T::NAME);
        Vector {
            name: T::NAME,
            cbor: encode(value),
            digest: digest_of(value),
            signature: sign(&key, value),
            signed: None,
            key: key.verifying_key().to_bytes(),
        }
    }

    /// The vector of one value of a type whose signature is a field of it: a `CrateRecord`.
    ///
    /// The value's own signature field is the key's signature over the six fields it signs, under
    /// the type's domain, so the golden vector carries a record a client can check rather than a
    /// record with a random field 6. The digest is still the whole record's, field 6 included,
    /// and `signed` is the bytes the signature is over.
    pub fn of_record<T: Wire + Named + Sample + SelfSigned>(seed: u64) -> Vector {
        let mut g = Gen::new(seed);
        let value: T = T::sample(&mut g);
        let key = key_for(T::NAME);
        let signed = value.signed_bytes();
        let value = value.with_signature(&key);
        Vector {
            name: T::NAME,
            cbor: encode(&value),
            digest: digest_of(&value),
            signature: value.own_signature().unwrap_or([0u8; 64]),
            signed: Some(signed),
            key: key.verifying_key().to_bytes(),
        }
    }

    /// The signing key a type's vector is signed with: `sha256(name)`.
    pub fn signing_key(&self) -> SigningKey {
        key_for(self.name)
    }

    /// The bytes the vector's signature is over: [`Vector::signed`] when the value carries its
    /// own signature, and the whole `cbor` otherwise.
    pub fn signed_bytes(&self) -> &[u8] {
        self.signed.as_deref().unwrap_or(&self.cbor)
    }

    /// Whether the signature is the one this vector's key makes over its signed bytes.
    pub fn signature_verifies(&self) -> bool {
        let key = self.signing_key().verifying_key();
        crate::verify_encoded_name(self.name, self.signed_bytes(), &key, &self.signature)
    }

    /// Whether the digest is the one this vector's name and CBOR make.
    pub fn digest_matches(&self) -> bool {
        self.digest == crate::digest_of_encoded_name(self.name, &self.cbor)
    }

    /// The vector as the text of a golden file.
    pub fn to_golden(&self) -> String {
        let mut out = String::new();
        out.push_str("type: ");
        out.push_str(self.name);
        out.push_str("\ncbor: ");
        out.push_str(&crate::json::hex(&self.cbor));
        if let Some(signed) = &self.signed {
            out.push_str("\nsigned: ");
            out.push_str(&crate::json::hex(signed));
        }
        out.push_str("\ndigest: ");
        out.push_str(&crate::json::hex(&self.digest));
        out.push_str("\nsignature: ");
        out.push_str(&crate::json::hex(&self.signature));
        out.push_str("\nkey: ");
        out.push_str(&crate::json::hex(&self.key));
        out.push('\n');
        out
    }

    /// The vector a golden file's text says.
    pub fn from_golden(text: &str) -> Option<Vector> {
        let mut name = None;
        let mut cbor = None;
        let mut signed = None;
        let mut digest = None;
        let mut signature = None;
        let mut key = None;
        for line in text.lines() {
            let (field, value) = line.split_once(": ")?;
            match field {
                "type" => name = Some(value),
                "cbor" => cbor = Some(unhex(value)?),
                "signed" => signed = Some(unhex(value)?),
                "digest" => digest = Some(unhex32(value)?),
                "signature" => signature = Some(unhex64(value)?),
                "key" => key = Some(unhex32(value)?),
                _ => return None,
            }
        }
        Some(Vector {
            name: leak(name?),
            cbor: cbor?,
            digest: digest?,
            signature: signature?,
            signed,
            key: key?,
        })
    }
}

/// A type whose signature is a field of the value itself, rather than the generic signature over
/// the whole value: a `CrateRecord`, whose field 6 is over fields 0 to 5.
///
/// [`Vector::of_record`] builds such a value's golden vector from these, so the vector carries a
/// signature a client checks the way the server made it.
pub trait SelfSigned: Sized {
    /// The bytes the value's own signature is over.
    fn signed_bytes(&self) -> Vec<u8>;
    /// This value with its signature field set to `key`'s signature over [`SelfSigned::signed_bytes`].
    fn with_signature(self, key: &SigningKey) -> Self;
    /// The value's own signature, as its field carries it.
    fn own_signature(&self) -> Option<[u8; 64]>;
}

fn key_for(name: &str) -> SigningKey {
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&Sha256::digest(name.as_bytes()));
    SigningKey::from_bytes(&seed)
}

/// A type's name as a `&'static str`, for a name read from a golden file.
fn leak(name: &str) -> &'static str {
    // The set of names is the schema's, a handful of dozen strings; a leak of one per golden file
    // read is the simplest way to keep a vector's name a `&'static str` as `Named` gives it.
    Box::leak(name.to_owned().into_boxed_str())
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

fn unhex32(text: &str) -> Option<[u8; 32]> {
    let bytes = unhex(text)?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

fn unhex64(text: &str) -> Option<[u8; 64]> {
    let bytes = unhex(text)?;
    <[u8; 64]>::try_from(bytes.as_slice()).ok()
}

/// One type of the schema: its name, a vector of one value of it, and a round trip of one value.
pub struct Entry {
    /// The type's name, as the domain separation spells it.
    pub name: &'static str,
    /// One value's vector, from a seed.
    pub vector: fn(u64) -> Vector,
    /// Decodes one value and encodes it again, refusing a byte that differs.
    pub round_trip: fn(u64) -> Result<(), String>,
    /// Reads these bytes as the type and writes that value again, refusing a byte that differs.
    pub reencode: fn(&[u8]) -> Result<Vec<u8>, String>,
}

/// Reads `cbor` as `T` and writes the value again; `Err` says how the two differ.
///
/// This is the strictest check a message gets: the validator's bytes pass first, then the schema,
/// and the bytes that come back must be the ones that went in.
pub fn reencode<T: Wire + Named>(cbor: &[u8]) -> Result<Vec<u8>, String> {
    let value: T =
        crate::validate_as(cbor, &crate::Limits::DOCUMENTED).map_err(|e| e.to_string())?;
    Ok(encode(&value))
}

/// Decodes one value of `T` from a seed and encodes it again; `Err` says how it differed.
pub fn round_trip<T: Wire + Named + Sample + PartialEq>(seed: u64) -> Result<(), String> {
    let mut g = Gen::new(seed);
    let value: T = T::sample(&mut g);
    let bytes = encode(&value);
    let limits = crate::Limits::DOCUMENTED;
    let decoded: T = crate::validate_as(&bytes, &limits).map_err(|e| e.to_string())?;
    let again = encode(&decoded);
    if again != bytes {
        return Err(format!("encode(decode(x)) is {} bytes and x is {}", again.len(), bytes.len()));
    }
    if decoded != value {
        return Err("decode(x) is another value".to_owned());
    }
    Ok(())
}

/// Every type of the schema, one entry each, so a test walks all of them and the golden vectors
/// are generated and checked from one list.
///
/// The list is the schema's types: "Data shapes", section 8's calls and the digest newtypes'
/// inputs. A type the doc adds is added here, and its golden file with it.
#[macro_export]
macro_rules! wire_types {
    ($($ty:ty),* $(,)?) => {
        /// Every type of the schema, one entry each.
        pub fn every() -> Vec<$crate::vectors::Entry> {
            vec![
                $($crate::vectors::Entry {
                    name: <$ty as $crate::Named>::NAME,
                    vector: (|seed| $crate::vectors::Vector::of::<$ty>(seed))
                        as fn(u64) -> $crate::vectors::Vector,
                    round_trip: (|seed| $crate::vectors::round_trip::<$ty>(seed))
                        as fn(u64) -> Result<(), String>,
                    reencode: (|cbor| $crate::vectors::reencode::<$ty>(cbor))
                        as fn(&[u8]) -> Result<Vec<u8>, String>,
                }),*
            ]
        }
    };
    // The same list with a type whose signature is its own field: a `CrateRecord`'s vector is
    // `Vector::of_record`, so its golden file carries the signature field 6 holds.
    (signed $signed:ty; $($ty:ty),* $(,)?) => {
        /// Every type of the schema, one entry each.
        pub fn every() -> Vec<$crate::vectors::Entry> {
            let mut entries = vec![
                $crate::vectors::Entry {
                    name: <$signed as $crate::Named>::NAME,
                    vector: (|seed| $crate::vectors::Vector::of_record::<$signed>(seed))
                        as fn(u64) -> $crate::vectors::Vector,
                    round_trip: (|seed| $crate::vectors::round_trip::<$signed>(seed))
                        as fn(u64) -> Result<(), String>,
                    reencode: (|cbor| $crate::vectors::reencode::<$signed>(cbor))
                        as fn(&[u8]) -> Result<Vec<u8>, String>,
                },
            ];
            entries.extend(vec![
                $($crate::vectors::Entry {
                    name: <$ty as $crate::Named>::NAME,
                    vector: (|seed| $crate::vectors::Vector::of::<$ty>(seed))
                        as fn(u64) -> $crate::vectors::Vector,
                    round_trip: (|seed| $crate::vectors::round_trip::<$ty>(seed))
                        as fn(u64) -> Result<(), String>,
                    reencode: (|cbor| $crate::vectors::reencode::<$ty>(cbor))
                        as fn(&[u8]) -> Result<Vec<u8>, String>,
                }),*
            ]);
            entries
        }
    };
}

wire_types! {
    signed CrateRecord;
    // "Data shapes"
    ExecPlatform,
    SourceRef,
    GitRef,
    RootRef,
    BuildSpec,
    SourcePath,
    ConfigEnvValue,
    UnitSpec,
    ProfileSpec,
    LocalPolicy,
    Command,
    RunSpec,
    Namespace,
    PrNamespace,
    PackRef,
    RootManifest,
    Capability,
    WorkKey,
    ItemKey,
    EdgeKey,
    Found,
    RootSet,
    Lease,
    Grant,
    ItemOutcome,
    EntryRef,
    Why,
    ItemResult,
    BatchResult,
    InputManifest,
    Observation,
    FileObservation,
    DirObservation,
    AbsentObservation,
    LinkObservation,
    TestVerdict,
    RuntimeBundle,
    Reservation,
    Stage,
    JobEvent,
    StageEvent,
    AssignedEvent,
    PublishedEvent,
    ProgramEvent,
    RefusedEvent,
    OutputEvent,
    DoneEvent,
    Outcome,
    ExecutorState,
    StoreIdentity,
    EntryKey,
    CrateKey,
    CrateKeyInput,
    FrontEndKey,
    FrontEndUnit,
    FrontEndKeyInput,
    ConfigKey,
    ConfigKeyInput,
    JobContent,
    JobContentInput,
    TestBaseKey,
    TestBaseKeyInput,
    TestKey,
    WorkName,
    ExecutionId,
    Attempt,
    // section 8
    HelloRequest,
    HelloAnswer,
    Limits,
    AuthRequest,
    SignedCapability,
    CacheLookupRequest,
    CacheLookupAnswer,
    CacheMissingRequest,
    CacheMissingAnswer,
    CachePublishRequest,
    CachePublishAnswer,
    ResultsLookupRequest,
    ResultsLookupAnswer,
    TestPass,
    ResultsPublishRequest,
    ResultsPublishAnswer,
    Ack,
    JobRequest,
    JobAnswer,
    EventItem,
    WorkersHelloRequest,
    WorkersHelloAnswer,
    WorkersNextRequest,
    WorkersNextAnswer,
    Assignment,
    WorkJoinRequest,
    WorkJoinAnswer,
    WorkLeaseRequest,
    WorkLeaseAnswer,
    WorkResultAnswer,
    WorkersEventsRequest,
    WorkersEventsAnswer,
    WireError,
}

/// The key a type's golden vector is signed with.
pub fn vector_key(name: &str) -> VerifyingKey {
    key_for(name).verifying_key()
}
