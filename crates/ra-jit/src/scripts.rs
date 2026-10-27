//! Build scripts compiled, run and judged by blaze in the whole build (`RA_JIT_WHOLE_BUILD=1`;
//! docs/no-rustc.md, step I; docs/reports/SCRIPTEDITS.md, "blaze runs the scripts"). The load
//! of the whole build asks no cargo for them: each script of the build is compiled by blaze
//! with its build dependencies, every crate from source over the standard library's unit
//! (`own_script::link_planned`), run as cargo 1.99 runs it (`blaze_build_script::run`, in its
//! `OUT_DIR` with cargo's environment, `blaze_build_script::env`), and what it printed is read
//! as cargo reads it and given to the crates rust-analyzer loads (`OUT_DIR`, cfgs, variables;
//! the link reads the libraries it named from the output beside `OUT_DIR`).
//!
//! Whether a script is compiled again, and run again, is cargo's rule, ported
//! (`blaze_build_script::fresh`), over blaze's own records beside what it made in
//! `target/ra-jit/build-scripts`: a compile is done again when what it is described with
//! changed, or a file or a variable one of its crates read did (the dep-info of every crate it
//! links, `--inputs`); a run is done again when its fingerprint is not the last run's, its
//! output is gone, the script or a dependency with `links` was made after it, or a file it
//! named changed. A run is never replayed from the store: what a script does beside its
//! `OUT_DIR` cannot be keyed, so a script that is to run runs. Each build of the daemon's
//! judges a script once ([`Asked::build`]): a build is several requests to the worker, and
//! two clients that ask at once are two builds.
//!
//! What a build judged, and the output it judged, last as long as the build ([`leases`]): the
//! build holds a lease on each script's run directory until it is retired, a build that must run
//! the script again waits for the builds that read its output, and only one build at a time is
//! mid-way through the scripts (the admission). A request that has to wait is put off ([`Busy`])
//! and asked again when [`generation`] has moved.

use std::sync::atomic::Ordering::SeqCst;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, atomic::AtomicBool},
    time::SystemTime,
};

use anyhow::Context as _;
use blaze_build_script::{
    fresh::{self, Fingerprint},
    output, run,
    unit::{self, Unit},
};
use blaze_manifest::{
    config::{CompileKind, Config},
    profiles::{Profile, ProfilePackage, Profiles, UnitFor},
};
use ide_db::base_db::TargetKind;
use project_model::{CargoWorkspace, DepKind, Package, ProjectWorkspace, ProjectWorkspaceKind};
use rustc_hash::FxHashMap;
use serde_json::json;

/// Where below the target directory the scripts are compiled and run.
const DIR: &str = "ra-jit/build-scripts";

pub(crate) use leases::{Busy, Stale, anonymous, generation, set_waker, watch};
use leases::{Leases, Refused};

/// What this process's builds judged and hold.
static LEASES: LazyLock<Leases> = LazyLock::new(Leases::default);

/// Ends the build `build` as the daemon says so: the output of the scripts it held is free to
/// be written again, and what it judged is forgotten.
pub(crate) fn retire(build: u64) {
    barrier::hold("retire", build);
    let released = LEASES.retire(build);
    barrier::event("retired", build, &format!("released={released}"));
}

/// Ends the consumer `build`, whose end is not the daemon's to say: a request that is over.
pub(crate) fn release(build: u64) {
    LEASES.retire(build);
}

/// An operation of a build: the judging of the scripts of all the checkouts it reads, which has
/// the admission for as long as it lasts, however many times [`run`] is called, so that no build
/// holds one group and waits for another. It ends when it is dropped; one that is put off and is
/// asked again (`wait`) keeps the admission.
pub(crate) struct Operation {
    build: u64,
    waiting: bool,
}

impl Operation {
    /// The operation is put off for what another build holds, and asked again.
    pub(crate) fn wait(&mut self) {
        self.waiting = true;
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        LEASES.end_operation(self.build, self.waiting);
    }
}

/// Begins an operation of `build`.
pub(crate) fn operation(build: u64) -> Operation {
    LEASES.begin_operation(build);
    Operation { build, waiting: false }
}

/// The scripts a database was loaded from: the run directory of each, with the generation of
/// the run it was loaded from ([`generation_of`]).
pub(crate) type World = Vec<(PathBuf, u64)>;

/// What a run directory's generation is written to: beside `out/`, so not in `OUT_DIR`, which
/// stays as cargo has it.
const GENERATION: &str = "blaze-generation";

/// The generation of the output in `run_dir`: which run left it. It is not the fingerprint: that
/// says what a run was made from (a watched file by its path, not its contents), and two runs
/// of one fingerprint (a watched file edited, a script that runs every build, one that is not
/// deterministic) leave different output. A run writes it as it begins, so that a run that
/// fails, which leaves what it likes in `OUT_DIR`, is another generation too. `None` when
/// there is none.
pub(crate) fn generation_of(run_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(run_dir.join(GENERATION)).ok()?.trim().parse().ok()
}

/// A generation that no other run has had, written to `run_dir` (replaced whole, so that a
/// reader sees one or the other).
fn new_generation(run_dir: &Path) -> Result<u64, String> {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| it.as_nanos());
    let count = COUNT.fetch_add(1, SeqCst);
    let text = format!("{}:{nanos}:{count}:{}", std::process::id(), run_dir.display());
    let generation = fresh::hash_bytes(text.as_bytes());
    let (part, file) = (run_dir.join(format!("{GENERATION}.{count}")), run_dir.join(GENERATION));
    std::fs::create_dir_all(run_dir)
        .and_then(|()| std::fs::write(&part, generation.to_string()))
        .and_then(|()| std::fs::rename(&part, &file))
        .map_err(|it| format!("{}: {it}", file.display()))?;
    Ok(generation)
}

/// The generation of the output in `run_dir`, made if a run of an earlier blaze left none (the
/// caller has the admission and a lease, so no run is writing).
fn ensure_generation(run_dir: &Path) -> Result<u64, String> {
    match generation_of(run_dir) {
        Some(it) => Ok(it),
        None => new_generation(run_dir),
    }
}

/// One consumer of `build` reading the output of a world, until it is dropped: a compile or a
/// link, with the errors and the link that is detached from the request.
pub(crate) struct Pin {
    build: u64,
}

impl Drop for Pin {
    fn drop(&mut self) {
        LEASES.unpin(self.build);
    }
}

