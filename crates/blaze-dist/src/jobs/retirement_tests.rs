//! Real registry transitions on a virtual clock with delayed, duplicated and stale actor calls.

use hegel::generators as gs;

use super::*;
use crate::admission::AdmissionConfig;
use blaze_wire::{
    EntryKey, EntryRef, Grant, ItemOutcome, ItemResult, JobAnswer, RootSet, Set, WorkName,
};

const LEAD: Digest = [11; 32];
const HELPER: Digest = [12; 32];
const STRANGER: Digest = [13; 32];

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
    jobs: Jobs,
    now: Millis,
    first: JobAnswer,
    leaving: JobAnswer,
    lead: String,
    helper: String,
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

fn join(jobs: &mut Jobs, now: Millis, executor: &str, token: AssignmentToken) {
    jobs.join(
        now,
        executor,
        &WorkJoinRequest {
            execution: token.execution,
            attempt: token.attempt,
            roots: roots(),
            rejected: Set::new(Vec::new()),
        },
        &mut Vec::new(),
    )
    .unwrap();
}

impl Sim {
    fn new(plant: Plant) -> Sim {
        let mut admission = Admission::new(AdmissionConfig::default());
        admission.add_machine("machine", [1; 32], Reservation { memory: 2, cpus: 2, disk: 2 });
        for _ in 0..2 {
            admission.add_builder(0, "machine", need()).unwrap();
        }
        let mut jobs = Jobs::new(
            admission,
            ExecutionConfig {
                heartbeat_ms: 100,
                work: WorkConfig { grant_roots: 1, ..WorkConfig::default() },
                ..ExecutionConfig::default()
            },
        );
        let first = jobs.submit(0, &request(1), need(), &mut Vec::new()).unwrap();
        let (lead, attempt) =
            jobs.claim(0, first.execution, LEAD, &mut Vec::new()).unwrap().unwrap();
        let leaving = jobs.submit(0, &request(1), need(), &mut Vec::new()).unwrap();
        let (helper, helped) =
            jobs.claim(0, first.execution, HELPER, &mut Vec::new()).unwrap().unwrap();
        assert_ne!(lead, helper);
        assert_eq!(attempt, helped);
        let original = AssignmentToken { execution: first.execution, attempt };
        join(&mut jobs, 0, &lead, original);
        join(&mut jobs, 0, &helper, original);
        let grant = jobs
            .lease(
                0,
                &helper,
                &WorkLeaseRequest { execution: first.execution, attempt },
                &mut Vec::new(),
            )
            .unwrap()
            .grant
            .unwrap();
        Sim {
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

    fn oracle(&self) -> Result<(), String> {
        let execution = self.jobs.execution(self.first.execution).unwrap();
        if execution.state() != ExecState::Running
            || self.jobs.terminal(self.first.execution, self.first.subscriber).is_some()
        {
            return Err("a surviving subscriber was ended by a stale/helper retirement".into());
        }
        let actual = self.jobs.admission_view().executor(&self.helper).unwrap().busy;
        let expected = self.helper_reservation.map(|token| (token.execution, token.attempt));
        if actual != expected {
            return Err(
                "helper capacity changed without its exact pending-stop acknowledgment".into(),
            );
        }
        for executor in execution.executors() {
            if self.jobs.assignments.get(executor).is_none_or(|assignment| {
                assignment.token != execution.assignment() || assignment.stopping
            }) {
                return Err("a live execution member has no exclusive running assignment".into());
            }
        }
        if self.retired && execution.executors().any(|who| who == self.helper) {
            return Err("the retired helper remains an execution member".into());
        }
        self.jobs.admission_view().check().map_err(|why| format!("capacity: {why}"))
    }

    fn leave(&mut self) {
        self.jobs
            .unsubscribe(
                self.now,
                self.leaving.execution,
                self.leaving.subscriber,
                &mut Vec::new(),
            )
            .unwrap();
    }

    fn retire(&mut self) -> Result<(), String> {
        let mut token = self.original;
        if matches!(self.plant, Plant::AcceptStaleRetirement)
            && let Some(assignment) = self.jobs.assignments.get(&self.helper)
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
            self.jobs.retire(self.now, &self.helper, token, HELPER, &mut Vec::new())
        };
        if let Some(work) = kept {
            self.jobs.executions.get_mut(&self.first.execution).unwrap().work = work;
        }
        if self.helper_reservation == Some(self.original) && !self.acknowledged {
            result.map_err(|why| format!("owned retirement refused: {why:?}"))?;
            self.retired = true;
            self.oracle()?;
            if !self.reclaimed {
                let grant = self
                    .jobs
                    .lease(
                        self.now,
                        &self.lead,
                        &WorkLeaseRequest {
                            execution: self.first.execution,
                            attempt: self.original.attempt,
                        },
                        &mut Vec::new(),
                    )
                    .map_err(|why| format!("surviving lead lease: {why:?}"))?
                    .grant
                    .ok_or("the retired helper's root was not returned")?;
                if grant.roots != self.grant.roots {
                    return Err("the retired helper's root was not returned".into());
                }
                self.reclaimed = true;
            }
        } else if result.is_ok() {
            return Err("a stale retirement was accepted for another assignment".into());
        }
        self.oracle()
    }

    fn acknowledge(&mut self) -> Result<(), String> {
        if !self.stopped {
            return Ok(());
        }
        if matches!(self.plant, Plant::ReleaseWithoutStop)
            && let Some(assignment) = self.jobs.assignments.get_mut(&self.helper)
        {
            // Plant removal of the coordinator's pending-stop check before acknowledgment.
            if assignment.token == self.original {
                assignment.stopping = true;
            }
        }
        let mut token = self.original;
        if matches!(self.plant, Plant::AcceptStaleAcknowledgment)
            && let Some(assignment) = self.jobs.assignments.get(&self.helper)
        {
            // Plant an ACK that releases whichever assignment is currently stopping.
            token = assignment.token;
        }
        let accepted = self.jobs.killed(self.now, &self.helper, token, true);
        let expected = self.retired
            && !self.acknowledged
            && self.helper_reservation == Some(self.original);
        if accepted != expected {
            return Err("acknowledgment was accepted without its exact pending stop".into());
        }
        if accepted {
            self.helper_reservation = None;
            self.acknowledged = true;
        }
        self.oracle()
    }

    fn late_result(&mut self) -> Result<(), String> {
        let result = BatchResult {
            lease: self.grant.lease.clone(),
            seq: 1,
            items: vec![ItemResult {
                name: WorkName([21; 32]),
                outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([22; 32]) }),
                found: Set::new(Vec::new()),
                unnamed: Set::new(Vec::new()),
            }],
            packs: Vec::new(),
            returned_roots: Set::new(Vec::new()),
            returned_names: Set::new(Vec::new()),
        };
        if self.jobs.result(self.now, &self.helper, &result, &mut Vec::new()).is_ok() {
            return Err("a retired lease's late result was accepted".into());
        }
        self.oracle()
    }

    fn promote(&mut self, voluntary: bool, observed: bool) {
        if voluntary {
            self.jobs
                .retire(self.now, &self.lead, self.original, LEAD, &mut Vec::new())
                .unwrap();
        } else {
            self.now = 301;
            self.jobs
                .worker_events(
                    300,
                    &self.helper,
                    &WorkersEventsRequest {
                        execution: self.first.execution,
                        attempt: self.original.attempt,
                        events: Vec::new(),
                    },
                    &mut Vec::new(),
                )
                .unwrap();
            self.jobs.tick(self.now, &mut Vec::new());
        }
        let current = self.jobs.execution(self.first.execution).unwrap().assignment();
        assert_eq!(current.attempt, Attempt(2));
        self.helper_reservation = Some(current);
        if matches!(self.plant, Plant::ForgetPromotedOwner) {
            self.jobs.assignments.get_mut(&self.helper).unwrap().claimant = None;
        }
        if observed {
            let (lead, attempt) = self
                .jobs
                .claim(self.now, self.first.execution, HELPER, &mut Vec::new())
                .unwrap()
                .unwrap();
            assert_eq!(lead, self.helper);
            assert_eq!(attempt, current.attempt);
            join(&mut self.jobs, self.now, &self.helper, current);
        }
    }

    fn reuse(&mut self) -> Result<(), String> {
        let next = self.jobs.submit(self.now, &request(2), need(), &mut Vec::new()).unwrap();
        let claimed = self
            .jobs
            .claim(self.now, next.execution, STRANGER, &mut Vec::new())
            .map_err(|why| format!("next unit claim: {why:?}"))?;
        let (executor, attempt) = claimed.ok_or("the retired helper's capacity never returned")?;
        if executor != self.helper {
            return Err("the next unit did not use the retired helper's released slot".into());
        }
        self.helper_reservation = Some(AssignmentToken { execution: next.execution, attempt });
        self.retire()?;
        self.acknowledge()?;
        self.late_result()?;
        // Even after the replacement receives its own stop, this old ACK cannot
        // release it: its local compiler has not acknowledged that new token.
        self.jobs
            .unsubscribe(self.now, next.execution, next.subscriber, &mut Vec::new())
            .unwrap();
        self.acknowledge()
    }
}

