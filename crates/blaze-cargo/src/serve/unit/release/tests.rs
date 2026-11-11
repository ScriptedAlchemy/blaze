use blaze_graph::admission::Pressure;
use blaze_mem::Tag;
use blaze_query::db::MemoryUsage;

use super::{FinalRelease, Finished, Holders, Plan, Release, Rows, Said};
use hegel::{TestCase, generators as gs};

#[derive(Clone, Copy)]
enum CompletionPlant {
    None,
    Pressure,
    Finished,
    End,
}

fn completed_plan(
    compiles: bool,
    pressed: bool,
    ended: bool,
    open: bool,
    count: usize,
    plant: CompletionPlant,
) -> Result<(), String> {
    let fine = || Pressure { held: 1, limit: 1000, over: None };
    let mut release = Release::of(
        Finished::new((0..count + 2).map(|it| Some(it as u32)).collect()), compiles,
    );
    release.began(0);
    release.compiled_under(0, fine);
    release.pressed();
    let frozen = release.plan().ok_or("the first plan was not frozen")?;
    for unit in 1..=count {
        release.began(unit);
        release.compiled_under(unit, fine);
    }
    if open {
        release.began(count + 1);
    }
    if pressed {
        release.pressed();
    }
    if ended {
        release.end();
    }
    release.released(frozen.pressed, fine());
    match plant {
        CompletionPlant::None => {}
        CompletionPlant::Pressure => release.pressed = false,
        CompletionPlant::Finished => release.finished.ready.clear(),
        CompletionPlant::End => release.ended = false,
    }
    if release.due() != (pressed || ended || (compiles && !open)) {
        return Err("completion erased a later release request".to_owned());
    }
    let expected = if compiles {
        Some(Plan {
            crates: (1..=count).map(|it| it as u32).collect(),
            mir: pressed || ended || !open, pressed, ended,
        })
    } else {
        (pressed || ended).then(|| Plan { crates: Vec::new(), mir: true, pressed, ended })
    };
    let next = release.plan();
    if next != expected {
        return Err(format!("completion changed the later plan: {next:?}, expected {expected:?}"));
    }
    Ok(())
}

/// The invariant: completing one frozen worker plan changes only its squeeze measure;
/// later pressure, completed crates and end requests survive in the next exact plan.
#[hegel::test(test_cases = 64)]
fn completion_preserves_later_pressure_finished_crates_and_end(tc: TestCase) {
    completed_plan(
        tc.draw(gs::booleans()), tc.draw(gs::booleans()), tc.draw(gs::booleans()),
        tc.draw(gs::booleans()), usize::from(tc.draw(gs::integers::<u8>().max_value(3))) + 1,
        CompletionPlant::None,
    ).unwrap();
}

#[test]
fn the_oracle_catches_completion_erasing_later_pressure() {
    assert!(completed_plan(false, true, false, true, 1, CompletionPlant::Pressure).is_err());
}

#[test]
fn the_oracle_catches_completion_erasing_later_finished_crates() {
    assert!(completed_plan(true, false, false, false, 1, CompletionPlant::Finished).is_err());
}

#[test]
fn the_oracle_catches_completion_erasing_a_later_end() {
    assert!(completed_plan(false, false, true, true, 1, CompletionPlant::End).is_err());
}

fn final_policy(pending_before: bool, after_return: bool, reuse_old: bool) -> Result<(), &'static str> {
    let mut final_release = FinalRelease::default();
    if final_release.request(pending_before) == pending_before {
        return Err("a final plan was scheduled before its prior inputs applied");
    }
    if pending_before {
        final_release.written();
        if !final_release.applied() {
            return Err("applying prior inputs lost the standing final request");
        }
    }
    if after_return {
        final_release.released(true, false);
        final_release.written();
    } else {
        final_release.written();
        final_release.released(true, true);
    }
    if final_release.ready() || !final_release.applied() {
        return Err("a late input lost its required newer final plan");
    }
    if reuse_old {
        final_release.reuse_older_plan_for_test();
    }
    final_release.released(false, false);
    if final_release.ready() {
        return Err("an older or nonfinal plan authorized keep after a late input");
    }
    final_release.released(true, false);
    if !final_release.ready() {
        return Err("the final plan after the last write did not authorize keep");
    }
    Ok(())
}

