//! A request, the configuration and the units made into a `BuildSpec`, its `ExecPlatform`
//! digest and the keys derived from it.
//!
//! [`build_spec`] is the whole of it: it refuses what blaze cannot honour of the environment and
//! the configuration, makes the unit graph (`blaze_metadata::units::unit_graph`), and
//! [`assemble`]s the spec from it. A refusal is made before the graph is, so that a command that
//! cannot be honoured is not resolved first.
//!
//! **The canonical unit order** is the units sorted by the canonical CBOR of each without its
//! dependencies ([`canonical_key`]). Every other field together identifies a unit, so the order
//! is total; where two units are alike in all of them the order is not, and the spec is refused
//! naming both. `deps` and `asked` name indexes in that order.
//!
//! **What a unit is.** A run of a build script is not a unit of a spec: an edge to one is an
//! edge to the unit that compiles the script, under the same name, because what is run is
//! decided by the script and the environment the spec carries. A unit is `test` when it is
//! compiled with `--test` (a check of an integration test or a benchmark too); a
//! `harness = false` test is not. The cfgs of a unit are those knowable without running
//! anything: its features, `test`, `proc_macro` for a proc macro, what its profile makes of
//! `debug_assertions`, `overflow_checks` and `panic`, and every `--cfg` of the rustflags, which
//! cargo gives to every unit when nothing is built for another target.
//!
//! **The run.** `harness_args` are every word after `--` as typed, and for `cargo run` the
//! program's arguments before it too; `exact`, `skip`, `ignored` and `test_threads` are read out
//! of them as libtest spells them. `env` is empty: a test runs with the variables cargo gives
//! it, which the executor sets, and the requester's ambient variables are not carried (a test
//! that reads one sees the worker's). `[env]` of the configuration is `config_env`, which the
//! worker applies over `script_env`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use blaze_manifest::metadata::root_manifest;
use blaze_manifest::profiles::{Profiles, requested_profile};
use blaze_manifest::{Config, Workspace};
use blaze_metadata::units::{self, Mode, TargetFilter, TargetKind, Unit, UnitGraph, UnitsAsked};
use blaze_metadata::{Asked, Build, TargetPlatform};
use blaze_resolve::Cfg;
use blaze_wire::{
    BuildSpec, Command, ConfigKey, ConfigKeyInput, Digest, Encoder, ExecPlatform, FrontEndKey,
    FrontEndKeyInput, FrontEndUnit, JobContent, JobContentInput, Namespace, RunSpec, Set,
    SourceRef, StoreIdentity, UnitSpec, Wire, digest_of,
};
use sha2::{Digest as _, Sha256};

use crate::Refusal;
use crate::config;
use crate::environment;
use crate::flags::{Request, Subcommand};
use crate::platform::{self, Machine, Probe};
use crate::profile::{self, Settings};
use crate::refuse::Reason;
use crate::rustflags;

/// What a blaze is, as far as a spec is concerned: the machine it builds for, which is the
/// machine it runs on.
#[derive(Clone, Debug)]
pub struct Host {
    /// The target triple of the blaze binary: no cross builds.
    pub triple: String,
    /// What `rustc --print cfg` says of the triple, which `[target.'cfg(..)']` tables and
    /// dependencies are matched against.
    pub cfg: Vec<Cfg>,
    /// The target CPU string the code generators assume, which `ExecPlatform.cpu` names.
    pub cpu: String,
    /// Whether this blaze was built with the llvm backend.
    pub llvm: bool,
}

/// What a spec is made in: where the command ran, with what, and the facts about this blaze that
/// the command's flags and files cannot say.
pub struct Context<'a> {
    /// Materialized root of `source` (Git repository root, or the Root snapshot).
    /// None means the selected workspace itself is the declared source root.
    pub source_root: Option<&'a Path>,
    /// The directory the command was run in, absolute.
    pub cwd: &'a Path,
    /// The requester's environment, all of it: what is refused, carried or ignored is decided
    /// here.
    pub env: &'a BTreeMap<String, String>,
    /// The machine this blaze builds for.
    pub host: &'a Host,
    /// Which tree the spec builds.
    pub source: SourceRef,
    /// The identity of the store and toolchain this blaze builds with.
    pub toolchain: StoreIdentity,
    /// Whether blaze itself is offline, as `--offline` makes the command.
    pub offline: bool,
    /// Where blaze keeps `.crate` files by their sha256, for the resolution.
    pub store: Option<Arc<dyn blaze_registry::Blobs>>,
    /// What the platform is read through: the real machine, or a fake of one.
    pub probe: &'a dyn Probe,
}

