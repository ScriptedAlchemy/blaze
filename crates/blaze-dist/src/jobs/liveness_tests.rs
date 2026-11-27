//! Lead replacement uses accepted work, never traffic a worker cannot complete.

use core::fmt;

use blaze_own::Vec;
use blaze_sim::{Broken, Config, Tape};
use blaze_wire::{Lease, RootSet, Set};

use super::*;

#[derive(Debug)]
enum LivenessBroken {
    RefusedWork,
}

impl fmt::Display for LivenessBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("refused work made a helper eligible as a recently heard lead replacement")
    }
}

blaze_sim::plants! {
    enum LivenessPlant breaks LivenessBroken {
        None,
        RecordBeforeAccept => LivenessBroken::RefusedWork,
    }
}

fn liveness(tape: &Tape, plant: LivenessPlant) -> Result<(), Broken<LivenessBroken>> {
    let alloc = blaze_own::testing::heap().shared();
    let mut out = Vec::new_in(alloc);
    let mut execution = Execution::new(
        0,
        ExecutionId(1),
        JobContent([1; 32]),
        Namespace::Main,
        Subscriber {
            id: 1,
            deadline_ms: 100_000,
            policy: LocalPolicy::Never,
            state: SubscriberState::Attached,
        },
        ExecutionConfig::default(),
        alloc,
        &mut out,
    );
    execution.assign(0, "lead", &["bad", "healthy"], &mut out).unwrap();
    let heartbeat = WorkersEventsRequest {
        execution: execution.id,
        attempt: execution.attempt(),
        events: Vec::new_in(alloc),
    };
    if tape.coin("helper was heard before becoming stale") {
        execution.worker_events(0, "bad", &heartbeat, &mut out).unwrap();
    }
    let mut now = tape.int_in(15_001..=30_000, "after the helper liveness window");
    execution.worker_events(now, "healthy", &heartbeat, &mut out).unwrap();
    let roots = RootSet {
        spec: [1; 32],
        roots: [2; 32],
        count: 1,
        index: [3; 32],
        inputs: [4; 32],
    };
    let first = tape.below(3, "first refused request");
    for offset in 0..3 {
        now += tape.int_in(1..=100, "delay before refused request");
        let refused = match (first + offset) % 3 {
            0 => {
                let mut invalid_roots = roots.clone();
                invalid_roots.count = execution.config.work.max_roots + 1;
                execution
                    .join(
                        now,
                        "bad",
                        &WorkJoinRequest {
                            execution: execution.id,
                            attempt: execution.attempt(),
                            roots: invalid_roots,
                            rejected: Set::empty_in(alloc),
                        },
                        &mut out,
                    )
                    .map(|_| ())
            }
            1 => execution
                .lease(
                    now,
                    "bad",
                    &WorkLeaseRequest {
                        execution: execution.id,
                        attempt: execution.attempt(),
                    },
                    &mut out,
                )
                .map(|_| ()),
            _ => execution
                .result(
                    now,
                    "bad",
                    &BatchResult {
                        lease: Lease {
                            execution: execution.id,
                            attempt: execution.attempt(),
                            holder: WireString::from_str_in("bad", alloc),
                            generation: 1,
                        },
                        seq: 1,
                        items: Vec::new_in(alloc),
                        packs: Vec::new_in(alloc),
                        returned_roots: Set::empty_in(alloc),
                        returned_names: Set::empty_in(alloc),
                    },
                    &mut out,
                )
                .map(|_| ()),
        };
        assert!(refused.is_err(), "the fault did not reach the refusal boundary");
        if matches!(plant, LivenessPlant::RecordBeforeAccept) {
            execution.heard.insert(WireString::from_str_in("bad", alloc), now);
        }
        if execution.heard_recently(now).any(|executor| executor == "bad") {
            return Err(Broken::Oracle(LivenessBroken::RefusedWork));
        }
        assert!(execution.heard_recently(now).any(|executor| executor == "healthy"));
    }
    execution
        .join(
            now,
            "bad",
            &WorkJoinRequest {
                execution: execution.id,
                attempt: execution.attempt(),
                roots,
                rejected: Set::empty_in(alloc),
            },
            &mut out,
        )
        .unwrap();
    assert!(execution.heard_recently(now).any(|executor| executor == "bad"));
    Ok(())
}

/// The invariant: refused work never creates or refreshes helper liveness for lead replacement.
#[test]
fn refused_work_never_refreshes_helper_liveness() {
    blaze_sim::check(Config::cases(128), |tape| liveness(tape, LivenessPlant::None));
}

#[test]
fn the_oracle_catches_liveness_recorded_before_acceptance() {
    blaze_sim::controls(Config::cases(128), liveness);
}