fn scenario(ops: &[u8], mode: u8, plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new(plant);
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
                let unowned =
                    sim.jobs.retire(sim.now, &sim.helper, current, claimant, &mut Vec::new());
                if unowned.is_ok() || sim.jobs.assignments != before {
                    return Err("an unowned retirement changed assignment ownership".into());
                }
            }
            6 => {
                let before = sim.jobs.assignments.clone();
                let wrong = AssignmentToken {
                    attempt: Attempt(sim.original.attempt.0 + 10),
                    ..sim.original
                };
                if sim.jobs.killed(sim.now, &sim.helper, wrong, true)
                    || sim.jobs.assignments != before
                {
                    return Err("a stale acknowledgment changed assignment ownership".into());
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
#[hegel::test(test_cases = 128)]
fn retirement_preserves_survivors_and_exact_assignment_ownership(tc: hegel::TestCase) {
    let mode = tc.draw(gs::integers::<u8>().max_value(2));
    let ops = tc.draw(gs::vecs(gs::integers::<u8>().max_value(7)).max_size(64));
    scenario(&ops, mode, Plant::None).unwrap();
}

fn cancel_before_observing_promotion(
    after_lookup: bool,
    voluntary: bool,
    stopped_after: u8,
    plant: Plant,
) -> Result<(), String> {
    let mut sim = Sim::new(plant);
    sim.leave();
    let observed = sim.jobs.owned_assignment(&sim.helper, sim.first.execution, HELPER).unwrap();
    sim.promote(voluntary, false);
    sim.now += u64::from(stopped_after);
    sim.oracle()?;
    let current = sim.helper_reservation.unwrap();
    if sim.jobs.owned_assignment(&sim.helper, sim.first.execution, HELPER) != Ok(current) {
        return Err("promotion forgot the continuing worker's owner".into());
    }
    let before = sim.jobs.assignments.clone();
    if sim.jobs.owned_assignment(&sim.helper, sim.first.execution, STRANGER).is_ok()
        || sim.jobs.claim(sim.now, sim.first.execution, STRANGER, &mut Vec::new()).unwrap().is_some()
        || sim.jobs.assignments != before
    {
        return Err("another owner acquired the continuing worker's assignment".into());
    }
    if sim.jobs.retire(sim.now, &sim.helper, observed, HELPER, &mut Vec::new()).is_ok()
        || sim.jobs.killed(sim.now, &sim.helper, observed, true)
        || sim.jobs.assignments != before
    {
        return Err("an old token changed the unobserved promoted assignment".into());
    }

    // The compiler has now stopped, but the agent never claimed the promoted attempt.
    // A read can also race the promotion; only an observed current token may stop it.
    let mut acknowledged = false;
    for retry in 0..MAX_ATTEMPTS {
        let token = if (after_lookup && retry == 0) || matches!(plant, Plant::KeepObservedAttempt) {
            observed
        } else {
            sim.jobs
                .owned_assignment(&sim.helper, sim.first.execution, HELPER)
                .map_err(|why| format!("owned cleanup lookup: {why:?}"))?
        };
        match sim.jobs.retire(sim.now, &sim.helper, token, HELPER, &mut Vec::new()) {
            Ok(()) => {
                if sim.jobs.admission_view().executor(&sim.helper).unwrap().busy
                    != Some((current.execution, current.attempt))
                {
                    return Err("retirement freed capacity before the stopped worker's ACK".into());
                }
                acknowledged = sim.jobs.killed(sim.now, &sim.helper, token, true);
                break;
            }
            Err(Refusal::AttemptMoved) => {}
            Err(why) => return Err(format!("owned cleanup retirement: {why:?}")),
        }
    }
    if !acknowledged || sim.jobs.admission_view().executor(&sim.helper).unwrap().busy.is_some() {
        return Err("cancellation stranded the unobserved promoted reservation".into());
    }
    // This was the second lead loss. Preserve that terminal reason rather than inventing
    // a third attempt or a successful compile while returning the stopped slot's capacity.
    if sim.jobs.execution(sim.first.execution).unwrap().state() != ExecState::Ended(Outcome::Lost) {
        return Err("cancellation weakened the existing lead-reassignment bound".into());
    }
    let next = sim.jobs.submit(sim.now, &request(2), need(), &mut Vec::new()).unwrap();
    let (executor, attempt) = sim
        .jobs
        .claim(sim.now, next.execution, STRANGER, &mut Vec::new())
        .unwrap()
        .ok_or("the stopped promoted slot was not returned to the next unit")?;
    if executor != sim.helper {
        return Err("the old lead's unacknowledged reservation was reused".into());
    }
    let before = sim.jobs.assignments.clone();
    if sim.jobs.owned_assignment(&sim.helper, sim.first.execution, HELPER).is_ok()
        || sim.jobs.retire(sim.now, &sim.helper, current, HELPER, &mut Vec::new()).is_ok()
        || sim.jobs.killed(sim.now, &sim.helper, current, true)
        || sim.jobs.assignments != before
    {
        return Err("cleanup followed a reused slot into another execution".into());
    }
    join(
        &mut sim.jobs,
        sim.now,
        &executor,
        AssignmentToken { execution: next.execution, attempt },
    );
    sim.jobs.admission_view().check().map_err(|why| format!("capacity: {why}"))
}

/// The invariant: cancellation before a worker observes its promotion releases only that
/// continuing owner's stopped slot, using a fresh exact token within the existing attempt
/// bound; stale requests and other owners cannot take it or follow it into a new execution.
#[hegel::test(test_cases = 128)]
fn cancellation_cleans_a_promotion_the_worker_has_not_observed(tc: hegel::TestCase) {
    let after_lookup = tc.draw(gs::booleans());
    let voluntary = tc.draw(gs::booleans());
    let stopped_after = tc.draw(gs::integers::<u8>().max_value(100));
    cancel_before_observing_promotion(after_lookup, voluntary, stopped_after, Plant::None).unwrap();
}

fn stale_claimed_helper(unclaimed_helper: bool, forget_owner: bool) -> Result<(), String> {
    let mut sim = Sim::new(Plant::None);
    sim.jobs.admission.add_machine("spare", [1; 32], need());
    let spare = sim.jobs.admission.add_builder(0, "spare", need()).unwrap();
    if unclaimed_helper {
        // Offer the spare as an unclaimed helper; otherwise it remains in the idle pool.
        sim.jobs.tick(0, &mut Vec::new());
    }
    if forget_owner {
        // Plant a stale claimed worker misclassified as available to any new actor.
        sim.jobs.assignments.get_mut(&sim.helper).unwrap().claimant = None;
    }
    sim.jobs.tick(301, &mut Vec::new());
    let execution = sim.jobs.execution(sim.first.execution).unwrap();
    if execution.lead() != Some(spare.as_str()) || execution.attempt() != Attempt(2) {
        return Err("a stale claimed helper blocked an available replacement".into());
    }
    // The excluded helper must retain its old reservation and exact pending stop.
    // Neither selection of the new lead nor its new attempt proves the old process stopped.
    if sim.jobs.owned_assignment(&sim.helper, sim.first.execution, HELPER) != Ok(sim.original)
        || sim.jobs.admission_view().executor(&sim.helper).unwrap().busy
            != Some((sim.original.execution, sim.original.attempt))
        || !sim.jobs.assignments[&sim.helper].stopping
    {
        return Err("replacement stole or released the stale helper's reservation".into());
    }
    let (claimed, attempt) = sim
        .jobs
        .claim(301, sim.first.execution, STRANGER, &mut Vec::new())
        .unwrap()
        .ok_or("the available replacement could not be claimed")?;
    if claimed != spare {
        return Err("the replacement acquired the stale helper's slot".into());
    }
    join(
        &mut sim.jobs,
        301,
        &spare,
        AssignmentToken { execution: sim.first.execution, attempt },
    );
    sim.jobs.admission_view().check().map_err(|why| format!("capacity: {why}"))
}

/// The invariant: a stale claimed helper cannot block an available unclaimed or idle lead
/// replacement, and the replacement cannot steal or release that old worker's reservation.
#[hegel::test(test_cases = 32)]
fn stale_claimed_helpers_do_not_block_available_replacements(tc: hegel::TestCase) {
    stale_claimed_helper(tc.draw(gs::booleans()), false).unwrap();
}

#[test]
fn the_oracle_catches_leaving_without_retirement() {
    assert!(
        scenario(&[], 0, Plant::SkipRetirement)
            .unwrap_err()
            .contains("remains an execution member")
    );
}

#[test]
fn the_oracle_catches_release_without_a_pending_stop() {
    assert!(
        scenario(&[1, 3], 0, Plant::ReleaseWithoutStop)
            .unwrap_err()
            .contains("without its exact pending stop")
    );
}

#[test]
fn the_oracle_catches_a_stale_retirement_of_the_replacement_lead() {
    assert!(
        scenario(&[2], 1, Plant::AcceptStaleRetirement)
            .unwrap_err()
            .contains("stale retirement was accepted")
    );
}

#[test]
fn the_oracle_catches_a_stale_acknowledgment_of_a_reused_slot() {
    assert!(
        scenario(&[], 0, Plant::AcceptStaleAcknowledgment)
            .unwrap_err()
            .contains("without its exact pending stop")
    );
}

#[test]
fn the_oracle_catches_an_unowned_retirement() {
    assert!(
        scenario(&[5], 0, Plant::IgnoreOwner)
            .unwrap_err()
            .contains("an unowned retirement changed assignment ownership")
    );
}

#[test]
fn the_oracle_catches_retired_work_kept_under_its_old_lease() {
    assert!(
        scenario(&[], 0, Plant::KeepRetiredWork)
            .unwrap_err()
            .contains("root was not returned")
    );
}

#[test]
fn the_oracle_catches_promotion_erasing_the_continuing_owner() {
    assert!(
        cancel_before_observing_promotion(false, true, 0, Plant::ForgetPromotedOwner)
            .unwrap_err()
            .contains("promotion forgot the continuing worker's owner")
    );
}

#[test]
fn the_oracle_catches_cleanup_using_only_the_last_observed_attempt() {
    assert!(
        cancel_before_observing_promotion(true, false, 1, Plant::KeepObservedAttempt)
            .unwrap_err()
            .contains("cancellation stranded the unobserved promoted reservation")
    );
}

#[test]
fn the_oracle_catches_a_stale_claimed_helper_misclassified_as_unclaimed() {
    assert!(
        stale_claimed_helper(false, true)
            .unwrap_err()
            .contains("a stale claimed helper blocked an available replacement")
    );
}
