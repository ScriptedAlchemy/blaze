//! The IO driver of one executor's [`Holder`]: it carries the holder's calls over a
//! [`Client`], hands its compile plans to the caller's batch compiler, and ticks it
//! with the caller's clock until the holder leaves the execution.
//!
//! The caller thread owns compilation and release (including thread-local compiler state),
//! while a scoped driver thread services holder events and heartbeats. A cancelled batch
//! still finishes; its unpublished findings are released before the driver returns.
//!
//! Nothing here compiles by itself and nothing falls back: a transport failure is
//! returned to the caller, whose `LocalPolicy` decides what happens next.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use blaze_dist::holder::{
    BatchCompiler, Call, Compiled, Effect, Event, Holder, Plan, Reply, StopWhy,
};
use blaze_wire::{JobContent, WorkName};

use crate::Error;
use crate::client::Client;
use crate::protocol::*;

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

/// Run the holder to completion over a local carrier. Compilation remains on the
/// caller's thread: a compiler may own thread-local or non-Sync semantic state.
/// The scoped driver thread ticks and sends heartbeats while it compiles. Nothing
/// falls back to another compiler. The clock must share the coordinator's origin.
pub fn run_holder(
    client: &mut Client,
    socket: &Path,
    holder: &mut Holder,
    content: JobContent,
    executor: &str,
    config: &DriverConfig,
    now_ms: impl Fn() -> u64 + Send,
    compiler: &mut dyn BatchCompiler,
) -> Result<StopWhy, Error> {
    let mut pending: Option<u64> = None;
    drive(holder, config, now_ms, compiler, |call, id| {
        send(client, socket, content, executor, call, id, &mut pending, config)
    })
}

enum CompilerTask {
    Compile(Plan),
    Release(Vec<WorkName>),
}

