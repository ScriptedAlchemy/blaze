//! Local transport version 2. This carrier version is independent of the selected global
//! wire codec and terminal contract version 2. Every body is one canonical CBOR value.
//!
//! Version 2 changed who names a session. Version 1 echoed a client-chosen session ID in
//! `Hello`, so refusing to revive an expired ID meant remembering every ID forever, which
//! exhausted capacity in ordinary use. Version 2 has the daemon issue a monotonic session
//! label and keeps the client's random nonce only to make a retried creation idempotent
//! while that session is live. An older peer is refused by the version mismatch, not
//! reinterpreted.

use blaze_wire::job_terminal_v2::SubscriberTerminalV2;
use blaze_wire::{
    Ack, Attempt, BatchResult, Bytes, Digest, EventItem, ExecutionId, JobContent, LocalPolicy,
    Reservation, Set, WireError, WorkJoinAnswer, WorkJoinRequest, WorkLeaseAnswer,
    WorkLeaseRequest, WorkResultAnswer, WorkersEventsAnswer, WorkersEventsRequest, wire_enum,
    wire_struct,
};

/// Carrier protocol version; not blaze-wire's global codec version.
pub const TRANSPORT_VERSION: u32 = 2;

wire_struct! {
    /// Creates a lifetime with `resume: None, epoch: None`, where `nonce` makes a retried
    /// creation idempotent while that lifetime is live, or resumes one by naming both its
    /// issued session and the daemon epoch. Naming only one of the two is malformed.
    LocalHello {
        0 => transport: u32,
        1 => codec: u32,
        2 => terminal: u32,
        3 => nonce: Digest,
        4 => epoch: Option<Digest>,
        5 => resume: Option<Digest>,
    }
}
wire_struct! {
    /// Negotiated versions, clock origin, lifetime and bounds. Absolute job deadlines use
    /// this daemon's monotonic milliseconds, not wall-clock Unix timestamps. `created` says
    /// this Hello issued a new lifetime, so a client holding a request for another session
    /// must not replay it here.
    LocalWelcome {
        0 => transport: u32,
        1 => codec: u32,
        2 => terminal: u32,
        3 => session: Digest,
        4 => epoch: Digest,
        5 => now_ms: u64,
        6 => expires_ms: u64,
        7 => max_frame: u32,
        8 => next_request: u64,
        9 => created: bool,
    }
}

