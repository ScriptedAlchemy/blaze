//! Real registry transitions on a virtual clock with delayed, duplicated and stale actor calls.

use blaze_own::{Vec, format_in, vec_in};
use blaze_wire::{
    EntryKey, EntryRef, Grant, ItemOutcome, ItemResult, JobAnswer, RootSet, Set, WireAlloc,
    WireString, WorkName,
};

use super::*;
use crate::admission::AdmissionConfig;
use crate::sim::Rng;

const LEAD: Digest = [11; 32];
const HELPER: Digest = [12; 32];
const STRANGER: Digest = [13; 32];

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

#[derive(Clone, Copy)]
enum Plant {
    None,
    SkipRetirement,
    ReleaseWithoutStop,
    AcceptStaleRetirement,
    AcceptStaleAcknowledgment,
    IgnoreOwner,
    KeepRetiredWork,
    ForgetPromotedOwner,
    KeepObservedAttempt,
}

struct Sim {
    alloc: WireAlloc,
    jobs: Jobs,
    now: Millis,
    first: JobAnswer,
    leaving: JobAnswer,
    lead: WireString,
    helper: WireString,
    original: AssignmentToken,
    grant: Grant,
    helper_reservation: Option<AssignmentToken>,
    stopped: bool,
    retired: bool,
    acknowledged: bool,
    reclaimed: bool,
    plant: Plant,
}

fn need() -> Reservation {
    Reservation { memory: 1, cpus: 1, disk: 1 }
}

fn request(tag: u8) -> JobRequest {
    JobRequest {
        unit: [tag; 32],
        platform: [1; 32],
        namespace: Namespace::Main,
        source: [0; 32],
        deadline_ms: 100_000,
        local: LocalPolicy::Never,
    }
}

fn roots() -> RootSet {
    RootSet { spec: [1; 32], roots: [2; 32], count: 2, index: [3; 32], inputs: [4; 32] }
}

fn join(jobs: &mut Jobs, now: Millis, executor: &str, token: AssignmentToken, alloc: WireAlloc) {
    jobs.join(
        now,
        executor,
        &WorkJoinRequest {
            execution: token.execution,
            attempt: token.attempt,
            roots: roots(),
            rejected: Set::empty_in(alloc),
        },
        &mut Vec::new_in(alloc),
    )
    .unwrap();
}

impl Sim {
    fn new(plant: Plant, alloc: WireAlloc) -> Sim {
        let mut admission = Admission::new(AdmissionConfig::default(), alloc);
        admission.add_machine("machine", [1; 32], Reservation { memory: 2, cpus: 2, disk: 2 });
        for _ in 0..2 {
            admission.add_builder(0, "machine", need(), alloc).unwrap();
        }
        let mut jobs = Jobs::new(
            admission,
            ExecutionConfig {
                heartbeat_ms: 100,
                work: WorkConfig { grant_roots: 1, ..WorkConfig::default() },
                ..ExecutionConfig::default()
            },
            alloc,
        );
        let first = jobs.submit(0, &request(1), need(), &mut Vec::new_in(alloc)).unwrap();
        let (lead, attempt) = jobs
            .claim(0, first.execution, LEAD, &mut Vec::new_in(alloc))
            .unwrap()
            .map(|(executor, attempt)| (WireString::from_str_in(executor, alloc), attempt))
            .unwrap();
        let leaving = jobs.submit(0, &request(1), need(), &mut Vec::new_in(alloc)).unwrap();
        let (helper, helped) = jobs
            .claim(0, first.execution, HELPER, &mut Vec::new_in(alloc))
            .unwrap()
            .map(|(executor, attempt)| (WireString::from_str_in(executor, alloc), attempt))
            .unwrap();
        assert_ne!(lead, helper);
        assert_eq!(attempt, helped);
        let original = AssignmentToken { execution: first.execution, attempt };
        join(&mut jobs, 0, lead.as_str(), original, alloc);
        join(&mut jobs, 0, helper.as_str(), original, alloc);
        let grant = jobs
            .lease(
                0,
                helper.as_str(),
                &WorkLeaseRequest { execution: first.execution, attempt },
                &mut Vec::new_in(alloc),
            )
            .unwrap()
            .grant
            .unwrap();
        Sim {
            alloc,
            jobs,
            now: 0,
            first,
            leaving,
            lead,
            helper,
            original,
            grant,
            helper_reservation: Some(original),
            stopped: false,
            retired: false,
            acknowledged: false,
            reclaimed: false,
            plant,
        }
    }

