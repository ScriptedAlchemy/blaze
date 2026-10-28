//! The work set's transitions one at a time, as D0's acceptance lists them (c3-B2.1 to
//! c3-B2.3, rb-M3): the root agreement, fencing, replays, returned work, finders, conflicts,
//! unknown names, lead loss, the progress timeout and the revocation bound.

use blaze_dist::work::{
    Effect, FenceWhy, LOST_FINDING, TailWhy, Terminal, UNKNOWN_NAME, WorkConfig, WorkSet,
};
use blaze_dist::{Ranges, Refusal};
use blaze_wire::{
    Attempt, BatchResult, EntryKey, EntryRef, ExecutionId, Found, Grant, ItemKey, ItemOutcome,
    ItemResult, RootSet, Set, Why, WorkJoinRequest, WorkKey, WorkLeaseRequest, WorkName,
};

const EXEC: ExecutionId = ExecutionId(1);
const A1: Attempt = Attempt(1);

fn rootset(count: u64, tag: u8) -> RootSet {
    RootSet { spec: [1; 32], roots: [tag; 32], count, index: [3; 32], inputs: [4; 32] }
}

fn name(tag: u8) -> WorkName {
    WorkName::of(&WorkKey::Item(ItemKey { unit: [9; 32], stable: [tag; 32], env: [0; 32] }))
}

fn set(config: WorkConfig) -> WorkSet {
    WorkSet::new(EXEC, A1, "lead", config)
}

fn small() -> WorkConfig {
    WorkConfig { grant_roots: 2, grant_names: 4, ..WorkConfig::default() }
}

fn join(
    w: &mut WorkSet,
    now: u64,
    who: &str,
    roots: &RootSet,
) -> Result<blaze_wire::WorkJoinAnswer, Refusal> {
    let req = WorkJoinRequest {
        execution: EXEC,
        attempt: w.attempt(),
        roots: roots.clone(),
        rejected: Set::new(Vec::new()),
    };
    w.join(now, who, &req, &mut Vec::new())
}

fn lease(w: &mut WorkSet, now: u64, who: &str) -> Option<Grant> {
    let req = WorkLeaseRequest { execution: EXEC, attempt: w.attempt() };
    w.lease(now, who, &req, &mut Vec::new()).expect("lease").grant
}

fn exported(n: WorkName) -> ItemResult {
    ItemResult {
        name: n,
        outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey(n.0) }),
        found: Set::new(Vec::new()),
        unnamed: Set::new(Vec::new()),
    }
}

fn with_found(mut item: ItemResult, found: &[(WorkName, u8)]) -> ItemResult {
    item.found = Set::new(found.iter().map(|&(name, t)| Found { name, tuple: [t; 32] }).collect());
    item
}

fn lost(n: WorkName, why: &str) -> ItemResult {
    ItemResult {
        name: n,
        outcome: ItemOutcome::Lost(Why { why: why.to_owned() }),
        found: Set::new(Vec::new()),
        unnamed: Set::new(Vec::new()),
    }
}

/// A result for `grant` with items for every root (named by index) and every name.
fn result(grant: &Grant, seq: u64, roots: Vec<ItemResult>, names: Vec<ItemResult>) -> BatchResult {
    let mut items = roots;
    items.extend(names);
    BatchResult {
        lease: grant.lease.clone(),
        seq,
        items,
        packs: Vec::new(),
        returned_roots: Set::new(Vec::new()),
        returned_names: Set::new(Vec::new()),
    }
}

fn root_items(grant: &Grant) -> Vec<ItemResult> {
    Ranges::from_wire(&grant.roots)
        .unwrap()
        .indexes()
        .map(|i| exported(name(100 + i as u8)))
        .collect()
}

#[test]
fn done_waits_for_the_lead_root_set() {
    let mut w = set(small());
    let rs = rootset(2, 1);
    join(&mut w, 0, "helper", &rs).unwrap();
    let g = lease(&mut w, 0, "helper").unwrap();
    let r = result(&g, 1, root_items(&g), Vec::new());
    w.result(10, "helper", &r, &mut Vec::new()).unwrap();
    // Every provisional root is terminal, but the lead has not fixed the list.
    assert!(!w.is_done());
    join(&mut w, 20, "lead", &rs).unwrap();
    assert!(w.is_done());
}

