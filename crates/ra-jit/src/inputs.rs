//! What a checkout was loaded from besides the source: its manifests and lock file, cargo's
//! configuration, the flags for rustc in the environment, and what its build scripts read.
//! Before each build the daemon looks whether they are still what it loaded, runs the build
//! scripts again where cargo would, and says which checkouts are to be loaded again.
//!
//! When a script runs again is cargo's rule, which `blaze_build_script::apply::Rerun` has
//! (rust-lang/cargo's `fingerprint/mod.rs` at 5f94df478): for a change to a file it named
//! (newer than the run, or not there, which is a change every build), to a variable it named
//! (unset, empty and set are three values), to any file of its package when it named nothing,
//! to the script itself or a package it is built with, and once more for having named other
//! than it did the run before, as cargo's fingerprint of that run was made of what the run
//! before it said. What is asked is always cargo's own run in its own target directory, so the
//! daemon is never the one that decides a script's output is the same: it only does not ask
//! while nothing cargo reads has changed.

use std::{
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
    time::SystemTime,
};

use blaze_build_script::{apply::Rerun, output};
use ide_db::base_db::{Env, TargetKind};
use project_model::{CargoConfig, ProjectWorkspace, ProjectWorkspaceKind};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

/// The variables cargo takes flags for rustc from.
const FLAGS: &[&str] = &["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS"];

