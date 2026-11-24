//! Non-blocking HTTP/1.1 cache server over `blaze-sys` (`poll` + non-blocking TCP).
//!
//! Single-threaded event loop with bounded concurrency (`Resources::connections`), bounded
//! header/body limits (`Limits`), per-request idle timeouts, write-progress stall detection
//! (`progress::WriteProgress`), periodic unleased temporary-file sweeps, and graceful shutdown
//! through a wakeup pipe.

pub(crate) mod progress;
pub(crate) mod publication;

use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use core::time::Duration;

use blaze_own::{Arc, String, Vec, format_in};
use blaze_wire::{Digest, Limits, WireAlloc, WireString};

use self::progress::WriteProgress;
use self::publication::Temporary;
use crate::blob::{Kind, ObjectName, hex_in, parse_digest};
use crate::error::{Error, ServeError, reason_phrase_for};
use crate::store::Store;
use crate::sys::{
    Errno, Fd, OwnedFd, PollEvents, PollFd, SysError, SystemClock, TcpServer,
    configure_peer_socket, dup_cloexec, duration_to_nanos, duration_to_timespec, poll_fds,
    pread_some, read_some, recv_nonblocking, send_nonblocking, shutdown_write, wakeup_pipe,
    write_fd_best_effort,
};

/// Maximum HTTP request header block size (`16 KiB`).
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Maximum length of a single chunk-size or trailer line in chunked transfer encoding.
const MAX_CHUNK_LINE_BYTES: usize = 1024;

/// Maximum number of trailer headers in a chunked request body.
const MAX_TRAILER_LINES: usize = 32;

/// Streaming buffer size for `GET /v1/blob/...` responses (`64 KiB`).
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// Receive scratch buffer size per read call (`16 KiB`).
const RECV_BUF_BYTES: usize = 16 * 1024;

/// Backoff duration after transient `accept` errors (`100 ms`).
const ACCEPT_BACKOFF_NS: u64 = 100_000_000;

/// Operational resource limits for a [`Server`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resources {
    /// Maximum time a request or stalled write may go without making progress.
    pub idle: Duration,
    /// Maximum simultaneous live TCP connections.
    pub connections: usize,
    /// Interval between background sweeps of stale `.tmp` files.
    pub sweep_every: Duration,
    /// Minimum age before an unleased `.tmp` file is swept.
    pub temporary_age: Duration,
}

impl Resources {
    /// Documented production defaults.
    pub const DOCUMENTED: Self = Self {
        idle: Duration::from_secs(30),
        connections: 256,
        sweep_every: Duration::from_secs(300),
        temporary_age: Duration::from_secs(3600),
    };
}

/// Complete configuration for starting a [`Server`].
#[derive(Clone, Debug)]
pub struct Config {
    /// Directory holding `packs/` and `manifests/`.
    pub store: WireString,
    /// TCP socket address to bind.
    pub listen: SocketAddr,
    /// Wire and blob byte limits.
    pub limits: Limits,
    /// Connection, idle, and sweep limits.
    pub resources: Resources,
}

impl Config {
    /// Builds a loopback configuration serving `dir` on `127.0.0.1:port` with documented limits.
    pub fn at(dir: &str, port: u16, alloc: WireAlloc) -> Self {
        let mut store = String::with_capacity_in(dir.len(), alloc);
        store.push_str(dir);
        Self {
            store,
            listen: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            limits: Limits::DOCUMENTED,
            resources: Resources::DOCUMENTED,
        }
    }
}

/// Handle capable of stopping a running [`Server::run`] loop cleanly from another thread.
pub struct ShutdownHandle {
    wake_write: OwnedFd,
}

impl ShutdownHandle {
    /// Signals the server loop to exit promptly.
    pub fn stop(&self) {
        write_fd_best_effort(self.wake_write.fd(), b"x");
    }
}

/// Bound HTTP/1.1 cache server ready to execute [`Server::run`].
pub struct Server {
    store: Arc<Store, WireAlloc>,
    listener: TcpServer,
    wake_read: OwnedFd,
    wake_write: OwnedFd,
    resources: Resources,
    alloc: WireAlloc,
    clock: SystemClock,
}

impl Server {
    /// Opens the store and binds the TCP listener described by `config`.
    pub fn bind(config: Config, alloc: WireAlloc) -> Result<Self, ServeError> {
        let store = Store::open(config.store.as_str(), config.limits, alloc.clone())
            .map_err(ServeError::Store)?;
        let store = Arc::new_in(store, alloc.clone());
        Self::bind_on(store, config.listen, config.resources, alloc)
    }

    /// Binds a TCP listener serving an already-open `store`.
    pub fn bind_on(
        store: Arc<Store, WireAlloc>,
        listen: SocketAddr,
        resources: Resources,
        alloc: WireAlloc,
    ) -> Result<Self, ServeError> {
        let listener = TcpServer::bind_addr(listen)
            .map_err(|e| ServeError::Listen { address: listen, source: SysError(e) })?;
        let (wake_read, wake_write) =
            wakeup_pipe().map_err(|e| ServeError::Pipe { source: SysError(e) })?;
        Ok(Self {
            store,
            listener,
            wake_read,
            wake_write,
            resources,
            alloc,
            clock: SystemClock,
        })
    }

