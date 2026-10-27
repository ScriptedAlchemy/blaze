//! What a command compiles: the packages its flags select (cargo's `Packages`), the targets of
//! each (`CompileFilter` and `UnitGenerator`), and the root units those make. Ported from cargo
//! 1.99.0's `ops/cargo_compile/{packages,compile_filter,unit_generator}.rs`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};
use blaze_manifest::manifest::Manifest;
use blaze_manifest::profiles::UnitFor;
use blaze_manifest::targets::{CrateType, Target, TargetKind};
use blaze_resolve::resolver::{FeaturesFor, HasDevUnits};
use blaze_resolve::{FeatureValue, InternedString, PackageId, PackageIdSpec, closest_msg};
use rustc_hash::{FxHashMap, FxHashSet};

use super::model::{Arena, CMode, Env, Inner, Kind};
use super::{Intent, TargetFilter};

/// Whether `name` is a glob pattern, as cargo's `is_glob_pattern`.
fn is_glob_pattern(name: &str) -> bool {
    name.contains(['*', '?', '[', ']'])
}

fn build_glob(pattern: &str) -> Result<glob::Pattern> {
    glob::Pattern::new(pattern).map_err(|err| {
        anyhow::Error::new(err).context(format!("cannot build glob pattern from `{pattern}`"))
    })
}

/// The workspace as the selection reads it.
pub(super) struct Members<'a> {
    pub root: &'a Path,
    pub is_virtual: bool,
    /// The members, in the workspace's order.
    pub members: Vec<(PackageId, &'a Manifest)>,
    pub default_members: Vec<PackageId>,
}

/// cargo's `Packages`: which members a command is asked to build.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Select {
    /// No flag: the default members.
    Default,
    /// `--workspace`: every member. The `-p` names are checked to be members.
    All(Vec<String>),
    /// `--workspace --exclude`: every member but these.
    OptOut(Vec<String>),
    /// `-p`.
    Packages(Vec<String>),
}

impl Select {
    pub(super) fn from_flags(all: bool, exclude: &[String], package: &[String]) -> Result<Select> {
        Ok(match (all, exclude.len(), package.len()) {
            (false, 0, 0) => Select::Default,
            (false, 0, _) => Select::Packages(package.to_vec()),
            (false, _, _) => bail!("--exclude can only be used together with --workspace"),
            (true, 0, _) => Select::All(package.to_vec()),
            (true, _, _) => Select::OptOut(exclude.to_vec()),
        })
    }

    /// The specs of the packages selected. A warning of what cargo only warns of goes to
    /// `warnings`.
    pub(super) fn to_package_id_specs(
        &self,
        ws: &Members<'_>,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<PackageIdSpec>> {
        let specs: Vec<PackageIdSpec> = match self {
            Select::All(packages) => {
                not_found_within_workspace(ws, packages)?;
                ws.members.iter().map(|(id, _)| id.to_spec()).collect()
            }
            Select::OptOut(opt_out) => {
                let mut wanted = Wanted::parse(opt_out)?;
                let specs = ws
                    .members
                    .iter()
                    .filter(|&&(id, m)| !wanted.takes(id, m))
                    .map(|(id, _)| id.to_spec())
                    .collect();
                if let Err(err) = wanted.package_not_found(ws, true) {
                    warnings.push(err.to_string());
                }
                if let Err(err) = wanted.pattern_not_found(ws, true) {
                    warnings.push(err.to_string());
                }
                specs
            }
            Select::Packages(opt_in) => {
                let mut wanted = Wanted::parse(opt_in)?;
                let mut specs: Vec<PackageIdSpec> =
                    wanted.ids.iter().map(|(it, _)| it.clone()).collect();
                if !wanted.patterns.is_empty() {
                    let matched = ws.members.iter().filter(|&&(_, m)| wanted.match_patterns(m));
                    specs.extend(matched.map(|(id, _)| id.to_spec()));
                }
                wanted.pattern_not_found(ws, false)?;
                let mut seen = FxHashSet::default();
                specs.retain(|it| seen.insert(it.to_string()));
                specs
            }
            Select::Default => ws.default_members.iter().map(|id| id.to_spec()).collect(),
        };
        if specs.is_empty() {
            if ws.is_virtual {
                bail!(
                    "manifest path `{}` contains no package: The manifest is virtual, \
                     and the workspace has no members.",
                    ws.root.display()
                )
            }
            bail!("no packages to compile")
        }
        Ok(specs)
    }
}