/// A spec, its platform and its keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// The spec that goes on the wire.
    pub build: BuildSpec,
    /// The platform [`BuildSpec::platform`] is the digest of.
    pub platform: ExecPlatform,
    /// `ExecPlatform`'s digest.
    pub platform_digest: Digest,
    /// `BuildSpec`'s digest.
    pub digest: Digest,
    /// Which warm front end can take a job for this spec.
    pub front_end: FrontEndKey,
    /// What retention groups the spec by.
    pub config: ConfigKey,
    /// The names in `config_env` whose value is a path relative to the workspace root, which a
    /// worker joins to its own root; the wire has no field to say so.
    pub rebased_env: Vec<String>,
}

impl Spec {
    /// What merges duplicates of a job: this spec in `namespace`.
    pub fn job_content(&self, namespace: Namespace) -> JobContent {
        JobContent::of(&JobContentInput { spec: self.digest, namespace })
    }
}

/// The spec of `request` in `ctx`: refuses what blaze cannot honour, makes the unit graph and
/// assembles the spec from it.
pub fn build_spec(request: &Request, ctx: &Context<'_>) -> Result<Spec, Refusal> {
    let command = request.command.unwrap_or(Subcommand::Build);
    let offline = Opened::with(request, ctx, |opened| Ok(opened.resolved.offline))?;
    let (build, targets) = (&build_of(request), &targets_of(request, command));
    let asked = Asked {
        cwd: ctx.cwd,
        env: ctx.env.iter().map(|(name, value)| (name.clone(), value.clone())).collect(),
        manifest_path: request.manifest_path.as_deref().map(Path::new),
        config: &request.config,
        features: &request.features,
        all_features: request.all_features,
        no_default_features: request.no_default_features,
        build: Some(Build {
            packages: &build.packages,
            workspace: build.workspace,
            excludes: &build.excludes,
            dev_units: build.dev_units,
        }),
        platform: Some(TargetPlatform { triple: &ctx.host.triple, cfg: &ctx.host.cfg }),
        offline: request.offline || ctx.offline || offline,
        store: ctx.store.clone(),
    };
    let wanted = UnitsAsked {
        asked: &asked,
        command: match command {
            Subcommand::Check => units::Command::Check,
            Subcommand::Build | Subcommand::Run => units::Command::Build,
            Subcommand::Test => units::Command::Test,
        },
        targets,
        profile: request.profile.as_deref(),
    };
    let graph = units::unit_graph(&wanted).map_err(|error| {
        Refusal::with(Reason::UnitGraphFailed, "the unit graph", format!("{error:#}"))
    })?;
    assemble(request, ctx, &graph)
}

/// The members a command builds and whether it builds what the dev-dependencies are for.
struct Selected {
    packages: Vec<String>,
    workspace: bool,
    excludes: Vec<String>,
    dev_units: bool,
}

fn build_of(request: &Request) -> Selected {
    Selected {
        packages: request.packages.clone(),
        workspace: request.workspace,
        excludes: request.excludes.clone(),
        dev_units: request.needs_dev_dependencies(),
    }
}

/// The targets the unit graph is asked for. Doc tests are of the libraries.
fn targets_of(request: &Request, command: Subcommand) -> TargetFilter {
    if command == Subcommand::Test && request.doc {
        return TargetFilter { doc_only: true, ..TargetFilter::default() };
    }
    request.targets.clone()
}

/// Whether a command runs doc tests, and which: 0 none, 1 with the unit tests (`cargo test`
/// with no flag that selects targets), 2 only them (`--doc`).
fn doc_tests(request: &Request, command: Subcommand) -> u8 {
    match command {
        Subcommand::Test if request.doc => 2,
        Subcommand::Test if !request.targets.explicit => 1,
        _ => 0,
    }
}