    /// Resolved local socket address (with concrete port if bound to port 0).
    pub const fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr()
    }

    /// The underlying [`Store`].
    pub fn store(&self) -> &Arc<Store, WireAlloc> {
        &self.store
    }

    /// Creates a [`ShutdownHandle`] that stops this server's [`Server::run`] loop.
    pub fn shutdown_handle(&self) -> Result<ShutdownHandle, ServeError> {
        let dup = dup_cloexec(self.wake_write.fd())
            .map_err(|e| ServeError::Pipe { source: SysError(e) })?;
        Ok(ShutdownHandle { wake_write: dup })
    }

    /// Runs the non-blocking HTTP/1.1 `poll()` loop until stopped via [`ShutdownHandle::stop`].
    pub fn run(self) -> Result<(), ServeError> {
        let idle_ns = duration_to_nanos(self.resources.idle);
        let sweep_every_ns = duration_to_nanos(self.resources.sweep_every);
        let mut next_sweep_ns = self
            .clock
            .monotonic_nanos()
            .unwrap_or(0)
            .saturating_add(sweep_every_ns);
        let mut accept_backoff_until_ns: Option<u64> = None;
        let max_conns = self.resources.connections.max(1);

        let mut conns: Vec<Conn, WireAlloc> = Vec::with_capacity_in(max_conns.min(64), self.alloc.clone());
        let mut polls: Vec<PollFd, WireAlloc> = Vec::with_capacity_in(max_conns.min(64).saturating_add(2), self.alloc.clone());

        loop {
            let now_ns = self.clock.monotonic_nanos().unwrap_or(0);

            if now_ns >= next_sweep_ns {
                let _ = self.store.sweep_temporaries(self.resources.temporary_age);
                next_sweep_ns = now_ns.saturating_add(sweep_every_ns);
            }

            if let Some(until) = accept_backoff_until_ns {
                if now_ns >= until {
                    accept_backoff_until_ns = None;
                }
            }

            // Build poll set: [0] = shutdown wakeup pipe, optional [1] = listener, then active conns.
            polls.clear();
            polls.push(PollFd::new(self.wake_read.fd(), PollEvents::IN));

            let listening = conns.len() < max_conns && accept_backoff_until_ns.is_none();
            if listening {
                polls.push(PollFd::new(self.listener.fd(), PollEvents::IN));
            }
            let conn_base = polls.len();
            for conn in &conns {
                polls.push(PollFd::new(conn.fd.fd(), conn.interest()));
            }

            // Compute nearest deadline across sweep, accept backoff, and all active connections.
            let mut next_deadline_ns = next_sweep_ns;
            if let Some(until) = accept_backoff_until_ns {
                next_deadline_ns = next_deadline_ns.min(until);
            }
            for conn in &conns {
                if let Some(d) = conn.deadline_ns() {
                    next_deadline_ns = next_deadline_ns.min(d);
                }
            }
            let wait_ns = next_deadline_ns.saturating_sub(now_ns);
            let timeout = duration_to_timespec(Duration::from_nanos(wait_ns));

            let ready = poll_fds(polls.as_mut_slice(), Some(timeout))
                .map_err(|e| ServeError::Loop { source: SysError(e) })?;

            let now_ns = self.clock.monotonic_nanos().unwrap_or(0);

            // Check shutdown pipe first.
            if let Some(wake_pfd) = polls.first() {
                if wake_pfd.found().contains(PollEvents::IN)
                    || wake_pfd.found().contains(PollEvents::HUP)
                {
                    let mut drain = [0u8; 32];
                    let _ = read_some(self.wake_read.fd(), &mut drain);
                    break;
                }
            }

            // Accept new connections if listener is ready and capacity allows.
            if ready > 0 && listening {
                if let Some(listen_pfd) = polls.get(1) {
                    if listen_pfd.found().contains(PollEvents::IN) {
                        while conns.len() < max_conns {
                            match self.listener.accept_one() {
                                Ok(peer_fd) => {
                                    configure_peer_socket(peer_fd.fd());
                                    conns.push(Conn::new(
                                        peer_fd,
                                        now_ns,
                                        idle_ns,
                                        self.alloc.clone(),
                                    ));
                                }
                                Err(Errno::Again | Errno::Intr) => break,
                                Err(err) if is_transient_accept_error(err) => {
                                    accept_backoff_until_ns =
                                        Some(now_ns.saturating_add(ACCEPT_BACKOFF_NS));
                                    break;
                                }
                                Err(err) => {
                                    return Err(ServeError::Loop { source: SysError(err) });
                                }
                            }
                        }
                    }
                }
            }

            // Drive active connections in reverse so swap-remove never shifts unvisited poll indices.
            let mut idx = polls.len().saturating_sub(conn_base);
            while idx > 0 {
                idx = idx.saturating_sub(1);
                let revents = polls
                    .get(conn_base.saturating_add(idx))
                    .map_or(PollEvents::NONE, PollFd::found);
                let keep = if let Some(conn) = conns.get_mut(idx) {
                    conn.step(&self.store, revents, now_ns, idle_ns)
                } else {
                    false
                };
                if !keep {
                    let _ = conns.remove_swap(idx);
                }
            }
        }
        Ok(())
    }
}

/// Convenience wrapper: binds `config` and runs the server until stopped or fatally errored.
pub fn serve(config: Config, alloc: WireAlloc) -> Result<(), ServeError> {
    let server = Server::bind(config, alloc.clone())?;
    let banner = format_in!(
        alloc,
        "blaze-serve: serving {} on http://{}, routes /v1, codec {}, no authentication\n",
        server.store().root(),
        server.local_addr(),
        blaze_wire::VERSION
    );
    write_fd_best_effort(Fd::new(1), banner.as_bytes());
    server.run()
}

/// Convenience wrapper: binds `listen` over an existing `store` and runs the server.
pub fn serve_on(
    store: Arc<Store, WireAlloc>,
    listen: SocketAddr,
    resources: Resources,
    alloc: WireAlloc,
) -> Result<(), ServeError> {
    let server = Server::bind_on(store, listen, resources, alloc)?;
    server.run()
}

