//! Keyed admission and output retirement driven through the real Jobs core on one tape.
//! The catalog is a simulated durable effect; the execution is the only accepted-result store.

use core::fmt;

use blaze_own::{BTreeMap, Vec, vec_in};
use blaze_sim::{Broken, Config, Tape};
use blaze_wire::{RootSet, Set};

use super::*;
use crate::admission::AdmissionConfig;

#[derive(Debug)]
enum AdmissionBroken {
    BeforeResolution,
    StaleResolution,
    DuplicateTail,
    DeliveryNeedsSubscriber,
    TerminalChanged,
    AcceptedWork,
}

impl fmt::Display for AdmissionBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BeforeResolution => "actual work was admitted before owned absence resolution",
            Self::StaleResolution => "a stale lookup completion enabled current work",
            Self::DuplicateTail => "one result identity started another full tail after acceptance",
            Self::DeliveryNeedsSubscriber => "accepted output could not drain without a live subscriber",
            Self::TerminalChanged => "publication delivery changed an existing subscriber terminal",
            Self::AcceptedWork => "accepted output did not stop its old holder cleanly",
        })
    }
}

blaze_sim::plants! {
    enum AdmissionPlant breaks AdmissionBroken {
        None,
        BypassResolution => AdmissionBroken::BeforeResolution,
        ForgetUndelivered => AdmissionBroken::DuplicateTail,
        RequireLiveSubscriber => AdmissionBroken::DeliveryNeedsSubscriber,
        AcceptedAsProtocolFailure => AdmissionBroken::AcceptedWork,
    }
}

blaze_sim::points! {
    enum AdmissionPoint {
        UnresolvedWorkRefused,
        StaleLookupRefused,
        AcceptedWithoutSubscribers,
        TerminalDelivery,
        DurableRestart,
        EphemeralRestart,
    }
}

type Checked<T = ()> = Result<T, Broken<AdmissionBroken>>;

fn need() -> Reservation {
    Reservation { memory: 1, cpus: 1, disk: 1 }
}

fn registry(epoch: u8, alloc: WireAlloc) -> Jobs {
    let mut admission = Admission::for_pull_workers(AdmissionConfig::default(), alloc);
    admission.add_machine("machine", [1; 32], Reservation { memory: 2, cpus: 2, disk: 2 });
    let mut jobs = Jobs::new(
        admission,
        ExecutionConfig {
            heartbeat_ms: 10,
            completion: CompletionMode::PublishedWork,
            ..ExecutionConfig::default()
        },
        alloc,
    )
    .start_on_claim();
    jobs.seed_ids(&[epoch; 32]);
    jobs
}

struct Simulation {
    alloc: WireAlloc,
    jobs: Jobs,
    now: Millis,
    epoch: u8,
    request: JobRequest,
    packs: WireVec<PackRef>,
    out: WireVec<(ExecutionId, Effect)>,
    durable: bool,
    /// Full-tail invocations are counted by content, never reset by changing execution IDs.
    tails: BTreeMap<JobContent, u32, WireAlloc>,
    /// An unmirrored coordinator crash may recompute; durable output never adds permission.
    allowed_tails: u32,
    plant: AdmissionPlant,
}

impl Simulation {
    fn new(plant: AdmissionPlant, alloc: WireAlloc) -> Self {
        Self {
            alloc,
            jobs: registry(1, alloc),
            now: 0,
            epoch: 1,
            request: JobRequest {
                unit: [7; 32],
                platform: [1; 32],
                namespace: Namespace::Main,
                source: [8; 32],
                deadline_ms: 1_000,
                local: LocalPolicy::Never,
            },
            packs: vec_in![alloc; PackRef {
                pack: [7; 32], size: 31, keys: [9; 32], packed: None, format: 1,
            }],
            out: Vec::new_in(alloc),
            durable: false,
            tails: BTreeMap::new_in(alloc),
            allowed_tails: 1,
            plant,
        }
    }

    fn acknowledge_stops(&mut self) {
        for (_, effect) in self.out.drain(..) {
            if let Effect::Kill { executor, assignment } = effect {
                self.jobs.killed(self.now, executor.as_str(), assignment, true);
            }
        }
    }

