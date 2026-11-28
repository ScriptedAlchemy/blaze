//! Accepted output is independent of old subscribers and carrier sessions.

use core::fmt;

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::{CompletionMode, ExecutionConfig};
use blaze_own::Vec;
use blaze_sim::{Broken, Config as SimConfig, Tape};
use blaze_wire::{LocalPolicy, Namespace, Outcome, PackRef, Reservation, RootSet, Set, WorkJoinRequest};

use super::*;

#[derive(Debug)]
enum PublicationBroken {
    ResultUnavailable,
    LostResultSession,
    OldTerminalChanged,
    DeliveryReplayChanged,
}

impl fmt::Display for PublicationBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ResultUnavailable => "retained accepted output could not be retrieved or delivered",
            Self::LostResultSession => "accepted-result delivery lost its own request lifetime",
            Self::OldTerminalChanged => "delivery changed an old subscriber's cancelled terminal",
            Self::DeliveryReplayChanged => "a lost delivery reply changed its replay bytes or effects",
        })
    }
}

blaze_sim::plants! {
    enum PublicationPlant breaks PublicationBroken {
        None,
        HelloLifetimeOnly => PublicationBroken::LostResultSession,
    }
}

struct Exchange {
    request: Request,
    bytes: WireVec<u8>,
    answer: Answer,
    effects: WireVec<(ExecutionId, Effect)>,
}

fn exchange(
    core: &mut Coordinator,
    now: u64,
    session: &mut LocalWelcome,
    command: Command,
) -> Result<Exchange, WireError> {
    let alloc = core.alloc;
    let request = Request {
        epoch: session.epoch,
        session: session.session,
        sequence: session.next_request,
        command,
    };
    let (bytes, effects) = core.request(now, &request, alloc)?;
    let ServerFrame::Response(response) = decode(&bytes, &Limits::DOCUMENTED, alloc).unwrap()
    else {
        panic!("the real carrier did not frame its reply")
    };
    assert_eq!(response.epoch, request.epoch);
    assert_eq!(response.session, request.session);
    assert_eq!(response.sequence, request.sequence);
    session.next_request += 1;
    Ok(Exchange { request, bytes, answer: response.answer, effects })
}

fn request_for(unit: u8, deadline_ms: u64) -> JobRequest {
    JobRequest {
        unit: [unit; 32],
        platform: [1; 32],
        namespace: Namespace::Main,
        source: [0; 32],
        deadline_ms,
        local: LocalPolicy::Never,
    }
}

