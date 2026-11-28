//! The current inputs to semantic read verification. The checked answer's storage root
//! also encodes its generation and cause history, which belong to the local home.

use blaze_ids::{ContentId, Hasher};

use crate::{engine_check::Standing, run_ids::RunIds, run_names::Program, sweep::Leaf};

const DOMAIN: &str = "blaze distributed dev verification inputs 1";

fn add_leaf(hash: &mut Hasher, path: &str, leaf: &Leaf) {
    let (flags, content) = leaf.state();
    hash.update(&[leaf.kind as u8, flags]).part(path.as_bytes()).id(&content);
}

pub(crate) fn context(standing: &Standing<'_, '_>, program: &Program, ids: &RunIds<'_>) -> ContentId {
    let mut hash = Hasher::new(DOMAIN);
    hash.id(&standing.producers.digest()).update(&standing.toolchain.0);
    for tool in &standing.tools {
        hash.id(&tool.0);
    }
    program.hash_export_inputs(&mut hash);
    ids.hash_export_inputs(&mut hash);
    // Leaves are validated on open in (kind, logical path) order. The sweep used
    // stat stamps to establish these contents locally; only their values travel.
    for index in 0..standing.leaves.len() {
        add_leaf(&mut hash, standing.leaves.path(index), &standing.leaves.row(index));
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::{LeafKind, Stamp};

    #[test]
    fn leaf_identity_survives_a_different_checkout_but_tracks_its_source() {
        let digest = |path, leaf: &Leaf| {
            let mut hash = Hasher::new(DOMAIN);
            add_leaf(&mut hash, path, leaf);
            hash.finish()
        };
        let mut leaf = Leaf { kind: LeafKind::Slot, path: 0, flags: 0, stamp: None, id: ContentId([1; 32]) };
        let before = digest("src/lib.rs", &leaf);
        leaf.path = 42;
        leaf.stamp = Some(Stamp { len: 1, mtime_ns: 2, ctime_ns: 3, ino: 4, dev: 5 });
        assert_eq!(digest("src/lib.rs", &leaf), before);
        assert_ne!(digest("other.rs", &leaf), before);
        leaf.id = ContentId([2; 32]);
        assert_ne!(digest("src/lib.rs", &leaf), before);
    }
}