/// The package specs and glob patterns of `-p` or `--exclude`, and which matched.
struct Wanted {
    patterns: Vec<(glob::Pattern, bool)>,
    ids: Vec<(PackageIdSpec, String)>,
}

impl Wanted {
    fn parse(opt: &[String]) -> Result<Wanted> {
        let mut wanted = Wanted { patterns: Vec::new(), ids: Vec::new() };
        for x in opt {
            match PackageIdSpec::parse(x) {
                Ok(spec) => {
                    let text = spec.to_string();
                    if !wanted.ids.iter().any(|(_, seen)| *seen == text) {
                        wanted.ids.push((spec, text));
                    }
                }
                Err(_) if is_glob_pattern(x) => wanted.patterns.push((build_glob(x)?, false)),
                Err(err) => return Err(err),
            }
        }
        Ok(wanted)
    }

    fn match_patterns(&mut self, manifest: &Manifest) -> bool {
        self.patterns.iter_mut().any(|(pattern, matched)| {
            let is_matched = pattern.matches(&manifest.package_id.name);
            *matched |= is_matched;
            is_matched
        })
    }

    /// Whether a spec or a pattern takes the package; a spec that does is used up.
    fn takes(&mut self, id: PackageId, manifest: &Manifest) -> bool {
        let at = self.ids.iter().position(|(spec, _)| spec.matches(id));
        if let Some(at) = at {
            self.ids.remove(at);
        }
        at.is_some() || self.match_patterns(manifest)
    }

    fn package_not_found(&self, ws: &Members<'_>, opt_out: bool) -> Result<()> {
        let names: std::collections::BTreeSet<&str> =
            self.ids.iter().map(|(_, it)| it.as_str()).collect();
        if !names.is_empty() {
            bail!(
                "{}package(s) `{}` not found in workspace `{}`",
                if opt_out { "excluded " } else { "" },
                names.into_iter().collect::<Vec<_>>().join(", "),
                ws.root.display(),
            )
        }
        Ok(())
    }

    fn pattern_not_found(&self, ws: &Members<'_>, opt_out: bool) -> Result<()> {
        let missed: Vec<&str> = self
            .patterns
            .iter()
            .filter(|(_, matched)| !*matched)
            .map(|(it, _)| it.as_str())
            .collect();
        if !missed.is_empty() {
            bail!(
                "{}package pattern(s) `{}` not found in workspace `{}`",
                if opt_out { "excluded " } else { "" },
                missed.join(", "),
                ws.root.display(),
            )
        }
        Ok(())
    }
}

fn not_found_within_workspace(ws: &Members<'_>, packages: &[String]) -> Result<()> {
    let mut wanted = Wanted::parse(packages)?;
    for &(id, m) in &ws.members {
        wanted.takes(id, m);
    }
    wanted.package_not_found(ws, false)?;
    wanted.pattern_not_found(ws, false)
}

/// Whether the library is compiled. `Default` is that of `--all-targets`: if there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LibRule {
    True,
    Default,
    False,
}

#[derive(Clone, Debug)]
pub(super) enum FilterRule {
    All,
    Just(Vec<String>),
}

impl FilterRule {
    fn is_specific(&self) -> bool {
        match self {
            FilterRule::All => true,
            FilterRule::Just(targets) => !targets.is_empty(),
        }
    }

    fn of(names: &Option<Vec<String>>) -> FilterRule {
        match names {
            Some(names) if names.is_empty() => FilterRule::All,
            Some(names) => FilterRule::Just(names.clone()),
            None => FilterRule::Just(Vec::new()),
        }
    }
}

/// cargo's `CompileFilter`: which targets of the selected packages.
#[derive(Clone, Debug)]
pub(super) enum Filter {
    Doctest,
    /// The command's own set of targets.
    Default,
    Only {
        lib: LibRule,
        bins: FilterRule,
        examples: FilterRule,
        tests: FilterRule,
        benches: FilterRule,
    },
}

impl Filter {
    pub(super) fn of(flags: &TargetFilter) -> Filter {
        if flags.doc_only {
            return Filter::Doctest;
        }
        if flags.all_targets {
            return Filter::Only {
                lib: LibRule::Default,
                bins: FilterRule::All,
                examples: FilterRule::All,
                tests: FilterRule::All,
                benches: FilterRule::All,
            };
        }
        let lib = if flags.lib { LibRule::True } else { LibRule::False };
        let (bins, tests) = (FilterRule::of(&flags.bins), FilterRule::of(&flags.tests));
        let (examples, benches) = (FilterRule::of(&flags.examples), FilterRule::of(&flags.benches));
        if lib == LibRule::True
            || bins.is_specific()
            || tests.is_specific()
            || examples.is_specific()
            || benches.is_specific()
        {
            Filter::Only { lib, bins, examples, tests, benches }
        } else {
            Filter::Default
        }
    }

