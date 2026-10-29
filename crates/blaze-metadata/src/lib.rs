//! Owned metadata and compile-unit resolution, with no Cargo subprocess.
//!
//! blaze-manifest reads the workspace, blaze-resolve chooses versions from blaze-registry,
//! and the same resolved package facts feed metadata, producer feature domains and the
//! canonical saved unit graph. Missing or stale locks are resolved and published under
//! the requested locked/offline policy. Captured replay uses only authenticated roots.

mod facts;
mod graph;
mod registry;
mod summary;
pub mod units;

pub use facts::{DependencyFacts, PackageFacts, ResolverFacts};
pub use units::{ResolvedUnit, UnitDependency};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail, ensure};
use blaze_manifest::config::RegistryConfig;
use blaze_manifest::manifest::{EitherManifest, Manifest, read_manifest};
use blaze_manifest::metadata::{SerializedPackage, export, export_package, root_manifest};
use blaze_manifest::{Config, Context, Workspace};
use blaze_resolve::lockfile::{self, PathDeps};
use blaze_resolve::metadata::{MetadataPackages, resolve_graph};
use blaze_resolve::resolver::{
    CliFeatures, CompileKind, FeatureOpts, FeatureResolver, ForceAllTargets, HasDevUnits, Members,
    PackageTargets, ResolveBehavior, VersionOrdering, members_with_features,
};
use blaze_resolve::workspace_resolve::{PackageRegistry, resolve_ws};
use blaze_resolve::{
    Cfg, IndexSummary, InternedString, MemRegistry, PackageId, PackageIdSpec, Platform,
    ResolveVersion, SourceId, Summary,
};
use rustc_hash::FxHashMap;

use crate::summary::Read;

/// What is asked of the workspace, as `cargo metadata` or `cargo build` is asked.
pub struct Asked<'a> {
    /// Actual native Cargo-home discovery path, when supplied by a native-env caller.
    pub cargo_home: Option<PathBuf>,
    /// The directory cargo would be run in, and its variables.
    pub cwd: &'a Path,
    pub env: Vec<(String, String)>,
    pub manifest_path: Option<&'a Path>,
    /// `--config` arguments.
    pub config: &'a [String],
    pub features: &'a [String],
    pub all_features: bool,
    pub no_default_features: bool,
    /// The build whose graph is wanted; none for `cargo metadata`'s.
    pub build: Option<Build<'a>>,
    /// `--filter-platform`: the target's triple and its `cfg`.
    pub platform: Option<TargetPlatform<'a>>,
    pub offline: bool,
    /// `--locked`; combine with `offline` for `--frozen`.
    pub locked: bool,
    /// blaze's store, which keeps `.crate` files by their sha256.
    pub store: Option<Arc<dyn blaze_registry::Blobs>>,
}

/// The members a `cargo build` builds, as its flags select them.
pub struct Build<'a> {
    /// `-p`: the members by name.
    pub packages: &'a [String],
    pub workspace: bool,
    pub excludes: &'a [String],
    /// Whether their tests, examples and benchmarks are built too, and so their
    /// dev-dependencies.
    pub dev_units: bool,
}

pub struct TargetPlatform<'a> {
    pub triple: &'a str,
    pub cfg: &'a [Cfg],
}

/// The captured directory of one non-path package named by `Cargo.lock`.
///
/// A replay authenticates these mappings and their directory contents before passing
/// them to [`units::unit_graph_captured`]. The original source spelling, including a
/// Git revision, is preserved; a captured package never becomes a path dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedSource {
    /// The exact source URL from the lock file, including a Git source's precise revision.
    pub source: String,
    /// The locked package name.
    pub name: String,
    /// The locked package version.
    pub version: String,
    /// The absolute directory containing the captured `Cargo.toml`.
    pub root: PathBuf,
}

/// Authenticated source roots and resolver facts for one closed replay.
#[derive(Clone, Copy)]
pub struct CapturedResolution<'a> {
    pub sources: &'a [CapturedSource],
    pub facts: &'a ResolverFacts,
}

impl TargetPlatform<'_> {
    fn activates(&self, platform: &Platform) -> bool {
        platform.matches(self.triple, self.cfg)
    }
}

/// A package read for the metadata: where its manifest is, the manifest, and the source the
/// lock file pins it to when it is not a path package.
struct Package {
    manifest_path: PathBuf,
    manifest: Box<Manifest>,
    locked_source: Option<String>,
}

/// The root manifest of the workspace the manifest at `manifest_path` is in, as
/// `cargo locate-project --workspace` finds it from `cwd` with the variables `env`.
pub fn workspace_root(
    cwd: &Path,
    env: Vec<(String, String)>,
    manifest_path: &Path,
) -> Result<PathBuf> {
    let ctx = Context::new(Config::load(cwd, env, &[])?);
    let ws = Workspace::new(&root_manifest(Some(manifest_path), &ctx)?, &ctx)?;
    Ok(ws.root_manifest().to_owned())
}

/// A program a `cargo build` links: a binary or an example of a member it builds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Program {
    /// The id of its package, as `cargo metadata` spells it.
    pub package: String,
    pub name: String,
    /// The root file of its crate.
    pub root: PathBuf,
    pub example: bool,
}

/// The program a build names with `--bin` or `--example`, or none for every binary.
#[derive(Clone, Copy, Default)]
pub struct Targets<'a> {
    pub bin: Option<&'a str>,
    pub example: Option<&'a str>,
}

