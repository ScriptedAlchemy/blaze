//! Nonblocking Unix reactor. Socket fragmentation/coalescing is unrelated to CBOR boundaries.

use core::alloc::{Allocator, AllocatorClone};
use core::sync::atomic::{AtomicBool, Ordering};

use blaze_dist::jobs::Effect;
use blaze_own::{Arc, Box, Vec, VecDeque, format_in};
use blaze_wire::{
    Digest, ErrorCode, ExecutionId, Limits, WireAlloc, WireString, WireVec, decode, encode,
};

use crate::Error;
use crate::coordinator::{Config, Coordinator};
use crate::protocol::*;
use crate::sys::{
    IoError, SystemClock, UnixListener, UnixStream, env_is_set, sleep_ms, stat_path, unlink_path,
};

/// Injected daemon-relative monotonic clock. Coordinator clamps accidental rollback.
pub trait Clock: Send + Sync {
    /// Milliseconds since this daemon's clock origin.
    fn now_ms(&self) -> u64;
}

impl<C: Clock + ?Sized, A: AllocatorClone + Send + Sync> Clock for Arc<C, A> {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }
}

/// Real monotonic clock used by the production reactor.
pub struct MonotonicClock {
    clock: SystemClock,
    origin_ns: u64,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        let clock = SystemClock;
        let origin_ns = clock.monotonic_nanos().unwrap_or(0);
        Self { clock, origin_ns }
    }
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        let now = self.clock.monotonic_nanos().unwrap_or(self.origin_ns);
        now.saturating_sub(self.origin_ns) / 1_000_000
    }
}

/// Effect callback, delivered once when the transition occurs, never again on request retry.
/// A Kill callback must actually stop the named assignment before its caller sends KillAck.
/// This carrier never manufactures kill acknowledgments or successful compiler completion.
/// Production callbacks must enqueue backend work without blocking the reactor. The backend
/// sends its independently completed acknowledgment over the same carrier API.
pub trait Effects: Send {
    /// Deliver one transition's batch, moving its effects to the holder/agent backend.
    /// The iterator is borrowed only for this call; yielded effects may be retained.
    fn deliver(&mut self, effects: &mut dyn Iterator<Item = (ExecutionId, Effect)>);
}

impl<F: FnMut(&mut dyn Iterator<Item = (ExecutionId, Effect)>) + Send> Effects for F {
    fn deliver(&mut self, effects: &mut dyn Iterator<Item = (ExecutionId, Effect)>) {
        self(effects)
    }
}

struct Connection {
    stream: UnixStream,
    input: WireVec<u8>,
    output: VecDeque<WireVec<u8>, WireAlloc>,
    written: usize,
    session: Option<Digest>,
    last_io: u64,
    closing: bool,
}

/// Owns one socket path and its inode. Drop removes only that original socket, never a replacement.
pub struct Server<C: Clock> {
    listener: UnixListener,
    path: WireString,
    inode: (u64, u64),
    coordinator: Coordinator,
    clock: C,
    effects: Box<dyn Effects, WireAlloc>,
    alloc: WireAlloc,
    #[cfg(test)]
    accept_errors: VecDeque<IoError, WireAlloc>,
}

fn fail<A: Allocator + Clone>(text: &str, wire_alloc: WireAlloc, alloc: A) -> Vec<u8, A> {
    encode(
        &ServerFrame::Error(blaze_wire::WireError::new(
            ErrorCode::Malformed,
            format_args!("{text}"),
            wire_alloc,
        )),
        alloc,
    )
}

/// Read an OS-random instance/session ID without a hashing or timestamp uniqueness guess.
pub fn random_id() -> Result<Digest, IoError> {
    crate::sys::random_id()
}