#[test]
fn a_provisional_helper_that_differs_is_fenced_and_the_lead_never_refused() {
    let mut w = set(small());
    let theirs = rootset(3, 1);
    let lead = rootset(2, 2);
    join(&mut w, 0, "helper", &theirs).unwrap();
    let g = lease(&mut w, 0, "helper").unwrap();
    let r = result(&g, 1, root_items(&g), Vec::new());
    w.result(10, "helper", &r, &mut Vec::new()).unwrap();
    let g2 = lease(&mut w, 11, "helper").unwrap();
    let mut log = Vec::new();
    let answer = w
        .join(
            20,
            "lead",
            &WorkJoinRequest {
                execution: EXEC,
                attempt: A1,
                roots: lead.clone(),
                rejected: Set::new(Vec::new()),
            },
            &mut log,
        )
        .unwrap();
    assert!(answer.fixed && answer.roots == lead);
    assert!(log.iter().any(|e| matches!(e, Effect::RootsDisagree { .. })));
    assert!(log.iter().any(|e| matches!(e, Effect::Reset { .. })));
    // Its live lease died with it, and its uncommitted result is refused.
    let late = result(&g2, 2, root_items(&g2), Vec::new());
    assert!(w.result(21, "helper", &late, &mut Vec::new()).is_err());
    assert_eq!(join(&mut w, 22, "helper", &theirs), Err(Refusal::FrontEndDiffers));
    // The work restarted on the lead's list: two roots, none terminal.
    let report = w.report();
    assert_eq!(report.roots.len(), 2);
    assert!(report.roots.iter().all(Option::is_none));
    assert_eq!(w.counters().root_disagreements, 1);
}

#[test]
fn a_helper_that_comes_before_the_lead_with_another_root_set_waits() {
    let mut w = set(small());
    join(&mut w, 0, "h1", &rootset(2, 1)).unwrap();
    assert_eq!(join(&mut w, 1, "h2", &rootset(2, 2)), Err(Refusal::NotYet));
    join(&mut w, 2, "lead", &rootset(2, 2)).unwrap();
    assert!(join(&mut w, 3, "h2", &rootset(2, 2)).is_ok());
}

#[test]
fn review_the_leads_rejections_apply_to_provisional_discoveries() {
    for stage in ["waiting", "held", "exported"] {
        let mut w = set(WorkConfig { grant_roots: 1, ..small() });
        let roots = rootset(2, 1);
        join(&mut w, 0, "helper", &roots).unwrap();
        let first = lease(&mut w, 0, "helper").unwrap();
        let discovered = vec![with_found(exported(name(100)), &[(name(7), 7)])];
        w.result(1, "helper", &result(&first, 1, discovered, Vec::new()), &mut Vec::new()).unwrap();
        let pending = (stage != "waiting").then(|| lease(&mut w, 2, "helper").unwrap());
        if stage == "exported" {
            let grant = pending.as_ref().unwrap();
            let exported = result(grant, 2, root_items(grant), vec![exported(name(7))]);
            w.result(3, "helper", &exported, &mut Vec::new()).unwrap();
        }

        let mut effects = Vec::new();
        w.join(
            4,
            "lead",
            &WorkJoinRequest {
                execution: EXEC,
                attempt: A1,
                roots,
                rejected: Set::new(vec![name(7)]),
            },
            &mut effects,
        )
        .unwrap();
        assert_eq!(
            w.report().names[&name(7)].1,
            Some(Terminal::Tail(TailWhy::Rejected)),
            "the lead's rejection must cover a {stage} provisional discovery"
        );
        if stage == "held" {
            let grant = pending.as_ref().unwrap();
            let exported = result(grant, 2, root_items(grant), vec![exported(name(7))]);
            w.result(5, "helper", &exported, &mut effects).unwrap();
        }
        if stage != "waiting" {
            assert!(effects.iter().any(|effect| matches!(
                effect,
                Effect::ExportInvalidated { name: invalidated, .. } if *invalidated == name(7)
            )));
        }
        assert_eq!(w.report().names[&name(7)].1, Some(Terminal::Tail(TailWhy::Rejected)));
        if let Some(grant) = lease(&mut w, 6, "lead") {
            assert!(!grant.names.iter().any(|found| *found == name(7)));
        }
    }
}

