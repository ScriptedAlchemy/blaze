//! Serialized routing, retry journal and retention for the local IO driver.

use std::collections::BTreeMap;

use blaze_dist::Refusal;
use blaze_dist::jobs::{AssignmentToken, Effect, ExecState, Jobs};
use blaze_wire::{
    Ack, Digest, ErrorCode, ExecutionId, JobRequest, Limits, WireError, decode, encode,
};

use crate::protocol::*;

/// Finite carrier resources. Exhaustion is explicit Busy, never eviction of a retry record.
#[derive(Clone, Debug)]
pub struct Config {
    /// Largest request or response body, before allocation.
    pub max_frame: u32,
    /// Concurrent accepted sockets.
    pub max_connections: usize,
    /// Concurrent live sessions. An expired session is removed, so its capacity returns;
    /// this is not a lifetime quota of session identities.
    pub max_sessions: usize,
    /// Fixed session lifetime in monotonic milliseconds. Hello does not extend it; a client
    /// that outlives it creates a new session and resumes its subscriptions explicitly.
    pub session_ms: u64,
    /// Maximum successful request IDs per session.
    pub max_requests: usize,
    /// Total request/response bytes retained per session.
    pub replay_bytes: usize,
    /// Executions retained, including those awaiting stop acknowledgments.
    pub max_executions: usize,
    /// Maximum public event count per execution before new worker reports get Busy.
    pub max_events: usize,
    /// Maximum encoded public-stream bytes retained by one execution.
    pub event_bytes: usize,
    /// Maximum interests on an execution, including terminal subscribers.
    pub max_subscribers: usize,
    /// Aggregate accepted result bytes per execution, bounding core findings/replay storage.
    pub work_bytes: usize,
    /// Terminal executions are retained at least this long, and until all stops are acked.
    pub terminal_ms: u64,
    /// Silent sockets time out; reconnecting within the session lifetime retains retries.
    pub idle_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_frame: 1 << 20,
            max_connections: 32,
            max_sessions: 32,
            session_ms: 300_000,
            max_requests: 1_024,
            replay_bytes: 8 << 20,
            max_executions: 128,
            max_events: 4_096,
            event_bytes: 8 << 20,
            max_subscribers: 256,
            work_bytes: 8 << 20,
            terminal_ms: 600_000,
            idle_ms: 30_000,
        }
    }
}

struct Session {
    expires: u64,
    next: u64,
    bytes: usize,
    replies: BTreeMap<u64, (Vec<u8>, Vec<u8>)>,
    /// The client's creation nonce, so a retried creation Hello returns this same lifetime.
    nonce: Digest,
}

/// Single-owner facade. A reactor owns it; no locks around individual core transitions.
pub struct Coordinator {
    /// Resource/lifetime configuration.
    pub config: Config,
    /// Instance identity; a restart cannot replay the previous daemon's request IDs.
    pub epoch: Digest,
    jobs: Jobs,
    /// Live sessions only. Expiry removes the entry, so its capacity returns. A label is
    /// never issued twice, so nothing has to be remembered to refuse a revival.
    sessions: BTreeMap<Digest, Session>,
    /// Monotonic label counter. Labels are not capabilities: they only need to be unique
    /// within this daemon, and the epoch check carries the authority.
    issued: u64,
    ended: BTreeMap<ExecutionId, u64>,
    work_bytes: BTreeMap<ExecutionId, usize>,
    last_now: u64,
}

fn error(code: ErrorCode, text: &str) -> WireError {
    WireError::new(code, text)
}

impl Coordinator {
    /// Own the actual core registry. The caller sets admission/builders, not worker messages.
    pub fn new(jobs: Jobs, config: Config, epoch: Digest) -> Self {
        Self {
            jobs,
            config,
            epoch,
            sessions: BTreeMap::new(),
            issued: 0,
            ended: BTreeMap::new(),
            work_bytes: BTreeMap::new(),
            last_now: 0,
        }
    }

