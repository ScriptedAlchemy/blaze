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

/// Local client with immutable retry state. Session IDs are lifetime identities, not capabilities.
pub struct Client {
    stream: UnixStream,
    /// Negotiated clock, versions, epoch and frame bounds.
    pub welcome: LocalWelcome,
    next: u64,
    pending: Option<Request>,
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
    if welcome.session != hello.session
        || welcome.transport != hello.transport
        || welcome.codec != hello.codec
        || welcome.terminal != hello.terminal
        || hello.epoch.is_some_and(|epoch| epoch != welcome.epoch)
        || welcome.max_frame == 0
        || welcome.max_frame > Limits::DOCUMENTED.request
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
    /// New daemon-relative session. A restarted process uses a new random ID.
    pub fn connect(path: &Path, session: Digest) -> Result<Self, Error> {
        let (stream, welcome) = connect(path, hello(session, None))?;
        Ok(Self { next: welcome.next_request, welcome, stream, pending: None })
    }

    /// Resume this same session and epoch; a pending request is not changed or silently skipped.
    pub fn reconnect(&mut self, path: &Path) -> Result<(), Error> {
        let (stream, welcome) =
            connect(path, hello(self.welcome.session, Some(self.welcome.epoch)))?;
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