/// Has `build` read `world`: its shared leases are taken before anything is read, and a world
/// that moved since the database was loaded from it is [`Stale`] and held for nothing.
pub(crate) fn pin(build: u64, world: &[(PathBuf, u64)]) -> Result<Pin, Stale> {
    LEASES.pin(build, world)?;
    Ok(Pin { build })
}

/// The number of the load of a process that no daemon's build started, made once.
static LOAD_BUILD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A number for the load of this process, that owns the output of its scripts until
/// [`release_load`]: a process run by hand has no build of the daemon's to hold it.
pub(crate) fn load_build() -> u64 {
    let ours = anonymous();
    match LOAD_BUILD.compare_exchange(0, ours, SeqCst, SeqCst) {
        Ok(_) => ours,
        Err(earlier) => earlier,
    }
}

/// Ends the load of this process, if it is the one [`load_build`] numbered: its first program
/// is compiled and linked.
pub(crate) fn release_load() {
    let load = LOAD_BUILD.swap(0, SeqCst);
    if load != 0 {
        release(load);
    }
}

/// Whether the caller can be told [`Busy`] and ask again: the worker's main loop. Elsewhere a
/// wait is made here.
static DEFERRABLE: AtomicBool = AtomicBool::new(false);

/// Has the process say that its callers put a request off when it is [`Busy`].
pub(crate) fn set_deferrable(deferrable: bool) {
    DEFERRABLE.store(deferrable, std::sync::atomic::Ordering::SeqCst);
}

/// What the runs of a build are asked for with.
pub(crate) struct Asked<'a> {
    /// The variables of the process that asked for the build, every one of them.
    pub(crate) env: &'a BTreeMap<String, String>,
    /// The build of the daemon's this is for, or `0` when none is said: a script is judged
    /// once in each build, and in every request of none.
    pub(crate) build: u64,
    /// The profile asked for: `dev`, `release` or another's name.
    pub(crate) profile: &'a str,
    /// `--config` arguments.
    pub(crate) config: &'a [String],
    /// The triple of the machine the scripts run on, and of the one the build is for.
    pub(crate) host: &'a str,
    pub(crate) target: &'a str,
    /// The packages the build has and whether they are built for the target, rather than
    /// only for the host; `None` for every package, treated as a target package.
    pub(crate) packages: Option<&'a FxHashMap<String, bool>>,
}