    fn oracle(&self) -> Result<(), WireString> {
        let alloc = self.alloc;
        let execution = self.jobs.execution(self.first.execution).unwrap();
        if execution.state() != ExecState::Running
            || self.jobs.terminal(self.first.execution, self.first.subscriber).is_some()
        {
            return Err(WireString::from_str_in(
                "a surviving subscriber was ended by a stale/helper retirement",
                alloc,
            ));
        }
        let actual = self.jobs.admission_view().executor(self.helper.as_str()).unwrap().busy;
        let expected = self.helper_reservation.map(|token| (token.execution, token.attempt));
        if actual != expected {
            return Err(WireString::from_str_in(
                "helper capacity changed without its exact pending-stop acknowledgment",
                alloc,
            ));
        }
        for executor in execution.executors() {
            if self.jobs.assignments.get(executor).is_none_or(|assignment| {
                assignment.token != execution.assignment() || assignment.stopping
            }) {
                return Err(WireString::from_str_in(
                    "a live execution member has no exclusive running assignment",
                    alloc,
                ));
            }
        }
        if self.retired && execution.executors().any(|who| who == self.helper.as_str()) {
            return Err(WireString::from_str_in(
                "the retired helper remains an execution member",
                alloc,
            ));
        }
        self.jobs.admission_view().check().map_err(|why| format_in!(alloc, "capacity: {why}"))
    }

    fn leave(&mut self) {
        self.jobs
            .unsubscribe(
                self.now,
                self.leaving.execution,
                self.leaving.subscriber,
                &mut Vec::new_in(self.alloc),
            )
            .unwrap();
    }

    fn retire(&mut self) -> Result<(), WireString> {
        let alloc = self.alloc;
        let mut token = self.original;
        if matches!(self.plant, Plant::AcceptStaleRetirement)
            && let Some(assignment) = self.jobs.assignments.get(self.helper.as_str())
        {
            // Plant a request whose old attempt is ignored in favor of the live assignment.
            token = assignment.token;
        }
        let kept = matches!(self.plant, Plant::KeepRetiredWork)
            .then(|| self.jobs.execution(self.first.execution).unwrap().work.clone());
        let result = if matches!(self.plant, Plant::SkipRetirement) {
            // The old agent left its subscription and sent an unmatched ACK instead.
            Ok(())
        } else {
            self.jobs.retire(
                self.now,
                self.helper.as_str(),
                token,
                HELPER,
                &mut Vec::new_in(alloc),
            )
        };
        if let Some(work) = kept {
            self.jobs.executions.get_mut(&self.first.execution).unwrap().work = work;
        }
        if self.helper_reservation == Some(self.original) && !self.acknowledged {
            result.map_err(|why| format_in!(alloc, "owned retirement refused: {why:?}"))?;
            self.retired = true;
            self.oracle()?;
            if !self.reclaimed {
                let grant = self
                    .jobs
                    .lease(
                        self.now,
                        self.lead.as_str(),
                        &WorkLeaseRequest {
                            execution: self.first.execution,
                            attempt: self.original.attempt,
                        },
                        &mut Vec::new_in(alloc),
                    )
                    .map_err(|why| format_in!(alloc, "surviving lead lease: {why:?}"))?
                    .grant
                    .ok_or_else(|| {
                        WireString::from_str_in("the retired helper's root was not returned", alloc)
                    })?;
                if grant.roots != self.grant.roots {
                    return Err(WireString::from_str_in(
                        "the retired helper's root was not returned",
                        alloc,
                    ));
                }
                self.reclaimed = true;
            }
        } else if result.is_ok() {
            return Err(WireString::from_str_in(
                "a stale retirement was accepted for another assignment",
                alloc,
            ));
        }
        self.oracle()
    }

