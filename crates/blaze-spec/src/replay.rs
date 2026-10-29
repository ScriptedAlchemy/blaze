//! Portable replay inputs. Original invocation recipes are outside build identity;
//! the immutable source snapshot authenticates the locked package-to-source mapping.
//!
//! Reproduction never guesses flags from the resulting units. It replays the original
//! request against the captured configuration and manifests, resolves offline, and
//! compares the entire BuildSpec. Only then is the execution platform probed.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail, ensure};
use blaze_manifest::config::ConfigImage;
use blaze_wire::{
    BuildSpec, ConfigEnvValue, ExecPlatform, RootRef, SourcePath, SourceRef, StoreIdentity, Wire,
};

use crate::{
    assemble::{self, Context, Host, Resolved, Spec},
    environment,
    flags::{FLAGS, Request, Subcommand, Takes},
    platform::Probe,
    recipe::{ConfigFile, ReplayRecipe},
    snapshot::Snapshot,
};

const SOURCES: &str = ".blaze-sources.cbor";
/// Where a Cargo home outside the workspace's own layout is kept in the snapshot.
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
}

impl Prepared {
    /// The effective compiler/run environment, after applying the spec's configuration
    /// values. Configuration-derived relative paths must not be ambient overrides while
    /// reproducing the configuration itself.
    pub fn execution_environment(&self) -> anyhow::Result<BTreeMap<String, OsString>> {
        let manifest = self.root.join(self.resolved.spec.build.workspace_manifest.to_native()?);
        let workspace = manifest.parent().context("workspace manifest has no parent")?;
        let mut env = BTreeMap::new();
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
const RECORD_VERSION: u32 = 2;

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
    config: ConfigLayers,
}

impl Wire for Record {
    fn encode(&self, e: &mut blaze_wire::Encoder) {
        e.array(6);
        RECORD_VERSION.encode(e);
        self.recipe.encode(e);
        self.manifest.encode(e);
        self.cargo_home.encode(e);
        self.platform.encode(e);
        self.config.encode(e);
    }