#[test]
fn review_an_invalidated_held_name_is_acknowledged_even_when_unknown() {
    for rejected in [false, true] {
        let mut w = set(WorkConfig { grant_roots: 1, ..small() });
        let roots = rootset(3, 1);
        join(&mut w, 0, "helper", &roots).unwrap();
        let first = lease(&mut w, 0, "helper").unwrap();
        let discovered = vec![with_found(exported(name(100)), &[(name(7), 7)])];
        w.result(1, "helper", &result(&first, 1, discovered, Vec::new()), &mut Vec::new()).unwrap();
        let held = lease(&mut w, 2, "helper").unwrap();
        assert_eq!(held.names.as_slice(), &[name(7)]);

        w.join(
            3,
            "lead",
            &WorkJoinRequest {
                execution: EXEC,
                attempt: A1,
                roots,
                rejected: Set::new(if rejected { vec![name(7)] } else { Vec::new() }),
            },
            &mut Vec::new(),
        )
        .unwrap();
        let other = lease(&mut w, 4, "lead").unwrap();
        let other_items = if rejected {
            root_items(&other)
        } else {
            vec![with_found(exported(name(102)), &[(name(7), 8)])]
        };
        w.result(5, "lead", &result(&other, 1, other_items, Vec::new()), &mut Vec::new()).unwrap();

        // The name became terminal, but its batch still owns an unfinished root.
        let late = result(&held, 2, root_items(&held), vec![lost(name(7), UNKNOWN_NAME)]);
        let answer = w.result(6, "helper", &late, &mut Vec::new()).unwrap();
        assert_eq!(answer.conflicts.as_slice(), &[name(7)]);
        let reason = if rejected { TailWhy::Rejected } else { TailWhy::Conflict };
        assert_eq!(w.report().names[&name(7)].1, Some(Terminal::Tail(reason)));
        assert!(w.is_done());
        let commits = w.counters().commits;
        let conflicts = w.counters().conflicts;
        assert_eq!(w.result(7, "helper", &late, &mut Vec::new()).unwrap(), answer);
        assert_eq!(w.counters().commits, commits);
        assert_eq!(w.counters().conflicts, conflicts);
    }
}

#[test]
fn a_dead_generation_is_refused_whole_and_registers_no_names() {
    let mut w = set(WorkConfig { lease_ms: 1_000, ..small() });
    join(&mut w, 0, "lead", &rootset(4, 1)).unwrap();
    join(&mut w, 0, "h", &rootset(4, 1)).unwrap();
    let g = lease(&mut w, 0, "h").unwrap();
    let mut log = Vec::new();
    w.tick(2_000, &mut log);
    assert!(log.iter().any(|e| matches!(e, Effect::Fenced { why: FenceWhy::Expired, .. })));
    let roots = root_items(&g).into_iter().map(|i| with_found(i, &[(name(7), 7)])).collect();
    let late = result(&g, 1, roots, Vec::new());
    assert_eq!(w.result(2_001, "h", &late, &mut Vec::new()), Err(Refusal::DeadLease));
    assert!(!w.report().names.contains_key(&name(7)));
}

#[test]
fn a_replay_gets_the_same_answer_before_and_after_a_generation_change() {
    let mut w = set(small());
    join(&mut w, 0, "lead", &rootset(4, 1)).unwrap();
    let g = lease(&mut w, 0, "lead").unwrap();
    let roots = root_items(&g).into_iter().map(|i| with_found(i, &[(name(7), 7)])).collect();
    let r = result(&g, 1, roots, Vec::new());
    let first = w.result(1, "lead", &r, &mut Vec::new()).unwrap();
    assert_eq!(first.granted.as_slice(), &[name(7)]);
    assert_eq!(w.result(2, "lead", &r, &mut Vec::new()).unwrap(), first);
    let g2 = lease(&mut w, 3, "lead").unwrap();
    assert!(g2.lease.generation > g.lease.generation);
    assert_eq!(w.result(4, "lead", &r, &mut Vec::new()).unwrap(), first);
    // The same seq with other content is refused.
    let mut other = r.clone();
    other.items[0] = exported(name(99));
    assert_eq!(w.result(5, "lead", &other, &mut Vec::new()), Err(Refusal::SeqReused));
}

#[test]
fn review_a_committed_replay_is_fenced_when_its_attempt_has_moved() {
    let mut w = set(small());
    let roots = rootset(4, 1);
    join(&mut w, 0, "lead", &roots).unwrap();
    let grant = lease(&mut w, 0, "lead").unwrap();
    let committed = result(&grant, 1, root_items(&grant), Vec::new());
    w.result(1, "lead", &committed, &mut Vec::new()).unwrap();

    w.new_attempt(Attempt(2), "lead", &mut Vec::new());
    join(&mut w, 2, "lead", &roots).unwrap();
    let current = lease(&mut w, 2, "lead").unwrap();
    let mut log = Vec::new();
    assert_eq!(w.result(3, "lead", &committed, &mut log), Err(Refusal::AttemptMoved));
    assert!(!log.iter().any(|effect| matches!(effect, Effect::Replayed { .. })));
    assert_eq!(w.counters().replays, 0);
    // A delayed packet from the old attempt cannot replace or complete the current grant.
    assert_eq!(lease(&mut w, 4, "lead").unwrap(), current);
}