/// Whether this process runs the build scripts of what it loads: a whole build's worker.
static OWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Has this process run the build scripts of what it loads, or not.
pub(crate) fn set_own(own: bool) {
    OWN.store(own, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this process runs the build scripts of what it loads.
pub(crate) fn own() -> bool {
    OWN.load(std::sync::atomic::Ordering::Relaxed)
}

/// The variables of the process whose build started this worker, which its load runs the
/// scripts with (`daemon::LOAD_ENV`); this process's own when none is said.
pub(crate) fn load_env() -> BTreeMap<String, String> {
    let told = std::env::var_os(crate::cargo::daemon::LOAD_ENV);
    match told.and_then(|it| std::fs::read_to_string(it).ok()) {
        Some(line) => crate::process::decode(line.trim_end_matches('\n'))
            .iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        None => std::env::vars().collect(),
    }
}

/// What a whole build's load asked its scripts for, owned, for the builds after it to ask
/// again with their own variables and build.
#[derive(Clone)]
pub(crate) struct Own {
    /// The variables of the process the load was for.
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) profile: String,
    pub(crate) config: Vec<String>,
    pub(crate) host: String,
    pub(crate) target: String,
    pub(crate) packages: FxHashMap<String, bool>,
    /// The scripts the workspace was loaded from, with the token of each run: set when it is.
    pub(crate) world: World,
}

impl Own {
    /// The ask of a build `build` by a process with the variables `env`.
    pub(crate) fn asked<'a>(&'a self, env: &'a BTreeMap<String, String>, build: u64) -> Asked<'a> {
        Asked {
            env,
            build,
            profile: &self.profile,
            config: &self.config,
            host: &self.host,
            target: &self.target,
            packages: Some(&self.packages),
        }
    }
}

/// Compiles and runs the build scripts of `workspace` that are to run, as `asked`: what they
/// all said, read as `cargo check` says it. `Err` is cargo's message of a script that failed,
/// or what blaze could not do, or a [`Busy`]: another build reads the output of a script this
/// one is to run, or runs scripts itself. A caller that can ([`set_deferrable`]) puts the
/// request off and asks again when [`generation`] has moved; any other waits here, for another
/// worker's build, and is told of one of its own.
pub(crate) fn run(
    workspace: &ProjectWorkspace,
    asked: &Asked<'_>,
) -> anyhow::Result<(project_model::WorkspaceBuildScripts, World)> {
    run_with(&LEASES, workspace, asked)
}

fn run_with(
    leases: &Leases,
    workspace: &ProjectWorkspace,
    asked: &Asked<'_>,
) -> anyhow::Result<(project_model::WorkspaceBuildScripts, World)> {
    let deferrable = DEFERRABLE.load(std::sync::atomic::Ordering::SeqCst);
    retrying(asked.build, deferrable, || attempt(leases, workspace, asked))
}

/// The result of `once`, asked again for as long as it waits for another process, unless the
/// caller `defers` that. The admission a build has is kept across the wait, and let go by the
/// attempt that ends in anything else ([`attempt`]).
fn retrying<T>(
    build: u64,
    defers: bool,
    mut once: impl FnMut() -> Result<T, Attempt>,
) -> anyhow::Result<T> {
    loop {
        match once() {
            Ok(done) => return Ok(done),
            Err(Attempt::Failed(error)) => return Err(error),
            Err(Attempt::Busy(busy)) => match busy {
                Busy::Elsewhere { ref path } if !defers => {
                    barrier::event("waits", 0, &format!("path={}", path.display()));
                    leases::wait(path, None).map_err(anyhow::Error::msg)?;
                }
                busy => {
                    if let Busy::Elsewhere { path } = &busy {
                        watch(path, build);
                    }
                    return Err(anyhow::Error::new(busy));
                }
            },
        }
    }
}

/// Why one pass over the scripts did not end.
#[derive(Debug)]
enum Attempt {
    Failed(anyhow::Error),
    /// Another build holds what the pass needs.
    Busy(Busy),
}

impl Attempt {
    /// Why a script did not make: a build holds what it needs, or `why`.
    fn of(busy: Option<Busy>, why: String) -> Attempt {
        match busy {
            Some(busy) => Attempt::Busy(busy),
            None => Attempt::Failed(anyhow::Error::msg(why)),
        }
    }
}

impl From<anyhow::Error> for Attempt {
    fn from(error: anyhow::Error) -> Attempt {
        Attempt::Failed(error)
    }
}

/// One pass over the scripts of `workspace`, for the build `asked.build`. The pass that ends in
/// anything but a [`Busy`] lets go of the admission the build had; one that waits keeps it, so
/// that a build that waits for readers is the one that is mid-way through the scripts.
fn attempt(
    leases: &Leases,
    workspace: &ProjectWorkspace,
    asked: &Asked<'_>,
) -> Result<(project_model::WorkspaceBuildScripts, World), Attempt> {
    let said = pass(leases, workspace, asked);
    if matches!(said, Err(Attempt::Busy(_))) {
        // A build that waits holds nothing that is not read: what it holds, a build that has
        // the admission and waits for it, or one that waits for what it holds, would wait for.
        leases.yield_world(asked.build);
    } else if !leases.in_operation(asked.build) {
        // Else the operation has it, for all the groups it judges.
        leases.release_admission(asked.build);
    }
    said
}

fn pass(
    leases: &Leases,
    workspace: &ProjectWorkspace,
    asked: &Asked<'_>,
) -> Result<(project_model::WorkspaceBuildScripts, World), Attempt> {
    let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind else {
        return Ok((project_model::WorkspaceBuildScripts::default(), World::new()));
    };
    let target: &Path = cargo.target_directory().as_ref();
    // A request that names no build judges and runs inside one critical section, as one that
    // names one does inside its admission: workers for different feature sets share a script's
    // compile directory and OUT_DIR, and one must not run another's half-written executable.
    let _call = match asked.build {
        0 => Some(lock::acquire(&target.join(DIR)).context("locking the build-script directory")?),
        _ => None,
    };
    let sysroot: PathBuf = workspace.sysroot.root().map(|it| PathBuf::from(it.as_str())).context(
        "blaze runs build scripts with the standard library's source, and has no toolchain",
    )?;
    let mut runner = Runner::new(leases, cargo, asked, sysroot, &workspace.rustc_cfg)?;
    let mut lines = Vec::new();
    for package in cargo.packages() {
        if !runner.runs(package) {
            continue;
        }
        let for_host = asked
            .packages
            .and_then(|it| it.get(&cargo[package].id.repr))
            .is_some_and(|for_target| !for_target);
        let done = match runner.script(package, for_host) {
            Ok(done) => done,
            Err(why) => return Err(Attempt::of(runner.busy.take(), why)),
        };
        let output = &done.output;
        lines.push(
            json!({
                "reason": "build-script-executed",
                "package_id": cargo[package].id.repr,
                "linked_libs": output.library_links,
                "linked_paths": output.library_paths.iter().map(|it| it.path().display().to_string()).collect::<Vec<_>>(),
                "cfgs": output.cfgs,
                "env": output.env,
                "out_dir": done.out_dir,
            })
            .to_string(),
        );
    }
    let said =
        project_model::WorkspaceBuildScripts::of_messages(cargo, lines.iter().map(String::as_str));
    Ok((said, runner.judged.clone()))
}

/// A script that ran, or whose last run stands.
#[derive(Debug)]
struct Done {
    out_dir: PathBuf,
    output: blaze_build_script::Output,
    /// The hash of the run's fingerprint, and when its output was written: what the runs of
    /// the packages that depend on it with `links` depend on.
    fingerprint: u64,
    /// Which run left the output ([`generation_of`]).
    generation: u64,
    made: Option<SystemTime>,
    links: Option<String>,
}

struct Runner<'a> {
    leases: &'a Leases,
    /// Why the script that was to run did not: the output it would write is read by a build.
    busy: Option<Busy>,
    cargo: &'a CargoWorkspace,
    asked: &'a Asked<'a>,
    sysroot: PathBuf,
    profiles: Profiles,
    /// The flags for rustc of the target's units and of the host's.
    flags: Vec<String>,
    host_flags: Vec<String>,
    /// `[env]`, as it applies.
    config_env: BTreeMap<String, String>,
    target_cfgs: Vec<String>,
    host_cfgs: Option<Vec<String>>,
    /// What `rustc -vV` says, which every fingerprint has.
    rustc: String,
    base: PathBuf,
    done: FxHashMap<(Package, bool), Result<Arc<Done>, String>>,
    /// The run directories of the scripts this pass judged.
    judged: World,
    /// The scripts being made, which a dependency cycle reaches again.
    making: Vec<(Package, bool)>,
}