/// The configuration and the workspace of a request, loaded and checked.
struct Opened<'m> {
    manifests: &'m blaze_manifest::Context,
    workspace: Workspace<'m>,
    resolved: config::Resolved,
}

impl Opened<'_> {
    /// Loads what the request's directory and environment have, refuses what blaze cannot
    /// honour of them, and gives `with` the workspace and what the configuration resolves to.
    fn with<T>(
        request: &Request,
        ctx: &Context<'_>,
        with: impl FnOnce(&Opened<'_>) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let command = request.command.unwrap_or(Subcommand::Build);
        environment::check(ctx.env)?;
        let rejected = |what: &str, error: anyhow::Error| {
            Refusal::with(Reason::ConfigRejected, what, format!("{error:#}"))
        };
        let mut config = Config::load(ctx.cwd, ctx.env.clone(), &[])
            .map_err(|error| rejected("the configuration", error))?;
        config
            .start(command.name(), &request.config)
            .map_err(|error| rejected("--config", error))?;
        let manifests = blaze_manifest::Context::new(config);
        let manifest = root_manifest(request.manifest_path.as_deref().map(Path::new), &manifests)
            .map_err(|error| rejected("the manifest", error))?;
        let workspace = Workspace::new(&manifest, &manifests)
            .map_err(|error| rejected("the workspace", error))?;
        let resolved = config::check(
            &manifests.config,
            ctx.host,
            command,
            doc_tests(request, command) != 0,
            workspace.root(),
            ctx.env,
        )?;
        with(&Opened { manifests: &manifests, workspace, resolved })
    }
}

/// The spec of `request` in `ctx` over the unit graph `graph`.
///
/// This is [`build_spec`] with the graph given, so that a spec is made without resolving
/// anything: tests hand it graphs they build.
pub fn assemble(request: &Request, ctx: &Context<'_>, graph: &UnitGraph) -> Result<Spec, Refusal> {
    let command = request.command.unwrap_or(Subcommand::Build);
    let host = ctx.host;
    Opened::with(request, ctx, |opened| {
        let manifests = opened.manifests;
        let settings = Settings::of(ctx.env, host.llvm)?;
        let words = rustflags::words(&manifests.config, host)?;
        let flags = rustflags::read(&words)?;

        let default = if command == Subcommand::Test { "test" } else { "dev" };
        let requested = requested_profile(default, false, request.profile.as_deref())
            .map_err(|error| Refusal::with(Reason::BadValue, "--profile", format!("{error:#}")))?;
        let profiles = Profiles::new(
            opened.workspace.profiles(),
            opened.workspace.unstable_features(),
            &manifests.config,
            &requested,
            &host.triple,
        )
        .map_err(|error| {
            Refusal::with(Reason::ConfigRejected, "the profiles", format!("{error:#}"))
        })?;
        let mut base = profiles.base_profile();
        base.name = requested;
        let top = profile::spec(&base, &flags.overrides, &settings, "the command")?;

        let units = unit_specs(graph, &flags, &settings)?;
        let asked = asked_units(request, command, graph, &units)?;

        let mut machine_env = ctx.env.clone();
        machine_env.extend(opened.resolved.config_env.iter().cloned());
        let machine =
            Machine { triple: &host.triple, cpu: &host.cpu, env: &machine_env, linker: None };
        let platform = platform::exec_platform(&machine, ctx.probe)?;
        let platform_digest = digest_of(&platform);

        let root = opened.workspace.root();
        let cwd = relative_cwd(ctx.cwd, root)?;
        let source_root = ctx.source_root.unwrap_or(root);
        let manifest = relative_cwd(opened.workspace.root_manifest(), source_root)?;
        let workspace_manifest = blaze_wire::SourcePath::new(manifest)
            .map_err(|error| Refusal::new("workspace manifest", error.to_string()))?;
        let lock: Digest = Sha256::digest(&graph.lock).into();
        let toolchain = ctx.toolchain;

        let build = BuildSpec {
            source: ctx.source.clone(),
            lock,
            units: units.specs,
            asked: Set::new(asked),
            profile: top,
            platform: platform_digest,
            rustflags: flags.flags.clone(),
            config_env: Set::new(opened.resolved.config_env.clone()),
            script_env: Set::new(environment::script_env(ctx.env, &host.triple)),
            cwd,
            command: command_of(request, command),
            toolchain,
            workspace_manifest,
        };
        let digest = digest_of(&build);
        let front_end = front_end_key(&build, &flags, &host.triple);
        let config = config_key(&build, kind_of(command));
        Ok(Spec {
            build,
            platform,
            platform_digest,
            digest,
            front_end,
            config,
            rebased_env: opened.resolved.rebased.clone(),
        })
    })
}

