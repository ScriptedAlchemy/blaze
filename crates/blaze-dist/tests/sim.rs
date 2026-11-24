//! The deterministic simulation, run over 10,000 seeds for each invariant set.
//!
//! The seeds are split into shards, one test each, so the test harness runs them in parallel
//! without the crate starting a thread.

#![no_std]
#![allow(missing_docs)]

use core::fmt::Write as _;

use blaze_dist::sim;
use blaze_own::{String, Vec};
use blaze_wire::WireAlloc;

const WORK_SHARDS: u64 = 8;
const JOBS_SHARDS: u64 = 4;
const SEEDS: u64 = 10_000;

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn fail(set: &str, run: u64, failures: &[sim::Violation], alloc: WireAlloc) {
    if let Some(first) = failures.first() {
        let seeds = Vec::from_iter_in(failures.iter().map(|v| v.seed).take(20), alloc);
        panic!(
            "{} of {run} {set} seeds broke an invariant; the first: {first}\n\
             failing seeds: {seeds:?}",
            failures.len(),
        );
    }
}

fn work_shard(shard: u64) {
    let alloc = heap();
    let mut failures = Vec::new_in(alloc);
    let mut run = 0u64;
    for seed in (0..SEEDS).filter(|s| s % WORK_SHARDS == shard) {
        run += 1;
        if let Err(v) = sim::work::run(seed, alloc) {
            failures.push(v);
        }
    }
    fail("work", run, &failures, alloc);
}

fn jobs_shard(shard: u64) {
    let alloc = heap();
    let mut failures = Vec::new_in(alloc);
    let mut run = 0u64;
    for seed in (0..SEEDS).filter(|s| s % JOBS_SHARDS == shard) {
        run += 1;
        if let Err(v) = sim::jobs::run(seed, alloc) {
            failures.push(v);
        }
    }
    fail("jobs", run, &failures, alloc);
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

/// One seed replayed with every event of the work run traced into a caller buffer.
#[test]
fn replay() {
    let alloc = heap();
    let seed = 0u64;
    let mut trace_buf = String::new_in(alloc);
    let work = sim::work::trace(seed, alloc, &mut |event| {
        writeln!(&mut trace_buf, "{event}").unwrap();
    });
    let jobs = sim::jobs::run(seed, alloc);
    assert!(work.is_ok() && jobs.is_ok(), "seed {seed} failed replay");
    assert!(!trace_buf.is_empty());
}

/// A seed replays exactly: the same run, event for event.
#[test]
fn deterministic() {
    let alloc = heap();
    for seed in [0, 1, 42, 9_999] {
        assert_eq!(sim::work::run(seed, alloc), sim::work::run(seed, alloc));
        assert_eq!(sim::jobs::run(seed, alloc), sim::jobs::run(seed, alloc));
        let (mut first, mut second) = (String::new_in(alloc), String::new_in(alloc));
        assert_eq!(
            sim::work::trace(seed, alloc, &mut |event| writeln!(&mut first, "{event}").unwrap()),
            sim::work::trace(seed, alloc, &mut |event| writeln!(&mut second, "{event}").unwrap()),
        );
        assert_eq!(first.as_str(), second.as_str(), "seed {seed} changed its event trace");
    }
}