/// The invariant: the production FinalRelease policy schedules the final plan after all
/// writes and accepts only its completion, including a late write on either side of return.
#[hegel::test(test_cases = 32)]
fn final_release_policy_requires_completion_after_every_late_write(tc: TestCase) {
    final_policy(tc.draw(gs::booleans()), tc.draw(gs::booleans()), false).unwrap();
}

#[test]
fn the_oracle_catches_reusing_a_final_plan_before_a_late_write() {
    for after_return in [false, true] {
        assert!(final_policy(false, after_return, true).unwrap_err().contains("authorized keep"));
    }
}

/// A crate is finished once every unit of it compiled, never before, and once; a unit is open
/// from its first task to its compile's end, and a build script's run (no crate) or a unit
/// told twice changes nothing it shouldn't.
#[test]
fn a_crate_is_finished_once_its_last_unit_compiled_and_open_units_are_counted() {
    // Units 0 and 1 of crate 7 (its lib and its tests), 2 of crate 9, 3 a build script's run.
    let mut finished = Finished::new(vec![Some(7u32), Some(7), Some(9), None]);
    assert!(finished.idle() && !finished.has_ready());
    finished.began(0);
    finished.began(2);
    finished.began(2);
    assert!(!finished.idle());
    finished.compiled(0);
    assert!(!finished.has_ready(), "crate 7 still has unit 1 to compile");
    finished.compiled(2);
    assert_eq!(finished.take(), vec![9]);
    assert!(finished.idle(), "every unit that began has compiled");
    finished.began(3);
    assert!(finished.idle(), "a build script's run never opens: no compile ends it");
    finished.compiled(3);
    assert!(finished.idle() && !finished.has_ready(), "a run has no crate");
    finished.compiled(1);
    assert_eq!(finished.take(), vec![7], "a unit that compiled without a begun task counts too");
    finished.compiled(1);
    finished.compiled(0);
    finished.compiled(9);
    assert!(finished.take().is_empty(), "each crate once; a unit past the build is none");
    assert!(finished.idle());
}