/// The variables of a build that are the daemon's own, not the process asking's: what picks
/// where it works and what it wraps.
const OWN: &[&str] = &["CARGO_TARGET_DIR", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"];

/// What each of the daemon's checkouts that is loaded in full was loaded from.
#[derive(Default)]
pub(crate) struct Inputs {
    checkouts: Vec<Checkout>,
    /// The names of the variables the processes that asked for a build had, which the daemon
    /// was started with some of: one of them a later process lacks is unset for the scripts.
    asked_with: FxHashSet<String>,
}

/// What the workspace at `root` was loaded from.
struct Checkout {
    root: PathBuf,
    /// The manifests and the lock file, the build scripts cargo finds by their name, and
    /// the files cargo's configuration is read from: those that are not there too.
    files: Vec<Watched>,
    /// The variables of `FLAGS` as cargo was run with them.
    flags: Vec<(&'static str, Option<String>)>,
    scripts: Option<Scripts>,
}

/// A file or a directory, and what it was when the daemon read it.
#[derive(Clone)]
struct Watched {
    path: PathBuf,
    /// Whether the directory is a package, of which cargo leaves out what a build wrote and
    /// the packages inside.
    package: bool,
    stamp: u64,
}

impl Watched {
    fn new(path: PathBuf, package: bool) -> Watched {
        let stamp = stamp(&path, package);
        Watched { path, package, stamp }
    }

    fn changed(&self) -> bool {
        stamp(&self.path, self.package) != self.stamp
    }
}

/// The build scripts of one workspace and what they said.
struct Scripts {
    workspace: ProjectWorkspace,
    /// How cargo is run for them, with the variables of the process the workspace was loaded
    /// for and none of a later one's ([`run_env`]).
    config: CargoConfig,
    /// The scripts of the workspace's own packages, with what cargo runs each again for a
    /// change of.
    scripts: Vec<Script>,
    /// The variables it runs one again for a change of, as they were when they last ran.
    variables: Vec<(String, Option<String>)>,
}

/// What a script said it reads when it ran, and when that run began.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Said {
    rerun: Rerun,
    /// Cargo dates the output of a run with its start.
    began: SystemTime,
}

impl Said {
    /// What the run that wrote `out_dir` said, from the output cargo keeps beside it. `None`
    /// when there is none, or when it is one cargo fails the build for.
    fn read(out_dir: &Path, package: &Path) -> Option<Said> {
        let printed = crate::cargo::script::printed(out_dir)?;
        let began = std::fs::metadata(&printed).and_then(|it| it.modified()).ok()?;
        let reads = output::reads(&std::fs::read(&printed).ok()?).ok()?;
        Some(Said { rerun: reads.rerun(package), began })
    }

    /// The variables it said.
    fn variables(&self) -> &[String] {
        match &self.rerun {
            Rerun::Watched { env, .. } => env,
            Rerun::Package => &[],
        }
    }
}

/// The build script of one package of the workspace.
struct Script {
    /// Where cargo had it write for the daemon's load.
    out_dir: PathBuf,
    /// The directories cargo's own builds run it in: `<package>-<hash>` below these.
    builds: Vec<PathBuf>,
    package: String,
    /// The package's directory, which cargo runs the script in.
    root: PathBuf,
    /// Where cargo ran rustc, which the files rustc read are named from.
    workspace: PathBuf,
    /// The roots of the script's targets.
    targets: Vec<PathBuf>,
    /// The packages of the workspace the script is built with: its build dependencies, and
    /// what those depend on. Cargo compiles the script again, and so runs it again, when one
    /// has changed.
    built_with: Vec<PathBuf>,
    /// What cargo compiles the script again for a change to, and a script that says nothing of
    /// what it reads runs again for: as it was when cargo last read it.
    files: Vec<Watched>,
    /// What the last run said. `None` when what it wrote is not there to read.
    said: Option<Said>,
    /// Whether cargo runs the script again for it having said other than it did the run
    /// before: its fingerprint of that run is of what the one before said.
    owes: bool,
}

impl Script {
    fn changed(&self) -> bool {
        // What it said, when it did not say or cannot be read, is nothing: its package's files.
        let stale = |said: &Said| said.rerun.stale_files(&self.root, said.began).is_some();
        self.owes
            || self.files.iter().any(Watched::changed)
            || self.said.as_ref().is_some_and(stale)
    }

    /// What cargo compiles the script again for a change to, and a script that said nothing
    /// of what it reads runs again for, with what each was at `before` where it was then.
    fn watched(&self, before: &[Watched]) -> Vec<Watched> {
        let mut files: Vec<(PathBuf, bool)> = Vec::new();
        files.extend(self.targets.iter().map(|it| (it.clone(), false)));
        files.extend(self.compiled_from().into_iter().map(|it| (it, false)));
        files.extend(self.built_with.iter().map(|it| (it.clone(), true)));
        if self.said.as_ref().is_none_or(|it| it.rerun == Rerun::Package) {
            files.push((self.root.clone(), true));
        }
        files.sort();
        files.dedup();
        let kept = |path: &Path, package: bool| {
            before.iter().find(|it| it.path == path && it.package == package).cloned()
        };
        files
            .into_iter()
            .map(|(path, package)| {
                kept(&path, package).unwrap_or_else(|| Watched::new(path, package))
            })
            .collect()
    }

    /// The files cargo's compile of the script read besides its root: the modules and what
    /// `include!` and its kin took, from the dep-info of each compile there is.
    fn compiled_from(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let in_dir = |dir: &Path, found: &mut Vec<PathBuf>| {
            let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
            for entry in entries.filter(|it| it.path().extension().is_some_and(|it| it == "d")) {
                let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
                let read = crate::dep_info::DepInfo::parse(&text);
                // Where rustc ran: the workspace's for a member, the package's for another.
                let named = read.files.iter();
                found.extend(named.flat_map(|it| [self.workspace.join(it), self.root.join(it)]));
            }
        };
        // `build/<package>-<hash>` in cargo's first layout, `build/<package>/<hash>` in its
        // second: of cargo's own builds, and of the one that ran the script for the load.
        let prefix = format!("{}-", self.package);
        let loaded =
            self.out_dir.ancestors().find(|it| it.file_name().is_some_and(|it| it == "build"));
        for build in self.builds.iter().map(PathBuf::as_path).chain(loaded) {
            let units = std::fs::read_dir(build).into_iter().flatten().flatten();
            for unit in units {
                let name = unit.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix) {
                    in_dir(&unit.path(), &mut found);
                } else if name == self.package {
                    let hashes = std::fs::read_dir(unit.path()).into_iter().flatten().flatten();
                    hashes.for_each(|it| in_dir(&it.path(), &mut found));
                }
            }
        }
        found
    }

    /// Whether the script, run again as cargo ran it, prints and writes what it did for the
    /// load: what cargo would find when it ran it, without asking cargo. Not when the script
    /// itself is older than its source, or was compiled reading a variable cargo does not
    /// set; cargo compiles it again.
    fn reruns_the_same(&self) -> bool {
        use crate::cargo::script;
        let Some(out_dir) = self.ran() else { return false };
        if !script::compiled_from_what_is_there(&out_dir, &self.workspace) {
            return false;
        }
        // `cargo ra-jit` ran it again before it asked for this build, as it does when
        // something it reads changes: that is not done twice.
        let newest = self.files.iter().map(|it| newest(&it.path, it.package));
        let newest = newest.collect::<Option<Vec<_>>>().and_then(|it| it.into_iter().max());
        if newest.is_some_and(|it| script::checked_after(&out_dir, it))
            && script::same_run(&out_dir, &self.out_dir)
        {
            return true;
        }
        script::rerun_matches(&out_dir, &self.out_dir).is_ok()
    }

    /// Where a run of the script that can be run again wrote: the load's, or else the last
    /// one of cargo's own builds of the package, which `cargo ra-jit` has write down how it
    /// ran. The script and what it reads are the same for both.
    fn ran(&self) -> Option<PathBuf> {
        if crate::cargo::script::can_rerun(&self.out_dir) {
            return Some(self.out_dir.clone());
        }
        // `build/<package>-<hash>/out` in cargo's first layout, `build/<package>/<hash>/out`
        // in its second.
        let prefix = format!("{}-", self.package);
        let first = self.builds.iter().filter_map(|it| std::fs::read_dir(it).ok()).flatten();
        let first = first
            .flatten()
            .filter(|it| it.file_name().to_str().is_some_and(|it| it.starts_with(&prefix)))
            .map(|it| it.path());
        let second =
            self.builds.iter().filter_map(|it| std::fs::read_dir(it.join(&self.package)).ok());
        let second = second.flatten().flatten().map(|it| it.path());
        let runs = first.chain(second).map(|it| it.join("out"));
        let runs =
            runs.filter(|it| crate::cargo::script::ran_in(it).is_some_and(|it| it == self.root));
        runs.max_by_key(|it| {
            let printed = crate::cargo::script::printed(it);
            printed.and_then(|it| std::fs::metadata(it).and_then(|it| it.modified()).ok())
        })
    }

    /// Cargo ran the build scripts again: what this one said, whether it ran, and what it
    /// is now watched for, with the files as they were `before` it, since cargo's fingerprint
    /// is made before the run and what the run wrote is a change at the next.
    fn ran_again(&mut self, before: &[Watched]) {
        let said = Said::read(&self.out_dir, &self.root);
        let ran = match (&self.said, &said) {
            (Some(was), Some(now)) => now.began != was.began,
            (None, Some(_)) => true,
            _ => false,
        };
        let said_other = |(was, now): (&Said, &Said)| was.rerun != now.rerun;
        self.owes = ran && self.said.as_ref().zip(said.as_ref()).is_some_and(said_other);
        self.said = said;
        self.files = self.watched(before);
    }
}