    fn advance(&mut self, by: Millis) {
        self.now += by;
        self.jobs.tick(self.now, &mut self.out);
        self.acknowledge_stops();
    }

    fn claim(&mut self, execution: ExecutionId) -> (WireString, Attempt) {
        let (lead, attempt) = self.jobs
            .claim(self.now, execution, [self.epoch; 32], &mut self.out)
            .unwrap()
            .unwrap();
        (WireString::from_str_in(lead, self.alloc), attempt)
    }

    fn join(&mut self, execution: ExecutionId, lead: &str, attempt: Attempt) -> Result<(), Refusal> {
        self.jobs.join(
            self.now,
            lead,
            &WorkJoinRequest {
                execution,
                attempt,
                roots: RootSet {
                    spec: [1; 32], roots: [2; 32], count: 0, index: [3; 32], inputs: [4; 32],
                },
                rejected: Set::empty_in(self.alloc),
            },
            &mut self.out,
        ).map(|_| ())
    }

    fn consume(&mut self, tape: &Tape) -> Checked {
        self.request.deadline_ms = self.now + 1_000;
        let subscribers = self.jobs.next_subscriber;
        let executions = self.jobs.next_execution;
        match self.jobs.submit(self.now, &self.request, need(), &mut self.out).unwrap() {
            SubmitAnswer::Accepted(publication) => {
                assert_eq!(publication.packs, self.packs);
                assert_eq!(self.jobs.next_subscriber, subscribers);
                assert_eq!(self.jobs.next_execution, executions);
                self.check_accepted(publication.execution)?;
            }
            SubmitAnswer::Attached(answer) => {
                let (mut lead, mut attempt) = self.claim(answer.execution);
                if self.plant == AdmissionPlant::BypassResolution {
                    // Remove exactly the owned-lookup barrier, then exercise the actual join.
                    self.jobs.executions.get_mut(&answer.execution).unwrap().resolved =
                        Some(CompilePermit { execution: answer.execution, attempt });
                }
                if self.join(answer.execution, lead.as_str(), attempt).is_ok()
                    || self.jobs.committed_packs(answer.execution).unwrap().is_some()
                    || self.jobs.lease(
                        self.now,
                        lead.as_str(),
                        &WorkLeaseRequest { execution: answer.execution, attempt },
                        &mut self.out,
                    ).is_ok()
                {
                    return Err(Broken::Oracle(AdmissionBroken::BeforeResolution));
                }
                tape.reach(AdmissionPoint::UnresolvedWorkRefused);
                assert_eq!(self.jobs.execution(answer.execution).unwrap().executors().count(), 1);

                if tape.coin("cache lookup completes after lead replacement") {
                    let previous = attempt;
                    let old_lead = WireString::from_str_in(lead.as_str(), self.alloc);
                    self.advance(31);
                    (lead, attempt) = self.claim(answer.execution);
                    assert_ne!(attempt, previous);
                    let stale = self.jobs.resolve_publication(
                        self.now, answer.execution, old_lead.as_str(), previous, None, &mut self.out,
                    );
                    if stale.is_ok() {
                        return Err(Broken::Oracle(AdmissionBroken::StaleResolution));
                    }
                    tape.reach(AdmissionPoint::StaleLookupRefused);
                }
                assert!(self.jobs.resolve_publication(
                    self.now, answer.execution, "not the lead", attempt, None, &mut self.out,
                ).is_err());
                let resolved = self.jobs.resolve_publication(
                    self.now,
                    answer.execution,
                    lead.as_str(),
                    attempt,
                    self.durable.then_some(self.packs.as_slice()),
                    &mut self.out,
                ).unwrap();
                if self.durable {
                    assert!(matches!(resolved, AdmissionState::Accepted(ref p) if p.delivered));
                    self.check_accepted(answer.execution)?;
                } else {
                    assert!(matches!(resolved, AdmissionState::Compile(permit)
                        if permit.execution() == answer.execution && permit.attempt() == attempt));
                    self.join(answer.execution, lead.as_str(), attempt).unwrap();
                    assert!(self.jobs.committed_packs(answer.execution).unwrap().is_some());
                    // A subscriber arrives while the full tail is still running. It joins
                    // this flight and cannot independently acquire a lead or start a tail.
                    let joined = attached_for_test(self.jobs.submit(
                        self.now, &self.request, need(), &mut self.out,
                    ));
                    assert_eq!(joined.execution, answer.execution);
                    let count = self.tails.entry(answer.content).or_insert(0);
                    *count += 1;
                    if *count > self.allowed_tails {
                        return Err(Broken::Oracle(AdmissionBroken::DuplicateTail));
                    }
                    self.jobs.commit_publication(
                        self.now, answer.execution, lead.as_str(), attempt, &self.packs, &mut self.out,
                    ).unwrap();
                    if tape.coin("commit reply is lost") {
                        let events = self.jobs.execution(answer.execution).unwrap().events(0).len();
                        self.jobs.commit_publication(
                            self.now, answer.execution, lead.as_str(), attempt, &self.packs, &mut self.out,
                        ).unwrap();
                        assert_eq!(self.jobs.execution(answer.execution).unwrap().events(0).len(), events);
                    }
                    self.check_accepted(answer.execution)?;
                }
            }
        }
        self.acknowledge_stops();
        Ok(())
    }

