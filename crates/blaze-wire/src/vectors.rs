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

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::calls::*;
use crate::cbor::{Named, Wire};
use crate::shapes::*;
use crate::{Digest, Set, WireAlloc, WireString, WireVec, encode};

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
        let digest = blaze_sha256::digest(name.as_bytes());
        let seed: [u8; 8] = digest.get(..8).and_then(|s| s.try_into().ok()).unwrap_or([0; 8]);
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
        if n == 0 { 0 } else { self.next_u64() % n }
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
            4_294_967_294,
            4_294_967_295,
            4_294_967_296,
            u64::MAX,
        ];
        if self.below(2) == 0 {
            let idx = usize::try_from(self.below(12)).unwrap_or(0);
            EDGES.get(idx).copied().unwrap_or(0)
        } else {
            self.next_u64() >> self.below(64)
        }
    }

    /// A length at a boundary of the head's width, or a small random one.
    pub fn len(&mut self, most: usize) -> usize {
        const EDGES: [usize; 6] = [0, 1, 22, 23, 24, 255];
        let idx = usize::try_from(self.below(6)).unwrap_or(0);
        let edge = EDGES.get(idx).copied().unwrap_or(0);
        if edge <= most {
            edge
        } else {
            let bound = u64::try_from(most).unwrap_or(u64::MAX).saturating_add(1);
            usize::try_from(self.below(bound)).unwrap_or(0)
        }
    }

    /// One value of `T`.
    pub fn value<T: Sample>(&mut self, alloc: WireAlloc) -> T {
        T::sample(self, alloc)
    }

    /// The bytes of printable ASCII text, which keeps a JSON rendering simple to read.
    pub fn text(&mut self) -> impl ExactSizeIterator<Item = u8> + '_ {
        const ALPHABET: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 -_./:";
        let alpha_len = u64::try_from(ALPHABET.len()).unwrap_or(1);
        let len = self.len(40);
        (0..len).map(move |_| {
            let idx = usize::try_from(self.below(alpha_len)).unwrap_or(0);
            ALPHABET.get(idx).copied().unwrap_or(b'a')
        })
    }

    /// Bytes at a length whose head changes width.
    pub fn bytes<A: Allocator>(&mut self, alloc: A) -> Vec<u8, A> {
        let len = self.len(300);
        let mut out = Vec::with_capacity_in(len, alloc);
        for _ in 0..len {
            out.push(u8::try_from(self.next_u64() & 0xff).unwrap_or(0));
        }
        out
    }
}

/// A type a random value can be made of.
pub trait Sample: Sized {
    /// One value of this type from `g`.
    fn sample(g: &mut Gen, alloc: WireAlloc) -> Self;
}

macro_rules! sample_uint {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Sample for $ty {
                fn sample(g: &mut Gen, _alloc: WireAlloc) -> Self {
                    let v = g.edge_u64();
                    let max = u64::from(<$ty>::MAX);
                    let narrowed = if v <= max { v } else { v % max.saturating_add(1) };
                    <$ty>::try_from(narrowed).unwrap_or(<$ty>::MAX)
                }
            }
        )*
    };
}

sample_uint!(u8, u16, u32, u64);

impl Sample for bool {
    fn sample(g: &mut Gen, _alloc: WireAlloc) -> Self {
        g.below(2) == 0
    }
}

impl Sample for WireString {
    fn sample(g: &mut Gen, alloc: WireAlloc) -> Self {
        let text = g.text();
        let mut bytes = Vec::with_capacity_in(text.len(), alloc);
        bytes.extend(text);
        WireString::from_utf8(bytes).unwrap_or_else(|_| WireString::new_in(alloc))
    }
}

impl<const N: usize> Sample for [u8; N] {
    fn sample(g: &mut Gen, _alloc: WireAlloc) -> Self {
        let mut out = [0u8; N];
        for b in &mut out {
            *b = u8::try_from(g.next_u64() & 0xff).unwrap_or(0);
        }
        out
    }
}

impl<T: Sample> Sample for Option<T> {
    fn sample(g: &mut Gen, alloc: WireAlloc) -> Self {
        if g.below(2) == 0 { None } else { Some(T::sample(g, alloc)) }
    }
}

impl<T: Sample> Sample for WireVec<T> {
    fn sample(g: &mut Gen, alloc: WireAlloc) -> Self {
        let len = g.len(4);
        let mut out = Vec::with_capacity_in(len, alloc);
        for _ in 0..len {
            out.push(T::sample(g, alloc));
        }
        out
    }
}

impl<T: Sample + Wire> Sample for Set<T> {
    fn sample(g: &mut Gen, alloc: WireAlloc) -> Self {
        let len = g.len(4);
        let mut items = Vec::with_capacity_in(len, alloc);
        for _ in 0..len {
            items.push(T::sample(g, alloc));
        }
        Set::new(items)
    }
}

impl<A: Sample, B: Sample> Sample for (A, B) {
    fn sample(g: &mut Gen, alloc: WireAlloc) -> Self {
        (A::sample(g, alloc), B::sample(g, alloc))
    }
}

impl<A: Sample, B: Sample, C: Sample> Sample for (A, B, C) {
    fn sample(g: &mut Gen, alloc: WireAlloc) -> Self {
        (A::sample(g, alloc), B::sample(g, alloc), C::sample(g, alloc))
    }
}

/// One value of a type, and what the wire says of it: what a golden file holds.
#[derive(Clone, Debug)]
pub struct Vector {
    /// The type's name.
    pub name: &'static str,
    /// The value's encoding.
    pub cbor: WireVec<u8>,
    /// Its digest under the type's domain.
    pub digest: Digest,
}