impl<C: Clock> Server<C> {
    /// Bind a new path; an existing socket is an error, not permission to unlink another daemon.
    /// Parent directory policy is the caller's; this local socket is owner-only, no new auth layer.
    pub fn bind(
        path: &str,
        jobs: blaze_dist::jobs::Jobs,
        config: Config,
        clock: C,
        effects: impl Effects + 'static,
        alloc: WireAlloc,
    ) -> Result<Self, Error> {
        let max_frame = usize::try_from(config.max_frame).unwrap_or(usize::MAX);
        if config.max_frame < 1 << 20
            || config.max_frame > Limits::DOCUMENTED.request
            || config.max_connections == 0
            || config.max_sessions == 0
            || config.session_ms == 0
            || config.max_execution_ms == 0
            || config.owner_cleanup_ms == 0
            || config.idle_ms == 0
            || config.max_subscribers == 0
            || config.max_executions == 0
            || config.max_events < 8
            || config.work_bytes < max_frame
            || config.event_bytes < max_frame
            || jobs.config().work.grant_names > 4096
            || jobs.config().work.grant_roots > 4096
        {
            return Err(Error::Protocol(WireString::from_str_in("invalid carrier bounds", alloc)));
        }
        let epoch = random_id()?;
        let listener = UnixListener::bind_owner_only(path)?;
        let metadata = stat_path(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path: WireString::from_str_in(path, alloc),
            inode: (metadata.dev, metadata.ino),
            coordinator: Coordinator::new(jobs, config, epoch, alloc),
            clock,
            effects: Box::new_in(effects, alloc),
            alloc,
            #[cfg(test)]
            accept_errors: VecDeque::new_in(alloc),
        })
    }

    /// Run until shutdown. IO cannot block a timer: reads/writes are nonblocking and each
    /// connection gets a bounded frame budget per pass. Shutdown drops sockets and the registry.
    pub fn run(mut self, stop: &AtomicBool) -> Result<(), Error> {
        let alloc = self.alloc;
        let mut connections = Vec::new_in(alloc);
        while !stop.load(Ordering::Acquire) {
            let now = self.coordinator.now(self.clock.now_ms());
            let timed = self.coordinator.tick(now, alloc);
            self.outcomes(&timed);
            self.effects.deliver(&mut timed.into_iter());
            // One accept per loop prevents an accept flood starving existing clients/timers.
            match self.accept() {
                Ok(stream) => {
                    if connections.len() < self.coordinator.config.max_connections {
                        stream.set_nonblocking(true)?;
                        connections.push(Connection {
                            stream,
                            input: Vec::new_in(alloc),
                            output: VecDeque::new_in(alloc),
                            written: 0,
                            session: None,
                            last_io: now,
                            closing: false,
                        });
                    }
                }
                Err(e) if e.is_transient_accept() => {}
                Err(e) => return Err(e.into()),
            }
            connections.retain_mut(|connection| self.step(now, connection));
            sleep_ms(1);
        }
        // Deliver stops for every active subscription on orderly shutdown. No automatic ack.
        // Abrupt process death is fenced by the next daemon epoch, not replayed as success.
        let effects = self.coordinator.shutdown(self.clock.now_ms(), alloc);
        self.outcomes(&effects);
        self.effects.deliver(&mut effects.into_iter());
        Ok(())
    }

    fn accept(&mut self) -> Result<UnixStream, IoError> {
        #[cfg(test)]
        if let Some(error) = self.accept_errors.pop_front() {
            return Err(error);
        }
        self.listener.accept()
    }

    fn step(&mut self, now: u64, c: &mut Connection) -> bool {
        let alloc = self.alloc;
        if now.saturating_sub(c.last_io) > self.coordinator.config.idle_ms {
            return false;
        }
        // One response at a time bounds slow-reader memory, while input may contain coalesced frames.
        if c.output.is_empty() && !c.closing {
            let declared = c
                .input
                .first_chunk::<4>()
                .and_then(|hdr| usize::try_from(u32::from_be_bytes(*hdr)).ok());
            let complete = declared
                .is_some_and(|size| c.input.len() >= size.saturating_add(4));
            if !complete {
                let mut chunk = [0u8; 8192];
                match c.stream.read(&mut chunk) {
                    Ok(0) => return false,
                    Ok(n) => {
                        if let Some(slice) = chunk.get(..n) {
                            c.input.extend_from_slice(slice);
                        }
                        c.last_io = now;
                    }
                    Err(e) if e.is_would_block() => {}
                    Err(_) => return false,
                }
            }
            if let Some(hdr) = c.input.first_chunk::<4>() {
                let raw_size = u32::from_be_bytes(*hdr);
                let size_opt = usize::try_from(raw_size).ok();
                if raw_size == 0 || raw_size > self.coordinator.config.max_frame {
                    c.output.push_back(frame(&fail("frame-size", alloc, alloc), alloc));
                    c.closing = true;
                    c.input.clear();
                } else if let Some(size) = size_opt
                    && let Some(end) = size.checked_add(4)
                    && let Some(payload) = c.input.get(4..end)
                {
                    let body = Vec::from_slice_in(payload, alloc);
                    c.input.drain(..end);
                    let (reply, close) = self.handle(now, c, &body, alloc);
                    c.output.push_back(frame(&reply, alloc));
                    c.closing = close;
                }
            }
        }
        if let Some(front) = c.output.front() {
            let Some(remaining) = front.get(c.written..) else {
                return false;
            };
            match c.stream.write(remaining) {
                Ok(0) => return false,
                Ok(n) => {
                    c.written = c.written.saturating_add(n);
                    c.last_io = now;
                    if c.written >= front.len() {
                        c.output.pop_front();
                        c.written = 0;
                    }
                }
                Err(e) if e.is_would_block() => {}
                Err(_) => return false,
            }
        }
        !(c.closing && c.output.is_empty())
    }

    fn evidence(&self, command: &Command, reply: &[u8], effects: &[(ExecutionId, Effect)]) {
        let alloc = self.alloc;
        if !env_is_set("BLAZE_DIST_EVENTS", alloc) {
            return;
        }
        use crate::telemetry::{emit, json};
        let Ok(ServerFrame::Response(response)) =
            decode::<ServerFrame>(reply, &Limits::DOCUMENTED, alloc)
        else {
            return;
        };
        match (command, &response.answer) {
            (Command::Result(result), answer) => {
                let request = &result.request;
                let mut fields = Vec::with_capacity_in(6, alloc);
                fields.push(("execution", json(&request.lease.execution.0, alloc)));
                fields.push(("attempt", json(&request.lease.attempt.0, alloc)));
                fields.push(("holder", json(&request.lease.holder, alloc)));
                fields.push(("generation", json(&request.lease.generation, alloc)));
                fields.push(("seq", json(&request.seq, alloc)));
                match answer {
                    Answer::Result(_)
                        if effects.iter().any(|(execution, effect)| {
                            *execution == request.lease.execution
                                && matches!(
                                    effect,
                                    Effect::Work(
                                        blaze_dist::work::Effect::Committed { by, .. }
                                            | blaze_dist::work::Effect::Replayed { by }
                                    ) if by.holder == request.lease.holder
                                        && by.generation == request.lease.generation
                                        && by.seq == request.seq
                                )
                        }) =>
                    {
                        let mut work = Vec::new_in(alloc);
                        work.extend(
                            self.coordinator
                                .committed_names(request, alloc)
                                .iter()
                                .map(|name| name.0),
                        );
                        fields.push(("work", json(&work, alloc)));
                        emit("result_accepted", &fields, alloc);
                    }
                    Answer::Error(error) => {
                        if let Some(
                            why @ (blaze_dist::Refusal::DeadLease
                            | blaze_dist::Refusal::AttemptMoved),
                        ) = blaze_dist::Refusal::of(error)
                        {
                            let rendered = format_in!(alloc, "{why:?}");
                            fields.push(("refusal", json(&rendered, alloc)));
                            emit("result_refused", &fields, alloc);
                        }
                    }
                    _ => {}
                }
            }
            (Command::CommitPublication(request), Answer::Publication(publication))
                if effects.iter().any(|(execution, effect)| {
                    *execution == request.execution
                        && matches!(
                            effect,
                            Effect::Event(blaze_wire::EventItem {
                                event: blaze_wire::JobEvent::Stage(blaze_wire::StageEvent {
                                    stage: blaze_wire::Stage::Published,
                                    ..
                                }),
                                ..
                            })
                        )
                }) =>
            {
                if let (Some(unit), Some(packs), Some(accepted)) = (
                    self.coordinator.unit(request.execution),
                    &publication.packs,
                    publication.accepted_attempt,
                ) {
                    let mut digests = Vec::new_in(alloc);
                    digests.extend(packs.iter().map(|pack| pack.pack));
                    emit(
                        "published",
                        &[
                            ("unit_digest", json(&unit, alloc)),
                            ("execution", json(&request.execution.0, alloc)),
                            ("attempt", json(&accepted.0, alloc)),
                            ("packs", json(&digests, alloc)),
                        ],
                        alloc,
                    );
                }
            }
            _ => {}
        }
        self.outcomes(effects);
    }

    fn outcomes(&self, effects: &[(ExecutionId, Effect)]) {
        let alloc = self.alloc;
        if !env_is_set("BLAZE_DIST_EVENTS", alloc) {
            return;
        }
        use crate::telemetry::{emit, json};
        for (execution, effect) in effects {
            if let Effect::Event(item) = effect
                && let blaze_wire::JobEvent::Done(done) = &item.event
            {
                let Some(unit) = self.coordinator.unit(*execution) else { continue };
                let outcome = format_in!(alloc, "{:?}", done.outcome);
                emit(
                    "done",
                    &[
                        ("unit_digest", json(&unit, alloc)),
                        ("execution", json(&execution.0, alloc)),
                        ("attempt", json(&item.attempt.0, alloc)),
                        ("outcome", json(&outcome, alloc)),
                    ],
                    alloc,
                );
            }
        }
    }

    fn handle<A: Allocator + Clone>(
        &mut self,
        now: u64,
        c: &mut Connection,
        body: &[u8],
        alloc: A,
    ) -> (Vec<u8, A>, bool) {
        let wire_alloc = self.alloc;
        let decoded = decode::<ClientFrame>(body, &Limits::DOCUMENTED, wire_alloc);
        match decoded {
            Ok(ClientFrame::Hello(hello)) if c.session.is_none() => {
                match self.coordinator.hello(now, &hello) {
                    Ok(welcome) => {
                        c.session = Some(welcome.session);
                        (encode(&ServerFrame::Welcome(welcome), alloc), false)
                    }
                    Err(e) => (encode(&ServerFrame::Error(e), alloc), true),
                }
            }
            Ok(ClientFrame::Request(request)) if c.session == Some(request.session) => {
                // A release ends the lifetime in the coordinator, which journals this answer
                // before it is written. The connection is closed after that answer, because
                // the session it named no longer exists.
                let releases = matches!(request.command, Command::Release(_));
                match self.coordinator.request(now, &request, alloc.clone()) {
                    Ok((reply, effects)) => {
                        self.evidence(&request.command, &reply, &effects);
                        self.effects.deliver(&mut effects.into_iter());
                        (reply, releases)
                    }
                    Err(e) => (encode(&ServerFrame::Error(e), alloc), true),
                }
            }
            Ok(_) => (fail("hello/session-binding", wire_alloc, alloc), true),
            Err(_) => (fail("canonical-cbor", wire_alloc, alloc), true),
        }
    }
}

