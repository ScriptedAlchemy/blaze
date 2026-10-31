//! What the unit graph is made of while it is built: cargo's `Unit` as an index into an arena,
//! its interner, and the order cargo gives units (`UnitInner`'s derived `Ord`), which decides
//! the order of the roots and of each unit's dependencies.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use blaze_manifest::config::CompileKind;
use blaze_manifest::manifest::Manifest;
use blaze_manifest::profiles::{DebugInfo, Lto, Profile, Strip, StripInner, UnitFor};
use blaze_manifest::targets::{CrateType, Target, TargetKind};
use blaze_resolve::resolver::{FeaturesFor, HasDevUnits, ResolvedFeatures};
use blaze_resolve::{DepKind, Dependency, InternedString, PackageId, Resolve};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{Intent, ProfilePkg};
use crate::TargetPlatform;

/// What a unit is compiled for. Without `--target` every unit is for the host, and cargo keeps
/// the target's own kind (`Target`: the host's triple named) apart from `Host` until it has
/// walked the whole graph, so that a build dependency and a normal one do not collide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Kind {
    Host,
    Target,
}

impl Kind {
    /// cargo's `CompileKind::for_target`: a build script or a proc macro is for the host.
    pub(super) fn for_target(self, target: &Target) -> Kind {
        if target.for_host { Kind::Host } else { self }
    }
}

/// cargo's `CompileMode`, less what only documentation and doc tests use. The order is cargo's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum CMode {
    Test,
    Build,
    Check { test: bool },
    RunCustomBuild,
}

impl CMode {
    pub(super) fn is_check(self) -> bool {
        matches!(self, CMode::Check { .. })
    }

    /// cargo's `is_any_test`: compiled with `--test`.
    pub(super) fn is_any_test(self) -> bool {
        matches!(self, CMode::Test | CMode::Check { test: true })
    }
}

/// A unit before the graph is shared: cargo's `UnitInner`, less the flags of the platform, which
/// are the same for every unit.
#[derive(Clone, Debug)]
pub(super) struct Inner {
    pub pkg: PackageId,
    /// The index in the package's `Manifest::targets`.
    pub target: usize,
    pub profile: Profile,
    pub kind: Kind,
    pub mode: CMode,
    pub features: Vec<InternedString>,
}

type Key = (PackageId, usize, Kind, CMode, Vec<InternedString>);

/// cargo's `UnitInterner`: units equal in everything, profile included, are one.
#[derive(Default)]
pub(super) struct Arena {
    pub units: Vec<Inner>,
    index: FxHashMap<Key, Vec<usize>>,
}

impl Arena {
    pub(super) fn intern(&mut self, unit: Inner) -> usize {
        let key = (unit.pkg, unit.target, unit.kind, unit.mode, unit.features.clone());
        let bucket = self.index.entry(key).or_default();
        if let Some(&same) = bucket.iter().find(|&&at| self.units[at].profile == unit.profile) {
            return same;
        }
        bucket.push(self.units.len());
        self.units.push(unit);
        self.units.len() - 1
    }

    /// The unit equal to `unit` in the arena, if there is one.
    pub(super) fn find(&self, unit: &Inner) -> Option<usize> {
        let key = (unit.pkg, unit.target, unit.kind, unit.mode, unit.features.clone());
        let bucket = self.index.get(&key)?;
        bucket.iter().copied().find(|&at| self.units[at].profile == unit.profile)
    }
}

/// An edge of the graph: cargo's `UnitDep`, less what stable cargo never sets (`public`,
/// `noprelude` and `nounused` are false, artifact dependencies are not read).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Edge {
    pub unit: usize,
    /// What the dependency is for, which decides its profile and its dependencies' features.
    pub unit_for: UnitFor,
    /// The name the parent's code uses for it.
    pub name: InternedString,
    /// The name in the parent's manifest, for the edges between build script runs.
    pub dep_name: Option<InternedString>,
}

/// A package as the unit graph reads it.
pub(super) struct Pkg<'a> {
    pub manifest: &'a Manifest,
    /// The directory of the manifest.
    pub dir: &'a Path,
    /// A member of the workspace.
    pub member: bool,
    /// From a path source: the only packages whose warnings are shown, and whose profiles are
    /// incremental.
    pub local: bool,
    /// Where the package comes from, as [`super::UnitPackage::source`] says.
    pub source: String,
}

