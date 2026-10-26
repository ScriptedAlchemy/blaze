//! The shapes of section 8's calls: every endpoint's request and answer, `Hello`'s limits, the
//! errors, and the credentials' headers.
//!
//! Every request and answer is a map with every field present, its keys the numbers
//! `docs/distributed.md` gives in ascending order. A lookup's answer is in the order of its
//! request's keys, so a client reads it without a map.

use crate::cbor::Wire;
use crate::wire_struct;
use crate::cbor::Limits;
use crate::shapes::*;
use crate::{Digest, Named};

wire_struct! {
    /// `POST /v1/hello`'s request: the versions the client speaks and what it is.
    HelloRequest {
        0 => versions: crate::Set<u32>,
        1 => identity: StoreIdentity,
        2 => platform: ExecPlatform,
        3 => role: u8,
    }
}

wire_struct! {
    /// `POST /v1/hello`'s answer: the version the server picked and what it holds.
    HelloAnswer {
        0 => version: u32,
        1 => key: [u8; 32],
        2 => epochs: crate::Set<(Namespace, u64)>,
        3 => identities: crate::Set<StoreIdentity>,
        4 => limits: Limits,
    }
}

wire_struct! {
    /// `POST /v1/auth`'s request: the GitHub OIDC token, and nothing else.
    AuthRequest {
        0 => token: String,
    }
}

wire_struct! {
    /// `POST /v1/auth`'s answer, and the bytes of `Blaze-Capability`.
    SignedCapability {
        0 => capability: Capability,
        1 => signature: [u8; 64],
    }
}

wire_struct! {
    /// `POST /v1/cache/lookup`'s request.
    CacheLookupRequest {
        0 => namespaces: Vec<Namespace>,
        1 => keys: crate::Set<CrateKey>,
    }
}

wire_struct! {
    /// `POST /v1/cache/lookup`'s answer: per key, in the request's order, the newest record.
    CacheLookupAnswer {
        0 => records: Vec<Option<CrateRecord>>,
    }
}

wire_struct! {
    /// `POST /v1/cache/missing`'s request.
    CacheMissingRequest {
        0 => blobs: crate::Set<Digest>,
    }
}

wire_struct! {
    /// `POST /v1/cache/missing`'s answer.
    CacheMissingAnswer {
        0 => missing: crate::Set<Digest>,
    }
}

wire_struct! {
    /// `POST /v1/cache/publish`'s request.
    CachePublishRequest {
        0 => crate_key: CrateKey,
        1 => packs: crate::Set<PackRef>,
        2 => generation: u64,
        3 => source: SourceRef,
    }
}

wire_struct! {
    /// `POST /v1/cache/publish`'s answer.
    CachePublishAnswer {
        0 => record: CrateRecord,
    }
}

wire_struct! {
    /// `POST /v1/results/lookup`'s request.
    ResultsLookupRequest {
        0 => namespaces: Vec<Namespace>,
        1 => keys: crate::Set<TestBaseKey>,
    }
}

wire_struct! {
    /// `POST /v1/results/lookup`'s answer: per key, in the request's order, its passes.
    ResultsLookupAnswer {
        0 => results: Vec<Vec<TestPass>>,
    }
}

wire_struct! {
    /// One kept pass: the table's "pass with manifest digest".
    TestPass {
        0 => key: TestKey,
        1 => manifest: Digest,
        2 => duration_ns: u64,
    }
}

wire_struct! {
    /// `POST /v1/results/publish`'s request.
    ResultsPublishRequest {
        0 => verdicts: Vec<TestVerdict>,
    }
}

wire_struct! {
    /// `POST /v1/results/publish`'s answer.
    ResultsPublishAnswer {
        0 => kept: u64,
    }
}

/// The answer of every call that has nothing to say: `{}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ack {}

impl Wire for Ack {
    fn encode(&self, e: &mut crate::Encoder) {
        e.map(0);
    }

    fn decode(r: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        r.map_start()?;
        r.map_end()?;
        Ok(Ack {})
    }
}

impl Named for Ack {
    const NAME: &'static str = "Ack";
}

impl crate::vectors::Sample for Ack {
    fn sample(_g: &mut crate::vectors::Gen) -> Self {
        Ack {}
    }
}

wire_struct! {
    /// `POST /v1/jobs`'s request. Fields 2 to 4 are the subscriber's, not the content's.
    JobRequest {
        0 => spec: BuildSpec,
        1 => namespace: Namespace,
        2 => deadline_ms: u64,
        3 => local: LocalPolicy,
        4 => test_timeout_ms: Option<u64>,
    }
}

wire_struct! {
    /// `POST /v1/jobs`'s answer.
    JobAnswer {
        0 => content: JobContent,
        1 => execution: ExecutionId,
        2 => subscriber: u64,
    }
}

wire_struct! {
    /// One item of an event stream, each behind its four-byte big-endian length.
    EventItem {
        0 => sequence: u64,
        1 => attempt: Attempt,
        2 => event: JobEvent,
    }
}

wire_struct! {
    /// `POST /v1/workers/hello`'s request.
    WorkersHelloRequest {
        0 => agent: String,
        1 => capacity: Reservation,
        2 => executors: Vec<ExecutorState>,
    }
}

wire_struct! {
    /// `POST /v1/workers/hello`'s answer.
    WorkersHelloAnswer {
        0 => accepted: bool,
    }
}

wire_struct! {
    /// `POST /v1/workers/next`'s request.
    WorkersNextRequest {
        0 => free: Vec<Reservation>,
        1 => front_ends: crate::Set<FrontEndKey>,
    }
}

wire_struct! {
    /// `POST /v1/workers/next`'s answer: an assignment, or `null` for nothing.
    WorkersNextAnswer {
        0 => assignment: Option<Assignment>,
    }
}

