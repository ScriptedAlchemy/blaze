//! Nonblocking Unix reactor. Socket fragmentation/coalescing is unrelated to CBOR boundaries.

use std::alloc::{Allocator, Global};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use blaze_dist::jobs::Effect;
use blaze_wire::{Digest, ErrorCode, ExecutionId, Limits, decode, encode};

use crate::Error;
use crate::coordinator::{Config, Coordinator};
use crate::protocol::*;

/// Injected daemon-relative monotonic clock. Coordinator clamps accidental rollback.
pub trait Clock: Send + Sync {
    /// Milliseconds since this daemon's clock origin.
    fn now_ms(&self) -> u64;
}

impl<C: Clock + ?Sized, A: Allocator + Send + Sync> Clock for Arc<C, A> {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }
}

/// Real monotonic clock used by the production reactor.
pub struct MonotonicClock(Instant);

impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis().min(u64::MAX as u128) as u64
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
    input: Vec<u8>,
    output: VecDeque<Vec<u8>>,
    written: usize,
    session: Option<Digest>,
    last_io: u64,
    closing: bool,
}

/// Owns one socket path and its inode. Drop removes only that original socket, never a replacement.
pub struct Server<C: Clock> {
    listener: UnixListener,
    path: PathBuf,
    inode: (u64, u64),
    coordinator: Coordinator,
    clock: C,
    effects: Box<dyn Effects>,
    #[cfg(test)]
    accept_errors: VecDeque<io::Error>,
}

fn fail<A: Allocator + Clone>(text: &str, alloc: A) -> Vec<u8, A> {
    encode(
        &ServerFrame::Error(blaze_wire::WireError::new(ErrorCode::Malformed, format_args!("{text}"))),
        alloc,
    )
}

/// Read an OS-random instance/session ID without a hashing or timestamp uniqueness guess.
pub fn random_id() -> io::Result<Digest> {
    let mut id = [0; 32];
    File::open("/dev/urandom")?.read_exact(&mut id)?;
    Ok(id)
}

