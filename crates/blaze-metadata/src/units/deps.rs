//! The dependencies of each unit: cargo 1.99.0's `compiler/unit_dependencies.rs`, ported, less
//! the standard library's units (`-Zbuild-std`) and artifact dependencies (`-Zbindeps`), and the
//! documentation units. Roots go in, and every unit they need comes out with the units it is
//! compiled against, a library's build script, and for the programs `cargo test` runs, the
//! binaries of their package.

use anyhow::Result;
use blaze_manifest::profiles::UnitFor;
use blaze_manifest::targets::Target;
use blaze_resolve::resolver::{FeaturesFor, HasDevUnits};
use blaze_resolve::{Dependency, PackageId};
use rustc_hash::FxHashSet;

use super::model::{Arena, CMode, Edge, Env, Inner, Kind};
use super::select::is_linkable;

/// The units and, for each, the units it depends on, in cargo's order.
pub(super) struct Graph {
    pub arena: Arena,
    pub deps: Vec<Vec<Edge>>,
}

/// cargo's `build_unit_dependencies`.
pub(super) fn build(env: &Env<'_>, arena: Arena, roots: &[usize]) -> Result<Graph> {
    let mut state = State { env, arena, deps: Vec::new(), dev_edges: FxHashSet::default() };
    state.deps_of_roots(roots)?;
    state.connect_run_custom_build_deps();
    // Dependencies are listed in the order of the units, so that the graph is the same on
    // every run.
    let State { arena, mut deps, .. } = state;
    for list in deps.iter_mut().flatten() {
        list.sort_by(|a, b| {
            env.cmp_units(&arena.units[a.unit], &arena.units[b.unit])
                .then_with(|| a.name.as_str().cmp(b.name.as_str()))
        });
    }
    let deps = deps.into_iter().map(Option::unwrap_or_default).collect();
    Ok(Graph { arena, deps })
}

struct State<'e, 'a> {
    env: &'e Env<'a>,
    arena: Arena,
    /// The dependencies of each unit, once computed.
    deps: Vec<Option<Vec<Edge>>>,
    /// The edges (parent, dependency) that are there only for being a dev-dependency.
    dev_edges: FxHashSet<(usize, usize)>,
}

/// The mode a dependency of a unit in `mode` is built in.
fn check_or_build_mode(mode: CMode, target: &Target) -> CMode {
    match mode {
        CMode::Check { .. } => {
            if target.for_host {
                // A proc macro is compiled for real: it is run.
                CMode::Build
            } else {
                // A regular dependency is not checked with `--test`.
                CMode::Check { test: false }
            }
        }
        _ => CMode::Build,
    }
}