/// The decisions the job loop acts on: a release is due once no unit is open and a crate is
/// finished, or once memory is short; the MIR goes only then, never while a unit is open and
/// memory is fine; a check releases only when memory is short, and then names no crate; a
/// release taken is not due again.
#[test]
fn a_release_is_due_when_no_unit_is_open_or_memory_is_short_and_takes_the_mir_only_then() {
    const GIB: u64 = 1 << 30;
    let fine = || Pressure { held: GIB, limit: 48 * GIB, over: None };
    let short = || Pressure { held: 47 * GIB, limit: 48 * GIB, over: Some(Tag::Mir) };
    // Units 0 and 1 of crate 7, 2 of crate 9.
    let mut release = Release::of(Finished::new(vec![Some(7u32), Some(7), Some(9)]), true);
    release.began(0);
    release.began(2);
    release.compiled_under(2, fine);
    assert!(!release.due(), "crate 9 is finished but unit 0 is open and memory is fine");
    release.began(1);
    release.compiled_under(0, short);
    assert!(release.due(), "memory is short");
    assert_eq!(
        release.plan(),
        Some(Plan { crates: vec![9], mir: true, pressed: true, ended: false })
    );
    assert!(!release.due() && release.plan().is_none(), "taken");
    release.compiled_under(1, fine);
    assert!(release.due(), "no unit is open and crate 7 is finished");
    assert_eq!(
        release.plan(),
        Some(Plan { crates: vec![7], mir: true, pressed: false, ended: false })
    );

    let mut open = Release::of(Finished::new(vec![Some(7u32), Some(9)]), true);
    open.began(0);
    open.began(1);
    open.compiled_under(1, fine);
    assert_eq!(
        open.plan(),
        Some(Plan { crates: vec![9], mir: false, pressed: false, ended: false }),
        "unit 0 still reads MIR"
    );

    let mut check = Release::of(Finished::new(vec![Some(7u32), Some(9)]), false);
    check.began(0);
    check.began(1);
    check.compiled_under(0, fine);
    assert!(
        !check.due() && check.plan().is_none(),
        "a check lets go of nothing while memory is fine"
    );
    check.compiled_under(1, fine);
    assert!(
        !check.due() && check.plan().is_none(),
        "nor once no unit is open: the next check reads its inference"
    );
    let mut pressed = Release::of(Finished::new(vec![Some(7u32), Some(9)]), false);
    pressed.began(0);
    pressed.began(1);
    pressed.compiled_under(0, short);
    assert!(pressed.due(), "memory is short in a check too");
    assert_eq!(
        pressed.plan(),
        Some(Plan { crates: vec![], mir: true, pressed: true, ended: false }),
        "a check's release under pressure lets go of every MIR and sweeps, but of no crate's inference by name"
    );
    assert!(!pressed.due() && pressed.plan().is_none(), "taken");
    let mut ended = Release::of(Finished::new(vec![Some(7u32), Some(9)]), false);
    ended.began(0);
    ended.compiled_under(0, fine);
    assert!(!ended.due(), "a check under way lets go of nothing while memory is fine");
    ended.end();
    assert!(ended.due(), "a check's end lets go of its MIR, under no pressure");
    assert_eq!(
        ended.plan(),
        Some(Plan { crates: vec![], mir: true, pressed: false, ended: true }),
        "every MIR, no crate's inference, no sweep: the next check reads its inference and verifies its MIR's memos"
    );
    assert!(!ended.due() && ended.plan().is_none(), "taken");
    let mut built = Release::of(Finished::new(vec![Some(7u32), Some(9)]), true);
    built.began(0);
    built.began(1);
    built.compiled_under(1, fine);
    assert_eq!(
        built.plan(),
        Some(Plan { crates: vec![9], mir: false, pressed: false, ended: false })
    );
    built.end();
    assert_eq!(
        built.plan(),
        Some(Plan { crates: vec![], mir: true, pressed: false, ended: true }),
        "a build's end lets go of every MIR though a unit (one that panicked) is still open"
    );
    let mut held_back = Release::of(Finished::new(vec![Some(7u32)]), false);
    held_back.began(0);
    held_back.short_under(short);
    assert_eq!(
        held_back.plan(),
        Some(Plan { crates: vec![], mir: true, pressed: true, ended: false }),
        "the plane holding a task back for memory makes it due too"
    );
}