/// The variable `name` as the commands of `config` are run with it.
fn run_with(config: &CargoConfig, name: &str) -> Option<String> {
    match config.extra_env.get(name) {
        Some(value) => value.clone(),
        None => std::env::var(name).ok(),
    }
}

/// The variables cargo's commands are run with over what the daemon has, for a build by a
/// process with `env`: those of the build that the daemon's own are not, `always` whatever
/// they are, and those a process that asked before had and this one has not, unset, which
/// the daemon, started by one of them, still has. `own` is what the daemon has.
fn run_env<'a>(
    own: &dyn Fn(&str) -> Option<String>,
    asked_with: &FxHashSet<String>,
    env: &Env,
    always: impl Iterator<Item = &'a str>,
) -> FxHashMap<String, Option<String>> {
    let theirs = |name: &str| !name.starts_with("RA_JIT_") && !OWN.contains(&name);
    let mut vars = FxHashMap::default();
    for (name, value) in env.iter().filter(|(name, _)| theirs(name)) {
        if own(name).as_deref() != Some(value) {
            vars.insert(name.to_owned(), Some(value.to_owned()));
        }
    }
    for name in asked_with.iter().filter(|name| theirs(name)) {
        if !env.contains_key(name) && own(name).is_some() {
            vars.insert(name.clone(), None);
        }
    }
    for name in always {
        vars.insert(name.to_owned(), env.get(name));
    }
    vars
}