    fn acknowledge(&mut self) -> Result<(), WireString> {
        let alloc = self.alloc;
        if !self.stopped {
            return Ok(());
        }
        if matches!(self.plant, Plant::ReleaseWithoutStop)
            && let Some(assignment) = self.jobs.assignments.get_mut(self.helper.as_str())
        {
            // Plant removal of the coordinator's pending-stop check before acknowledgment.
            if assignment.token == self.original {
                assignment.stopping = true;
            }
        }
        let mut token = self.original;
        if matches!(self.plant, Plant::AcceptStaleAcknowledgment)
            && let Some(assignment) = self.jobs.assignments.get(self.helper.as_str())
        {
            // Plant an ACK that releases whichever assignment is currently stopping.
            token = assignment.token;
        }
        let accepted = self.jobs.killed(self.now, self.helper.as_str(), token, true);
        let expected = self.retired
            && !self.acknowledged
            && self.helper_reservation == Some(self.original);
        if accepted != expected {
            return Err(WireString::from_str_in(
                "acknowledgment was accepted without its exact pending stop",
                alloc,
            ));
        }
        if accepted {
            self.helper_reservation = None;
            self.acknowledged = true;
        }
        self.oracle()
    }

    fn late_result(&mut self) -> Result<(), WireString> {
        let alloc = self.alloc;
        let result = BatchResult {
            lease: self.grant.lease.clone(),
            seq: 1,
            items: vec_in![
                alloc;
                ItemResult {
                    name: WorkName([21; 32]),
                    outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([22; 32]) }),
                    found: Set::empty_in(alloc),
                    unnamed: Set::empty_in(alloc),
                }
            ],
            packs: Vec::new_in(alloc),
            returned_roots: Set::empty_in(alloc),
            returned_names: Set::empty_in(alloc),
        };
        if self.jobs.result(self.now, self.helper.as_str(), &result, &mut Vec::new_in(alloc)).is_ok()
        {
            return Err(WireString::from_str_in(
                "a retired lease's late result was accepted",
                alloc,
            ));
        }
        self.oracle()
    }

    fn promote(&mut self, voluntary: bool, observed: bool) {
        let alloc = self.alloc;
        if voluntary {
            self.jobs
                .retire(
                    self.now,
                    self.lead.as_str(),
                    self.original,
                    LEAD,
                    &mut Vec::new_in(alloc),
                )
                .unwrap();
        } else {
            self.now = 301;
            self.jobs
                .worker_events(
                    300,
                    self.helper.as_str(),
                    &WorkersEventsRequest {
                        execution: self.first.execution,
                        attempt: self.original.attempt,
                        events: Vec::new_in(alloc),
                    },
                    &mut Vec::new_in(alloc),
                )
                .unwrap();
            self.jobs.tick(self.now, &mut Vec::new_in(alloc));
        }
        let current = self.jobs.execution(self.first.execution).unwrap().assignment();
        assert_eq!(current.attempt, Attempt(2));
        self.helper_reservation = Some(current);
        if matches!(self.plant, Plant::ForgetPromotedOwner) {
            self.jobs.assignments.get_mut(self.helper.as_str()).unwrap().claimant = None;
        }
        if observed {
            let (lead, attempt) = self
                .jobs
                .claim(self.now, self.first.execution, HELPER, &mut Vec::new_in(alloc))
                .unwrap()
                .unwrap();
            assert_eq!(lead, self.helper.as_str());
            assert_eq!(attempt, current.attempt);
            join(&mut self.jobs, self.now, self.helper.as_str(), current, alloc);
        }
    }

    fn reuse(&mut self) -> Result<(), WireString> {
        let alloc = self.alloc;
        let next =
            self.jobs.submit(self.now, &request(2), need(), &mut Vec::new_in(alloc)).unwrap();
        let claimed = self
            .jobs
            .claim(self.now, next.execution, STRANGER, &mut Vec::new_in(alloc))
            .map_err(|why| format_in!(alloc, "next unit claim: {why:?}"))?;
        let (executor, attempt) = claimed.ok_or_else(|| {
            WireString::from_str_in("the retired helper's capacity never returned", alloc)
        })?;
        if executor != self.helper.as_str() {
            return Err(WireString::from_str_in(
                "the next unit did not use the retired helper's released slot",
                alloc,
            ));
        }
        self.helper_reservation = Some(AssignmentToken { execution: next.execution, attempt });
        self.retire()?;
        self.acknowledge()?;
        self.late_result()?;
        // Even after the replacement receives its own stop, this old ACK cannot
        // release it: its local compiler has not acknowledged that new token.
        self.jobs
            .unsubscribe(self.now, next.execution, next.subscriber, &mut Vec::new_in(alloc))
            .unwrap();
        self.acknowledge()
    }
}