/// The JSON `cargo metadata --format-version 1` prints for `asked`.
pub fn metadata(asked: &Asked<'_>) -> Result<String> {
    Ok(made(asked, None, None)?.0)
}

/// Metadata with the structural source bytes that produced it. A daemon must not certify
/// this graph with manifest bytes read after a build script has run.
pub fn metadata_with_sources(asked: &Asked<'_>) -> Result<(String, Vec<(PathBuf, Vec<u8>)>)> {
    let (json, _, sources) = made(asked, None, None)?;
    Ok((json, sources))
}

/// Resolve an application's build without folding host and target feature domains.
/// Target facts are supplied by the owned planner, never queried from a toolchain.
pub fn build_units(
    asked: &Asked<'_>,
    host: &TargetPlatform<'_>,
) -> Result<(String, Vec<ResolvedUnit>, Vec<(PathBuf, Vec<u8>)>)> {
    anyhow::ensure!(asked.build.is_some(), "compile units require a build selection");
    let mut units = Vec::new();
    let (json, _, sources) = made(asked, Some(host), Some(&mut units))?;
    Ok((json, units, sources))
}

/// The metadata of `asked`'s build, and the programs it links of `targets`, as cargo's unit
/// graph has them: of each member the build selects, the binaries whose `required-features`
/// it is built with, or the one target named.
pub fn programs(asked: &Asked<'_>, targets: Targets<'_>) -> Result<(String, Vec<Program>)> {
    anyhow::ensure!(asked.build.is_some(), "the programs are of a build");
    let (json, roots, _) = made(asked, None, None)?;
    let metadata: serde_json::Value = serde_json::from_str(&json)?;
    let nodes = metadata["resolve"]["nodes"].as_array().into_iter().flatten();
    let features: FxHashMap<&str, Vec<&str>> = nodes
        .filter_map(|node| {
            let on = node["features"].as_array()?.iter().filter_map(|it| it.as_str());
            Some((node["id"].as_str()?, on.collect()))
        })
        .collect();
    let packages = metadata["packages"].as_array().into_iter().flatten();
    let mut programs = Vec::new();
    for package in packages {
        let Some(id) = package["id"].as_str().filter(|id| roots.iter().any(|it| it == id)) else {
            continue;
        };
        let on = features.get(id).map(Vec::as_slice).unwrap_or_default();
        for target in package["targets"].as_array().into_iter().flatten() {
            let of = |key: &str, it: &str| {
                target[key].as_array().is_some_and(|all| all.iter().any(|kind| kind == it))
            };
            let only_bin = target["crate_types"].as_array().is_some_and(|it| it == &["bin"]);
            let (bin, example) = (of("kind", "bin") && only_bin, of("kind", "example") && only_bin);
            let name = target["name"].as_str().unwrap_or_default();
            let named = match targets {
                Targets { bin: Some(asked), .. } => bin && name == asked,
                Targets { example: Some(asked), .. } => example && name == asked,
                Targets { bin: None, example: None } => bin,
            };
            if !named {
                continue;
            }
            let required = target["required-features"].as_array().into_iter().flatten();
            // A feature of a dependency (`dep/feature`) is taken to be on.
            let missing: Vec<&str> = required
                .filter_map(|it| it.as_str())
                .filter(|it| !it.contains('/') && !on.contains(it))
                .collect();
            let asked_for = targets.bin.is_some() || targets.example.is_some();
            if !missing.is_empty() && asked_for {
                let listed: Vec<String> = missing.iter().map(|it| format!("`{it}`")).collect();
                bail!(
                    "target `{name}` in package `{}` requires the features: {}\nConsider enabling them by passing, e.g., `--features=\"{}\"`",
                    package["name"].as_str().unwrap_or_default(),
                    listed.join(", "),
                    missing.join(" ")
                );
            }
            if missing.is_empty() {
                programs.push(Program {
                    package: id.to_owned(),
                    name: name.to_owned(),
                    root: PathBuf::from(target["src_path"].as_str().unwrap_or_default()),
                    example,
                });
            }
        }
    }
    Ok((json, programs))
}