    /// Whether the dev-dependencies are built (`need_dev_deps`).
    pub(super) fn need_dev_deps(&self, intent: Intent) -> bool {
        match intent {
            Intent::Test | Intent::Check { test: true } => true,
            Intent::Build | Intent::Check { test: false } => match self {
                Filter::Doctest => true,
                Filter::Default => false,
                Filter::Only { examples, tests, benches, .. } => {
                    examples.is_specific() || tests.is_specific() || benches.is_specific()
                }
            },
        }
    }

    fn is_specific(&self) -> bool {
        matches!(self, Filter::Only { .. })
    }
}

/// A proposed target, filtered into a unit by whether its features are there.
struct Proposal {
    pkg: PackageId,
    target: usize,
    /// Whether the features it requires must be present: targets named are, the others are
    /// skipped without them.
    requires_features: bool,
    mode: CMode,
}

fn to_compile_mode(intent: Intent) -> CMode {
    match intent {
        Intent::Test => CMode::Test,
        Intent::Build => CMode::Build,
        Intent::Check { test } => CMode::Check { test },
    }
}

pub(super) fn is_example(target: &Target) -> bool {
    matches!(target.kind, TargetKind::ExampleBin | TargetKind::ExampleLib(_))
}

pub(super) fn is_test(target: &Target) -> bool {
    target.kind == TargetKind::Test
}

pub(super) fn is_bench(target: &Target) -> bool {
    target.kind == TargetKind::Bench
}

/// cargo's `Target::is_linkable`: a library another crate can be compiled against.
pub(super) fn is_linkable(target: &Target) -> bool {
    match &target.kind {
        TargetKind::Lib(kinds) => kinds.iter().any(|k| {
            matches!(k, CrateType::Lib | CrateType::Rlib | CrateType::Dylib | CrateType::ProcMacro)
        }),
        _ => false,
    }
}

/// The root units of a command: cargo's `UnitGenerator`.
pub(super) struct Generator<'e, 'a> {
    pub env: &'e Env<'a>,
    /// The packages selected, in the order of their ids.
    pub packages: &'e [PackageId],
    pub select: &'e Select,
    pub members: &'e Members<'e>,
    pub filter: &'e Filter,
    pub has_dev_units: HasDevUnits,
    pub warnings: &'e mut Vec<String>,
}

impl Generator<'_, '_> {
    /// Libraries cargo compiles for documentation tests, even when the documentation-test
    /// units themselves are not represented in this graph.
    pub(super) fn doctest_libraries(&self, arena: &mut Arena) -> Result<Vec<usize>> {
        if self.env.intent != Intent::Test || matches!(self.filter, Filter::Only { .. }) {
            return Ok(Vec::new());
        }
        let mut libraries = Vec::new();
        for &pkg in self.packages {
            for (at, target) in self.targets(pkg).iter().enumerate() {
                if target.is_lib() && target.doctest && target.doctestable() {
                    // A doctest's linked library is generated as a build dependency, not
                    // a test root. In particular a proc macro uses its host profile.
                    let unit_for = if target.proc_macro {
                        UnitFor::new_host_test()
                    } else {
                        UnitFor::new_test()
                    };
                    // Apply the host profile before interning. Profile equality ignores
                    // deferred debuginfo, so a provisional Build unit could otherwise
                    // absorb this one and lose the marker Sharing needs to weaken it.
                    let mut library = self.make_unit(pkg, at, CMode::Build)?;
                    library.profile = self.env.profile(pkg, unit_for);
                    libraries.push(arena.intern(library));
                }
            }
        }
        Ok(libraries)
    }

    fn targets(&self, pkg: PackageId) -> &[Target] {
        &self.env.pkg(pkg).manifest.targets
    }

    /// The unit of `target` in `mode`, a test in `Test` mode but an example (which is tested
    /// by no one) compiled as the program it is.
    fn new_unit(
        &self,
        arena: &mut Arena,
        pkg: PackageId,
        target_at: usize,
        initial_mode: CMode,
    ) -> Result<usize> {
        Ok(arena.intern(self.make_unit(pkg, target_at, initial_mode)?))
    }