    /// The label a created session is issued, derived from the epoch and the counter only.
    /// Two daemons differ by epoch; two creations differ by counter. Neither is a secret.
    fn label(&self, counter: u64) -> Digest {
        let mut bytes = [0u8; 40];
        bytes[..32].copy_from_slice(&self.epoch);
        bytes[32..].copy_from_slice(&counter.to_be_bytes());
        blaze_wire::digest_of_encoded_name("blaze/session", &bytes)
    }

    /// Monotonic clock, including injected clocks: a rollback never extends a lease.
    pub fn now(&mut self, now: u64) -> u64 {
        self.last_now = self.last_now.max(now);
        self.last_now
    }

    /// Actual timer transitions and terminal/session retention.
    pub fn tick(&mut self, now: u64) -> Vec<(ExecutionId, Effect)> {
        let now = self.now(now);
        let mut effects = Vec::new();
        self.jobs.tick(now, &mut effects);
        // A session is capacity only while it is live. Expiry removes the entry and its
        // journal, which is safe because an issued identity is never issued again: a request
        // naming an expired session is refused as unknown, never replayed into a new one.
        self.sessions.retain(|_, session| now <= session.expires);
        for e in self.jobs.executions() {
            if matches!(e.state(), ExecState::Ended(_)) {
                self.ended.entry(e.id).or_insert(now);
            }
        }
        let expired: Vec<_> = self
            .ended
            .iter()
            .filter(|(_, at)| now.saturating_sub(**at) > self.config.terminal_ms)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if self.jobs.forget_terminal(id) {
                self.ended.remove(&id);
                self.work_bytes.remove(&id);
            }
        }
        effects
    }

    /// Orderly reactor shutdown cancels active subscriptions and emits genuine stop effects.
    pub fn shutdown(&mut self, now: u64) -> Vec<(ExecutionId, Effect)> {
        let now = self.now(now);
        let attached: Vec<_> = self
            .jobs
            .executions()
            .flat_map(|e| {
                e.subscribers()
                    .filter(|s| s.state == blaze_dist::jobs::SubscriberState::Attached)
                    .map(|s| (e.id, s.id))
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut effects = Vec::new();
        for (execution, subscriber) in attached {
            let _ = self.jobs.unsubscribe(now, execution, subscriber, &mut effects);
        }
        effects
    }

    /// Version negotiation, then either creation or resume. Creation issues a fresh label
    /// and is idempotent for its nonce only while that lifetime is live; resume never
    /// creates, so an expired or unknown label stays refused in this daemon epoch.
    pub fn hello(&mut self, now: u64, hello: &LocalHello) -> Result<LocalWelcome, WireError> {
        let now = self.now(now);
        if hello.transport != TRANSPORT_VERSION
            || hello.codec != blaze_wire::VERSION
            || hello.terminal != blaze_wire::job_terminal_v2::VERSION
        {
            return Err(error(ErrorCode::Conflict, "version-mismatch"));
        }
        match (&hello.epoch, &hello.resume) {
            (None, None) => self.create(now, hello),
            (Some(epoch), Some(session)) => {
                if *epoch != self.epoch {
                    return Err(error(ErrorCode::Conflict, "daemon-moved"));
                }
                let live =
                    self.sessions.get(session).filter(|s| now <= s.expires).ok_or_else(|| {
                        error(
                            ErrorCode::NotFound,
                            "session-expired: resume names no live lifetime of this daemon",
                        )
                    })?;
                Ok(self.welcome(now, *session, live, false))
            }
            _ => Err(error(
                ErrorCode::Malformed,
                "hello-shape: creation names no epoch/resume, resume names both",
            )),
        }
    }

    /// A creation is one issued label per live nonce. Retrying it returns that same
    /// lifetime; it never migrates another session's pending request into the new one.
    /// Expired lifetimes are purged here, so ordinary expiry needs no separate tick to make
    /// capacity available again on the Hello path.
    fn create(&mut self, now: u64, hello: &LocalHello) -> Result<LocalWelcome, WireError> {
        self.sessions.retain(|_, session| now <= session.expires);
        let existing =
            self.sessions.iter().find(|(_, s)| s.nonce == hello.nonce).map(|(session, _)| *session);
        if let Some(session) = existing {
            let live = &self.sessions[&session];
            return Ok(self.welcome(now, session, live, false));
        }
        if self.sessions.len() >= self.config.max_sessions {
            return Err(error(ErrorCode::Busy, "session-capacity: live sessions are full"));
        }
        // A label is never issued twice. Exhausting the counter is a typed capacity
        // refusal, never a wrap that would reissue an old identity.
        let counter = self
            .issued
            .checked_add(1)
            .ok_or_else(|| error(ErrorCode::Busy, "session-label-exhausted"))?;
        let session = self.label(counter);
        self.sessions.insert(
            session,
            Session {
                expires: now.saturating_add(self.config.session_ms),
                next: 1,
                bytes: 0,
                replies: BTreeMap::new(),
                nonce: hello.nonce,
            },
        );
        self.issued = counter;
        let live = &self.sessions[&session];
        Ok(self.welcome(now, session, live, true))
    }

    fn welcome(&self, now: u64, session: Digest, live: &Session, created: bool) -> LocalWelcome {
        LocalWelcome {
            transport: TRANSPORT_VERSION,
            codec: blaze_wire::VERSION,
            terminal: blaze_wire::job_terminal_v2::VERSION,
            session,
            epoch: self.epoch,
            now_ms: now,
            expires_ms: live.expires,
            max_frame: self.config.max_frame,
            next_request: live.next,
            created,
        }
    }

    /// Execute once, journal the exact encoded response before it is written to the socket.
    /// Invalid/reused/out-of-order IDs never reach the core. Cached refusals are immutable too.
    pub fn request(
        &mut self,
        now: u64,
        request: &Request,
    ) -> Result<(Vec<u8>, Vec<(ExecutionId, Effect)>), WireError> {
        let now = self.now(now);
        if request.epoch != self.epoch {
            return Err(error(ErrorCode::Conflict, "daemon-moved"));
        }
        let raw = encode(&ClientFrame::Request(request.clone()));
        let session = self
            .sessions
            .get(&request.session)
            .filter(|s| now <= s.expires)
            .ok_or_else(|| error(ErrorCode::NotFound, "session-expired"))?;
        if let Some((old, reply)) = session.replies.get(&request.sequence) {
            if old != &raw {
                return Err(error(ErrorCode::Malformed, "sequence-reused"));
            }
            return Ok((reply.clone(), Vec::new()));
        }
        if request.sequence != session.next {
            return Err(error(ErrorCode::Conflict, "request-out-of-order"));
        }
        // Reserve the largest response before a side effect can occur. Never discover a
        // replay-budget failure after submitting/committing the job.
        if session.replies.len() >= self.config.max_requests
            || session
                .bytes
                .saturating_add(raw.len())
                .saturating_add(self.config.max_frame as usize)
                > self.config.replay_bytes
        {
            return Err(error(ErrorCode::Busy, "replay-capacity: use a new session"));
        }
        let mut effects = Vec::new();
        let answer = match self.route(now, &request.command, &mut effects) {
            Ok(answer) => answer,
            Err(error) => Answer::Error(error),
        };
        let mut reply = encode(&ServerFrame::Response(Response {
            epoch: self.epoch,
            session: request.session,
            sequence: request.sequence,
            answer,
        }));
        if reply.len() > self.config.max_frame as usize {
            // Keep the already-observed transition journaled if a future response variant
            // breaks its structural bound; never transmit an oversize frame or retry it.
            reply = encode(&ServerFrame::Response(Response {
                epoch: self.epoch,
                session: request.session,
                sequence: request.sequence,
                answer: Answer::Error(error(ErrorCode::Busy, "response-frame-capacity")),
            }));
        }
        // Responses are bounded structurally by event pagination and request limits.
        // The server configuration is validated to leave room for all non-event answers.
        let session = self.sessions.get_mut(&request.session).expect("validated session");
        session.bytes += raw.len() + reply.len();
        session.next += 1;
        session.replies.insert(request.sequence, (raw, reply.clone()));
        // A release is terminal: its answer is journaled above, then the lifetime and its
        // journal are removed. Capacity and memory return at once, and nothing is kept to
        // revive. A release performs no core transition, so a client that never saw this
        // answer and retries meets `session-expired` and has lost no effect, only a lifetime.
        if matches!(request.command, Command::Release(_)) {
            self.sessions.remove(&request.session);
        }
        Ok((reply, effects))
    }

    fn subscription(&self, s: &Subscription) -> Result<Binding, WireError> {
        let e =
            self.jobs.execution(s.execution).ok_or_else(|| Refusal::NoExecution.to_wire(None))?;
        if e.content != s.content || !e.subscribers().any(|sub| sub.id == s.subscriber) {
            return Err(Refusal::NoSubscriber.to_wire(None));
        }
        // Ended subscribers resume their immutable terminal attempt, even if the
        // shared execution moved to a replacement lead for its remaining subscribers.
        let attempt =
            self.jobs.terminal(s.execution, s.subscriber).map_or(e.attempt(), |t| t.attempt);
        Ok(Binding { subscription: s.clone(), attempt })
    }

    fn binding(&self, b: &Binding) -> Result<(), WireError> {
        if self.subscription(&b.subscription)?.attempt != b.attempt {
            return Err(Refusal::AttemptMoved.to_wire(None));
        }
        Ok(())
    }

    fn work(
        &self,
        content: blaze_wire::JobContent,
        execution: ExecutionId,
        attempt: blaze_wire::Attempt,
    ) -> Result<(), WireError> {
        let e = self.jobs.execution(execution).ok_or_else(|| Refusal::NoExecution.to_wire(None))?;
        if e.content != content {
            return Err(Refusal::NoExecution.to_wire(None));
        }
        if e.attempt() != attempt {
            return Err(Refusal::AttemptMoved.to_wire(None));
        }
        Ok(())
    }

    fn route(
        &mut self,
        now: u64,
        command: &Command,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) -> Result<Answer, WireError> {
        match command {
            Command::Submit(s) => {
                let req: JobRequest = decode(&s.request.bytes, &Limits::DOCUMENTED)
                    .map_err(|_| error(ErrorCode::Malformed, "submission-codec"))?;
                if submission(&req, s.need.clone()).content != s.content {
                    return Err(error(ErrorCode::Conflict, "content-mismatch"));
                }
                let running = self
                    .jobs
                    .executions()
                    .find(|e| e.content == s.content && !matches!(e.state(), ExecState::Ended(_)));
                // An equal live job adds an interest to the existing execution. A retained
                // terminal instead creates a new execution with its own subscriber budget.
                if running.is_none() && self.jobs.executions().count() >= self.config.max_executions
                {
                    return Err(error(ErrorCode::Busy, "execution-retention-capacity"));
                }
                if running.is_some_and(|e| e.subscribers().count() >= self.config.max_subscribers) {
                    return Err(error(ErrorCode::Busy, "subscriber-retention-capacity"));
                }
                let a = self
                    .jobs
                    .submit(now, &req, s.need.clone(), out)
                    .map_err(|(r, retry)| r.to_wire(retry))?;
                self.jobs.tick(now, out);
                Ok(Answer::Binding(Binding {
                    subscription: Subscription {
                        content: a.content,
                        execution: a.execution,
                        subscriber: a.subscriber,
                    },
                    attempt: self.jobs.execution(a.execution).expect("submitted").attempt(),
                }))
            }
            Command::Subscribe(s) => {
                if self
                    .jobs
                    .execution(s.execution)
                    .is_some_and(|e| e.subscribers().count() >= self.config.max_subscribers)
                {
                    return Err(error(ErrorCode::Busy, "subscriber-retention-capacity"));
                }
                let a = self
                    .jobs
                    .subscribe(now, s.execution, s.content, s.deadline_ms, s.policy.clone(), out)
                    .map_err(|r| r.to_wire(None))?;
                Ok(Answer::Binding(Binding {
                    subscription: Subscription {
                        content: a.content,
                        execution: a.execution,
                        subscriber: a.subscriber,
                    },
                    attempt: self.jobs.execution(a.execution).expect("subscribed").attempt(),
                }))
            }
            Command::Resume(s) => Ok(Answer::Binding(self.subscription(s)?)),
            Command::Events(r) => {
                self.binding(&r.binding)?;
                if r.limit == 0 || r.limit > 256 {
                    return Err(error(ErrorCode::Malformed, "event-page-limit"));
                }
                let events = self
                    .jobs
                    .execution(r.binding.subscription.execution)
                    .expect("bound")
                    .events(r.from);
                let mut page =
                    EventPage { binding: r.binding.clone(), events: Vec::new(), next: r.from };
                for event in events.iter().take(r.limit as usize) {
                    page.events.push(event.clone());
                    // Leave framing/response envelope headroom, never split a CBOR event.
                    if encode(&page).len() + 256 > self.config.max_frame as usize {
                        page.events.pop();
                        break;
                    }
                    page.next = event.sequence;
                }
                if !events.is_empty() && page.events.is_empty() {
                    return Err(error(ErrorCode::Busy, "event-too-large"));
                }
                Ok(Answer::Events(page))
            }
            Command::Cancel(b) => {
                self.binding(b)?;
                self.jobs
                    .unsubscribe(now, b.subscription.execution, b.subscription.subscriber, out)
                    .map_err(|r| r.to_wire(None))?;
                Ok(Answer::Ack(Ack {}))
            }
            Command::Join(r) => {
                self.work(r.content, r.request.execution, r.request.attempt)?;
                if r.request.roots.count > 4096 {
                    return Err(error(ErrorCode::Busy, "root-capacity"));
                }
                Ok(Answer::Join(
                    self.jobs
                        .join(now, &r.executor, &r.request, out)
                        .map_err(|r| r.to_wire(None))?,
                ))
            }
            Command::Lease(r) => {
                self.work(r.content, r.request.execution, r.request.attempt)?;
                if self
                    .jobs
                    .execution(r.request.execution)
                    .and_then(|e| e.work())
                    .is_some_and(|w| w.counters().grants >= 4096)
                {
                    return Err(error(ErrorCode::Busy, "grant-retention-capacity"));
                }
                Ok(Answer::Lease(
                    self.jobs
                        .lease(now, &r.executor, &r.request, out)
                        .map_err(|r| r.to_wire(None))?,
                ))
            }
            Command::Result(r) => {
                self.work(r.content, r.request.lease.execution, r.request.lease.attempt)?;
                if r.request.items.iter().map(|item| item.found.len()).sum::<usize>() > 4096 {
                    return Err(error(ErrorCode::Busy, "discovery-frame-capacity"));
                }
                let bytes = encode(&r.request).len();
                let held = self.work_bytes.get(&r.request.lease.execution).copied().unwrap_or(0);
                if held.saturating_add(bytes) > self.config.work_bytes {
                    return Err(error(ErrorCode::Busy, "work-retention-capacity"));
                }
                let answer = self
                    .jobs
                    .result(now, &r.executor, &r.request, out)
                    .map_err(|r| r.to_wire(None))?;
                self.work_bytes.insert(r.request.lease.execution, held + bytes);
                Ok(Answer::Result(answer))
            }
            Command::WorkerEvents(r) => {
                self.work(r.content, r.request.execution, r.request.attempt)?;
                let e = self.jobs.execution(r.request.execution).expect("bound");
                if e.events(0).len().saturating_add(r.request.events.len()).saturating_add(4)
                    > self.config.max_events
                {
                    return Err(error(ErrorCode::Busy, "event-retention-capacity"));
                }
                let held: usize = e.events(0).iter().map(|event| encode(event).len()).sum();
                if held.saturating_add(encode(&r.request).len()).saturating_add(1024)
                    > self.config.event_bytes
                {
                    return Err(error(ErrorCode::Busy, "event-byte-capacity"));
                }
                Ok(Answer::WorkerEvents(
                    self.jobs
                        .worker_events(now, &r.executor, &r.request, out)
                        .map_err(|r| r.to_wire(None))?,
                ))
            }
            Command::KillAck(r) => {
                // Old-attempt stop acknowledgments are necessary; Jobs checks the exact
                // pending token, not the current execution attempt.
                if self.jobs.execution(r.execution).is_none_or(|e| e.content != r.content) {
                    return Err(Refusal::NoExecution.to_wire(None));
                }
                let accepted = self.jobs.killed(
                    now,
                    &r.executor,
                    AssignmentToken { execution: r.execution, attempt: r.attempt },
                    r.alive,
                );
                Ok(Answer::KillAck(KillAnswer { accepted }))
            }
            Command::Terminal(s) => {
                self.subscription(s)?;
                Ok(Answer::Terminal(Terminal {
                    subscription: s.clone(),
                    value: self.jobs.terminal(s.execution, s.subscriber).cloned(),
                }))
            }
            Command::Release(_) => Ok(Answer::Ack(Ack {})),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_dist::admission::{Admission, AdmissionConfig};
    use blaze_dist::jobs::ExecutionConfig;

    fn coordinator(config: Config) -> Coordinator {
        let jobs =
            Jobs::new(Admission::new(AdmissionConfig::default()), ExecutionConfig::default());
        Coordinator::new(jobs, config, [7; 32])
    }

    /// A label is never issued twice. Running the counter out is a typed capacity refusal,
    /// not a wrap that would hand back an old identity, and the daemon keeps serving.
    #[test]
    fn label_counter_exhaustion_is_a_typed_refusal_not_a_reissued_identity() {
        let mut c = coordinator(Config::default());
        c.issued = u64::MAX;
        let refused = c.hello(0, &hello([1; 32])).unwrap_err();
        assert_eq!(refused.code(), Some(ErrorCode::Busy));
        assert!(refused.message.contains("session-label-exhausted"));
        // Nothing was issued, so capacity was not spent and no label came back around.
        assert!(c.sessions.is_empty());
        assert_eq!(c.issued, u64::MAX);
    }

    /// A creation is idempotent for its nonce only while that lifetime is live, and the two
    /// outcomes are distinguishable so a client never mistakes a revival for a creation.
    #[test]
    fn creation_is_idempotent_while_live_and_distinct_after_expiry() {
        let mut c = coordinator(Config { session_ms: 10, ..Config::default() });
        let first = c.hello(0, &hello([2; 32])).unwrap();
        assert!(first.created);
        let retried = c.hello(5, &hello([2; 32])).unwrap();
        assert_eq!(retried.session, first.session);
        assert!(!retried.created);
        let expired = c.hello(11, &hello([2; 32])).unwrap();
        assert!(expired.created);
        assert_ne!(expired.session, first.session);
        // The old label is dead, not resumable, and not a live session any more.
        assert!(c.hello(11, &resume(first.session, c.epoch)).is_err());
    }

    /// A release frees capacity and journal memory at once, and no tombstone remains, so the
    /// map cannot grow with the number of clients served.
    #[test]
    fn release_returns_capacity_without_retaining_a_tombstone() {
        let mut c = coordinator(Config { max_sessions: 2, ..Config::default() });
        let a = c.hello(0, &hello([3; 32])).unwrap();
        let b = c.hello(0, &hello([4; 32])).unwrap();
        assert_ne!(a.session, b.session);
        let third = c.hello(0, &hello([5; 32])).unwrap_err();
        assert_eq!(third.code(), Some(ErrorCode::Busy));
        // Release `a`: its journal goes with it and one slot comes back.
        let request = Request {
            epoch: c.epoch,
            session: a.session,
            sequence: 1,
            command: Command::Release(Release {}),
        };
        let (_, effects) = c.request(0, &request).unwrap();
        assert!(effects.is_empty());
        assert_eq!(c.sessions.len(), 1);
        assert!(!c.sessions.contains_key(&a.session));
        let c2 = c.hello(0, &hello([6; 32])).unwrap();
        assert!(c2.created);
        // The released label cannot be resumed or replayed.
        assert!(c.hello(0, &resume(a.session, c.epoch)).is_err());
    }
}
