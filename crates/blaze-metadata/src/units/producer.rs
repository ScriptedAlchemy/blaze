//! Source units before metadata folds host and target feature domains together.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result};
use blaze_resolve::resolver::{CompileKind, FeaturesFor, PackageTargets, ResolvedFeatures};
use blaze_resolve::{DepKind, PackageId, Resolve};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::summary::Read;

/// A library or selected package unit. Profiles and script execution belong to the
/// build planner; this graph preserves resolution and dependency domains.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ResolvedUnit {
    pub package: String,
    /// Host dependency feature domain, not merely whether the target triple is native.
    pub host: bool,
    pub features: Vec<String>,
    pub proc_macro: bool,
    pub build_script: bool,
    pub dependencies: Vec<UnitDependency>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct UnitDependency {
    pub name: String,
    pub unit: usize,
    /// Build dependencies compile the script, not the package's library.
    pub build: bool,
}

pub(crate) fn of_build(
    resolve: &Resolve,
    features: &ResolvedFeatures,
    roots: &[PackageId],
    dev_units: bool,
    reads: &FxHashMap<PackageId, Read>,
    targets: &dyn PackageTargets,
    target_kind: CompileKind,
    root_features: &FxHashMap<PackageId, BTreeSet<String>>,
) -> Result<Vec<ResolvedUnit>> {
    let mut graph = BTreeMap::new();
    let mut seen = FxHashSet::default();
    let mut queue: Vec<_> =
        roots.iter().map(|&id| (id, reads.get(&id).is_some_and(|it| it.proc_macro_lib))).collect();
    while let Some((pkg, host)) = queue.pop() {
        if !seen.insert((pkg, host)) {
            continue;
        }
        let fk = FeaturesFor::from_for_host(host);
        let on = features
            .activated_features_unverified(pkg, fk)
            .with_context(|| format!("no resolved features for ({pkg}, {fk})"))?;
        let read = &reads[&pkg];
        if !dev_units
            && roots.contains(&pkg)
            && read.lib.is_none()
            && !read.bins.iter().any(|required| {
                required.iter().all(|it| root_features.get(&pkg).is_some_and(|on| on.contains(it)))
            })
        {
            continue;
        }
        let mut edges = Vec::new();
        for (dep_id, deps) in resolve.deps(pkg) {
            let Some(dep_read) = reads.get(&dep_id) else { continue };
            let Some(lib) = dep_read.lib else { continue };
            for dep in deps {
                let build = dep.kind() == DepKind::Build;
                match dep.kind() {
                    DepKind::Development if !roots.contains(&pkg) || !dev_units => continue,
                    DepKind::Build if !read.build_script => continue,
                    _ => {}
                }
                let kind = if host || build { CompileKind::Host } else { target_kind };
                if dep.platform().is_some_and(|p| !targets.platform_activated(p, kind)) {
                    continue;
                }
                if dep.is_optional() && !features.is_dep_activated(pkg, fk, dep.name_in_toml()) {
                    continue;
                }
                let (name, _) =
                    resolve.extern_crate_name_and_dep_name(pkg, dep_id, lib.as_str())?;
                let child_host = host || build || dep_read.proc_macro_lib;
                edges.push((name.to_string(), (dep_id, child_host), build));
                queue.push((dep_id, child_host));
            }
        }
        edges.sort();
        edges.dedup();
        graph.insert((pkg, host), (on, edges));
    }
    let slots: BTreeMap<_, _> = graph.keys().enumerate().map(|(i, key)| (*key, i)).collect();
    graph
        .into_iter()
        .map(|((pkg, host), (features, edges))| {
            let dependencies = edges
                .into_iter()
                .map(|(name, key, build)| {
                    let unit = *slots.get(&key).with_context(|| {
                        format!("a resolved dependency has no compile unit: {}", key.0)
                    })?;
                    Ok(UnitDependency { name, unit, build })
                })
                .collect::<Result<_>>()?;
            Ok(ResolvedUnit {
                package: pkg.to_spec().to_string(),
                host,
                features: features.into_iter().map(|it| it.to_string()).collect(),
                proc_macro: reads[&pkg].proc_macro_lib,
                build_script: reads[&pkg].build_script,
                dependencies,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_resolve::resolver::{
        CliFeatures, FeatureOpts, FeatureResolver, ForceAllTargets, HasDevUnits, ResolveBehavior,
        ResolveOpts, ResolveSettings, VersionPreferences, resolve,
    };
    use blaze_resolve::{Cfg, IndexSummary, MemRegistry, Platform};
    use serde_json::json;

    fn package(
        name: &str,
        macro_lib: bool,
        deps: serde_json::Value,
        features: serde_json::Value,
    ) -> Read {
        crate::summary::read(&json!({
            "name": name, "version": "0.1.0", "manifest_path": format!("/units/{name}/Cargo.toml"),
            "dependencies": deps, "features": features,
            "targets": [{"name": name, "kind": [if macro_lib { "proc-macro" } else { "lib" }]}],
        }))
        .unwrap()
    }

    fn dep(
        name: &str,
        rename: Option<&str>,
        optional: bool,
        target: Option<&str>,
        features: &[&str],
    ) -> serde_json::Value {
        json!({
            "name": name, "path": format!("/units/{name}"), "req": "*",
            "rename": rename, "optional": optional, "target": target,
            "uses_default_features": false, "features": features,
        })
    }

    #[test]
    fn units_preserve_the_resolvers_host_features_optional_activation_and_predicates() {
        let app = package(
            "app",
            false,
            json!([
                dep("derive", None, false, None, &[]),
                dep("shared", None, false, None, &["target"]),
            ]),
            json!({}),
        );
        let derive = package(
            "derive",
            true,
            json!([dep("shared", None, false, None, &["host", "optional"]),]),
            json!({}),
        );
        let shared = package(
            "shared",
            false,
            json!([dep("extra", Some("renamed"), true, Some("cfg(target_os = \"macos\")"), &[]),]),
            json!({"host": [], "target": [], "optional": ["dep:renamed"]}),
        );
        let extra = package("extra", false, json!([]), json!({}));
        let app_id = app.summary.package_id();
        let shared_id = shared.summary.package_id();
        let reads: FxHashMap<_, _> = [app, derive, shared, extra]
            .into_iter()
            .map(|read| (read.summary.package_id(), read))
            .collect();
        let mut registry = MemRegistry::new();
        for read in reads.values() {
            registry.add(IndexSummary::Candidate(read.summary.clone()));
        }
        let resolved = resolve(
            &[(reads[&app_id].summary.clone(), ResolveOpts::everything())],
            &[],
            &registry,
            &VersionPreferences::default(),
            Default::default(),
            &ResolveSettings::default(),
        )
        .unwrap();
        let cfg: Vec<Cfg> = vec!["target_os=\"macos\"".parse().unwrap()];
        let targets = crate::Packages {
            reads: &reads,
            host: Some(&crate::TargetPlatform { triple: "aarch64-apple-darwin", cfg: &cfg }),
            platform: Some(&crate::TargetPlatform {
                triple: "aarch64-unknown-linux-gnu",
                cfg: &[],
            }),
        };
        let cli = CliFeatures::from_command_line(&[], false, true).unwrap();
        for behavior in [ResolveBehavior::V1, ResolveBehavior::V2, ResolveBehavior::V3] {
            let features = FeatureResolver::resolve(
                &targets,
                &resolved,
                &[(&reads[&app_id].summary, cli.clone())],
                &[CompileKind::Target("aarch64-unknown-linux-gnu".into())],
                FeatureOpts::new(behavior, HasDevUnits::No, ForceAllTargets::No),
            )
            .unwrap();
            let units = of_build(
                &resolved,
                &features,
                &[app_id],
                false,
                &reads,
                &targets,
                CompileKind::Target("aarch64-unknown-linux-gnu".into()),
                &FxHashMap::default(),
            )
            .unwrap();
            for unit in &units {
                let pkg = reads.keys().find(|id| id.to_spec().to_string() == unit.package).unwrap();
                let on: Vec<String> = features
                    .activated_features(*pkg, FeaturesFor::from_for_host(unit.host))
                    .into_iter()
                    .map(|it| it.to_string())
                    .collect();
                assert_eq!(unit.features, on, "the resolver, not folded metadata, owns features");
            }
            let host = units
                .iter()
                .find(|u| u.package == shared_id.to_spec().to_string() && u.host)
                .unwrap();
            let target = units
                .iter()
                .find(|u| u.package == shared_id.to_spec().to_string() && !u.host)
                .unwrap();
            match behavior {
                ResolveBehavior::V1 => assert_eq!(host.features, target.features),
                _ => {
                    assert_eq!(host.features, ["host", "optional"]);
                    assert_eq!(target.features, ["target"]);
                }
            }
            assert_eq!(host.dependencies.len(), 1);
            assert_eq!(host.dependencies[0].name, "renamed");
            assert!(units[host.dependencies[0].unit].host);
            assert!(target.dependencies.is_empty(), "the target predicate is not the host's");
            assert!(PackageTargets::platform_activated(
                &targets,
                &"cfg(target_os = \"macos\")".parse::<Platform>().unwrap(),
                CompileKind::Host,
            ));
        }
    }
}