    fn check_accepted(&mut self, id: ExecutionId) -> Checked {
        let e = self.jobs.execution(id).unwrap();
        assert_eq!(e.publication(), Some(self.packs.as_slice()));
        let attempt = e.attempt();
        let lead = WireString::from_str_in(e.lead().unwrap_or("stopped"), self.alloc);
        let live = matches!(e.state(), ExecState::Running);
        if matches!(e.admission_state(), AdmissionState::Compile(_))
            || self.jobs.committed_packs(id).unwrap().is_some()
        {
            return Err(Broken::Oracle(AdmissionBroken::AcceptedWork));
        }
        // The old holder may have lost a response or already be preparing its next
        // request. It must stop normally, including while publication is still pending;
        // Malformed would be treated as a protocol failure and could enable fallback.
        for _ in 0..2 {
            let hidden = if live && self.plant == AdmissionPlant::AcceptedAsProtocolFailure {
                // Bypass the accepted-result stop and reach the old no-permit refusal in
                // the real work-call path. Preserve the immutable record for the oracle.
                let e = self.jobs.executions.get_mut(&id).unwrap();
                Some((e.publication.take(), e.resolved.take()))
            } else {
                None
            };
            let joined = self.join(id, lead.as_str(), attempt);
            if let Some((publication, resolved)) = hidden {
                let e = self.jobs.executions.get_mut(&id).unwrap();
                e.publication = publication;
                e.resolved = resolved;
            }
            let leased = self.jobs.lease(
                self.now,
                lead.as_str(),
                &WorkLeaseRequest { execution: id, attempt },
                &mut self.out,
            );
            if joined != Err(Refusal::Ended) || leased != Err(Refusal::Ended) {
                return Err(Broken::Oracle(AdmissionBroken::AcceptedWork));
            }
        }
        Ok(())
    }

    fn cancel(&mut self) {
        let subscribers = Vec::from_iter_in(
            self.jobs.executions().flat_map(|e| e.attached().map(move |s| (e.id, s.id))),
            self.alloc,
        );
        for (execution, subscriber) in subscribers {
            self.jobs.unsubscribe(self.now, execution, subscriber, &mut self.out).unwrap();
        }
        self.acknowledge_stops();
    }

    fn expire(&mut self) {
        let deadline = self.jobs.executions().filter_map(Execution::deadline).max();
        if let Some(deadline) = deadline {
            self.advance(deadline + 1 - self.now);
        }
    }