/// The units of a spec, in the canonical order.
struct Units {
    specs: Vec<UnitSpec>,
    /// The canonical index of each unit of the graph that is one of the spec's.
    of_graph: Vec<Option<u32>>,
}

/// The kind of a target as a spec spells it.
fn kind_code(kind: TargetKind) -> u8 {
    match kind {
        TargetKind::Lib => 0,
        TargetKind::Bin => 1,
        TargetKind::Test => 2,
        TargetKind::Example => 3,
        TargetKind::Bench => 4,
        TargetKind::CustomBuild => 5,
        TargetKind::ProcMacro => 6,
    }
}

/// A unit as a refusal names it.
fn describe(unit: &Unit) -> String {
    format!("unit `{} {}` ({})", unit.package.name, unit.package.version, unit.target.name)
}

/// The bytes the canonical order sorts a unit by: its canonical CBOR without field 5, the
/// dependencies, which are indexes in the order this decides.
pub fn canonical_key(unit: &UnitSpec) -> Vec<u8> {
    let mut e = Encoder::new();
    e.map(8);
    e.uint(0);
    unit.package.encode(&mut e);
    e.uint(1);
    unit.target.encode(&mut e);
    e.uint(2);
    unit.features.encode(&mut e);
    e.uint(3);
    unit.cfg.encode(&mut e);
    e.uint(4);
    unit.edition.encode(&mut e);
    e.uint(6);
    unit.test.encode(&mut e);
    e.uint(7);
    unit.host.encode(&mut e);
    e.uint(8);
    unit.profile.encode(&mut e);
    e.into_bytes()
}

/// The unit a dependency on `index` stands for: the unit itself, or for a run of a build
/// script the unit that compiles the script.
fn stands_for(graph: &UnitGraph, index: usize) -> Result<usize, Refusal> {
    let Some(unit) = graph.units.get(index) else {
        return Err(Refusal::with(
            Reason::UnitEdge,
            format!("unit {index}"),
            "it is not in the graph",
        ));
    };
    if unit.mode != Mode::RunCustomBuild {
        return Ok(index);
    }
    unit.dependencies
        .iter()
        .map(|dep| dep.index)
        .find(|&dep| {
            graph.units.get(dep).is_some_and(|it| {
                it.target.kind == TargetKind::CustomBuild && it.mode != Mode::RunCustomBuild
            })
        })
        .ok_or_else(|| {
            Refusal::with(
                Reason::UnitEdge,
                describe(unit),
                "it depends on no compile of the script",
            )
        })
}

