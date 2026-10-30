//! Portable replay inputs. Original invocation recipes are outside build identity;
//! the immutable source snapshot authenticates the locked package-to-source mapping.
//!
//! Reproduction never guesses flags from the resulting units. It replays the original
//! request against the captured configuration and manifests, resolves offline, and
//! compares the entire BuildSpec. Only then is the execution platform probed.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail, ensure};
use blaze_manifest::config::ConfigImage;
use blaze_wire::{
    BuildSpec, ConfigEnvValue, Digest, ExecPlatform, RootRef, SourcePath, SourceRef, StoreIdentity,
    Wire,
};
use sha2::{Digest as _, Sha256};

use crate::{
    assemble::{self, Context, Host, Resolved, Spec},
    environment,
    flags::{FLAGS, Request, Subcommand, Takes},
    platform::Probe,
    recipe::{ConfigFile, ReplayRecipe},
    snapshot::Snapshot,
};

const SOURCES: &str = ".blaze-sources.cbor";
const FACTS: &str = ".blaze-resolver-facts.cbor";
const CARGO_HOME: &str = ".blaze-cargo-home";

/// The inputs that have passed source, request, graph, toolchain and platform validation.
pub struct Prepared {
    /// The reproduced spec and native graph, from one resolution.
    pub resolved: Resolved,
    /// The materialized snapshot root. Every source graph path is below this root.
    pub root: PathBuf,
    /// The original working directory, relocated within the snapshot.
    pub cwd: PathBuf,
    /// The original parsed request, with manifest/configuration paths relocated.
    pub request: Request,
    /// Only recorded compiler/configuration/native-tool values; no replay caller values.
    pub environment: BTreeMap<String, OsString>,
    /// Toolchain-derived target cfg facts, verified against the saved platform.
    pub host: Host,
    /// The configuration files the capture read, at their places in the snapshot: what every
    /// later reading of the configuration (the executor's too) is made from.
    pub config_image: ConfigImage,
    /// The names of the variables the requester's process had, as digests: the record never
    /// holds a name or a value. See [`Prepared::requester_had`].
    requester_names: BTreeSet<Digest>,
}

/// What a variable name is recorded as.
fn name_digest(name: &str) -> Digest {
    Sha256::digest(name.as_bytes()).into()
}

impl Prepared {
    /// Keep a live request's resolution, configuration layers and effective environment
    /// together for the owned executor.
    pub fn from_live(
        resolved: Resolved,
        root: PathBuf,
        cwd: PathBuf,
        request: Request,
        environment: BTreeMap<String, OsString>,
        host: Host,
    ) -> anyhow::Result<Self> {
        let mut files = BTreeMap::new();
        for path in &resolved.config.files {
            let bytes = std::fs::read(path)
                .with_context(|| format!("configuration {} is unreadable", path.display()))?;
            files.insert(path.clone(), bytes);
        }
        let config_image = ConfigImage { layers: resolved.config.layers.clone(), files };
        let requester_names = environment.keys().map(|name| name_digest(name)).collect();
        Ok(Self { resolved, root, cwd, request, environment, host, config_image, requester_names })
    }

    /// Manifest/configuration context scoped to the authenticated materialized replay.
    pub fn manifests(&self) -> anyhow::Result<blaze_manifest::Context> {
        let env = environment::text_view(&self.environment)?;
        let home = environment::cargo_home(&self.environment, &self.cwd)
            .context("replay has no Cargo home")?;
        let mut config = blaze_manifest::Config::from_image(
            &self.cwd,
            home,
            env,
            &[],
            self.config_image.clone(),
        )?
        .with_discovery_root(&self.root)?;
        config.start(
            self.request.command.unwrap_or(Subcommand::Build).name(),
            &self.request.config,
        )?;
        Ok(blaze_manifest::Context::new(config))
    }

    /// Complete native trees probed with the same effective environment and machine
    /// facts that authenticated this replay's execution platform.
    pub fn native_closure(&self, probe: &dyn Probe) -> anyhow::Result<crate::native::Closure> {
        let mut env = self.environment.clone();
        env.extend(self.execution_environment()?);
        let machine = crate::platform::Machine {
            cwd: &self.cwd,
            triple: &self.host.triple,
            cpu: &self.host.cpu,
            env: &env,
            linker: None,
        };
        let described = crate::platform::describe_of(&machine, probe, Some(&self.root))?;
        ensure!(
            described.platform == self.resolved.spec.platform,
            "native execution platform changed after replay validation"
        );
        Ok(described.native)
    }

