//! Cache hits and newly compiled entries obey the same admission decisions.

use core::fmt;

use blaze_own::Vec;
use blaze_sim::{Broken, Config, Point, Tape};

use super::*;

#[derive(Debug)]
enum AdmissionBroken {
    EntryNotInvalidated,
}

impl fmt::Display for AdmissionBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a rejected or conflicting entry was not invalidated")
    }
}

blaze_sim::plants! {
    enum AdmissionPlant breaks AdmissionBroken {
        None,
        DropInvalidation => AdmissionBroken::EntryNotInvalidated,
    }
}

blaze_sim::points! {
    enum AdmissionPoint {
        CachedBeforeRejection,
        CachedAfterRejection,
        CachedBeforeConflict,
        CachedAfterConflict,
        Exported,
    }
}

fn invalidation(tape: &Tape, plant: AdmissionPlant) -> Result<(), Broken<AdmissionBroken>> {
    let alloc = heap();
    let cached = tape.coin("entry came from the store");
    let committed_before = tape.coin("entry committed before the refusal");
    let rejected = tape.coin("the lead rejects the name");
    tape.reach(match (cached, committed_before, rejected) {
        (true, true, true) => AdmissionPoint::CachedBeforeRejection,
        (true, false, true) => AdmissionPoint::CachedAfterRejection,
        (true, true, false) => AdmissionPoint::CachedBeforeConflict,
        (true, false, false) => AdmissionPoint::CachedAfterConflict,
        (false, _, _) => AdmissionPoint::Exported,
    });
    let n7 = name(7, alloc);
    let mut w = set(WorkConfig { grant_roots: 1, ..small() }, alloc);
    let roots = rootset(3, 1);
    join(&mut w, 0, "helper", &roots, alloc).unwrap();
    let first = lease(&mut w, 0, "helper", alloc).unwrap();
    let found = vec_in![alloc; with_found(exported(name(100, alloc), alloc), &[(n7, 7)], alloc)];
    w.result(
        1,
        "helper",
        &result(&first, 1, found, Vec::new_in(alloc), alloc),
        &mut Vec::new_in(alloc),
    )
    .unwrap();
    let pending = lease(&mut w, 2, "helper", alloc).unwrap();
    assert_eq!(pending.names.as_slice(), &[n7]);
    let mut item = exported(n7, alloc);
    if cached {
        item.outcome = ItemOutcome::FromStore(EntryRef { entry: EntryKey(n7.0) });
    }
    let named_result =
        result(&pending, 2, root_items(&pending, alloc), vec_in![alloc; item], alloc);
    if committed_before {
        w.result(3, "helper", &named_result, &mut Vec::new_in(alloc)).unwrap();
    }
    let mut effects = Vec::new_in(alloc);
    let refusal = if rejected {
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
        TailWhy::Rejected
    } else {
        join(&mut w, 4, "other", &roots, alloc).unwrap();
        let other = lease(&mut w, 4, "other", alloc).unwrap();
        let conflicting =
            vec_in![alloc; with_found(exported(name(102, alloc), alloc), &[(n7, 8)], alloc)];
        w.result(
            5,
            "other",
            &result(&other, 1, conflicting, Vec::new_in(alloc), alloc),
            &mut effects,
        )
        .unwrap();
        TailWhy::Conflict
    };
    if !committed_before {
        w.result(6, "helper", &named_result, &mut effects).unwrap();
    }
    assert_eq!(w.report().names[&n7].1, Some(Terminal::Tail(refusal)));
    assert!(w.committed_names(&named_result, alloc).iter().all(|name| *name != n7));
    if matches!(plant, AdmissionPlant::DropInvalidation) {
        effects.retain(|effect| !matches!(effect, Effect::ExportInvalidated { .. }));
    }
    if !effects.iter().any(|effect| matches!(
        effect,
        Effect::ExportInvalidated { name, entry } if *name == n7 && *entry == EntryKey(n7.0)
    )) {
        return Err(Broken::Oracle(AdmissionBroken::EntryNotInvalidated));
    }
    Ok(())
}

/// The invariant: an entry excluded by a name conflict or lead rejection is invalidated whether compiled or read from the store, before or after its batch commits.
#[test]
fn cached_and_compiled_entries_share_invalidation() {
    blaze_sim::check(
        Config::cases(128).must_reach_points(AdmissionPoint::IDS),
        |tape| invalidation(tape, AdmissionPlant::None),
    );
}

#[test]
fn the_oracle_catches_an_entry_without_invalidation() {
    blaze_sim::controls(Config::cases(128), invalidation);
}