impl Inputs {
    /// Has the workspace at `root`, loaded with `config`, be one whose inputs are watched. The
    /// variables the process it was loaded for gave it over the daemon's are `given`.
    pub(crate) fn watch(
        &mut self,
        root: &Path,
        workspace: &ProjectWorkspace,
        config: &CargoConfig,
        given: &FxHashMap<String, Option<String>>,
    ) {
        let mut files = Vec::new();
        for dir in root.ancestors() {
            for name in ["config.toml", "config"] {
                files.push(Watched::new(dir.join(".cargo").join(name), false));
            }
        }
        for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain"] {
            files.push(Watched::new(root.join(name), false));
        }
        if let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind {
            for package in cargo.packages().filter(|&it| cargo[it].is_local) {
                let manifest: &Path = cargo[package].manifest.as_ref();
                files.push(Watched::new(manifest.to_owned(), false));
                // A script that appears there is run without the manifest naming it.
                files.push(Watched::new(manifest.with_file_name("build.rs"), false));
            }
        }
        let flags = FLAGS.iter().map(|&it| (it, run_with(config, it))).collect();
        // The daemon was started by one of the processes that ask it for builds, and has its
        // variables.
        if self.asked_with.is_empty() {
            self.asked_with.extend(std::env::vars().map(|it| it.0));
        }
        let mut scripts = Scripts::of(workspace, config, given);
        // A script that owes a run is owed it still when the checkout is loaded again.
        let owing = |scripts: &Option<Scripts>, out_dir: &Path| {
            let scripts = scripts.as_ref();
            scripts.is_some_and(|it| it.scripts.iter().any(|it| it.out_dir == out_dir && it.owes))
        };
        let before = self.checkouts.iter().find(|it| it.root == root);
        if let (Some(before), Some(now)) = (before, &mut scripts) {
            for script in &mut now.scripts {
                script.owes |= owing(&before.scripts, &script.out_dir);
            }
        }
        self.checkouts.retain(|it| it.root != root);
        self.checkouts.push(Checkout { root: root.to_owned(), files, flags, scripts });
    }

    /// Whether the checkout at `root` is no longer what it was loaded from. It is asked for
    /// a process with the variables `env`, or with `None` for whoever it was loaded for.
    /// An error is why its build scripts can't say.
    pub(crate) fn stale(&mut self, root: &Path, env: Option<&Env>) -> anyhow::Result<bool> {
        if let Some(env) = env {
            self.asked_with.extend(env.iter().map(|(name, _)| name.to_owned()));
        }
        let asked_with = &self.asked_with;
        let Some(checkout) = self.checkouts.iter_mut().find(|it| it.root == root) else {
            return Ok(false);
        };
        if checkout.files.iter().any(Watched::changed) {
            return Ok(true);
        }
        if let Some(env) = env
            && checkout.flags.iter().any(|(name, was)| env.get(name) != *was)
        {
            return Ok(true);
        }
        match &mut checkout.scripts {
            Some(scripts) => scripts.changed(env, asked_with),
            None => Ok(false),
        }
    }

    /// The variables cargo and the build scripts of the checkout at `root` read, as a
    /// process with `env` has them, and the others it has otherwise than the daemon.
    pub(crate) fn variables(&self, root: &Path, env: &Env) -> FxHashMap<String, Option<String>> {
        let scripts = self.checkouts.iter().find(|it| it.root == root);
        let scripts = scripts.and_then(|it| it.scripts.as_ref());
        let read = scripts.into_iter().flat_map(|it| it.variables.iter().map(|it| &*it.0));
        let own = |name: &str| std::env::var(name).ok();
        run_env(&own, &self.asked_with, env, FLAGS.iter().copied().chain(read))
    }
}