wire_struct! {
    /// Execution identity without an attempt, used only for immutable terminal/resume lookup.
    Subscription {
        0 => content: JobContent,
        1 => execution: ExecutionId,
        2 => subscriber: u64,
    }
}
wire_struct! {
    /// A current attempt bound to one exact subscription.
    Binding {
        0 => subscription: Subscription,
        1 => attempt: Attempt,
    }
}
wire_struct! {
    /// Opaque submission bytes must decode in the negotiated codec; content is verified
    /// against their exact decoded JobContentInput before any submit transition.
    Submission {
        0 => content: JobContent,
        1 => request: Bytes,
        2 => need: Reservation,
    }
}
wire_struct! {
    /// Attach to a specific live content/execution, not whichever newer build is running.
    Subscribe {
        0 => content: JobContent,
        1 => execution: ExecutionId,
        2 => deadline_ms: u64,
        3 => policy: LocalPolicy,
    }
}
wire_struct! {
    /// Bounded event replay after sequence from. Events retain their original attempt.
    Events {
        0 => binding: Binding,
        1 => from: u64,
        2 => limit: u32,
    }
}
wire_struct! {
    /// Work sender id and content, checked against the named execution and attempt.
    Join {
        0 => content: JobContent,
        1 => executor: String,
        2 => request: WorkJoinRequest,
    }
}
wire_struct! {
    /// A lease request from an assigned executor.
    Lease {
        0 => content: JobContent,
        1 => executor: String,
        2 => request: WorkLeaseRequest,
    }
}
wire_struct! {
    /// A batch result, with content and sender binding outside its wire lease.
    BatchSubmission {
        0 => content: JobContent,
        1 => executor: String,
        2 => request: BatchResult,
    }
}
wire_struct! {
    /// Worker heartbeat/stage/verdict events; the core enforces full-batch authority.
    WorkerEvents {
        0 => content: JobContent,
        1 => executor: String,
        2 => request: WorkersEventsRequest,
    }
}
wire_struct! {
    /// Acknowledgment only of a delivered stop for this exact assignment.
    KillAck {
        0 => content: JobContent,
        1 => executor: String,
        2 => execution: ExecutionId,
        3 => attempt: Attempt,
        4 => alive: bool,
    }
}
wire_enum! {
    /// Requests after Hello. Reconnect uses Resume; Subscribe creates an additional interest.
    /// Release ends this lifetime deliberately, so a finished client returns its capacity
    /// instead of holding it until the fixed lifetime elapses.
    Command {
        Submit => 0 (Submission),
        Subscribe => 1 (Subscribe),
        Resume => 2 (Subscription),
        Events => 3 (Events),
        Cancel => 4 (Binding),
        Join => 5 (Join),
        Lease => 6 (Lease),
        Result => 7 (BatchSubmission),
        WorkerEvents => 8 (WorkerEvents),
        KillAck => 9 (KillAck),
        Terminal => 10 (Subscription),
        Release => 11 (Release),
    }
}
wire_struct! {
    /// Deliberately ends the calling session. No core transition is performed or reversed.
    Release {}
}
wire_struct! {
    /// At-most-once transition key within a bounded daemon/session lifetime.
    Request {
        0 => epoch: Digest,
        1 => session: Digest,
        2 => sequence: u64,
        3 => command: Command,
    }
}
wire_struct! {
    /// Paginated public stream; next is the last returned sequence (or the request cursor).
    EventPage {
        0 => binding: Binding,
        1 => events: Vec<EventItem>,
        2 => next: u64,
    }
}
wire_struct! {
    /// Pending is None. A present terminal is immutable and checked against the subscription.
    Terminal {
        0 => subscription: Subscription,
        1 => value: Option<SubscriberTerminalV2>,
    }
}
wire_struct! {
    /// Whether this exact pending stop was acknowledged, not whether an OS process was killed.
    KillAnswer {
        0 => accepted: bool,
    }
}
wire_enum! {
    /// The actual protocol response, including stable refusals.
    Answer {
        Binding => 0 (Binding),
        Events => 1 (EventPage),
        Ack => 2 (Ack),
        Join => 3 (WorkJoinAnswer),
        Lease => 4 (WorkLeaseAnswer),
        Result => 5 (WorkResultAnswer),
        WorkerEvents => 6 (WorkersEventsAnswer),
        KillAck => 7 (KillAnswer),
        Terminal => 8 (Terminal),
        Error => 9 (WireError),
    }
}
wire_struct! {
    /// Response echo binds a decoded answer to exactly the pending request.
    Response {
        0 => epoch: Digest,
        1 => session: Digest,
        2 => sequence: u64,
        3 => answer: Answer,
    }
}
wire_enum! {
    /// Frames accepted from a socket; Hello is mandatory before Request.
    ClientFrame {
        Hello => 0 (LocalHello),
        Request => 1 (Request),
    }
}
wire_enum! {
    /// Frames sent by the server. Protocol/framing errors close that connection.
    ServerFrame {
        Welcome => 0 (LocalWelcome),
        Response => 1 (Response),
        Error => 2 (WireError),
    }
}

/// Exact opaque submission envelope from a request in this pinned codec.
pub fn submission(request: &blaze_wire::JobRequest, need: Reservation) -> Submission {
    Submission {
        content: JobContent::of(&blaze_wire::JobContentInput {
            spec: blaze_wire::digest_of(&request.spec),
            namespace: request.namespace.clone(),
        }),
        request: Bytes::new(blaze_wire::encode(request)),
        need,
    }
}

/// A creation Hello. The nonce makes a retried creation idempotent while it is live.
pub fn hello(nonce: Digest) -> LocalHello {
    LocalHello {
        transport: TRANSPORT_VERSION,
        codec: blaze_wire::VERSION,
        terminal: blaze_wire::job_terminal_v2::VERSION,
        nonce,
        epoch: None,
        resume: None,
    }
}

/// A resume Hello: both the daemon epoch and the issued session label are required.
pub fn resume(session: Digest, epoch: Digest) -> LocalHello {
    LocalHello { resume: Some(session), epoch: Some(epoch), ..hello([0; 32]) }
}

/// Versions this implementation speaks, as a deterministic set for diagnostic rendering.
pub fn versions() -> Set<u32> {
    Set::new(vec![TRANSPORT_VERSION])
}
