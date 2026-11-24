use super::*;
use crate::coordinator::{Config, Coordinator};
use crate::protocol::*;
use crate::sys::{TempDir, path_exists, path_is_file, read_file, write_file};
use blaze_dist::{
    Refusal,
    admission::{Admission, AdmissionConfig},
    jobs::{ExecutionConfig, Jobs},
};
use blaze_own::{Vec, format_in, vec_in};
use blaze_rng::SplitMix64;
use blaze_wire::{
    BatchResult, EntryKey, EntryRef, ItemOutcome, ItemResult, JobRequest, Limits, LocalPolicy,
    Namespace, Reservation, RootSet, Set, WireAlloc, WireString, WorkJoinRequest, WorkLeaseRequest,
    WorkName, decode,
};

const NEED: Reservation = Reservation { memory: 1, cpus: 1, disk: 1 };

fn test_alloc() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

struct VirtualClock {
    now_ms: u64,
}

impl VirtualClock {
    fn new() -> Self {
        Self { now_ms: 0 }
    }

    fn advance(&mut self, duration: Duration) {
        let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self.now_ms = self.now_ms.saturating_add(ms);
    }
}

struct World {
    clock: VirtualClock,
    coordinator: Coordinator,
    welcome: LocalWelcome,
    sequence: u64,
    binding: Binding,
    lead: WireString,
    helper: WireString,
    alloc: WireAlloc,
}

impl World {
    fn new() -> Self {
        let alloc = test_alloc();
        let mut admission = Admission::new(AdmissionConfig::default(), alloc);
        admission.add_machine("machine", [1; 32], Reservation { memory: 8, cpus: 8, disk: 8 });
        let lead = admission.add_builder(0, "machine", NEED, alloc).unwrap();
        let helper = admission.add_builder(0, "machine", NEED, alloc).unwrap();
        let lead = WireString::from_str_in(&lead, alloc);
        let helper = WireString::from_str_in(&helper, alloc);
        let jobs = Jobs::new(
            admission,
            ExecutionConfig {
                heartbeat_ms: 1_000_000,
                work: blaze_dist::work::WorkConfig {
                    grant_roots: 1,
                    lease_ms: 10,
                    progress_first_ms: 1_000_000,
                    ..Default::default()
                },
                ..Default::default()
            },
            alloc,
        );
        let mut coordinator = Coordinator::new(jobs, Config::default(), [7; 32], alloc);
        let welcome = coordinator.hello(0, &hello([8; 32])).unwrap();
        let request = JobRequest {
            unit: [9; 32],
            platform: [1; 32],
            namespace: Namespace::Main,
            source: [0; 32],
            deadline_ms: 1_000_000,
            local: LocalPolicy::Never,
        };
        let (bytes, _) = coordinator
            .request(
                0,
                &Request {
                    epoch: welcome.epoch,
                    session: welcome.session,
                    sequence: 1,
                    command: Command::Submit(submission(&request, NEED, alloc)),
                },
                alloc,
            )
            .unwrap();
        let ServerFrame::Response(Response { answer: Answer::Binding(binding), .. }) =
            decode(&bytes, &Limits::DOCUMENTED, alloc).unwrap()
        else {
            panic!("submit")
        };
        let mut world = Self {
            clock: VirtualClock::new(),
            coordinator,
            welcome,
            sequence: 2,
            binding,
            lead,
            helper,
            alloc,
        };
        for executor in [world.lead.clone(), world.helper.clone()] {
            let joined = world.call(Command::Join(Join {
                content: world.binding.subscription.content,
                executor,
                request: WorkJoinRequest {
                    execution: world.binding.subscription.execution,
                    attempt: world.binding.attempt,
                    roots: RootSet {
                        spec: [1; 32],
                        roots: [2; 32],
                        count: 2,
                        index: [3; 32],
                        inputs: [4; 32],
                    },
                    rejected: Set::new(Vec::new_in(alloc)),
                },
            }));
            assert!(matches!(joined, Answer::Join(_)), "{joined:?}");
        }
        world
    }

    fn call(&mut self, command: Command) -> Answer {
        let now = self.clock.now_ms;
        let request = Request {
            epoch: self.welcome.epoch,
            session: self.welcome.session,
            sequence: self.sequence,
            command,
        };
        self.sequence += 1;
        let (bytes, _) = self.coordinator.request(now, &request, self.alloc).unwrap();
        let ServerFrame::Response(response) =
            decode(&bytes, &Limits::DOCUMENTED, self.alloc).unwrap()
        else {
            panic!("response")
        };
        response.answer
    }

    fn result(&mut self, executor: &str) -> Command {
        let alloc = self.alloc;
        // A carrier result uses only the actual grant issued by the production coordinator.
        let joined = self.call(Command::Join(Join {
            content: self.binding.subscription.content,
            executor: WireString::from_str_in(executor, alloc),
            request: WorkJoinRequest {
                execution: self.binding.subscription.execution,
                attempt: self.binding.attempt,
                roots: RootSet {
                    spec: [1; 32],
                    roots: [2; 32],
                    count: 2,
                    index: [3; 32],
                    inputs: [4; 32],
                },
                rejected: Set::new(Vec::new_in(alloc)),
            },
        }));
        assert!(matches!(joined, Answer::Join(_)), "{joined:?}");
        let leased = self.call(Command::Lease(Lease {
            content: self.binding.subscription.content,
            executor: WireString::from_str_in(executor, alloc),
            request: WorkLeaseRequest {
                execution: self.binding.subscription.execution,
                attempt: self.binding.attempt,
            },
        }));
        let Answer::Lease(answer) = leased else { panic!("{leased:?}") };
        let grant = answer.grant.expect("actual lease");
        Command::Result(BatchSubmission {
            content: self.binding.subscription.content,
            executor: WireString::from_str_in(executor, alloc),
            request: BatchResult {
                lease: grant.lease,
                seq: 1,
                items: vec_in![
                    alloc;
                    ItemResult {
                        name: WorkName([5; 32]),
                        outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([6; 32]) }),
                        found: Set::new(Vec::new_in(alloc)),
                        unnamed: Set::new(Vec::new_in(alloc)),
                    }
                ],
                packs: Vec::new_in(alloc),
                returned_roots: Set::new(Vec::new_in(alloc)),
                returned_names: Set::new(Vec::new_in(alloc)),
            },
        })
    }
}