    /// Construct the complete unit before its caller chooses when to intern it.
    fn make_unit(&self, pkg: PackageId, target_at: usize, initial_mode: CMode) -> Result<Inner> {
        let target = &self.targets(pkg)[target_at];
        let mode = match initial_mode {
            CMode::Test if is_example(target) && !self.filter.is_specific() && !target.tested => {
                CMode::Build
            }
            CMode::Test => CMode::Test,
            other => other,
        };
        // No build dependency is a root: a proc macro's own features are the host's.
        let features_for = FeaturesFor::from_for_host(target.proc_macro);
        let Some(features) = self.env.features.activated_features_unverified(pkg, features_for)
        else {
            bail!("no features were resolved for `{pkg}`")
        };
        // What the unit is for: the `panic` setting is cleared for tests and the programs
        // `cargo test` builds with them, so that the library is not built three times.
        let unit_for = if initial_mode.is_any_test() {
            UnitFor::new_test()
        } else if target.for_host {
            UnitFor::new_compiler()
        } else {
            UnitFor::new_normal()
        };
        let profile = self.env.profile(pkg, unit_for);
        Ok(Inner {
            pkg,
            target: target_at,
            profile,
            kind: Kind::Target.for_target(target),
            mode,
            features,
        })
    }

    fn filter_default_targets(&self, pkg: PackageId) -> Vec<usize> {
        let targets = self.targets(pkg);
        let wanted = |t: &Target| match self.env.intent {
            Intent::Test => t.tested || is_example(t),
            Intent::Build | Intent::Check { .. } => t.is_bin() || t.is_lib(),
        };
        (0..targets.len()).filter(|&at| wanted(&targets[at])).collect()
    }

    fn filter_targets(
        &self,
        predicate: impl Fn(&Target) -> bool,
        requires_features: bool,
        mode: CMode,
    ) -> Vec<Proposal> {
        let mut out = Vec::new();
        for &pkg in self.packages {
            for (at, target) in self.targets(pkg).iter().enumerate() {
                if predicate(target) {
                    out.push(Proposal { pkg, target: at, requires_features, mode });
                }
            }
        }
        out
    }

    fn find_named_targets(
        &self,
        target_name: &str,
        target_desc: &'static str,
        is_expected_kind: fn(&Target) -> bool,
        mode: CMode,
    ) -> Result<Vec<Proposal>> {
        let is_glob = is_glob_pattern(target_name);
        let pattern = build_glob(target_name)?;
        let filter = |t: &Target| {
            if is_glob {
                is_expected_kind(t) && pattern.matches(&t.name)
            } else {
                is_expected_kind(t) && t.name == target_name
            }
        };
        let proposals = self.filter_targets(filter, true, mode);
        if !proposals.is_empty() {
            return Ok(proposals);
        }
        // `BTreeMap`: the targets by name, each with the packages that have one.
        let mut targets: BTreeMap<&str, Vec<PackageId>> = BTreeMap::new();
        for &pkg in self.packages {
            for target in self.targets(pkg).iter().filter(|t| is_expected_kind(t)) {
                targets.entry(&target.name).or_default().push(pkg);
            }
        }
        let suggestion = closest_msg(target_name, targets.keys(), |t| t, "target");
        let elsewhere = self.targets_from_other_packages(filter);
        let unmatched_packages = match self.select {
            Select::Default | Select::OptOut(_) | Select::All(_) => {
                " in default-run packages".to_owned()
            }
            Select::Packages(packages) => match packages.len() {
                0 => String::new(),
                1 => format!(" in `{}` package", packages[0]),
                _ => format!(" in `{}`, ... packages", packages[0]),
            },
        };
        let named = if is_glob { "matches pattern" } else { "named" };
        let mut msg = format!(
            "no {target_desc} target {named} `{target_name}`{unmatched_packages}{suggestion}"
        );
        if !elsewhere.is_empty() {
            let mut available = Vec::new();
            for (package, names) in &elsewhere {
                available.push(format!("help: available {target_desc} in `{package}` package:"));
                for name in names {
                    available.push(format!("    {name}"));
                }
            }
            msg.push_str(&format!("\n{}", available.join("\n")));
        } else if suggestion.is_empty() && !targets.is_empty() {
            msg.push_str(&format!("\nhelp: available {target_desc} targets:"));
            for (name, pkgs) in targets {
                if pkgs.len() == 1 {
                    msg.push_str(&format!("\n    {name}"));
                } else {
                    for pkg in pkgs {
                        msg.push_str(&format!("\n    {name} in package {}", pkg.name()));
                    }
                }
            }
        }
        bail!(msg)
    }