/// The metadata of `asked`, and the ids of the members its build selects.
fn made(
    asked: &Asked<'_>,
    host: Option<&TargetPlatform<'_>>,
    units: Option<&mut Vec<ResolvedUnit>>,
) -> Result<(String, Vec<String>, Vec<(PathBuf, Vec<u8>)>)> {
    let (ctx, offline) = open(asked, "metadata", None)?;
    let ws = Workspace::new(&root_manifest(asked.manifest_path, &ctx)?, &ctx)?;
    let member_ids = member_ids(&ws)?;
    let specs: Vec<PackageIdSpec> = match &asked.build {
        None => member_ids.iter().map(|id| id.to_spec()).collect(),
        Some(build) => selected(&ws, &member_ids, build)?,
    };
    let cli_features = CliFeatures::from_command_line(
        asked.features,
        asked.all_features,
        !asked.no_default_features,
    )?;
    let has_dev_units = match &asked.build {
        Some(build) if !build.dev_units => HasDevUnits::No,
        _ => HasDevUnits::Yes,
    };
    let selection = ResolutionSelection::new(&specs, &cli_features, has_dev_units);
    let locked = Locked::read(&ws, &ctx, asked, offline, &selection)?;
    let world = World::new(&ws, &locked)?;
    let targeted = world.resolve(&ws, &ctx, &specs, &cli_features, has_dev_units)?.targeted;

    let kinds = match &asked.platform {
        Some(platform) => vec![CompileKind::Target(InternedString::new(platform.triple))],
        None => vec![CompileKind::Host],
    };
    let packages = Packages { reads: &world.reads, platform: asked.platform.as_ref(), host };
    let mut roots = Vec::new();
    let mut resolved_units = Vec::new();
    let resolve = match &asked.build {
        None => resolve_graph(&targeted, &member_ids, world.current, &packages, &kinds)?,
        Some(build) => {
            let summaries = world.member_summaries();
            let member_features =
                members_with_features(&world.members(&summaries), &specs, &cli_features)?;
            let opts = FeatureOpts::new(world.behavior, has_dev_units, ForceAllTargets::No);
            let features =
                FeatureResolver::resolve(&packages, &targeted, &member_features, &kinds, opts)?;
            roots = member_features.iter().map(|(it, _)| it.package_id()).collect();
            if units.is_some() {
                resolved_units = units::of_build(
                    &targeted,
                    &features,
                    &roots,
                    build.dev_units,
                    &world.reads,
                    &packages,
                    kinds[0],
                )?;
            }

            let activated = |platform: &Platform| packages.activates(platform);
            graph::of_build(
                &targeted,
                &features,
                &roots,
                build.dev_units,
                &world.reads,
                &activated,
                world.current,
            )?
        }
    };

    // The packages the graph reaches, in the order of their ids, as cargo lists them.
    let reached: FxHashMap<PackageId, usize> =
        world.ids.iter().enumerate().map(|(at, id)| (*id, at)).collect();
    let mut listed: Vec<(PackageId, usize)> = Vec::new();
    for node in &resolve.nodes {
        let id = world.ids.iter().find(|id| id.to_spec() == node.id).copied();
        let id = id.with_context(|| format!("no package {} was read", node.id))?;
        listed.push((id, reached[&id]));
    }
    // Target-folded metadata may omit a dependency selected only by a host predicate.
    // Its manifest was already read by this resolution; retain its source package too.
    for unit in &resolved_units {
        let id = world.ids.iter().find(|id| id.to_spec().to_string() == unit.package).copied();
        let id = id.with_context(|| format!("no unit package {} was read", unit.package))?;
        if !listed.iter().any(|(listed, _)| *listed == id) {
            listed.push((id, reached[&id]));
        }
    }
    listed.sort();
    let mut slots: Vec<Option<SerializedPackage<'_>>> =
        world.serialized.into_iter().map(Some).collect();
    let packages: Vec<SerializedPackage<'_>> =
        listed.iter().filter_map(|(_, at)| slots[*at].take()).collect();
    let json = serde_json::to_string(&export(&ws, packages, Some(resolve)))?;
    if let Some(units) = units {
        *units = resolved_units;
    }
    Ok((json, roots.iter().map(|it| it.to_spec().to_string()).collect(), locked.inputs(&ctx)))
}

/// The configuration `asked` is read with, for the command cargo would be run as, and whether
/// the run is offline.
fn open(
    asked: &Asked<'_>,
    command: &str,
    discovery_root: Option<&Path>,
) -> Result<(Context, bool)> {
    let mut config = match &asked.cargo_home {
        Some(home) => Config::load_with_home(asked.cwd, asked.env.clone(), &[], home.clone())?,
        None => Config::load(asked.cwd, asked.env.clone(), &[])?,
    };
    if let Some(root) = discovery_root {
        config = config.with_discovery_root(root)?;
    }
    config.start(command, asked.config)?;
    let offline = asked.offline || config.net_config()?.offline.unwrap_or(false);
    Ok((Context::new(config), offline))
}

/// The source selection used by both metadata and the canonical unit graph.
#[derive(Clone, PartialEq)]
struct ResolutionSelection {
    specs: Vec<PackageIdSpec>,
    features: CliFeatures,
    dev: HasDevUnits,
}

impl ResolutionSelection {
    fn new(specs: &[PackageIdSpec], features: &CliFeatures, dev: HasDevUnits) -> Self {
        Self { specs: specs.to_vec(), features: features.clone(), dev }
    }
}

/// The resolved lock and package manifests shared by metadata, producers and saved graphs.
struct Locked {
    lock_path: PathBuf,
    lock_text: String,
    lock: blaze_resolve::Resolve,
    targeted: blaze_resolve::Resolve,
    facts: ResolverFacts,
    selection: ResolutionSelection,
    rewritten: bool,
    path_packages: Vec<Package>,
    fetched: Vec<Package>,
    sources: Vec<CapturedSource>,
}

impl Locked {
    fn read(
        ws: &Workspace<'_>,
        ctx: &Context,
        asked: &Asked<'_>,
        offline: bool,
        selection: &ResolutionSelection,
    ) -> Result<Self> {
        Self::read_with_sources(ws, ctx, asked, offline, selection, None)
    }

    fn read_captured(
        ws: &Workspace<'_>,
        ctx: &Context,
        asked: &Asked<'_>,
        offline: bool,
        selection: &ResolutionSelection,
        captured: Option<CapturedResolution<'_>>,
    ) -> Result<Self> {
        Self::read_with_sources(ws, ctx, asked, offline, selection, captured)
    }

