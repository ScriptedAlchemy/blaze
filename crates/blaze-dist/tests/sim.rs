//! The deterministic simulation, run over at least 10,000 seeds for each invariant set.
//!
//! The seeds are split into shards, one test each, so the test harness runs them in parallel
//! without the crate starting a thread. A failure prints the seed and the command that replays
//! it, with every event, through the `replay` test:
//!
//!     BLAZE_DIST_SEED=<seed> cargo +1.99.0 test -p blaze-dist --test sim replay -- --nocapture
//!
//! `BLAZE_DIST_SEEDS=<n>` changes the number of seeds per invariant set.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use blaze_dist::sim;

const WORK_SHARDS: u64 = 8;
const JOBS_SHARDS: u64 = 4;

fn seeds() -> u64 {
    std::env::var("BLAZE_DIST_SEEDS").ok().and_then(|n| n.parse().ok()).unwrap_or(10_000)
}

fn fail(set: &str, run: u64, failures: &[sim::Violation]) {
    if let Some(first) = failures.first() {
        let seeds: Vec<u64> = failures.iter().map(|v| v.seed).take(20).collect();
        panic!(
            "{} of {run} {set} seeds broke an invariant; the first: {first}\n\
             failing seeds: {seeds:?}\n\
             replay: BLAZE_DIST_SEED={} cargo +1.99.0 test -p blaze-dist --test sim replay -- --nocapture",
            failures.len(),
            first.seed,
        );
    }
}

/// A seed's run, with a panic reported as a failure of that seed rather than of the shard.
fn guarded<T>(
    set: &'static str,
    seed: u64,
    run: impl FnOnce() -> Result<T, sim::Violation>,
) -> Result<T, sim::Violation> {
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(result) => result,
        Err(panic) => {
            let what = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            Err(sim::Violation { seed, invariant: set, at: 0, detail: format!("panicked: {what}") })
        }
    }
}

fn work_shard(shard: u64) {
    let started = Instant::now();
    let mut failures = Vec::new();
    let (mut run, mut done, mut lost_twice, mut no_lead, mut events) = (0, 0, 0, 0, 0);
    let (mut conflicts, mut revocations, mut resets, mut replays, mut dead_leases) =
        (0, 0, 0, 0, 0);
    for seed in (0..seeds()).filter(|s| s % WORK_SHARDS == shard) {
        run += 1;
        match guarded("panic", seed, || sim::work::run(seed)) {
            Ok(s) => {
                done += u64::from(s.done);
                lost_twice += u64::from(s.ended == Some(blaze_dist::work::EndWhy::LeadLostTwice));
                no_lead += u64::from(s.ended == Some(blaze_dist::work::EndWhy::NoLead));
                events += s.events;
                conflicts += s.counters.conflicts;
                revocations += s.counters.revocations;
                resets += s.counters.resets;
                replays += s.counters.replays;
                dead_leases += s.counters.dead_leases;
            }
            Err(v) => failures.push(v),
        }
    }
    eprintln!(
        "work shard {shard}: {run} seeds, {done} done, {lost_twice} lead lost twice, {no_lead} no lead, \
         {events} events; {conflicts} conflicts, {revocations} revocations, {resets} resets, \
         {replays} replays, {dead_leases} dead-lease refusals; {:.3} seconds",
        started.elapsed().as_secs_f64(),
    );
    fail("work", run, &failures);
}

fn jobs_shard(shard: u64) {
    let started = Instant::now();
    let mut failures = Vec::new();
    let (mut run, mut executions, mut busy, mut joined, mut reassigned) = (0, 0, 0, 0, 0);
    for seed in (0..seeds()).filter(|s| s % JOBS_SHARDS == shard) {
        run += 1;
        match guarded("panic", seed, || sim::jobs::run(seed)) {
            Ok(s) => {
                executions += s.executions;
                busy += s.busy;
                joined += s.joined;
                reassigned += s.reassigned;
            }
            Err(v) => failures.push(v),
        }
    }
    eprintln!(
        "jobs shard {shard}: {run} seeds, {executions} executions, {busy} busy, {joined} joined, \
         {reassigned} reassigned; {:.3} seconds",
        started.elapsed().as_secs_f64(),
    );
    fail("jobs", run, &failures);
}

#[test]
fn work_0() {
    work_shard(0);
}
#[test]
fn work_1() {
    work_shard(1);
}
#[test]
fn work_2() {
    work_shard(2);
}
#[test]
fn work_3() {
    work_shard(3);
}
#[test]
fn work_4() {
    work_shard(4);
}
#[test]
fn work_5() {
    work_shard(5);
}
#[test]
fn work_6() {
    work_shard(6);
}
#[test]
fn work_7() {
    work_shard(7);
}

#[test]
fn jobs_0() {
    jobs_shard(0);
}
#[test]
fn jobs_1() {
    jobs_shard(1);
}
#[test]
fn jobs_2() {
    jobs_shard(2);
}
#[test]
fn jobs_3() {
    jobs_shard(3);
}

/// One seed again, with every event of the work run printed.
#[test]
fn replay() {
    let Some(seed) = std::env::var("BLAZE_DIST_SEED").ok().and_then(|s| s.parse().ok()) else {
        return;
    };
    let mut events = Vec::new();
    let work = sim::work::trace(seed, &mut events);
    for event in events {
        eprintln!("{event}");
    }
    let jobs = sim::jobs::run(seed);
    eprintln!("work: {work:?}");
    eprintln!("jobs: {jobs:?}");
    assert!(work.is_ok() && jobs.is_ok(), "seed {seed} failed replay");
}

/// A seed replays exactly: the same run, event for event.
#[test]
fn deterministic() {
    for seed in [0, 1, 42, 9_999] {
        assert_eq!(sim::work::run(seed), sim::work::run(seed));
        assert_eq!(sim::jobs::run(seed), sim::jobs::run(seed));
        let (mut first, mut second) = (Vec::new(), Vec::new());
        assert_eq!(sim::work::trace(seed, &mut first), sim::work::trace(seed, &mut second),);
        assert_eq!(first, second, "seed {seed} changed its event trace");
    }
}