    /// Revalidate all materialized source bytes, kinds, native names and membership.
    pub fn validate_sources(&self, store: &Path) -> anyhow::Result<()> {
        Snapshot::verify_materialized(source_root(&self.resolved.spec.build)?, store, &self.root)
    }

    /// Whether the requester's process had a variable called `name`, whether or not the spec
    /// carries it. A program that reads one it did not have (`option_env!`) is given the same
    /// answer by a replay, which never passes it the worker's value; one it did have, and
    /// the spec does not carry, cannot be answered.
    pub fn requester_had(&self, name: &str) -> bool {
        self.requester_names.contains(&name_digest(name))
    }

    /// Authenticated requester variable-name digests for the private compiler context.
    pub fn requester_names(&self) -> &BTreeSet<Digest> {
        &self.requester_names
    }

    /// The effective compiler/run environment, after applying the spec's configuration
    /// values. Configuration-derived relative paths must not be ambient overrides while
    /// reproducing the configuration itself.
    pub fn execution_environment(&self) -> anyhow::Result<BTreeMap<String, OsString>> {
        let manifest = self.root.join(self.resolved.spec.build.workspace_manifest.to_native()?);
        let workspace = manifest.parent().context("workspace manifest has no parent")?;
        let mut env = BTreeMap::new();
        if let Some(path) = self.environment.get("PATH") {
            env.insert("PATH".to_owned(), path.clone());
        }
        for (name, value) in self.resolved.spec.build.script_env.iter() {
            env.insert(name.clone(), value.materialize(workspace)?);
        }
        for (name, value) in self.resolved.spec.build.config_env.iter() {
            env.insert(name.clone(), value.materialize(workspace)?);
        }
        Ok(env)
    }
}

/// The version of the replay record's encoding.
const RECORD_VERSION: u32 = 4;

/// The configuration files resolution read, placed in the snapshot. Replay reads these and no
/// others, in this order: discovery from the worker's tree could fold a file in at another
/// precedence (a Cargo home that lay above the workspace was an ancestor's `.cargo`, and is a
/// mount in the snapshot) or find one the capture never saw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ConfigLayers {
    /// The top-level files, highest precedence first.
    layers: Vec<SourcePath>,
    /// Every file read: the layers, what they include, and the `--config` files.
    files: Vec<SourcePath>,
}

impl ConfigLayers {
    fn image(&self, root: &Path) -> anyhow::Result<ConfigImage> {
        let native =
            |path: &SourcePath| -> anyhow::Result<PathBuf> { Ok(root.join(path.to_native()?)) };
        let mut files = BTreeMap::new();
        for file in &self.files {
            let path = native(file)?;
            let bytes = std::fs::read(&path).with_context(|| {
                format!("captured configuration {} is unreadable", path.display())
            })?;
            files.insert(path, bytes);
        }
        let layers = self.layers.iter().map(native).collect::<anyhow::Result<_>>()?;
        Ok(ConfigImage { layers, files })
    }
}

impl Wire for ConfigLayers {
    fn encode(&self, e: &mut blaze_wire::Encoder) {
        e.array(2);
        self.layers.encode(e);
        self.files.encode(e);
    }

    fn decode(r: &mut blaze_wire::Reader<'_>) -> Result<Self, blaze_wire::Error> {
        r.array_start(2)?;
        let layers = Vec::decode(r)?;
        let files = Vec::decode(r)?;
        r.array_end()?;
        Ok(Self { layers, files })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    recipe: ReplayRecipe,
    manifest: SourcePath,
    cargo_home: Option<SourcePath>,
    platform: ExecPlatform,
    sources: SourcePath,
    facts: SourcePath,
    config: ConfigLayers,
    /// The digests of the names of the requester's variables, in order.
    requester: Vec<Digest>,
}

impl Wire for Record {
    fn encode(&self, e: &mut blaze_wire::Encoder) {
        e.array(9);
        RECORD_VERSION.encode(e);
        self.recipe.encode(e);
        self.manifest.encode(e);
        self.cargo_home.encode(e);
        self.platform.encode(e);
        self.sources.encode(e);
        self.facts.encode(e);
        self.config.encode(e);
        self.requester.encode(e);
    }

