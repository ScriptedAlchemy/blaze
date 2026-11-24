//! Blocking CLI/client IO over the carrier's nonblocking server. A lost response leaves the
//! exact pending request available for retry, including after reconnect.

use core::time::Duration;

use blaze_own::{Vec, format_in};
use blaze_wire::{Digest, Limits, WireAlloc, WireString, decode, encode};

use crate::Error;
use crate::protocol::*;
use crate::server::frame;
use crate::sys::{SystemClock, UnixStream};

mod socket;

/// Local client with immutable retry state. Session labels are lifetime identities, not capabilities.
pub struct Client {
    stream: UnixStream,
    socket: WireString,
    /// Clock, versions, epoch and bounds from the most recent handshake. Verified job
    /// traffic can extend expiry; reconnect refreshes expires_ms without changing identity.
    pub welcome: LocalWelcome,
    next: u64,
    pending: Option<Request>,
    alloc: WireAlloc,
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "a hello is sent once; its one caller gives it up"
)]
fn connect(
    path: &str,
    hello: LocalHello,
    alloc: WireAlloc,
) -> Result<(UnixStream, LocalWelcome), Error> {
    let clock = SystemClock;
    let now = clock.monotonic_nanos()?;
    let deadline = socket::ConnectDeadline::after(now, Duration::from_secs(5));
    let mut stream = socket::connect_until(path, deadline, &clock, alloc)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(&frame(&encode(&ClientFrame::Hello(hello.clone()), alloc), alloc))?;
    let response = read(&mut stream, 1 << 20, alloc)?;
    let ServerFrame::Welcome(welcome) = response else {
        return Err(Error::Protocol(format_in!(alloc, "hello refused: {response:?}")));
    };
    let resumes = hello.resume.is_some();
    if welcome.transport != hello.transport
        || welcome.codec != hello.codec
        || welcome.terminal != hello.terminal
        || welcome.max_frame == 0
        || welcome.max_frame > Limits::DOCUMENTED.request
        // A creation must not answer as a resume of some other lifetime, and a resume must
        // answer with exactly the label it named.
        || (resumes && (welcome.created || Some(welcome.session) != hello.resume))
        || hello.epoch.is_some_and(|epoch| epoch != welcome.epoch)
    {
        return Err(Error::Protocol(WireString::from_str_in("welcome-binding/version", alloc)));
    }
    Ok((stream, welcome))
}

fn read(stream: &mut UnixStream, max_frame: u32, alloc: WireAlloc) -> Result<ServerFrame, Error> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let size = u32::from_be_bytes(header);
    if size == 0 || size > max_frame {
        return Err(Error::Protocol(WireString::from_str_in("response-size", alloc)));
    }
    let body_len = usize::try_from(size)
        .map_err(|_| Error::Protocol(WireString::from_str_in("response-size", alloc)))?;
    let mut body = Vec::with_capacity_in(body_len, alloc);
    body.resize(body_len, 0);
    stream.read_exact(&mut body)?;
    decode(&body, &Limits::DOCUMENTED, alloc)
        .map_err(|e| Error::Protocol(format_in!(alloc, "response-codec: {e:?}")))
}

impl Client {
    /// Allocator owned by this client.
    pub(crate) const fn alloc(&self) -> WireAlloc {
        self.alloc
    }

    /// Create a new daemon-issued lifetime. `nonce` only makes a retried creation
    /// idempotent while that lifetime is live; it is not a capability or an authority.
    pub fn connect(path: &str, nonce: Digest, alloc: WireAlloc) -> Result<Self, Error> {
        let (stream, welcome) = connect(path, hello(nonce), alloc)?;
        Ok(Self {
            next: welcome.next_request,
            welcome,
            stream,
            socket: WireString::from_str_in(path, alloc),
            pending: None,
            alloc,
        })
    }

    /// Resume this same lifetime and epoch. A pending request is never changed, skipped or
    /// carried into a new lifetime: an expired lifetime refuses this and the caller decides
    /// explicitly whether to create a fresh one and re-issue its work.
    pub fn reconnect(&mut self, path: &str) -> Result<(), Error> {
        let (stream, welcome) =
            connect(path, resume(self.welcome.session, self.welcome.epoch), self.alloc)?;
        if let Some(pending) = &self.pending {
            if welcome.next_request != pending.sequence
                && welcome.next_request != pending.sequence.saturating_add(1)
            {
                return Err(Error::Protocol(WireString::from_str_in("resume-request-gap", self.alloc)));
            }
        } else if welcome.next_request != self.next {
            return Err(Error::Protocol(WireString::from_str_in("resume-request-gap", self.alloc)));
        }
        self.stream = stream;
        self.socket = WireString::from_str_in(path, self.alloc);
        self.welcome = welcome;
        Ok(())
    }