fn unit_specs(
    graph: &UnitGraph,
    flags: &rustflags::Rustflags,
    settings: &Settings,
) -> Result<Units, Refusal> {
    // The units without their dependencies, which the order does not depend on.
    let mut bare: Vec<(usize, UnitSpec)> = Vec::new();
    for (index, unit) in graph.units.iter().enumerate() {
        if unit.mode == Mode::RunCustomBuild {
            continue;
        }
        let profile = profile::spec(&unit.profile, &flags.overrides, settings, &describe(unit))?;
        let kind = unit.target.kind;
        // The graph distinguishes normal and test checks of the same library or
        // binary. Target kind alone loses that distinction. A custom harness still
        // receives cfg(test), but cargo uses --cfg test rather than --test for it.
        let test = unit.test && unit.target.harness;
        let mut cfg: Vec<String> =
            unit.features.iter().map(|it| format!("feature=\"{it}\"")).collect();
        if unit.test {
            cfg.push("test".to_owned());
        }
        if kind == TargetKind::ProcMacro {
            cfg.push("proc_macro".to_owned());
        }
        if profile.debug_assertions {
            cfg.push("debug_assertions".to_owned());
        }
        if profile.overflow_checks {
            cfg.push("overflow_checks".to_owned());
        }
        cfg.push(format!("panic=\"{}\"", if profile.panic == 1 { "abort" } else { "unwind" }));
        cfg.extend(flags.cfgs.iter().cloned());
        bare.push((
            index,
            UnitSpec {
                package: (
                    unit.package.name.clone(),
                    unit.package.version.clone(),
                    Sha256::digest(unit.package.source.as_bytes()).into(),
                ),
                target: (kind_code(kind), unit.target.name.clone()),
                features: Set::new(unit.features.clone()),
                cfg: Set::new(cfg),
                edition: unit.target.edition,
                deps: Set::new(Vec::new()),
                test,
                host: unit.for_host,
                profile,
            },
        ));
    }
    let mut keyed: Vec<(Vec<u8>, usize, UnitSpec)> =
        bare.into_iter().map(|(index, spec)| (canonical_key(&spec), index, spec)).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    for pair in keyed.windows(2) {
        if pair[0].0 == pair[1].0 {
            let (a, b) = (&graph.units[pair[0].1], &graph.units[pair[1].1]);
            let what = format!("{} and {}", describe(a), describe(b));
            return Err(Refusal::of(Reason::UnitOrder, what));
        }
    }
    let mut of_graph = vec![None; graph.units.len()];
    for (canonical, (_, index, _)) in keyed.iter().enumerate() {
        of_graph[*index] = Some(canonical as u32);
    }
    let mut specs = Vec::with_capacity(keyed.len());
    for (_, index, mut spec) in keyed {
        let mut deps = Vec::new();
        for dep in &graph.units[index].dependencies {
            let at = stands_for(graph, dep.index)?;
            let canonical = of_graph[at].ok_or_else(|| {
                Refusal::with(
                    Reason::UnitEdge,
                    describe(&graph.units[index]),
                    "it depends on a unit that is not one",
                )
            })?;
            deps.push((canonical, dep.extern_crate_name.clone()));
        }
        spec.deps = Set::new(deps);
        specs.push(spec);
    }
    Ok(Units { specs, of_graph })
}

/// The units the command asked for, as indexes in the canonical order.
///
/// They are the graph's roots; for `cargo run` the one binary or example among them, and for
/// `cargo test --doc` the libraries of the members the packages select, which the graph has
/// built for the doc tests to link against.
fn asked_units(
    request: &Request,
    command: Subcommand,
    graph: &UnitGraph,
    units: &Units,
) -> Result<Vec<u32>, Refusal> {
    let mut asked: Vec<usize> = Vec::new();
    if command == Subcommand::Test && request.doc {
        for &index in &graph.roots {
            let unit = &graph.units[index];
            let library = matches!(unit.target.kind, TargetKind::Lib | TargetKind::ProcMacro)
                && unit.mode == Mode::Build
                && unit.target.doctest;
            if library {
                asked.push(index);
            }
        }
        if asked.is_empty() {
            return Err(Refusal::of(Reason::DocLibrary, "--doc"));
        }
    } else {
        for &root in &graph.roots {
            asked.push(stands_for(graph, root)?);
        }
    }
    if command == Subcommand::Run {
        asked.retain(|&it| {
            matches!(graph.units[it].target.kind, TargetKind::Bin | TargetKind::Example)
        });
        if asked.len() != 1 {
            let what = format!("cargo run: {} programs", asked.len());
            return Err(Refusal::with(Reason::RunTarget, what, "name one with --bin or --example"));
        }
    }
    if asked.is_empty() {
        return Err(Refusal::with(
            Reason::UnitGraphFailed,
            "the unit graph",
            "it has no unit to build",
        ));
    }
    let mut at: Vec<u32> = Vec::new();
    for index in asked {
        let canonical = units.of_graph.get(index).copied().flatten();
        at.push(canonical.ok_or_else(|| {
            Refusal::with(
                Reason::UnitEdge,
                format!("unit {index}"),
                "it is asked for and is not one",
            )
        })?);
    }
    at.sort_unstable();
    at.dedup();
    Ok(at)
}

