//! The unit graph of a build, made by blaze with no cargo: what `cargo <command> --unit-graph`
//! prints (cargo 1.99.0's `unit_dependencies` and `unit_generator`, ported), unit by unit: the
//! package, the target, the mode, the features that unit is built with (host and target apart,
//! as the resolver unifies them), its profile once `[profile.*.package.*]` and `build-override`
//! apply, and the units it depends on.
//!
//! `harness/dist/buildspec.py` compares [`UnitGraph::to_cargo_json`] with cargo's `--unit-graph`
//! over its fixtures; `docs/resolve.md` says what else is compared.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use blaze_manifest::Workspace;
use blaze_manifest::metadata::root_manifest;
use blaze_manifest::profiles::{ProfilePackage, Profiles, requested_profile};
use blaze_manifest::source::SourceId;
use blaze_resolve::resolver::{
    CliFeatures, FeatureOpts, FeatureResolver, ForceAllTargets, HasDevUnits, members_with_features,
};
use blaze_resolve::{InternedString, PackageId};
use cargo_util_schemas::core::SourceKind;
use rustc_hash::FxHashMap;

use crate::{Asked, Locked, Packages, World};

mod deps;
mod json;
mod model;
mod select;
mod share;

use model::{Arena, CMode, Env, Pkg};
use select::{Filter, Generator, Members, Select};

/// What cargo is asked to compile: `cargo check`, `build` (and `run`), `test`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// `cargo check`: libraries are checked, build scripts and proc macros built.
    Check,
    /// `cargo build` and `cargo run`.
    Build,
    /// `cargo test`: unit tests, integration tests and the rest of the targets' tests, with the
    /// dev-dependencies.
    Test,
}

/// The targets of the members a build compiles, as `--lib`, `--bin`, `--bins`, `--example`,
/// `--examples`, `--test`, `--tests`, `--bench`, `--benches` and `--all-targets` choose them.
/// `Default` is what the command compiles with none of them (cargo's `CompileFilter::Default`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetFilter {
    /// Whether any of the flags was given. When none was, the command's defaults apply.
    pub explicit: bool,
    /// `--lib`.
    pub lib: bool,
    /// `--bins`, or the names of `--bin`: `None` is not asked for, `Some(vec![])` is every one.
    pub bins: Option<Vec<String>>,
    /// `--examples`, or the names of `--example`.
    pub examples: Option<Vec<String>>,
    /// `--tests`, or the names of `--test`.
    pub tests: Option<Vec<String>>,
    /// `--benches`, or the names of `--bench`.
    pub benches: Option<Vec<String>>,
    /// `--all-targets`.
    pub all_targets: bool,
}

/// What [`unit_graph`] is asked: the workspace as [`Asked`] has it (`build` is required: it
/// selects the members, and `platform` the triple and cfg of the host and the target), the
/// command, which of the members' targets it compiles, and the profile it is asked for.
pub struct UnitsAsked<'a> {
    /// The workspace, its configuration, the members and the features asked for.
    pub asked: &'a Asked<'a>,
    /// The command whose units are wanted.
    pub command: Command,
    /// Which targets of the members.
    pub targets: &'a TargetFilter,
    /// `dev`, `release`, `test`, `bench` or a custom profile's name; `None` for the command's
    /// own (`dev`, and `test` for a test).
    pub profile: Option<&'a str>,
}

/// The kind of a target. `CustomBuild` is a build script, `ProcMacro` a `proc-macro` library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetKind {
    /// A library (`lib`, `rlib`, `dylib`, `cdylib`, `staticlib`).
    Lib,
    /// A binary.
    Bin,
    /// An integration test.
    Test,
    /// An example.
    Example,
    /// A benchmark.
    Bench,
    /// A build script.
    CustomBuild,
    /// A procedural macro library.
    ProcMacro,
}

/// cargo's `CompileMode` for a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// Compiled to be linked or loaded.
    Build,
    /// Checked: metadata only.
    Check,
    /// Compiled with `--test`: the libtest harness is in it.
    Test,
    /// A build script, run.
    RunCustomBuild,
}