#[test]
fn returned_work_is_back_before_the_next_generation() {
    let mut w = set(WorkConfig { grant_roots: 4, ..small() });
    join(&mut w, 0, "lead", &rootset(4, 1)).unwrap();
    let g = lease(&mut w, 0, "lead").unwrap();
    let mut r = result(&g, 1, vec![exported(name(100)), exported(name(101))], Vec::new());
    r.returned_roots = Set::new(vec![(2, 4)]);
    w.result(1, "lead", &r, &mut Vec::new()).unwrap();
    let g2 = lease(&mut w, 2, "lead").unwrap();
    assert_eq!(g2.roots.as_slice(), &[(2, 4)]);
}

#[test]
fn a_lost_finding_goes_to_another_finder_and_an_unknown_name_to_the_lead() {
    let mut w = set(small());
    let rs = rootset(4, 1);
    join(&mut w, 0, "lead", &rs).unwrap();
    join(&mut w, 0, "a", &rs).unwrap();
    join(&mut w, 0, "b", &rs).unwrap();
    // a finds n and m first; b finds n too, with the same tuple.
    let ga = lease(&mut w, 0, "a").unwrap();
    let mut items = root_items(&ga);
    items[0] = with_found(items[0].clone(), &[(name(7), 7), (name(8), 8)]);
    w.result(1, "a", &result(&ga, 1, items, Vec::new()), &mut Vec::new()).unwrap();
    let gb = lease(&mut w, 1, "b").unwrap();
    let mut items = root_items(&gb);
    items[0] = with_found(items[0].clone(), &[(name(7), 7)]);
    let answer = w.result(2, "b", &result(&gb, 1, items, Vec::new()), &mut Vec::new()).unwrap();
    assert!(answer.granted.is_empty());
    // a is granted both, loses one finding and does not know the other.
    let ga2 = lease(&mut w, 3, "a").unwrap();
    assert_eq!(ga2.names.len(), 2);
    let names: Vec<ItemResult> = ga2
        .names
        .iter()
        .map(|n| if *n == name(7) { lost(*n, LOST_FINDING) } else { lost(*n, UNKNOWN_NAME) })
        .collect();
    let answer =
        w.result(4, "a", &result(&ga2, 2, root_items(&ga2), names), &mut Vec::new()).unwrap();
    assert_eq!(answer.conflicts.as_slice(), &[name(8)]);
    let report = w.report();
    assert_eq!(report.names[&name(8)].1, Some(Terminal::Tail(TailWhy::UnknownName)));
    assert_eq!(report.names[&name(7)].1, None);
    // n goes to b, its other finder.
    let gb2 = lease(&mut w, 5, "b").unwrap();
    assert_eq!(gb2.names.as_slice(), &[name(7)]);
    assert_eq!(w.counters().unknown_names, 1);
    assert_eq!(w.counters().lost_findings, 1);
}

#[test]
fn two_tuples_for_one_name_are_a_conflict_and_invalidate_its_export() {
    let mut w = set(WorkConfig { grant_roots: 1, ..small() });
    let rs = rootset(3, 1);
    join(&mut w, 0, "lead", &rs).unwrap();
    let g = lease(&mut w, 0, "lead").unwrap();
    let items = vec![with_found(exported(name(100)), &[(name(7), 7)])];
    w.result(1, "lead", &result(&g, 1, items, Vec::new()), &mut Vec::new()).unwrap();
    let g = lease(&mut w, 2, "lead").unwrap();
    assert_eq!(g.names.as_slice(), &[name(7)]);
    w.result(3, "lead", &result(&g, 2, root_items(&g), vec![exported(name(7))]), &mut Vec::new())
        .unwrap();
    let g = lease(&mut w, 4, "lead").unwrap();
    let items = vec![with_found(exported(name(102)), &[(name(7), 8)])];
    let mut log = Vec::new();
    let answer = w.result(5, "lead", &result(&g, 3, items, Vec::new()), &mut log).unwrap();
    assert_eq!(answer.conflicts.as_slice(), &[name(7)]);
    assert!(log.iter().any(|e| matches!(e, Effect::ExportInvalidated { .. })));
    assert_eq!(w.report().names[&name(7)].1, Some(Terminal::Tail(TailWhy::Conflict)));
}