fn is_transient_accept_error(err: Errno) -> bool {
    matches!(
        err,
        Errno::ConnAborted
            | Errno::ConnReset
            | Errno::NoMem
            | Errno::MFile
            | Errno::NFile
            | Errno::NoBufs
            | Errno::NetDown
            | Errno::Proto
            | Errno::NoProtoOpt
            | Errno::HostDown
            | Errno::NoNet
            | Errno::HostUnreach
            | Errno::OpNotSupp
            | Errno::NetUnreach
    )
}

// ---------------------------------------------------------------------------------------------
// Per-Connection HTTP/1.1 State Machine
// ---------------------------------------------------------------------------------------------

struct Conn {
    fd: OwnedFd,
    alloc: WireAlloc,
    in_buf: Vec<u8, WireAlloc>,
    in_pos: usize,
    keep_alive: bool,
    state: ConnState,
}

enum ConnState {
    ReadingHeaders {
        deadline_ns: u64,
    },
    ReadingBlobBody {
        name: ObjectName,
        temp: Temporary,
        framing: BodyFraming,
        deadline_ns: u64,
    },
    ReadingListBody {
        name: ObjectName,
        expected: Option<Digest>,
        body: Vec<u8, WireAlloc>,
        framing: BodyFraming,
        deadline_ns: u64,
    },
    Writing {
        out_buf: Vec<u8, WireAlloc>,
        out_pos: usize,
        file: Option<FileTail>,
        progress: WriteProgress,
    },
}

struct FileTail {
    fd: OwnedFd,
    offset: u64,
    remaining: u64,
}

#[derive(Debug)]
enum BodyFraming {
    Fixed { remaining: u64 },
    Chunked(ChunkedState),
}

#[derive(Debug)]
enum ChunkedState {
    SizeLine,
    Data { remaining: u64 },
    DataCr,
    DataLf,
    Trailers { lines: usize },
    Done,
}

struct ParsedRequest<'a> {
    method: &'a str,
    target: &'a str,
    keep_alive: bool,
    framing: BodyFraming,
    if_match: Option<Digest>,
    expect_continue: bool,
}

impl Conn {
    fn new(fd: OwnedFd, now_ns: u64, idle_ns: u64, alloc: WireAlloc) -> Self {
        Self {
            fd,
            in_buf: Vec::new_in(alloc.clone()),
            in_pos: 0,
            keep_alive: true,
            state: ConnState::ReadingHeaders { deadline_ns: now_ns.saturating_add(idle_ns) },
            alloc,
        }
    }

    fn interest(&self) -> PollEvents {
        match &self.state {
            ConnState::ReadingHeaders { .. }
            | ConnState::ReadingBlobBody { .. }
            | ConnState::ReadingListBody { .. } => PollEvents::IN,
            ConnState::Writing { .. } => PollEvents::OUT,
        }
    }

    fn deadline_ns(&self) -> Option<u64> {
        match &self.state {
            ConnState::ReadingHeaders { deadline_ns }
            | ConnState::ReadingBlobBody { deadline_ns, .. }
            | ConnState::ReadingListBody { deadline_ns, .. } => Some(*deadline_ns),
            ConnState::Writing { progress, .. } => progress.deadline_ns(),
        }
    }

    /// Advances the connection; returns `false` when the connection should be closed and dropped.
    fn step(&mut self, store: &Store, revents: PollEvents, now_ns: u64, idle_ns: u64) -> bool {
        if revents.contains(PollEvents::ERR) || revents.contains(PollEvents::NVAL) {
            return false;
        }

        // Loop boundedly so pipelined input or immediate transitions complete in one poll turn.
        for _ in 0..8 {
            match &mut self.state {
                ConnState::ReadingHeaders { deadline_ns } => {
                    if now_ns >= *deadline_ns {
                        return false;
                    }
                    if revents.contains(PollEvents::IN) || revents.contains(PollEvents::HUP) {
                        match self.recv_more(now_ns, idle_ns) {
                            RecvOutcome::Bytes(n) if n > 0 => {
                                if let ConnState::ReadingHeaders { deadline_ns } = &mut self.state {
                                    *deadline_ns = now_ns.saturating_add(idle_ns);
                                }
                            }
                            RecvOutcome::Bytes(_) | RecvOutcome::WouldBlock => {}
                            RecvOutcome::Eof => {
                                if self.unread().is_empty() {
                                    return false;
                                }
                            }
                            RecvOutcome::Closed => return false,
                        }
                    }
                    if !self.try_parse_headers(store, now_ns, idle_ns) {
                        if revents.contains(PollEvents::HUP) && self.unread().is_empty() {
                            return false;
                        }
                        return true;
                    }
                }
                ConnState::ReadingBlobBody { deadline_ns, .. } => {
                    if now_ns >= *deadline_ns {
                        self.keep_alive = false;
                        self.queue_error(
                            Error::new_static(
                                crate::error::ErrorCode::Malformed,
                                "timed out waiting for pack upload body",
                            ),
                            408,
                            idle_ns,
                        );
                        continue;
                    }
                    let mut got_eof = false;
                    if revents.contains(PollEvents::IN) || revents.contains(PollEvents::HUP) {
                        match self.recv_more(now_ns, idle_ns) {
                            RecvOutcome::Bytes(n) if n > 0 => {
                                if let ConnState::ReadingBlobBody { deadline_ns, .. } = &mut self.state {
                                    *deadline_ns = now_ns.saturating_add(idle_ns);
                                }
                            }
                            RecvOutcome::Bytes(_) | RecvOutcome::WouldBlock => {}
                            RecvOutcome::Eof => got_eof = true,
                            RecvOutcome::Closed => return false,
                        }
                    }
                    if !self.pump_blob_body(store, got_eof, idle_ns) {
                        return !got_eof;
                    }
                }
                ConnState::ReadingListBody { deadline_ns, .. } => {
                    if now_ns >= *deadline_ns {
                        self.keep_alive = false;
                        self.queue_error(
                            Error::new_static(
                                crate::error::ErrorCode::Malformed,
                                "timed out waiting for manifest upload body",
                            ),
                            408,
                            idle_ns,
                        );
                        continue;
                    }
                    let mut got_eof = false;
                    if revents.contains(PollEvents::IN) || revents.contains(PollEvents::HUP) {
                        match self.recv_more(now_ns, idle_ns) {
                            RecvOutcome::Bytes(n) if n > 0 => {
                                if let ConnState::ReadingListBody { deadline_ns, .. } = &mut self.state {
                                    *deadline_ns = now_ns.saturating_add(idle_ns);
                                }
                            }
                            RecvOutcome::Bytes(_) | RecvOutcome::WouldBlock => {}
                            RecvOutcome::Eof => got_eof = true,
                            RecvOutcome::Closed => return false,
                        }
                    }
                    if !self.pump_list_body(store, got_eof, idle_ns) {
                        return !got_eof;
                    }
                }
                ConnState::Writing { .. } => {
                    return self.pump_write(now_ns, idle_ns);
                }
            }
        }
        true
    }

