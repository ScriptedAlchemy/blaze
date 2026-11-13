//! The types of docs/distributed.md's "Data shapes", and the input its digest newtypes hash.
//!
//! Each struct is a CBOR map of the field numbers the doc's comments give, every key present, in
//! ascending order; each enum is the two-element array `[discriminant, payload]`. A newtype over a
//! digest is its inner value: a byte string of 32 bytes. A `Vec` marked **set** in the doc is a
//! [`Set`], sorted by the bytes of each item's encoding and without duplicates.

use crate::cbor::Wire;
use crate::{Digest, Named, Set};
use crate::{wire_enum, wire_struct};

wire_enum! {
    /// Where a local build looks before it compiles.
    LocalPolicy {
        Never => 0,
        Fallback => 1,
    }
}

wire_enum! {
    /// The namespaces of the cache, and what may write each.
    Namespace {
        Main => 0,
        Pr => 1 (PrNamespace),
    }
}

wire_struct! {
    /// The fields of `Namespace::Pr`: its pull request's number.
    PrNamespace {
        0 => n: u64,
    }
}

wire_struct! {
    /// One pack of a crate's record.
    PackRef {
        0 => pack: Digest,
        1 => size: u64,
        2 => keys: Digest,
        3 => packed: Option<(Digest, u64)>,
        4 => format: u32,
    }
}

wire_enum! {
    /// A function of distributed codegen, as every executor of an execution names it: the input of
    /// its `WorkName`.
    WorkKey {
        Item => 0 (ItemKey),
        Edge => 1 (EdgeKey),
    }
}

wire_struct! {
    /// The fields of `WorkKey::Item`: a function with a stable name.
    ItemKey {
        0 => unit: Digest,
        1 => stable: Digest,
        2 => env: Digest,
    }
}

wire_struct! {
    /// The fields of `WorkKey::Edge`: a request with no stable name.
    EdgeKey {
        0 => parent: WorkName,
        1 => ordinal: u32,
    }
}

wire_struct! {
    /// A name an executor found, and the digest of the complete local request it holds for it.
    Found {
        0 => name: WorkName,
        1 => tuple: Digest,
    }
}

wire_struct! {
    /// What an executor computed from its front end before it may take work.
    RootSet {
        0 => spec: Digest,
        1 => roots: Digest,
        2 => count: u64,
        3 => index: Digest,
        4 => inputs: Digest,
    }
}

wire_struct! {
    /// An executor's tenure over the work it holds in one execution.
    Lease {
        0 => execution: ExecutionId,
        1 => attempt: Attempt,
        2 => holder: String,
        3 => generation: u64,
    }
}

wire_struct! {
    /// Work given to a holder: a range of the sorted roots, and names granted to it.
    Grant {
        0 => lease: Lease,
        1 => roots: Set<(u64, u64)>,
        2 => names: Set<WorkName>,
    }
}

wire_enum! {
    /// What an executor's batch did with one name.
    ItemOutcome {
        Exported => 0 (EntryRef),
        FromStore => 1 (EntryRef),
        Failed => 2 (Why),
        Local => 3 (Why),
        Lost => 4 (Why),
    }
}

wire_struct! {
    /// The fields of `ItemOutcome::Exported` and `ItemOutcome::FromStore`.
    EntryRef {
        0 => entry: EntryKey,
    }
}

wire_struct! {
    /// The fields of `ItemOutcome::Failed`, `Local` and `Lost`.
    Why {
        0 => why: String,
    }
}

wire_struct! {
    /// One name's result in a batch.
    ItemResult {
        0 => name: WorkName,
        1 => outcome: ItemOutcome,
        2 => found: Set<Found>,
        3 => unnamed: Set<(u32, String)>,
    }
}

wire_struct! {
    /// One batch's results, committed at once.
    BatchResult {
        0 => lease: Lease,
        1 => seq: u64,
        2 => items: Vec<ItemResult>,
        3 => packs: Vec<PackRef>,
        4 => returned_roots: Set<(u64, u64)>,
        5 => returned_names: Set<WorkName>,
    }
}

wire_struct! {
    /// What admission holds for an executor, a job or a test shard.
    Reservation {
        0 => memory: u64,
        1 => cpus: u32,
        2 => disk: u64,
    }
}

wire_enum! {
    /// Stage timestamps, research 7.12.
    Stage {
        Queued => 0,
        Admitted => 1,
        Assigned => 2,
        Fetched => 3,
        Loaded => 4,
        Compiled => 5,
        Published => 6,
        Linked => 7,
        Reported => 8,
        Done => 9,
    }
}