impl Scripts {
    fn of(
        workspace: &ProjectWorkspace,
        config: &CargoConfig,
        given: &FxHashMap<String, Option<String>>,
    ) -> Option<Scripts> {
        let ProjectWorkspaceKind::Cargo { cargo, build_scripts, .. } = &workspace.kind else {
            return None;
        };
        // How the daemon runs cargo for them: the variables the load was given, from a later
        // process, are not what a later run of them starts from.
        let mut config = config.clone();
        config.extra_env.retain(|name, _| !given.contains_key(name));
        let value = |name: &str| match given.get(name) {
            Some(value) => value.clone(),
            None => run_with(&config, name),
        };
        let (mut scripts, mut variables) = (Vec::new(), Vec::new());
        for package in cargo.packages() {
            let Some(out_dir) = build_scripts.out_dir(package) else { continue };
            let root: &Path = cargo[package].manifest.parent().as_ref();
            let out_dir: &Path = out_dir.as_ref();
            let said = Said::read(out_dir, root);
            let named = said.iter().flat_map(|it| it.variables().iter().cloned());
            variables.extend(named.map(|it| {
                let value = value(&it);
                (it, value)
            }));
            if !cargo[package].is_local {
                continue;
            }
            let script = cargo[package].targets.iter().map(|&it| &cargo[it]);
            let targets = script.filter(|it| it.kind == TargetKind::BuildScript);
            let workspace: &Path = cargo.workspace_root().as_ref();
            let mut script = Script {
                out_dir: out_dir.to_owned(),
                builds: builds_of(cargo, &config),
                package: cargo[package].name.clone(),
                root: root.to_owned(),
                workspace: workspace.to_owned(),
                targets: targets.map(|it| PathBuf::from(it.root.as_str())).collect(),
                built_with: built_with(cargo, package),
                files: Vec::new(),
                said,
                owes: false,
            };
            script.files = script.watched(&[]);
            scripts.push(script);
        }
        variables.sort();
        variables.dedup();
        if scripts.is_empty() && variables.is_empty() {
            return None;
        }
        Some(Scripts { workspace: workspace.clone(), config, scripts, variables })
    }

    /// Runs the scripts again where cargo would for a process with `env`, or with `None`
    /// for one with the variables they last ran with: whether what they say is no longer
    /// what was loaded.
    fn changed(
        &mut self,
        env: Option<&Env>,
        asked_with: &FxHashSet<String>,
    ) -> anyhow::Result<bool> {
        let changed: Vec<usize> =
            (0..self.scripts.len()).filter(|&at| self.scripts[at].changed()).collect();
        let variables =
            env.is_some_and(|env| self.variables.iter().any(|(name, was)| env.get(name) != *was));
        if changed.is_empty() && !variables {
            return Ok(false);
        }
        // Cargo would run only the scripts whose files changed: each is run again here as
        // cargo ran it, which costs the script and not a cargo that resolves the workspace.
        if !variables && changed.iter().all(|&at| self.scripts[at].reruns_the_same()) {
            for at in changed {
                self.scripts[at].files = self.scripts[at].watched(&[]);
            }
            return Ok(false);
        }
        let ProjectWorkspaceKind::Cargo { build_scripts: loaded, .. } = &self.workspace.kind else {
            return Ok(false);
        };
        // The scripts run as they do under cargo, with the variables of the process that asks.
        let mut config = self.config.clone();
        if let Some(env) = env {
            let own = |name: &str| std::env::var(name).ok();
            let names = self.variables.iter().map(|it| &*it.0).chain(FLAGS.iter().copied());
            config.extra_env.extend(run_env(&own, asked_with, env, names));
        } else {
            for (name, was) in &self.variables {
                config.extra_env.insert(name.clone(), was.clone());
            }
        }
        // Cargo makes its fingerprint of a script before it runs it.
        let before: Vec<Vec<Watched>> = self.scripts.iter().map(|it| it.watched(&[])).collect();
        let said = self.workspace.run_build_scripts(&config, &|_| {})?;
        if let Some(error) = said.error() {
            anyhow::bail!("a build script failed: {}", error.trim().replace('\n', " "));
        }
        for (script, before) in self.scripts.iter_mut().zip(&before) {
            script.ran_again(before);
        }
        // What each said it reads now, and the value of each as the run had it.
        let mut variables: Vec<(String, Option<String>)> = Vec::new();
        for script in &self.scripts {
            let named = script.said.iter().flat_map(|it| it.variables().iter());
            variables.extend(named.map(|it| (it.clone(), run_with(&config, it))));
        }
        for (name, was) in &self.variables {
            if !variables.iter().any(|it| it.0 == *name) {
                variables.push((name.clone(), was.clone()));
            }
        }
        variables.sort();
        variables.dedup();
        self.variables = variables;
        Ok(said != *loaded)
    }
}

