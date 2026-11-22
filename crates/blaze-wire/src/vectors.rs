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

use sha2::{Digest as _, Sha256};
use std::alloc::{Allocator, Global};
use std::io::{self, Write};

use crate::calls::*;
use crate::cbor::{Named, Wire};
use crate::shapes::*;
use crate::{Digest, Set, encode};

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

    /// The bytes of printable ASCII text, which keeps a JSON rendering simple to read. The caller chooses where to
    /// keep them; the iterator reports its exact length before it consumes the random stream.
    pub fn text(&mut self) -> impl ExactSizeIterator<Item = u8> + '_ {
        const ALPHABET: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 -_./:";
        let len = self.len(40);
        (0..len).map(|_| ALPHABET[self.below(ALPHABET.len() as u64) as usize])
    }

    /// Bytes at a length whose head changes width.
    pub fn bytes<A: Allocator>(&mut self, alloc: A) -> Vec<u8, A> {
        let len = self.len(300);
        let mut out = Vec::with_capacity_in(len, alloc);
        out.extend((0..len).map(|_| self.next_u64() as u8));
        out
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
                    if v <= max { v as $ty } else { (v % max.saturating_add(1)) as $ty }
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
        let text = g.text();
        let mut bytes = Vec::with_capacity_in(text.len(), Global);
        bytes.extend(text);
        String::from_utf8(bytes).expect("the sample alphabet is ASCII")
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

/// One value of a type, and what the wire says of it: what a golden file holds.
#[derive(Clone, Debug)]
pub struct Vector {
    /// The type's name.
    pub name: &'static str,
    /// The value's encoding.
    pub cbor: Vec<u8>,
    /// Its digest under the type's domain.
    pub digest: Digest,
}

impl Vector {
    /// The vector of one value of `T`, and of that value's encoding and digest.
    pub fn of<T: Wire + Named + Sample>(seed: u64) -> Vector {
        let mut g = Gen::new(seed);
        let value: T = T::sample(&mut g);
        Vector::of_value(&value)
    }

    /// The vector of `value`.
    pub fn of_value<T: Wire + Named>(value: &T) -> Vector {
        let cbor = encode(value, Global);
        let digest = crate::digest_of_encoded::<T>(&cbor);
        Vector { name: T::NAME, cbor, digest }
    }

    /// Whether the digest is the one this vector's name and CBOR make.
    pub fn digest_matches(&self) -> bool {
        self.digest == crate::digest_of_encoded_name(self.name, &self.cbor)
    }

    /// The vector as the text of a golden file.
    pub fn to_golden(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        write!(
            out,
            "type: {}\ncbor: {}\ndigest: {}\n",
            self.name,
            crate::json::hex(&self.cbor),
            crate::json::hex(&self.digest)
        )
    }

    /// The vector a golden file's text says.
    pub fn from_golden(text: &str) -> Option<Vector> {
        let (mut name, mut cbor, mut digest) = (None, None, None);
        for line in text.lines() {
            let (field, value) = line.split_once(": ")?;
            match field {
                "type" => name = Some(value),
                "cbor" => cbor = Some(unhex(value, Global)?),
                "digest" => digest = Some(unhex32(value)?),
                _ => return None,
            }
        }
        Some(Vector { name: leak(name?), cbor: cbor?, digest: digest? })
    }
}

/// A type's name as a `&'static str`, for a name read from a golden file.
fn leak(name: &str) -> &'static str {
    // The set of names is the schema's, a handful of dozen strings; a leak of one per golden file
    // read is the simplest way to keep a vector's name a `&'static str` as `Named` gives it.
    Box::leak(name.to_owned().into_boxed_str())
}

fn unhex<A: Allocator>(text: &str, alloc: A) -> Option<Vec<u8, A>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity_in(text.len() / 2, alloc);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        out.push(unhex_byte([pair[0], pair[1]])?);
    }
    Some(out)
}

fn unhex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *byte = unhex_byte([pair[0], pair[1]])?;
    }
    Some(out)
}

fn unhex_byte([hi, lo]: [u8; 2]) -> Option<u8> {
    Some(((hi as char).to_digit(16)? * 16 + (lo as char).to_digit(16)?) as u8)
}

/// The codec property violated by a seeded schema round trip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundTripErrorKind {
    /// The type's encoder produced bytes the strict decoder refused.
    Decode(crate::Error),
    /// Reencoding the decoded value changed its canonical bytes.
    Bytes {
        /// The original encoding's byte length.
        original: usize,
        /// The decoded value's reencoding length.
        reencoded: usize,
    },
    /// Decoding changed the value even though the bytes may be equal.
    Value,
}

/// A seeded schema round trip failed, with the type and cause retained separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundTripError {
    /// The schema type whose codec was exercised.
    pub type_name: &'static str,
    /// The generator seed that reproduces the failure.
    pub seed: u64,
    /// The codec property that failed.
    pub kind: RoundTripErrorKind,
}