    fn forget(&mut self) {
        self.acknowledge_stops();
        let ended = Vec::from_iter_in(
            self.jobs.executions().filter(|e| matches!(e.state(), ExecState::Ended(_))).map(|e| e.id),
            self.alloc,
        );
        for id in ended {
            let pending = self.jobs.execution(id).unwrap().publication_pending();
            if pending && self.plant == AdmissionPlant::ForgetUndelivered {
                // Plant retirement without the pending-output guard, through the real
                // forget transition and its content index removal.
                self.jobs.executions.get_mut(&id).unwrap().publication_delivered = true;
            }
            let forgotten = self.jobs.forget_terminal(id);
            if pending && !forgotten && self.plant == AdmissionPlant::ForgetUndelivered {
                self.jobs.executions.get_mut(&id).unwrap().publication_delivered = false;
            }
        }
    }

    fn deliver(&mut self, tape: &Tape) -> Checked {
        if !self.durable {
            return Ok(());
        }
        let ids = Vec::from_iter_in(
            self.jobs.executions().filter(|e| e.publication().is_some()).map(|e| e.id),
            self.alloc,
        );
        for id in ids {
            let execution = self.jobs.execution(id).unwrap();
            let before = execution.state();
            let terminals = Vec::from_iter_in(execution.terminals.values().cloned(), self.alloc);
            let needs_subscriber = self.plant == AdmissionPlant::RequireLiveSubscriber
                && execution.attached().next().is_none();
            let delivered = if needs_subscriber {
                // The previous delivery guard rejected every terminal execution.
                Err(Refusal::Ended)
            } else {
                self.jobs.deliver_publication(self.now, id, &self.packs, &mut self.out)
            };
            if delivered.is_err() {
                return Err(Broken::Oracle(AdmissionBroken::DeliveryNeedsSubscriber));
            }
            let after = self.jobs.execution(id).unwrap();
            if matches!(before, ExecState::Ended(_)) {
                tape.reach(AdmissionPoint::TerminalDelivery);
                if after.state() != before {
                    return Err(Broken::Oracle(AdmissionBroken::TerminalChanged));
                }
            }
            if terminals.iter().any(|terminal| after.terminal(terminal.subscriber) != Some(terminal)) {
                return Err(Broken::Oracle(AdmissionBroken::TerminalChanged));
            }
            if tape.coin("delivery reply is lost") {
                self.jobs.deliver_publication(self.now, id, &self.packs, &mut self.out).unwrap();
            }
        }
        self.acknowledge_stops();
        Ok(())
    }

    fn restart(&mut self, tape: &Tape) {
        self.epoch += 1;
        if self.durable {
            tape.reach(AdmissionPoint::DurableRestart);
        } else {
            tape.reach(AdmissionPoint::EphemeralRestart);
            self.allowed_tails += 1;
        }
        self.jobs = registry(self.epoch, self.alloc);
        self.out.clear();
    }
}

fn admission(tape: &Tape, plant: AdmissionPlant) -> Checked {
    let alloc = blaze_own::testing::heap().shared();
    let mut sim = Simulation::new(plant, alloc);
    sim.consume(tape)?;
    // Lose every original subscriber before the mirror, then expire terminal retention.
    // A later requester must still discover the accepted record without attaching.
    if tape.coin("last subscriber deadline precedes mirror") {
        sim.expire();
    } else {
        sim.cancel();
    }
    sim.advance(tape.int_in(1..=100, "terminal retention expires"));
    sim.forget();
    sim.consume(tape)?;
    tape.reach(AdmissionPoint::AcceptedWithoutSubscribers);

    let steps = tape.int_in(1..=24, "publication schedule length");
    for _ in 0..steps {
        match tape.below(8, "next publication actor") {
            0 => sim.consume(tape)?,
            1 => sim.cancel(),
            2 => sim.expire(),
            3 => sim.advance(tape.int_in(31..=40, "lead loss delay")),
            4 => {
                sim.forget();
                sim.consume(tape)?;
            }
            5 => sim.durable = true,
            6 => sim.deliver(tape)?,
            _ => {
                sim.restart(tape);
                sim.consume(tape)?;
            }
        }
    }
    sim.cancel();
    sim.forget();
    sim.consume(tape)?;
    // This is the durable catalog's completion, not a worker's unmirrored acceptance.
    sim.durable = true;
    sim.deliver(tape)?;
    sim.forget();
    // A request whose speculative miss predates completion still owns no compile permit.
    // Its new flight must resolve the durable hit, including in a fresh coordinator epoch.
    sim.consume(tape)?;
    sim.restart(tape);
    sim.consume(tape)?;
    Ok(())
}