/// The packages of the workspace that `package`'s build script is built with: its build
/// dependencies of the workspace and the packages of the workspace those depend on, whatever
/// the dependency.
fn built_with(
    cargo: &project_model::CargoWorkspace,
    package: project_model::Package,
) -> Vec<PathBuf> {
    // `project_model` does not export the kind of a dependency, which only names itself.
    let kind = |it: &project_model::PackageDependency| format!("{:?}", it.kind);
    let mut found: Vec<project_model::Package> = Vec::new();
    let build = cargo[package].dependencies.iter().filter(|it| kind(it) == "Build");
    let mut pending: Vec<project_model::Package> = build.map(|it| it.pkg).collect();
    while let Some(next) = pending.pop() {
        if found.contains(&next) || next == package {
            continue;
        }
        found.push(next);
        let linked = cargo[next].dependencies.iter().filter(|it| kind(it) != "Dev");
        pending.extend(linked.map(|it| it.pkg));
    }
    let dirs = found.into_iter().filter(|&it| cargo[it].is_local);
    dirs.map(|it| AsRef::<Path>::as_ref(cargo[it].manifest.parent()).to_owned()).collect()
}

/// The directories cargo runs the build scripts of the build `config` asks for in:
/// `build` of the profile's directory, for the target that is asked for or the host.
fn builds_of(cargo: &project_model::CargoWorkspace, config: &CargoConfig) -> Vec<PathBuf> {
    let profile = config.extra_args.iter().skip_while(|it| *it != "--profile").nth(1);
    let profile = match profile.map(String::as_str) {
        None | Some("dev" | "test") => "debug",
        Some("bench") => "release",
        Some(other) => other,
    };
    let target: &Path = cargo.target_directory().as_ref();
    let mut dirs = vec![target.to_owned()];
    dirs.extend(cargo.build_directory().map(|it| AsRef::<Path>::as_ref(it).to_owned()));
    let triples = config.target.iter();
    let under: Vec<PathBuf> =
        dirs.iter().flat_map(|dir| triples.clone().map(move |it| dir.join(it))).collect();
    dirs.extend(under);
    dirs.into_iter().map(|it| it.join(profile).join("build")).collect()
}

/// A number that changes when the file at `path`, or a file below the directory there, is
/// written, added or removed.
fn stamp(path: &Path, package: bool) -> u64 {
    let mut hasher = FxHasher::default();
    stamp_into(path, package, &mut hasher);
    hasher.finish()
}

