//! Features used to select targets, including enabled dependency features.

use std::collections::BTreeSet;

use blaze_resolve::resolver::{CompileKind, FeaturesFor, PackageTargets, ResolvedFeatures};
use blaze_resolve::{DepKind, PackageId, Resolve};
use rustc_hash::FxHashMap;

use crate::summary::Read;

/// Cargo 1.99's `resolve_all_features` (`5f94df4789`,
/// `src/ops/cargo_compile/mod.rs`). Target requirements include the package's
/// normal features and enabled dependency features under their manifest names.
/// Host dependencies use their host feature domain, never the folded metadata.
pub(crate) fn available(
    pkg: PackageId,
    resolve: &Resolve,
    features: &ResolvedFeatures,
    reads: &FxHashMap<PackageId, Read>,
    dev_units: bool,
    targets: &dyn PackageTargets,
    target_kind: CompileKind,
) -> BTreeSet<String> {
    let mut available: BTreeSet<_> = features
        .activated_features_unverified(pkg, FeaturesFor::NormalOrDev)
        .unwrap_or_default()
        .into_iter()
        .map(|feature| feature.to_string())
        .collect();
    for (dep_id, deps) in resolve.deps(pkg) {
        let Some(read) = reads.get(&dep_id) else { continue };
        // Cargo filters package edges before enumerating their dependency declarations.
        if !deps.iter().any(|dep| {
            (dev_units || dep.kind() != DepKind::Development)
                && dep.platform().is_none_or(|platform| {
                    targets.platform_activated(platform, target_kind)
                        || targets.platform_activated(platform, CompileKind::Host)
                })
        }) {
            continue;
        }
        for dep in deps {
            let domain =
                FeaturesFor::from_for_host(read.any_proc_macro || dep.kind() == DepKind::Build);
            for feature in
                features.activated_features_unverified(dep_id, domain).unwrap_or_default()
            {
                available.insert(format!("{}/{feature}", dep.name_in_toml()));
            }
        }
    }
    available
}