    fn decode(r: &mut blaze_wire::Reader<'_>) -> Result<Self, blaze_wire::Error> {
        r.array_start(6)?;
        let at = r.position();
        if u32::decode(r)? != RECORD_VERSION {
            return Err(blaze_wire::Error {
                kind: blaze_wire::ErrorKind::UnexpectedType { expected: "replay record version 2" },
                at,
            });
        }
        let record = Self {
            recipe: <ReplayRecipe as Wire>::decode(r)?,
            manifest: SourcePath::decode(r)?,
            cargo_home: Option::decode(r)?,
            platform: ExecPlatform::decode(r)?,
            config: ConfigLayers::decode(r)?,
        };
        r.array_end()?;
        Ok(record)
    }
}

/// `path` spelled below `root`. A system alias above the layout, such as macOS's `/var` for
/// `/private/var`, names one directory two ways; a root made of both would be `/`, and
/// capture would follow that link and walk the machine. Only the parent of a path that is
/// not below `root` as written is resolved, so a link in the last component is kept.
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
    root: &Path,
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
                        files
                            .push(ConfigFile { argument: at as u32, path: relative(root, &path)? });
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
    let first = assemble::resolve(request, ctx)?;
    let native = native(ctx);
    let home = environment::cargo_home(&native, ctx.cwd)
        .unwrap_or_else(|| first.graph.root.join(".blaze-cargo-home"));
    let home = blaze_manifest::paths::normalize_path(&home);
    let mut trees = vec![first.graph.root.clone()];
    trees.extend(first.graph.units.iter().map(|unit| unit.package.manifest_dir.clone()));
    // Resolution reads packages that are locked but not selected, too. Preserve their
    // source roots so the same offline resolution can prove they remain unselected.
    trees.extend(first.inputs.iter().filter_map(|(path, _)| {
        (path.file_name().is_some_and(|name| name == "Cargo.toml"))
            .then(|| path.parent().map(Path::to_owned))
            .flatten()
    }));
    trees.sort();
    trees.dedup();
    let mut layout = trees.clone();
    layout.extend(first.inputs.iter().filter_map(|(path, _)| path.parent().map(Path::to_owned)));
    // The layout is made of directories: spell each by its canonical name, so one directory
    // never appears under two names in the common root.
    let canonical = |path: &Path| {
        let path = blaze_manifest::paths::normalize_path(path);
        path.canonicalize().unwrap_or(path)
    };
    // A Cargo home beside or above the workspace is mounted in the snapshot rather than laid
    // out around it: otherwise the layout root, and with it every path and digest of the
    // spec, would depend on where the home lives relative to the sources, and on the
    // directories above both. Only a home that neither holds nor lies in the workspace is
    // mounted; one inside it is captured with the rest.
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
    // Where a path is in the snapshot: below the layout root, or in the mounted home.
    let place = |path: &Path| -> anyhow::Result<SourcePath> {
        if in_home(path) {
            let inside = anchor(&home_dir, path);
            let inside = inside.strip_prefix(&home_dir)?;
            Ok(SourcePath::from_native(&Path::new(CARGO_HOME).join(inside))?)
        } else {
            relative(&root, path)
        }
    };
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
    };
    // This pass also resolves workspace-relative configuration paths against the
    // final layout, rather than the provisional workspace root.
    // The platform is read again here: the requester's tree is now the layout root, which is
    // what a replay's snapshot is, and a tool inside it is named by its bytes and not run.
    let mut resolved = assemble::resolve_captured(request, &captured_ctx, None, &first.sources)?;
    resolved.inputs.extend(first.inputs);
    let config_files = config_files(words, ctx.cwd, &root, &resolved.inputs)?;
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
    let has_home = !home_trees.is_empty() || !home_inputs.is_empty();
    if has_home {
        let trees: Vec<_> = home_trees.iter().map(|path| anchor(&home_dir, path)).collect();
        let inputs: Vec<_> = home_inputs
            .iter()
            .map(|(path, bytes)| (anchor(&home_dir, path), bytes.clone()))
            .collect();
        let at = SourcePath::new(CARGO_HOME.to_owned())?;
        let held = Snapshot::capture_selected(&home_dir, &trees, &excluded, &inputs, store)?;
        snapshot = snapshot.with_mount(&at, held)?;
    }
    let snapshot = snapshot.with_file(
        &SourcePath::new(SOURCES.to_owned())?,
        &blaze_wire::encode(&(1_u32, sources)),
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
        config: ConfigLayers {
            layers: configuration(&resolved.config.layers)?,
            files: configuration(&resolved.config.files)?,
        },
        manifest: place(&resolved.manifest)?,
        // An empty external Cargo home is not a source input and must not make the
        // source root depend on the absolute spelling of a checkout's location.
        cargo_home: if has_home {
            Some(SourcePath::new(CARGO_HOME.to_owned())?)
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
    };
    let relocated = tempfile::tempdir()?;
    prepare_record(
        &made.build,
        store,
        &relocated.path().join("source"),
        ctx.host,
        ctx.toolchain,
        ctx.probe,
        &native,
        &record,
    )
    .context("captured inputs cannot reproduce the request offline")?;
    let mut writing = tempfile::NamedTempFile::new_in(store)?;
    writing.write_all(&blaze_wire::encode(&record))?;
    writing.as_file().sync_all()?;
    writing.persist(record_path(&made.build, store)).map_err(|error| error.error)?;
    Ok(made)
}

/// Authenticate and materialize a replay, reproduce its exact graph and request, and
/// only then verify the native execution platform. Nothing from the caller's cwd or
/// compiler environment is used. The destination must not already exist.
pub fn prepare(
    given: &BuildSpec,
    store: &Path,
    destination: &Path,
    host: &Host,
    identity: StoreIdentity,
    probe: &dyn Probe,
    executor: &BTreeMap<String, OsString>,
) -> anyhow::Result<Prepared> {
    let bytes = std::fs::read(record_path(given, store))
        .context("the saved replay recipe is missing or does not match this BuildSpec")?;
    let record: Record = blaze_wire::decode(&bytes, &blaze_wire::Limits::DOCUMENTED)?;
    prepare_record(given, store, destination, host, identity, probe, executor, &record)
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
    executor: &BTreeMap<String, OsString>,
    record: &Record,
) -> anyhow::Result<Prepared> {
    let SourceRef::Root(source) = &given.source else {
        bail!("this spec has no immutable replay inputs; save it again with --out");
    };
    ensure!(given.toolchain == identity, "the replay toolchain differs from the BuildSpec");
    ensure!(
        record.recipe.spec_digest() == blaze_wire::digest_of(given),
        "recipe does not match the BuildSpec"
    );
    let bytes = Snapshot::read_file(source.root, store, &SourcePath::new(SOURCES.to_owned())?)?;
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
    let resolved = assemble::resolve_captured(&request, &ctx, Some(&record.platform), &sources)?;
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
    let verified = assemble::assemble(&request, &ctx, &resolved.graph)?;
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
    })
}

fn source_root(build: &BuildSpec) -> anyhow::Result<blaze_wire::Digest> {
    match &build.source {
        SourceRef::Root(source) => Ok(source.root),
        _ => bail!("replay requires immutable source inputs"),
    }
}