fn result_lifetime(tape: &Tape, plant: PublicationPlant) -> Result<(), Broken<PublicationBroken>> {
    let alloc = blaze_own::testing::heap().shared();
    let need = Reservation { memory: 1, cpus: 1, disk: 1 };
    let mut admission = Admission::new(AdmissionConfig::default(), alloc);
    admission.add_machine("machine", [1; 32], need.clone());
    admission.add_builder(0, "machine", need.clone(), alloc).unwrap();
    let jobs = Jobs::new(admission, ExecutionConfig {
        completion: CompletionMode::PublishedWork,
        ..ExecutionConfig::default()
    }, alloc).start_on_claim();
    let mut core = Coordinator::new(jobs, Config {
        max_sessions: 1,
        max_executions: 1,
        max_subscribers: 1,
        session_ms: 5,
        max_execution_ms: 100,
        owner_cleanup_ms: 5,
        terminal_ms: 3,
        ..Config::default()
    }, [7; 32], alloc);
    let mut owner = core.hello(0, &hello([1; 32])).unwrap();
    let submitted = exchange(&mut core, 0, &mut owner,
        Command::Submit(submission(&request_for(1, 30), need.clone(), alloc))).unwrap();
    let Answer::Binding(binding) = submitted.answer else { panic!("initial submit") };
    let content = binding.subscription.content;
    let execution = binding.subscription.execution;
    let claimed = exchange(&mut core, 0, &mut owner,
        Command::Claim(Claim { content, execution, claimant: [2; 32] })).unwrap();
    let Answer::Claimed(Claimed { executor: Some(executor), attempt, admission, .. }) = claimed.answer
    else { panic!("initial claim") };
    assert!(matches!(admission, AdmissionState::Resolving(_)));
    let join = Join {
        content,
        executor: executor.clone(),
        request: WorkJoinRequest {
            execution,
            attempt,
            roots: RootSet { spec: [1; 32], roots: [2; 32], count: 0, index: [3; 32], inputs: [4; 32] },
            rejected: Set::new(Vec::new_in(alloc)),
        },
    };
    let before_resolution = exchange(&mut core, 0, &mut owner, Command::Join(join.clone())).unwrap();
    assert!(matches!(before_resolution.answer, Answer::Error(_)));
    let resolved = exchange(&mut core, 0, &mut owner, Command::ResolvePublication(ResolvePublication {
        content, execution, executor: executor.clone(), attempt, packs: None,
    })).unwrap();
    assert_eq!(resolved.answer, Answer::AdmissionState(AdmissionState::Compile(CompileReady {
        execution, attempt,
    })));
    assert!(matches!(exchange(&mut core, 0, &mut owner, Command::Join(join)).unwrap().answer, Answer::Join(_)));
    let packs = Vec::from_slice_in(&[PackRef {
        pack: [8; 32], size: 64, keys: [9; 32], packed: None, format: blaze_wire::DIST_PACK_FORMAT,
    }], alloc);
    let accepted = exchange(&mut core, 0, &mut owner, Command::CommitPublication(CommitPublication {
        content, execution, attempt, executor: executor.clone(), packs: packs.clone(),
    })).unwrap();
    assert!(matches!(accepted.answer, Answer::Publication(Publication { packs: Some(_), .. })));
    assert!(matches!(exchange(&mut core, 0, &mut owner, Command::Cancel(binding.clone())).unwrap().answer, Answer::Ack(_)));
    let old_terminal = core.jobs.terminal(execution, binding.subscription.subscriber).unwrap().clone();
    assert_eq!(old_terminal.termination.outcome, Outcome::Cancelled);
    assert!(matches!(exchange(&mut core, 0, &mut owner, Command::KillAck(KillAck {
        content, execution, attempt, executor, alive: true,
    })).unwrap().answer, Answer::KillAck(KillAnswer { accepted: true })));
    exchange(&mut core, 0, &mut owner, Command::Release(Release {})).unwrap();
    assert!(core.sessions.is_empty());

    // Every original interest, session, worker and terminal-retention interval is gone.
    // The actual retention timer still has to keep the accepted, undelivered record.
    let reopened = 50 + tape.below(20, "time after the original lifetime");
    core.tick(reopened, alloc);
    assert!(core.jobs.execution(execution).unwrap().publication_pending());
    assert_eq!(core.jobs.executions().count(), core.config.max_executions);
    assert_eq!(core.jobs.execution(execution).unwrap().subscribers().count(), core.config.max_subscribers);
    let mut reader = core.hello(reopened, &hello([3; 32])).unwrap();
    let deadline = reopened + 40;
    let found = exchange(&mut core, reopened, &mut reader,
        Command::Submit(submission(&request_for(1, deadline), need.clone(), alloc))).unwrap();
    let Answer::AcceptedPublication(handle) = found.answer else {
        return Err(Broken::Oracle(PublicationBroken::ResultUnavailable));
    };
    assert_eq!(handle.content, content);
    assert_eq!(handle.execution, execution);
    assert_eq!(handle.accepted_attempt, attempt);
    assert_eq!(handle.packs, packs);
    assert!(!handle.delivered);
    assert!(found.effects.is_empty());
    assert_eq!(core.jobs.execution(execution).unwrap().subscribers().count(), 1);
    assert!(core.sessions[&reader.session].executions.is_empty());
    let cold = exchange(&mut core, reopened, &mut reader,
        Command::Submit(submission(&request_for(2, deadline), need.clone(), alloc))).unwrap();
    assert!(matches!(cold.answer, Answer::Error(ref error)
        if error.message.as_str() == "execution-retention-capacity"));
    if core.jobs.terminal(execution, binding.subscription.subscriber) != Some(&old_terminal) {
        return Err(Broken::Oracle(PublicationBroken::OldTerminalChanged));
    }
    // Reuse the now-extended session after its ordinary Hello lease. This request's
    // shorter deadline fits its fixed maximum even though a new maximum-length cold
    // execution at this later instant would not fit. Accepted lookup creates no flight.
    let shorter_deadline = deadline - 1 - tape.below(9, "shorter accepted-result request");
    let again = exchange(&mut core, reopened + 6, &mut reader,
        Command::Submit(submission(&request_for(1, shorter_deadline), need.clone(), alloc))).unwrap();
    let Answer::AcceptedPublication(again) = again.answer else {
        return Err(Broken::Oracle(PublicationBroken::ResultUnavailable));
    };
    assert_eq!(again, handle);
    assert_eq!(core.sessions[&reader.session].expires, deadline + core.config.owner_cleanup_ms);
    assert_eq!(core.sessions[&reader.session].maximum_expires,
        reopened + core.config.session_ms + core.config.max_execution_ms + core.config.owner_cleanup_ms);
    if matches!(plant, PublicationPlant::HelloLifetimeOnly) {
        // Remove only the production Submit(Accepted) lifetime extension. The old
        // execution has no attached deadline that could accidentally keep this alive.
        core.sessions.get_mut(&reader.session).unwrap().expires = reopened + core.config.session_ms;
    }
    let delivered_at = reopened + 6 + tape.below(20, "delivery after the ordinary session lease");
    core.tick(delivered_at, alloc);
    let delivered = exchange(&mut core, delivered_at, &mut reader,
        Command::DeliverPublication(DeliverPublication { content, execution, packs: handle.packs }));
    let Ok(delivered) = delivered else {
        return Err(Broken::Oracle(PublicationBroken::LostResultSession));
    };
    if !matches!(delivered.answer, Answer::Ack(_)) {
        return Err(Broken::Oracle(PublicationBroken::ResultUnavailable));
    }
    assert!(core.jobs.execution(execution).unwrap().publication_delivered());
    assert!(core.sessions[&reader.session].executions.is_empty());
    if core.jobs.terminal(execution, binding.subscription.subscriber) != Some(&old_terminal) {
        return Err(Broken::Oracle(PublicationBroken::OldTerminalChanged));
    }
    if tape.coin("delivery reply is lost") {
        let (bytes, effects) = core.request(delivered_at + 1, &delivered.request, alloc).unwrap();
        if bytes != delivered.bytes || !effects.is_empty() {
            return Err(Broken::Oracle(PublicationBroken::DeliveryReplayChanged));
        }
    }
    // Only acknowledged delivery permits ordinary retention to free the capacity.
    core.tick(delivered_at + 2, alloc);
    assert!(core.jobs.execution(execution).is_none());
    exchange(&mut core, delivered_at + 2, &mut reader, Command::Release(Release {})).unwrap();
    let mut next = core.hello(delivered_at + 2, &hello([4; 32])).unwrap();
    let cold = exchange(&mut core, delivered_at + 2, &mut next,
        Command::Submit(submission(&request_for(2, delivered_at + 30), need, alloc))).unwrap();
    assert!(matches!(cold.answer, Answer::Binding(_)));
    Ok(())
}

/// The invariant (simulated): accepted output survives original subscriber/session/terminal expiry;
/// full metadata capacity still permits lookup and exact delivery under a new request's
/// own bounded lifetime, without changing an old terminal or losing replay identity.
#[test]
fn accepted_output_outlives_subscribers_and_is_deliverable_at_capacity() {
    blaze_sim::check(SimConfig::cases(64), |tape| result_lifetime(tape, PublicationPlant::None));
}

#[test]
fn the_oracle_catches_an_accepted_result_using_only_its_hello_lifetime() {
    blaze_sim::controls(SimConfig::cases(64), result_lifetime);
}
