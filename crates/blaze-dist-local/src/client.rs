//! Blocking CLI/client IO over the carrier's nonblocking server. A lost response leaves the
//! exact pending request available for retry, including after reconnect.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use blaze_dist::jobs::{LocalAction, local_action};
use blaze_wire::{Digest, Limits, LocalPolicy, decode, encode};

use crate::Error;
use crate::protocol::*;
use crate::server::frame;

/// Local client with immutable retry state. Session labels are lifetime identities, not capabilities.
pub struct Client {
    stream: UnixStream,
    /// Negotiated clock, versions, epoch and frame bounds.
    pub welcome: LocalWelcome,
    next: u64,
    pending: Option<Request>,
    /// Creation nonce, kept so a retried creation returns this same lifetime.
    nonce: Digest,
}

fn connect(path: &Path, hello: LocalHello) -> Result<(UnixStream, LocalWelcome), Error> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(&frame(&encode(&ClientFrame::Hello(hello.clone()))))?;
    let response = read(&mut stream, 1 << 20)?;
    let ServerFrame::Welcome(welcome) = response else {
        return Err(Error::Protocol(format!("hello refused: {response:?}")));
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
        return Err(Error::Protocol("welcome-binding/version".to_owned()));
    }
    Ok((stream, welcome))
}

fn read(stream: &mut UnixStream, max_frame: u32) -> Result<ServerFrame, Error> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let size = u32::from_be_bytes(header);
    if size == 0 || size > max_frame {
        return Err(Error::Protocol("response-size".to_owned()));
    }
    let mut body = vec![0; size as usize];
    stream.read_exact(&mut body)?;
    decode(&body, &Limits::DOCUMENTED)
        .map_err(|e| Error::Protocol(format!("response-codec: {e:?}")))
}

impl Client {
    /// Create a new daemon-issued lifetime. `nonce` only makes a retried creation
    /// idempotent while that lifetime is live; it is not a capability or an authority.
    pub fn connect(path: &Path, nonce: Digest) -> Result<Self, Error> {
        let (stream, welcome) = connect(path, hello(nonce))?;
        Ok(Self { next: welcome.next_request, welcome, stream, pending: None, nonce })
    }

    /// Resume this same lifetime and epoch. A pending request is never changed, skipped or
    /// carried into a new lifetime: an expired lifetime refuses this and the caller decides
    /// explicitly whether to create a fresh one and re-issue its work.
    pub fn reconnect(&mut self, path: &Path) -> Result<(), Error> {
        let (stream, welcome) = connect(path, resume(self.welcome.session, self.welcome.epoch))?;
        if let Some(pending) = &self.pending {
            if welcome.next_request != pending.sequence
                && welcome.next_request != pending.sequence + 1
            {
                return Err(Error::Protocol("resume-request-gap".to_owned()));
            }
        } else if welcome.next_request != self.next {
            return Err(Error::Protocol("resume-request-gap".to_owned()));
        }
        self.stream = stream;
        self.welcome = welcome;
        Ok(())
    }

    /// Explicitly create a fresh lifetime after expiry. Nothing of the old lifetime carries
    /// over: it is refused while a request is pending, because an unresolved request is
    /// never migrated into a new session, and its exact bytes stay available for inspection.
    pub fn renew(&mut self, path: &Path, nonce: Digest) -> Result<(), Error> {
        if self.pending.is_some() {
            return Err(Error::Protocol(
                "pending-request: abandon it explicitly, never migrate it".to_owned(),
            ));
        }
        let (stream, welcome) = connect(path, hello(nonce))?;
        self.next = welcome.next_request;
        self.stream = stream;
        self.welcome = welcome;
        self.nonce = nonce;
        Ok(())
    }

    /// Give up the pending request deliberately, returning it so the caller can decide
    /// whether to re-issue the work as a new request in a new lifetime.
    pub fn abandon_pending(&mut self) -> Option<Request> {
        self.pending.take()
    }