/// Four-byte big-endian length plus the exact canonical body.
pub fn frame<A: Allocator>(body: &[u8], alloc: A) -> Vec<u8, A> {
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    let mut bytes = Vec::with_capacity_in(body.len().saturating_add(4), alloc);
    bytes.extend_from_slice(&len.to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

impl<C: Clock> Drop for Server<C> {
    fn drop(&mut self) {
        let effects = self.coordinator.shutdown(self.clock.now_ms(), self.alloc);
        self.effects.deliver(&mut effects.into_iter());
        if stat_path(self.path.as_str()).is_ok_and(|m| (m.dev, m.ino) == self.inode) {
            let _ = unlink_path(self.path.as_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::test_alloc::{Counted, Counts};
    use crate::sys::{Errno, TempDir, path_exists, spawn};
    use blaze_dist::admission::{Admission, AdmissionConfig};
    use blaze_dist::jobs::{ExecutionConfig, Jobs};

    fn test_alloc() -> WireAlloc {
        blaze_own::testing::heap().shared()
    }

    #[test]
    fn effect_sink_moves_one_batch_from_the_callers_allocator() {
        let alloc = test_alloc();
        let execution = ExecutionId(7);
        let assignment = blaze_dist::jobs::AssignmentToken {
            execution,
            attempt: blaze_wire::Attempt(8),
        };
        let executor = WireString::from_str_in("one owned executor name", alloc);
        let original = executor.as_ptr();
        let mut received = Vec::new_in(alloc);
        let mut calls = 0;
        let counts = Counts::default();
        {
            let mut batch = Vec::new_in(Counted(&counts));
            batch.push((execution, Effect::Kill { executor, assignment }));
            batch.push((execution, Effect::NeedLead));
            let mut pending = batch.into_iter();
            let mut sink = |effects: &mut dyn Iterator<Item = (ExecutionId, Effect)>| {
                calls += 1;
                received.extend(effects);
            };
            sink.deliver(&mut pending);
            assert!(pending.next().is_none(), "the one callback consumed the whole batch");
        }
        assert_eq!(counts.allocations.get(), counts.deallocations.get());
        assert_eq!(calls, 1);
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].0, execution);
        let Effect::Kill { executor, assignment: delivered } = &received[0].1 else {
            panic!("the first effect changed kind");
        };
        assert_eq!(
            executor.as_ptr(),
            original,
            "the callback moves the owned effect without cloning"
        );
        assert_eq!(*delivered, assignment);
        assert_eq!(received[1], (execution, Effect::NeedLead));
    }

    #[test]
    fn framing_uses_the_callers_allocator_without_changing_the_body() {
        let counts = Counts::default();
        let framed = frame(&[0, 1, 255], Counted(&counts));
        assert!(core::ptr::eq(framed.allocator().0, &counts));
        assert_eq!(framed.as_slice(), &[0, 0, 0, 3, 0, 1, 255]);
        drop(framed);
        assert_eq!(counts.allocations.get(), counts.deallocations.get());
    }

    #[test]
    fn transient_accept_errors_leave_the_reactor_available_to_healthy_clients() {
        let alloc = test_alloc();
        let dir = TempDir::new("blaze-accept", alloc).unwrap();
        let path = dir.join("accept.sock");
        let jobs = Jobs::new(
            Admission::new(AdmissionConfig::default(), alloc),
            ExecutionConfig::default(),
            alloc,
        );
        let mut server = Server::bind(
            path.as_str(),
            jobs,
            Config::default(),
            Arc::new_in(MonotonicClock::default(), alloc),
            |_: &mut dyn Iterator<Item = (ExecutionId, Effect)>| {},
            alloc,
        )
        .unwrap();
        for errno in [Errno::Intr, Errno::ConnAborted, Errno::ConnReset] {
            server.accept_errors.push_back(IoError::from_errno(errno));
        }
        let stop = Arc::new_in(AtomicBool::new(false), alloc);
        let signal = stop.clone();
        let thread = spawn(move || server.run(&signal), alloc).unwrap();
        let connected = crate::client::Client::connect(path.as_str(), random_id().unwrap(), alloc);
        stop.store(true, Ordering::Release);
        let result = thread.join().unwrap();
        assert!(result.is_ok(), "transient accept failure ended the reactor: {result:?}");
        assert!(connected.is_ok(), "a healthy client could not connect: {:?}", connected.err());
        assert!(!path_exists(path.as_str()));
    }
}