impl<C: Clock> Server<C> {
    /// Bind a new path; an existing socket is an error, not permission to unlink another daemon.
    /// Parent directory policy is the caller's; this local socket is owner-only, no new auth layer.
    pub fn bind(
        path: &Path,
        jobs: blaze_dist::jobs::Jobs,
        config: Config,
        clock: C,
        effects: impl Effects + 'static,
    ) -> Result<Self, Error> {
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
            || config.work_bytes < config.max_frame as usize
            || config.event_bytes < config.max_frame as usize
            || jobs.config().work.grant_names > 4096
            || jobs.config().work.grant_roots > 4096
        {
            return Err(Error::Protocol("invalid carrier bounds".to_owned()));
        }
        let epoch = random_id()?;
        let listener = UnixListener::bind(path)?;
        let metadata = fs::metadata(path)?;
        let server = Self {
            listener,
            path: path.to_owned(),
            inode: (metadata.dev(), metadata.ino()),
            coordinator: Coordinator::new(jobs, config, epoch),
            clock,
            effects: Box::new_in(effects, Global),
            #[cfg(test)]
            accept_errors: VecDeque::new(),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        server.listener.set_nonblocking(true)?;
        Ok(server)
    }

    /// Run until shutdown. IO cannot block a timer: reads/writes are nonblocking and each
    /// connection gets a bounded frame budget per pass. Shutdown drops sockets and the registry.
    pub fn run(mut self, stop: &AtomicBool) -> Result<(), Error> {
        let mut connections = Vec::new_in(Global);
        while !stop.load(Ordering::Acquire) {
            let now = self.coordinator.now(self.clock.now_ms());
            let timed = self.coordinator.tick(now, Global);
            self.outcomes(&timed);
            self.effects.deliver(&mut timed.into_iter());
            // One accept per loop prevents an accept flood starving existing clients/timers.
            match self.accept() {
                Ok((stream, _)) => {
                    if connections.len() < self.coordinator.config.max_connections {
                        stream.set_nonblocking(true)?;
                        connections.push(Connection {
                            stream,
                            input: Vec::new_in(Global),
                            output: VecDeque::new_in(Global),
                            written: 0,
                            session: None,
                            last_io: now,
                            closing: false,
                        });
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::Interrupted
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(e) => return Err(e.into()),
            }
            connections.retain_mut(|connection| self.step(now, connection));
            std::thread::sleep(Duration::from_millis(1));
        }
        // Deliver stops for every active subscription on orderly shutdown. No automatic ack.
        // Abrupt process death is fenced by the next daemon epoch, not replayed as success.
        let effects = self.coordinator.shutdown(self.clock.now_ms(), Global);
        self.outcomes(&effects);
        self.effects.deliver(&mut effects.into_iter());
        Ok(())
    }

    fn accept(&mut self) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
        #[cfg(test)]
        if let Some(error) = self.accept_errors.pop_front() {
            return Err(error);
        }
        self.listener.accept()
    }

    fn step(&mut self, now: u64, c: &mut Connection) -> bool {
        if now.saturating_sub(c.last_io) > self.coordinator.config.idle_ms {
            return false;
        }
        // One response at a time bounds slow-reader memory, while input may contain coalesced frames.
        if c.output.is_empty() && !c.closing {
            let complete = c.input.len() >= 4
                && c.input.len()
                    >= 4 + u32::from_be_bytes(c.input[..4].try_into().expect("length")) as usize;
            if !complete {
                let mut chunk = [0; 8192];
                match c.stream.read(&mut chunk) {
                    Ok(0) => return false,
                    Ok(n) => {
                        c.input.extend_from_slice(&chunk[..n]);
                        c.last_io = now;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => return false,
                }
            }
            if c.input.len() >= 4 {
                let size = u32::from_be_bytes(c.input[..4].try_into().expect("length")) as usize;
                if size == 0 || size > self.coordinator.config.max_frame as usize {
                    c.output.push_back(frame(&fail("frame-size", Global), Global));
                    c.closing = true;
                    c.input.clear();
                } else if c.input.len() >= size + 4 {
                    let body = c.input[4..size + 4].to_vec_in(Global);
                    c.input.drain(..size + 4);
                    let (reply, close) = self.handle(now, c, &body, Global);
                    c.output.push_back(frame(&reply, Global));
                    c.closing = close;
                }
            }
        }
        if let Some(front) = c.output.front() {
            match c.stream.write(&front[c.written..]) {
                Ok(0) => return false,
                Ok(n) => {
                    c.written += n;
                    c.last_io = now;
                    if c.written == front.len() {
                        c.output.pop_front();
                        c.written = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return false,
            }
        }
        !(c.closing && c.output.is_empty())
    }

    fn evidence(&self, command: &Command, reply: &[u8], effects: &[(ExecutionId, Effect)]) {
        if std::env::var_os("BLAZE_DIST_EVENTS").is_none() {
            return;
        }
        use crate::telemetry::{emit, json};
        let Ok(ServerFrame::Response(response)) = decode::<ServerFrame>(reply, &Limits::DOCUMENTED)
        else {
            return;
        };
        match (command, &response.answer) {
            (Command::Result(result), answer) => {
                let request = &result.request;
                let mut fields = vec![
                    ("execution", json(&request.lease.execution.0, Global)),
                    ("attempt", json(&request.lease.attempt.0, Global)),
                    ("holder", json(&request.lease.holder, Global)),
                    ("generation", json(&request.lease.generation, Global)),
                    ("seq", json(&request.seq, Global)),
                ];
                match answer {
                    Answer::Result(_) if effects.iter().any(|(execution, effect)| {
                        *execution == request.lease.execution && matches!(effect,
                            Effect::Work(blaze_dist::work::Effect::Committed { by, .. } | blaze_dist::work::Effect::Replayed { by })
                                if by.holder == request.lease.holder && by.generation == request.lease.generation && by.seq == request.seq)
                    }) => {
                        let mut work = Vec::new_in(Global);
                        work.extend(
                            self.coordinator.committed_names(request, Global)
                                .iter().map(|name| name.0),
                        );
                        fields.push(("work", json(&work, Global)));
                        emit("result_accepted", &fields);
                    }
                    Answer::Error(error) => {
                        if let Some(why @ (blaze_dist::Refusal::DeadLease | blaze_dist::Refusal::AttemptMoved)) = blaze_dist::Refusal::of(error) {
                            fields.push(("refusal", json(&format!("{why:?}"), Global)));
                            emit("result_refused", &fields);
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
                    let mut digests = Vec::new_in(Global);
                    digests.extend(packs.iter().map(|pack| pack.pack));
                    emit(
                        "published",
                        &[
                            ("unit_digest", json(&unit, Global)),
                            ("execution", json(&request.execution.0, Global)),
                            ("attempt", json(&accepted.0, Global)),
                            ("packs", json(&digests, Global)),
                        ],
                    );
                }
            }
            _ => {}
        }
        self.outcomes(effects);
    }

    fn outcomes(&self, effects: &[(ExecutionId, Effect)]) {
        if std::env::var_os("BLAZE_DIST_EVENTS").is_none() {
            return;
        }
        use crate::telemetry::{emit, json};
        for (execution, effect) in effects {
            if let Effect::Event(item) = effect
                && let blaze_wire::JobEvent::Done(done) = &item.event
            {
                let Some(unit) = self.coordinator.unit(*execution) else { continue };
                emit(
                    "done",
                    &[
                        ("unit_digest", json(&unit, Global)),
                        ("execution", json(&execution.0, Global)),
                        ("attempt", json(&item.attempt.0, Global)),
                        ("outcome", json(&format!("{:?}", done.outcome), Global)),
                    ],
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
        let decoded = decode::<ClientFrame>(body, &Limits::DOCUMENTED);
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
            Ok(_) => (fail("hello/session-binding", alloc), true),
            Err(_) => (fail("canonical-cbor", alloc), true),
        }
    }
}

/// Four-byte big-endian length plus the exact canonical body.
pub fn frame<A: Allocator>(body: &[u8], alloc: A) -> Vec<u8, A> {
    let mut bytes = Vec::with_capacity_in(body.len() + 4, alloc);
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

impl<C: Clock> Drop for Server<C> {
    fn drop(&mut self) {
        let effects = self.coordinator.shutdown(self.clock.now_ms(), Global);
        self.effects.deliver(&mut effects.into_iter());
        if fs::metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_dist::admission::{Admission, AdmissionConfig};
    use blaze_dist::jobs::{ExecutionConfig, Jobs};

    #[test]
    fn effect_sink_moves_one_batch_from_the_callers_allocator() {
        let execution = ExecutionId(7);
        let assignment = blaze_dist::jobs::AssignmentToken {
            execution,
            attempt: blaze_wire::Attempt(8),
        };
        let executor = "one owned executor name".to_owned();
        let original = executor.as_ptr();
        let mut received = Vec::new_in(Global);
        let mut calls = 0;
        blaze_mem::frame::task(blaze_mem::Tag::Plane, |alloc| {
            let mut batch = Vec::new_in(alloc);
            batch.push((execution, Effect::Kill { executor, assignment }));
            batch.push((execution, Effect::NeedLead));
            let mut pending = batch.into_iter();
            let mut sink = |effects: &mut dyn Iterator<Item = (ExecutionId, Effect)>| {
                calls += 1;
                received.extend(effects);
            };
            sink.deliver(&mut pending);
            assert!(pending.next().is_none(), "the one callback consumed the whole batch");
        });
        assert_eq!(calls, 1);
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].0, execution);
        let Effect::Kill { executor, assignment: delivered } = &received[0].1 else {
            panic!("the first effect changed kind");
        };
        assert_eq!(executor.as_ptr(), original, "the callback moves the owned effect without cloning");
        assert_eq!(*delivered, assignment);
        assert_eq!(received[1], (execution, Effect::NeedLead));
    }

    #[test]
    fn framing_uses_the_callers_allocator_without_changing_the_body() {
        blaze_mem::frame::task(blaze_mem::Tag::Plane, |alloc| {
            let framed = frame(&[0, 1, 255], alloc);
            assert!(std::ptr::eq(*framed.allocator(), alloc));
            assert_eq!(framed.as_slice(), &[0, 0, 0, 3, 0, 1, 255]);
        });
    }

    #[test]
    fn transient_accept_errors_leave_the_reactor_available_to_healthy_clients() {
        let tag = random_id().unwrap();
        let path = std::env::temp_dir().join(format!(
            "blaze-accept-{}.sock",
            tag[..8].iter().map(|byte| format!("{byte:02x}")).collect::<String>()
        ));
        let jobs =
            Jobs::new(Admission::new(AdmissionConfig::default()), ExecutionConfig::default());
        let mut server = Server::bind(
            &path,
            jobs,
            Config::default(),
            Arc::new(MonotonicClock::default()),
            |_: &mut dyn Iterator<Item = (ExecutionId, Effect)>| {},
        )
        .unwrap();
        server.accept_errors.extend(
            [
                io::ErrorKind::Interrupted,
                io::ErrorKind::ConnectionAborted,
                io::ErrorKind::ConnectionReset,
            ]
            .into_iter()
            .map(io::Error::from),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let thread = std::thread::spawn(move || server.run(&signal));
        let connected = crate::client::Client::connect(&path, random_id().unwrap());
        stop.store(true, Ordering::Release);
        let result = thread.join().unwrap();
        assert!(result.is_ok(), "transient accept failure ended the reactor: {result:?}");
        assert!(connected.is_ok(), "a healthy client could not connect: {:?}", connected.err());
        assert!(!path.exists());
    }
}
