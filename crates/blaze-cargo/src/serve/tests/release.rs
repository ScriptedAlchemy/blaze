//! The real daemon and plane on Sim: the exclusive release carrier, its parked readers,
//! cancellation boundaries, and the one bounded discard. No sleeps or worker threads.

use hegel::{TestCase, generators as gs};

use super::*;
use crate::serve::job;
use blaze_graph::admission::{self, kinds};

type Checked = Result<(), String>;
const BOUND: usize = 1024;
// Far above any test process's footprint, but finite for measured refusal controls.
const CEILING: u64 = 1 << 50;

#[derive(Clone, Copy)]
enum Plant {
    None,
    Inline,
    HeldGrant,
    LateDrop,
    OldFinal,
}

struct Controls {
    limit: u64,
    _budget: std::sync::MutexGuard<'static, ()>,
}

impl Controls {
    fn new(plant: Plant) -> Self {
        let budget = BUDGET_TESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let controls = Self { limit: admission::limit(), _budget: budget };
        RELEASE_FRONTS.set(true);
        RELEASE_ON_COMPILED.set(false);
        RELEASES_TAKEN.set(0);
        RELEASES_RUN.set(0);
        RELEASES_INSTALLED.set(0);
        FAILS_RELEASE.set(false);
        PANICS_RELEASE.set(false);
        FRONT_DROPS.with_borrow_mut(Vec::clear);
        FRONT_EVENTS.with_borrow_mut(Vec::clear);
        job::RECLAIM_ATTEMPTS.with_borrow_mut(Vec::clear);
        job::PARKED_READERS.set(0);
        job::ACTIVE_RECLAIM_TASK.set(None);
        job::INLINE_RELEASE.set(matches!(plant, Plant::Inline));
        job::KEEP_READER_GRANT.set(matches!(plant, Plant::HeldGrant));
        job::DROP_LATE_RETURN.set(matches!(plant, Plant::LateDrop));
        job::CANCEL_RELEASE_RETURN.set(false);
        job::STALL_RELEASE_RETURN.set(false);
        controls
    }
}

impl Drop for Controls {
    fn drop(&mut self) {
        RELEASE_FRONTS.set(false);
        RELEASE_ON_COMPILED.set(false);
        FAILS_RELEASE.set(false);
        PANICS_RELEASE.set(false);
        job::INLINE_RELEASE.set(false);
        job::KEEP_READER_GRANT.set(false);
        job::DROP_LATE_RETURN.set(false);
        job::CANCEL_RELEASE_RETURN.set(false);
        job::STALL_RELEASE_RETURN.set(false);
        admission::set_budgets(self.limit);
    }
}

fn require(ok: bool, why: &str) -> Checked {
    if ok { Ok(()) } else { Err(why.to_owned()) }
}

fn daemon(seed: u64, budget: bool) -> SimDaemon {
    crate::release::for_tests();
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let mut config = options();
    config.threads = 1;
    config.budget = budget.then(|| blaze_graph::Budget {
        bytes: CEILING, from: "release carrier control".to_owned(), kept_back: 0,
    });
    Daemon::start(sim, config, Arc::new(Fake::default()), Runner::Inline(Inline::new(seed)))
        .unwrap_or_else(|why| panic!("{why}"))
}

fn build(client: &mut Client, daemon: &mut SimDaemon) -> u64 {
    client.send(daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() })
}

fn until(
    daemon: &mut SimDaemon,
    client: &mut Client,
    mut reached: impl FnMut(&Client) -> bool,
) -> bool {
    for _ in 0..BOUND {
        client.take(daemon);
        if reached(client) {
            return true;
        }
        if !daemon.turn() {
            client.take(daemon);
            return reached(client);
        }
    }
    false
}

fn finished(daemon: &mut SimDaemon, client: &mut Client, id: u64, ok: bool) -> Checked {
    require(until(daemon, client, |client| client.done(id)), "the build did not finish within the turn bound")?;
    require(client.of(id).last() == Some(&Response::Done { ok }), "the build's final outcome is wrong")
}

