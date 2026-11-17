use super::*;
use crate::coordinator::{Config, Coordinator};
use crate::protocol::*;
use blaze_core::{
    Driver,
    sim::{Faults, Sim},
};
use blaze_dist::{
    Refusal,
    admission::{Admission, AdmissionConfig},
    jobs::{ExecutionConfig, Jobs},
};
use blaze_wire::{
    BatchResult, EntryKey, EntryRef, ItemOutcome, ItemResult, JobRequest, Limits, LocalPolicy,
    Namespace, Reservation, RootSet, Set, WorkJoinRequest, WorkLeaseRequest, WorkName, decode,
};
use hegel::generators as gs;

const NEED: Reservation = Reservation { memory: 1, cpus: 1, disk: 1 };

struct World {
    sim: Sim,
    coordinator: Coordinator,
    welcome: LocalWelcome,
    sequence: u64,
    binding: Binding,
    lead: String,
    helper: String,
}

impl World {
    fn new(seed: u64) -> Self {
        let mut admission = Admission::new(AdmissionConfig::default());
        admission.add_machine("machine", [1; 32], Reservation { memory: 8, cpus: 8, disk: 8 });
        let lead = admission.add_builder(0, "machine", NEED).unwrap();
        let helper = admission.add_builder(0, "machine", NEED).unwrap();
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
        );
        let mut coordinator = Coordinator::new(jobs, Config::default(), [7; 32]);
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
                    command: Command::Submit(submission(&request, NEED)),
                },
            )
            .unwrap();
        let ServerFrame::Response(Response { answer: Answer::Binding(binding), .. }) =
            decode(&bytes, &Limits::DOCUMENTED).unwrap()
        else {
            panic!("submit")
        };
        let mut world = Self {
            sim: Sim::new(seed, Faults::default()),
            coordinator,
            welcome,
            sequence: 2,
            binding,
            lead,
            helper,
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
                    rejected: Set::new(Vec::new()),
                },
            }));
            assert!(matches!(joined, Answer::Join(_)), "{joined:?}");
        }
        world
    }

    fn call(&mut self, command: Command) -> Answer {
        let now = self.sim.now().as_millis() as u64;
        let request = Request {
            epoch: self.welcome.epoch,
            session: self.welcome.session,
            sequence: self.sequence,
            command,
        };
        self.sequence += 1;
        let (bytes, _) = self.coordinator.request(now, &request).unwrap();
        let ServerFrame::Response(response) = decode(&bytes, &Limits::DOCUMENTED).unwrap() else {
            panic!("response")
        };
        response.answer
    }

    fn result(&mut self, executor: &str) -> Command {
        // A carrier result uses only the actual grant issued by the production coordinator.
        let joined = self.call(Command::Join(Join {
            content: self.binding.subscription.content,
            executor: executor.to_owned(),
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
                rejected: Set::new(Vec::new()),
            },
        }));
        assert!(matches!(joined, Answer::Join(_)), "{joined:?}");
        let leased = self.call(Command::Lease(Lease {
            content: self.binding.subscription.content,
            executor: executor.to_owned(),
            request: WorkLeaseRequest {
                execution: self.binding.subscription.execution,
                attempt: self.binding.attempt,
            },
        }));
        let Answer::Lease(answer) = leased else { panic!("{leased:?}") };
        let grant = answer.grant.expect("actual lease");
        Command::Result(BatchSubmission {
            content: self.binding.subscription.content,
            executor: executor.to_owned(),
            request: BatchResult {
                lease: grant.lease,
                seq: 1,
                items: vec![ItemResult {
                    name: WorkName([5; 32]),
                    outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([6; 32]) }),
                    found: Set::new(Vec::new()),
                    unnamed: Set::new(Vec::new()),
                }],
                packs: Vec::new(),
                returned_roots: Set::new(Vec::new()),
                returned_names: Set::new(Vec::new()),
            },
        })
    }
}