/// What the graph is made from.
pub(super) struct Env<'a> {
    pub resolve: &'a Resolve,
    pub features: &'a ResolvedFeatures,
    pub profiles: &'a blaze_manifest::profiles::Profiles,
    pub packages: FxHashMap<PackageId, Pkg<'a>>,
    pub platform: &'a TargetPlatform<'a>,
    pub intent: Intent,
}

impl<'a> Env<'a> {
    pub(super) fn pkg(&self, id: PackageId) -> &Pkg<'a> {
        &self.packages[&id]
    }

    pub(super) fn target(&self, unit: &Inner) -> &'a Target {
        &self.packages[&unit.pkg].manifest.targets[unit.target]
    }

    /// cargo's `Package::proc_macro`: any of the package's targets is a proc macro.
    pub(super) fn any_proc_macro(&self, id: PackageId) -> bool {
        self.pkg(id).manifest.targets.iter().any(|it| it.proc_macro)
    }

    /// The profile of a unit of `id` that is `unit_for`, as cargo's `Profiles::get_profile`.
    pub(super) fn profile(&self, id: PackageId, unit_for: UnitFor) -> Profile {
        let pkg = self.pkg(id);
        let package = ProfilePkg(&pkg.manifest.package_id);
        // Without `--target`, the host's own triple is what every unit is built for.
        self.profiles.get_profile(&package, pkg.member, pkg.local, unit_for, &CompileKind::Host)
    }

    /// cargo's `resolve_all_features`: the features enabled for `pkg`, and each of its
    /// dependencies' as `dep/feature`, so that `required-features` can name them.
    pub(super) fn all_features(
        &self,
        pkg: PackageId,
        has_dev_units: HasDevUnits,
    ) -> FxHashSet<String> {
        let features = self.features.activated_features_unverified(pkg, FeaturesFor::NormalOrDev);
        let mut all: FxHashSet<String> =
            features.into_iter().flatten().map(|it| it.to_string()).collect();
        for (dep_id, deps) in self.resolve.deps(pkg) {
            // cargo's `PackageSet::filter_deps`: the package is a dependency if any one of its
            // dependencies is built.
            let wanted = deps.iter().any(|dep| {
                (dep.kind() != DepKind::Development || has_dev_units == HasDevUnits::Yes)
                    && self.platform_activated(dep)
            });
            if !wanted {
                continue;
            }
            let is_proc_macro = self.any_proc_macro(dep_id);
            for dep in deps {
                let features_for = FeaturesFor::from_for_host(is_proc_macro || dep.is_build());
                let activated = self.features.activated_features_unverified(dep_id, features_for);
                for feature in activated.into_iter().flatten() {
                    all.insert(format!("{}/{}", dep.name_in_toml(), feature));
                }
            }
        }
        all
    }

    /// cargo's `dep_platform_activated`: a `[target.'cfg(..)'.dependencies]` platform holds.
    pub(super) fn platform_activated(&self, dep: &Dependency) -> bool {
        dep.platform().is_none_or(|it| it.matches(self.platform.triple, self.platform.cfg))
    }

    pub(super) fn cmp_units(&self, a: &Inner, b: &Inner) -> Ordering {
        a.pkg
            .cmp(&b.pkg)
            .then_with(|| cmp_target(self.target(a), self.target(b)))
            .then_with(|| cmp_profile(&a.profile, &b.profile))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.mode.cmp(&b.mode))
            .then_with(|| cmp_features(&a.features, &b.features))
    }
}

fn cmp_features(a: &[InternedString], b: &[InternedString]) -> Ordering {
    a.iter().map(|it| it.as_str()).cmp(b.iter().map(|it| it.as_str()))
}

fn crate_type_rank(it: &CrateType) -> (u8, &str) {
    match it {
        CrateType::Bin => (0, ""),
        CrateType::Lib => (1, ""),
        CrateType::Rlib => (2, ""),
        CrateType::Dylib => (3, ""),
        CrateType::Cdylib => (4, ""),
        CrateType::Staticlib => (5, ""),
        CrateType::ProcMacro => (6, ""),
        CrateType::Other(name) => (7, name),
    }
}

fn crate_type_ranks(kinds: &[CrateType]) -> Vec<(u8, &str)> {
    kinds.iter().map(crate_type_rank).collect()
}