wire_enum! {
    /// One event of an execution's stream.
    JobEvent {
        Stage => 0 (StageEvent),
        Assigned => 1 (AssignedEvent),
        Refused => 2 (RefusedEvent),
        Output => 3 (OutputEvent),
        Done => 4 (DoneEvent),
    }
}

wire_struct! {
    /// The fields of `JobEvent::Stage`.
    StageEvent {
        0 => stage: Stage,
        1 => at_ms: u64,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Assigned`.
    AssignedEvent {
        0 => executor: String,
        1 => attempt: Attempt,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Refused`.
    RefusedEvent {
        0 => function: String,
        1 => why: String,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Output`.
    OutputEvent {
        0 => stream: u8,
        1 => bytes: Vec<u8>,
    }
}

wire_struct! {
    /// The fields of `JobEvent::Done`.
    DoneEvent {
        0 => outcome: Outcome,
        1 => counters: Vec<(String, u64)>,
    }
}

wire_enum! {
    /// How an execution ended.
    Outcome {
        Passed => 0,
        Failed => 1,
        Refused => 2,
        Lost => 3,
        Cancelled => 4,
        TimedOut => 5,
    }
}

wire_struct! {
    /// The input of `JobContent`'s digest.
    JobContentInput {
        0 => unit: Digest,
        1 => platform: Digest,
        2 => namespace: Namespace,
    }
}

// The digest newtypes. Each is its inner value, a 32-byte string, and each names the type its
// digest is taken of, so the domain separation covers the name as well as the bytes.

macro_rules! digest_newtype {
    ($(#[$meta:meta])* $name:ident => $input:ty) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub Digest);

        impl Wire for $name {
            fn encode(&self, e: &mut crate::Encoder) {
                e.bytes(&self.0);
            }

            fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
                let mut out = [0u8; 32];
                out.copy_from_slice(r.bytes_of(32)?);
                Ok($name(out))
            }
        }

        impl Named for $name {
            const NAME: &'static str = stringify!($name);
        }

        impl $name {
            /// The digest of one input value, under this type's domain separation.
            pub fn of(value: &$input) -> $name {
                $name(crate::digest_of_encoded::<Self>(&crate::encode(value)))
            }
        }

        impl crate::vectors::Sample for $name {
            fn sample(g: &mut crate::vectors::Gen) -> Self {
                $name(<[u8; 32]>::sample(g))
            }
        }
    };
}

/// The caller's key of one compiled function, unchanged (cache.md): the store's
/// `sha256(identity ‖ key)`, not a digest this crate computes. [`digest_of`](crate::digest_of)
/// of one is its wire digest, for golden vectors and for a blob that names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryKey(pub Digest);

impl Wire for EntryKey {
    fn encode(&self, e: &mut crate::Encoder) {
        e.bytes(&self.0);
    }

    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        let mut out = [0u8; 32];
        out.copy_from_slice(r.bytes_of(32)?);
        Ok(EntryKey(out))
    }
}

impl Named for EntryKey {
    const NAME: &'static str = "EntryKey";
}

impl crate::vectors::Sample for EntryKey {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        EntryKey(<[u8; 32]>::sample(g))
    }
}

digest_newtype! {
    /// A job's content: what merges duplicates.
    JobContent => JobContentInput
}

digest_newtype! {
    /// A function of distributed codegen, as every executor of an execution names it: the digest of
    /// its `WorkKey`.
    WorkName => WorkKey
}

/// The server's, one per execution of a `JobContent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExecutionId(pub u64);

impl Wire for ExecutionId {
    fn encode(&self, e: &mut crate::Encoder) {
        e.uint(self.0);
    }

    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        Ok(ExecutionId(r.uint()?))
    }
}

impl Named for ExecutionId {
    const NAME: &'static str = "ExecutionId";
}

impl crate::vectors::Sample for ExecutionId {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        ExecutionId(u64::sample(g))
    }
}

/// Goes up when an execution is reassigned; fences events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Attempt(pub u32);

impl Wire for Attempt {
    fn encode(&self, e: &mut crate::Encoder) {
        e.uint(u64::from(self.0));
    }

    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        let at = r.position();
        let v = r.uint()?;
        u32::try_from(v).map(Attempt).map_err(|_| crate::Error {
            kind: crate::ErrorKind::UnexpectedType { expected: "an attempt in range" },
            at,
        })
    }
}

impl Named for Attempt {
    const NAME: &'static str = "Attempt";
}

impl crate::vectors::Sample for Attempt {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        Attempt(u32::sample(g))
    }
}