    fn read_with_sources(
        ws: &Workspace<'_>,
        ctx: &Context,
        asked: &Asked<'_>,
        offline: bool,
        selection: &ResolutionSelection,
        captured: Option<CapturedResolution<'_>>,
    ) -> Result<Self> {
        let root = ws.root();
        if summary::has_replace(root, ctx)? {
            bail!("`[replace]` is not read by blaze yet");
        }
        let lock_path = root.join("Cargo.lock");
        let previous = match ctx.read_source(&lock_path) {
            Ok(bytes) => Some(
                String::from_utf8(bytes)
                    .with_context(|| format!("failed to read {}", lock_path.display()))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", lock_path.display()));
            }
        };
        let path_packages = path_packages(ws, root, ctx)?;
        let mut path_deps = PathDeps::default();
        let mut reads = FxHashMap::default();
        let mut ids = Vec::new();
        for (_, manifest) in ws.members() {
            let read = summary::native_manifest(manifest, None)?;
            let id = read.summary.package_id();
            ids.push(id);
            reads.insert(id, read);
        }
        let member_count = ids.len();
        for package in &path_packages {
            let read = summary::native_manifest(&package.manifest, None)?;
            let id = read.summary.package_id();
            ids.push(id);
            reads.insert(id, read);
        }
        for manifest in
            ws.members().map(|(_, m)| m).chain(path_packages.iter().map(|p| &*p.manifest))
        {
            let dir =
                manifest.package_id.source_id.local_path().context("a path package's directory")?;
            path_deps
                .entry(manifest.package_id.name.clone())
                .or_default()
                .insert(manifest.package_id.version.clone(), SourceId::for_path(&dir)?);
        }
        let previous_lock = previous
            .as_deref()
            .map(|text| {
                lockfile::parse(text)
                    .and_then(|parsed| lockfile::into_resolve(parsed, text, &path_deps))
                    .with_context(|| format!("failed to parse {}", lock_path.display()))
            })
            .transpose()?;
        if let Some(captured) = captured {
            let previous_lock =
                previous_lock.context("captured replay requires its authenticated Cargo.lock")?;
            let lock_text =
                previous.context("captured replay requires its authenticated Cargo.lock")?;
            let mut registry = MemRegistry::new();
            for read in reads.values() {
                registry.add(IndexSummary::Candidate(read.summary.clone()));
            }
            captured.facts.add_to(&mut registry, &previous_lock)?;
            let (lock, targeted, facts) = resolve_packages(
                ws,
                ctx,
                &reads,
                &ids[..member_count],
                registry,
                Some(&previous_lock),
                selection,
            )?;
            let current = lockfile::serialize_resolve(&lock, Some(&lock_text));
            let policy =
                lockfile::Locked { flag: "--locked", path: &lock_path, path_deps: &path_deps };
            ensure!(
                lockfile::are_equal_lockfiles(&lock_text, &current, Some(policy)),
                "{} would change: the manifests no longer agree with it",
                lock_path.display()
            );
            let (fetched, sources) = fetch(&targeted, ctx, FetchFrom::Captured(captured.sources))?;
            return Ok(Self {
                lock_path,
                lock_text,
                lock,
                targeted,
                facts,
                selection: selection.clone(),
                rewritten: false,
                path_packages,
                fetched,
                sources,
            });
        }
        let mut context = blaze_registry::Context::new(
            ctx.config.home().to_owned(),
            blaze_registry::http::HyperHttp::shared()?,
        );
        context.net.offline = offline;
        context.store = asked.store.clone();
        let context = Arc::new(context);
        let sources = blaze_registry::Sources::new(&source_config(&ctx.config)?, context.clone())?;
        let mut registry = MemRegistry::new();
        for read in reads.values() {
            registry.add(IndexSummary::Candidate(read.summary.clone()));
        }
        let registry = registry::SourcesRegistry::new(registry, &sources, &context, ctx);
        let (mut lock, targeted, facts) = resolve_packages(
            ws,
            ctx,
            &reads,
            &ids[..member_count],
            registry,
            previous_lock.as_ref(),
            selection,
        )?;
        let policy = asked.locked.then_some(lockfile::Locked {
            flag: "--locked",
            path: &lock_path,
            path_deps: &path_deps,
        });
        let lowest =
            ids[..member_count].iter().filter_map(|id| reads[id].summary.rust_version()).min();
        let new_lock = lockfile::lockfile_to_write(&mut lock, previous.as_deref(), lowest, policy)?;
        let rewritten = new_lock.is_some();
        if let Some(text) = &new_lock {
            write_lockfile(&lock_path, text)?;
        }
        let lock_text = new_lock.or(previous).context("owned resolution produced no lock file")?;
        let (fetched, sources) = fetch(&targeted, ctx, FetchFrom::Owned(&sources))?;
        Ok(Self {
            lock_path,
            lock_text,
            lock,
            targeted,
            facts,
            selection: selection.clone(),
            rewritten,
            path_packages,
            fetched,
            sources,
        })
    }

    fn inputs(&self, ctx: &Context) -> Vec<(PathBuf, Vec<u8>)> {
        let mut inputs = ctx.sources();
        if self.rewritten {
            inputs.retain(|(path, _)| path != &self.lock_path);
            inputs.push((self.lock_path.clone(), self.lock_text.as_bytes().to_vec()));
        }
        inputs
    }
}