impl<'a> Runner<'a> {
    fn new(
        leases: &'a Leases,
        cargo: &'a CargoWorkspace,
        asked: &'a Asked<'a>,
        sysroot: PathBuf,
        rustc_cfg: &[cfg::CfgAtom],
    ) -> anyhow::Result<Runner<'a>> {
        let root: &Path = cargo.workspace_root().as_ref();
        let env: Vec<(String, String)> =
            asked.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut config = Config::load(root, env, &[])?;
        config.start("build", asked.config)?;
        let ctx = blaze_manifest::Context::new(config);
        let manifest =
            blaze_manifest::metadata::root_manifest(Some(&root.join("Cargo.toml")), &ctx)?;
        let ws = blaze_manifest::Workspace::new(&manifest, &ctx)?;
        let profiles = Profiles::new(
            ws.profiles(),
            ws.unstable_features(),
            &ctx.config,
            asked.profile,
            asked.host,
        )?;
        let config_env = ctx
            .config
            .env_config()?
            .into_iter()
            .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
            .collect();
        let flags = rustflags(&ctx.config, asked.target, rustc_cfg)?;
        // Without `--target` the scripts are compiled with the target's flags; with it, with
        // none (cargo's `target-applies-to-host`).
        let host_flags = match asked.target == asked.host {
            true => flags.clone(),
            false => Vec::new(),
        };
        let target: &Path = cargo.target_directory().as_ref();
        // A version string is shared by different builds of blaze. A compiler change must
        // invalidate the scripts it compiled even when their source files did not change.
        let build = option_env!("RA_JIT_SOURCES")
            .map(str::to_owned)
            .unwrap_or_else(crate::cargo::daemon::version);
        let rustc = compiler_identity(&sysroot, &build);
        let target_cfgs: Vec<String> = rustc_cfg
            .iter()
            .map(|it| match it {
                cfg::CfgAtom::Flag(name) => name.to_string(),
                cfg::CfgAtom::KeyValue { key, value } => format!("{key}={:?}", value.as_str()),
            })
            .collect();
        let host_cfgs = match asked.target == asked.host {
            true => Some(target_cfgs.clone()),
            false => crate::answers::target_cfg(&sysroot, asked.host, asked.env),
        };
        Ok(Runner {
            leases,
            busy: None,
            cargo,
            asked,
            sysroot,
            base: target.join(DIR),
            // One a line, as `rustc --print cfg` prints them.
            target_cfgs,
            host_cfgs,
            flags,
            host_flags,
            config_env,
            profiles,
            rustc,
            done: FxHashMap::default(),
            judged: Vec::new(),
            making: Vec::new(),
        })
    }

    /// Whether `package` has a script the build runs.
    fn runs(&self, package: Package) -> bool {
        let has = self.cargo[package]
            .targets
            .iter()
            .any(|&it| self.cargo[it].kind == TargetKind::BuildScript);
        has && self.asked.packages.is_none_or(|it| it.contains_key(&self.cargo[package].id.repr))
    }

    /// The variable `name` as the build has it: `[env]`'s where that applies, else the
    /// process's.
    fn value(&self, name: &str) -> Option<String> {
        self.config_env.get(name).or_else(|| self.asked.env.get(name)).cloned()
    }

    fn profile(&self, package: Package, unit_for: UnitFor, kind: &CompileKind) -> Profile {
        let data = &self.cargo[package];
        let named = Named::of(data);
        self.profiles.get_profile(&named, data.is_member, data.is_local, unit_for, kind)
    }

    /// The script of `package`, made and run where cargo would, or as it was.
    fn script(&mut self, package: Package, for_host: bool) -> Result<Arc<Done>, String> {
        let key = (package, for_host);
        if let Some(done) = self.done.get(&key) {
            return done.clone();
        }
        if self.making.contains(&key) {
            return Err(format!(
                "the build scripts of `{}` depend on each other",
                self.cargo[package].name
            ));
        }
        self.making.push(key);
        let done = self.make(package, for_host);
        self.making.pop();
        self.done.insert(key, done.clone());
        done
    }

    fn make(&mut self, package: Package, for_host: bool) -> Result<Arc<Done>, String> {
        let cargo = self.cargo;
        let data = &cargo[package];
        let descr = format!("{} v{} ({})", data.name, data.version, data.manifest.parent());
        let unit_for = if for_host { UnitFor::new_host(false) } else { UnitFor::new_normal() };
        let lib = self.profile(package, unit_for, &self.kind(for_host));
        if let Some(judged) = self.begin(package, for_host, &lib)? {
            return Ok(judged);
        }
        let (exe, compiled) = self.compile(package).map_err(|why| {
            format!("blaze could not compile the build script of `{descr}`: {why}")
        })?;
        // The dependencies whose metadata the script is given, which run before it.
        let mut dependencies = Vec::new();
        let mut deps = Vec::new();
        for dep in data.dependencies.iter().filter(|it| it.kind == DepKind::Normal) {
            let dep_data = &cargo[dep.pkg];
            if !self.runs(dep.pkg) {
                continue;
            }
            let ran = self.script(dep.pkg, for_host)?;
            dependencies.push(unit::Dependency {
                name: dep_data.name.clone(),
                links: ran.links.clone(),
                metadata: ran.output.metadata.clone(),
            });
            if ran.links.is_some() {
                deps.push((format!("run of {}", dep_data.name), ran.fingerprint, ran.made));
            }
        }
        let unit = self.unit(package, for_host, &lib, exe, dependencies)?;
        self.judge(package, unit, compiled, &deps)
    }

    /// Where a build's judgment of the script of `package` starts: what the build judged already,
    /// which is its own whatever other builds did since (it failed, and fails each request the
    /// same way, or it stands, and the lease the build took then has held what the script wrote),
    /// and needs no admission to be said; else the admission, which compiling, judging and running
    /// need, one build at a time.
    fn begin(
        &mut self,
        package: Package,
        for_host: bool,
        lib: &Profile,
    ) -> Result<Option<Arc<Done>>, String> {
        let probe = self.unit(package, for_host, lib, PathBuf::new(), Vec::new())?;
        let run_dir = blaze_build_script::run::run_dir(&probe).to_owned();
        let build = self.asked.build;
        match self.leases.decision(build, &run_dir) {
            Some(leases::Judged::Standing(done)) => {
                self.judged.push((run_dir, done.generation));
                return Ok(Some(done));
            }
            // The build gave its lease up to wait: its result is the build's again if the output
            // is still the run it was of, and if another build ran the script meanwhile it is not
            // (that build's output is not this one's) and the script is judged again.
            Some(leases::Judged::Released(done)) => {
                let again = self.leases.reacquire(build, &run_dir, done.generation);
                if again.map_err(|it| self.refused(it))? {
                    self.judged.push((run_dir, done.generation));
                    return Ok(Some(done));
                }
            }
            Some(leases::Judged::Failed(failed)) => return Err(failed),
            None => {}
        }
        self.leases.admit(build, &self.base).map_err(|it| self.refused(it))?;
        Ok(None)
    }

    /// The error of a lease that was not given: a [`Busy`] is kept for [`attempt`] to say.
    fn refused(&mut self, refused: Refused) -> String {
        match refused {
            Refused::Busy(busy) => {
                let why = busy.to_string();
                self.busy = Some(busy);
                why
            }
            Refused::Failed(why) => why,
        }
    }

    /// Whether the script `unit` describes runs for this build, runs it if so, and holds its
    /// output for the build: cargo's decision, over the records blaze keeps, once for each
    /// build. `compiled` is the hash and time of the executable, `deps` the runs of the
    /// dependencies with `links` that it depends on.
    fn judge(
        &mut self,
        package: Package,
        unit: Unit,
        compiled: (u64, SystemTime),
        deps: &[(String, u64, Option<SystemTime>)],
    ) -> Result<Arc<Done>, String> {
        let cargo = self.cargo;
        let data = &cargo[package];
        let build = self.asked.build;
        let run_dir = blaze_build_script::run::run_dir(&unit).to_owned();
        let output_file = run_dir.join("output");
        let record = run_dir.join("blaze-run.json");
        let package_dir = PathBuf::from(data.manifest.parent().as_str());
        // What the last run said, which the fingerprint of this one reads.
        let said = std::fs::read(&output_file).ok().and_then(|it| output::reads(&it).ok());
        let now = Fingerprint {
            rustc: self.rustc.clone(),
            rustflags: unit.rustflags.clone(),
            deps: std::iter::once(("build-script-build".to_owned(), compiled.0))
                .chain(deps.iter().map(|(name, hash, _)| (name.clone(), *hash)))
                .collect(),
            local: fresh::local(said.as_ref(), &package_dir, &|name| self.value(name), &|| {
                self.package_fingerprint(package)
            }),
        };
        let was: Option<Fingerprint> =
            std::fs::read(&record).ok().and_then(|it| serde_json::from_slice(&it).ok());
        let made: Vec<(String, Option<SystemTime>)> =
            std::iter::once(("build-script-build".to_owned(), Some(compiled.1)))
                .chain(deps.iter().map(|(name, _, made)| (name.clone(), *made)))
                .collect();
        let stale = fresh::stale(was.as_ref(), &now, &output_file, &made, &package_dir);
        let cargo_env: Vec<(OsString, OsString)> =
            self.asked.env.iter().map(|(k, v)| (k.into(), v.into())).collect();
        let cx = output::Context::of(&unit, &cargo_env);
        let mut writer = None;
        let mut generation = 0;
        let printed = match stale {
            None => {
                self.leases.hold(build, &run_dir).map_err(|it| self.refused(it))?;
                barrier::event("holds", build, &format!("fresh run_dir={}", run_dir.display()));
                generation = ensure_generation(&run_dir)?;
                let printed = std::fs::read(&output_file)
                    .map_err(|it| format!("{}: {it}", output_file.display()))?;
                output::parse(&printed, &cx).map_err(|it| it.0)?
            }
            Some(why) => {
                // No build reads the output this run is to replace.
                let held = self.leases.writer(build, &run_dir).map_err(|it| self.refused(it))?;
                eprintln!("build script of {}: runs, {why}", data.name);
                barrier::event("runs", build, &format!("run_dir={}", run_dir.display()));
                // As cargo empties its record before a run: a run that does not end is not one.
                _ = std::fs::remove_file(&record);
                // This run's, whether it ends well or not: it leaves what it likes in `OUT_DIR`.
                generation = new_generation(&run_dir)?;
                let ran = blaze_build_script::env(&unit, &cargo_env)
                    .and_then(|env| run::run(&unit, run::Invocation::of(&unit, env).command()))
                    .map_err(|it| it.0)
                    .and_then(|ran| {
                        ran.finish(&unit, &cx).map_err(|failed| {
                            let logged =
                                failed.log_messages.iter().map(|(_, it)| format!("{it}\n"));
                            format!("{}{}", logged.collect::<String>(), failed.error.0)
                        })
                    });
                let printed = match ran {
                    Ok(printed) => printed,
                    Err(failed) => {
                        held.failed(failed.clone());
                        return Err(failed);
                    }
                };
                let reads = output::Reads {
                    files: printed.rerun_if_changed.clone(),
                    env: printed.rerun_if_env_changed.clone(),
                };
                let after = fresh::after_run(&now, &reads, &package_dir, &|name| self.value(name));
                let json = serde_json::to_vec(&after).map_err(|it| it.to_string())?;
                std::fs::write(&record, json)
                    .map_err(|it| format!("{}: {it}", record.display()))?;
                writer = Some(held);
                printed
            }
        };
        let fingerprint: Fingerprint = std::fs::read(&record)
            .ok()
            .and_then(|it| serde_json::from_slice(&it).ok())
            .unwrap_or(now);
        let done = Arc::new(Done {
            out_dir: unit.out_dir.clone(),
            output: printed,
            fingerprint: fingerprint.hash(),
            generation,
            made: std::fs::metadata(&output_file).and_then(|it| it.modified()).ok(),
            links: data.links.clone(),
        });
        // The result is the build's from here on, and the lease keeps the output it is of.
        match writer {
            Some(held) => {
                held.stands(done.clone()).map_err(|it| self.refused(it))?;
                barrier::event("holds", build, &format!("ran run_dir={}", run_dir.display()));
            }
            None => self.leases.record(build, &run_dir, Ok(done.clone())),
        }
        self.judged.push((run_dir, done.generation));
        Ok(done)
    }

    /// The kind of the build's own units: the host's, or the target's.
    fn kind(&self, for_host: bool) -> CompileKind {
        match for_host || self.asked.target == self.asked.host {
            true => CompileKind::Host,
            false => CompileKind::Target(self.asked.target.to_owned()),
        }
    }

    /// The package's files as cargo fingerprints them for a script that names nothing it
    /// reads: the newest of what its `PathSource` lists, or its version when it is not a
    /// path package.
    fn package_fingerprint(&self, package: Package) -> String {
        let data = &self.cargo[package];
        if !data.is_local {
            return data.version.to_string();
        }
        let dir = PathBuf::from(data.manifest.parent().as_str());
        let (include, exclude) = include_exclude(&dir);
        match blaze_build_script::files::list_files(&dir, &include, &exclude) {
            Ok(files) => {
                fresh::package_fingerprint(&dir, blaze_build_script::files::last_modified(&files))
            }
            // cargo fails the build; a fingerprint that is never the same runs the script.
            Err(err) => format!("unlisted: {err} {:?}", SystemTime::now()),
        }
    }

    /// The description of `package`'s run, as cargo runs it for the profile `lib`.
    fn unit(
        &self,
        package: Package,
        for_host: bool,
        lib: &Profile,
        script: PathBuf,
        dependencies: Vec<unit::Dependency>,
    ) -> Result<Unit, String> {
        let data = &self.cargo[package];
        let mut features = data.active_features.clone();
        features.sort();
        let dir_name = self.profiles.get_dir_name().to_owned();
        let target_dir: &Path = self.cargo.target_directory().as_ref();
        let (target, flags, cfgs) = if for_host {
            let cfgs = self.host_cfgs.as_ref().ok_or_else(|| {
                format!(
                    "blaze has no built-in cfg answer for build-script host `{}`",
                    self.asked.host
                )
            })?;
            (self.asked.host, &self.host_flags, cfgs)
        } else {
            (self.asked.target, &self.flags, &self.target_cfgs)
        };
        let mut artifacts = target_dir.to_owned();
        let mut runs = self.base.clone();
        if target != self.asked.host {
            artifacts.push(target);
            runs.push(target);
        }
        artifacts.push(&dir_name);
        let key = json!([data.id.repr, features, format!("{lib:?}"), flags, target, self.rustc]);
        let hash = fresh::hash_bytes(key.to_string().as_bytes());
        let out_dir = runs
            .join(&dir_name)
            .join("build")
            .join(format!("{}-{hash:016x}", data.name))
            .join("out");
        let tool = |name: &str, default: &str| {
            PathBuf::from(self.asked.env.get(name).map_or(default, String::as_str))
        };
        let package_dir = PathBuf::from(data.manifest.parent().as_str());
        Ok(Unit {
            package: unit::Package {
                name: data.name.clone(),
                version: data.version.to_string(),
                descr: Some(match data.is_local {
                    true => format!("{} v{} ({})", data.name, data.version, package_dir.display()),
                    false => format!("{} v{}", data.name, data.version),
                }),
                manifest_dir: package_dir,
                manifest_path: AsRef::<Path>::as_ref(&data.manifest).to_owned(),
                authors: data.authors.clone(),
                description: data.description.clone(),
                homepage: data.homepage.clone(),
                repository: data.repository.clone(),
                license: data.license.clone(),
                license_file: data.license_file.as_ref().map(ToString::to_string),
                readme: data.readme.as_ref().map(ToString::to_string),
                rust_version: data.rust_version.as_ref().map(ToString::to_string),
                links: data.links.clone(),
                targets: data.targets.iter().map(|&it| target_of(&self.cargo[it])).collect(),
            },
            features,
            profile: unit::Profile {
                name: self.asked.profile.to_owned(),
                release: lib.root == blaze_manifest::profiles::ProfileRoot::Release,
                opt_level: lib.opt_level.to_string(),
                debuginfo: lib.debuginfo.is_turned_on(),
                debug_assertions: lib.debug_assertions,
            },
            target: target.to_owned(),
            host: self.asked.host.to_owned(),
            target_cfgs: cfgs.clone(),
            rustflags: flags.clone(),
            jobs: std::thread::available_parallelism().map_or(1, |it| it.get() as u32),
            script,
            out_dir,
            tools: unit::Tools {
                cargo: tool("CARGO", "cargo"),
                rustc: tool("RUSTC", "rustc"),
                rustdoc: tool("RUSTDOC", "rustdoc"),
                rustc_wrapper: self
                    .asked
                    .env
                    .get("RUSTC_WRAPPER")
                    .filter(|it| !it.is_empty())
                    .map(PathBuf::from),
                rustc_workspace_wrapper: self
                    .asked
                    .env
                    .get("RUSTC_WORKSPACE_WRAPPER")
                    .filter(|it| !it.is_empty())
                    .map(PathBuf::from),
                linker: None,
            },
            workspace_member: data.is_member,
            dependencies,
            config_env: self
                .config_env
                .iter()
                .map(|(name, value)| unit::ConfigEnv {
                    name: name.clone(),
                    value: value.clone(),
                    force: true,
                })
                .collect(),
            dylib_search: unit::DylibSearch {
                host_root: Some(target_dir.join(&dir_name)),
                host_deps: vec![target_dir.join(&dir_name).join("deps")],
                sysroot_libdir: self.sysroot.join("lib/rustlib").join(self.asked.host).join("lib"),
                plugin_search: Vec::new(),
            },
            jobserver: None,
        })
    }

    /// The script of `package`, compiled by blaze where it is not as it was: its executable,
    /// and the hash of what it was compiled from and when, which its run depends on.
    fn compile(&mut self, package: Package) -> Result<(PathBuf, (u64, SystemTime)), String> {
        let mut crates = Vec::new();
        let mut at: FxHashMap<Package, usize> = FxHashMap::default();
        let cargo = self.cargo;
        let data = &cargo[package];
        let script =
            data.targets.iter().map(|&it| &cargo[it]).find(|it| it.kind == TargetKind::BuildScript);
        let script = script.ok_or("the package has no build script")?;
        let host = self.profile(package, UnitFor::new_host(false), &CompileKind::Host);
        let root = PathBuf::from(script.root.as_str());
        crates.push(self.described(package, "build_script_build", &root, &host, None)?);
        let build_deps: Vec<_> = data
            .dependencies
            .iter()
            .filter(|it| it.kind == DepKind::Build)
            .map(|it| (it.name.clone(), it.pkg))
            .collect();
        let mut deps = Vec::new();
        for (name, dep) in build_deps {
            deps.push((name.replace('-', "_"), self.add(dep, &mut crates, &mut at)?));
        }
        crates[0].deps = deps;
        let key = json!([data.id.repr, format!("{host:?}"), self.host_flags, self.rustc]);
        let hash = fresh::hash_bytes(key.to_string().as_bytes());
        let dir = self
            .base
            .join(self.profiles.get_dir_name())
            .join("build")
            .join(format!("{}-{hash:016x}", data.name));
        let described = crate::cargo::own_script::planned_description(&self.sysroot, crates);
        let fingerprint = fresh::hash_bytes(
            json!([described, self.host_flags, self.rustc]).to_string().as_bytes(),
        );
        let exe = dir.join("build-script-build");
        let inputs = dir.join("build-script-build.inputs");
        let record = dir.join("blaze-compile.json");
        if let Some(made) = self.compiled(&exe, &inputs, &record, fingerprint) {
            return Ok((exe, (fingerprint, made)));
        }
        _ = std::fs::remove_file(&record);
        std::fs::create_dir_all(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
        let began = SystemTime::now();
        let mut env: Vec<(String, String)> =
            self.asked.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        env.extend(self.config_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        crate::cargo::own_script::link_planned(&described, &dir, &exe, &inputs, &env)?;
        if let Some(line) = std::fs::read_to_string(dir.join("ra-jit.log"))
            .unwrap_or_default()
            .lines()
            .find(|it| it.contains(" functions in it ra-jit could not compile"))
            && !line.ends_with(", 0 functions in it ra-jit could not compile")
        {
            return Err(format!("it calls functions blaze could not compile ({line})"));
        }
        // What was read is dated when the compile began, as cargo dates its dep-info: a file
        // written while it compiled is a change for the next build.
        std::fs::File::options()
            .write(true)
            .open(&inputs)
            .and_then(|it| it.set_modified(began))
            .map_err(|it| format!("{}: {it}", inputs.display()))?;
        let read =
            crate::dep_info::DepInfo::parse(&std::fs::read_to_string(&inputs).unwrap_or_default());
        let env: Vec<(String, Option<String>)> =
            read.env.iter().filter(|(name, _)| !set_for_crates(name)).cloned().collect();
        let record_json = json!({ "fingerprint": fingerprint, "env": env });
        std::fs::write(&record, record_json.to_string())
            .map_err(|it| format!("{}: {it}", record.display()))?;
        let made =
            std::fs::metadata(&exe).and_then(|it| it.modified()).map_err(|it| it.to_string())?;
        Ok((exe, (fingerprint, made)))
    }

    /// When the script at `exe` was made, when it stands: compiled from `fingerprint`, with
    /// no file it read written after the compile began and no variable it read other now.
    fn compiled(
        &self,
        exe: &Path,
        inputs: &Path,
        record: &Path,
        fingerprint: u64,
    ) -> Option<SystemTime> {
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(record).ok()?).ok()?;
        if record["fingerprint"].as_u64() != Some(fingerprint) {
            return None;
        }
        let made = std::fs::metadata(exe).and_then(|it| it.modified()).ok()?;
        let began = std::fs::metadata(inputs).and_then(|it| it.modified()).ok()?;
        let read = crate::dep_info::DepInfo::parse(&std::fs::read_to_string(inputs).ok()?);
        for file in &read.files {
            let written = std::fs::metadata(file).and_then(|it| it.modified()).ok()?;
            if written > began {
                return None;
            }
        }
        let env: Vec<(String, Option<String>)> =
            serde_json::from_value(record["env"].clone()).ok()?;
        if env.iter().any(|(name, was)| self.compiled_with(name) != *was) {
            return None;
        }
        Some(made)
    }

    /// The variable `name` as a compile of a script reads it: blaze's compile is not given
    /// what cargo and rustc set for themselves (`own_script::link_planned`).
    fn compiled_with(&self, name: &str) -> Option<String> {
        let theirs = (name.starts_with("CARGO_") && name != "CARGO_HOME")
            || name.starts_with("RUSTC_")
            || name.starts_with("RA_JIT_");
        if theirs { None } else { self.value(name) }
    }

    /// Adds the library of `package` and what it links to `crates`, as a build script's
    /// dependencies are compiled: for the host.
    fn add(
        &mut self,
        package: Package,
        crates: &mut Vec<crate::cargo::own_script::Planned>,
        at: &mut FxHashMap<Package, usize>,
    ) -> Result<usize, String> {
        if let Some(&it) = at.get(&package) {
            return Ok(it);
        }
        let cargo = self.cargo;
        let data = &cargo[package];
        let lib = data
            .targets
            .iter()
            .map(|&it| &cargo[it])
            .find(|it| matches!(it.kind, TargetKind::Lib { .. }));
        let lib =
            lib.ok_or_else(|| format!("`{}`, which it is built with, has no library", data.name))?;
        if matches!(lib.kind, TargetKind::Lib { is_proc_macro: true }) {
            return Err(format!(
                "it is built with the proc macro `{}`, which blaze does not compile for a build script yet",
                data.name
            ));
        }
        let name = lib.name.replace('-', "_");
        let root = PathBuf::from(lib.root.as_str());
        let host = self.profile(package, UnitFor::new_host(false), &CompileKind::Host);
        // A library with a script of its own is compiled with what that script said.
        let ran = match self.runs(package) {
            true => Some(self.script(package, true)?),
            false => None,
        };
        let described = self.described(package, &name, &root, &host, ran.as_deref())?;
        let index = crates.len();
        crates.push(described);
        at.insert(package, index);
        let normal: Vec<_> = cargo[package]
            .dependencies
            .iter()
            .filter(|it| it.kind == DepKind::Normal)
            .map(|it| (it.name.clone(), it.pkg))
            .collect();
        let mut deps = Vec::new();
        for (name, dep) in normal {
            deps.push((name.replace('-', "_"), self.add(dep, crates, at)?));
        }
        crates[index].deps = deps;
        Ok(index)
    }

    /// A crate of `package` rooted at `root`, as cargo has rustc compile it with the profile
    /// `profile`, and with what its script said when it has one.
    fn described(
        &self,
        package: Package,
        name: &str,
        root: &Path,
        profile: &Profile,
        ran: Option<&Done>,
    ) -> Result<crate::cargo::own_script::Planned, String> {
        let data = &self.cargo[package];
        let mut cfg: Vec<String> =
            data.active_features.iter().map(|it| format!("feature={it:?}")).collect();
        cfg.sort();
        if profile.debug_assertions {
            cfg.push("debug_assertions".to_owned());
        }
        let package_dir: &Path = data.manifest.parent().as_ref();
        let manifest: &Path = data.manifest.as_ref();
        let version = &data.version;
        let mut env = BTreeMap::new();
        let mut set = |name: &str, value: String| {
            env.insert(name.to_owned(), value);
        };
        set("CARGO", self.asked.env.get("CARGO").cloned().unwrap_or_else(|| "cargo".to_owned()));
        set("CARGO_MANIFEST_DIR", package_dir.display().to_string());
        set("CARGO_MANIFEST_PATH", manifest.display().to_string());
        set("CARGO_PKG_VERSION", version.to_string());
        set("CARGO_PKG_VERSION_MAJOR", version.major.to_string());
        set("CARGO_PKG_VERSION_MINOR", version.minor.to_string());
        set("CARGO_PKG_VERSION_PATCH", version.patch.to_string());
        set("CARGO_PKG_VERSION_PRE", version.pre.to_string());
        set("CARGO_PKG_AUTHORS", data.authors.join(":"));
        set("CARGO_PKG_NAME", data.name.clone());
        set("CARGO_PKG_DESCRIPTION", data.description.clone().unwrap_or_default());
        set("CARGO_PKG_HOMEPAGE", data.homepage.clone().unwrap_or_default());
        set("CARGO_PKG_REPOSITORY", data.repository.clone().unwrap_or_default());
        set("CARGO_PKG_LICENSE", data.license.clone().unwrap_or_default());
        set(
            "CARGO_PKG_LICENSE_FILE",
            data.license_file.as_ref().map(ToString::to_string).unwrap_or_default(),
        );
        set(
            "CARGO_PKG_RUST_VERSION",
            data.rust_version.as_ref().map(ToString::to_string).unwrap_or_default(),
        );
        set("CARGO_PKG_README", data.readme.as_ref().map(ToString::to_string).unwrap_or_default());
        set("CARGO_CRATE_NAME", name.to_owned());
        if let Some(ran) = ran {
            set("OUT_DIR", ran.out_dir.display().to_string());
            for (key, value) in &ran.output.env {
                set(key, value.clone());
            }
            cfg.extend(ran.output.cfgs.iter().cloned());
        }
        Ok(crate::cargo::own_script::Planned {
            name: name.to_owned(),
            root: root.to_owned(),
            edition: data.edition.to_string(),
            cfg,
            env,
            deps: Vec::new(),
        })
    }
}