/// When the file at `path`, or the directory there or anything below it that [`stamp`] looks
/// at, was last written. Nothing when it is not there.
fn newest(path: &Path, package: bool) -> Option<std::time::SystemTime> {
    let metadata = std::fs::metadata(path).ok()?;
    let mut newest = metadata.modified().ok()?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).ok()? {
            let entry = entry.ok()?.path();
            let built = entry.join("CACHEDIR.TAG").exists() || entry.ends_with(".git");
            if package && (built || entry.join("Cargo.toml").exists()) {
                continue;
            }
            newest = newest.max(self::newest(&entry, package)?);
        }
    }
    Some(newest)
}

fn stamp_into(path: &Path, package: bool, hasher: &mut FxHasher) {
    let Ok(metadata) = std::fs::metadata(path) else { return };
    if !metadata.is_dir() {
        (metadata.len(), metadata.modified().ok()).hash(hasher);
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return };
    let mut entries: Vec<PathBuf> = entries.filter_map(|it| Some(it.ok()?.path())).collect();
    entries.sort();
    for entry in entries {
        let built = entry.join("CACHEDIR.TAG").exists() || entry.ends_with(".git");
        if package && (built || entry.join("Cargo.toml").exists()) {
            continue;
        }
        entry.hash(hasher);
        stamp_into(&entry, package, hasher);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ra-jit-inputs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn set_time(path: &Path, time: SystemTime) {
        std::fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
    }

    /// A package at `root/pkg` whose script wrote `printed` to `output` beside its `OUT_DIR` at
    /// `at`, as cargo's first layout has it, and the `Script` that watches it.
    fn script(root: &Path, printed: &str, at: SystemTime) -> Script {
        let package = root.join("pkg");
        let run = root.join("target/debug/build/pkg-1");
        std::fs::create_dir_all(run.join("out")).unwrap();
        std::fs::create_dir_all(package.join("src")).unwrap();
        std::fs::write(package.join("build.rs"), "fn main() {}").unwrap();
        std::fs::write(run.join("output"), printed).unwrap();
        set_time(&run.join("output"), at);
        let mut script = Script {
            out_dir: run.join("out"),
            builds: vec![root.join("target/debug/build")],
            package: "pkg".to_owned(),
            root: package.clone(),
            workspace: root.to_owned(),
            targets: vec![package.join("build.rs")],
            built_with: Vec::new(),
            files: Vec::new(),
            said: Said::read(&run.join("out"), &package),
            owes: false,
        };
        script.files = script.watched(&[]);
        script
    }

    /// `rerun_if_missing`: cargo runs a script again for every build while a file it named is
    /// not there, and for one newer than its last run, and for none else.
    #[test]
    fn a_script_is_asked_for_a_file_that_is_missing_or_newer_than_its_run() {
        let root = dir("files");
        let ran = SystemTime::now() - Duration::from_secs(60);
        let script = script(&root, "cargo:rerun-if-changed=data.txt\n", ran);
        let data = root.join("pkg/data.txt");
        assert!(script.changed(), "a file that is not there, which is a change every build");
        std::fs::write(&data, "1").unwrap();
        set_time(&data, ran - Duration::from_secs(1));
        let script = self::script(&root, "cargo:rerun-if-changed=data.txt\n", ran);
        assert!(!script.changed(), "a file as old as the run");
        set_time(&data, ran + Duration::from_secs(1));
        assert!(script.changed(), "a file newer than the run");
        std::fs::remove_file(&data).unwrap();
        assert!(script.changed(), "a file that went");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `rebuild_only_on_explicit_paths`: a file the script did not name is not its concern
    /// while it names others, and the package's every file is when it names none.
    #[test]
    fn a_script_that_names_nothing_is_asked_for_any_file_of_its_package() {
        let root = dir("package");
        let ran = SystemTime::now() - Duration::from_secs(60);
        let named = script(&root, "cargo:rerun-if-changed=data.txt\n", ran);
        std::fs::write(root.join("pkg/data.txt"), "1").unwrap();
        set_time(&root.join("pkg/data.txt"), ran);
        let named = Script { files: named.watched(&[]), ..named };
        std::fs::write(root.join("pkg/src/other.rs"), "").unwrap();
        assert!(!named.changed(), "a file of the package it did not name");
        let nothing = script(&root, "cargo:rustc-cfg=a\n", ran);
        assert!(!nothing.changed());
        std::fs::write(root.join("pkg/src/other.rs"), "changed").unwrap();
        assert!(nothing.changed(), "any file of the package");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Cargo's fingerprint of a run is made of what the run before it said: a script that
    /// says other than it did is run again once more, and not again for it after that.
    #[test]
    fn a_script_that_says_other_than_it_did_owes_a_run() {
        let root = dir("owes");
        let began = SystemTime::now() - Duration::from_secs(60);
        let mut script = script(&root, "cargo:rerun-if-changed=src/main.rs\n", began);
        std::fs::write(root.join("pkg/src/main.rs"), "").unwrap();
        set_time(&root.join("pkg/src/main.rs"), began - Duration::from_secs(5));
        script.files = script.watched(&[]);
        assert!(!script.changed());
        // It ran again and stopped saying what it reads.
        let output = root.join("target/debug/build/pkg-1/output");
        std::fs::write(&output, "cargo:rustc-cfg=a\n").unwrap();
        set_time(&output, began + Duration::from_secs(30));
        script.ran_again(&[]);
        assert!(script.owes);
        assert!(script.changed(), "cargo runs it once more");
        // That run said the same, and is not owed another.
        set_time(&output, began + Duration::from_secs(40));
        script.ran_again(&[]);
        assert!(!script.owes);
        // A check in which it did not run changes nothing.
        script.owes = true;
        script.ran_again(&[]);
        assert!(!script.owes, "cargo found it fresh");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What cargo compiled the script from is what cargo compiles it again for: the modules
    /// its dep-info names, in either layout of the build directory.
    #[test]
    fn a_script_is_asked_for_the_modules_it_was_compiled_from() {
        let root = dir("modules");
        let ran = SystemTime::now();
        let script = script(&root, "cargo:rerun-if-changed=src/main.rs\n", ran);
        let first = root.join("target/debug/build/pkg-2");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join("build_script_build-2.d"), "x: build.rs table.rs\n").unwrap();
        let second = root.join("target/debug/build/pkg/3");
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("build_script_build-3.d"), "x: build.rs other/gen.rs\n")
            .unwrap();
        let found = script.compiled_from();
        for name in ["table.rs", "other/gen.rs"] {
            assert!(found.contains(&root.join(name)), "{name}: {found:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    fn env(vars: &[(&str, &str)]) -> Env {
        vars.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
    }

    /// A process that asks for a build gives the scripts its variables: those the daemon does
    /// not have alike, those it has not that one before had, and the ones asked for.
    #[test]
    fn the_scripts_run_with_the_variables_of_the_process_that_asks() {
        let daemon = |name: &str| match name {
            "SAME" => Some("1".to_owned()),
            "OLD" => Some("old".to_owned()),
            "GONE" => Some("x".to_owned()),
            _ => None,
        };
        let asked_with: FxHashSet<String> =
            ["SAME", "OLD", "GONE"].iter().map(|it| (*it).to_owned()).collect();
        let asking = env(&[
            ("SAME", "1"),
            ("OLD", "new"),
            ("NEW", "n"),
            ("RA_JIT_HOME", "/h"),
            ("CARGO_TARGET_DIR", "/t"),
        ]);
        let vars = run_env(&daemon, &asked_with, &asking, ["WATCHED", "SAME"].into_iter());
        let expected: FxHashMap<String, Option<String>> = [
            ("OLD", Some("new")),
            ("NEW", Some("n")),
            ("GONE", None),
            ("WATCHED", None),
            ("SAME", Some("1")),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.map(str::to_owned)))
        .collect();
        assert_eq!(vars, expected);
        // Set and empty are not unset.
        let empty = env(&[("WATCHED", "")]);
        let vars = run_env(&daemon, &FxHashSet::default(), &empty, ["WATCHED"].into_iter());
        assert_eq!(vars.get("WATCHED"), Some(&Some(String::new())));
    }
}