/// `cwd` below the workspace `root`, with `/` for separators and `.` for the root itself.
fn relative_cwd(cwd: &Path, root: &Path) -> Result<String, Refusal> {
    let normal = |path: &Path| -> PathBuf { blaze_manifest::paths::normalize_path(path) };
    let Ok(below) = normal(cwd).strip_prefix(normal(root)).map(Path::to_owned) else {
        return Err(Refusal::with(
            Reason::Cwd,
            cwd.display().to_string(),
            format!("the workspace root is {}", root.display()),
        ));
    };
    let parts: Vec<String> =
        below.components().map(|it| it.as_os_str().to_string_lossy().into_owned()).collect();
    Ok(if parts.is_empty() { ".".to_owned() } else { parts.join("/") })
}

/// The command of a spec, with what a run is asked to do.
fn command_of(request: &Request, command: Subcommand) -> Command {
    match command {
        Subcommand::Check => Command::Check,
        Subcommand::Build => Command::Build,
        Subcommand::Test => Command::Test(RunSpec {
            filters: request.positional.clone(),
            exact: request.libtest.exact,
            skip: request.libtest.skip.clone(),
            ignored: request.libtest.ignored,
            no_run: request.no_run,
            no_fail_fast: request.no_fail_fast,
            test_threads: request.libtest.test_threads,
            harness_args: request.trailing.clone(),
            env: Set::new(Vec::new()),
            doc: doc_tests(request, command),
        }),
        Subcommand::Run => Command::Run(RunSpec {
            filters: Vec::new(),
            exact: false,
            skip: Vec::new(),
            ignored: 0,
            no_run: false,
            no_fail_fast: false,
            test_threads: None,
            harness_args: request.positional.iter().chain(&request.trailing).cloned().collect(),
            env: Set::new(Vec::new()),
            doc: 0,
        }),
    }
}

/// `Command`'s discriminant.
fn kind_of(command: Subcommand) -> u8 {
    match command {
        Subcommand::Check => 0,
        Subcommand::Build => 1,
        Subcommand::Test => 2,
        Subcommand::Run => 3,
    }
}

/// What a warm front end depends on: the units without their sources, the lock, the cfgs the
/// rustflags set, the toolchain and the triple.
fn front_end_key(build: &BuildSpec, flags: &rustflags::Rustflags, triple: &str) -> FrontEndKey {
    let units = build
        .units
        .iter()
        .map(|unit| FrontEndUnit {
            name: unit.package.0.clone(),
            version: unit.package.1.clone(),
            target: unit.target.clone(),
            features: unit.features.clone(),
            cfg: unit.cfg.clone(),
            edition: unit.edition,
            deps: unit.deps.clone(),
            test: unit.test,
            host: unit.host,
            profile: unit.profile.clone(),
        })
        .collect();
    FrontEndKey::of(&FrontEndKeyInput {
        units,
        lock: build.lock,
        cfg_rustflags: Set::new(flags.cfg_flags()),
        toolchain: build.toolchain,
        triple: triple.to_owned(),
        workspace_manifest: build.workspace_manifest.clone(),
    })
}

/// The spec without what changes from commit to commit.
fn config_key(build: &BuildSpec, kind: u8) -> ConfigKey {
    let asked = || build.asked.iter().map(|&it| &build.units[it as usize]);
    ConfigKey::of(&ConfigKeyInput {
        kind,
        asked: Set::new(
            asked().map(|unit| (unit.package.0.clone(), unit.target.1.clone())).collect(),
        ),
        profiles: asked().map(|unit| unit.profile.clone()).collect(),
        platform: build.platform,
        rustflags: Set::new(build.rustflags.clone()),
        toolchain: build.toolchain,
        workspace_manifest: build.workspace_manifest.clone(),
    })
}