    fn unread(&self) -> &[u8] {
        self.in_buf.get(self.in_pos..).unwrap_or(&[])
    }

    fn consume(&mut self, n: usize) {
        self.in_pos = self.in_pos.saturating_add(n).min(self.in_buf.len());
        if self.in_pos == self.in_buf.len() {
            self.in_buf.clear();
            self.in_pos = 0;
        } else if self.in_pos >= 8192 {
            let remaining = self.in_buf.len().saturating_sub(self.in_pos);
            self.in_buf.copy_within(self.in_pos.., 0);
            self.in_buf.truncate(remaining);
            self.in_pos = 0;
        }
    }

    fn recv_more(&mut self, _now_ns: u64, _idle_ns: u64) -> RecvOutcome {
        let mut scratch = [0u8; RECV_BUF_BYTES];
        let mut total = 0usize;
        for _ in 0..4 {
            match recv_nonblocking(self.fd.fd(), &mut scratch) {
                Ok(0) => return if total > 0 { RecvOutcome::Bytes(total) } else { RecvOutcome::Eof },
                Ok(n) => {
                    if let Some(chunk) = scratch.get(..n) {
                        self.in_buf.extend_from_slice(chunk);
                    }
                    total = total.saturating_add(n);
                    if n < scratch.len() || self.in_buf.len() >= STREAM_CHUNK_BYTES {
                        break;
                    }
                }
                Err(Errno::Again) => {
                    return if total > 0 {
                        RecvOutcome::Bytes(total)
                    } else {
                        RecvOutcome::WouldBlock
                    };
                }
                Err(Errno::Intr) => continue,
                Err(_) => return RecvOutcome::Closed,
            }
        }
        RecvOutcome::Bytes(total)
    }