fn resolve_packages<R: blaze_resolve::Registry>(
    ws: &Workspace<'_>,
    ctx: &Context,
    reads: &FxHashMap<PackageId, Read>,
    member_ids: &[PackageId],
    registry: R,
    previous: Option<&blaze_resolve::Resolve>,
    selection: &ResolutionSelection,
) -> Result<(blaze_resolve::Resolve, blaze_resolve::Resolve, ResolverFacts)> {
    let patch = summary::patch_tables(ws.root(), &registry, ctx)?;
    let by_path =
        reads.values().map(|read| (read.summary.source_id(), read.summary.clone())).collect();
    let members: Vec<_> = member_ids.iter().map(|id| &reads[id].summary).collect();
    let current =
        ws.members().position(|(path, _)| path == ws.current_manifest()).map(|at| member_ids[at]);
    let behavior = resolve_behavior(ws);
    let resolving = resolver_workspace(
        Members {
            members: &members,
            current,
            new_cli_feature_behavior: behavior != ResolveBehavior::V1 || ws.is_virtual(),
        },
        &by_path,
        &patch,
        behavior,
        ctx,
    )?;
    let mut registry = PackageRegistry::new(registry);
    let (whole, targeted) = resolve_ws(
        &mut registry,
        &resolving,
        previous,
        &selection.features,
        &selection.specs,
        selection.dev,
    )?;
    let facts = ResolverFacts::capture(&whole, registry.patches().values().flatten())?;
    Ok((whole, targeted, facts))
}

fn member_ids(ws: &Workspace<'_>) -> Result<Vec<PackageId>> {
    ws.members()
        .map(|(_, manifest)| Ok(summary::native_manifest(manifest, None)?.summary.package_id()))
        .collect()
}

fn resolve_behavior(ws: &Workspace<'_>) -> ResolveBehavior {
    match ws.resolve_behavior() {
        blaze_manifest::features::ResolveBehavior::V1 => ResolveBehavior::V1,
        blaze_manifest::features::ResolveBehavior::V2 => ResolveBehavior::V2,
        blaze_manifest::features::ResolveBehavior::V3 => ResolveBehavior::V3,
    }
}

fn resolver_workspace<'a>(
    members: Members<'a>,
    by_path: &'a FxHashMap<SourceId, Summary>,
    patch: &'a [(url::Url, Vec<blaze_resolve::workspace_resolve::Patch>)],
    behavior: ResolveBehavior,
    ctx: &Context,
) -> Result<blaze_resolve::workspace_resolve::Workspace<'a>> {
    let fallback = ctx.config.get_string("resolver.incompatible-rust-versions")?;
    let fallback = fallback
        .as_ref()
        .map(|it| it.val.as_str())
        .unwrap_or(if behavior == ResolveBehavior::V3 { "fallback" } else { "allow" });
    let rust_versions = match fallback {
        "allow" => None,
        "fallback" => {
            let mut versions = members
                .members
                .iter()
                .filter_map(|m| m.rust_version())
                .map(|v| v.to_partial())
                .collect::<Vec<_>>();
            if versions.is_empty() {
                versions.push(env!("CARGO_PKG_RUST_VERSION").parse()?);
            }
            Some(versions)
        }
        value => bail!("invalid resolver.incompatible-rust-versions value `{value}`"),
    };
    let resolve_version = ResolveVersion::with_rust_version(
        members.members.iter().filter_map(|m| m.rust_version()).min(),
    );
    Ok(blaze_resolve::workspace_resolve::Workspace {
        members,
        path_packages: by_path,
        patch,
        replace: &[],
        require_optional_deps: true,
        version_ordering: VersionOrdering::MaximumVersionsFirst,
        rust_versions,
        resolve_version,
    })
}

/// The packages of a build as the resolve and the graphs built on it read them: the members,
/// then the path packages that are not and the fetched ones.
struct World<'a> {
    /// Each package as `cargo metadata` prints it.
    serialized: Vec<SerializedPackage<'a>>,
    /// The manifest of each package, in the order of `serialized`, and where it is.
    manifests: Vec<&'a Manifest>,
    manifest_paths: Vec<&'a Path>,
    /// Where the lock file says each package is from, none for a path package.
    locked_sources: Vec<Option<&'a str>>,
    reads: FxHashMap<PackageId, Read>,
    /// The id of each package, in the order of `serialized`.
    ids: Vec<PackageId>,
    member_count: usize,
    /// The member whose manifest is in the current directory.
    current: Option<PackageId>,
    behavior: ResolveBehavior,
    /// `resolver = "2"` or later, or a virtual workspace (cargo's
    /// `allows_new_cli_feature_behavior`).
    new_cli_feature_behavior: bool,
    lock: &'a Locked,
}

