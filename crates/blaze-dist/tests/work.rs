//! The work set's transitions one at a time, as D0's acceptance lists them (c3-B2.1 to
//! c3-B2.3, rb-M3): the root agreement, fencing, replays, returned work, finders, conflicts,
//! unknown names, lead loss, the progress timeout and the revocation bound.

#![no_std]
#![allow(missing_docs)]

use blaze_dist::work::{
    Effect, FenceWhy, LOST_FINDING, TailWhy, Terminal, UNKNOWN_NAME, WorkConfig, WorkSet,
};
use blaze_dist::{Ranges, Refusal};
use blaze_own::{Vec, vec_in};
use blaze_wire::{
    Attempt, BatchResult, EntryKey, EntryRef, ExecutionId, Found, Grant, ItemKey, ItemOutcome,
    ItemResult, RootSet, Set, Why, WireAlloc, WireString, WireVec, WorkJoinRequest, WorkKey,
    WorkLeaseRequest, WorkName,
};

const EXEC: ExecutionId = ExecutionId(1);
const A1: Attempt = Attempt(1);

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn rootset(count: u64, tag: u8) -> RootSet {
    RootSet { spec: [1; 32], roots: [tag; 32], count, index: [3; 32], inputs: [4; 32] }
}

fn name(tag: u8, alloc: WireAlloc) -> WorkName {
    WorkName::of(&WorkKey::Item(ItemKey { unit: [9; 32], stable: [tag; 32], env: [0; 32] }), alloc)
}

fn set(config: WorkConfig, alloc: WireAlloc) -> WorkSet {
    WorkSet::new(EXEC, A1, "lead", config, alloc)
}

fn small() -> WorkConfig {
    WorkConfig { grant_roots: 2, grant_names: 4, ..WorkConfig::default() }
}

fn join(
    w: &mut WorkSet,
    now: u64,
    who: &str,
    roots: &RootSet,
    alloc: WireAlloc,
) -> Result<blaze_wire::WorkJoinAnswer, Refusal> {
    let req = WorkJoinRequest {
        execution: EXEC,
        attempt: w.attempt(),
        roots: roots.clone(),
        rejected: Set::empty_in(alloc),
    };
    w.join(now, who, &req, &mut Vec::new_in(alloc))
}

fn lease(w: &mut WorkSet, now: u64, who: &str, alloc: WireAlloc) -> Option<Grant> {
    let req = WorkLeaseRequest { execution: EXEC, attempt: w.attempt() };
    w.lease(now, who, &req, &mut Vec::new_in(alloc)).expect("lease").grant
}

fn exported(n: WorkName, alloc: WireAlloc) -> ItemResult {
    ItemResult {
        name: n,
        outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey(n.0) }),
        found: Set::empty_in(alloc),
        unnamed: Set::empty_in(alloc),
    }
}

fn with_found(mut item: ItemResult, found: &[(WorkName, u8)], alloc: WireAlloc) -> ItemResult {
    item.found = Set::new(Vec::from_iter_in(
        found.iter().map(|&(name, t)| Found { name, tuple: [t; 32] }),
        alloc,
    ));
    item
}

fn lost(n: WorkName, why: &str, alloc: WireAlloc) -> ItemResult {
    ItemResult {
        name: n,
        outcome: ItemOutcome::Lost(Why { why: WireString::from_str_in(why, alloc) }),
        found: Set::empty_in(alloc),
        unnamed: Set::empty_in(alloc),
    }
}

/// A result for `grant` with items for every root (named by index) and every name.
fn result(
    grant: &Grant,
    seq: u64,
    roots: WireVec<ItemResult>,
    names: WireVec<ItemResult>,
    alloc: WireAlloc,
) -> BatchResult {
    let mut items = roots;
    items.extend(names);
    BatchResult {
        lease: grant.lease.clone(),
        seq,
        items,
        packs: Vec::new_in(alloc),
        returned_roots: Set::empty_in(alloc),
        returned_names: Set::empty_in(alloc),
    }
}