fn scenario(ops: &[u8], mode: u8, plant: Plant, alloc: WireAlloc) -> Result<(), WireString> {
    let mut sim = Sim::new(plant, alloc);
    if mode != 0 {
        sim.promote(mode == 2, true);
    }
    for op in ops {
        sim.now += 1;
        match op % 8 {
            0 => sim.leave(),
            1 => sim.stopped = true,
            2 => sim.retire()?,
            3 => sim.acknowledge()?,
            4 if sim.retired || mode != 0 => sim.late_result()?,
            5 => {
                let current = sim.jobs.execution(sim.first.execution).unwrap().assignment();
                let before = sim.jobs.assignments.clone();
                let claimant = if matches!(plant, Plant::IgnoreOwner) { HELPER } else { STRANGER };
                let unowned = sim.jobs.retire(
                    sim.now,
                    sim.helper.as_str(),
                    current,
                    claimant,
                    &mut Vec::new_in(alloc),
                );
                if unowned.is_ok() || sim.jobs.assignments != before {
                    return Err(WireString::from_str_in(
                        "an unowned retirement changed assignment ownership",
                        alloc,
                    ));
                }
            }
            6 => {
                let before = sim.jobs.assignments.clone();
                let wrong = AssignmentToken {
                    attempt: Attempt(sim.original.attempt.0 + 10),
                    ..sim.original
                };
                if sim.jobs.killed(sim.now, sim.helper.as_str(), wrong, true)
                    || sim.jobs.assignments != before
                {
                    return Err(WireString::from_str_in(
                        "a stale acknowledgment changed assignment ownership",
                        alloc,
                    ));
                }
            }
            _ => {}
        }
        sim.oracle()?;
    }
    sim.leave();
    sim.stopped = true;
    sim.retire()?;
    sim.acknowledge()?;
    sim.late_result()?;
    if mode == 0 {
        sim.reuse()?;
    }
    sim.oracle()
}

/// The invariant: retiring an owned assignment fences its work without ending another
/// subscriber or releasing capacity before its exact stop acknowledgment; stale retirement,
/// unowned retirement and stale acknowledgments cannot affect a replacement assignment.
#[test]
fn retirement_preserves_survivors_and_exact_assignment_ownership() {
    let alloc = heap();
    for seed in 0..128u64 {
        let mut rng = Rng::new(seed ^ 0xAE71_AE00);
        let mode = u8::try_from(rng.below(3)).unwrap();
        let len = rng.range_usize(0, 64);
        let mut ops = Vec::with_capacity_in(len, alloc);
        for _ in 0..len {
            ops.push(u8::try_from(rng.below(8)).unwrap());
        }
        scenario(&ops, mode, Plant::None, alloc).unwrap();
    }
}