    /// Parses headers if `\r\n\r\n` has arrived; returns `true` if state transitioned.
    fn try_parse_headers(&mut self, store: &Store, now_ns: u64, idle_ns: u64) -> bool {
        let unread = self.unread();
        let Some(end) = find_header_end(unread) else {
            if unread.len() > MAX_HEADER_BYTES {
                self.keep_alive = false;
                self.queue_error(Error::malformed("HTTP headers exceed 16 KiB limit"), 400, idle_ns);
                return true;
            }
            return false;
        };
        if end > MAX_HEADER_BYTES {
            self.keep_alive = false;
            self.queue_error(Error::malformed("HTTP headers exceed 16 KiB limit"), 400, idle_ns);
            return true;
        }

        // Copy header slice into a small local buffer so we can consume `in_buf` cleanly.
        let mut hdr_copy = Vec::with_capacity_in(end, self.alloc.clone());
        if let Some(slice) = unread.get(..end) {
            hdr_copy.extend_from_slice(slice);
        }
        self.consume(end.saturating_add(4));

        let parsed = match parse_request_headers(hdr_copy.as_slice()) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.keep_alive = false;
                let status = err.status_code();
                self.queue_error(err, status, idle_ns);
                return true;
            }
        };

        self.keep_alive = parsed.keep_alive;
        let path = parsed.target.split('?').next().unwrap_or(parsed.target);

        if path == "/" {
            self.queue_inline_response(200, "text/plain; charset=utf-8", b"blaze-serve\n", None, idle_ns);
            return true;
        }

        if let Some(raw_name) = path.strip_prefix("/v1/blob/") {
            match parsed.method {
                "GET" => {
                    let name = match ObjectName::parse_kind(raw_name, Kind::Pack) {
                        Ok(n) => n,
                        Err(_) => {
                            let err = Error::not_found_owned(format_in!(
                                self.alloc.clone(),
                                "invalid pack route {raw_name}"
                            ));
                            self.queue_error(err, 404, idle_ns);
                            return true;
                        }
                    };
                    match store.open_blob(&name) {
                        Ok(Some((fd, len))) => {
                            let etag = hex_in(name.digest(), self.alloc.clone());
                            let etag_hdr = format_in!(self.alloc.clone(), "etag: \"{}\"\r\n", etag.as_str());
                            self.queue_file_response(
                                200,
                                "application/octet-stream",
                                len,
                                etag_hdr.as_str(),
                                fd,
                                idle_ns,
                            );
                        }
                        Ok(None) => {
                            let err = Error::not_found_owned(format_in!(
                                self.alloc.clone(),
                                "no such blob {raw_name}"
                            ));
                            let st = err.status_code();
                            self.queue_error(err, st, idle_ns);
                        }
                        Err(err) => {
                            let st = err.status_code();
                            self.queue_error(err, st, idle_ns);
                        }
                    }
                    return true;
                }
                "PUT" => {
                    let name = match ObjectName::parse_kind(raw_name, Kind::Pack) {
                        Ok(n) => n,
                        Err(_) => {
                            self.keep_alive = false;
                            let err = Error::not_found_owned(format_in!(
                                self.alloc.clone(),
                                "invalid pack route {raw_name}"
                            ));
                            self.queue_error(err, 404, idle_ns);
                            return true;
                        }
                    };
                    if let BodyFraming::Fixed { remaining } = parsed.framing {
                        if remaining > store.limits().blob {
                            self.keep_alive = false;
                            let err = Error::too_large_owned(format_in!(
                                self.alloc.clone(),
                                "pack content-length {} exceeds {}-byte limit",
                                remaining,
                                store.limits().blob
                            ));
                            let st = err.status_code();
                            self.queue_error(err, st, idle_ns);
                            return true;
                        }
                    }
                    let temp = match Temporary::open(store, &name) {
                        Ok(t) => t,
                        Err(e) => {
                            self.keep_alive = false;
                            let st = e.status_code();
                            self.queue_error(e, st, idle_ns);
                            return true;
                        }
                    };
                    if parsed.expect_continue {
                        let _ = send_nonblocking(self.fd.fd(), b"HTTP/1.1 100 Continue\r\n\r\n");
                    }
                    self.state = ConnState::ReadingBlobBody {
                        name,
                        temp,
                        framing: parsed.framing,
                        deadline_ns: now_ns.saturating_add(idle_ns),
                    };
                    return true;
                }
                _ => {
                    self.keep_alive = false;
                    self.queue_error(
                        Error::malformed("blob route supports GET and PUT only"),
                        400,
                        idle_ns,
                    );
                    return true;
                }
            }
        }

        if let Some(raw_name) = path.strip_prefix("/v1/list/") {
            match parsed.method {
                "GET" => {
                    let name = match ObjectName::parse_kind(raw_name, Kind::Manifest) {
                        Ok(n) => n,
                        Err(_) => {
                            let err = Error::not_found_owned(format_in!(
                                self.alloc.clone(),
                                "invalid manifest route {raw_name}"
                            ));
                            self.queue_error(err, 404, idle_ns);
                            return true;
                        }
                    };
                    match store.read_small(&name, u64::from(store.limits().request), self.alloc.clone()) {
                        Ok(Some(bytes)) => {
                            self.queue_inline_response(
                                200,
                                "text/plain; charset=utf-8",
                                bytes.as_slice(),
                                None,
                                idle_ns,
                            );
                        }
                        Ok(None) => {
                            let err = Error::not_found_owned(format_in!(
                                self.alloc.clone(),
                                "no such manifest {raw_name}"
                            ));
                            let st = err.status_code();
                            self.queue_error(err, st, idle_ns);
                        }
                        Err(err) => {
                            let st = err.status_code();
                            self.queue_error(err, st, idle_ns);
                        }
                    }
                    return true;
                }
                "PUT" => {
                    let name = match ObjectName::parse_kind(raw_name, Kind::Manifest) {
                        Ok(n) => n,
                        Err(_) => {
                            self.keep_alive = false;
                            let err = Error::not_found_owned(format_in!(
                                self.alloc.clone(),
                                "invalid manifest route {raw_name}"
                            ));
                            self.queue_error(err, 404, idle_ns);
                            return true;
                        }
                    };
                    if let BodyFraming::Fixed { remaining } = parsed.framing {
                        let req_cap = u64::from(store.limits().request);
                        if remaining > req_cap {
                            self.keep_alive = false;
                            let err = Error::too_large_owned(format_in!(
                                self.alloc.clone(),
                                "manifest content-length {} exceeds {}-byte limit",
                                remaining,
                                req_cap
                            ));
                            let st = err.status_code();
                            self.queue_error(err, st, idle_ns);
                            return true;
                        }
                    }
                    if parsed.expect_continue {
                        let _ = send_nonblocking(self.fd.fd(), b"HTTP/1.1 100 Continue\r\n\r\n");
                    }
                    self.state = ConnState::ReadingListBody {
                        name,
                        expected: parsed.if_match,
                        body: Vec::new_in(self.alloc.clone()),
                        framing: parsed.framing,
                        deadline_ns: now_ns.saturating_add(idle_ns),
                    };
                    return true;
                }
                _ => {
                    self.keep_alive = false;
                    self.queue_error(
                        Error::malformed("list route supports GET and PUT only"),
                        400,
                        idle_ns,
                    );
                    return true;
                }
            }
        }

        let err = Error::not_found_owned(format_in!(self.alloc.clone(), "unknown route {path}"));
        let st = err.status_code();
        self.queue_error(err, st, idle_ns);
        true
    }

    fn pump_blob_body(&mut self, store: &Store, got_eof: bool, idle_ns: u64) -> bool {
        let blob_limit = store.limits().blob;
        let alloc = self.alloc.clone();
        let outcome = {
            let ConnState::ReadingBlobBody { temp, framing, .. } = &mut self.state else {
                return false;
            };
            let unread = self.in_buf.get(self.in_pos..).unwrap_or(&[]);
            let mut consumed = 0usize;
            let res = drain_body(unread, &mut consumed, framing, |chunk| {
                temp.write_chunk(chunk, blob_limit)
            });
            (consumed, res)
        };
        let (consumed, res) = outcome;
        if consumed > 0 {
            self.consume(consumed);
        }
        match res {
            Ok(BodyStep::NeedMore) => {
                if got_eof {
                    // Client disconnected mid-body; dropping state unlinks Temporary.
                    self.keep_alive = false;
                    self.queue_error(
                        Error::malformed("unexpected EOF inside pack upload body"),
                        400,
                        idle_ns,
                    );
                    return true;
                }
                false
            }
            Ok(BodyStep::Complete) => {
                let placeholder = ConnState::ReadingHeaders { deadline_ns: 0 };
                let prev = core::mem::replace(&mut self.state, placeholder);
                let ConnState::ReadingBlobBody { name, temp, .. } = prev else {
                    return false;
                };
                match temp.publish(store, &name) {
                    Ok(wrote) => {
                        let name_s = name.as_string_in(alloc.clone());
                        let verb = if wrote { "written" } else { "already there" };
                        let hdr = format_in!(
                            alloc,
                            "x-blaze-blob: {}: {}\r\n",
                            name_s.as_str(),
                            verb
                        );
                        self.queue_empty_response(204, Some(hdr.as_str()), idle_ns);
                    }
                    Err(err) => {
                        let st = err.status_code();
                        self.queue_error(err, st, idle_ns);
                    }
                }
                true
            }
            Err(err) => {
                self.keep_alive = false;
                let st = err.status_code();
                self.queue_error(err, st, idle_ns);
                true
            }
        }
    }

    fn pump_list_body(&mut self, store: &Store, got_eof: bool, idle_ns: u64) -> bool {
        let req_limit = u64::from(store.limits().request);
        let alloc = self.alloc.clone();
        let outcome = {
            let ConnState::ReadingListBody { body, framing, .. } = &mut self.state else {
                return false;
            };
            let unread = self.in_buf.get(self.in_pos..).unwrap_or(&[]);
            let mut consumed = 0usize;
            let res = drain_body(unread, &mut consumed, framing, |chunk| {
                let next_len = u64::try_from(body.len().saturating_add(chunk.len()))
                    .unwrap_or(u64::MAX);
                if next_len > req_limit {
                    return Err(Error::too_large_owned(format_in!(
                        alloc.clone(),
                        "manifest body exceeds {req_limit}-byte limit"
                    )));
                }
                body.extend_from_slice(chunk);
                Ok(())
            });
            (consumed, res)
        };
        let (consumed, res) = outcome;
        if consumed > 0 {
            self.consume(consumed);
        }
        match res {
            Ok(BodyStep::NeedMore) => {
                if got_eof {
                    self.keep_alive = false;
                    self.queue_error(
                        Error::malformed("unexpected EOF inside manifest upload body"),
                        400,
                        idle_ns,
                    );
                    return true;
                }
                false
            }
            Ok(BodyStep::Complete) => {
                let placeholder = ConnState::ReadingHeaders { deadline_ns: 0 };
                let prev = core::mem::replace(&mut self.state, placeholder);
                let ConnState::ReadingListBody { name, expected, body, .. } = prev else {
                    return false;
                };
                match store.repair_list(&name, expected, body.as_slice()) {
                    Ok(true) => self.queue_empty_response(204, None, idle_ns),
                    Ok(false) => self.queue_empty_response(412, None, idle_ns),
                    Err(err) => {
                        let st = err.status_code();
                        self.queue_error(err, st, idle_ns);
                    }
                }
                true
            }
            Err(err) => {
                self.keep_alive = false;
                let st = err.status_code();
                self.queue_error(err, st, idle_ns);
                true
            }
        }
    }

    fn pump_write(&mut self, now_ns: u64, idle_ns: u64) -> bool {
        let ConnState::Writing { out_buf, out_pos, file, progress } = &mut self.state else {
            return true;
        };
        if progress.is_expired(now_ns) {
            return false;
        }
        for _ in 0..8 {
            if *out_pos >= out_buf.len() {
                out_buf.clear();
                *out_pos = 0;
                if let Some(tail) = file {
                    if tail.remaining > 0 {
                        let want = usize::try_from(
                            tail.remaining.min(u64::try_from(STREAM_CHUNK_BYTES).unwrap_or(65536)),
                        )
                        .unwrap_or(STREAM_CHUNK_BYTES);
                        out_buf.resize(want, 0);
                        match pread_some(tail.fd.fd(), out_buf.as_mut_slice(), tail.offset) {
                            Ok(0) => return false,
                            Ok(n) => {
                                out_buf.truncate(n);
                                let n_u64 = u64::try_from(n).unwrap_or(0);
                                tail.offset = tail.offset.saturating_add(n_u64);
                                tail.remaining = tail.remaining.saturating_sub(n_u64);
                            }
                            Err(_) => return false,
                        }
                    } else {
                        *file = None;
                    }
                }
            }

            if *out_pos >= out_buf.len() && file.is_none() {
                // Entire response sent!
                if self.keep_alive {
                    self.state = ConnState::ReadingHeaders {
                        deadline_ns: now_ns.saturating_add(idle_ns),
                    };
                    return true;
                }
                let _ = shutdown_write(self.fd.fd());
                return false;
            }

            let pending = out_buf.get(*out_pos..).unwrap_or(&[]);
            match send_nonblocking(self.fd.fd(), pending) {
                Ok(0) => return false,
                Ok(wrote) => {
                    *out_pos = out_pos.saturating_add(wrote);
                    progress.on_written(wrote);
                }
                Err(Errno::Again) => {
                    progress.on_stalled(now_ns);
                    return !progress.is_expired(now_ns);
                }
                Err(Errno::Intr) => continue,
                Err(_) => return false,
            }
        }
        true
    }

    fn queue_inline_response(
        &mut self,
        status: u16,
        content_type: &str,
        body: &[u8],
        extra_headers: Option<&str>,
        idle_ns: u64,
    ) {
        let conn_hdr = if self.keep_alive { "keep-alive" } else { "close" };
        let extra = extra_headers.unwrap_or("");
        let head = format_in!(
            self.alloc.clone(),
            "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\n{}connection: {}\r\n\r\n",
            status,
            reason_phrase_for(status),
            content_type,
            body.len(),
            extra,
            conn_hdr
        );
        let mut out = Vec::with_capacity_in(head.len().saturating_add(body.len()), self.alloc.clone());
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(body);
        self.state = ConnState::Writing {
            out_buf: out,
            out_pos: 0,
            file: None,
            progress: WriteProgress::new(idle_ns),
        };
    }

    fn queue_empty_response(&mut self, status: u16, extra_headers: Option<&str>, idle_ns: u64) {
        let conn_hdr = if self.keep_alive { "keep-alive" } else { "close" };
        let extra = extra_headers.unwrap_or("");
        let head = format_in!(
            self.alloc.clone(),
            "HTTP/1.1 {} {}\r\ncontent-length: 0\r\n{}connection: {}\r\n\r\n",
            status,
            reason_phrase_for(status),
            extra,
            conn_hdr
        );
        let mut out = Vec::with_capacity_in(head.len(), self.alloc.clone());
        out.extend_from_slice(head.as_bytes());
        self.state = ConnState::Writing {
            out_buf: out,
            out_pos: 0,
            file: None,
            progress: WriteProgress::new(idle_ns),
        };
    }

    fn queue_file_response(
        &mut self,
        status: u16,
        content_type: &str,
        len: u64,
        extra_headers: &str,
        fd: OwnedFd,
        idle_ns: u64,
    ) {
        let conn_hdr = if self.keep_alive { "keep-alive" } else { "close" };
        let head = format_in!(
            self.alloc.clone(),
            "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\n{}connection: {}\r\n\r\n",
            status,
            reason_phrase_for(status),
            content_type,
            len,
            extra_headers,
            conn_hdr
        );
        let mut out = Vec::with_capacity_in(head.len(), self.alloc.clone());
        out.extend_from_slice(head.as_bytes());
        self.state = ConnState::Writing {
            out_buf: out,
            out_pos: 0,
            file: Some(FileTail { fd, offset: 0, remaining: len }),
            progress: WriteProgress::new(idle_ns),
        };
    }

    fn queue_error(&mut self, error: Error, status: u16, idle_ns: u64) {
        let retry_hdr = error.retry_after_ms.map(|ms| {
            let secs = ms.div_ceil(1000).max(1);
            format_in!(self.alloc.clone(), "retry-after: {secs}\r\n")
        });
        let wire = error.into_wire(self.alloc.clone());
        let encoded = blaze_wire::encode(&wire, self.alloc.clone());
        self.queue_inline_response(
            status,
            "application/cbor",
            encoded.as_slice(),
            retry_hdr.as_ref().map(|s| s.as_str()),
            idle_ns,
        );
    }
}