wire_struct! {
    /// One job assigned to an executor.
    Assignment {
        0 => execution: ExecutionId,
        1 => attempt: Attempt,
        2 => spec: BuildSpec,
        3 => capability: Capability,
        4 => role: u8,
    }
}

wire_struct! {
    /// `POST /v1/work/join`'s request.
    WorkJoinRequest {
        0 => execution: ExecutionId,
        1 => attempt: Attempt,
        2 => roots: RootSet,
        3 => rejected: crate::Set<WorkName>,
    }
}

wire_struct! {
    /// `POST /v1/work/join`'s answer.
    WorkJoinAnswer {
        0 => roots: RootSet,
        1 => fixed: bool,
    }
}

wire_struct! {
    /// `POST /v1/work/lease`'s request.
    WorkLeaseRequest {
        0 => execution: ExecutionId,
        1 => attempt: Attempt,
    }
}

wire_struct! {
    /// `POST /v1/work/lease`'s answer: a grant, or `null` for nothing.
    WorkLeaseAnswer {
        0 => grant: Option<Grant>,
    }
}

wire_struct! {
    /// `POST /v1/work/result`'s answer.
    WorkResultAnswer {
        0 => granted: crate::Set<WorkName>,
        1 => conflicts: crate::Set<WorkName>,
    }
}

wire_struct! {
    /// `POST /v1/workers/events`'s request.
    WorkersEventsRequest {
        0 => execution: ExecutionId,
        1 => attempt: Attempt,
        2 => events: Vec<JobEvent>,
    }
}

wire_struct! {
    /// `POST /v1/workers/events`'s answer.
    WorkersEventsAnswer {
        0 => verdict: u8,
    }
}

impl WorkersEventsAnswer {
    /// The holder keeps its lease.
    pub const CONTINUE: WorkersEventsAnswer = WorkersEventsAnswer { verdict: 0 };
    /// The holder stops and kills its process group.
    pub const CANCEL: WorkersEventsAnswer = WorkersEventsAnswer { verdict: 1 };
}

wire_struct! {
    /// The one shape an error takes on every call.
    WireError {
        0 => code: u8,
        1 => message: String,
        2 => retry_after_ms: Option<u64>,
    }
}

impl WireError {
    /// The error of one code, with its message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> WireError {
        WireError { code: code as u8, message: message.into(), retry_after_ms: None }
    }

    /// The error with the milliseconds a client waits.
    pub fn retry_after(self, ms: u64) -> WireError {
        WireError { retry_after_ms: Some(ms), ..self }
    }

    /// The code, when it is one section 8 names.
    pub fn code(&self) -> Option<ErrorCode> {
        ErrorCode::from_u8(self.code)
    }
}

/// The codes of section 8's errors, with the HTTP status each is answered with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ErrorCode {
    /// 1, 400: the message is not the schema's, or the validator refused it.
    Malformed = 1,
    /// 2, 400: the version is not one the server speaks.
    Version = 2,
    /// 3, 401: no capability, or one that does not verify.
    Unauthenticated = 3,
    /// 4, 403: a capability that does not allow the operation.
    NotAllowed = 4,
    /// 5, 404: no such record, blob or execution.
    NotFound = 5,
    /// 6, 409: a generation or an attempt moved.
    Conflict = 6,
    /// 7, 413: past a limit.
    TooLarge = 7,
    /// 8, 429: busy.
    Busy = 8,
    /// 9, 422: blaze cannot build it, with the reason.
    Refused = 9,
    /// 10, 500: the server's fault.
    Internal = 10,
}

impl ErrorCode {
    /// The code of a byte, when it is one section 8 names.
    pub fn from_u8(code: u8) -> Option<ErrorCode> {
        Some(match code {
            1 => ErrorCode::Malformed,
            2 => ErrorCode::Version,
            3 => ErrorCode::Unauthenticated,
            4 => ErrorCode::NotAllowed,
            5 => ErrorCode::NotFound,
            6 => ErrorCode::Conflict,
            7 => ErrorCode::TooLarge,
            8 => ErrorCode::Busy,
            9 => ErrorCode::Refused,
            10 => ErrorCode::Internal,
            _ => return None,
        })
    }

    /// The HTTP status the code's class is answered with.
    pub fn status(self) -> u16 {
        match self {
            ErrorCode::Malformed | ErrorCode::Version => 400,
            ErrorCode::Unauthenticated => 401,
            ErrorCode::NotAllowed => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict => 409,
            ErrorCode::TooLarge => 413,
            ErrorCode::Refused => 422,
            ErrorCode::Busy => 429,
            ErrorCode::Internal => 500,
        }
    }
}

/// The bits of a `Capability`'s `ops`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum Op {
    /// 0: a lookup of a crate's record.
    Lookup = 1 << 0,
    /// 1: a fetch of a blob.
    Fetch = 1 << 1,
    /// 2: a publish of packs and a record.
    Publish = 1 << 2,
    /// 3: an execution's events.
    Events = 1 << 3,
    /// 4: a cancel of an execution.
    Cancel = 1 << 4,
    /// 5: a root manifest's write.
    Roots = 1 << 5,
    /// 6: a namespace's deletion.
    DeleteNamespace = 1 << 6,
    /// 7: work: join, lease and result.
    Work = 1 << 7,
}

impl Op {
    /// Every operation, in bit order.
    pub const ALL: [Op; 8] = [
        Op::Lookup,
        Op::Fetch,
        Op::Publish,
        Op::Events,
        Op::Cancel,
        Op::Roots,
        Op::DeleteNamespace,
        Op::Work,
    ];

    /// Whether `ops` has this bit.
    pub fn in_bits(self, ops: u32) -> bool {
        ops & (self as u32) != 0
    }
}