fn cancel_before_observing_promotion(
    after_lookup: bool,
    voluntary: bool,
    stopped_after: u8,
    plant: Plant,
    alloc: WireAlloc,
) -> Result<(), WireString> {
    let mut sim = Sim::new(plant, alloc);
    sim.leave();
    let observed =
        sim.jobs.owned_assignment(sim.helper.as_str(), sim.first.execution, HELPER).unwrap();
    sim.promote(voluntary, false);
    sim.now += u64::from(stopped_after);
    sim.oracle()?;
    let current = sim.helper_reservation.unwrap();
    if sim.jobs.owned_assignment(sim.helper.as_str(), sim.first.execution, HELPER) != Ok(current) {
        return Err(WireString::from_str_in(
            "promotion forgot the continuing worker's owner",
            alloc,
        ));
    }
    let before = sim.jobs.assignments.clone();
    if sim.jobs.owned_assignment(sim.helper.as_str(), sim.first.execution, STRANGER).is_ok()
        || sim
            .jobs
            .claim(sim.now, sim.first.execution, STRANGER, &mut Vec::new_in(alloc))
            .unwrap()
            .is_some()
        || sim.jobs.assignments != before
    {
        return Err(WireString::from_str_in(
            "another owner acquired the continuing worker's assignment",
            alloc,
        ));
    }
    if sim
        .jobs
        .retire(sim.now, sim.helper.as_str(), observed, HELPER, &mut Vec::new_in(alloc))
        .is_ok()
        || sim.jobs.killed(sim.now, sim.helper.as_str(), observed, true)
        || sim.jobs.assignments != before
    {
        return Err(WireString::from_str_in(
            "an old token changed the unobserved promoted assignment",
            alloc,
        ));
    }

    // The compiler has now stopped, but the agent never claimed the promoted attempt.
    // A read can also race the promotion; only an observed current token may stop it.
    let mut acknowledged = false;
    for retry in 0..MAX_ATTEMPTS {
        let token = if (after_lookup && retry == 0) || matches!(plant, Plant::KeepObservedAttempt) {
            observed
        } else {
            sim.jobs
                .owned_assignment(sim.helper.as_str(), sim.first.execution, HELPER)
                .map_err(|why| format_in!(alloc, "owned cleanup lookup: {why:?}"))?
        };
        match sim.jobs.retire(sim.now, sim.helper.as_str(), token, HELPER, &mut Vec::new_in(alloc))
        {
            Ok(()) => {
                if sim.jobs.admission_view().executor(sim.helper.as_str()).unwrap().busy
                    != Some((current.execution, current.attempt))
                {
                    return Err(WireString::from_str_in(
                        "retirement freed capacity before the stopped worker's ACK",
                        alloc,
                    ));
                }
                acknowledged = sim.jobs.killed(sim.now, sim.helper.as_str(), token, true);
                break;
            }
            Err(Refusal::AttemptMoved) => {}
            Err(why) => return Err(format_in!(alloc, "owned cleanup retirement: {why:?}")),
        }
    }
    if !acknowledged
        || sim.jobs.admission_view().executor(sim.helper.as_str()).unwrap().busy.is_some()
    {
        return Err(WireString::from_str_in(
            "cancellation stranded the unobserved promoted reservation",
            alloc,
        ));
    }
    // This was the second lead loss. Preserve that terminal reason rather than inventing
    // a third attempt or a successful compile while returning the stopped slot's capacity.
    if sim.jobs.execution(sim.first.execution).unwrap().state() != ExecState::Ended(Outcome::Lost) {
        return Err(WireString::from_str_in(
            "cancellation weakened the existing lead-reassignment bound",
            alloc,
        ));
    }
    let next = sim.jobs.submit(sim.now, &request(2), need(), &mut Vec::new_in(alloc)).unwrap();
    let (executor, attempt) = sim
        .jobs
        .claim(sim.now, next.execution, STRANGER, &mut Vec::new_in(alloc))
        .unwrap()
        .ok_or_else(|| {
            WireString::from_str_in(
                "the stopped promoted slot was not returned to the next unit",
                alloc,
            )
        })?;
    if executor != sim.helper.as_str() {
        return Err(WireString::from_str_in(
            "the old lead's unacknowledged reservation was reused",
            alloc,
        ));
    }
    let before = sim.jobs.assignments.clone();
    if sim.jobs.owned_assignment(sim.helper.as_str(), sim.first.execution, HELPER).is_ok()
        || sim
            .jobs
            .retire(sim.now, sim.helper.as_str(), current, HELPER, &mut Vec::new_in(alloc))
            .is_ok()
        || sim.jobs.killed(sim.now, sim.helper.as_str(), current, true)
        || sim.jobs.assignments != before
    {
        return Err(WireString::from_str_in(
            "cleanup followed a reused slot into another execution",
            alloc,
        ));
    }
    join(
        &mut sim.jobs,
        sim.now,
        sim.helper.as_str(),
        AssignmentToken { execution: next.execution, attempt },
        alloc,
    );
    sim.jobs.admission_view().check().map_err(|why| format_in!(alloc, "capacity: {why}"))
}