enum RecvOutcome {
    Bytes(usize),
    WouldBlock,
    Eof,
    Closed,
}

enum BodyStep {
    NeedMore,
    Complete,
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

fn parse_request_headers(raw: &[u8]) -> Result<ParsedRequest<'_>, Error> {
    let text = core::str::from_utf8(raw)
        .map_err(|_| Error::malformed("HTTP headers must be valid UTF-8"))?;
    let mut lines = text.split("\r\n");
    let req_line = lines.next().ok_or_else(|| Error::malformed("empty HTTP request line"))?;
    let mut parts = req_line.split(' ');
    let method = parts.next().ok_or_else(|| Error::malformed("missing HTTP method"))?;
    let target = parts.next().ok_or_else(|| Error::malformed("missing HTTP request target"))?;
    let version = parts.next().ok_or_else(|| Error::malformed("missing HTTP version"))?;
    if parts.next().is_some() {
        return Err(Error::malformed("extra tokens in HTTP request line"));
    }
    let mut keep_alive = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err(Error::malformed("unsupported HTTP version")),
    };

    let mut content_length: Option<u64> = None;
    let mut is_chunked = false;
    let mut if_match: Option<Digest> = None;
    let mut saw_if_match = false;
    let mut expect_continue = false;

    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, val)) = line.split_once(':') else {
            return Err(Error::malformed("malformed HTTP header line"));
        };
        let name = name.trim();
        let val = val.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = val
                .parse::<u64>()
                .map_err(|_| Error::malformed("invalid Content-Length header"))?;
            if let Some(prev) = content_length {
                if prev != parsed {
                    return Err(Error::malformed("conflicting Content-Length headers"));
                }
            }
            content_length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if val.eq_ignore_ascii_case("chunked") {
                is_chunked = true;
            } else {
                return Err(Error::malformed("unsupported Transfer-Encoding"));
            }
        } else if name.eq_ignore_ascii_case("connection") {
            if val.eq_ignore_ascii_case("close") {
                keep_alive = false;
            } else if val.eq_ignore_ascii_case("keep-alive") {
                keep_alive = true;
            }
        } else if name.eq_ignore_ascii_case("if-match") {
            if saw_if_match {
                return Err(Error::malformed("multiple If-Match headers are refused"));
            }
            saw_if_match = true;
            if_match = Some(parse_if_match(val)?);
        } else if name.eq_ignore_ascii_case("expect") {
            if val.eq_ignore_ascii_case("100-continue") {
                expect_continue = true;
            }
        }
    }

    if is_chunked && content_length.is_some() {
        return Err(Error::malformed("both Content-Length and Transfer-Encoding given"));
    }
    let framing = if is_chunked {
        BodyFraming::Chunked(ChunkedState::SizeLine)
    } else {
        BodyFraming::Fixed { remaining: content_length.unwrap_or(0) }
    };

    Ok(ParsedRequest { method, target, keep_alive, framing, if_match, expect_continue })
}