    fn targets_from_other_packages(
        &self,
        filter: impl Fn(&Target) -> bool,
    ) -> Vec<(&str, Vec<&str>)> {
        let mut out = Vec::new();
        for (_, manifest) in &self.members.members {
            let mut names: Vec<&str> =
                manifest.targets.iter().filter(|t| filter(t)).map(|t| t.name.as_str()).collect();
            if !names.is_empty() {
                names.sort();
                out.push((manifest.package_id.name.as_str(), names));
            }
        }
        out
    }

    fn list_rule_targets(
        &self,
        rule: &FilterRule,
        target_desc: &'static str,
        is_expected_kind: fn(&Target) -> bool,
        mode: CMode,
    ) -> Result<Vec<Proposal>> {
        match rule {
            FilterRule::All => Ok(self.filter_targets(is_expected_kind, false, mode)),
            FilterRule::Just(names) => {
                let mut out = Vec::new();
                for name in names {
                    out.extend(self.find_named_targets(
                        name,
                        target_desc,
                        is_expected_kind,
                        mode,
                    )?);
                }
                Ok(out)
            }
        }
    }

    fn create_proposals(&self) -> Result<Vec<Proposal>> {
        let intent = self.env.intent;
        let mut proposals = Vec::new();
        match self.filter {
            Filter::Doctest => return Ok(Vec::new()),
            Filter::Default => {
                for &pkg in self.packages {
                    for target in self.filter_default_targets(pkg) {
                        // The targets cargo skips without their features are those it was not
                        // told to build.
                        proposals.push(Proposal {
                            pkg,
                            target,
                            requires_features: false,
                            mode: to_compile_mode(intent),
                        });
                    }
                }
            }
            Filter::Only { lib, bins, examples, tests, benches } => {
                if *lib != LibRule::False {
                    let libs = self.filter_targets(Target::is_lib, false, to_compile_mode(intent));
                    if *lib == LibRule::True && libs.is_empty() {
                        let names: Vec<&str> =
                            self.packages.iter().map(|it| it.name().as_str()).collect();
                        if names.len() == 1 {
                            bail!("no library targets found in package `{}`", names[0]);
                        }
                        bail!("no library targets found in packages: {}", names.join(", "));
                    }
                    proposals.extend(libs);
                }
                let default_mode = to_compile_mode(intent);
                let test_filter: fn(&Target) -> bool = match tests {
                    FilterRule::All => |t| t.tested,
                    FilterRule::Just(_) => is_test,
                };
                let test_mode = match intent {
                    Intent::Build => CMode::Test,
                    Intent::Check { .. } => CMode::Check { test: true },
                    Intent::Test => default_mode,
                };
                let bench_filter: fn(&Target) -> bool = match benches {
                    FilterRule::All => |t| t.benched,
                    FilterRule::Just(_) => is_bench,
                };
                proposals.extend(self.list_rule_targets(
                    bins,
                    "bin",
                    Target::is_bin,
                    default_mode,
                )?);
                proposals.extend(self.list_rule_targets(
                    examples,
                    "example",
                    is_example,
                    default_mode,
                )?);
                proposals.extend(self.list_rule_targets(tests, "test", test_filter, test_mode)?);
                proposals.extend(self.list_rule_targets(
                    benches,
                    "bench",
                    bench_filter,
                    test_mode,
                )?);
            }
        }
        Ok(proposals)
    }

    /// cargo's `resolve_all_features`: the features enabled for `pkg`, and each of its
    /// dependencies' as `dep/feature`.
    pub(super) fn all_features(
        &self,
        pkg: PackageId,
        has_dev_units: HasDevUnits,
    ) -> FxHashSet<String> {
        self.env.all_features(pkg, has_dev_units)
    }