/// The package of a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitPackage {
    /// The id as `cargo build --unit-graph` prints it for `pkg_id` (`registry+https://...#name@1.0.0`,
    /// `path+file:///abs/dir#name@0.1.0`): a path package's id names where it is on this machine.
    pub id: String,
    /// The name.
    pub name: String,
    /// The version.
    pub version: String,
    /// Where it comes from as `Cargo.lock` spells it (`registry+https://...`, `git+https://...`),
    /// or `path+` and the package's directory relative to the workspace root, so that the same
    /// source on another machine is the same.
    pub source: String,
    /// The directory of its manifest.
    pub manifest_dir: PathBuf,
    /// Whether it is a member of the workspace.
    pub member: bool,
}

/// The target of a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitTarget {
    /// Its kind.
    pub kind: TargetKind,
    /// `lib`, `rlib`, `bin`, `proc-macro` and the like, as cargo's unit graph spells them.
    pub crate_types: Vec<String>,
    /// Its name.
    pub name: String,
    /// The root file of the crate.
    pub src_path: PathBuf,
    /// The edition, `2021`.
    pub edition: u16,
    /// Whether the libtest harness is used (`harness = false` is a custom harness).
    pub harness: bool,
    /// `required-features`, if the target has any.
    pub required_features: Option<Vec<String>>,
    /// `doc`: whether `cargo doc` documents it.
    pub doc: bool,
    /// `doctest`: whether its documentation is tested, as `cargo build --unit-graph` prints it
    /// (a library that doc tests can be compiled against).
    pub doctest: bool,
    /// `test`: whether `cargo test` tests it, as the target's `test` key says.
    pub tested: bool,
}

/// An edge: a unit this one is compiled against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitDep {
    /// The index in [`UnitGraph::units`].
    pub index: usize,
    /// The name the crate is used under.
    pub extern_crate_name: String,
    /// `public = true` (unstable): always false on stable.
    pub public: bool,
    /// Whether it is left out of the prelude (`noprelude`).
    pub noprelude: bool,
    /// Whether it is exempt from the unused-crate lint (`nounused`): always false here.
    pub nounused: bool,
}

/// One compilation.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    /// Its package.
    pub package: UnitPackage,
    /// Its target.
    pub target: UnitTarget,
    /// Its mode.
    pub mode: Mode,
    /// Whether it is compiled with `--test` (`cfg(test)`): in `Mode::Test`, and in `Mode::Check`
    /// when what is checked is a test.
    pub test: bool,
    /// Its profile: the package's override, or the build override if it is built for the host.
    pub profile: blaze_manifest::profiles::Profile,
    /// The features it is built with, sorted.
    pub features: Vec<String>,
    /// Whether it is built for the host alone: a build script, a proc macro, and what only they
    /// depend on. A unit the target's programs link too is not, as cargo builds it once when
    /// the host's needs and the target's are the same.
    pub for_host: bool,
    /// What it depends on.
    pub dependencies: Vec<UnitDep>,
}

/// The units of a build and the ones the command asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitGraph {
    /// Every unit.
    pub units: Vec<Unit>,
    /// The indexes of the units the command named: the targets of the members it compiles.
    pub roots: Vec<usize>,
    /// The workspace's root directory.
    pub root: PathBuf,
    /// The bytes of `Cargo.lock`.
    pub lock: Vec<u8>,
    /// What cargo warns of while it makes the graph: package selection, `required-features`,
    /// package overrides in profiles that match nothing, and the configuration.
    pub warnings: Vec<String>,
}

impl UnitGraph {
    /// The graph as `cargo build --unit-graph` prints it (`{version, units, roots}`; `platform`
    /// null when nothing is built for another target), so that a harness can compare the two.
    pub fn to_cargo_json(&self) -> serde_json::Value {
        json::of(self)
    }
}

/// What a command compiles for, as far as cargo's `UserIntent` matters to the units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Intent {
    Build,
    Check { test: bool },
    Test,
}

impl Intent {
    /// Whether the units are compiled with `--test` (cargo's `is_rustc_test`).
    fn is_rustc_test(self) -> bool {
        matches!(self, Intent::Test | Intent::Check { test: true })
    }
}

/// A package as the `[profile.*.package.*]` overrides are matched against it.
struct ProfilePkg<'a>(&'a blaze_manifest::manifest::PackageId);

impl fmt::Display for ProfilePkg<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let PackageIdParts { name, version, source } = PackageIdParts::of(self.0);
        write!(f, "{name} v{version}")?;
        match &source.kind {
            SourceKind::Path => write!(f, " ({})", source.url.path()),
            _ if source.url.as_str() == blaze_manifest::source::CRATES_IO_INDEX => Ok(()),
            _ => write!(f, " ({})", source.url),
        }
    }
}