/// The variables cargo sets for rustc, which a description sets for each crate and its
/// fingerprint has.
fn set_for_crates(name: &str) -> bool {
    crate::cargo::own_script::read_by_crates(name)
}

/// The compiler and installed toolchain that made a script, including an in-place upgrade.
fn compiler_identity(sysroot: &Path, build: &str) -> String {
    let mut toolchain = crate::installed_identity(&format!("{}/", sysroot.display()));
    toolchain.sort();
    json!([build, sysroot, toolchain]).to_string()
}

/// A target as `cargo metadata` lists it.
fn target_of(target: &project_model::TargetData) -> unit::Target {
    let (kind, crate_type) = match target.kind {
        TargetKind::Lib { is_proc_macro: true } => ("proc-macro", "proc-macro"),
        TargetKind::Lib { is_proc_macro: false } => ("lib", "lib"),
        TargetKind::Bin => ("bin", "bin"),
        TargetKind::Example => ("example", "bin"),
        TargetKind::Test => ("test", "bin"),
        TargetKind::Bench => ("bench", "bin"),
        TargetKind::BuildScript => ("custom-build", "bin"),
        TargetKind::Other => ("lib", "lib"),
    };
    unit::Target {
        name: target.name.clone(),
        kind: vec![kind.to_owned()],
        crate_types: vec![crate_type.to_owned()],
    }
}

