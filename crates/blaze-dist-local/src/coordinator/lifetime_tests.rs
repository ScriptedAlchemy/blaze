//! Real coordinator transitions on a virtual clock, including lost and conflicting retries.

use hegel::generators as gs;

use super::*;
use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::ExecutionConfig;
use blaze_wire::{LocalPolicy, Namespace, Reservation};

const ORDINARY: u64 = 10;
const WORK: u64 = 1_000;
const CLEANUP: u64 = 50;
const CLAIMANT: Digest = [4; 32];

#[derive(Clone, Copy)]
enum Plant {
    None,
    MissingBinding,
    SlidingCeiling,
    LostReplayIdentity,
    ReusedRequestIdentity,
}

struct Exchange {
    request: Request,
    bytes: Vec<u8>,
    answer: Answer,
    effects: Vec<(ExecutionId, Effect)>,
}

struct Sim {
    core: Coordinator,
    now: u64,
    next: BTreeMap<Digest, u64>,
}

fn need() -> Reservation {
    Reservation { memory: 1, cpus: 1, disk: 1 }
}

fn job(unit: u8, deadline_ms: u64) -> JobRequest {
    JobRequest {
        unit: [unit; 32],
        platform: [1; 32],
        namespace: Namespace::Main,
        source: [0; 32],
        deadline_ms,
        local: LocalPolicy::Never,
    }
}

impl Sim {
    fn new() -> Sim {
        let mut admission = Admission::new(AdmissionConfig::default());
        admission.add_machine("machine", [1; 32], need());
        admission.add_builder(0, "machine", need()).unwrap();
        let jobs = Jobs::new(
            admission,
            ExecutionConfig { heartbeat_ms: 10_000, ..ExecutionConfig::default() },
        );
        Sim {
            core: Coordinator::new(
                jobs,
                Config {
                    session_ms: ORDINARY,
                    max_execution_ms: WORK,
                    owner_cleanup_ms: CLEANUP,
                    ..Config::default()
                },
                [7; 32],
            ),
            now: 0,
            next: BTreeMap::new(),
        }
    }

    fn open(&mut self, nonce: u8) -> Digest {
        let welcome = self.core.hello(self.now, &hello([nonce; 32])).unwrap();
        self.next.insert(welcome.session, welcome.next_request);
        welcome.session
    }

    fn call(&mut self, session: Digest, command: Command) -> Result<Exchange, String> {
        let request = Request {
            epoch: self.core.epoch,
            session,
            sequence: self.next[&session],
            command,
        };
        let (bytes, effects) = self.core.request(self.now, &request)
            .map_err(|why| format!("carrier call refused: {}", why.message))?;
        let ServerFrame::Response(response) = decode(&bytes, &Limits::DOCUMENTED).unwrap() else {
            return Err("a request had no bound response".into());
        };
        if response.epoch != request.epoch || response.session != request.session
            || response.sequence != request.sequence
        {
            return Err("a response lost its exact replay identity".into());
        }
        *self.next.get_mut(&session).unwrap() += 1;
        Ok(Exchange { request, bytes, answer: response.answer, effects })
    }

    fn oracle(&self, session: Digest, deadline: u64) -> Result<(), String> {
        let live = self.core.sessions.get(&session).ok_or("verified work lost its session")?;
        if live.expires != deadline + CLEANUP {
            return Err("verified work lost its cleanup lifetime".into());
        }
        if live.maximum_expires != ORDINARY + WORK + CLEANUP {
            return Err("a live session slid its immutable lifetime ceiling".into());
        }
        if live.executions.len() > self.core.config.max_executions {
            return Err("execution binding retention exceeded its finite bound".into());
        }
        Ok(())
    }
}