fn scenario(elapsed: u64, replacement_first: bool, rewrite: bool) -> Result<(), WireString> {
    let alloc = test_alloc();
    let mut world = World::new();
    let helper = world.helper.clone();
    let mut held = world.result(helper.as_str());
    let captured = encode(&held, alloc);
    world.clock.advance(Duration::from_millis(elapsed));
    let expired = elapsed > 10;
    let lead = world.lead.clone();
    let replacement = expired.then(|| world.result(lead.as_str()));
    if replacement_first
        && let Some(command) = &replacement
        && !matches!(world.call(command.clone()), Answer::Result(_))
    {
        return Err(WireString::from_str_in("replacement was not committed", alloc));
    }
    // Plant the tempting but invalid workaround: turn the delayed submission into the new
    // owner's result. The identity oracle must reject this even when the core would accept it.
    if rewrite && let Some(command) = &replacement {
        held = command.clone();
    }
    if encode(&held, alloc) != captured {
        return Err(WireString::from_str_in("the gated carrier changed before submission", alloc));
    }
    let answer = world.call(held);
    match (expired, answer) {
        (false, Answer::Result(_)) => {}
        (true, Answer::Error(error)) if Refusal::of(&error) == Some(Refusal::DeadLease) => {}
        (_, other) => {
            return Err(format_in!(
                alloc,
                "issued result violated the lease oracle after {elapsed}ms: {other:?}"
            ));
        }
    }
    if !replacement_first
        && let Some(command) = replacement
        && !matches!(world.call(command), Answer::Result(_))
    {
        return Err(WireString::from_str_in("replacement was not committed", alloc));
    }
    Ok(())
}

#[test]
fn virtual_clock_and_actor_order_preserve_the_captured_results_authority() {
    let mut rng = SplitMix64::new(0x5245_5355_4c54_4741);
    for _ in 0..64 {
        let elapsed = rng.next_u64() % 41;
        let replacement_first = (rng.next_u64() & 1) == 0;
        scenario(elapsed, replacement_first, false).unwrap();
    }
}

#[test]
fn the_identity_oracle_catches_reissuing_the_captured_result_under_a_new_lease() {
    assert!(scenario(11, false, true).is_err());
    scenario(0, false, false).unwrap();
    scenario(11, true, false).unwrap();
}

#[test]
fn a_capture_roundtrips_the_exact_command_and_actual_answer_and_is_claimed_once() {
    let alloc = test_alloc();
    let mut world = World::new();
    let helper = world.helper.clone();
    let command = world.result(helper.as_str());
    let directory = TempDir::new("blaze-gate", alloc).unwrap();
    write_file(directory.join("release").as_str(), b"").unwrap();
    let gate = ResultGate::new(
        WireString::from_str_in(directory.path(), alloc),
        Duration::from_secs(2),
        alloc,
    )
    .unwrap();
    let capture = gate.hold(&command).unwrap().unwrap();
    let saved = read_file(directory.join("result.cbor").as_str(), alloc).unwrap();
    assert_eq!(decode::<Command>(&saved, &Limits::DOCUMENTED, alloc).unwrap(), command);
    assert_eq!(saved, encode(&command, alloc));
    let answer = world.call(command.clone());
    assert!(matches!(answer, Answer::Result(_)), "{answer:?}");
    capture.answer(&answer).unwrap();
    assert_eq!(
        decode::<Answer>(
            &read_file(directory.join("answer.cbor").as_str(), alloc).unwrap(),
            &Limits::DOCUMENTED,
            alloc,
        )
        .unwrap(),
        answer
    );
    assert!(
        gate.hold(&command).unwrap().is_none(),
        "another unit must not pause on the same one-shot gate"
    );
    assert!(!path_exists(directory.join(".ready.json.tmp").as_str()));
}

#[test]
fn a_missing_release_times_out_and_leaves_complete_failure_evidence() {
    let alloc = test_alloc();
    let mut world = World::new();
    let helper = world.helper.clone();
    let command = world.result(helper.as_str());
    let directory = TempDir::new("blaze-gate", alloc).unwrap();
    let dir_str = WireString::from_str_in(directory.path(), alloc);
    assert!(ResultGate::new(dir_str.clone(), Duration::ZERO, alloc).is_err());
    assert!(ResultGate::new(dir_str.clone(), Duration::MAX, alloc).is_err());
    let gate = ResultGate::new(dir_str, Duration::from_nanos(1), alloc).unwrap();
    assert!(matches!(gate.hold(&command), Err(Error::Protocol(_))));
    assert!(path_is_file(directory.join("error.txt").as_str()));
    assert!(!path_exists(directory.join("answer.cbor").as_str()));
    assert_eq!(
        decode::<Command>(
            &read_file(directory.join("result.cbor").as_str(), alloc).unwrap(),
            &Limits::DOCUMENTED,
            alloc,
        )
        .unwrap(),
        command
    );
}