/// `package.include` and `package.exclude` of the manifest in `dir`, inherited from the
/// workspace's `[workspace.package]` where it says so.
fn include_exclude(dir: &Path) -> (Vec<String>, Vec<String>) {
    let read = |path: &Path| -> Option<toml::Table> {
        toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
    };
    let Some(manifest) = read(&dir.join("Cargo.toml")) else { return Default::default() };
    let package = manifest.get("package").and_then(|it| it.as_table());
    let list = |key: &str| -> Vec<String> {
        let Some(value) = package.and_then(|it| it.get(key)) else { return Vec::new() };
        let value = match value.get("workspace").and_then(|it| it.as_bool()) {
            Some(true) => {
                let root = if manifest.contains_key("workspace") {
                    Some(manifest.clone())
                } else if let Some(workspace) =
                    package.and_then(|it| it.get("workspace")).and_then(|it| it.as_str())
                {
                    read(&dir.join(workspace).join("Cargo.toml"))
                } else {
                    dir.ancestors().skip(1).find_map(|it| {
                        let root = read(&it.join("Cargo.toml"))?;
                        root.get("workspace").is_some().then_some(root)
                    })
                };
                let inherited = root
                    .as_ref()
                    .and_then(|it| it.get("workspace")?.get("package")?.get(key).cloned());
                match inherited {
                    Some(it) => it,
                    None => return Vec::new(),
                }
            }
            _ => value.clone(),
        };
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|it| it.as_str().map(str::to_owned))
            .collect()
    };
    (list("include"), list("exclude"))
}