impl<'a> World<'a> {
    fn new(ws: &'a Workspace<'_>, locked: &'a Locked) -> Result<World<'a>> {
        let mut serialized: Vec<SerializedPackage<'a>> =
            ws.members().map(|(path, m)| export_package(path, m)).collect();
        let mut manifests: Vec<&'a Manifest> = ws.members().map(|(_, m)| m).collect();
        let mut manifest_paths: Vec<&'a Path> = ws.members().map(|(path, _)| path).collect();
        let mut locked_sources: Vec<Option<&'a str>> = vec![None; serialized.len()];
        let member_count = serialized.len();
        for package in locked.path_packages.iter().chain(&locked.fetched) {
            let mut it = export_package(&package.manifest_path, &package.manifest);
            if let Some(source) = &package.locked_source {
                it.set_source(source.clone());
            }
            serialized.push(it);
            manifests.push(&package.manifest);
            manifest_paths.push(&package.manifest_path);
            locked_sources.push(package.locked_source.as_deref());
        }
        let mut reads = FxHashMap::default();
        let mut ids = Vec::with_capacity(serialized.len());
        for (at, manifest) in manifests.iter().enumerate() {
            let mut read = summary::native_manifest(manifest, locked_sources[at])?;
            let id = read.summary.package_id();
            if !id.source_id().is_path() {
                locked.facts.validate_manifest(&read.summary)?;
            }
            if let Some(Some(checksum)) = locked.lock.checksums().get(&id) {
                read.summary.set_checksum(checksum.clone());
            }
            ids.push(id);
            reads.insert(id, read);
        }
        let current =
            ws.members().position(|(path, _)| path == ws.current_manifest()).map(|at| ids[at]);
        let behavior = resolve_behavior(ws);
        let new_cli_feature_behavior = behavior != ResolveBehavior::V1 || ws.is_virtual();
        Ok(World {
            serialized,
            manifests,
            manifest_paths,
            locked_sources,
            reads,
            ids,
            member_count,
            current,
            behavior,
            new_cli_feature_behavior,
            lock: locked,
        })
    }

    fn member_ids(&self) -> &[PackageId] {
        &self.ids[..self.member_count]
    }

    fn member_summaries(&self) -> Vec<&Summary> {
        self.member_ids().iter().map(|id| &self.reads[id].summary).collect()
    }

    fn members<'m>(&self, summaries: &'m [&'m Summary]) -> Members<'m> {
        Members {
            members: summaries,
            current: self.current,
            new_cli_feature_behavior: self.new_cli_feature_behavior,
        }
    }

    fn resolve(
        &self,
        _ws: &Workspace<'_>,
        _ctx: &Context,
        specs: &[PackageIdSpec],
        cli_features: &CliFeatures,
        has_dev_units: HasDevUnits,
    ) -> Result<Resolved> {
        ensure!(
            self.lock.selection == ResolutionSelection::new(specs, cli_features, has_dev_units),
            "locked source selection differs from the requested unit selection"
        );
        Ok(Resolved { whole: self.lock.lock.clone(), targeted: self.lock.targeted.clone() })
    }
}

/// The two resolves of cargo's `resolve_ws_with_opts`.
struct Resolved {
    /// Of the whole workspace, as the lock file has it: what package overrides in profiles are
    /// checked against.
    whole: blaze_resolve::Resolve,
    /// Of what the build selects: the graph of its units.
    targeted: blaze_resolve::Resolve,
}

/// What the resolve graph and the feature resolver read of the packages.
struct Packages<'a> {
    reads: &'a FxHashMap<PackageId, Read>,
    platform: Option<&'a TargetPlatform<'a>>,
    host: Option<&'a TargetPlatform<'a>>,
}

impl Packages<'_> {
    fn activates(&self, platform: &Platform) -> bool {
        self.platform.is_none_or(|it| it.activates(platform))
    }

    fn activates_for(&self, platform: &Platform, kind: CompileKind) -> bool {
        match (kind, self.host) {
            (CompileKind::Host, Some(host)) => host.activates(platform),
            _ => self.activates(platform),
        }
    }
}

impl MetadataPackages for Packages<'_> {
    fn package_id(&self, id: PackageId) -> PackageId {
        id
    }

    fn lib_crate_name(&self, id: PackageId) -> Option<InternedString> {
        self.reads.get(&id).and_then(|it| it.lib)
    }

    fn platform_activated(&self, platform: &Platform, kind: CompileKind) -> bool {
        self.activates_for(platform, kind)
    }
}

impl PackageTargets for Packages<'_> {
    fn has_any_proc_macro(&self, package_id: PackageId) -> bool {
        self.reads.get(&package_id).is_some_and(|it| it.any_proc_macro)
    }

    fn has_proc_macro_lib(&self, package_id: PackageId) -> bool {
        self.reads.get(&package_id).is_some_and(|it| it.proc_macro_lib)
    }

    fn platform_activated(&self, platform: &Platform, kind: CompileKind) -> bool {
        self.activates_for(platform, kind)
    }
}

/// The members `build` selects, as cargo's `Packages::from_flags` and `to_package_id_specs`
/// select them.
fn selected(
    ws: &Workspace<'_>,
    member_ids: &[PackageId],
    build: &Build<'_>,
) -> Result<Vec<PackageIdSpec>> {
    let named = |name: &str| member_ids.iter().find(|id| id.name().as_str() == name).copied();
    let ids: Vec<PackageId> = if build.workspace {
        let excluded = |id: &PackageId| build.excludes.iter().any(|it| it == id.name().as_str());
        member_ids.iter().filter(|id| !excluded(id)).copied().collect()
    } else if !build.packages.is_empty() {
        let found = build.packages.iter().map(|name| {
            named(name).with_context(|| format!("package `{name}` is not a member blaze knows"))
        });
        found.collect::<Result<_>>()?
    } else {
        let defaults = ws.default_members().map(|(_, m)| named(&m.package_id.name));
        defaults.collect::<Option<_>>().context("a default member that is not a member")?
    };
    Ok(ids.iter().map(|id| id.to_spec()).collect())
}