    /// The creation nonce this client was opened with.
    pub fn nonce(&self) -> Digest {
        self.nonce
    }

    /// End this lifetime deliberately. The daemon journals the answer first, then removes the
    /// session, so capacity returns at once instead of at the fixed lifetime. Nothing of it
    /// can be resumed afterwards; a pending request is refused rather than migrated.
    pub fn release(&mut self) -> Result<(), Error> {
        if self.pending.is_some() {
            return Err(Error::Protocol(
                "pending-request: abandon it explicitly, never migrate it".to_owned(),
            ));
        }
        self.call(Command::Release(Release {}))?;
        Ok(())
    }

    /// Submit a new command only after the previous response is bound and decoded.
    pub fn call(&mut self, command: Command) -> Result<Answer, Error> {
        if self.pending.is_some() {
            return Err(Error::Protocol("retry pending request first".to_owned()));
        }
        self.pending = Some(Request {
            epoch: self.welcome.epoch,
            session: self.welcome.session,
            sequence: self.next,
            command,
        });
        self.retry()
    }

    /// Resend the pending bytes unchanged. IO/disconnect errors retain them.
    pub fn retry(&mut self) -> Result<Answer, Error> {
        let pending = self
            .pending
            .as_ref()
            .ok_or_else(|| Error::Protocol("no pending request".to_owned()))?;
        let bytes = encode(&ClientFrame::Request(pending.clone()));
        if bytes.len() > self.welcome.max_frame as usize {
            return Err(Error::Protocol("request-size".to_owned()));
        }
        self.stream.write_all(&frame(&bytes))?;
        let response = read(&mut self.stream, self.welcome.max_frame)?;
        let ServerFrame::Response(response) = response else {
            return Err(Error::Protocol(format!("request refused: {response:?}")));
        };
        if response.epoch != pending.epoch
            || response.session != pending.session
            || response.sequence != pending.sequence
        {
            return Err(Error::Protocol("response-binding".to_owned()));
        }
        validate_answer(&pending.command, &response.answer)?;
        self.next += 1;
        self.pending = None;
        Ok(response.answer)
    }
}

fn validate_answer(command: &Command, answer: &Answer) -> Result<(), Error> {
    let matched = match (command, answer) {
        (_, Answer::Error(_)) => true,
        (Command::Release(_), Answer::Ack(_)) => true,
        (Command::Submit(s), Answer::Binding(b)) => b.subscription.content == s.content,
        (Command::Subscribe(s), Answer::Binding(b)) => {
            b.subscription.content == s.content && b.subscription.execution == s.execution
        }
        (Command::Resume(s), Answer::Binding(b)) => b.subscription == *s,
        (Command::Events(r), Answer::Events(page)) => {
            page.binding == r.binding
                && page.events.len() <= r.limit as usize
                && page.events.windows(2).all(|w| w[0].sequence < w[1].sequence)
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
    if matched { Ok(()) } else { Err(Error::Protocol("answer-command-binding".to_owned())) }
}

/// The policy conversion boundary requires exact content/execution/subscriber/attempt binding.
/// A pending or mismatched terminal never triggers fallback. Call Resume first after lead loss.
pub fn terminal_action(
    binding: &Binding,
    terminal: &Terminal,
    policy: &LocalPolicy,
) -> Result<Option<LocalAction>, Error> {
    if binding.subscription != terminal.subscription {
        return Err(Error::Protocol("terminal-subscription-binding".to_owned()));
    }
    let Some(value) = &terminal.value else {
        return Ok(None);
    };
    if value.execution != binding.subscription.execution
        || value.subscriber != binding.subscription.subscriber
        || value.attempt != binding.attempt
    {
        return Err(Error::Protocol("terminal-attempt-binding".to_owned()));
    }
    local_action(policy, &value.termination)
        .map(Some)
        .map_err(|e| Error::Protocol(format!("terminal-contract: {e:?}")))
}