fn root_items(grant: &Grant, alloc: WireAlloc) -> WireVec<ItemResult> {
    Vec::from_iter_in(
        Ranges::from_wire(&grant.roots, alloc)
            .unwrap()
            .indexes()
            .map(|i| exported(name(100 + u8::try_from(i).unwrap(), alloc), alloc)),
        alloc,
    )
}

#[test]
fn done_waits_for_the_lead_root_set() {
    let alloc = heap();
    let mut w = set(small(), alloc);
    let rs = rootset(2, 1);
    join(&mut w, 0, "helper", &rs, alloc).unwrap();
    let g = lease(&mut w, 0, "helper", alloc).unwrap();
    let r = result(&g, 1, root_items(&g, alloc), Vec::new_in(alloc), alloc);
    w.result(10, "helper", &r, &mut Vec::new_in(alloc)).unwrap();
    // Every provisional root is terminal, but the lead has not fixed the list.
    assert!(!w.is_done());
    join(&mut w, 20, "lead", &rs, alloc).unwrap();
    assert!(w.is_done());
}

#[test]
fn a_provisional_helper_that_differs_is_fenced_and_the_lead_never_refused() {
    let alloc = heap();
    let mut w = set(small(), alloc);
    let theirs = rootset(3, 1);
    let lead = rootset(2, 2);
    join(&mut w, 0, "helper", &theirs, alloc).unwrap();
    let g = lease(&mut w, 0, "helper", alloc).unwrap();
    let r = result(&g, 1, root_items(&g, alloc), Vec::new_in(alloc), alloc);
    w.result(10, "helper", &r, &mut Vec::new_in(alloc)).unwrap();
    let g2 = lease(&mut w, 11, "helper", alloc).unwrap();
    let mut log = Vec::new_in(alloc);
    let answer = w
        .join(
            20,
            "lead",
            &WorkJoinRequest {
                execution: EXEC,
                attempt: A1,
                roots: lead.clone(),
                rejected: Set::empty_in(alloc),
            },
            &mut log,
        )
        .unwrap();
    assert!(answer.fixed && answer.roots == lead);
    assert!(log.iter().any(|e| matches!(e, Effect::RootsDisagree { .. })));
    assert!(log.iter().any(|e| matches!(e, Effect::Reset { .. })));
    // Its live lease died with it, and its uncommitted result is refused.
    let late = result(&g2, 2, root_items(&g2, alloc), Vec::new_in(alloc), alloc);
    assert!(w.result(21, "helper", &late, &mut Vec::new_in(alloc)).is_err());
    assert_eq!(join(&mut w, 22, "helper", &theirs, alloc), Err(Refusal::FrontEndDiffers));
    // The work restarted on the lead's list: two roots, none terminal.
    let report = w.report();
    assert_eq!(report.roots.len(), 2);
    assert!(report.roots.iter().all(Option::is_none));
    assert_eq!(w.counters().root_disagreements, 1);
}

#[test]
fn a_helper_that_comes_before_the_lead_with_another_root_set_waits() {
    let alloc = heap();
    let mut w = set(small(), alloc);
    join(&mut w, 0, "h1", &rootset(2, 1), alloc).unwrap();
    assert_eq!(join(&mut w, 1, "h2", &rootset(2, 2), alloc), Err(Refusal::NotYet));
    join(&mut w, 2, "lead", &rootset(2, 2), alloc).unwrap();
    assert!(join(&mut w, 3, "h2", &rootset(2, 2), alloc).is_ok());
}