fn scenario(queued_at: u64, extension: u64, first_read: u64, plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new();
    let owner = sim.open(1);
    let watcher = sim.open(2);
    let unbound = sim.open(3);
    sim.now = queued_at;
    let first = sim.call(owner, Command::Submit(submission(&job(9, queued_at + 100), need())))?;
    let Answer::Binding(binding) = first.answer else {
        return Err("the first execution was not admitted".into());
    };
    let execution = binding.subscription.execution;
    let content = binding.subscription.content;
    let claim = sim.call(owner, Command::Claim(Claim { content, execution, claimant: CLAIMANT }))?;
    let Answer::Claimed(Claimed { executor: Some(executor), attempt, .. }) = claim.answer else {
        return Err("the admitted owner could not claim its slot".into());
    };
    let watched = sim.call(watcher, Command::Packs(Packs { content, execution }))?;
    if !matches!(watched.answer, Answer::Packs(_)) {
        return Err("the independent watcher did not verify its execution".into());
    }
    if matches!(plant, Plant::MissingBinding) {
        let session = sim.core.sessions.get_mut(&owner).unwrap();
        session.executions.clear();
        session.expires = ORDINARY;
    }
    sim.now = queued_at + first_read;
    sim.core.tick(sim.now);
    if sim.core.hello(sim.now, &resume(owner, sim.core.epoch)).is_err() {
        return Err("verified work lost its cleanup lifetime".into());
    }
    sim.oracle(owner, queued_at + 100)?;
    sim.oracle(watcher, queued_at + 100)?;
    if sim.core.hello(sim.now, &resume(unbound, sim.core.epoch)).is_ok() {
        return Err("an unbound session outlived its ordinary expiry".into());
    }
    let subscriber = sim.open(5);
    let deadline = queued_at + extension;
    let extended = sim.call(subscriber, Command::Subscribe(Subscribe {
        content, execution, deadline_ms: deadline, policy: LocalPolicy::Never,
    }))?;
    if !matches!(extended.answer, Answer::Binding(_)) {
        return Err("the later subscriber could not extend the live execution".into());
    }
    if matches!(plant, Plant::SlidingCeiling) {
        sim.core.sessions.get_mut(&owner).unwrap().maximum_expires += sim.now;
    }
    sim.oracle(owner, deadline)?;
    sim.oracle(watcher, deadline)?;

    let lookup = OwnedAssignment {
        content, execution, executor: executor.clone(), claimant: CLAIMANT,
    };
    let pending = sim.call(owner, Command::OwnedAssignment(lookup.clone()))?;
    if pending.answer != Answer::OwnedAssignment(OwnedAssignmentAnswer { attempt }) {
        return Err("the owner lookup changed its assignment".into());
    }
    let events = sim.core.jobs.execution(execution).unwrap().events(0).to_vec();
    if matches!(plant, Plant::LostReplayIdentity) {
        sim.core.sessions.get_mut(&owner).unwrap().last.as_mut().unwrap().reply =
            encode(&ServerFrame::Response(Response {
                epoch: sim.core.epoch,
                session: watcher,
                sequence: pending.request.sequence,
                answer: pending.answer.clone(),
            }));
    }
    let (replayed, effects) = sim.core.request(sim.now, &pending.request).unwrap();
    if replayed != pending.bytes || !effects.is_empty()
        || sim.core.jobs.execution(execution).unwrap().events(0) != events.as_slice()
    {
        return Err("a lost-response retry changed its identity or effects".into());
    }
    let conflicting = Request {
        command: Command::OwnedAssignment(OwnedAssignment { claimant: [99; 32], ..lookup.clone() }),
        ..pending.request.clone()
    };
    if matches!(plant, Plant::ReusedRequestIdentity) {
        sim.core.sessions.get_mut(&owner).unwrap().last.as_mut().unwrap().request =
            encode(&ClientFrame::Request(conflicting.clone()));
    }
    if sim.core.request(sim.now, &conflicting).is_ok() {
        return Err("a reused replay identity accepted different request bytes".into());
    }
    finish(&mut sim, owner, binding, lookup, deadline, &events)
}