/// The invariant: cancellation before a worker observes its promotion releases only that
/// continuing owner's stopped slot, using a fresh exact token within the existing attempt
/// bound; stale requests and other owners cannot take it or follow it into a new execution.
#[test]
fn cancellation_cleans_a_promotion_the_worker_has_not_observed() {
    let alloc = heap();
    for seed in 0..128u64 {
        let mut rng = Rng::new(seed ^ 0xCA0C_E100);
        let after_lookup = rng.below(2) != 0;
        let voluntary = rng.below(2) != 0;
        let stopped_after = u8::try_from(rng.below(101)).unwrap();
        cancel_before_observing_promotion(after_lookup, voluntary, stopped_after, Plant::None, alloc)
            .unwrap();
    }
}

fn stale_claimed_helper(
    unclaimed_helper: bool,
    forget_owner: bool,
    alloc: WireAlloc,
) -> Result<(), WireString> {
    let mut sim = Sim::new(Plant::None, alloc);
    sim.jobs.admission.add_machine("spare", [1; 32], need());
    let spare = WireString::from_str_in(
        &sim.jobs.admission.add_builder(0, "spare", need(), alloc).unwrap(),
        alloc,
    );
    if unclaimed_helper {
        // Offer the spare as an unclaimed helper; otherwise it remains in the idle pool.
        sim.jobs.tick(0, &mut Vec::new_in(alloc));
    }
    if forget_owner {
        // Plant a stale claimed worker misclassified as available to any new actor.
        sim.jobs.assignments.get_mut(sim.helper.as_str()).unwrap().claimant = None;
    }
    sim.jobs.tick(301, &mut Vec::new_in(alloc));
    let execution = sim.jobs.execution(sim.first.execution).unwrap();
    if execution.lead() != Some(spare.as_str()) || execution.attempt() != Attempt(2) {
        return Err(WireString::from_str_in(
            "a stale claimed helper blocked an available replacement",
            alloc,
        ));
    }
    // The excluded helper must retain its old reservation and exact pending stop.
    // Neither selection of the new lead nor its new attempt proves the old process stopped.
    if sim.jobs.owned_assignment(sim.helper.as_str(), sim.first.execution, HELPER)
        != Ok(sim.original)
        || sim.jobs.admission_view().executor(sim.helper.as_str()).unwrap().busy
            != Some((sim.original.execution, sim.original.attempt))
        || !sim.jobs.assignments.get(sim.helper.as_str()).unwrap().stopping
    {
        return Err(WireString::from_str_in(
            "replacement stole or released the stale helper's reservation",
            alloc,
        ));
    }
    let (claimed, attempt) = sim
        .jobs
        .claim(301, sim.first.execution, STRANGER, &mut Vec::new_in(alloc))
        .unwrap()
        .ok_or_else(|| {
            WireString::from_str_in("the available replacement could not be claimed", alloc)
        })?;
    if claimed != spare.as_str() {
        return Err(WireString::from_str_in(
            "the replacement acquired the stale helper's slot",
            alloc,
        ));
    }
    join(
        &mut sim.jobs,
        301,
        spare.as_str(),
        AssignmentToken { execution: sim.first.execution, attempt },
        alloc,
    );
    sim.jobs.admission_view().check().map_err(|why| format_in!(alloc, "capacity: {why}"))
}