fn has_error(client: &Client, id: u64, text: &str) -> bool {
    client.of(id).iter().any(|it| matches!(it, Response::Error(error) if error.what.contains(text)))
}

fn drops_on(kind: Option<blaze_graph::admission::TaskKind>) -> bool {
    FRONT_DROPS.with_borrow(|it| it.as_slice() == [("/w".to_owned(), kind)])
}

fn held_release(seed: u64, budget: bool, plant: Plant) -> Checked {
    let _controls = Controls::new(plant);
    let mut daemon = daemon(seed, budget);
    daemon.hold_tasks_for_test(kinds::FRONT_RELEASE, true);
    let stores = STORES.get();
    let mut client = dial(&mut daemon);
    let id = build(&mut client, &mut daemon);
    require(until(&mut daemon, &mut client, |client| RELEASES_TAKEN.get() > 0 || client.done(id)), "no exclusive release was transferred")?;
    require(RELEASES_TAKEN.get() == 1, "the writer did not transfer exactly one final release")?;
    require(RELEASES_RUN.get() == 0, "the release ran inline before its held worker could run")?;
    require(!client.done(id) && STORES.get() == stores, "publication preceded the worker release")?;
    require(FRONT_EVENTS.with_borrow(Vec::is_empty), "history or keep consumed the front end before final release completed")?;
    let status = client.send(&mut daemon, Request::Status);
    require(until(&mut daemon, &mut client, |client| client.done(status)), "the loop could not answer status while release was held")?;
    require(!client.done(id) && RELEASES_RUN.get() == 0, "answering status released or published the held front end")?;
    daemon.hold_tasks_for_test(kinds::FRONT_RELEASE, false);
    finished(&mut daemon, &mut client, id, true)?;
    require(RELEASES_RUN.get() == 1 && RELEASES_INSTALLED.get() == 1, "the exact final release was not run and installed once")?;
    require(FRONT_EVENTS.with_borrow(|it| it.as_slice() == [("history", 1), ("keep", 1)]), "history was not read from the returned state before keep")?;
    require(STORES.get() == stores + 1 && holds_kept(daemon.store(), "/w"), "the completed release was not kept durably")
}