/// The flags for rustc cargo gives the units of `target`, as `env` and the configuration say
/// (cargo's `extra_args`): `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, `target.<triple>.rustflags`
/// and the `target.'cfg(..)'` tables, then `build.rustflags`.
pub(crate) fn rustflags(
    config: &Config,
    target: &str,
    cfg: &[cfg::CfgAtom],
) -> anyhow::Result<Vec<String>> {
    let cfg: Vec<cargo_platform::Cfg> = cfg
        .iter()
        .map(|atom| match atom {
            cfg::CfgAtom::Flag(name) => name.to_string(),
            cfg::CfgAtom::KeyValue { key, value } => format!("{key}={:?}", value.as_str()),
        })
        .map(|it| it.parse())
        .collect::<Result<_, _>>()?;
    let kind = CompileKind::Target(target.to_owned());
    config.extra_args(
        std::slice::from_ref(&kind),
        target,
        Some(&cfg),
        &kind,
        blaze_manifest::config::Flags::Rust,
    )
}

/// A package as a profile's `[profile.*.package.<spec>]` is matched against it.
pub(crate) struct Named<'a> {
    name: &'a str,
    version: &'a cargo_metadata::semver::Version,
    source: blaze_manifest::source::SourceId,
}

/// `data` as a profile's package overrides are matched against it.
pub(crate) fn named(data: &project_model::PackageData) -> Named<'_> {
    Named::of(data)
}

impl<'a> Named<'a> {
    fn of(data: &'a project_model::PackageData) -> Named<'a> {
        let dir: &Path = data.manifest.parent().as_ref();
        let source = match data.is_local {
            true => blaze_manifest::source::SourceId::for_path(dir).ok(),
            false => data.id.repr.split_once('+').and_then(|(_, rest)| {
                let url = rest.split('#').next()?.parse().ok()?;
                Some(blaze_manifest::source::SourceId::for_registry(&url))
            }),
        };
        let source = source.unwrap_or_else(|| {
            blaze_manifest::source::SourceId::for_path(Path::new("/")).expect("the root is a path")
        });
        Named { name: &data.name, version: &data.version, source }
    }
}

impl std::fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} v{}", self.name, self.version)
    }
}

impl ProfilePackage for Named<'_> {
    fn name(&self) -> &str {
        self.name
    }
    fn version(&self) -> &cargo_metadata::semver::Version {
        self.version
    }
    fn source_id(&self) -> &blaze_manifest::source::SourceId {
        &self.source
    }
}

#[cfg(test)]
mod tests;

pub(crate) mod barrier;
mod leases;
mod lock;