fn config() -> Config {
    Config::cases(128).must_reach_points(<AdmissionPoint as blaze_sim::Point>::IDS)
}

/// The invariant (simulated): current-attempt absence resolution gates actual grants and full-tail
/// readiness; accepted output remains discoverable without subscribers until durable delivery,
/// and delivery preserves terminals. Full tails are counted by complete result identity across
/// executions; only an unmirrored coordinator restart permits recomputation of accepted output.
#[test]
fn resolution_and_delivery_preserve_one_accepted_result() {
    blaze_sim::check(config(), |tape| admission(tape, AdmissionPlant::None));
}

#[test]
fn the_oracle_catches_admission_and_delivery_faults() {
    blaze_sim::controls(config(), admission);
}

#[test]
fn accepted_output_survives_the_last_subscriber_and_terminal_retention() {
    admission(&Tape::replay(&[0, 0, 0, 0, 0]), AdmissionPlant::None).unwrap();
}

#[test]
fn pending_output_preserves_pr_source_fences_and_namespace_identity() {
    blaze_sim::check(Config::cases(64), |tape| {
        let alloc = blaze_own::testing::heap().shared();
        let mut sim = Simulation::new(AdmissionPlant::None, alloc);
        sim.request.namespace = Namespace::Pr(blaze_wire::PrNamespace { n: 7 });
        sim.consume(tape).unwrap();
        let accepted = sim.jobs.executions().find_map(Execution::accepted_publication).unwrap();
        sim.cancel();
        sim.advance(tape.int_in(1_001..=2_000, "past every original PR deadline"));
        sim.forget();
        assert!(sim.jobs.has_accepted_for(accepted.content));
        assert_eq!(sim.jobs.pr_sources, 1, "pending output retains its source history");

        let subscriber = sim.jobs.next_subscriber;
        sim.request.source = [9; 32];
        sim.request.deadline_ms = sim.now + tape.int_in(100..=1_000, "new revision deadline");
        let newest_deadline = sim.request.deadline_ms;
        let answer = sim.jobs.submit(sim.now, &sim.request, need(), &mut sim.out).unwrap();
        assert!(matches!(answer, SubmitAnswer::Accepted(ref record)
            if record.execution == accepted.execution && record.packs == accepted.packs));
        assert_eq!(sim.jobs.next_subscriber, subscriber, "a result does not revive old interest");
        sim.request.source = [8; 32];
        assert_eq!(
            sim.jobs.submit(sim.now, &sim.request, need(), &mut sim.out).unwrap_err().0,
            Refusal::Superseded,
        );
        sim.request.source = [9; 32];
        sim.request.deadline_ms = sim.now - 1;
        assert_eq!(
            sim.jobs.submit(sim.now, &sim.request, need(), &mut sim.out).unwrap_err().0,
            Refusal::PrDeadlineExpired,
        );

        sim.request.deadline_ms = newest_deadline;
        for namespace in [Namespace::Main, Namespace::Pr(blaze_wire::PrNamespace { n: 8 })] {
            sim.request.namespace = namespace;
            let other = attached_for_test(sim.jobs.submit(
                sim.now, &sim.request, need(), &mut sim.out,
            ));
            assert_ne!(other.content, accepted.content);
            assert_ne!(other.execution, accepted.execution);
        }
        sim.jobs.deliver_publication(
            sim.now, accepted.execution, &accepted.packs, &mut sim.out,
        ).unwrap();
        assert!(sim.jobs.forget_terminal(accepted.execution));
        sim.advance(newest_deadline + 1 - sim.now);
        assert!(!sim.jobs.pr_jobs.contains_key(&7), "delivered output no longer pins PR history");
    });
}