/// The invariant: a stale claimed helper cannot block an available unclaimed or idle lead
/// replacement, and the replacement cannot steal or release that old worker's reservation.
#[test]
fn stale_claimed_helpers_do_not_block_available_replacements() {
    let alloc = heap();
    for unclaimed_helper in [false, true] {
        stale_claimed_helper(unclaimed_helper, false, alloc).unwrap();
    }
}

#[test]
fn the_oracle_catches_leaving_without_retirement() {
    let alloc = heap();
    assert!(
        scenario(&[], 0, Plant::SkipRetirement, alloc)
            .unwrap_err()
            .contains("remains an execution member")
    );
}

#[test]
fn the_oracle_catches_release_without_a_pending_stop() {
    let alloc = heap();
    assert!(
        scenario(&[1, 3], 0, Plant::ReleaseWithoutStop, alloc)
            .unwrap_err()
            .contains("without its exact pending stop")
    );
}

#[test]
fn the_oracle_catches_a_stale_retirement_of_the_replacement_lead() {
    let alloc = heap();
    assert!(
        scenario(&[2], 1, Plant::AcceptStaleRetirement, alloc)
            .unwrap_err()
            .contains("stale retirement was accepted")
    );
}

#[test]
fn the_oracle_catches_a_stale_acknowledgment_of_a_reused_slot() {
    let alloc = heap();
    assert!(
        scenario(&[], 0, Plant::AcceptStaleAcknowledgment, alloc)
            .unwrap_err()
            .contains("without its exact pending stop")
    );
}

#[test]
fn the_oracle_catches_an_unowned_retirement() {
    let alloc = heap();
    assert!(
        scenario(&[5], 0, Plant::IgnoreOwner, alloc)
            .unwrap_err()
            .contains("an unowned retirement changed assignment ownership")
    );
}

#[test]
fn the_oracle_catches_retired_work_kept_under_its_old_lease() {
    let alloc = heap();
    assert!(
        scenario(&[], 0, Plant::KeepRetiredWork, alloc)
            .unwrap_err()
            .contains("root was not returned")
    );
}

#[test]
fn the_oracle_catches_promotion_erasing_the_continuing_owner() {
    let alloc = heap();
    assert!(
        cancel_before_observing_promotion(false, true, 0, Plant::ForgetPromotedOwner, alloc)
            .unwrap_err()
            .contains("promotion forgot the continuing worker's owner")
    );
}

#[test]
fn the_oracle_catches_cleanup_using_only_the_last_observed_attempt() {
    let alloc = heap();
    assert!(
        cancel_before_observing_promotion(true, false, 1, Plant::KeepObservedAttempt, alloc)
            .unwrap_err()
            .contains("cancellation stranded the unobserved promoted reservation")
    );
}

#[test]
fn the_oracle_catches_a_stale_claimed_helper_misclassified_as_unclaimed() {
    let alloc = heap();
    assert!(
        stale_claimed_helper(false, true, alloc)
            .unwrap_err()
            .contains("a stale claimed helper blocked an available replacement")
    );
}
