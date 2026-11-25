//! The lease and work named by both sides of one granted compilation.

use super::{BackendKind, BatchError, Emission, Plan, Unit, wire_name};
use crate::telemetry::json;
use blaze_own::{BTreeMap, Vec};
use blaze_wire::{Digest, WireAlloc, WireString, WireVec};

type Fields = WireVec<(&'static str, WireString)>;

pub(super) fn issued(unit: &Unit, plan: &Plan, roots: &[usize], pid: u32, alloc: WireAlloc) -> Result<Fields, BatchError> {
    let mut work = Vec::with_capacity_in(roots.len().saturating_add(plan.grant.names.len()), alloc);
    for &at in roots {
        let name = unit.names.get(at).ok_or(BatchError::RootIndex { unit: unit.spec, index: u64::try_from(at).unwrap_or(u64::MAX) })?;
        work.push(wire_name(&unit.spec, name, alloc).0);
    }
    work.extend(plan.grant.names.iter().map(|name| name.0));
    let lease = &plan.grant.lease;
    let mut fields = Vec::with_capacity_in(11, alloc);
    fields.extend([
        ("pid", json(&pid, alloc)),
        ("unit_digest", json(&unit.spec, alloc)),
        ("execution", json(&lease.execution.0, alloc)),
        ("attempt", json(&lease.attempt.0, alloc)),
        ("holder", json(&lease.holder, alloc)),
        ("generation", json(&lease.generation, alloc)),
        ("seq", json(&plan.seq, alloc)),
        ("work", json(&work, alloc)),
    ]);
    Ok(fields)
}

pub(super) fn backend(fields: &mut Fields, unit: Digest, emissions: &[Emission], alloc: WireAlloc) -> Result<(), BatchError> {
    let mut raw = Vec::with_capacity_in(emissions.len(), alloc);
    let mut kinds = BTreeMap::new_in(alloc);
    let mut release = Vec::new_in(alloc);
    let mut folded = Vec::new_in(alloc);
    for emission in emissions {
        raw.push(emission.key);
        if let Some(kind) = emission.backend {
            match kinds.insert(emission.key, kind) {
                Some(previous) if previous != kind => return Err(BatchError::ConflictingEmission { unit, key: emission.key }),
                Some(_) => {}
                None => match kind {
                    BackendKind::ReleaseBlob => release.push(emission.key),
                    BackendKind::FoldedOutput => folded.push(emission.key),
                },
            }
        }
    }
    fields.extend([
        ("compiled_keys", json(&raw, alloc)),
        ("release_keys", json(&release, alloc)),
        ("folded_keys", json(&folded, alloc)),
    ]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_own::vec_in;
    use blaze_wire::{Attempt, ExecutionId, Lease, Set, WorkName};

    #[test]
    fn backend_evidence_names_the_issued_lease_and_only_observed_output_kinds() {
        let alloc = blaze_own::testing::heap().shared();
        let unit = Unit { spec: [1; 32], names: vec_in![alloc; [2; 32], [3; 32]] };
        let plan = Plan {
            grant: blaze_wire::Grant {
                lease: Lease { execution: ExecutionId(7), attempt: Attempt(2), holder: WireString::from_str_in("helper", alloc), generation: 5 },
                roots: Set::new(vec_in![alloc; (1, 2)]),
                names: Set::new(vec_in![alloc; WorkName([4; 32])]),
            },
            seq: 8,
            compile: Vec::new_in(alloc),
        };
        let mut fields = issued(&unit, &plan, &[1], 31, alloc).unwrap();
        let work = vec_in![alloc; wire_name(&unit.spec, &[3; 32], alloc).0, [4; 32]];
        assert_ne!(work[0], unit.names[1]);
        let actual = |key, kind| Emission { key: [key; 32], backend: kind };
        backend(&mut fields, unit.spec, &[
            actual(11, None), actual(12, Some(BackendKind::ReleaseBlob)),
            actual(13, Some(BackendKind::FoldedOutput)), actual(12, Some(BackendKind::ReleaseBlob)),
        ], alloc).unwrap();
        let digest = json(&unit.spec, alloc);
        let work = json(&work, alloc);
        let raw = json(&vec_in![alloc; [11u8; 32], [12; 32], [13; 32], [12; 32]], alloc);
        let release = json(&vec_in![alloc; [12u8; 32]], alloc);
        let folded = json(&vec_in![alloc; [13u8; 32]], alloc);
        let expected = [
            ("pid", "31"), ("unit_digest", digest.as_str()), ("execution", "7"), ("attempt", "2"),
            ("holder", "\"helper\""), ("generation", "5"), ("seq", "8"), ("work", work.as_str()),
            ("compiled_keys", raw.as_str()), ("release_keys", release.as_str()), ("folded_keys", folded.as_str()),
        ];
        assert!(fields.iter().map(|(k, v)| (*k, v.as_str())).eq(expected));

        let mut plain = Vec::new_in(alloc);
        backend(&mut plain, unit.spec, &[actual(11, None)], alloc).unwrap();
        assert_eq!(plain[1].1, "[]");
        assert_eq!(plain[2].1, "[]");
        let conflict = backend(&mut plain, unit.spec, &[
            actual(12, Some(BackendKind::ReleaseBlob)), actual(12, Some(BackendKind::FoldedOutput)),
        ], alloc);
        assert!(matches!(conflict, Err(BatchError::ConflictingEmission { unit: u, key: k }) if u == unit.spec && k == [12; 32]));
    }
}