fn kind_rank(kind: &TargetKind) -> (u8, Vec<(u8, &str)>) {
    match kind {
        TargetKind::Lib(kinds) => (0, crate_type_ranks(kinds)),
        TargetKind::Bin => (1, Vec::new()),
        TargetKind::Test => (2, Vec::new()),
        TargetKind::Bench => (3, Vec::new()),
        TargetKind::ExampleLib(kinds) => (4, crate_type_ranks(kinds)),
        TargetKind::ExampleBin => (5, Vec::new()),
        TargetKind::CustomBuild => (6, Vec::new()),
    }
}

/// `TargetInner`'s derived order: its fields in the order they are declared.
fn cmp_target(a: &Target, b: &Target) -> Ordering {
    // `Path` sorts before `Metabuild`, which is no path at all.
    let path =
        |it: &Target| -> (bool, Option<PathBuf>) { (it.src_path.is_none(), it.src_path.clone()) };
    kind_rank(&a.kind)
        .cmp(&kind_rank(&b.kind))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.name_inferred.cmp(&b.name_inferred))
        .then_with(|| a.bin_name.cmp(&b.bin_name))
        .then_with(|| path(a).cmp(&path(b)))
        .then_with(|| a.required_features.cmp(&b.required_features))
        .then_with(|| a.tested.cmp(&b.tested))
        .then_with(|| a.benched.cmp(&b.benched))
        .then_with(|| a.doc.cmp(&b.doc))
        .then_with(|| a.doctest.cmp(&b.doctest))
        .then_with(|| a.harness.cmp(&b.harness))
        .then_with(|| a.for_host.cmp(&b.for_host))
        .then_with(|| a.proc_macro.cmp(&b.proc_macro))
        .then_with(|| a.edition.to_string().cmp(&b.edition.to_string()))
        .then_with(|| a.doc_scrape_examples.cmp(&b.doc_scrape_examples))
}

fn lto_rank(it: &Lto) -> (u8, bool, &str) {
    match it {
        Lto::Off => (0, false, ""),
        Lto::Bool(b) => (1, *b, ""),
        Lto::Named(name) => (2, false, name.as_str()),
    }
}

fn strip_name(it: &StripInner) -> Option<&str> {
    match it {
        StripInner::None => None,
        StripInner::Named(name) => Some(name.as_str()),
    }
}

fn strip_rank(it: &Strip) -> (u8, Option<&str>) {
    match it {
        Strip::Resolved(it) => (0, strip_name(it)),
        Strip::Deferred(it) => (1, strip_name(it)),
    }
}

fn debuginfo_rank(it: DebugInfo) -> u8 {
    use cargo_util_schemas::manifest::TomlDebugInfo::*;
    match it.into_inner() {
        None => 0,
        LineDirectivesOnly => 1,
        LineTablesOnly => 2,
        Limited => 3,
        Full => 4,
    }
}

/// `Profile`'s derived order, `name` first.
fn cmp_profile(a: &Profile, b: &Profile) -> Ordering {
    let root = |it: &Profile| it.root as u8;
    a.name
        .cmp(&b.name)
        .then_with(|| a.opt_level.cmp(&b.opt_level))
        .then_with(|| root(a).cmp(&root(b)))
        .then_with(|| lto_rank(&a.lto).cmp(&lto_rank(&b.lto)))
        .then_with(|| a.codegen_backend.cmp(&b.codegen_backend))
        .then_with(|| a.codegen_units.cmp(&b.codegen_units))
        .then_with(|| debuginfo_rank(a.debuginfo).cmp(&debuginfo_rank(b.debuginfo)))
        .then_with(|| a.split_debuginfo.cmp(&b.split_debuginfo))
        .then_with(|| a.debug_assertions.cmp(&b.debug_assertions))
        .then_with(|| a.overflow_checks.cmp(&b.overflow_checks))
        .then_with(|| a.rpath.cmp(&b.rpath))
        .then_with(|| a.incremental.cmp(&b.incremental))
        .then_with(|| (a.panic as u8).cmp(&(b.panic as u8)))
        .then_with(|| strip_rank(&a.strip).cmp(&strip_rank(&b.strip)))
        .then_with(|| a.rustflags.cmp(&b.rustflags))
}