/// The same event loop as the socket driver, with the transport boundary injected
/// so cancellation, late results and transport loss can be tested deterministically.
fn drive(
    holder: &mut Holder,
    config: &DriverConfig,
    now_ms: impl Fn() -> u64 + Send,
    compiler: &mut dyn BatchCompiler,
    mut transmit: impl FnMut(&Call, u64) -> Result<Reply, Error> + Send,
) -> Result<StopWhy, Error> {
    std::thread::scope(|scope| {
        let (tasks, receiver) = mpsc::channel::<CompilerTask>();
        let (finished, completions) = mpsc::channel::<Result<Compiled, String>>();
        let driver = scope.spawn(move || {
            let mut working = false;
            // The compiler retains new findings until the result is acknowledged.
            // Transport failure before acknowledgment must release them as well as
            // findings already retained from earlier successful batches.
            let mut unconfirmed = BTreeSet::new();
            let result =
                (|| -> Result<StopWhy, Error> {
                    let mut effects = Vec::new();
                    let mut stopped: Option<StopWhy> = None;
                    holder.start(now_ms(), &mut effects);
                    holder.step(now_ms(), Event::Loaded, &mut effects);
                    loop {
                        if effects.is_empty() {
                            if let Some(why) = stopped.take() {
                                if !working {
                                    return Ok(why);
                                }
                                stopped = Some(why);
                            }
                            match completions.recv_timeout(config.poll) {
                                Ok(Ok(batch)) => {
                                    working = false;
                                    if !holder.is_stopped() {
                                        unconfirmed.extend(
                                            batch.roots.iter().chain(&batch.names).flat_map(|item| {
                                                item.found.iter().map(|found| found.name)
                                            }),
                                        );
                                    }
                                    // The stopped holder releases even findings from a
                                    // compile that finished after cancellation.
                                    holder.step(now_ms(), Event::Compiled(batch), &mut effects);
                                }
                                Ok(Err(error)) => {
                                    working = false;
                                    if stopped.is_none() {
                                        return Err(Error::Protocol(error));
                                    }
                                }
                                Err(RecvTimeoutError::Timeout) => {
                                    if stopped.is_none() {
                                        holder.step(now_ms(), Event::Tick, &mut effects);
                                    }
                                }
                                Err(RecvTimeoutError::Disconnected) => {
                                    return Err(Error::Protocol(
                                        "compiler worker exited without a result".to_owned(),
                                    ));
                                }
                            }
                            continue;
                        }
                        for effect in std::mem::take(&mut effects) {
                            match effect {
                                Effect::Stop(why) => stopped = Some(why),
                                Effect::Release(names) => {
                                    for name in &names {
                                        unconfirmed.remove(name);
                                    }
                                    tasks.send(CompilerTask::Release(names)).map_err(|_| {
                                        Error::Protocol("compiler worker is unavailable".to_owned())
                                    })?;
                                }
                                Effect::Compile(plan) => {
                                    if working {
                                        return Err(Error::Protocol(
                                            "holder granted overlapping compiler batches".to_owned(),
                                        ));
                                    }
                                    tasks.send(CompilerTask::Compile(plan)).map_err(|_| {
                                        Error::Protocol("compiler worker is unavailable".to_owned())
                                    })?;
                                    working = true;
                                }
                                Effect::Send { id, call } if stopped.is_none() => {
                                    let is_result = matches!(call, Call::Result(_));
                                    let reply = transmit(&call, id)?;
                                    holder.step(now_ms(), Event::Reply { id, reply }, &mut effects);
                                    if is_result {
                                        // The holder has either retained or released every
                                        // discovery from this acknowledged result.
                                        unconfirmed.clear();
                                    }
                                }
                                Effect::Send { .. } => {}
                            }
                        }
                    }
                })();

            if result.is_err() {
                if working {
                    // The socket failed while compilation was running. Do not leave
                    // unreported findings in the caller's compiler after joining it.
                    if let Ok(Ok(batch)) = completions.recv() {
                        unconfirmed.extend(
                            batch
                                .roots
                                .iter()
                                .chain(&batch.names)
                                .flat_map(|item| item.found.iter().map(|found| found.name)),
                        );
                    }
                }
                unconfirmed.extend(holder.findings().keys().copied());
                if !unconfirmed.is_empty() {
                    let _ = tasks.send(CompilerTask::Release(unconfirmed.into_iter().collect()));
                }
            }
            drop(tasks);
            result
        });

        // The actual exporter owns non-Send data: always execute and release on
        // its originating thread, never move its database or resolver into a worker.
        while let Ok(task) = receiver.recv() {
            match task {
                CompilerTask::Compile(plan) => {
                    if finished.send(compiler.compile(&plan)).is_err() {
                        break;
                    }
                }
                CompilerTask::Release(names) => compiler.release(&names),
            }
        }
        match driver.join() {
            Ok(result) => result,
            Err(_) => Err(Error::Protocol("holder driver panicked".to_owned())),
        }
    })
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeSet;
    use std::rc::Rc;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    use blaze_dist::Ranges;
    use blaze_dist::holder::{Compiled, HolderConfig};
    use blaze_wire::{
        Attempt, EntryKey, EntryRef, ExecutionId, Found, Grant, ItemOutcome, ItemResult, Lease,
        RootSet, Set, WorkJoinAnswer, WorkLeaseAnswer, WorkersEventsAnswer,
    };

    use super::*;

    const FOUND: WorkName = WorkName([7; 32]);

    // The real exporter also holds non-Send semantic state. These tests prove
    // its compile and release methods stay on the thread where it was created.
    struct SlowCompiler {
        active: Arc<AtomicBool>,
        released: Vec<WorkName>,
        caller: std::thread::ThreadId,
        calls: Rc<Cell<usize>>,
    }

    impl BatchCompiler for SlowCompiler {
        fn compile(&mut self, plan: &Plan) -> Result<Compiled, String> {
            assert_eq!(std::thread::current().id(), self.caller);
            self.calls.set(self.calls.get() + 1);
            self.active.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(120));
            self.active.store(false, Ordering::SeqCst);
            Ok(Compiled {
                generation: plan.grant.lease.generation,
                roots: vec![ItemResult {
                    name: WorkName([8; 32]),
                    outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([9; 32]) }),
                    found: Set::new(vec![Found { name: FOUND, tuple: [10; 32] }]),
                    unnamed: Set::new(Vec::new()),
                }],
                names: Vec::new(),
                packs: Vec::new(),
                returned_roots: Ranges::new(),
                returned_names: Vec::new(),
            })
        }

        fn release(&mut self, names: &[WorkName]) {
            assert_eq!(std::thread::current().id(), self.caller);
            self.released.extend_from_slice(names);
        }
    }

    fn test_holder() -> (Holder, RootSet, Grant) {
        let roots =
            RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
        let grant = Grant {
            lease: Lease {
                execution: ExecutionId(1),
                attempt: Attempt(1),
                holder: "helper".to_owned(),
                generation: 1,
            },
            roots: Set::new(vec![(0, 1)]),
            names: Set::new(Vec::new()),
        };
        let holder = Holder::new(
            "helper",
            ExecutionId(1),
            Attempt(1),
            roots.clone(),
            Set::new(Vec::new()),
            BTreeSet::new(),
            HolderConfig { heartbeat_ms: 10, retry_ms: 100, idle_ms: 5 },
        );
        (holder, roots, grant)
    }

    #[test]
    fn heartbeats_during_compile_fence_a_cancelled_batch_and_release_its_findings() {
        let (mut holder, roots, grant) = test_holder();
        let active = Arc::new(AtomicBool::new(false));
        let during = Arc::new(AtomicUsize::new(0));
        let mut compiler = SlowCompiler {
            active: active.clone(),
            released: Vec::new(),
            caller: std::thread::current().id(),
            calls: Rc::new(Cell::new(0)),
        };
        let mut leased = false;
        let start = Instant::now();
        let why = drive(
            &mut holder,
            &DriverConfig { poll: Duration::from_millis(1), max_reconnects: 0 },
            || start.elapsed().as_millis() as u64,
            &mut compiler,
            |call, _| {
                Ok(match call {
                    Call::Events(_) => {
                        if active.load(Ordering::SeqCst)
                            && during.fetch_add(1, Ordering::SeqCst) >= 1
                        {
                            Reply::Events(Ok(WorkersEventsAnswer::CANCEL))
                        } else {
                            Reply::Events(Ok(WorkersEventsAnswer::CONTINUE))
                        }
                    }
                    Call::Join(_) => {
                        Reply::Join(Ok(WorkJoinAnswer { roots: roots.clone(), fixed: true }))
                    }
                    Call::Lease(_) => {
                        let next = (!leased).then_some(grant.clone());
                        leased = true;
                        Reply::Lease(Ok(WorkLeaseAnswer { grant: next }))
                    }
                    Call::Result(_) => panic!("cancelled work must not be published"),
                })
            },
        )
        .unwrap();
        assert_eq!(why, StopWhy::Cancelled);
        assert!(during.load(Ordering::SeqCst) >= 2, "no heartbeat while compiling");
        assert_eq!(compiler.released, vec![FOUND]);
        assert_eq!(compiler.calls.get(), 1);
        assert!(holder.is_stopped());
    }

    #[test]
    fn transport_failure_during_compile_releases_its_unreported_findings() {
        let (mut holder, roots, grant) = test_holder();
        let active = Arc::new(AtomicBool::new(false));
        let mut compiler = SlowCompiler {
            active: active.clone(),
            released: Vec::new(),
            caller: std::thread::current().id(),
            calls: Rc::new(Cell::new(0)),
        };
        let start = Instant::now();
        let mut leased = false;
        let result = drive(
            &mut holder,
            &DriverConfig { poll: Duration::from_millis(1), max_reconnects: 0 },
            || start.elapsed().as_millis() as u64,
            &mut compiler,
            |call, _| match call {
                Call::Events(_) if active.load(Ordering::SeqCst) => {
                    Err(Error::Protocol("carrier disconnected while compiling".to_owned()))
                }
                Call::Events(_) => Ok(Reply::Events(Ok(WorkersEventsAnswer::CONTINUE))),
                Call::Join(_) => {
                    Ok(Reply::Join(Ok(WorkJoinAnswer { roots: roots.clone(), fixed: true })))
                }
                Call::Lease(_) => {
                    let next = (!leased).then_some(grant.clone());
                    leased = true;
                    Ok(Reply::Lease(Ok(WorkLeaseAnswer { grant: next })))
                }
                Call::Result(_) => panic!("a disconnected carrier cannot commit the batch"),
            },
        );
        assert!(matches!(result, Err(Error::Protocol(_))));
        assert_eq!(compiler.released, vec![FOUND]);
        assert_eq!(compiler.calls.get(), 1);
    }

    #[test]
    fn failed_result_acknowledgement_releases_new_findings() {
        let (mut holder, roots, grant) = test_holder();
        let active = Arc::new(AtomicBool::new(false));
        let mut compiler = SlowCompiler {
            active,
            released: Vec::new(),
            caller: std::thread::current().id(),
            calls: Rc::new(Cell::new(0)),
        };
        let start = Instant::now();
        let mut leased = false;
        let result = drive(
            &mut holder,
            &DriverConfig { poll: Duration::from_millis(1), max_reconnects: 0 },
            || start.elapsed().as_millis() as u64,
            &mut compiler,
            |call, _| match call {
                Call::Events(_) => Ok(Reply::Events(Ok(WorkersEventsAnswer::CONTINUE))),
                Call::Join(_) => {
                    Ok(Reply::Join(Ok(WorkJoinAnswer { roots: roots.clone(), fixed: true })))
                }
                Call::Lease(_) => {
                    let next = (!leased).then_some(grant.clone());
                    leased = true;
                    Ok(Reply::Lease(Ok(WorkLeaseAnswer { grant: next })))
                }
                Call::Result(_) => Err(Error::Protocol("result acknowledgement lost".to_owned())),
            },
        );
        assert!(matches!(result, Err(Error::Protocol(_))));
        assert_eq!(compiler.released, vec![FOUND]);
        assert_eq!(compiler.calls.get(), 1);
    }
}
