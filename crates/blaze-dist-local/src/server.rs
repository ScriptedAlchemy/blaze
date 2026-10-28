//! Nonblocking Unix reactor. Socket fragmentation/coalescing is unrelated to CBOR boundaries.

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
    /// Deliver actual core effects to the holder/agent backend.
    fn deliver(&mut self, effects: Vec<(ExecutionId, Effect)>);
}

impl<F: FnMut(Vec<(ExecutionId, Effect)>) + Send> Effects for F {
    fn deliver(&mut self, effects: Vec<(ExecutionId, Effect)>) {
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
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    inode: (u64, u64),
    coordinator: Coordinator,
    clock: Arc<dyn Clock>,
    effects: Box<dyn Effects>,
    #[cfg(test)]
    accept_errors: VecDeque<io::Error>,
}

fn fail(text: &str) -> Vec<u8> {
    encode(&ServerFrame::Error(blaze_wire::WireError::new(ErrorCode::Malformed, text)))
}

/// Read an OS-random instance/session ID without a hashing or timestamp uniqueness guess.
pub fn random_id() -> io::Result<Digest> {
    let mut id = [0; 32];
    File::open("/dev/urandom")?.read_exact(&mut id)?;
    Ok(id)
}

impl Server {
    /// Bind a new path; an existing socket is an error, not permission to unlink another daemon.
    /// Parent directory policy is the caller's; this local socket is owner-only, no new auth layer.
    pub fn bind(
        path: &Path,
        jobs: blaze_dist::jobs::Jobs,
        config: Config,
        clock: Arc<dyn Clock>,
        effects: impl Effects + 'static,
    ) -> Result<Self, Error> {
        if config.max_frame < 1 << 20
            || config.max_frame > Limits::DOCUMENTED.request
            || config.max_connections == 0
            || config.max_sessions == 0
            || config.max_requests == 0
            || config.session_ms == 0
            || config.idle_ms == 0
            || config.replay_bytes < config.max_frame as usize * 2
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
            effects: Box::new(effects),
            #[cfg(test)]
            accept_errors: VecDeque::new(),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        server.listener.set_nonblocking(true)?;
        Ok(server)
    }

    /// Run until shutdown. IO cannot block a timer: reads/writes are nonblocking and each
    /// connection gets a bounded frame budget per pass. Shutdown drops sockets and the registry.
    pub fn run(mut self, stop: Arc<AtomicBool>) -> Result<(), Error> {
        let mut connections = Vec::new();
        while !stop.load(Ordering::Acquire) {
            let now = self.coordinator.now(self.clock.now_ms());
            self.effects.deliver(self.coordinator.tick(now));
            // One accept per loop prevents an accept flood starving existing clients/timers.
            match self.accept() {
                Ok((stream, _)) => {
                    if connections.len() < self.coordinator.config.max_connections {
                        stream.set_nonblocking(true)?;
                        connections.push(Connection {
                            stream,
                            input: Vec::new(),
                            output: VecDeque::new(),
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
        let effects = self.coordinator.shutdown(self.clock.now_ms());
        self.effects.deliver(effects);
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
                    c.output.push_back(frame(&fail("frame-size")));
                    c.closing = true;
                    c.input.clear();
                } else if c.input.len() >= size + 4 {
                    let body = c.input[4..size + 4].to_vec();
                    c.input.drain(..size + 4);
                    let (reply, close) = self.handle(now, c, &body);
                    c.output.push_back(frame(&reply));
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

    fn handle(&mut self, now: u64, c: &mut Connection, body: &[u8]) -> (Vec<u8>, bool) {
        let decoded = decode::<ClientFrame>(body, &Limits::DOCUMENTED);
        match decoded {
            Ok(ClientFrame::Hello(hello)) if c.session.is_none() => {
                match self.coordinator.hello(now, &hello) {
                    Ok(welcome) => {
                        c.session = Some(welcome.session);
                        (encode(&ServerFrame::Welcome(welcome)), false)
                    }
                    Err(e) => (encode(&ServerFrame::Error(e)), true),
                }
            }
            Ok(ClientFrame::Request(request)) if c.session == Some(request.session) => {
                // A release ends the lifetime in the coordinator, which journals this answer
                // before it is written. The connection is closed after that answer, because
                // the session it named no longer exists.
                let releases = matches!(request.command, Command::Release(_));
                match self.coordinator.request(now, &request) {
                    Ok((reply, effects)) => {
                        self.effects.deliver(effects);
                        (reply, releases)
                    }
                    Err(e) => (encode(&ServerFrame::Error(e)), true),
                }
            }
            Ok(_) => (fail("hello/session-binding"), true),
            Err(_) => (fail("canonical-cbor"), true),
        }
    }
}

/// Four-byte big-endian length plus the exact canonical body.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(body.len() + 4);
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

impl Drop for Server {
    fn drop(&mut self) {
        let effects = self.coordinator.shutdown(self.clock.now_ms());
        self.effects.deliver(effects);
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
            |_| {},
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
        let thread = std::thread::spawn(move || server.run(signal));
        let connected = crate::client::Client::connect(&path, random_id().unwrap());
        stop.store(true, Ordering::Release);
        let result = thread.join().unwrap();
        assert!(result.is_ok(), "transient accept failure ended the reactor: {result:?}");
        assert!(connected.is_ok(), "a healthy client could not connect: {:?}", connected.err());
        assert!(!path.exists());
    }
}