fn parse_if_match(val: &str) -> Result<Digest, Error> {
    let inner = val
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| Error::malformed("If-Match must be a single quoted 64-hex SHA-256"))?;
    parse_digest(inner)
}

fn drain_body(
    input: &[u8],
    consumed: &mut usize,
    framing: &mut BodyFraming,
    mut on_chunk: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<BodyStep, Error> {
    match framing {
        BodyFraming::Fixed { remaining } => {
            if *remaining == 0 {
                return Ok(BodyStep::Complete);
            }
            let avail = input.get(*consumed..).unwrap_or(&[]);
            if avail.is_empty() {
                return Ok(BodyStep::NeedMore);
            }
            let take_u64 = (*remaining).min(u64::try_from(avail.len()).unwrap_or(u64::MAX));
            let take = usize::try_from(take_u64).unwrap_or(avail.len());
            if let Some(chunk) = avail.get(..take) {
                on_chunk(chunk)?;
                *consumed = consumed.saturating_add(take);
                *remaining = remaining.saturating_sub(take_u64);
            }
            if *remaining == 0 {
                Ok(BodyStep::Complete)
            } else {
                Ok(BodyStep::NeedMore)
            }
        }
        BodyFraming::Chunked(state) => loop {
            let avail = input.get(*consumed..).unwrap_or(&[]);
            match state {
                ChunkedState::SizeLine => {
                    let Some(line_end) = find_crlf(avail) else {
                        if avail.len() > MAX_CHUNK_LINE_BYTES {
                            return Err(Error::malformed("chunk size line too long"));
                        }
                        return Ok(BodyStep::NeedMore);
                    };
                    let line_bytes = avail.get(..line_end).unwrap_or(&[]);
                    let line = core::str::from_utf8(line_bytes)
                        .map_err(|_| Error::malformed("non-UTF-8 chunk size"))?;
                    let hex_part = line.split(';').next().unwrap_or("").trim();
                    if hex_part.is_empty() {
                        return Err(Error::malformed("empty chunk size"));
                    }
                    let size = u64::from_str_radix(hex_part, 16)
                        .map_err(|_| Error::malformed("invalid hex chunk size"))?;
                    *consumed = consumed.saturating_add(line_end.saturating_add(2));
                    if size == 0 {
                        *state = ChunkedState::Trailers { lines: 0 };
                    } else {
                        *state = ChunkedState::Data { remaining: size };
                    }
                }
                ChunkedState::Data { remaining } => {
                    if avail.is_empty() {
                        return Ok(BodyStep::NeedMore);
                    }
                    let take_u64 = (*remaining).min(u64::try_from(avail.len()).unwrap_or(u64::MAX));
                    let take = usize::try_from(take_u64).unwrap_or(avail.len());
                    if let Some(chunk) = avail.get(..take) {
                        on_chunk(chunk)?;
                        *consumed = consumed.saturating_add(take);
                        *remaining = remaining.saturating_sub(take_u64);
                    }
                    if *remaining == 0 {
                        *state = ChunkedState::DataCr;
                    }
                }
                ChunkedState::DataCr => {
                    let Some(&b) = avail.first() else {
                        return Ok(BodyStep::NeedMore);
                    };
                    if b != b'\r' {
                        return Err(Error::malformed("expected CR after chunk data"));
                    }
                    *consumed = consumed.saturating_add(1);
                    *state = ChunkedState::DataLf;
                }
                ChunkedState::DataLf => {
                    let Some(&b) = avail.first() else {
                        return Ok(BodyStep::NeedMore);
                    };
                    if b != b'\n' {
                        return Err(Error::malformed("expected LF after chunk data"));
                    }
                    *consumed = consumed.saturating_add(1);
                    *state = ChunkedState::SizeLine;
                }
                ChunkedState::Trailers { lines } => {
                    let Some(line_end) = find_crlf(avail) else {
                        if avail.len() > MAX_CHUNK_LINE_BYTES {
                            return Err(Error::malformed("chunk trailer line too long"));
                        }
                        return Ok(BodyStep::NeedMore);
                    };
                    *consumed = consumed.saturating_add(line_end.saturating_add(2));
                    if line_end == 0 {
                        *state = ChunkedState::Done;
                        return Ok(BodyStep::Complete);
                    }
                    *lines = lines.saturating_add(1);
                    if *lines > MAX_TRAILER_LINES {
                        return Err(Error::malformed("too many chunk trailer lines"));
                    }
                }
                ChunkedState::Done => return Ok(BodyStep::Complete),
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_match_parser_requires_exact_quoted_lowercase_sha256() {
        let valid = "\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"";
        assert!(parse_if_match(valid).is_ok());

        for bad in [
            "*",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "W/\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"",
            "\"0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef\"",
            "\"short\"",
            "",
        ] {
            assert!(parse_if_match(bad).is_err(), "expected refusal for {bad:?}");
        }
    }

    #[test]
    fn chunked_decoder_handles_split_chunks_and_trailers() {
        let alloc = blaze_own::testing::heap().shared();
        let wire = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nX-Trailer: ok\r\n\r\n";
        let mut framing = BodyFraming::Chunked(ChunkedState::SizeLine);
        let mut out = Vec::new_in(alloc);
        let mut consumed = 0usize;
        let done = drain_body(wire, &mut consumed, &mut framing, |chunk| {
            out.extend_from_slice(chunk);
            Ok(())
        });
        assert!(matches!(done, Ok(BodyStep::Complete)));
        assert_eq!(consumed, wire.len());
        assert_eq!(out.as_slice(), b"hello world");
    }
}