struct PackageIdParts<'a> {
    name: &'a str,
    version: &'a semver::Version,
    source: &'a SourceId,
}

impl<'a> PackageIdParts<'a> {
    fn of(id: &'a blaze_manifest::manifest::PackageId) -> PackageIdParts<'a> {
        PackageIdParts { name: &id.name, version: &id.version, source: &id.source_id }
    }
}

impl ProfilePackage for ProfilePkg<'_> {
    fn name(&self) -> &str {
        &self.0.name
    }

    fn version(&self) -> &semver::Version {
        &self.0.version
    }

    fn source_id(&self) -> &SourceId {
        &self.0.source_id
    }
}

/// `path` as a relative path from `root`, written with `/`; `.` for `root` itself.
fn relative(root: &Path, path: &Path) -> String {
    let (from, to): (Vec<_>, Vec<_>) = (root.components().collect(), path.components().collect());
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_owned(); from.len() - common];
    parts.extend(to[common..].iter().map(|it| it.as_os_str().to_string_lossy().into_owned()));
    if parts.is_empty() { ".".to_owned() } else { parts.join("/") }
}

/// The ids of the packages `specs` name in `resolve`, each once, as cargo's `specs_to_ids`.
fn specs_to_ids(
    resolve: &blaze_resolve::Resolve,
    specs: &[blaze_resolve::PackageIdSpec],
) -> Result<Vec<PackageId>> {
    let mut ids: Vec<PackageId> = Vec::new();
    for spec in specs {
        let found: Vec<PackageId> = resolve.iter().filter(|id| spec.matches(*id)).collect();
        match found.as_slice() {
            [] => bail!("package ID specification `{spec}` did not match any packages"),
            [one] => {
                if !ids.contains(one) {
                    ids.push(*one);
                }
            }
            many => {
                let listed: Vec<String> = many.iter().map(|it| it.to_spec().to_string()).collect();
                bail!(
                    "There are multiple `{}` packages in your project, and the specification \
                     `{spec}` is ambiguous.\nPlease re-run this command with one of the \
                     following specifications:\n  {}",
                    spec.name(),
                    listed.join("\n  ")
                )
            }
        }
    }
    Ok(ids)
}

/// A build for another target, or a `links` override, is what the unit graph does not read.
fn refuse_what_is_not_read(config: &blaze_manifest::Config, host: &str) -> Result<()> {
    if let Some(target) = &config.build_config()?.target {
        let named = target.values(config.cwd()).join(", ");
        bail!(
            "`build.target` is `{named}`: a unit graph is only made for the host, a build for a \
             target (`--target`) is not read yet"
        );
    }
    let table: Option<std::collections::BTreeMap<String, toml::Value>> =
        config.get(&format!("target.{host}"))?;
    for key in table.into_iter().flatten().map(|(key, _)| key) {
        if !["ar", "linker", "runner", "rustflags", "rustdocflags"].contains(&key.as_str()) {
            bail!("`target.{host}.{key}` overrides a build script (`links`): not read yet");
        }
    }
    Ok(())
}