    /// End this lifetime deliberately. The daemon journals the answer first, then removes the
    /// session, so capacity returns at once instead of at the fixed lifetime. Nothing of it
    /// can be resumed afterwards; a pending request is refused rather than migrated.
    pub fn release(&mut self) -> Result<(), Error> {
        if self.pending.is_some() {
            return Err(Error::Protocol(WireString::from_str_in(
                "pending-request: it is never migrated",
                self.alloc,
            )));
        }
        self.call(Command::Release(Release {}))?;
        Ok(())
    }

    /// Submit a new command only after the previous response is bound and decoded.
    /// One IO failure resumes this same session and retries its identical pending bytes.
    /// Protocol refusal or genuine lifetime expiry never creates a replacement session.
    pub fn call(&mut self, command: Command) -> Result<Answer, Error> {
        if self.pending.is_some() {
            return Err(Error::Protocol(WireString::from_str_in(
                "retry pending request first",
                self.alloc,
            )));
        }
        self.pending = Some(Request {
            epoch: self.welcome.epoch,
            session: self.welcome.session,
            sequence: self.next,
            command,
        });
        match self.retry() {
            Err(Error::Io(_)) => {
                let socket = self.socket.clone();
                self.reconnect(socket.as_str())?;
                self.retry()
            }
            answer => answer,
        }
    }

    /// Resend the pending bytes unchanged. IO/disconnect errors retain them.
    pub fn retry(&mut self) -> Result<Answer, Error> {
        let pending = self
            .pending
            .as_ref()
            .ok_or_else(|| Error::Protocol(WireString::from_str_in("no pending request", self.alloc)))?;
        let bytes = encode(&ClientFrame::Request(pending.clone()), self.alloc);
        let max_frame = usize::try_from(self.welcome.max_frame).unwrap_or(usize::MAX);
        if bytes.len() > max_frame {
            return Err(Error::Protocol(WireString::from_str_in("request-size", self.alloc)));
        }
        self.stream.write_all(&frame(&bytes, self.alloc))?;
        let response = read(&mut self.stream, self.welcome.max_frame, self.alloc)?;
        let ServerFrame::Response(response) = response else {
            return Err(Error::Protocol(format_in!(self.alloc, "request refused: {response:?}")));
        };
        if response.epoch != pending.epoch
            || response.session != pending.session
            || response.sequence != pending.sequence
        {
            return Err(Error::Protocol(WireString::from_str_in("response-binding", self.alloc)));
        }
        validate_answer(&pending.command, &response.answer, self.alloc)?;
        self.next = self.next.saturating_add(1);
        self.pending = None;
        Ok(response.answer)
    }
}

fn validate_answer(command: &Command, answer: &Answer, alloc: WireAlloc) -> Result<(), Error> {
    let matched = match (command, answer) {
        (_, Answer::Error(_)) => true,
        (Command::Release(_), Answer::Ack(_)) => true,
        (Command::Submit(s), Answer::Binding(b)) => b.subscription.content == s.content,
        (Command::Subscribe(s), Answer::Binding(b)) => {
            b.subscription.content == s.content && b.subscription.execution == s.execution
        }
        (Command::Resume(s), Answer::Binding(b)) => b.subscription == *s,
        (Command::Events(r), Answer::Events(page)) => {
            let limit = usize::try_from(r.limit).unwrap_or(usize::MAX);
            page.binding == r.binding
                && page.events.len() <= limit
                && page.events.windows(2).all(|w| {
                    matches!(w, [a, b] if a.sequence < b.sequence)
                })
                && page.events.first().is_none_or(|e| e.sequence > r.from)
                && page.next == page.events.last().map_or(r.from, |e| e.sequence)
        }
        (Command::Cancel(_), Answer::Ack(_)) => true,
        (Command::Join(_), Answer::Join(_)) => true,
        (Command::Lease(r), Answer::Lease(a)) => a.grant.as_ref().is_none_or(|g| {
            g.lease.execution == r.request.execution
                && g.lease.attempt == r.request.attempt
                && g.lease.holder == r.executor
        }),
        (Command::Result(_), Answer::Result(_)) => true,
        (Command::WorkerEvents(_), Answer::WorkerEvents(_)) => true,
        (Command::KillAck(_), Answer::KillAck(_)) => true,
        (Command::RetireAssignment(_), Answer::Ack(_)) => true,
        (Command::OwnedAssignment(_), Answer::OwnedAssignment(_)) => true,
        (Command::Claim(_), Answer::Claimed(_)) => true,
        (Command::Packs(_), Answer::Packs(_)) => true,
        (Command::CommitPublication(_), Answer::Publication(_)) => true,
        (Command::Publication(_), Answer::Publication(_)) => true,
        (Command::DeliverPublication(_), Answer::Ack(_)) => true,
        (Command::Terminal(s), Answer::Terminal(t)) => {
            t.subscription == *s
                && t.value.as_ref().is_none_or(|value| {
                    value.execution == s.execution
                        && value.subscriber == s.subscriber
                        && value.termination.is_valid()
                })
        }
        _ => false,
    };
    if matched {
        Ok(())
    } else {
        Err(Error::Protocol(WireString::from_str_in("answer-command-binding", alloc)))
    }
}