/// The invariant: an exclusive final release stays on its admitted worker, keeps the loop
/// responsive, and gates publication until that exact state returns and the store commits.
#[hegel::test(test_cases = 24)]
fn worker_release_keeps_the_loop_responsive_and_publication_fenced(tc: TestCase) {
    held_release(tc.draw(gs::integers::<u64>()), tc.draw(gs::booleans()), Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_inline_release() {
    assert!(held_release(11, false, Plant::Inline).unwrap_err().contains("release ran inline"));
}

fn successor(seed: u64, budget: bool, plant: Plant) -> Checked {
    let _controls = Controls::new(plant);
    RELEASE_ON_COMPILED.set(true);
    let mut daemon = daemon(seed, budget);
    let mut client = dial(&mut daemon);
    let id = build(&mut client, &mut daemon);
    // Graph Done admits a successor before Jobs sees the predecessor's Did and makes its
    // release due. One Work slot makes retaining that posting's grant/slot a real cycle.
    require(until(&mut daemon, &mut client, |client| client.done(id)), "a held successor blocked release with the only Work slot")?;
    require(client.of(id).last() == Some(&Response::Done { ok: true }), "a held successor blocked release instead of completing")?;
    require(job::PARKED_READERS.get() > 0, "the fixture never parked an admitted successor")?;
    require(RELEASES_RUN.get() > 1, "the fixture never ran a release between unit compiles")?;
    require(RELEASES_TAKEN.get() == RELEASES_INSTALLED.get(), "an exclusive handoff was lost")
}

/// The invariant: a successor admitted before Did gives its real grant and the only Work
/// slot back while the writer is exclusive, then runs only after ordinary readmission.
#[hegel::test(test_cases = 16)]
fn parked_successors_allow_release_with_one_work_slot(tc: TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    for budget in [false, true] {
        successor(seed, budget, Plant::None).unwrap();
    }
}

#[test]
fn the_oracle_catches_a_held_successor_grant() {
    for budget in [false, true] {
        assert!(successor(27, budget, Plant::HeldGrant).unwrap_err().contains("held successor blocked release"));
    }
}

fn cancellation(seed: u64, budget: bool, at: u8, plant: Plant) -> Checked {
    let _controls = Controls::new(plant);
    let mut daemon = daemon(seed, budget);
    let (anew, stores) = (ANEW.get(), STORES.get());
    if at == 0 {
        daemon.hold_tasks_for_test(kinds::UNIT_NAMES, true);
        daemon.hold_tasks_for_test(kinds::PROGRAM, true);
    } else {
        daemon.hold_tasks_for_test(kinds::FRONT_RELEASE, true);
    }
    let mut client = dial(&mut daemon);
    let id = build(&mut client, &mut daemon);
    require(until(&mut daemon, &mut client, |client| {
        if at == 0 { ANEW.get() > anew || client.done(id) }
        else { RELEASES_TAKEN.get() > 0 || client.done(id) }
    }), "the cancellation boundary was never reached")?;
    require(!client.done(id), "the build answered before its held cancellation boundary")?;
    if at == 2 {
        // Invoke the real cancel transition at the consumer, after the worker's final
        // cancelled load has returned Some(front), before the loop installs that result.
        job::CANCEL_RELEASE_RETURN.set(true);
    } else {
        let cancel = client.send(&mut daemon, Request::Cancel { id });
        require(until(&mut daemon, &mut client, |client| client.done(cancel)), "the loop did not acknowledge cancellation")?;
        if at == 1 {
            require(!client.done(id), "cancellation forgot the outstanding owned release")?;
        }
    }
    daemon.hold_tasks_for_test(kinds::UNIT_NAMES, false);
    daemon.hold_tasks_for_test(kinds::PROGRAM, false);
    daemon.hold_tasks_for_test(kinds::FRONT_RELEASE, false);
    finished(&mut daemon, &mut client, id, false)?;
    require(STORES.get() == stores && !holds_kept(daemon.store(), "/w"), "cancellation published a front end")?;
    require(RELEASES_INSTALLED.get() == 0, "cancellation installed an owned worker result")?;
    let expected = match at {
        0 => None, // Ordinary cancellation before transfer still owns its local writer.
        1 => Some(kinds::FRONT_RELEASE),
        _ => Some(kinds::FRONT_DISCARD),
    };
    require(drops_on(expected), "a late owned result was dropped on the loop or never disposed")?;
    if at == 2 {
        require(job::RECLAIM_ATTEMPTS.with_borrow(|it| it.as_slice() == [kinds::FRONT_RELEASE, kinds::FRONT_DISCARD]), "late cancellation did not get exactly one discard")?;
    }
    let next = build(&mut client, &mut daemon);
    finished(&mut daemon, &mut client, next, true)
}

/// The invariant: cancel before transfer, while the worker is held, and after its result
/// returns each retire Stage::Units, preserve exclusive ownership, and publish no front end.
#[hegel::test(test_cases = 16)]
fn cancellation_at_each_handoff_boundary_retires_the_build(tc: TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let budget = tc.draw(gs::booleans());
    for at in 0u8..3 {
        cancellation(seed.wrapping_add(u64::from(at)), budget, at, Plant::None).unwrap();
    }
}

#[test]
fn the_oracle_catches_a_late_result_dropped_on_the_loop() {
    assert!(cancellation(39, false, 2, Plant::LateDrop).unwrap_err().contains("late owned result was dropped on the loop"));
}

fn failed_release(seed: u64, panic: bool) -> Checked {
    let _controls = Controls::new(Plant::None);
    FAILS_RELEASE.set(!panic);
    PANICS_RELEASE.set(panic);
    let mut daemon = daemon(seed, false);
    let stores = STORES.get();
    let mut client = dial(&mut daemon);
    let id = build(&mut client, &mut daemon);
    finished(&mut daemon, &mut client, id, false)?;
    let cause = if panic { "serve-test: front end release panics" } else { "serve-test: front end release fails" };
    require(has_error(&client, id, cause), "the release's original failure cause was replaced")?;
    require(STORES.get() == stores && !holds_kept(daemon.store(), "/w"), "a failed or partly released front end was kept")?;
    require(drops_on(Some(kinds::FRONT_RELEASE)), "a release failure destroyed its owned state outside the worker")?;
    require(job::RECLAIM_ATTEMPTS.with_borrow(|it| it.as_slice() == [kinds::FRONT_RELEASE]), "a consumed failing release was needlessly retried")?;
    FAILS_RELEASE.set(false);
    PANICS_RELEASE.set(false);
    let next = build(&mut client, &mut daemon);
    finished(&mut daemon, &mut client, next, true)
}

/// The invariant: release errors and panics consume their exclusive state on the worker,
/// keep the first cause, publish nothing from that state, and fail only their owning build.
#[hegel::test(test_cases = 16)]
fn release_failure_and_panic_preserve_the_cause_and_keep_serving(tc: TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    for panic in [false, true] {
        failed_release(seed, panic).unwrap();
    }
}

fn refused_release(seed: u64, refuse_discard: bool) -> Checked {
    let _controls = Controls::new(Plant::None);
    let mut daemon = daemon(seed, true);
    // Actual measured forecasts are retained; neither refusal nor the continuation resets
    // them. The discard has its own learned cost and uses the same positive admission.
    daemon.observe_task_for_test(kinds::FRONT_RELEASE, job::release_cost_key("/w"), CEILING * 2);
    daemon.observe_task_for_test(kinds::FRONT_DISCARD, job::discard_cost_key("/w"), if refuse_discard { CEILING * 2 } else { 1 });
    let stores = STORES.get();
    let mut client = dial(&mut daemon);
    let id = build(&mut client, &mut daemon);
    finished(&mut daemon, &mut client, id, false)?;
    require(RELEASES_TAKEN.get() == 1 && RELEASES_RUN.get() == 0, "the measured release refusal ran its worker or lost its input")?;
    require(has_error(&client, id, "front end release"), "the release admission failure was lost")?;
    require(job::RECLAIM_ATTEMPTS.with_borrow(|it| it.as_slice() == [kinds::FRONT_RELEASE, kinds::FRONT_DISCARD]), "the failed release got other than one discard continuation")?;
    let expected = if refuse_discard { None } else { Some(kinds::FRONT_DISCARD) };
    require(drops_on(expected), "refused release ownership did not follow the bounded discard policy")?;
    require(has_error(&client, id, "single bounded front end discard") == refuse_discard, "the terminal discard limit was hidden or reported on success")?;
    require(STORES.get() == stores && !holds_kept(daemon.store(), "/w"), "a refused release or discard published state")?;
    // A second build refuses again by the retained measured release cost, with one new
    // discard, instead of resetting the prediction or looping an unadmittable task.
    let next = build(&mut client, &mut daemon);
    finished(&mut daemon, &mut client, next, false)?;
    require(RELEASES_RUN.get() == 0 && job::RECLAIM_ATTEMPTS.with_borrow(|it| it.len() == 4), "a refusal reset its measured cost or retried cleanup")
}

/// The invariant: refusal before a worker starts retains ownership for one independently
/// accounted discard; refusing that discard is terminal, named, and never resets a forecast.
#[hegel::test(test_cases = 16)]
fn refused_release_uses_one_accounted_discard(tc: TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    for refuse_discard in [false, true] {
        refused_release(seed, refuse_discard).unwrap();
    }
}

fn orphan(seed: u64) -> Checked {
    let _controls = Controls::new(Plant::None);
    let mut daemon = daemon(seed, false);
    daemon.hold_tasks_for_test(kinds::FRONT_RELEASE, true);
    daemon.hold_tasks_for_test(kinds::FRONT_DISCARD, true);
    let stores = STORES.get();
    let mut client = dial(&mut daemon);
    let id = build(&mut client, &mut daemon);
    require(until(&mut daemon, &mut client, |_| RELEASES_TAKEN.get() == 1), "the orphan fixture never transferred its state")?;
    job::STALL_RELEASE_RETURN.set(true);
    daemon.hold_tasks_for_test(kinds::FRONT_RELEASE, false);
    finished(&mut daemon, &mut client, id, false)?;
    require(has_error(&client, id, "stall after the worker returned"), "job removal lost its original stall cause")?;
    require(job::RECLAIM_ATTEMPTS.with_borrow(|it| it.as_slice() == [kinds::FRONT_RELEASE, kinds::FRONT_DISCARD]), "job removal lost the exact orphan cleanup owner")?;
    require(FRONT_DROPS.with_borrow(Vec::is_empty), "the orphan was dropped before its held discard worker ran")?;
    require(RELEASES_INSTALLED.get() == 0 && STORES.get() == stores, "an orphan result was installed or published")?;
    daemon.hold_tasks_for_test(kinds::FRONT_DISCARD, false);
    require(until(&mut daemon, &mut client, |_| FRONT_DROPS.with_borrow(|it| !it.is_empty())), "the orphan discard never completed")?;
    require(drops_on(Some(kinds::FRONT_DISCARD)), "the orphan's state was destroyed on the loop")?;
    let next = build(&mut client, &mut daemon);
    finished(&mut daemon, &mut client, next, true)
}

/// The invariant: removing a stalled job before consuming its worker result does not drop
/// the owned state on the loop or attach it to another job; its one discard stays accounted.
#[hegel::test(test_cases = 16)]
fn orphaned_worker_result_is_discarded_after_job_removal(tc: TestCase) {
    orphan(tc.draw(gs::integers::<u64>())).unwrap();
}

fn late_write(after_return: bool, plant: Plant) -> Checked {
    let _controls = Controls::new(plant);
    let mut writer = FakeWriter::new(FakeInputs { checkout: "/w".into(), ..FakeInputs::default() });
    writer.ending();
    let first = writer.take_release().ok_or("the initial final release was not transferred")?;
    require(writer.reader().is_none() && !writer.apply().map_err(|it| it.what)?, "the writer exposed state during transfer")?;
    if !after_return {
        writer.ran(7, blaze_lower::Ran::default()).map_err(|it| it.what)?;
    }
    let first = FakeWriter::release(first).map_err(|it| it.what)?;
    writer.released(first).map_err(|(it, _)| it.what)?;
    if after_return {
        writer.ran(7, blaze_lower::Ran::default()).map_err(|it| it.what)?;
    }
    if matches!(plant, Plant::OldFinal) {
        writer.final_release.reuse_older_plan_for_test();
    }
    require(writer.reader().is_none(), "a queued late write exposed a reader")?;
    require(writer.apply().map_err(|it| it.what)?, "the late write did not apply after ownership returned")?;
    if matches!(plant, Plant::OldFinal) {
        // The old policy also kept the older completion marker across applying that write.
        // Plant it at this boundary; the healthy production applied() still invalidates it.
        writer.final_release.reuse_older_plan_for_test();
    }
    if !writer.waiting() {
        return match writer.keep() {
            Ok(_) => Err("keep accepted a late write after an older final release".to_owned()),
            Err(_) => Err("the late write lost its required final release".to_owned()),
        };
    }
    let last = writer.take_release().ok_or("the applied late write did not request another final release")?;
    require(last.ending, "the later release was not marked final")?;
    let last = FakeWriter::release(last).map_err(|it| it.what)?;
    writer.released(last).map_err(|(it, _)| it.what)?;
    let kept = writer.keep().map_err(|it| it.what)?;
    require(kept.runs == [7] && RELEASES_TAKEN.get() == 2 && RELEASES_INSTALLED.get() == 2, "keep did not follow the exact final release after the late write")
}

/// The invariant: a write arriving during or just after a taken final plan remains queued,
/// applies only after ownership returns, and uses UnitWriter's production FinalRelease
/// policy to require a new final release before keep.
#[hegel::test(test_cases = 16)]
fn late_write_requires_another_final_release(tc: TestCase) {
    late_write(tc.draw(gs::booleans()), Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_keep_after_an_older_final_release() {
    for after_return in [false, true] {
        assert!(late_write(after_return, Plant::OldFinal).unwrap_err().contains("keep accepted a late write"));
    }
}