fn finish(
    sim: &mut Sim,
    owner: Digest,
    binding: Binding,
    lookup: OwnedAssignment,
    deadline: u64,
    events: &[blaze_wire::EventItem],
) -> Result<(), String> {
    let content = binding.subscription.content;
    let execution = binding.subscription.execution;
    let executor = lookup.executor.clone();
    let attempt = binding.attempt;
    let queued_at = sim.core.jobs.execution(execution).unwrap().stages()[0].1;
    let count = sim.core.jobs.execution(execution).unwrap().subscribers().count();
    for command in [
        Command::Subscribe(Subscribe {
            content, execution, deadline_ms: queued_at + WORK + 1, policy: LocalPolicy::Never,
        }),
        Command::Submit(submission(&job(10, sim.now + WORK + 1), need())),
        // Even a short new execution cannot slide this old session's immutable ceiling.
        Command::Submit(submission(&job(10, sim.now + 1), need())),
    ] {
        let refused = sim.call(owner, command)?;
        if !matches!(refused.answer, Answer::Error(_)) || !refused.effects.is_empty()
            || sim.core.jobs.executions().count() != 1
            || sim.core.jobs.execution(execution).unwrap().subscribers().count() != count
            || sim.core.jobs.execution(execution).unwrap().deadline() != Some(deadline)
            || sim.core.jobs.execution(execution).unwrap().events(0) != events
        {
            return Err("a lifetime refusal published a transition prefix".into());
        }
        sim.oracle(owner, deadline)?;
    }
    sim.now = deadline + 1;
    sim.core.tick(sim.now);
    let current = sim.call(owner, Command::OwnedAssignment(lookup))?;
    if current.answer != Answer::OwnedAssignment(OwnedAssignmentAnswer { attempt }) {
        return Err("the deadline discarded ownership before cleanup".into());
    }
    let retired = sim.call(owner, Command::RetireAssignment(RetireAssignment {
        content, execution, executor: executor.clone(), attempt, claimant: CLAIMANT,
    }))?;
    if retired.answer != Answer::Ack(Ack {})
        || sim.core.jobs.admission_view().executor(&executor).unwrap().busy != Some((execution, attempt))
    {
        return Err("retirement released capacity before the stopped worker's acknowledgment".into());
    }
    let stopped = sim.call(owner, Command::KillAck(KillAck {
        content, execution, executor: executor.clone(), attempt, alive: true,
    }))?;
    if stopped.answer != Answer::KillAck(KillAnswer { accepted: true })
        || sim.core.jobs.admission_view().executor(&executor).unwrap().busy.is_some()
    {
        return Err("the retained owner could not acknowledge its stopped slot".into());
    }
    sim.oracle(owner, deadline)?;
    sim.now = deadline + CLEANUP + 1;
    sim.core.tick(sim.now);
    if sim.core.hello(sim.now, &resume(owner, sim.core.epoch)).is_ok()
        || sim.core.request(sim.now, &stopped.request).is_ok()
    {
        return Err("expired cleanup authority was revived".into());
    }
    let fresh = sim.open(1);
    if fresh == owner || sim.core.request(sim.now, &stopped.request).is_ok() {
        return Err("a new lifetime replayed an expired owner's request".into());
    }
    Ok(())
}

/// The invariant: verified work retains its bounded cleanup lifetime across coalesced
/// deadline changes; replay keeps the exact request and response identity, refused limits
/// publish no transition prefix, and neither reconnect nor a new lifetime revives expiry.
#[hegel::test(test_cases = 64)]
fn execution_bindings_retain_cleanup_without_sliding_replay_or_expiry(tc: hegel::TestCase) {
    let queued_at = if tc.draw(gs::booleans()) { ORDINARY } else { 0 };
    let extension = if tc.draw(gs::booleans()) { WORK } else { 200 };
    let first_read = 11 + u64::from(tc.draw(gs::integers::<u8>().max_value(78)));
    scenario(queued_at, extension, first_read, Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_a_missing_verified_execution_binding() {
    assert!(scenario(0, 200, 11, Plant::MissingBinding).unwrap_err()
        .contains("verified work lost its cleanup lifetime"));
}

#[test]
fn the_oracle_catches_a_sliding_session_ceiling() {
    assert!(scenario(10, WORK, 11, Plant::SlidingCeiling).unwrap_err()
        .contains("slid its immutable lifetime ceiling"));
}

#[test]
fn the_oracle_catches_a_lost_reply_with_changed_replay_identity() {
    assert!(scenario(0, 200, 11, Plant::LostReplayIdentity).unwrap_err()
        .contains("a lost-response retry changed its identity or effects"));
}

#[test]
fn the_oracle_catches_different_bytes_reusing_a_replay_identity() {
    assert!(scenario(0, 200, 11, Plant::ReusedRequestIdentity).unwrap_err()
        .contains("a reused replay identity accepted different request bytes"));
}