/// The path packages that are not members: those the members and the root's `[patch]` name
/// by path, and theirs in turn.
fn path_packages(ws: &Workspace<'_>, root: &Path, ctx: &Context) -> Result<Vec<Package>> {
    let members: Vec<PathBuf> =
        ws.members().filter_map(|(path, _)| path.parent().map(Path::to_owned)).collect();
    let mut queue = summary::patch_paths(root, ctx)?;
    for (_, m) in ws.members() {
        queue.extend(native_path_dependencies(m));
    }
    let mut seen: Vec<PathBuf> = members.clone();
    let mut read = Vec::new();
    while let Some(dir) = queue.pop() {
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir.clone());
        let manifest_path = dir.join("Cargo.toml");
        let source = blaze_manifest::source::SourceId::for_path(&dir)?;
        let EitherManifest::Real(manifest) = read_manifest(&manifest_path, source, ctx)? else {
            bail!("{} is a path dependency with no package", manifest_path.display());
        };
        queue.extend(native_path_dependencies(&manifest));
        read.push(Package { manifest_path, manifest, locked_source: None });
    }
    Ok(read)
}

fn native_path_dependencies(manifest: &blaze_manifest::manifest::Manifest) -> Vec<PathBuf> {
    manifest
        .dependencies
        .iter()
        .filter(|dep| dep.source_id.is_path())
        .filter_map(|dep| dep.source_id.url.to_file_path().ok())
        .collect()
}