fn scenario(seed: u64, elapsed: u64, replacement_first: bool, rewrite: bool) -> Result<(), String> {
    let mut world = World::new(seed);
    let mut held = world.result(&world.helper.clone());
    let captured = encode(&held);
    world.sim.advance(Duration::from_millis(elapsed));
    let expired = elapsed > 10;
    let replacement = expired.then(|| world.result(&world.lead.clone()));
    if replacement_first && let Some(command) = &replacement {
        if !matches!(world.call(command.clone()), Answer::Result(_)) {
            return Err("replacement was not committed".to_owned());
        }
    }
    // Plant the tempting but invalid workaround: turn the delayed submission into the new
    // owner's result. The identity oracle must reject this even when the core would accept it.
    if rewrite && let Some(command) = &replacement {
        held = command.clone();
    }
    if encode(&held) != captured {
        return Err("the gated carrier changed before submission".to_owned());
    }
    let answer = world.call(held);
    match (expired, answer) {
        (false, Answer::Result(_)) => {}
        (true, Answer::Error(error)) if Refusal::of(&error) == Some(Refusal::DeadLease) => {}
        (_, other) => {
            return Err(format!(
                "issued result violated the lease oracle after {elapsed}ms: {other:?}"
            ));
        }
    }
    if !replacement_first && let Some(command) = replacement {
        if !matches!(world.call(command), Answer::Result(_)) {
            return Err("replacement was not committed".to_owned());
        }
    }
    Ok(())
}

#[hegel::test(test_cases = 32)]
fn virtual_clock_and_actor_order_preserve_the_captured_results_authority(tc: hegel::TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let elapsed = tc.draw(gs::integers::<u64>().max_value(40));
    let order = tc.draw(gs::integers::<u8>());
    scenario(seed, elapsed, order % 2 == 0, false).unwrap();
}

#[test]
fn the_identity_oracle_catches_reissuing_the_captured_result_under_a_new_lease() {
    assert!(scenario(7, 11, false, true).is_err());
    scenario(7, 0, false, false).unwrap();
    scenario(7, 11, true, false).unwrap();
}

#[test]
fn a_capture_roundtrips_the_exact_command_and_actual_answer_and_is_claimed_once() {
    let mut world = World::new(1);
    let command = world.result(&world.helper.clone());
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("release"), b"").unwrap();
    let gate = ResultGate::new(directory.path().to_owned(), Duration::from_secs(2)).unwrap();
    let capture = gate.hold(&command).unwrap().unwrap();
    let saved = fs::read(directory.path().join("result.cbor")).unwrap();
    assert_eq!(decode::<Command>(&saved, &Limits::DOCUMENTED).unwrap(), command);
    assert_eq!(saved, encode(&command));
    let answer = world.call(command.clone());
    assert!(matches!(answer, Answer::Result(_)), "{answer:?}");
    capture.answer(&answer).unwrap();
    assert_eq!(
        decode::<Answer>(
            &fs::read(directory.path().join("answer.cbor")).unwrap(),
            &Limits::DOCUMENTED
        )
        .unwrap(),
        answer
    );
    assert!(
        gate.hold(&command).unwrap().is_none(),
        "another unit must not pause on the same one-shot gate"
    );
    assert!(!directory.path().join(".ready.json.tmp").exists());
}

#[test]
fn a_missing_release_times_out_and_leaves_complete_failure_evidence() {
    let mut world = World::new(1);
    let command = world.result(&world.helper.clone());
    let directory = tempfile::tempdir().unwrap();
    assert!(ResultGate::new(directory.path().to_owned(), Duration::ZERO).is_err());
    assert!(ResultGate::new(directory.path().to_owned(), Duration::MAX).is_err());
    let gate = ResultGate::new(directory.path().to_owned(), Duration::from_nanos(1)).unwrap();
    assert!(matches!(gate.hold(&command), Err(Error::Protocol(_))));
    assert!(directory.path().join("error.txt").is_file());
    assert!(!directory.path().join("answer.cbor").exists());
    assert_eq!(
        decode::<Command>(
            &fs::read(directory.path().join("result.cbor")).unwrap(),
            &Limits::DOCUMENTED
        )
        .unwrap(),
        command
    );
}