impl std::fmt::Display for RoundTripError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at seed {}: ", self.type_name, self.seed)?;
        match &self.kind {
            RoundTripErrorKind::Decode(source) => write!(f, "decoding the encoded value: {source}"),
            RoundTripErrorKind::Bytes { original, reencoded } => {
                write!(f, "encode(decode(x)) is {reencoded} bytes and x is {original}")
            }
            RoundTripErrorKind::Value => f.write_str("decode(x) is another value"),
        }
    }
}

impl std::error::Error for RoundTripError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            RoundTripErrorKind::Decode(source) => Some(source),
            _ => None,
        }
    }
}

/// One schema type and the real codec controls run for it.
pub struct Entry {
    /// The type's name, as the domain separation spells it.
    pub name: &'static str,
    /// One value's vector, from a seed.
    pub vector: fn(u64) -> Vector,
    /// Decodes one value and encodes it again, refusing a byte that differs.
    pub round_trip: fn(u64) -> Result<(), RoundTripError>,
    /// Reads these bytes as the type and writes that value again, refusing a byte that differs.
    pub reencode: fn(&[u8]) -> Result<Vec<u8>, crate::Error>,
}

/// Reads `cbor` as `T` and writes the value again; `Err` says how the two differ.
///
/// This is the strictest check a message gets: the validator's bytes pass first, then the schema,
/// and the bytes that come back must be the ones that went in.
pub fn reencode<T: Wire + Named, A: Allocator + Clone>(cbor: &[u8], alloc: A) -> Result<Vec<u8, A>, crate::Error> {
    let value: T = crate::validate_as(cbor, &crate::Limits::DOCUMENTED)?;
    Ok(encode(&value, alloc))
}

/// Decodes one value of `T` from a seed and encodes it again; `Err` says how it differed.
pub fn round_trip<T: Wire + Named + Sample + PartialEq, A: Allocator + Clone>(seed: u64, alloc: A) -> Result<(), RoundTripError> {
    let mut g = Gen::new(seed);
    let value: T = T::sample(&mut g);
    let bytes = encode(&value, alloc.clone());
    let limits = crate::Limits::DOCUMENTED;
    let refused = |kind| RoundTripError { type_name: T::NAME, seed, kind };
    let decoded: T = crate::validate_as(&bytes, &limits)
        .map_err(|source| refused(RoundTripErrorKind::Decode(source)))?;
    let again = encode(&decoded, alloc);
    if again != bytes {
        return Err(refused(RoundTripErrorKind::Bytes {
            original: bytes.len(), reencoded: again.len(),
        }));
    }
    if decoded != value {
        return Err(refused(RoundTripErrorKind::Value));
    }
    Ok(())
}

/// Every type of the schema, one entry each, so a test walks all of them and the golden vectors
/// are generated and checked from one list.
///
/// The list is the schema's types: "Data shapes", the calls and the digest newtypes'
/// inputs. A type the doc adds is added here, and its golden file with it.
#[macro_export]
macro_rules! wire_types {
    ($($ty:ty),* $(,)?) => {
        /// Every type of the schema, one entry each.
        pub fn every() -> [$crate::vectors::Entry; $crate::wire_types!(@count $($ty),*)] {
            [
                $($crate::vectors::Entry {
                    name: <$ty as $crate::Named>::NAME,
                    vector: (|seed| $crate::vectors::Vector::of::<$ty>(seed))
                        as fn(u64) -> $crate::vectors::Vector,
                    round_trip: (|seed| $crate::vectors::round_trip::<$ty, _>(seed, ::std::alloc::Global))
                        as fn(u64) -> Result<(), $crate::vectors::RoundTripError>,
                    reencode: (|cbor| $crate::vectors::reencode::<$ty, _>(cbor, ::std::alloc::Global))
                        as fn(&[u8]) -> Result<Vec<u8>, $crate::Error>,
                }),*
            ]
        }
    };
    (@count $($ty:ty),*) => {
        <[()]>::len(&[$($crate::wire_types!(@one $ty)),*])
    };
    (@one $ty:ty) => { () };
}

wire_types! {
    // "Data shapes"
    LocalPolicy,
    Namespace,
    PrNamespace,
    PackRef,
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
    Reservation,
    Stage,
    JobEvent,
    StageEvent,
    AssignedEvent,
    RefusedEvent,
    OutputEvent,
    DoneEvent,
    Outcome,
    EntryKey,
    JobContent,
    JobContentInput,
    WorkName,
    ExecutionId,
    Attempt,
    // the calls
    Ack,
    JobRequest,
    JobAnswer,
    EventItem,
    WorkJoinRequest,
    WorkJoinAnswer,
    WorkLeaseRequest,
    WorkLeaseAnswer,
    WorkResultAnswer,
    WorkersEventsRequest,
    WorkersEventsAnswer,
    WireError,
}