#[test]
fn review_the_leads_rejections_apply_to_provisional_discoveries() {
    let alloc = heap();
    let n7 = name(7, alloc);
    for stage in ["waiting", "held", "exported"] {
        let mut w = set(WorkConfig { grant_roots: 1, ..small() }, alloc);
        let roots = rootset(2, 1);
        join(&mut w, 0, "helper", &roots, alloc).unwrap();
        let first = lease(&mut w, 0, "helper", alloc).unwrap();
        let discovered =
            vec_in![alloc; with_found(exported(name(100, alloc), alloc), &[(n7, 7)], alloc)];
        w.result(
            1,
            "helper",
            &result(&first, 1, discovered, Vec::new_in(alloc), alloc),
            &mut Vec::new_in(alloc),
        )
        .unwrap();
        let pending = (stage != "waiting").then(|| lease(&mut w, 2, "helper", alloc).unwrap());
        if stage == "exported" {
            let grant = pending.as_ref().unwrap();
            let exported_res =
                result(grant, 2, root_items(grant, alloc), vec_in![alloc; exported(n7, alloc)], alloc);
            w.result(3, "helper", &exported_res, &mut Vec::new_in(alloc)).unwrap();
        }

        let mut effects = Vec::new_in(alloc);
        w.join(
            4,
            "lead",
            &WorkJoinRequest {
                execution: EXEC,
                attempt: A1,
                roots,
                rejected: Set::new(vec_in![alloc; n7]),
            },
            &mut effects,
        )
        .unwrap();
        assert_eq!(
            w.report().names[&n7].1,
            Some(Terminal::Tail(TailWhy::Rejected)),
            "the lead's rejection must cover a {stage} provisional discovery"
        );
        if stage == "held" {
            let grant = pending.as_ref().unwrap();
            let exported_res =
                result(grant, 2, root_items(grant, alloc), vec_in![alloc; exported(n7, alloc)], alloc);
            w.result(5, "helper", &exported_res, &mut effects).unwrap();
        }
        if stage != "waiting" {
            assert!(effects.iter().any(|effect| matches!(
                effect,
                Effect::ExportInvalidated { name: invalidated, .. } if *invalidated == n7
            )));
        }
        assert_eq!(w.report().names[&n7].1, Some(Terminal::Tail(TailWhy::Rejected)));
        if let Some(grant) = lease(&mut w, 6, "lead", alloc) {
            assert!(!grant.names.iter().any(|found| *found == n7));
        }
    }
}

#[test]
fn review_an_invalidated_held_name_is_acknowledged_even_when_unknown() {
    let alloc = heap();
    let n7 = name(7, alloc);
    for rejected in [false, true] {
        let mut w = set(WorkConfig { grant_roots: 1, ..small() }, alloc);
        let roots = rootset(3, 1);
        join(&mut w, 0, "helper", &roots, alloc).unwrap();
        let first = lease(&mut w, 0, "helper", alloc).unwrap();
        let discovered =
            vec_in![alloc; with_found(exported(name(100, alloc), alloc), &[(n7, 7)], alloc)];
        w.result(
            1,
            "helper",
            &result(&first, 1, discovered, Vec::new_in(alloc), alloc),
            &mut Vec::new_in(alloc),
        )
        .unwrap();
        let held = lease(&mut w, 2, "helper", alloc).unwrap();
        assert_eq!(held.names.as_slice(), &[n7]);

        w.join(
            3,
            "lead",
            &WorkJoinRequest {
                execution: EXEC,
                attempt: A1,
                roots,
                rejected: if rejected { Set::new(vec_in![alloc; n7]) } else { Set::empty_in(alloc) },
            },
            &mut Vec::new_in(alloc),
        )
        .unwrap();
        let other = lease(&mut w, 4, "lead", alloc).unwrap();
        let other_items = if rejected {
            root_items(&other, alloc)
        } else {
            vec_in![alloc; with_found(exported(name(102, alloc), alloc), &[(n7, 8)], alloc)]
        };
        w.result(
            5,
            "lead",
            &result(&other, 1, other_items, Vec::new_in(alloc), alloc),
            &mut Vec::new_in(alloc),
        )
        .unwrap();

        // The name became terminal, but its batch still owns an unfinished root.
        let late = result(
            &held,
            2,
            root_items(&held, alloc),
            vec_in![alloc; lost(n7, UNKNOWN_NAME, alloc)],
            alloc,
        );
        let answer = w.result(6, "helper", &late, &mut Vec::new_in(alloc)).unwrap();
        assert_eq!(answer.conflicts.as_slice(), &[n7]);
        let reason = if rejected { TailWhy::Rejected } else { TailWhy::Conflict };
        assert_eq!(w.report().names[&n7].1, Some(Terminal::Tail(reason)));
        assert!(w.is_done());
        let commits = w.counters().commits;
        let conflicts = w.counters().conflicts;
        assert_eq!(w.result(7, "helper", &late, &mut Vec::new_in(alloc)).unwrap(), answer);
        assert_eq!(w.counters().commits, commits);
        assert_eq!(w.counters().conflicts, conflicts);
    }
}

