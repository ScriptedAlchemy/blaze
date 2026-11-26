//! Receipts associate generated machine code with the lease and work that authorized it.

use super::{BackendKind, BatchError, Emission, NativeEmission, Plan, Unit, wire_name};
use crate::telemetry::json;
use blaze_own::{BTreeMap, Vec};
use blaze_wire::{Digest, WireAlloc, WireString, WireVec};

type Fields = WireVec<(&'static str, WireString)>;

pub(super) fn issued(unit: &Unit, plan: &Plan, roots: &[usize], pid: u32, alloc: WireAlloc) -> Result<Fields, BatchError> {
    let mut work = Vec::with_capacity_in(roots.len().saturating_add(plan.grant.names.len()), alloc);
    for &at in roots {
        let name = unit.names.get(at).ok_or(BatchError::RootIndex {
            unit: unit.spec,
            index: u64::try_from(at).unwrap_or(u64::MAX),
        })?;
        work.push(wire_name(&unit.spec, name, alloc).0);
    }
    work.extend(plan.grant.names.iter().map(|name| name.0));
    let lease = &plan.grant.lease;
    let mut fields = Vec::with_capacity_in(12, alloc);
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

pub(super) fn native_outputs(
    unit: &Unit,
    roots: &[usize],
    emissions: &[Emission],
    alloc: WireAlloc,
) -> Result<WireVec<(Digest, NativeEmission)>, BatchError> {
    let mut native = Vec::new_in(alloc);
    if emissions.iter().all(|output| output.native.is_none()) {
        return Ok(native);
    }

    let mut seen = BTreeMap::new_in(alloc);
    for output in emissions {
        match seen.insert(output.key, output.native) {
            Some(previous) if previous != output.native => {
                return Err(BatchError::ConflictingEmission { unit: unit.spec, key: output.key });
            }
            Some(_) => continue,
            None => {}
        }
        let Some(origin) = output.native else { continue };
        if !roots.iter().any(|&at| unit.names.get(at) == Some(&origin.grant)) {
            return Err(BatchError::EmissionGrant { unit: unit.spec, grant: origin.grant });
        }
        native.push((output.key, origin));
    }
    Ok(native)
}

pub(super) fn backend(fields: &mut Fields, unit: Digest, emissions: &[Emission], native: &[(Digest, NativeEmission)], alloc: WireAlloc) {
    let raw = Vec::from_iter_in(emissions.iter().map(|output| output.key), alloc);
    let mut outputs = Vec::with_capacity_in(native.len(), alloc);
    let mut release = Vec::new_in(alloc);
    let mut folded = Vec::new_in(alloc);
    for &(key, output) in native {
        outputs.push((wire_name(&unit, &output.grant, alloc).0, key, output.content));
        match output.kind {
            BackendKind::ReleaseBlob => release.push(key),
            BackendKind::FoldedOutput => folded.push(key),
        }
    }
    fields.extend([
        ("compiled_keys", json(&raw, alloc)),
        ("release_keys", json(&release, alloc)),
        ("folded_keys", json(&folded, alloc)),
        ("native_outputs", json(&outputs, alloc)),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_own::vec_in;
    use blaze_wire::{Attempt, ExecutionId, Lease, Set, WorkName};

    #[test]
    fn backend_evidence_names_only_outputs_of_the_issued_lease() {
        let alloc = blaze_own::testing::heap().shared();
        let unit = Unit { spec: [1; 32], names: vec_in![alloc; [2; 32], [3; 32]] };
        let plan = Plan {
            grant: blaze_wire::Grant {
                lease: Lease {
                    execution: ExecutionId(7),
                    attempt: Attempt(2),
                    holder: WireString::from_str_in("helper", alloc),
                    generation: 5,
                },
                roots: Set::new(vec_in![alloc; (1, 2)]),
                names: Set::new(vec_in![alloc; WorkName([4; 32])]),
            },
            seq: 8,
            compile: Vec::new_in(alloc),
        };
        let mut fields = issued(&unit, &plan, &[1], 31, alloc).unwrap();
        let work = vec_in![alloc; wire_name(&unit.spec, &[3; 32], alloc).0, [4; 32]];
        let actual = |key, kind| Emission {
            key: [key; 32],
            native: Some(NativeEmission { kind, grant: [3; 32], content: [14; 32] }),
        };
        let release = actual(12, BackendKind::ReleaseBlob);
        let folded = actual(13, BackendKind::FoldedOutput);
        let events = [Emission::plain([11; 32]), release, folded];
        let outputs = native_outputs(&unit, &[1], &events, alloc).unwrap();
        backend(&mut fields, unit.spec, &events, &outputs, alloc);
        assert_eq!(fields[2].1.as_str(), "7");
        assert_eq!(fields[6].1.as_str(), "8");
        assert_eq!(fields[7].1, json(&work, alloc));
        assert_eq!(fields[8].1, json(&vec_in![alloc; [11u8; 32], [12; 32], [13; 32]], alloc));
        assert_eq!(fields[9].1, json(&vec_in![alloc; [12u8; 32]], alloc));
        assert_eq!(fields[10].1, json(&vec_in![alloc; [13u8; 32]], alloc));
        assert_eq!(fields[11].1, json(&vec_in![alloc; (work[0], [12u8; 32], [14; 32]), (work[0], [13; 32], [14; 32])], alloc));

        let wrong_grant = native_outputs(&unit, &[0], &events, alloc);
        assert!(matches!(wrong_grant, Err(BatchError::EmissionGrant { grant, .. }) if grant == [3; 32]));
        let conflicting = native_outputs(&unit, &[1], &[release, actual(12, BackendKind::FoldedOutput)], alloc);
        assert!(matches!(conflicting, Err(BatchError::ConflictingEmission { key, .. }) if key == [12; 32]));
    }
}