impl<'a> State<'_, 'a> {
    fn sync(&mut self) {
        self.deps.resize(self.arena.units.len(), None);
    }

    fn deps_of_roots(&mut self, roots: &[usize]) -> Result<()> {
        for &root in roots {
            let unit = self.arena.units[root].clone();
            let target = self.env.target(&unit);
            // The dependencies of a test or a bench have no `panic` set, so that a library is
            // not built three times (with `panic`, without, and for `--test`).
            let unit_for = if unit.mode.is_any_test() || self.env.intent.is_rustc_test() {
                if target.proc_macro {
                    // A proc macro is for the host, to link with `proc_macro`.
                    UnitFor::new_host_test()
                } else {
                    UnitFor::new_test()
                }
            } else if target.is_custom_build() {
                UnitFor::new_host(false)
            } else if target.proc_macro {
                UnitFor::new_host(true)
            } else if target.for_host {
                UnitFor::new_compiler()
            } else {
                UnitFor::new_normal()
            };
            self.deps_of(root, unit_for)?;
        }
        Ok(())
    }

    /// The dependencies of `unit` and, recursively, theirs. A unit is walked once, as whatever it
    /// was first reached for.
    fn deps_of(&mut self, unit: usize, unit_for: UnitFor) -> Result<()> {
        self.sync();
        if self.deps[unit].is_some() {
            return Ok(());
        }
        let edges = self.compute_deps(unit, unit_for)?;
        self.sync();
        self.deps[unit] = Some(edges.clone());
        for edge in edges {
            self.deps_of(edge.unit, edge.unit_for)?;
        }
        Ok(())
    }

    fn compute_deps(&mut self, u: usize, unit_for: UnitFor) -> Result<Vec<Edge>> {
        let unit = self.arena.units[u].clone();
        if unit.mode == CMode::RunCustomBuild {
            return self.compute_deps_custom_build(&unit, unit_for);
        }
        let env = self.env;
        let target = env.target(&unit);
        let mut ret = Vec::new();
        let mut dev_deps = Vec::new();
        for (dep_id, deps) in self.deps(&unit, target, unit_for) {
            let manifest = env.pkg(dep_id).manifest;
            let Some(lib_at) = manifest.targets.iter().position(Target::is_lib) else { continue };
            let dep_lib = &manifest.targets[lib_at];
            let mode = check_or_build_mode(unit.mode, dep_lib);
            let dep_unit_for = unit_for.with_dependency(
                target.is_custom_build(),
                dep_lib.for_host,
                dep_lib.proc_macro,
            );
            let edge = self.new_unit_dep(
                &unit,
                dep_id,
                lib_at,
                dep_unit_for,
                unit.kind.for_target(dep_lib),
                mode,
            )?;
            // A dev-dependency's edge is remembered, to leave it out of the `links` connections.
            if deps.iter().all(|d| !d.is_transitive()) {
                dev_deps.push((u, edge.unit));
            }
            ret.push(edge);
        }
        self.dev_edges.extend(dev_deps);

        // A build script is compiled against its build-dependencies alone. Anything else is
        // run after its package's build script.
        if target.is_custom_build() {
            return Ok(ret);
        }
        ret.extend(self.dep_build_script(&unit, unit_for)?);

        // A binary, test or example is compiled against the library of its package, which
        // `resolve.deps` does not list.
        if target.is_lib() {
            return Ok(ret);
        }
        ret.extend(self.maybe_lib(&unit, unit_for)?);

        // The integration tests and benchmarks run the package's binaries: they are built too.
        if unit.mode == CMode::Test
            && (super::select::is_test(target) || super::select::is_bench(target))
        {
            let pkg = env.pkg(unit.pkg);
            for (at, t) in pkg.manifest.targets.iter().enumerate() {
                let built = match &t.required_features {
                    // A binary whose required features have not been selected is not built.
                    Some(required) if t.is_bin() => {
                        let features = env.all_features(unit.pkg, HasDevUnits::No);
                        required.iter().all(|f| features.contains(f))
                    }
                    None if t.is_bin() => true,
                    _ => false,
                };
                if built {
                    let edge = self.new_unit_dep(
                        &unit,
                        unit.pkg,
                        at,
                        UnitFor::new_normal(),
                        unit.kind.for_target(t),
                        CMode::Build,
                    )?;
                    ret.push(edge);
                }
            }
        }
        Ok(ret)
    }

    /// The dependencies of the build script's run: compiling it. Running a build script after
    /// those of the packages it depends on that have a `links` key is added at the end.
    fn compute_deps_custom_build(&mut self, unit: &Inner, unit_for: UnitFor) -> Result<Vec<Edge>> {
        // All its dependencies use the profile of build scripts: for the host, a proc macro's
        // build script with the host's features.
        let script_unit_for = unit_for.for_custom_build();
        let compile = self.new_unit_dep(
            unit,
            unit.pkg,
            unit.target,
            script_unit_for,
            // Build scripts are always compiled for the host.
            Kind::Host,
            CMode::Build,
        )?;
        Ok(vec![compile])
    }

    /// The units a unit is compiled against that `resolve.deps` lists: those of its kind that its
    /// features, its platform and what the command builds leave in.
    fn deps(
        &self,
        unit: &Inner,
        target: &Target,
        unit_for: UnitFor,
    ) -> Vec<(PackageId, Vec<&'a Dependency>)> {
        let env = self.env;
        let features_for = if unit_for.is_for_host_features() {
            FeaturesFor::HostDep
        } else {
            FeaturesFor::NormalOrDev
        };
        let mut out = Vec::new();
        for (id, deps) in env.resolve.deps(unit.pkg) {
            let deps: Vec<&Dependency> = deps
                .iter()
                .filter(|dep| {
                    // A build script has its build-dependencies, anything else all but those.
                    if target.is_custom_build() != dep.is_build() {
                        return false;
                    }
                    // A dev-dependency is for tests and examples only.
                    if !dep.is_transitive()
                        && !super::select::is_test(target)
                        && !super::select::is_example(target)
                        && !unit.mode.is_any_test()
                    {
                        return false;
                    }
                    if !env.platform_activated(dep) {
                        return false;
                    }
                    // An optional dependency the feature resolver did not enable is left out.
                    if dep.is_optional()
                        && !env.features.is_dep_activated(
                            unit.pkg,
                            features_for,
                            dep.name_in_toml(),
                        )
                    {
                        return false;
                    }
                    true
                })
                .collect();
            if !deps.is_empty() {
                out.push((id, deps));
            }
        }
        out
    }

    /// The run of the package's build scripts, if it has any.
    fn dep_build_script(&mut self, unit: &Inner, unit_for: UnitFor) -> Result<Vec<Edge>> {
        let env = self.env;
        let pkg = env.pkg(unit.pkg);
        let mut out = Vec::new();
        for (at, target) in pkg.manifest.targets.iter().enumerate() {
            if !target.is_custom_build() {
                continue;
            }
            // The profile stored in a unit is that of what the script is run for.
            let profile = env.profiles.get_profile_run_custom_build(&unit.profile);
            // The script is for the host in all its dependencies, itself included, so that
            // they get the build override. With the host's features if the package is a build
            // dependency or a proc macro, the target's if it is a normal dependency.
            let script_unit_for = unit_for.for_custom_build();
            out.push(self.new_unit_dep_with_profile(
                unit,
                unit.pkg,
                at,
                script_unit_for,
                unit.kind,
                CMode::RunCustomBuild,
                profile,
            )?);
        }
        Ok(out)
    }

    /// The library of the unit's package, which a binary, test or example is compiled against.
    fn maybe_lib(&mut self, unit: &Inner, unit_for: UnitFor) -> Result<Option<Edge>> {
        let env = self.env;
        let manifest = env.pkg(unit.pkg).manifest;
        let Some(at) = manifest.targets.iter().position(is_linkable) else { return Ok(None) };
        let lib = &manifest.targets[at];
        let mode = check_or_build_mode(unit.mode, lib);
        let target = env.target(unit);
        let dep_unit_for =
            unit_for.with_dependency(target.is_custom_build(), lib.for_host, lib.proc_macro);
        let edge =
            self.new_unit_dep(unit, unit.pkg, at, dep_unit_for, unit.kind.for_target(lib), mode)?;
        Ok(Some(edge))
    }

    fn new_unit_dep(
        &mut self,
        parent: &Inner,
        pkg: PackageId,
        target: usize,
        unit_for: UnitFor,
        kind: Kind,
        mode: CMode,
    ) -> Result<Edge> {
        let profile = self.env.profile(pkg, unit_for);
        self.new_unit_dep_with_profile(parent, pkg, target, unit_for, kind, mode, profile)
    }

    #[allow(clippy::too_many_arguments, reason = "cargo's signature")]
    fn new_unit_dep_with_profile(
        &mut self,
        parent: &Inner,
        pkg: PackageId,
        target: usize,
        unit_for: UnitFor,
        kind: Kind,
        mode: CMode,
        profile: blaze_manifest::profiles::Profile,
    ) -> Result<Edge> {
        let env = self.env;
        let t = &env.pkg(pkg).manifest.targets[target];
        let crate_name = t.name.replace('-', "_");
        let (name, dep_name) =
            env.resolve.extern_crate_name_and_dep_name(parent.pkg, pkg, &crate_name)?;
        let features_for = if unit_for.is_for_host_features() {
            FeaturesFor::HostDep
        } else {
            FeaturesFor::NormalOrDev
        };
        let Some(features) = env.features.activated_features_unverified(pkg, features_for) else {
            anyhow::bail!("no features were resolved for `{pkg}` as {features_for:?}")
        };
        let unit = self.arena.intern(Inner { pkg, target, profile, kind, mode, features });
        Ok(Edge { unit, unit_for, name, dep_name })
    }

    /// Makes the run of a build script depend on those of the build scripts of the packages its
    /// package depends on that have a `links` key, as `cargo` runs them in that order.
    fn connect_run_custom_build_deps(&mut self) {
        self.sync();
        let env = self.env;
        let units = &self.arena.units;
        // Each build script's run, and what depends on it.
        let mut reverse: rustc_hash::FxHashMap<usize, Vec<usize>> =
            rustc_hash::FxHashMap::default();
        for (unit, edges) in self.deps.iter().enumerate() {
            for edge in edges.iter().flatten() {
                if units[edge.unit].mode == CMode::RunCustomBuild {
                    let parents = reverse.entry(edge.unit).or_default();
                    if !parents.contains(&unit) {
                        parents.push(unit);
                    }
                }
            }
        }
        let mut new_deps: Vec<(usize, Vec<Edge>)> = Vec::new();
        for (run, unit) in units.iter().enumerate() {
            if unit.mode != CMode::RunCustomBuild {
                continue;
            }
            let Some(parents) = reverse.get(&run) else { continue };
            let mut to_add: Vec<Edge> = Vec::new();
            for &parent in parents {
                for other in self.deps[parent].iter().flatten() {
                    let other_unit = &units[other.unit];
                    // The package's siblings that are libraries with a `links` key.
                    if other_unit.pkg == unit.pkg
                        || !is_linkable(env.target(other_unit))
                        || env.resolve.summary(other_unit.pkg).links().is_none()
                    {
                        continue;
                    }
                    // Only normal dependencies: the dev-dependencies can make cycles.
                    if self.dev_edges.contains(&(parent, other.unit)) {
                        continue;
                    }
                    // The run of its build script.
                    let script = self.deps[other.unit]
                        .iter()
                        .flatten()
                        .find(|dep| units[dep.unit].mode == CMode::RunCustomBuild);
                    if let Some(script) = script {
                        let mut edge = script.clone();
                        let name = other.dep_name.unwrap_or(other_unit.pkg.name());
                        edge.dep_name = Some(name);
                        if !to_add.contains(&edge) {
                            to_add.push(edge);
                        }
                    }
                }
            }
            if !to_add.is_empty() {
                new_deps.push((run, to_add));
            }
        }
        for (run, edges) in new_deps {
            self.deps[run].get_or_insert_with(Vec::new).extend(edges);
        }
    }
}