    /// Warns of a `required-features` entry cargo warns of, and refuses those it refuses.
    fn validate_required_features(
        &mut self,
        pkg: PackageId,
        name: &str,
        required: &[String],
    ) -> Result<()> {
        let summary = self.env.resolve.summary(pkg);
        for feature in required {
            let fv = FeatureValue::new(InternedString::new(feature));
            match &fv {
                FeatureValue::Feature(f) => {
                    if !summary.features().contains_key(f) {
                        self.warnings.push(format!(
                            "invalid feature `{fv}` in required-features of target `{name}`: \
                             `{fv}` is not present in [features] section"
                        ));
                    }
                }
                FeatureValue::Dep { .. } => bail!(
                    "invalid feature `{fv}` in required-features of target `{name}`: \
                     `dep:` prefixed feature values are not allowed in required-features"
                ),
                FeatureValue::DepFeature { weak: true, .. } => bail!(
                    "invalid feature `{fv}` in required-features of target `{name}`: \
                     optional dependency with `?` is not allowed in required-features"
                ),
                FeatureValue::DepFeature { dep_name, dep_feature, weak: false } => {
                    let found =
                        self.env.resolve.deps(pkg).find(|(_, deps)| {
                            deps.iter().any(|dep| dep.name_in_toml() == *dep_name)
                        });
                    match found {
                        Some((dep_id, _)) => {
                            let dep_summary = self.env.resolve.summary(dep_id);
                            if !dep_summary.features().contains_key(dep_feature)
                                && !dep_summary.dependencies().iter().any(|dep| {
                                    dep.name_in_toml() == *dep_feature && dep.is_optional()
                                })
                            {
                                self.warnings.push(format!(
                                    "invalid feature `{fv}` in required-features of target `{name}`: \
                                     feature `{dep_feature}` does not exist in package `{dep_id}`"
                                ));
                            }
                        }
                        None => self.warnings.push(format!(
                            "invalid feature `{fv}` in required-features of target `{name}`: \
                             dependency `{dep_name}` does not exist"
                        )),
                    }
                }
            }
        }
        Ok(())
    }

    /// The proposals that have their features, as units, in cargo's order.
    fn proposals_to_units(
        &mut self,
        arena: &mut Arena,
        proposals: Vec<Proposal>,
    ) -> Result<Vec<usize>> {
        let mut features_map: FxHashMap<PackageId, FxHashSet<String>> = FxHashMap::default();
        let mut units: Vec<usize> = Vec::new();
        let env = self.env;
        for Proposal { pkg, target, requires_features, mode } in proposals {
            let t = &env.pkg(pkg).manifest.targets[target];
            let unavailable: Vec<&String> = match &t.required_features {
                Some(required) => {
                    self.validate_required_features(pkg, &t.name, required)?;
                    let have = features_map
                        .entry(pkg)
                        .or_insert_with(|| env.all_features(pkg, self.has_dev_units));
                    required.iter().filter(|f| !have.contains(*f)).collect()
                }
                None => Vec::new(),
            };
            if t.is_lib() || unavailable.is_empty() {
                let unit = self.new_unit(arena, pkg, target, mode)?;
                if !units.contains(&unit) {
                    units.push(unit);
                }
            } else if requires_features {
                let required =
                    t.required_features.as_ref().expect("a target that requires features");
                let quoted: Vec<String> = required.iter().map(|s| format!("`{s}`")).collect();
                bail!(
                    "target `{}` in package `{}` requires the features: {}\n\
                     Consider enabling them by passing, e.g., `--features=\"{}\"`",
                    t.name,
                    pkg.name(),
                    quoted.join(", "),
                    required.join(" ")
                );
            }
        }
        self.unmatched_target_filters(&units);
        units.sort_by(|&a, &b| self.env.cmp_units(&arena.units[a], &arena.units[b]));
        Ok(units)
    }

    fn unmatched_target_filters(&mut self, units: &[usize]) {
        let Filter::Only { bins, examples, tests, benches, lib } = self.filter else { return };
        if !units.is_empty() {
            return;
        }
        let all_targets = matches!(
            (lib, bins, examples, tests, benches),
            (LibRule::Default, FilterRule::All, FilterRule::All, FilterRule::All, FilterRule::All)
        );
        let mut filters = String::new();
        let mut miss_count = 0;
        let mut append = |rule: &FilterRule, s: &str| {
            if let FilterRule::All = rule {
                miss_count += 1;
                filters.push_str(s);
            }
        };
        if all_targets {
            filters.push_str(" `all-targets`");
        } else {
            append(bins, " `bins`,");
            append(tests, " `tests`,");
            append(examples, " `examples`,");
            append(benches, " `benches`,");
            filters.pop();
        }
        self.warnings.push(format!(
            "target {}{} specified, but no targets matched; this is a no-op",
            if miss_count > 1 { "filters" } else { "filter" },
            filters,
        ));
    }

    /// The root units of the command.
    pub(super) fn generate_root_units(&mut self, arena: &mut Arena) -> Result<Vec<usize>> {
        if matches!(self.filter, Filter::Doctest) {
            return Ok(Vec::new());
        }
        let proposals = self.create_proposals()?;
        self.proposals_to_units(arena, proposals)
    }
}
