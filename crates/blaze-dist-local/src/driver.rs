//! The IO driver of one executor's [`Holder`]: it carries the holder's calls over a
//! [`Client`], hands its compile plans to the caller's batch compiler, and ticks it
//! with the caller's clock until the holder leaves the execution.
//!
//! The compile runs on the driver's thread, so no heartbeat is sent while a batch
//! compiles: a batch must fit inside the work set's lease window (`lease_ms` of
//! silence, and the progress timeout), which the batch sizing of section 4 already
//! assumes. A driver that compiles on other threads keeps this same shape and
//! delivers [`Compiled`] through its own queue.
//!
//! Nothing here compiles by itself and nothing falls back: a transport failure is
//! returned to the caller, whose `LocalPolicy` decides what happens next.

use std::path::Path;
use std::time::Duration;

use blaze_dist::holder::{Call, Compiled, Effect, Event, Holder, Plan, Reply, StopWhy};
use blaze_wire::{JobContent, WorkName};

use crate::Error;
use crate::client::Client;
use crate::protocol::*;

/// The executor's batch compiler: what a [`Plan`] is compiled by, and where
/// findings the coordinator no longer counts on are dropped. The compiler of
/// record is the existing batch exporter; this carrier never compiles itself.
pub trait BatchCompiler {
    /// Compile one granted batch whole, its entries sealed and uploaded.
    fn compile(&mut self, plan: &Plan) -> Result<Compiled, Error>;
    /// Drop retained findings of these names.
    fn release(&mut self, names: &[WorkName]);
}

/// How the driver paces an idle holder and bounds its reconnects.
#[derive(Clone, Debug)]
pub struct DriverConfig {
    /// Sleep between ticks when the holder asked for nothing.
    pub poll: Duration,
    /// Reconnects of one call before its IO error is the caller's.
    pub max_reconnects: u32,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self { poll: Duration::from_millis(5), max_reconnects: 8 }
    }
}

/// Run `holder` to its stop over `client`, compiling its plans with `compile` and
/// dropping retained findings through `release`. `now_ms` is the daemon-relative
/// monotonic clock the holder's timings live on; the welcome's `now_ms` anchors the
/// first steps, so the caller's clock and the daemon's must share an origin (the
/// local carrier's `MonotonicClock` and a driver started beside it do).
pub fn run_holder(
    client: &mut Client,
    socket: &Path,
    holder: &mut Holder,
    content: JobContent,
    executor: &str,
    config: &DriverConfig,
    now_ms: impl Fn() -> u64,
    compiler: &mut dyn BatchCompiler,
) -> Result<StopWhy, Error> {
    let mut pending: Option<u64> = None;
    let mut effects = Vec::new();
    holder.start(now_ms(), &mut effects);
    holder.step(now_ms(), Event::Loaded, &mut effects);
    loop {
        if effects.is_empty() {
            std::thread::sleep(config.poll);
            holder.step(now_ms(), Event::Tick, &mut effects);
            continue;
        }
        for effect in std::mem::take(&mut effects) {
            match effect {
                Effect::Stop(why) => return Ok(why),
                Effect::Release(names) => compiler.release(&names),
                Effect::Compile(plan) => {
                    let batch = compiler.compile(&plan)?;
                    holder.step(now_ms(), Event::Compiled(batch), &mut effects);
                }
                Effect::Send { id, call } => {
                    let reply =
                        send(client, socket, content, executor, &call, id, &mut pending, config)?;
                    holder.step(now_ms(), Event::Reply { id, reply }, &mut effects);
                }
            }
        }
    }
}

/// One holder call over the carrier. A repeated `Send` of the call still pending in
/// the client (the holder's own resend after `retry_ms`) retries those exact bytes;
/// an IO failure reconnects the same lifetime and retries, bounded by the config.
fn send(
    client: &mut Client,
    socket: &Path,
    content: JobContent,
    executor: &str,
    call: &Call,
    id: u64,
    pending: &mut Option<u64>,
    config: &DriverConfig,
) -> Result<Reply, Error> {
    let mut reconnects = 0;
    loop {
        let answer = if *pending == Some(id) {
            client.retry()
        } else {
            *pending = Some(id);
            client.call(command(content, executor, call))
        };
        match answer {
            Ok(answer) => {
                *pending = None;
                return reply(call, answer);
            }
            Err(Error::Io(error)) => {
                reconnects += 1;
                if reconnects > config.max_reconnects {
                    return Err(Error::Io(error));
                }
                client.reconnect(socket)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn command(content: JobContent, executor: &str, call: &Call) -> Command {
    let executor = executor.to_owned();
    match call {
        Call::Join(request) => Command::Join(Join { content, executor, request: request.clone() }),
        Call::Lease(request) => {
            Command::Lease(Lease { content, executor, request: request.clone() })
        }
        Call::Result(request) => {
            Command::Result(BatchSubmission { content, executor, request: request.clone() })
        }
        Call::Events(request) => {
            Command::WorkerEvents(WorkerEvents { content, executor, request: request.clone() })
        }
    }
}

/// The holder's typed reply for one answer: a carrier `Error` answer is the call's
/// `Err`, never a driver failure, so refusals reach the holder's own rules.
fn reply(call: &Call, answer: Answer) -> Result<Reply, Error> {
    Ok(match (call, answer) {
        (_, Answer::Error(error)) => match call {
            Call::Join(_) => Reply::Join(Err(error)),
            Call::Lease(_) => Reply::Lease(Err(error)),
            Call::Result(_) => Reply::Result(Err(error)),
            Call::Events(_) => Reply::Events(Err(error)),
        },
        (Call::Join(_), Answer::Join(answer)) => Reply::Join(Ok(answer)),
        (Call::Lease(_), Answer::Lease(answer)) => Reply::Lease(Ok(answer)),
        (Call::Result(_), Answer::Result(answer)) => Reply::Result(Ok(answer)),
        (Call::Events(_), Answer::WorkerEvents(answer)) => Reply::Events(Ok(answer)),
        (call, answer) => {
            return Err(Error::Protocol(format!("answer {answer:?} to holder call {call:?}")));
        }
    })
}