/// Each release says one line naming why it ran, what went, what blaze held before and after,
/// and the limit, so a run's log shows every release (B5, coord-105 showed none), then what the
/// query storage holds: its largest holders on a release memory made due, its total on another.
#[test]
fn a_release_says_why_what_went_and_the_memory_against_the_limit() {
    const GIB: u64 = 1 << 30;
    let dropped = hir::evict::Dropped {
        bodies: 40,
        inference: 30,
        mir: 900,
        const_eval: 7,
        solver_caches: 5,
        intern_tables: 3,
    };
    let before = Pressure { held: 44 * GIB, limit: 48 * GIB, over: Some(Tag::Mir) };
    let after = Pressure { held: 38 * GIB, limit: 48 * GIB, over: None };
    // A release memory made due says the storage's holders.
    let infer = MemoryUsage {
        name: "infer",
        rows: 2048,
        used: 1500,
        row_bytes: 1 << 28,
        value_bytes: 1 << 28,
    };
    let storage = Holders::of(vec![infer]);
    let said =
        Said { crates: 2, mir: true, pressed: true, ended: false, dropped, before, after, storage }
            .to_string();
    assert_eq!(
        said,
        "front end release (memory: blaze is past the limit's line of 42.00 GiB): let go of 30 inference results of 40 \
         bodies in 2 crates, 900 MIR bodies, 7 constant values and 5 trait solver caches, and swept the front end's \
         values to their fair shares; blaze held 44.00 GiB, now 38.00 GiB (6.00 freed), of a limit of 48.00 GiB; the \
         query storage holds 0.50 GiB: infer 0.50 (2.0k rows made, 1.5k used)"
    );
    let early = Pressure { held: 20 * GIB, ..before };
    let storage = Holders::of(Vec::new());
    let said = Said {
        crates: 1,
        mir: false,
        pressed: true,
        ended: false,
        dropped,
        before: early,
        after: early,
        storage,
    }
    .to_string();
    assert!(said.starts_with("front end release (memory: mir is past its share of "), "{said}");
    assert!(
        !said.contains("MIR bodies") && said.contains("in 1 crates and 5 trait solver caches"),
        "{said}"
    );
    assert!(said.contains("(0.00 freed)"), "{said}");
    // A release memory did not make due says the storage's total alone, read from counters.
    let idle = Pressure { held: GIB, limit: u64::MAX, over: None };
    let storage = Holders::total(3 << 29);
    let said = Said {
        crates: 1,
        mir: true,
        pressed: false,
        ended: false,
        dropped,
        before: idle,
        after: idle,
        storage,
    }
    .to_string();
    assert!(said.starts_with("front end release (no unit is open)"), "{said}");
    assert!(said.ends_with(", under no limit; the query storage holds 1.50 GiB"), "{said}");
}

/// A release's line ends with what the query storage holds and its eight largest holders,
/// most held first (a tie by name), each with its rows made and, where fewer, its rows in use,
/// so a function's pages made for keys it never ran read apart (memcap6).
#[test]
fn the_query_storage_s_largest_holders_are_said_with_their_rows_made_and_used() {
    let usage = |name: &'static str, rows, used, row_bytes, value_bytes| MemoryUsage {
        name,
        rows,
        used,
        row_bytes,
        value_bytes,
    };
    let holders = Holders::of(vec![
        usage("c", 1, 1, 1 << 20, 0),
        usage("source_place", 3_190_784, 400_000, 1 << 29, 0),
        usage("captured reads", 0, 0, 1 << 25, 0),
        usage("b", 1024, 1024, 1 << 23, 0),
        usage("dependency nodes", 4_000_000, 4_000_000, 3 << 26, 0),
        usage("infer", 2048, 1500, 1 << 28, 1 << 28),
        usage("a", 1024, 3, 1 << 24, 0),
        usage("dependency edges", 10_000_000, 10_000_000, 3 << 29, 0),
        usage("Crate", 950, 950, 1 << 26, 0),
    ]);
    assert_eq!(holders.total, 3_012_558_848, "every holder, the ninth not said included");
    assert_eq!(
        holders.to_string(),
        "the query storage holds 2.81 GiB: dependency edges 1.50 (10.00M rows), infer 0.50 (2.0k rows made, 1.5k used), \
         source_place 0.50 (3.19M rows made, 400.0k used), dependency nodes 0.19 (4.00M rows), Crate 0.06 (950 rows), \
         captured reads 0.03 (0 rows), a 0.02 (1.0k rows made, 3 used), b 0.01 (1.0k rows)"
    );
}

/// A count of rows is said in the unit it rounds to: one that rounds up past a unit's last
/// value says the next unit (999,950 rows are `1.00M`, never `1000.0k`).
#[test]
fn a_count_of_rows_is_said_in_the_unit_it_rounds_to() {
    let said = |rows| Rows(rows).to_string();
    assert_eq!(said(999), "999");
    assert_eq!(said(1_000), "1.0k");
    assert_eq!(said(1_049), "1.0k");
    assert_eq!(said(1_050), "1.1k");
    assert_eq!(said(999_949), "999.9k");
    assert_eq!(said(999_950), "1.00M");
    assert_eq!(said(999_999), "1.00M");
    assert_eq!(said(3_190_784), "3.19M");
    assert_eq!(said(9_995_000), "10.00M");
}