#[test]
fn a_dead_generation_is_refused_whole_and_registers_no_names() {
    let alloc = heap();
    let n7 = name(7, alloc);
    let mut w = set(WorkConfig { lease_ms: 1_000, ..small() }, alloc);
    join(&mut w, 0, "lead", &rootset(4, 1), alloc).unwrap();
    join(&mut w, 0, "h", &rootset(4, 1), alloc).unwrap();
    let g = lease(&mut w, 0, "h", alloc).unwrap();
    let mut log = Vec::new_in(alloc);
    w.tick(2_000, &mut log);
    assert!(log.iter().any(|e| matches!(e, Effect::Fenced { why: FenceWhy::Expired, .. })));
    let roots = Vec::from_iter_in(
        root_items(&g, alloc).into_iter().map(|i| with_found(i, &[(n7, 7)], alloc)),
        alloc,
    );
    let late = result(&g, 1, roots, Vec::new_in(alloc), alloc);
    assert_eq!(w.result(2_001, "h", &late, &mut Vec::new_in(alloc)), Err(Refusal::DeadLease));
    assert!(!w.report().names.contains_key(&n7));
}

#[test]
fn a_replay_gets_the_same_answer_before_and_after_a_generation_change() {
    let alloc = heap();
    let n7 = name(7, alloc);
    let mut w = set(small(), alloc);
    join(&mut w, 0, "lead", &rootset(4, 1), alloc).unwrap();
    let g = lease(&mut w, 0, "lead", alloc).unwrap();
    let roots = Vec::from_iter_in(
        root_items(&g, alloc).into_iter().map(|i| with_found(i, &[(n7, 7)], alloc)),
        alloc,
    );
    let r = result(&g, 1, roots, Vec::new_in(alloc), alloc);
    let first = w.result(1, "lead", &r, &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(first.granted.as_slice(), &[n7]);
    assert_eq!(w.result(2, "lead", &r, &mut Vec::new_in(alloc)).unwrap(), first);
    let g2 = lease(&mut w, 3, "lead", alloc).unwrap();
    assert!(g2.lease.generation > g.lease.generation);
    assert_eq!(w.result(4, "lead", &r, &mut Vec::new_in(alloc)).unwrap(), first);
    // The same seq with other content is refused.
    let mut other = r.clone();
    other.items[0] = exported(name(99, alloc), alloc);
    assert_eq!(w.result(5, "lead", &other, &mut Vec::new_in(alloc)), Err(Refusal::SeqReused));
}

#[test]
fn review_a_committed_replay_is_fenced_when_its_attempt_has_moved() {
    let alloc = heap();
    let mut w = set(small(), alloc);
    let roots = rootset(4, 1);
    join(&mut w, 0, "lead", &roots, alloc).unwrap();
    let grant = lease(&mut w, 0, "lead", alloc).unwrap();
    let committed = result(&grant, 1, root_items(&grant, alloc), Vec::new_in(alloc), alloc);
    w.result(1, "lead", &committed, &mut Vec::new_in(alloc)).unwrap();

    w.new_attempt(Attempt(2), "lead", &mut Vec::new_in(alloc));
    join(&mut w, 2, "lead", &roots, alloc).unwrap();
    let current = lease(&mut w, 2, "lead", alloc).unwrap();
    let mut log = Vec::new_in(alloc);
    assert_eq!(w.result(3, "lead", &committed, &mut log), Err(Refusal::AttemptMoved));
    assert!(!log.iter().any(|effect| matches!(effect, Effect::Replayed { .. })));
    assert_eq!(w.counters().replays, 0);
    // A delayed packet from the old attempt cannot replace or complete the current grant.
    assert_eq!(lease(&mut w, 4, "lead", alloc).unwrap(), current);
}

#[test]
fn returned_work_is_back_before_the_next_generation() {
    let alloc = heap();
    let mut w = set(WorkConfig { grant_roots: 4, ..small() }, alloc);
    join(&mut w, 0, "lead", &rootset(4, 1), alloc).unwrap();
    let g = lease(&mut w, 0, "lead", alloc).unwrap();
    let mut r = result(
        &g,
        1,
        vec_in![alloc; exported(name(100, alloc), alloc), exported(name(101, alloc), alloc)],
        Vec::new_in(alloc),
        alloc,
    );
    r.returned_roots = Set::new(vec_in![alloc; (2, 4)]);
    w.result(1, "lead", &r, &mut Vec::new_in(alloc)).unwrap();
    let g2 = lease(&mut w, 2, "lead", alloc).unwrap();
    assert_eq!(g2.roots.as_slice(), &[(2, 4)]);
}

#[test]
fn a_lost_finding_goes_to_another_finder_and_an_unknown_name_to_the_lead() {
    let alloc = heap();
    let n7 = name(7, alloc);
    let n8 = name(8, alloc);
    let mut w = set(small(), alloc);
    let rs = rootset(4, 1);
    join(&mut w, 0, "lead", &rs, alloc).unwrap();
    join(&mut w, 0, "a", &rs, alloc).unwrap();
    join(&mut w, 0, "b", &rs, alloc).unwrap();
    // a finds n and m first; b finds n too, with the same tuple.
    let ga = lease(&mut w, 0, "a", alloc).unwrap();
    let mut items = root_items(&ga, alloc);
    items[0] = with_found(items[0].clone(), &[(n7, 7), (n8, 8)], alloc);
    w.result(1, "a", &result(&ga, 1, items, Vec::new_in(alloc), alloc), &mut Vec::new_in(alloc))
        .unwrap();
    let gb = lease(&mut w, 1, "b", alloc).unwrap();
    let mut items = root_items(&gb, alloc);
    items[0] = with_found(items[0].clone(), &[(n7, 7)], alloc);
    let answer = w
        .result(2, "b", &result(&gb, 1, items, Vec::new_in(alloc), alloc), &mut Vec::new_in(alloc))
        .unwrap();
    assert!(answer.granted.is_empty());
    // a is granted both, loses one finding and does not know the other.
    let ga2 = lease(&mut w, 3, "a", alloc).unwrap();
    assert_eq!(ga2.names.len(), 2);
    let names = Vec::from_iter_in(
        ga2.names.iter().map(|n| {
            if *n == n7 {
                lost(*n, LOST_FINDING, alloc)
            } else {
                lost(*n, UNKNOWN_NAME, alloc)
            }
        }),
        alloc,
    );
    let answer = w
        .result(
            4,
            "a",
            &result(&ga2, 2, root_items(&ga2, alloc), names, alloc),
            &mut Vec::new_in(alloc),
        )
        .unwrap();
    assert_eq!(answer.conflicts.as_slice(), &[n8]);
    let report = w.report();
    assert_eq!(report.names[&n8].1, Some(Terminal::Tail(TailWhy::UnknownName)));
    assert_eq!(report.names[&n7].1, None);
    // n goes to b, its other finder.
    let gb2 = lease(&mut w, 5, "b", alloc).unwrap();
    assert_eq!(gb2.names.as_slice(), &[n7]);
    assert_eq!(w.counters().unknown_names, 1);
    assert_eq!(w.counters().lost_findings, 1);
}

#[test]
fn two_tuples_for_one_name_are_a_conflict_and_invalidate_its_export() {
    let alloc = heap();
    let n7 = name(7, alloc);
    let mut w = set(WorkConfig { grant_roots: 1, ..small() }, alloc);
    let rs = rootset(3, 1);
    join(&mut w, 0, "lead", &rs, alloc).unwrap();
    let g = lease(&mut w, 0, "lead", alloc).unwrap();
    let items = vec_in![alloc; with_found(exported(name(100, alloc), alloc), &[(n7, 7)], alloc)];
    w.result(1, "lead", &result(&g, 1, items, Vec::new_in(alloc), alloc), &mut Vec::new_in(alloc))
        .unwrap();
    let g = lease(&mut w, 2, "lead", alloc).unwrap();
    assert_eq!(g.names.as_slice(), &[n7]);
    w.result(
        3,
        "lead",
        &result(&g, 2, root_items(&g, alloc), vec_in![alloc; exported(n7, alloc)], alloc),
        &mut Vec::new_in(alloc),
    )
    .unwrap();
    let g = lease(&mut w, 4, "lead", alloc).unwrap();
    let items = vec_in![alloc; with_found(exported(name(102, alloc), alloc), &[(n7, 8)], alloc)];
    let mut log = Vec::new_in(alloc);
    let answer = w
        .result(5, "lead", &result(&g, 3, items, Vec::new_in(alloc), alloc), &mut log)
        .unwrap();
    assert_eq!(answer.conflicts.as_slice(), &[n7]);
    assert!(log.iter().any(|e| matches!(e, Effect::ExportInvalidated { .. })));
    assert_eq!(w.report().names[&n7].1, Some(Terminal::Tail(TailWhy::Conflict)));
}

#[test]
fn an_unnamed_request_is_an_edge_and_its_parent_is_local() {
    let alloc = heap();
    let mut w = set(WorkConfig { grant_roots: 1, ..small() }, alloc);
    join(&mut w, 0, "lead", &rootset(1, 1), alloc).unwrap();
    let g = lease(&mut w, 0, "lead", alloc).unwrap();
    let mut item = exported(name(100, alloc), alloc);
    item.unnamed = Set::new(vec_in![alloc; (0, WireString::from_str_in("a const argument", alloc))]);
    w.result(
        1,
        "lead",
        &result(&g, 1, vec_in![alloc; item], Vec::new_in(alloc), alloc),
        &mut Vec::new_in(alloc),
    )
    .unwrap();
    let report = w.report();
    assert!(w.is_done());
    assert_eq!(report.edges.len(), 1);
    let (parent, ordinal, _) = report.edges.values().next().unwrap();
    assert_eq!((*parent, *ordinal), (name(100, alloc), 0));
    assert!(report.roots[0].as_ref().unwrap().is_local());
}

#[test]
fn a_lost_lead_moves_the_attempt_and_its_names_go_to_the_tail() {
    let alloc = heap();
    let n7 = name(7, alloc);
    let mut w = set(WorkConfig { grant_roots: 1, ..small() }, alloc);
    let rs = rootset(2, 1);
    join(&mut w, 0, "lead", &rs, alloc).unwrap();
    join(&mut w, 0, "h", &rs, alloc).unwrap();
    let g = lease(&mut w, 0, "h", alloc).unwrap();
    let items = vec_in![alloc; with_found(exported(name(100, alloc), alloc), &[(n7, 7)], alloc)];
    w.result(1, "h", &result(&g, 1, items, Vec::new_in(alloc), alloc), &mut Vec::new_in(alloc))
        .unwrap();
    let g2 = lease(&mut w, 2, "h", alloc).unwrap();
    let mut log = Vec::new_in(alloc);
    w.new_attempt(Attempt(2), "h", &mut log);
    assert!(log.iter().any(|e| matches!(e, Effect::Fenced { why: FenceWhy::AttemptMoved, .. })));
    let late = result(&g2, 2, root_items(&g2, alloc), vec_in![alloc; exported(n7, alloc)], alloc);
    assert_eq!(w.result(3, "h", &late, &mut Vec::new_in(alloc)), Err(Refusal::AttemptMoved));
    let report = w.report();
    assert_eq!(report.names[&n7].1, Some(Terminal::Tail(TailWhy::LeadLost)));
    // The root committed under the old attempt stays; the other is back in the pool.
    assert!(matches!(report.roots[0], Some(Terminal::Committed { .. })));
    assert_eq!(report.roots[1], None);
    join(&mut w, 4, "h", &rs, alloc).unwrap();
    let g3 = lease(&mut w, 4, "h", alloc).unwrap();
    assert_eq!(g3.roots.as_slice(), &[(1, 2)]);
}

#[test]
fn a_heartbeating_helper_that_commits_nothing_is_revoked_before_any_median() {
    let alloc = heap();
    let mut w = set(small(), alloc);
    let rs = rootset(2, 1);
    join(&mut w, 0, "lead", &rs, alloc).unwrap();
    join(&mut w, 0, "h", &rs, alloc).unwrap();
    lease(&mut w, 0, "h", alloc).unwrap();
    for t in (0..=300_000).step_by(5_000) {
        w.heartbeat(t, "h", A1, &mut Vec::new_in(alloc));
        w.heartbeat(t, "lead", A1, &mut Vec::new_in(alloc));
    }
    let mut log = Vec::new_in(alloc);
    w.heartbeat(300_001, "h", A1, &mut log);
    assert!(log.iter().any(|e| matches!(e, Effect::Fenced { why: FenceWhy::Hung, .. })));
}

#[test]
fn a_root_revoked_three_times_goes_to_the_tail() {
    let alloc = heap();
    let mut w = set(WorkConfig { lease_ms: 1_000, grant_roots: 1, ..small() }, alloc);
    let rs = rootset(1, 1);
    join(&mut w, 0, "lead", &rs, alloc).unwrap();
    let mut t = 0;
    for _ in 0..3 {
        join(&mut w, t, "h", &rs, alloc).unwrap();
        w.heartbeat(t, "lead", A1, &mut Vec::new_in(alloc));
        lease(&mut w, t, "h", alloc).unwrap();
        t += 900;
        w.heartbeat(t, "lead", A1, &mut Vec::new_in(alloc));
        t += 900;
        w.heartbeat(t, "lead", A1, &mut Vec::new_in(alloc));
    }
    assert_eq!(w.report().roots[0], Some(Terminal::Tail(TailWhy::Revoked)));
    assert!(w.is_done());
}

#[test]
fn a_malformed_result_is_refused_and_its_lease_revoked() {
    let alloc = heap();
    let mut w = set(small(), alloc);
    join(&mut w, 0, "lead", &rootset(2, 1), alloc).unwrap();
    let g = lease(&mut w, 0, "lead", alloc).unwrap();
    // One item for two roots.
    let r = result(&g, 1, vec_in![alloc; exported(name(100, alloc), alloc)], Vec::new_in(alloc), alloc);
    assert!(matches!(w.result(1, "lead", &r, &mut Vec::new_in(alloc)), Err(Refusal::Malformed(_))));
    let g2 = lease(&mut w, 2, "lead", alloc).unwrap();
    assert_eq!(g2.roots.as_slice(), &[(0, 2)]);
    assert!(g2.lease.generation > g.lease.generation);
}