/// Every package of `lock` that is not a path package, fetched where cargo would have it and
/// read from its manifest.
enum FetchFrom<'a> {
    Owned(&'a blaze_registry::Sources),
    Captured(&'a [CapturedSource]),
}

fn fetch(
    lock: &blaze_resolve::Resolve,
    ctx: &Context,
    from: FetchFrom<'_>,
) -> Result<(Vec<Package>, Vec<CapturedSource>)> {
    let ids: Vec<_> = lock
        .sort()
        .into_iter()
        .filter(|id| !id.source_id().is_path())
        .map(|id| (id, id.source_id().as_url().to_string()))
        .collect();
    if let FetchFrom::Captured(captured) = from {
        let mut roots = BTreeMap::new();
        for source in captured {
            ensure!(
                source.root.is_absolute(),
                "captured source root is not absolute: {}",
                source.root.display()
            );
            let key = (source.source.clone(), source.name.clone(), source.version.clone());
            ensure!(
                roots.insert(key, source.root.clone()).is_none(),
                "duplicate captured source for {} {} from {}",
                source.name,
                source.version,
                source.source
            );
        }
        let mut selected = Vec::with_capacity(ids.len());
        for (id, source) in &ids {
            let key = (source.clone(), id.name().to_string(), id.version().to_string());
            selected.push(roots.remove(&key).with_context(|| {
                format!("missing captured source for {} {} from {source}", id.name(), id.version())
            })?);
        }
        ensure!(
            roots.is_empty(),
            "captured source list contains packages not selected by the build"
        );
        // No registry, Git database, transport, archive cache or source-replacement
        // lookup is consulted when replay supplies authenticated package roots.
        return read_fetched(ids, selected, ctx);
    }
    let FetchFrom::Owned(sources) = from else { unreachable!("captured sources returned above") };
    let mut locked = Vec::new();
    for (id, source_text) in &ids {
        let (source, precise) = blaze_registry::SourceId::from_url(source_text)?;
        let from = sources.map().replacement(&source, precise.is_some())?;
        if from.is_remote_registry() && !from.is_sparse() {
            bail!("{source_text} is read from a git index, which blaze does not read yet");
        }
        let mut checksum = lock.checksums().get(id).cloned().flatten().unwrap_or_default();
        // A lock file of cargo before 1.53 may name no checksum: the index's.
        if checksum.is_empty() && from.is_remote_registry() {
            checksum = sources.registry(&from)?.checksum(id.name().as_str(), id.version())?;
        }
        let package = blaze_registry::Package {
            name: id.name().to_string(),
            version: id.version().clone(),
            checksum,
        };
        locked.push(blaze_registry::Locked { source, precise, package });
    }
    let roots = sources.fetch(&locked)?;
    read_fetched(ids, roots.into_iter().map(|(root, _)| root).collect(), ctx)
}

fn read_fetched(
    ids: Vec<(PackageId, String)>,
    roots: Vec<PathBuf>,
    ctx: &Context,
) -> Result<(Vec<Package>, Vec<CapturedSource>)> {
    ensure!(ids.len() == roots.len(), "fetched source count differs from the locked packages");
    let mut read = Vec::with_capacity(ids.len());
    let mut captured = Vec::with_capacity(ids.len());
    for ((id, source_text), dir) in ids.into_iter().zip(roots) {
        let manifest_path = dir.join("Cargo.toml");
        let source = manifest_source(&source_text, &ctx.config)?;
        let EitherManifest::Real(manifest) = read_manifest(&manifest_path, source, ctx)? else {
            bail!("{} has no package", manifest_path.display());
        };
        ensure!(
            manifest.package_id.name == id.name().as_str()
                && manifest.package_id.version == *id.version(),
            "{} contains {} {}, expected locked package {} {} from {}",
            manifest_path.display(),
            manifest.package_id.name,
            manifest.package_id.version,
            id.name(),
            id.version(),
            source_text
        );
        captured.push(CapturedSource {
            source: source_text.clone(),
            name: id.name().to_string(),
            version: id.version().to_string(),
            root: dir,
        });
        read.push(Package { manifest_path, manifest, locked_source: Some(source_text) });
    }
    Ok((read, captured))
}

/// The source of a package the lock file pins to `source`, as blaze-manifest names it.
fn manifest_source(source: &str, config: &Config) -> Result<blaze_manifest::source::SourceId> {
    use blaze_manifest::source::{CRATES_IO_INDEX, SourceId};
    use cargo_util_schemas::core::GitReference;
    let (kind, rest) = source.split_once('+').context("a source with no kind")?;
    match kind {
        "registry" if rest == CRATES_IO_INDEX => SourceId::crates_io(config),
        "registry" => Ok(SourceId::for_registry(&rest.parse()?)),
        "sparse" => Ok(SourceId::for_registry(&source.parse()?)),
        "git" => {
            let mut url: url::Url = rest.parse()?;
            let query: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
            let reference = match (query.get("branch"), query.get("tag"), query.get("rev")) {
                (Some(branch), _, _) => GitReference::Branch(branch.clone()),
                (_, Some(tag), _) => GitReference::Tag(tag.clone()),
                (_, _, Some(rev)) => GitReference::Rev(rev.clone()),
                _ => GitReference::DefaultBranch,
            };
            url.set_fragment(None);
            url.set_query(None);
            Ok(SourceId::for_git(&url, reference))
        }
        _ => bail!("the source {source} is not one blaze reads yet"),
    }
}

/// `[source]` and `[registries]` as blaze-registry takes them.
fn source_config(config: &Config) -> Result<blaze_registry::SourceConfig> {
    use blaze_manifest::source::SourceId;
    use cargo_util_schemas::core::{GitReference, SourceKind};
    let mut out = blaze_registry::SourceConfig::default();
    for (name, def) in config.source_configs()? {
        let (replace_with, definition) = def.replace_with.clone().unzip();
        let mut it = blaze_registry::SourceDef {
            replace_with,
            definition: definition.unwrap_or_default(),
            ..Default::default()
        };
        if name == blaze_registry::CRATES_IO_REGISTRY {
            if it.replace_with.is_some() {
                out.sources.push((name, it));
            }
            continue;
        }
        let SourceId { kind, url, .. } = &def.id;
        match kind {
            SourceKind::Registry | SourceKind::SparseRegistry => {
                it.registry = Some(url.to_string());
            }
            SourceKind::LocalRegistry => it.local_registry = url.to_file_path().ok(),
            SourceKind::Directory => it.directory = url.to_file_path().ok(),
            SourceKind::Git(reference) => {
                it.git = Some(url.to_string());
                match reference {
                    GitReference::Branch(b) => it.branch = Some(b.clone()),
                    GitReference::Tag(t) => it.tag = Some(t.clone()),
                    GitReference::Rev(r) => it.rev = Some(r.clone()),
                    GitReference::DefaultBranch => {}
                }
            }
            _ => bail!("`source.{name}` is not a source blaze reads yet"),
        }
        out.sources.push((name, it));
    }
    let registries: Option<BTreeMap<String, RegistryConfig>> = config.get("registries")?;
    for name in registries.into_iter().flatten().map(|(name, _)| name) {
        if name == blaze_registry::CRATES_IO_REGISTRY {
            continue;
        }
        let index = config.registry_index(&name)?;
        out.registries.insert(name, index.to_string());
    }
    out.crates_io_git =
        config.get_string("registries.crates-io.protocol")?.is_some_and(|it| it.val == "git");
    Ok(out)
}

fn write_lockfile(path: &Path, text: &str) -> Result<()> {
    blaze_registry::write_whole(path, text.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod lock_publication_tests {
    use super::write_lockfile;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "blaze-lock-publication-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("{error}"),
                }
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn concurrent_readers_see_only_complete_lockfiles() {
        let fixture = Fixture::new();
        let path = fixture.0.join("Cargo.lock");
        let old = format!("version = 4\n# old {}\n", "x".repeat(64 * 1024));
        let new = format!("version = 4\n# new {}\n", "y".repeat(64 * 1024));
        write_lockfile(&path, &old).unwrap();
        let done = AtomicBool::new(false);
        let started = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                started.wait();
                for n in 0..128 {
                    write_lockfile(&path, if n % 2 == 0 { &new } else { &old }).unwrap();
                }
                done.store(true, Ordering::Release);
            });
            started.wait();
            let mut reads = 0;
            while !done.load(Ordering::Acquire) || reads == 0 {
                let read = std::fs::read_to_string(&path).unwrap();
                assert!(read == old || read == new, "partial lockfile: {} bytes", read.len());
                reads += 1;
            }
        });
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn publication_preserves_private_permissions_and_symlink_identity() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new();
        let target = fixture.0.join("shared.lock");
        let path = fixture.0.join("Cargo.lock");
        write_lockfile(&target, "version = 4\n# old\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink("shared.lock", &path).unwrap();
        write_lockfile(&path, "version = 4\n# new\n").unwrap();
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "version = 4\n# new\n");
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn publication_refuses_a_read_only_lockfile() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let path = fixture.0.join("Cargo.lock");
        write_lockfile(&path, "version = 4\n# authoritative\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
            return;
        }
        assert!(write_lockfile(&path, "version = 4\n# replacement\n").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "version = 4\n# authoritative\n");
    }

    #[test]
    #[cfg(unix)]
    fn failed_publication_preserves_the_authoritative_lock() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let path = fixture.0.join("Cargo.lock");
        write_lockfile(&path, "version = 4\n# authoritative\n").unwrap();
        std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o500)).unwrap();
        let probe = fixture.0.join("permission-probe");
        if std::fs::write(&probe, "").is_ok() {
            std::fs::remove_file(probe).unwrap();
            return;
        }
        assert!(write_lockfile(&path, "version = 4\n# replacement\n").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "version = 4\n# authoritative\n");
    }
}