/// The unit graph of `asked`, made with no cargo: `cargo <command> --unit-graph` for the
/// command and flags it names.
///
/// What it leaves out: the units of doc tests, which `cargo test` makes for a library (a
/// test's `doctest` unit; blaze runs them apart from the build), and those of `--doc`, `cargo
/// doc` and `-Zbuild-std` and artifact dependencies. What it refuses, naming it: a build for
/// a target other than the host, `links` overrides in the configuration, `[replace]`, and a
/// manifest or lock file blaze-metadata does not read.
pub fn unit_graph(asked: &UnitsAsked<'_>) -> Result<UnitGraph> {
    let a = asked.asked;
    let Some(build) = &a.build else {
        bail!("the unit graph is that of a build: no `Build` was given")
    };
    let Some(platform) = &a.platform else {
        bail!("the unit graph needs the host's triple and its cfg: no `TargetPlatform` was given")
    };
    let (command, default_profile, intent) = match asked.command {
        Command::Build => ("build", "dev", Intent::Build),
        Command::Check => ("check", "dev", Intent::Check { test: asked.profile == Some("test") }),
        Command::Test => ("test", "test", Intent::Test),
    };
    let (ctx, offline) = crate::open(a, command)?;
    refuse_what_is_not_read(&ctx.config, platform.triple)?;
    let ws = Workspace::new(&root_manifest(a.manifest_path, &ctx)?, &ctx)?;
    let locked = Locked::read(&ws, &ctx, offline, a.store.clone())?;
    let world = World::new(&ws, &locked)?;
    let mut warnings: Vec<String> = Vec::new();
    for warning in ws.workspace_warnings() {
        warnings.push(warning.to_string());
    }
    for (path, warning) in ws.warnings() {
        warnings.push(format!("{}: {warning}", path.display()));
    }

    let members = members_of(&ws, &world);
    let select = Select::from_flags(build.workspace, build.excludes, build.packages)?;
    let specs = select.to_package_id_specs(&members, &mut warnings)?;
    let filter = Filter::of(asked.targets);
    let has_dev_units =
        if filter.need_dev_deps(intent) { HasDevUnits::Yes } else { HasDevUnits::No };
    let cli_features =
        CliFeatures::from_command_line(a.features, a.all_features, !a.no_default_features)?;
    let resolved = world.resolve(&ws, &specs, &cli_features, has_dev_units)?;

    let summaries = world.member_summaries();
    let member_features = members_with_features(&world.members(&summaries), &specs, &cli_features)?;
    let packages = Packages { reads: &world.reads, platform: Some(platform) };
    let opts = FeatureOpts::new(world.behavior, has_dev_units, ForceAllTargets::No);
    let kinds = [blaze_resolve::resolver::CompileKind::Host];
    let features =
        FeatureResolver::resolve(&packages, &resolved.targeted, &member_features, &kinds, opts)?;

    let mut to_build = specs_to_ids(&resolved.targeted, &specs)?;
    to_build.sort();
    let env_packages = packages_of(&ws, &world);
    for id in &to_build {
        let pkg = &env_packages[id];
        let is_test = matches!(intent, Intent::Test | Intent::Check { test: true });
        if is_test
            && !pkg.member
            && resolved.targeted.summary(*id).dependencies().iter().any(|dep| !dep.is_transitive())
        {
            bail!(
                "package `{}` cannot be tested because it requires dev-dependencies and is not a \
                 member of the workspace",
                id.name()
            );
        }
    }

    let requested = match asked.profile {
        None => default_profile.to_owned(),
        Some(name) => requested_profile(default_profile, false, Some(name))?,
    };
    let profiles = Profiles::new(
        ws.profiles(),
        ws.unstable_features(),
        &ctx.config,
        &requested,
        platform.triple,
    )?;
    let in_lock: Vec<ProfilePkg<'_>> = resolved
        .whole
        .iter()
        .map(|id| ProfilePkg(&env_packages[&id].manifest.package_id))
        .collect();
    let in_lock: Vec<&dyn ProfilePackage> =
        in_lock.iter().map(|it| it as &dyn ProfilePackage).collect();
    profiles.validate_packages(ws.profiles(), &in_lock, &mut warnings)?;

    let env = Env {
        resolve: &resolved.targeted,
        features: &features,
        profiles: &profiles,
        packages: env_packages,
        platform,
        intent,
    };
    let mut arena = Arena::default();
    let mut generator = Generator {
        env: &env,
        packages: &to_build,
        select: &select,
        members: &members,
        filter: &filter,
        has_dev_units,
        warnings: &mut warnings,
    };
    let roots = generator.generate_root_units(&mut arena)?;
    let mut reachable = roots.clone();
    reachable.extend(generator.doctest_libraries(&mut arena)?);
    let graph = deps::build(&env, arena, &reachable)?;
    let mut shared = share::share(&env, &graph, &reachable);
    shared.roots.truncate(roots.len());
    warnings.extend(ctx.config.take_warnings());
    assemble(&env, &shared, ws.root(), &locked, warnings)
}

/// The members of the workspace, as the package selection reads them.
fn members_of<'w>(ws: &'w Workspace<'_>, world: &'w World<'_>) -> Members<'w> {
    let members: Vec<(PackageId, &blaze_manifest::manifest::Manifest)> =
        world.member_ids().iter().copied().zip(world.manifests.iter().copied()).collect();
    let by_name =
        |name: &str| members.iter().map(|(id, _)| *id).find(|id| id.name().as_str() == name);
    let default_members =
        ws.default_members().filter_map(|(_, m)| by_name(&m.package_id.name)).collect();
    Members { root: ws.root(), is_virtual: ws.is_virtual(), members, default_members }
}