    fn decode(r: &mut blaze_wire::Reader<'_>) -> Result<Self, blaze_wire::Error> {
        r.array_start(9)?;
        let at = r.position();
        if u32::decode(r)? != RECORD_VERSION {
            return Err(blaze_wire::Error {
                kind: blaze_wire::ErrorKind::UnexpectedType { expected: "replay record version 4" },
                at,
            });
        }
        let record = Self {
            recipe: <ReplayRecipe as Wire>::decode(r)?,
            manifest: SourcePath::decode(r)?,
            cargo_home: Option::decode(r)?,
            platform: ExecPlatform::decode(r)?,
            sources: SourcePath::decode(r)?,
            facts: SourcePath::decode(r)?,
            config: ConfigLayers::decode(r)?,
            requester: Vec::decode(r)?,
        };
        r.array_end()?;
        Ok(record)
    }
}

fn anchor(root: &Path, path: &Path) -> PathBuf {
    let path = blaze_manifest::paths::normalize_path(path);
    if path.starts_with(root) {
        return path;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = parent.canonicalize()
        && parent.join(name).starts_with(root)
    {
        return parent.join(name);
    }
    path
}

fn relative(root: &Path, path: &Path) -> anyhow::Result<SourcePath> {
    let path = anchor(root, path);
    let path = path.strip_prefix(root).with_context(|| {
        format!("{} is outside the captured source layout {}", path.display(), root.display())
    })?;
    Ok(SourcePath::from_native(path)?)
}

fn common_root(paths: impl IntoIterator<Item = PathBuf>) -> anyhow::Result<PathBuf> {
    let mut paths = paths.into_iter();
    let mut root = paths.next().context("no source directory to capture")?;
    for path in paths {
        while !path.starts_with(&root) {
            ensure!(root.pop(), "source inputs have no common filesystem root");
        }
    }
    Ok(root)
}

fn native(ctx: &Context<'_>) -> BTreeMap<String, OsString> {
    ctx.native_env.cloned().unwrap_or_else(|| {
        ctx.env.iter().map(|(name, value)| (name.clone(), value.into())).collect()
    })
}

fn record_path(spec: &BuildSpec, store: &Path) -> PathBuf {
    let digest: String =
        blaze_wire::digest_of(spec).iter().map(|byte| format!("{byte:02x}")).collect();
    store.join(format!("replay-{digest}.cbor"))
}

fn config_files(
    words: &[String],
    cwd: &Path,
    place: &impl Fn(&Path) -> anyhow::Result<SourcePath>,
    inputs: &[(PathBuf, Vec<u8>)],
) -> anyhow::Result<Vec<ConfigFile>> {
    let mut files = Vec::new();
    let mut at = 0;
    while at < words.len() && words[at] != "--" {
        let word = &words[at];
        let mut end = at + 1;
        if let Some(long) = word.strip_prefix("--") {
            let (name, attached) =
                long.split_once('=').map_or((long, None), |(name, value)| (name, Some(value)));
            if let Some(flag) = FLAGS.iter().find(|flag| flag.long == name) {
                if flag.takes == Takes::Value && attached.is_none() {
                    end += 1;
                }
                if name == "config" {
                    let value = attached.unwrap_or_else(|| words[at + 1].as_str());
                    let path = blaze_manifest::paths::normalize_path(&cwd.join(value));
                    // Cargo chooses an existing file before interpreting an assignment;
                    // `settings=dev.toml` is therefore a file when resolution read it.
                    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                    if inputs.iter().any(|(input, _)| {
                        input.canonicalize().unwrap_or_else(|_| input.clone()) == canonical
                    }) {
                        files.push(ConfigFile { argument: at as u32, path: place(&path)? });
                    }
                }
            }
        } else if word.starts_with('-') {
            for (offset, short) in word[1..].char_indices() {
                if let Some(flag) = FLAGS.iter().find(|flag| flag.short == Some(short))
                    && flag.takes != Takes::Nothing
                {
                    if flag.takes == Takes::Value && 1 + offset + short.len_utf8() == word.len() {
                        end += 1;
                    }
                    break;
                }
            }
        }
        at = end;
    }
    Ok(files)
}

/// Capture a resolved request and its complete selected package trees. `words` are the
/// exact words that produced `request`, excluding the command. Output paths include the
/// bundle currently being written; they must never become inputs of their own snapshot.
/// A relocated offline reproduction is checked before the returned identity is published.
pub fn capture(
    request: &Request,
    words: &[String],
    ctx: &Context<'_>,
    store: &Path,
    outputs: &[PathBuf],
) -> anyhow::Result<Spec> {
    let subcommand = request.command.unwrap_or(Subcommand::Build);
    ensure!(Request::parse(subcommand, words)? == *request, "request does not match its words");
    let mut first = assemble::resolve(request, ctx)?;
    drop(first.observation.take());
    let native = native(ctx);
    let home = environment::cargo_home(&native, ctx.cwd)
        .unwrap_or_else(|| first.graph.root.join(".blaze-cargo-home"));
    let home = blaze_manifest::paths::normalize_path(&home);
    let mut trees = vec![first.graph.root.clone()];
    trees.extend(first.graph.units.iter().map(|unit| unit.package.manifest_dir.clone()));
    // Path-package manifests read during resolution remain source inputs. Inactive
    // registry/Git packages contribute authenticated summaries rather than archives.
    trees.extend(first.inputs.iter().filter_map(|(path, _)| {
        (path.file_name().is_some_and(|name| name == "Cargo.toml"))
            .then(|| path.parent().map(Path::to_owned))
            .flatten()
    }));
    trees.sort();
    trees.dedup();
    let mut layout = trees.clone();
    layout.extend(first.inputs.iter().filter_map(|(path, _)| path.parent().map(Path::to_owned)));
    let canonical = |path: &Path| {
        let path = blaze_manifest::paths::normalize_path(path);
        path.canonicalize().unwrap_or(path)
    };
    let home_dir = canonical(&home);
    let workspace_dir = canonical(&first.graph.root);
    let mounted = !workspace_dir.starts_with(&home_dir) && !home_dir.starts_with(&workspace_dir);
    let in_home = |path: &Path| mounted && anchor(&home_dir, path).starts_with(&home_dir);
    let root = common_root(
        layout
            .into_iter()
            .map(|path| canonical(&path))
            .filter(|path| !(mounted && path.starts_with(&home_dir))),
    )?;
    let captured_ctx = Context {
        source_root: Some(&root),
        config_root: ctx.config_root,
        config_image: ctx.config_image,
        source: SourceRef::Root(RootRef { root: [0; 32] }),
        cwd: ctx.cwd,
        env: ctx.env,
        native_env: ctx.native_env,
        host: ctx.host,
        toolchain: ctx.toolchain,
        offline: ctx.offline,
        store: ctx.store.clone(),
        probe: ctx.probe,
        rust_release: ctx.rust_release,
    };
    // This pass also resolves workspace-relative configuration paths against the
    // final layout, rather than the provisional workspace root.
    // Probe with the final requester root so tools within it are named by their bytes.
    let mut resolved =
        assemble::resolve_captured(request, &captured_ctx, None, &first.sources, &first.facts)?;
    resolved.inputs.extend(first.inputs);
    let mut excluded = resolved.outputs;
    excluded.extend_from_slice(outputs);
    excluded.push(store.to_owned());
    let (home_trees, trees): (Vec<_>, Vec<_>) = trees.iter().partition(|path| in_home(path));
    let (home_inputs, inputs): (Vec<_>, Vec<_>) =
        resolved.inputs.iter().partition(|(path, _)| in_home(path));
    let trees: Vec<_> = trees.iter().map(|path| anchor(&root, path)).collect();
    let inputs: Vec<_> =
        inputs.iter().map(|(path, bytes)| (anchor(&root, path), bytes.clone())).collect();
    let mut snapshot = Snapshot::capture_selected(&root, &trees, &excluded, &inputs, store)?;
    let home_mount = if !home_trees.is_empty() || !home_inputs.is_empty() {
        let trees: Vec<_> = home_trees.iter().map(|path| anchor(&home_dir, path)).collect();
        let inputs: Vec<_> = home_inputs
            .iter()
            .map(|(path, bytes)| (anchor(&home_dir, path), bytes.clone()))
            .collect();
        let held = Snapshot::capture_selected(&home_dir, &trees, &excluded, &inputs, store)?;
        let (mounted, at) = snapshot.with_unique_mount(CARGO_HOME, held)?;
        snapshot = mounted;
        Some(at)
    } else {
        None
    };
    let place = |path: &Path| -> anyhow::Result<SourcePath> {
        if in_home(path) {
            let inside = anchor(&home_dir, path);
            let inside = inside.strip_prefix(&home_dir)?;
            let mount = home_mount.as_ref().context("captured Cargo home has no mount")?;
            Ok(SourcePath::from_native(&mount.to_native()?.join(inside))?)
        } else {
            relative(&root, path)
        }
    };
    let config_files = config_files(words, ctx.cwd, &place, &resolved.inputs)?;
    let mut sources: Vec<_> = resolved
        .sources
        .iter()
        .map(|source| {
            Ok((
                (source.source.clone(), source.name.clone(), source.version.clone()),
                place(&source.root)?,
            ))
        })
        .collect::<anyhow::Result<_>>()?;
    sources.sort();
    let (snapshot, sources_path) =
        snapshot.with_unique_file(SOURCES, &blaze_wire::encode(&(1_u32, sources)), store)?;
    let (snapshot, facts_path) = snapshot.with_unique_file(
        FACTS,
        &crate::resolver_facts::encode(&resolved.facts)?,
        store,
    )?;
    let mut made = resolved.spec;
    made.build.source = SourceRef::Root(RootRef { root: snapshot.digest() });
    // The warm front-end and configuration keys intentionally do not contain source.
    made.digest = blaze_wire::digest_of(&made.build);
    let recipe_env = native
        .iter()
        .filter(|(name, value)| {
            !(value.is_empty()
                && matches!(name.as_str(), "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER"))
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let configuration = |paths: &[PathBuf]| -> anyhow::Result<Vec<SourcePath>> {
        paths.iter().map(|path| place(path)).collect()
    };
    let record = Record {
        recipe: ReplayRecipe::capture(&made.build, subcommand, words, &recipe_env, &config_files)?,
        requester: native
            .keys()
            .map(|name| name_digest(name))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        config: ConfigLayers {
            layers: configuration(&resolved.config.layers)?,
            files: configuration(&resolved.config.files)?,
        },
        manifest: place(&resolved.manifest)?,
        // An empty external Cargo home is not a source input and must not make the
        // source root depend on the absolute spelling of a checkout's location.
        cargo_home: if home_mount.is_some() {
            home_mount.clone()
        } else {
            resolved
                .inputs
                .iter()
                .any(|(path, _)| {
                    let path = anchor(&root, path);
                    path == anchor(&root, &home).join("config")
                        || path == anchor(&root, &home).join("config.toml")
                })
                .then(|| relative(&root, &home))
                .transpose()?
        },
        platform: made.platform.clone(),
        sources: sources_path,
        facts: facts_path,
    };
    let relocated = tempfile::tempdir()?;
    let baseline = resolved.observation.take();
    prepare_record(
        &made.build,
        store,
        &relocated.path().join("source"),
        ctx.host,
        ctx.toolchain,
        ctx.probe,
        ctx.rust_release,
        &native,
        &record,
        baseline.as_ref(),
    )
    .context("captured inputs cannot reproduce the request offline")?;
    drop(baseline);
    let mut writing = tempfile::NamedTempFile::new_in(store)?;
    writing.write_all(&blaze_wire::encode(&record))?;
    writing.as_file().sync_all()?;
    writing.persist(record_path(&made.build, store)).map_err(|error| error.error)?;
    Ok(made)
}

/// Authenticate and materialize a replay, reproduce its exact graph and request, and
/// only then verify the native execution platform. Nothing from the caller's cwd or
/// compiler environment is used. The destination must not already exist.
#[expect(
    clippy::too_many_arguments,
    reason = "the authenticated identity and executor context are explicit"
)]
pub fn prepare(
    given: &BuildSpec,
    store: &Path,
    destination: &Path,
    host: &Host,
    identity: StoreIdentity,
    probe: &dyn Probe,
    rust_release: Option<&blaze_metadata::RustRelease<'_>>,
    executor: &BTreeMap<String, OsString>,
) -> anyhow::Result<Prepared> {
    let bytes = std::fs::read(record_path(given, store))
        .context("the saved replay recipe is missing or does not match this BuildSpec")?;
    let record: Record = blaze_wire::decode(&bytes, &blaze_wire::Limits::DOCUMENTED)?;
    prepare_record(
        given,
        store,
        destination,
        host,
        identity,
        probe,
        rust_release,
        executor,
        &record,
        None,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the authenticated identity and executor context are explicit"
)]
fn prepare_record(
    given: &BuildSpec,
    store: &Path,
    destination: &Path,
    host: &Host,
    identity: StoreIdentity,
    probe: &dyn Probe,
    rust_release: Option<&blaze_metadata::RustRelease<'_>>,
    executor: &BTreeMap<String, OsString>,
    record: &Record,
    baseline: Option<&crate::native::Observation>,
) -> anyhow::Result<Prepared> {
    let SourceRef::Root(source) = &given.source else {
        bail!("this spec has no immutable replay inputs; save it again with --out");
    };
    ensure!(given.toolchain == identity, "the replay toolchain differs from the BuildSpec");
    ensure!(
        record.recipe.spec_digest() == blaze_wire::digest_of(given),
        "recipe does not match the BuildSpec"
    );
    let bytes = Snapshot::read_file(source.root, store, &record.sources)?;
    let (version, captured): (u32, Vec<((String, String, String), SourcePath)>) =
        blaze_wire::decode(&bytes, &blaze_wire::Limits::DOCUMENTED)?;
    ensure!(version == 1, "unknown captured source mapping version");
    ensure!(
        blaze_wire::digest_of(&record.platform) == given.platform,
        "the recorded platform differs from the BuildSpec"
    );
    ensure!(
        record.platform.triple == host.triple && record.platform.cpu == host.cpu,
        "the replay host differs from the BuildSpec"
    );
    Snapshot::materialize(source.root, store, destination)?;
    let root = destination.canonicalize()?;
    let service: BTreeMap<_, _> = executor
        .iter()
        .filter(|(name, _)| name.as_str() == "PATH")
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let invocation = record.recipe.materialize(given, &root, &service)?;
    let args: Vec<_> = invocation
        .args
        .into_iter()
        .map(|word| {
            word.into_string().map_err(|_| {
                anyhow::anyhow!("the owned request parser requires UTF-8 argument paths")
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let mut request = Request::parse(invocation.command, &args)?;
    let cwd = blaze_manifest::paths::normalize_path(&invocation.cwd);
    request.manifest_path = Some(root.join(record.manifest.to_native()?));
    let workspace = root.join(given.workspace_manifest.to_native()?);
    let workspace = workspace.parent().context("workspace manifest has no directory")?;
    let mut native = invocation.env;
    // The executor's PATH is the worker's: a relative or empty entry in it is relative to the
    // worker's process, never to the replayed request's directory, where the requester's tree
    // would answer for it. The requester's own PATH, below, is relative to its directory.
    if let Some(path) = native.get("PATH") {
        let process = std::env::current_dir().context("the worker's directory is unknown")?;
        let entries = std::env::split_paths(path).map(|entry| process.join(entry));
        let path = std::env::join_paths(entries).context("the executor's PATH cannot be rooted")?;
        native.insert("PATH".to_owned(), path);
    }
    for (name, value) in given.script_env.iter() {
        native.insert(name.clone(), value.materialize(workspace)?);
    }
    for (name, value) in given.config_env.iter() {
        let separately_recorded = environment::is_script_var(name, &host.triple)
            || name.starts_with("CARGO_")
            || environment::RA_JIT_CARRIED.contains(&name.as_str())
            || matches!(name.as_str(), "RUSTFLAGS" | "RUSTDOCFLAGS" | "RA_JIT_SPLICE");
        if !separately_recorded && !matches!(value, ConfigEnvValue::WorkspaceRelative(_)) {
            native.insert(name.clone(), value.materialize(workspace)?);
        }
    }
    let config_image = record.config.image(&root)?;
    let cargo_home = match &record.cargo_home {
        Some(home) => root.join(home.to_native()?),
        None => {
            destination.parent().context("replay has no parent directory")?.join("resolution-home")
        }
    };
    native.insert("CARGO_HOME".to_owned(), cargo_home.into());
    let env = environment::text_view(&native)?;
    let ctx = Context {
        source_root: Some(&root),
        config_root: Some(&root),
        config_image: Some(&config_image),
        cwd: &cwd,
        env: &env,
        native_env: Some(&native),
        host,
        source: given.source.clone(),
        toolchain: identity,
        offline: true,
        store: None,
        probe,
        rust_release,
    };
    let sources = captured
        .iter()
        .map(|((source, name, version), path)| {
            let directory = root.join(path.to_native()?);
            let manifest = directory.join("Cargo.toml").canonicalize()?;
            let logical = relative(&root, &manifest)?;
            ensure!(
                Snapshot::read_file(source_root(given)?, store, &logical)?
                    == std::fs::read(&manifest)?,
                "captured package manifest differs from its source snapshot"
            );
            Ok(blaze_metadata::CapturedSource {
                source: source.clone(),
                name: name.clone(),
                version: version.clone(),
                root: directory,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let facts =
        crate::resolver_facts::decode(&Snapshot::read_file(source.root, store, &record.facts)?)?;
    let resolved =
        assemble::resolve_captured(&request, &ctx, Some(&record.platform), &sources, &facts)?;
    if let Some(difference) = crate::run::first_difference(given, &resolved.spec.build) {
        bail!("captured request does not match the BuildSpec: {difference}");
    }
    // Absolute path dependencies/configuration and escaping aliases may still resolve
    // on the worker. They are never allowed to certify a snapshot build.
    for (path, bytes) in &resolved.inputs {
        let canonical = path.canonicalize()?;
        let logical = relative(&root, &canonical)?;
        ensure!(
            Snapshot::read_file(source.root, store, &logical)? == *bytes,
            "structural input {} differs from the snapshot",
            path.display()
        );
    }
    for unit in &resolved.graph.units {
        let canonical = unit.target.src_path.canonicalize()?;
        let logical = relative(&root, &canonical)?;
        ensure!(
            Snapshot::read_file(source.root, store, &logical)? == std::fs::read(&canonical)?,
            "unit source {} differs from the snapshot",
            canonical.display()
        );
        relative(&root, &unit.package.manifest_dir.canonicalize()?)?;
    }
    assemble::canonical_units(given, &resolved.graph)?;
    let (verified, observation) = assemble::assemble_observed(&request, &ctx, &resolved.graph)?;
    if record.platform != verified.platform {
        let (a, b) = (&record.platform, &verified.platform);
        let fields: Vec<_> = [
            ("triple", a.triple == b.triple),
            ("os", a.os == b.os),
            ("sdk", a.sdk == b.sdk),
            ("libc", a.libc == b.libc),
            ("linker", a.linker == b.linker),
            ("cpu", a.cpu == b.cpu),
            ("cc", a.cc == b.cc),
            ("cxx", a.cxx == b.cxx),
            ("ar", a.ar == b.ar),
            ("native_root", a.native_root == b.native_root),
        ]
        .into_iter()
        .filter_map(|(field, same)| (!same).then_some(field))
        .collect();
        let detail = if a.native_root != b.native_root {
            baseline.zip(observation.as_ref()).map(|(before, after)| before.difference(after))
        } else {
            None
        };
        let mut message = format!(
            "execution machine does not match the BuildSpec: platform fields differ: {}",
            fields.join(", ")
        );
        if let Some(detail) = detail {
            message.push_str("; ");
            message.push_str(&detail);
        }
        bail!("{message}");
    }
    if let Some(difference) = crate::run::first_difference(given, &verified.build) {
        bail!("execution machine does not match the BuildSpec: {difference}");
    }
    Ok(Prepared {
        resolved,
        root,
        cwd,
        request,
        environment: native,
        host: host.clone(),
        config_image,
        requester_names: record.requester.iter().copied().collect(),
    })
}

fn source_root(build: &BuildSpec) -> anyhow::Result<blaze_wire::Digest> {
    match &build.source {
        SourceRef::Root(source) => Ok(source.root),
        _ => bail!("replay requires immutable source inputs"),
    }
}