impl Vector {
    /// The vector of one value of `T`, and of that value's encoding and digest.
    pub fn of<T: Wire + Named + Sample>(seed: u64, alloc: WireAlloc) -> Vector {
        let mut g = Gen::new(seed);
        let value: T = T::sample(&mut g, alloc);
        Vector::of_value(&value, alloc)
    }

    /// The vector of `value`.
    pub fn of_value<T: Wire + Named>(value: &T, alloc: WireAlloc) -> Vector {
        let cbor = encode(value, alloc);
        let digest = crate::digest_of_encoded::<T>(&cbor);
        Vector { name: T::NAME, cbor, digest }
    }

    /// Whether the digest is the one this vector's name and CBOR make.
    pub fn digest_matches(&self) -> bool {
        self.digest == crate::digest_of_encoded_name(self.name, &self.cbor)
    }

    /// The vector as the text of a golden file.
    pub fn to_golden(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(
            out,
            "type: {}\ncbor: {}\ndigest: {}\n",
            self.name,
            crate::json::hex(&self.cbor),
            crate::json::hex(&self.digest)
        )
    }

    /// The vector a golden file's text says.
    pub fn from_golden(text: &str, alloc: WireAlloc) -> Option<Vector> {
        let (mut name, mut cbor, mut digest) = (None, None, None);
        for line in text.lines() {
            let (field, value) = line.split_once(": ")?;
            match field {
                "type" => name = Some(value),
                "cbor" => cbor = Some(unhex(value, alloc)?),
                "digest" => digest = Some(unhex32(value)?),
                _ => return None,
            }
        }
        let static_name = static_type_name(name?)?;
        Some(Vector { name: static_name, cbor: cbor?, digest: digest? })
    }
}

fn static_type_name(name: &str) -> Option<&'static str> {
    for entry in every() {
        if entry.name == name {
            return Some(entry.name);
        }
    }
    None
}

fn unhex<A: Allocator>(text: &str, alloc: A) -> Option<Vec<u8, A>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity_in(text.len() / 2, alloc);
    for pair in text.as_bytes().chunks_exact(2) {
        let pair: [u8; 2] = pair.try_into().ok()?;
        out.push(unhex_byte(pair)?);
    }
    Some(out)
}

fn unhex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        let pair: [u8; 2] = pair.try_into().ok()?;
        *byte = unhex_byte(pair)?;
    }
    Some(out)
}

fn unhex_byte([hi, lo]: [u8; 2]) -> Option<u8> {
    let hi_v = char::from(hi).to_digit(16)?;
    let lo_v = char::from(lo).to_digit(16)?;
    u8::try_from(hi_v * 16 + lo_v).ok()
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

impl fmt::Display for RoundTripError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

impl core::error::Error for RoundTripError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
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
    pub vector: fn(u64, WireAlloc) -> Vector,
    /// Decodes one value and encodes it again, refusing a byte that differs.
    pub round_trip: fn(u64, WireAlloc) -> Result<(), RoundTripError>,
    /// Reads these bytes as the type and writes that value again, refusing a byte that differs.
    pub reencode: fn(&[u8], WireAlloc) -> Result<WireVec<u8>, crate::Error>,
}

/// Reads `cbor` as `T` and writes the value again; `Err` says how the two differ.
pub fn reencode<T: Wire + Named, A: Allocator + Clone>(
    cbor: &[u8],
    wire_alloc: WireAlloc,
    out_alloc: A,
) -> Result<Vec<u8, A>, crate::Error> {
    let value: T = crate::validate_as(cbor, &crate::Limits::DOCUMENTED, wire_alloc)?;
    Ok(encode(&value, out_alloc))
}

/// Decodes one value of `T` from a seed and encodes it again; `Err` says how it differed.
pub fn round_trip<T: Wire + Named + Sample + PartialEq>(
    seed: u64,
    alloc: WireAlloc,
) -> Result<(), RoundTripError> {
    let mut g = Gen::new(seed);
    let value: T = T::sample(&mut g, alloc);
    let bytes = encode(&value, alloc);
    let limits = crate::Limits::DOCUMENTED;
    let refused = |kind| RoundTripError { type_name: T::NAME, seed, kind };
    let decoded: T = crate::validate_as(&bytes, &limits, alloc)
        .map_err(|source| refused(RoundTripErrorKind::Decode(source)))?;
    let again = encode(&decoded, alloc);
    if again.as_slice() != bytes.as_slice() {
        return Err(refused(RoundTripErrorKind::Bytes {
            original: bytes.len(),
            reencoded: again.len(),
        }));
    }
    if decoded != value {
        return Err(refused(RoundTripErrorKind::Value));
    }
    Ok(())
}

/// Every type of the schema, one entry each, so a test walks all of them and the golden vectors
/// are generated and checked from one list.
#[macro_export]
macro_rules! wire_types {
    ($($ty:ty),* $(,)?) => {
        /// Every type of the schema, one entry each.
        pub fn every() -> [$crate::vectors::Entry; <[()]>::len(&[$({ let _ = stringify!($ty); () }),*])] {
            [
                $($crate::vectors::Entry {
                    name: <$ty as $crate::Named>::NAME,
                    vector: (|seed, alloc| $crate::vectors::Vector::of::<$ty>(seed, alloc))
                        as fn(u64, $crate::WireAlloc) -> $crate::vectors::Vector,
                    round_trip: (|seed, alloc| $crate::vectors::round_trip::<$ty>(seed, alloc))
                        as fn(u64, $crate::WireAlloc) -> Result<(), $crate::vectors::RoundTripError>,
                    reencode: (|cbor, alloc| $crate::vectors::reencode::<$ty, _>(cbor, alloc, alloc))
                        as fn(&[u8], $crate::WireAlloc) -> Result<$crate::WireVec<u8>, $crate::Error>,
                }),*
            ]
        }
    };
}

wire_types! {
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