/// Every package the build can reach, with what the units read of it.
fn packages_of<'a>(ws: &Workspace<'_>, world: &World<'a>) -> FxHashMap<PackageId, Pkg<'a>> {
    let mut out = FxHashMap::default();
    for (at, id) in world.ids.iter().enumerate() {
        let manifest: &'a blaze_manifest::manifest::Manifest = world.manifests[at];
        let local = world.reads[id].summary.source_id().is_path();
        let dir = world.manifest_paths[at].parent().expect("a manifest is in a directory");
        let source = match world.locked_sources[at] {
            Some(source) => source.to_owned(),
            None => format!("path+{}", relative(ws.root(), dir)),
        };
        out.insert(*id, Pkg { manifest, dir, member: at < world.member_count, local, source });
    }
    out
}

fn mode_of(mode: CMode) -> (Mode, bool) {
    match mode {
        CMode::Test => (Mode::Test, true),
        CMode::Build => (Mode::Build, false),
        CMode::Check { test } => (Mode::Check, test),
        CMode::RunCustomBuild => (Mode::RunCustomBuild, false),
    }
}

/// The graph made for the library to give out.
fn assemble(
    env: &Env<'_>,
    shared: &share::Shared,
    root: &Path,
    locked: &Locked,
    warnings: Vec<String>,
) -> Result<UnitGraph> {
    // The units the programs link: those the roots reach without going through a build script
    // or a proc macro. The others are only for the host.
    let mut linked = vec![false; shared.units.len()];
    let mut queue: Vec<usize> = shared.roots.clone();
    while let Some(at) = queue.pop() {
        if env.target(&shared.units[at]).for_host || linked[at] {
            continue;
        }
        linked[at] = true;
        queue.extend(shared.deps[at].iter().map(|dep| dep.unit));
    }
    let mut units = Vec::with_capacity(shared.units.len());
    for (at, inner) in shared.units.iter().enumerate() {
        let pkg = env.pkg(inner.pkg);
        let target = env.target(inner);
        let src_path = target
            .src_path
            .clone()
            .with_context(|| format!("`{}` has a metabuild script: not read yet", inner.pkg))?;
        let crate_types: Vec<String> =
            target.kind.rustc_crate_types().iter().map(|it| it.to_string()).collect();
        let kind = match &target.kind {
            blaze_manifest::targets::TargetKind::Lib(_) if target.proc_macro => {
                TargetKind::ProcMacro
            }
            blaze_manifest::targets::TargetKind::Lib(_) => TargetKind::Lib,
            blaze_manifest::targets::TargetKind::Bin => TargetKind::Bin,
            blaze_manifest::targets::TargetKind::Test => TargetKind::Test,
            blaze_manifest::targets::TargetKind::Bench => TargetKind::Bench,
            blaze_manifest::targets::TargetKind::ExampleLib(_)
            | blaze_manifest::targets::TargetKind::ExampleBin => TargetKind::Example,
            blaze_manifest::targets::TargetKind::CustomBuild => TargetKind::CustomBuild,
        };
        let (mode, test) = mode_of(inner.mode);
        units.push(Unit {
            package: UnitPackage {
                id: inner.pkg.to_spec().to_string(),
                name: inner.pkg.name().to_string(),
                version: inner.pkg.version().to_string(),
                source: pkg.source.clone(),
                manifest_dir: pkg.dir.to_owned(),
                member: pkg.member,
            },
            target: UnitTarget {
                kind,
                crate_types,
                name: target.name.clone(),
                src_path,
                edition: target.edition.to_string().parse().context("an edition")?,
                harness: target.harness,
                required_features: target.required_features.clone(),
                doc: target.doc,
                doctest: target.doctest && target.doctestable(),
                tested: target.tested,
            },
            mode,
            test,
            profile: inner.profile.clone(),
            features: inner.features.iter().map(InternedString::to_string).collect(),
            for_host: !linked[at],
            dependencies: shared.deps[at]
                .iter()
                .map(|dep| UnitDep {
                    index: dep.unit,
                    extern_crate_name: dep.name.to_string(),
                    public: false,
                    noprelude: false,
                    nounused: false,
                })
                .collect(),
        });
    }
    Ok(UnitGraph {
        units,
        roots: shared.roots.clone(),
        root: root.to_owned(),
        lock: locked.lock_text.clone().into_bytes(),
        warnings,
    })
}
