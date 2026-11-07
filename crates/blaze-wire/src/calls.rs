//! The shapes of a distributed build's calls: a job's request and answer, the work calls of its
//! executors, and the errors.
//!
//! Every request and answer is a map with every field present, its keys the numbers
//! `docs/distributed.md` gives in ascending order.

use crate::cbor::Wire;
use crate::shapes::*;
use crate::wire_struct;
use crate::{Digest, Named};

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
    /// A job: one unit of a build on one platform. Fields 0 to 2 are its content, what equal
    /// jobs share an execution by. `unit` is what the unit's walk computed from its sources,
    /// settings and toolchain (`blaze_codegen::driver::WorkSet::digest`). `source` says which
    /// build of a pull request this is: a newer one supersedes the older one's executions that
    /// nothing else waits for. Fields 3 to 5 are the subscriber's.
    JobRequest {
        0 => unit: Digest,
        1 => platform: Digest,
        2 => namespace: Namespace,
        3 => source: Digest,
        4 => deadline_ms: u64,
        5 => local: LocalPolicy,
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

    /// The code, when it is one this schema names.
    pub fn code(&self) -> Option<ErrorCode> {
        ErrorCode::from_u8(self.code)
    }
}

/// The codes of the errors, with the HTTP status each is answered with.
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
    /// The code of a byte, when it is one this schema names.
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