#[test]
fn an_unnamed_request_is_an_edge_and_its_parent_is_local() {
    let mut w = set(WorkConfig { grant_roots: 1, ..small() });
    join(&mut w, 0, "lead", &rootset(1, 1)).unwrap();
    let g = lease(&mut w, 0, "lead").unwrap();
    let mut item = exported(name(100));
    item.unnamed = Set::new(vec![(0, "a const argument".to_owned())]);
    w.result(1, "lead", &result(&g, 1, vec![item], Vec::new()), &mut Vec::new()).unwrap();
    let report = w.report();
    assert!(w.is_done());
    assert_eq!(report.edges.len(), 1);
    let (parent, ordinal, _) = report.edges.values().next().unwrap();
    assert_eq!((*parent, *ordinal), (name(100), 0));
    assert!(report.roots[0].as_ref().unwrap().is_local());
}

#[test]
fn a_lost_lead_moves_the_attempt_and_its_names_go_to_the_tail() {
    let mut w = set(WorkConfig { grant_roots: 1, ..small() });
    let rs = rootset(2, 1);
    join(&mut w, 0, "lead", &rs).unwrap();
    join(&mut w, 0, "h", &rs).unwrap();
    let g = lease(&mut w, 0, "h").unwrap();
    let items = vec![with_found(exported(name(100)), &[(name(7), 7)])];
    w.result(1, "h", &result(&g, 1, items, Vec::new()), &mut Vec::new()).unwrap();
    let g2 = lease(&mut w, 2, "h").unwrap();
    let mut log = Vec::new();
    w.new_attempt(Attempt(2), "h", &mut log);
    assert!(log.iter().any(|e| matches!(e, Effect::Fenced { why: FenceWhy::AttemptMoved, .. })));
    let late = result(&g2, 2, root_items(&g2), vec![exported(name(7))]);
    assert_eq!(w.result(3, "h", &late, &mut Vec::new()), Err(Refusal::AttemptMoved));
    let report = w.report();
    assert_eq!(report.names[&name(7)].1, Some(Terminal::Tail(TailWhy::LeadLost)));
    // The root committed under the old attempt stays; the other is back in the pool.
    assert!(matches!(report.roots[0], Some(Terminal::Committed { .. })));
    assert_eq!(report.roots[1], None);
    join(&mut w, 4, "h", &rs).unwrap();
    let g3 = lease(&mut w, 4, "h").unwrap();
    assert_eq!(g3.roots.as_slice(), &[(1, 2)]);
}

#[test]
fn a_heartbeating_helper_that_commits_nothing_is_revoked_before_any_median() {
    let mut w = set(small());
    let rs = rootset(2, 1);
    join(&mut w, 0, "lead", &rs).unwrap();
    join(&mut w, 0, "h", &rs).unwrap();
    lease(&mut w, 0, "h").unwrap();
    for t in (0..=300_000).step_by(5_000) {
        w.heartbeat(t, "h", A1, &mut Vec::new());
        w.heartbeat(t, "lead", A1, &mut Vec::new());
    }
    let mut log = Vec::new();
    w.heartbeat(300_001, "h", A1, &mut log);
    assert!(log.iter().any(|e| matches!(e, Effect::Fenced { why: FenceWhy::Hung, .. })));
}

#[test]
fn a_root_revoked_three_times_goes_to_the_tail() {
    let mut w = set(WorkConfig { lease_ms: 1_000, grant_roots: 1, ..small() });
    let rs = rootset(1, 1);
    join(&mut w, 0, "lead", &rs).unwrap();
    let mut t = 0;
    for _ in 0..3 {
        join(&mut w, t, "h", &rs).unwrap();
        w.heartbeat(t, "lead", A1, &mut Vec::new());
        lease(&mut w, t, "h").unwrap();
        t += 900;
        w.heartbeat(t, "lead", A1, &mut Vec::new());
        t += 900;
        w.heartbeat(t, "lead", A1, &mut Vec::new());
    }
    assert_eq!(w.report().roots[0], Some(Terminal::Tail(TailWhy::Revoked)));
    assert!(w.is_done());
}

#[test]
fn a_malformed_result_is_refused_and_its_lease_revoked() {
    let mut w = set(small());
    join(&mut w, 0, "lead", &rootset(2, 1)).unwrap();
    let g = lease(&mut w, 0, "lead").unwrap();
    // One item for two roots.
    let r = result(&g, 1, vec![exported(name(100))], Vec::new());
    assert!(matches!(w.result(1, "lead", &r, &mut Vec::new()), Err(Refusal::Malformed(_))));
    let g2 = lease(&mut w, 2, "lead").unwrap();
    assert_eq!(g2.roots.as_slice(), &[(0, 2)]);
    assert!(g2.lease.generation > g.lease.generation);
}
