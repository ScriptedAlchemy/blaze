//! Build scripts of the workspace. Cargo runs one again when what it said it reads has
//! changed, or when anything in its package has if it said nothing, and then compiles the
//! package again whatever the script printed. The daemon runs the script the same way, in the
//! same directory, records the run as cargo records its own, and looks at what it printed and
//! wrote: when that is what the build had, the crates compiled with it are still right, and the
//! build stays the daemon's.
//!
//! To run a script as cargo does, the wrapper puts a few lines of shell where rustc linked
//! it. Cargo runs those, and they run `ra-jit build-script`, which writes down the
//! environment and the directory cargo ran it in and becomes the script.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::records;

mod continuation;
pub(crate) use continuation::{Completed, Continuation};

/// The variable that names the script itself to `ra-jit build-script`.
const REAL: &str = "RA_JIT_BUILD_SCRIPT";

/// The file next to a script's `OUT_DIR` that says how cargo ran it.
const RAN: &str = "ra-jit-ran.json";

/// How cargo ran a build script.
#[derive(Serialize, Deserialize)]
struct Ran {
    real: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
}

/// Has cargo run the script rustc linked at `linked` through `ra-jit build-script`.
#[cfg(unix)]
pub(crate) fn stand_in_front(linked: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let real = real_of(linked);
    std::fs::rename(linked, &real)?;
    let exe = std::env::current_exe()?;
    let quoted = |path: &Path| -> anyhow::Result<String> {
        let path = path.to_str().context("a path that is not text")?;
        Ok(shlex::try_quote(path)?.into_owned())
    };
    let lines = format!(
        "#!/bin/sh\n{REAL}={} exec {} build-script \"$@\"\n",
        quoted(&real)?,
        quoted(&exe)?
    );
    std::fs::write(linked, lines)?;
    Ok(std::fs::set_permissions(linked, std::fs::Permissions::from_mode(0o755))?)
}

/// Has cargo run the script rustc linked at `linked` through `ra-jit build-script`. There is
/// no shell to say so in a file cargo runs, so this executable is linked there under the
/// script's name, and [`stood_in_front_of`] is how it knows. Where it can't be linked, on
/// another volume, cargo runs the script itself and a change it reruns for is cargo's to
/// build.
#[cfg(windows)]
pub(crate) fn stand_in_front(linked: &Path) -> anyhow::Result<()> {
    let real = real_of(linked);
    std::fs::rename(linked, &real)?;
    if std::fs::hard_link(std::env::current_exe()?, linked).is_err() {
        std::fs::rename(&real, linked)?;
    }
    Ok(())
}

/// The script this executable stands in front of, when it runs under the name of one:
/// the name rustc linked it under, or the one cargo gives it in the same directory.
#[cfg(windows)]
pub(crate) fn stood_in_front_of() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = exe.file_stem()?.to_str()?;
    if !name.starts_with("build_script_") && !name.starts_with("build-script-") {
        return None;
    }
    let beside = std::fs::read_dir(exe.parent()?).ok()?.flatten().map(|it| it.path());
    beside.into_iter().find(|it| it.to_str().is_some_and(|it| it.ends_with(".exe.real")))
}

fn real_of(linked: &Path) -> PathBuf {
    let mut real = linked.as_os_str().to_owned();
    real.push(".real");
    PathBuf::from(real)
}

/// `ra-jit build-script`: writes down how cargo runs the script and becomes it.
pub(crate) fn command(args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let real = PathBuf::from(std::env::var_os(REAL).context("no build script is named")?);
    run_as_cargo_does(real, args)
}

/// Writes down how cargo runs the script at `real` and becomes it.
pub(crate) fn run_as_cargo_does(
    real: PathBuf,
    args: impl Iterator<Item = OsString>,
) -> anyhow::Result<()> {
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter(|(name, _)| name != REAL)
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let out_dir =
        env.iter().find(|(name, _)| name == "OUT_DIR").map(|(_, value)| PathBuf::from(value));
    let args: Vec<OsString> = args.collect();
    let cwd = std::env::current_dir()?;
    if let Some(out_dir) = &out_dir {
        let ran = Ran { real: real.clone(), cwd: cwd.clone(), env: env.clone() };
        if continuation::consume(&ran, out_dir)? {
            return Ok(());
        }
    }
    if let Some(run_dir) = out_dir.as_deref().and_then(Path::parent) {
        let ran = Ran { real: real.clone(), cwd: cwd.clone(), env };
        // Without it the script is not run again by the daemon, and cargo builds.
        let _ = std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran)?);
    }
    // A run is not replayed from the store: what a script reads besides the files it names,
    // and what it does beside its `OUT_DIR` (a file elsewhere, an extended attribute), are
    // not in any key a run is kept under, so a run that is to happen happens.
    let mut script = Command::new(&real);
    script.args(&args).env_remove(REAL);
    become_it(&mut script)
}

#[cfg(unix)]
pub(crate) fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
pub(crate) fn make_executable(_: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    Err(std::os::unix::process::CommandExt::exec(script).into())
}

#[cfg(not(unix))]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    std::process::exit(script.status()?.code().unwrap_or(1))
}

/// The file cargo kept what the script that wrote `out_dir` printed in, beside it: `output`
/// in cargo's first layout of its build directory, `run/stdout` in the second
/// (`build/<package>/<hash>/out`, the default of nightly cargo since 2026).
pub(crate) fn printed(out_dir: &Path) -> Option<PathBuf> {
    let run_dir = out_dir.parent()?;
    let second = run_dir.join("run").join("stdout");
    Some(if second.exists() { second } else { run_dir.join("output") })
}

/// Whether `path` can be in a script's `OUT_DIR`: below `build/<unit>/out` in cargo's first
/// layout or `build/<package>/<hash>/out` in its second, where the objects of a library are
/// too.
pub(crate) fn in_out_dir(path: &Path) -> bool {
    let names: Vec<&str> = path.components().filter_map(|c| c.as_os_str().to_str()).collect();
    names.windows(3).any(|it| it[0] == "build" && it[2] == "out")
        || names.windows(4).any(|it| it[0] == "build" && it[3] == "out")
}

/// Whether a script can be run again as cargo ran it into `out_dir`.
pub(crate) fn can_rerun(out_dir: &Path) -> bool {
    out_dir.parent().is_some_and(|it| it.join(RAN).exists())
}

/// The directory cargo ran the script that wrote `out_dir` in, which is its package's, when
/// it was written down.
pub(crate) fn ran_in(out_dir: &Path) -> Option<PathBuf> {
    let ran = std::fs::read(out_dir.parent()?.join(RAN)).ok()?;
    serde_json::from_slice::<Ran>(&ran).ok().map(|it| it.cwd)
}

/// Whether the script that wrote `out_dir` is as cargo would compile it now: no file rustc
/// read for it (`.d` beside it, named from `workspace`, where cargo runs rustc) is newer than
/// it, and it read no variable while compiled but cargo's own, as cargo set them to run it:
/// cargo compiles it again when one changes.
pub(crate) fn compiled_from_what_is_there(out_dir: &Path, workspace: &Path) -> bool {
    let Some(run_dir) = out_dir.parent() else { return false };
    let Ok(ran) = std::fs::read(run_dir.join(RAN)) else { return false };
    let Ok(ran) = serde_json::from_slice::<Ran>(&ran) else { return false };
    let Some(linked) = ran.real.to_str().and_then(|it| it.strip_suffix(".real")) else {
        return false;
    };
    let compiled = std::fs::metadata(&ran.real).and_then(|it| it.modified());
    let Ok(read) = std::fs::read_to_string(Path::new(linked).with_extension("d")) else {
        return false;
    };
    let Ok(compiled) = compiled else { return false };
    let read = crate::dep_info::DepInfo::parse(&read);
    // Cargo sets its own from the manifest, which a change of has the checkout loaded again;
    // whoever runs cargo sets the others.
    let set = |name: &str| ran.env.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
    if read
        .env
        .iter()
        .any(|(name, value)| !name.starts_with("CARGO_") || set(name) != value.as_deref())
    {
        return false;
    }
    !read.files.is_empty()
        && read.files.iter().all(|it| {
            let written = std::fs::metadata(workspace.join(it)).and_then(|it| it.modified());
            written.is_ok_and(|it| it <= compiled)
        })
}

/// What a run of a build script left for what is compiled with it: the lines of what it printed
/// that cargo reads ([`directives`]) and what it wrote ([`tree_digest`] of its `OUT_DIR`). A build
/// keeps the run its compilers read, and a run after it is compared with that one, not with what
/// the directory has by then: a run since, cargo's or ra-jit's, may have written something else.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Run {
    directives: Vec<String>,
    wrote: Vec<u8>,
}

impl Run {
    /// The last run of the script that wrote `out_dir`: what cargo kept of what it printed (a run
    /// ra-jit made there is recorded as cargo's, [`current`]), and what the directory has now.
    /// The output and directory are read under one lock, only after a completed run: neither
    /// an active cargo nor the partial output of an interrupted run can become the baseline.
    pub(crate) fn of(out_dir: &Path) -> Result<Run, String> {
        let unit = records::Unit::of(out_dir)?;
        let _held = unit.hold()?;
        unit.recorded()?;
        let printed = printed(out_dir).and_then(|it| std::fs::read(it).ok()).ok_or_else(|| {
            format!("{}: no record of what the script printed", out_dir.display())
        })?;
        Run::left(&printed, out_dir)
    }

    fn left(printed: &[u8], out_dir: &Path) -> Result<Run, String> {
        let wrote = tree_digest(out_dir).map_err(|it| format!("{}: {it}", out_dir.display()))?;
        Ok(Run { directives: directives(printed).into_iter().map(str::to_owned).collect(), wrote })
    }

    /// It, of a run in `dir`, as a run in `other` is: the lines it printed with the name of the
    /// one directory replaced by the other's, as cargo does when it moves a build directory.
    pub(crate) fn named(self, dir: &Path, other: &Path) -> Result<Run, String> {
        if dir == other {
            return Ok(self);
        }
        let (Some(names), Some(named)) = (dir.to_str(), other.to_str()) else {
            return Err("has a directory whose name is not text".to_owned());
        };
        let directives = self.directives.iter().map(|it| it.replace(names, named)).collect();
        Ok(Run { directives, ..self })
    }

    /// `Ok` when it printed and wrote what `was` did; `Err` is which it did not.
    pub(crate) fn same_as(&self, was: &Run) -> Result<(), String> {
        if self.directives != was.directives {
            return Err("printed something else".to_owned());
        }
        if self.wrote != was.wrote {
            return Err("wrote something else".to_owned());
        }
        Ok(())
    }
}

/// What the cargo that builds next has that a run of a build script is reckoned from, besides
/// its directories.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Dependency {
    pub(crate) package: String,
    pub(crate) links: String,
    pub(crate) out_dir: PathBuf,
    pub(crate) parents: Vec<PathBuf>,
    pub(crate) targets: Vec<blaze_build_script::unit::Target>,
}

pub(crate) struct Cargo<'a> {
    /// A variable as that cargo has it: from the `[env]` of its configuration, or else its
    /// environment. Cargo's record of a run keeps the value of each variable the script said it
    /// reads, and runs the script again when one is another.
    pub(crate) env: &'a dyn Fn(&str) -> Option<String>,
    /// All variable names in that request, including ones the last script run did not have.
    pub(crate) env_names: &'a [String],
    /// The targets of the script's package, as `cargo metadata` lists them: cargo takes a
    /// `rustc-link-arg-bins` line and its like only from the script of a package that has one.
    pub(crate) targets: &'a [blaze_build_script::unit::Target],
    /// Run-unit edges for native dependencies. Their output time matters even when the
    /// metadata and fingerprint are identical, as in cargo's `check_filesystem`.
    pub(crate) dependencies: &'a [Dependency],
}

/// The run of the script that wrote `out_dir` that cargo's next build has, by cargo's own records
/// of its last run ([`records`]): that run, when cargo would not run the script again, and else a
/// run made now.
///
/// A run made now is cargo's next, and is recorded as cargo's: it is made in `out_dir` itself, with
/// current user variables and cargo's recorded script variables and directory, under its locks,
/// directory, and cargo's hash of the run is emptied before it and written after it with what the
/// run printed, as cargo does (`build_work`, `write_fingerprint`). Cargo's next build finds the
/// script run, as after a run of its own, and does not run it again, whether that build is the one
/// a difference sends to cargo or one after the daemon's: the script runs once for a change, as
/// under cargo. No copy of the directory is run in: a script can read more of a directory than a
/// copy keeps, and one that finds a copy does other than it would in cargo's
/// (docs/reports/BUILDSCRIPT.md, "Round 3" and "Round 4").
///
/// It is `Err`, the build cargo's, and nothing is run: when cargo's records are not ones cargo
/// 1.99 wrote, or of a run that did not end; when a watched variable has another value; when a
/// cargo holds the lock, so its run cannot be read consistently; and when there is no
/// lock to take. An executed failure is the result of this build, not permission to retry
/// it through cargo. Its empty fingerprint permits the next requested build to retry.
pub(crate) fn current(out_dir: &Path, cargo: &Cargo<'_>) -> Result<Run, String> {
    judge(out_dir, cargo).map_err(|it| it.to_string())
}

/// Failure to inspect a run is different from a run that failed this build.
#[derive(Debug)]
pub(crate) enum JudgementError {
    Unavailable(String),
    Failed(String),
}

impl From<String> for JudgementError {
    fn from(value: String) -> Self {
        Self::Unavailable(value)
    }
}

impl From<&str> for JudgementError {
    fn from(value: &str) -> Self {
        Self::Unavailable(value.to_owned())
    }
}

impl std::fmt::Display for JudgementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Unavailable(what) | Self::Failed(what)) = self;
        f.write_str(what)
    }
}

pub(crate) fn judge(out_dir: &Path, cargo: &Cargo<'_>) -> Result<Run, JudgementError> {
    judge_for(out_dir, cargo, &mut Vec::new())
}

pub(crate) fn judge_for(
    out_dir: &Path,
    cargo: &Cargo<'_>,
    completed: &mut Vec<Completed>,
) -> Result<Run, JudgementError> {
    // Keep every run-unit input stable through the parent's judgment. Dependencies often
    // share the parent's directory, so acquire each directory's Cargo locks only once.
    let mut pending = vec![out_dir.to_owned()];
    let mut seen = std::collections::BTreeSet::new();
    let mut units = std::collections::BTreeMap::new();
    while let Some(out_dir) = pending.pop() {
        if !seen.insert(out_dir.clone()) {
            continue;
        }
        let unit = records::Unit::of(&out_dir)?;
        let dir = std::fs::canonicalize(unit.lock_dir()).map_err(|it| it.to_string())?;
        units.entry(dir).or_insert(unit);
        pending.extend(
            cargo
                .dependencies
                .iter()
                .filter(|it| it.parents.contains(&out_dir))
                .map(|it| it.out_dir.clone()),
        );
    }
    let _held = units.values().map(records::Unit::hold).collect::<Result<Vec<_>, _>>()?;
    judge_locked(out_dir, cargo, completed, &mut Vec::new())
}

fn judge_locked(
    out_dir: &Path,
    cargo: &Cargo<'_>,
    completed: &mut Vec<Completed>,
    parents: &mut Vec<PathBuf>,
) -> Result<Run, JudgementError> {
    if parents.iter().any(|it| it == out_dir) {
        return Err("a cycle in cargo's run-unit dependencies".into());
    }
    parents.push(out_dir.to_owned());
    let result = judge_locked_inner(out_dir, cargo, completed, parents);
    parents.pop();
    result
}

fn judge_locked_inner(
    out_dir: &Path,
    cargo: &Cargo<'_>,
    completed: &mut Vec<Completed>,
    parents: &mut Vec<PathBuf>,
) -> Result<Run, JudgementError> {
    let mut dependencies = Vec::new();
    let mut metadata = std::collections::BTreeMap::new();
    // The dependencies cargo runs this script after. A package declared as a dependency may be
    // none of them (optional and off, or for another target), which cargo's record of the run
    // tells.
    let recorded = records::Unit::of(out_dir).ok().and_then(|it| it.recorded().ok());
    let edges: Vec<&Dependency> = cargo
        .dependencies
        .iter()
        .filter(|it| it.parents.iter().any(|it| it == out_dir))
        .filter(|it| recorded.as_ref().is_none_or(|run| run.has_dependency(&it.package)))
        .collect();
    for dep in &edges {
        let context = Cargo { targets: &dep.targets, ..*cargo };
        judge_locked(&dep.out_dir, &context, completed, parents)?;
        let unit = records::Unit::of(&dep.out_dir)?;
        let fingerprint = unit.recorded()?;
        let printed = std::fs::read(unit.output()).map_err(|it| it.to_string())?;
        let ran: Ran = std::fs::read(dep.out_dir.parent().ok_or("no run directory")?.join(RAN))
            .map_err(|it| it.to_string())
            .and_then(|it| serde_json::from_slice(&it).map_err(|it| it.to_string()))?;
        let said = said_to_cargo(&printed, &dep.out_dir, &dep.out_dir, &ran, &context)
            .map_err(|it| it.to_string())?;
        for (key, value) in said.metadata {
            metadata.insert(
                format!(
                    "DEP_{}_{}",
                    blaze_build_script::envify(&dep.links),
                    blaze_build_script::envify(&key)
                ),
                value,
            );
        }
        dependencies.push((dep.package.as_str(), fingerprint.hash_u64(), unit.output()));
    }
    // A fresh fingerprint does not make an unlocked read safe: cargo can invalidate it and
    // start replacing OUT_DIR between that read and the tree digest. Hold the lock before
    // reading the invocation too, which cargo can replace with another environment.
    let run_dir = out_dir.parent().ok_or("a build script without a directory")?;
    let ran: Ran = std::fs::read(run_dir.join(RAN))
        .map_err(|it| format!("{RAN}: {it}"))
        .and_then(|it| serde_json::from_slice(&it).map_err(|it| it.to_string()))?;
    let var = |name: &str| ran.env.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
    // The directory as cargo names it, which its records have.
    let cargos = var("OUT_DIR")
        .map(Path::new)
        .filter(|it| *it == out_dir || same_file::is_same_file(it, out_dir).unwrap_or(false));
    let out_dir = cargos.ok_or("ran in another directory than cargo's")?;
    let package = var("CARGO_MANIFEST_DIR").map(Path::new).ok_or("ran without a package")?;
    let unit = records::Unit::of(out_dir)?;
    let recorded = unit.recorded()?;
    let fingerprint = recorded.with_dependencies(&dependencies)?;
    let output = unit.output();
    let printed = std::fs::read(&output).map_err(|it| format!("{}: {it}", output.display()))?;
    let said = |printed: &[u8], generated: &Path| {
        said_to_cargo(printed, generated, out_dir, &ran, cargo)
            .map_err(|it| format!("printed what cargo does not take: {it}"))
    };
    // Cargo reads the last run with the paths it printed in the directory it ran in.
    let generated = std::fs::read(unit.output().with_file_name("root-output")).ok();
    let generated = generated.and_then(|it| String::from_utf8(it).ok()).map(PathBuf::from);
    let last = said(&printed, generated.as_deref().unwrap_or(out_dir))?;
    let named = if last.rerun_if_changed.is_empty() {
        PathBuf::new()
    } else {
        unit.output_named(&recorded)?
    };
    let reckoned = |said: &blaze_build_script::Output, named: &Path| {
        let (files, variables) = (&said.rerun_if_changed, &said.rerun_if_env_changed);
        records::local(files, variables, named, package, cargo.env)
    };
    let now = reckoned(&last, &named).unwrap_or_else(|| match &recorded.local[..] {
        [records::Local::Precalculated(value)] if !value.ends_with(')') => recorded.local.clone(),
        _ => vec![records::Local::Precalculated(crate::scripts::path_package_fingerprint(package))],
    });
    let local_changed = now != recorded.local;
    if local_changed && !matches!(&now[..], [records::Local::Precalculated(_)]) {
        return Err(changed(&now, &recorded.local).into());
    }
    let compiled = ran.real.to_str().and_then(|it| it.strip_suffix(".real")).map(PathBuf::from);
    let compiled = compiled.unwrap_or_else(|| ran.real.clone());
    let home = (cargo.env)("CARGO_HOME")
        .map(|it| ran.cwd.join(it))
        .or_else(|| (cargo.env)("HOME").map(|it| Path::new(&it).join(".cargo")));
    if !local_changed
        && fingerprint == recorded
        && unit.stale_dependencies(&dependencies).is_none()
        && unit.stale(&now, package, &compiled, home).is_none()
    {
        return Ok(Run::left(&printed, out_dir)?);
    }
    let began = unit.begin()?;
    let mut env: std::collections::BTreeMap<String, String> = ran
        .env
        .iter()
        .filter_map(|(name, _)| (cargo.env)(name).map(|value| (name.clone(), value)))
        .collect();
    for name in cargo.env_names {
        if let Some(value) = (cargo.env)(name) {
            env.insert(name.clone(), value);
        }
    }
    for (name, value) in &ran.env {
        if cargo_sets(name) {
            env.insert(name.clone(), value.clone());
        }
    }
    // Cargo builds a script's `DEP_*` from its dependencies' current runs alone, so a key a
    // dependency we judged stopped printing is not there, whatever the last run of this script
    // had. (Those of a dependency we did not judge stay as that run had them.)
    for dep in &edges {
        let prefix = format!("DEP_{}_", blaze_build_script::envify(&dep.links));
        env.retain(|name, _| !name.starts_with(&prefix));
    }
    env.extend(metadata);
    let mut script = Command::new(&ran.real);
    script.env_clear().envs(&env);
    script.current_dir(&ran.cwd).stdin(Stdio::null());
    let again = script.output().map_err(|it| JudgementError::Failed(it.to_string()))?;
    if !again.status.success() {
        return Err(JudgementError::Failed(format!(
            "failed ({})\n{}",
            again.status,
            String::from_utf8_lossy(&again.stderr).trim_end()
        )));
    }
    // Cargo reads each line as the script prints it, and a run that logged an error is a failed
    // one, of which it writes nothing.
    if String::from_utf8_lossy(&again.stdout).lines().any(|it| it.starts_with("cargo::error=")) {
        return Err(JudgementError::Failed(format!(
            "logged an error\n{}",
            String::from_utf8_lossy(&again.stdout).trim_end()
        )));
    }
    unit.ran(began, &again.stdout, &again.stderr)?;
    let said = said(&again.stdout, out_dir).map_err(JudgementError::Failed)?;
    let named = if !said.rerun_if_changed.is_empty() && named.as_os_str().is_empty() {
        unit.output_named(&recorded)?
    } else {
        named
    };
    // A run that says nothing of what it reads leaves what cargo reckoned before it.
    let fingerprint = fingerprint.with_local(reckoned(&said, &named).unwrap_or(now));
    unit.record(&fingerprint)?;
    let run = Run::left(&again.stdout, out_dir)?;
    completed.push(Completed::new(&ran, out_dir, run.clone(), again, fingerprint, began));
    Ok(run)
}

fn cargo_sets(name: &str) -> bool {
    name.starts_with("CARGO_PKG_")
        || name.starts_with("CARGO_FEATURE_")
        || name.starts_with("CARGO_CFG_")
        || name.starts_with("DEP_")
        || matches!(
            name,
            "CARGO"
                | "CARGO_MANIFEST_DIR"
                | "CARGO_MANIFEST_PATH"
                | "CARGO_MANIFEST_LINKS"
                | "CARGO_ENCODED_RUSTFLAGS"
                | "CARGO_MAKEFLAGS"
                | "OUT_DIR"
                | "NUM_JOBS"
                | "TARGET"
                | "DEBUG"
                | "OPT_LEVEL"
                | "PROFILE"
                | "HOST"
                | "RUSTC"
                | "RUSTDOC"
                | "RUSTC_LINKER"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "DYLD_FALLBACK_LIBRARY_PATH"
                | "LD_LIBRARY_PATH"
        )
}

/// Why cargo's record of a run, `recorded`, is not what cargo reckons of it now, `now`.
fn changed(now: &[records::Local], recorded: &[records::Local]) -> String {
    use records::Local::RerunIfEnvChanged as Var;
    let var = now.iter().zip(recorded).find_map(|it| match it {
        (Var { var, val }, Var { var: was, val: then }) if var == was && val != then => Some(var),
        _ => None,
    });
    match var {
        Some(var) => format!("reads the variable {var}, which changed: cargo runs it again"),
        None => {
            "has a record of cargo's that is not of what it printed: cargo runs it again".to_owned()
        }
    }
}

/// What a script printed, `printed`, as cargo reads it (`BuildOutput::parse`), for a package with
/// `ran`'s variables and `cargo`'s targets. `Err` is what cargo fails the build with.
fn said_to_cargo(
    printed: &[u8],
    generated: &Path,
    out_dir: &Path,
    ran: &Ran,
    cargo: &Cargo<'_>,
) -> Result<blaze_build_script::Output, blaze_build_script::Error> {
    let var = |name: &str| ran.env.iter().find(|it| it.0 == name).map(|it| it.1.clone());
    let package = blaze_build_script::unit::Package {
        name: var("CARGO_PKG_NAME").unwrap_or_default(),
        version: var("CARGO_PKG_VERSION").unwrap_or_default(),
        manifest_dir: var("CARGO_MANIFEST_DIR").map(PathBuf::from).unwrap_or_default(),
        rust_version: var("CARGO_PKG_RUST_VERSION").filter(|it| !it.is_empty()),
        targets: cargo.targets.to_vec(),
        ..Default::default()
    };
    // A stable cargo allows nightly features when it is told it is another (`features::channel`).
    let channel = (cargo.env)("__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS").or_else(|| {
        ((cargo.env)("RUSTC_BOOTSTRAP").as_deref() == Some("1")).then(|| "dev".to_owned())
    });
    let context = blaze_build_script::output::Context {
        package: &package,
        out_dir_when_generated: generated,
        out_dir,
        nightly_features_allowed: matches!(channel.as_deref(), Some("nightly" | "dev")),
        rustc_bootstrap: (cargo.env)("RUSTC_BOOTSTRAP"),
    };
    blaze_build_script::parse(printed, &context)
}

/// The lines of what a script printed that cargo reads, as its `BuildOutput::parse` reads them:
/// the lines that are text (a line that is not valid UTF-8 is skipped whole, and never decoded
/// with its bad bytes replaced) and start with `cargo:` or `cargo::` once trimmed. Cargo does
/// nothing with the rest. It keeps them in the file beside the script's `OUT_DIR`, but a script
/// prints them when it runs a tool, such as the build of a frontend, which says how long it
/// took, and a rerun never prints the same.
fn directives(printed: &[u8]) -> Vec<&str> {
    let lines = printed.split(|byte| *byte == b'\n');
    let lines = lines.filter_map(|line| std::str::from_utf8(line).ok()).map(str::trim);
    lines.filter(|line| line.starts_with("cargo:")).collect()
}

/// A hash of the tree below `root` as a compiler that reads it by its paths finds it: the name
/// of each file, directory and link, what it is, the bytes of a file, and of a link its text and
/// what it leads to, followed as `include!` and `std::fs::read` follow it: a file's bytes or a
/// directory's tree. A directory reached again (a link back up, or a second link to one) is
/// named, by its path below `root` or else its whole path, and not read again; a link that
/// leads nowhere, or round in a circle, is its text alone. It is an error when something there
/// cannot be read, or is no file, directory or link (a pipe would be waited for): a tree that was
/// not read is not the same as another. Times, permissions and the rest are not in it: rustc
/// reads none of them.
fn tree_digest(root: &Path) -> std::io::Result<Vec<u8>> {
    let canonical = std::fs::canonicalize(root)?;
    let mut seen = std::collections::HashSet::from([canonical.clone()]);
    let mut hash = Sha256::new();
    let mut pending = vec![(root.to_owned(), PathBuf::new())];
    let other = |path: &Path| {
        let it = format!("{}: neither a file, a directory nor a link", path.display());
        std::io::Error::other(it)
    };
    while let Some((dir, below)) = pending.pop() {
        let mut entries = std::fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|it| it.file_name());
        for entry in entries {
            let (path, name) = (entry.path(), below.join(entry.file_name()));
            let mut parts: Vec<(u8, Vec<u8>)> = Vec::new();
            let kind = entry.file_type()?;
            let mut leads_to = |path: &Path, link: bool| -> std::io::Result<(u8, Vec<u8>)> {
                let metadata = match std::fs::metadata(path) {
                    Ok(it) => it,
                    Err(_) if link => return Ok((b'n', Vec::new())),
                    Err(it) => return Err(it),
                };
                if metadata.is_file() {
                    return Ok((b'f', file_digest(path)?));
                }
                if !metadata.is_dir() {
                    return Err(other(path));
                }
                let at = std::fs::canonicalize(path)?;
                if seen.insert(at.clone()) || !link {
                    pending.push((path.to_owned(), name.clone()));
                    return Ok((b'd', Vec::new()));
                }
                let at = at.strip_prefix(&canonical).map_or(at.clone(), Path::to_owned);
                Ok((b's', at.as_os_str().as_encoded_bytes().to_vec()))
            };
            if kind.is_symlink() {
                let text = std::fs::read_link(&path)?;
                parts.push((b'l', text.as_os_str().as_encoded_bytes().to_vec()));
                parts.push(leads_to(&path, true)?);
            } else if kind.is_dir() || kind.is_file() {
                parts.push(leads_to(&path, false)?);
            } else {
                return Err(other(&path));
            }
            hash.update((name.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
            hash.update(name.as_os_str().as_encoded_bytes());
            for (tag, what) in parts {
                hash.update([tag]);
                hash.update((what.len() as u64).to_le_bytes());
                hash.update(&what);
            }
        }
    }
    Ok(hash.finalize().to_vec())
}

/// A hash of the bytes of the file `path`, which is not read at once.
fn file_digest(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut hash = Sha256::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut hash)?;
    Ok(hash.finalize().to_vec())
}

/// A hash of the names, as they are below `dir`, and the contents of the files there.
fn digest_of(dir: &Path) -> Vec<u8> {
    let mut hash = Sha256::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(below) = pending.pop() {
        let entries = std::fs::read_dir(&below).into_iter().flatten().flatten();
        let mut paths: Vec<PathBuf> = entries.map(|it| it.path()).collect();
        paths.sort();
        for path in paths {
            let name = path.strip_prefix(dir).unwrap_or(&path);
            hash.update(name.as_os_str().as_encoded_bytes());
            match path.is_dir() {
                true => pending.push(path),
                false => hash.update(std::fs::read(&path).unwrap_or_default()),
            }
        }
    }
    hash.finalize().to_vec()
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    /// A run directory as cargo and `ra-jit build-script` leave one, for a script that
    /// writes `wrote` and prints where.
    fn ran(name: &str, wrote: &str) -> PathBuf {
        let lines = "#!/bin/sh\nprintf %s \"$CARGO_PKG_DESCRIPTION\" > \"$OUT_DIR/made.rs\"\necho \"cargo:in=$OUT_DIR\"\n";
        ran_script(name, lines, wrote)
    }

    /// The variables of the cargo that builds next, for a test: the test's.
    fn cargo_env(name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    /// What the cargo that builds next has, for a test: its variables, and a package with no
    /// targets.
    const CARGO: Cargo<'static> =
        Cargo { env: &cargo_env, env_names: &[], targets: &[], dependencies: &[] };

    /// The lines of shell a test's script is run through: the script, then what it reads.
    const WATCHES: &str =
        "#!/bin/sh\n\"$0.inner\" \"$@\" || exit $?\necho cargo:rerun-if-changed=watched\n";

    /// A run of the script `lines`, given `wrote` as `CARGO_PKG_DESCRIPTION`, as cargo and `ra-jit
    /// build-script` leave one: in cargo's build directory, `target/debug/build/<name>-0123`,
    /// which is the package's directory too and the one the script ran in, with cargo's records
    /// of a run that printed where it ran and that it reads `watched` beside it, made after
    /// `watched` was last written. The script is run through [`WATCHES`].
    fn ran_script(name: &str, lines: &str, wrote: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("ra-jit-script-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dest = root.join("target/debug");
        let run_dir = dest.join(format!("build/{name}-0123"));
        let out_dir = run_dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        for lock in [".cargo-build-lock", ".cargo-lock"] {
            std::fs::write(dest.join(lock), "").unwrap();
        }
        let real = run_dir.join("script");
        // A process a test thread starts while this one has the script open to write holds it
        // open too until that process execs, and the script is refused meanwhile ("text file
        // busy"): another process writes it.
        for (path, lines) in [(run_dir.join("script.inner"), lines), (real.clone(), WATCHES)] {
            let written = Command::new("sh")
                .args(["-c", "printf %s \"$1\" > \"$0\" && chmod 755 \"$0\""])
                .arg(&path)
                .arg(lines)
                .status()
                .unwrap();
            assert!(written.success());
        }
        let text = |path: &Path| path.to_str().unwrap().to_owned();
        let env = vec![
            ("OUT_DIR".to_owned(), text(&out_dir)),
            ("CARGO_MANIFEST_DIR".to_owned(), text(&run_dir)),
            ("CARGO_PKG_DESCRIPTION".to_owned(), wrote.to_owned()),
            ("PATH".to_owned(), std::env::var("PATH").unwrap()),
        ];
        let ran = Ran { real, cwd: run_dir.clone(), env };
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        std::fs::write(run_dir.join("watched"), "").unwrap();
        let printed = format!("cargo:in={}\ncargo:rerun-if-changed=watched\n", out_dir.display());
        recorded(&out_dir, printed.as_bytes());
        out_dir
    }

    /// Has cargo's records be of a run of `out_dir`'s script that printed `printed`, made an
    /// hour ago and after `watched` was last written.
    fn recorded(out_dir: &Path, printed: &[u8]) {
        let run_dir = out_dir.parent().unwrap();
        std::fs::write(run_dir.join("output"), printed).unwrap();
        std::fs::write(run_dir.join("root-output"), out_dir.as_os_str().as_encoded_bytes())
            .unwrap();
        records::tests::recorded_as_cargo_would(out_dir, run_dir, printed);
        made_at(out_dir, 3600, 7200);
    }

    /// Has cargo's last run of `out_dir`'s script be `run` seconds ago, and `watched`, which it
    /// says it reads, be written `watched` seconds ago, and the script compiled before both.
    fn made_at(out_dir: &Path, run: u64, watched: u64) {
        let run_dir = out_dir.parent().unwrap();
        let ago = |seconds| SystemTime::now() - Duration::from_secs(seconds);
        set_written(&run_dir.join("output"), ago(run));
        set_written(&run_dir.join("watched"), ago(watched));
        set_written(&ran_of(out_dir).real, ago(run.max(watched) + 1));
    }

    /// Has what the script of `out_dir` reads be written after cargo's last run of it, which
    /// cargo then runs again.
    fn changed(out_dir: &Path) {
        made_at(out_dir, 3600, 1800);
    }

    /// What the daemon finds of the script of `out_dir` once something it reads has changed: a
    /// run now, in the directory, as cargo makes it, compared with the run cargo built with,
    /// which is what the directory and the record beside it have before it.
    fn rerun_is_same(out_dir: &Path) -> Result<(), String> {
        let built = Run::of(out_dir)?;
        changed(out_dir);
        current(out_dir, &CARGO)?.same_as(&built)
    }

    /// The hash of cargo's fingerprint of the run of `out_dir`'s script.
    fn hash_of(out_dir: &Path) -> PathBuf {
        let run_dir = out_dir.parent().unwrap();
        let dest = run_dir.parent().unwrap().parent().unwrap();
        let dir = dest.join(".fingerprint").join(run_dir.file_name().unwrap());
        dir.join("run-build-script-build-script-build")
    }

    /// Whether cargo would run the script of `out_dir` again, by its records, read as cargo
    /// reads them.
    fn cargo_would_run(out_dir: &Path) -> bool {
        let unit = records::Unit::of(out_dir).unwrap();
        let Ok(recorded) = unit.recorded() else { return true };
        let run_dir = out_dir.parent().unwrap();
        let output = std::fs::read(unit.output()).unwrap();
        let said = said_to_cargo(&output, out_dir, out_dir, &ran_of(out_dir), &CARGO).unwrap();
        let named = unit.output_named(&recorded).unwrap();
        let local = records::local(
            &said.rerun_if_changed,
            &said.rerun_if_env_changed,
            &named,
            run_dir,
            &cargo_env,
        );
        local.as_ref() != Some(&recorded.local)
            || unit.stale(&recorded.local, run_dir, Path::new("/nowhere"), None).is_some()
    }

    fn ran_of(out_dir: &Path) -> Ran {
        serde_json::from_slice(&std::fs::read(out_dir.parent().unwrap().join(RAN)).unwrap())
            .unwrap()
    }

    #[test]
    fn a_run_is_recorded_as_cargo_records_its_own() {
        let out_dir = ran("same", "fn made() {}");
        let run_dir = out_dir.parent().unwrap();
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        assert!(!cargo_would_run(&out_dir));
        changed(&out_dir);
        assert!(cargo_would_run(&out_dir));
        let fingerprint = std::fs::read(hash_of(&out_dir)).unwrap();
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        // Cargo's records are of this run: what it printed, as of when it began, which
        // `invoked.timestamp` says; its stderr and directory; and a fingerprint that says it read
        // what the run said it read, which cargo finds current.
        let output = std::fs::read(run_dir.join("output")).unwrap();
        assert_eq!(
            directives(&output),
            [
                format!("cargo:in={}", out_dir.display()),
                "cargo:rerun-if-changed=watched".to_owned()
            ]
        );
        let written =
            |name: &str| std::fs::metadata(run_dir.join(name)).unwrap().modified().unwrap();
        assert_eq!(written("output"), written("invoked.timestamp"));
        assert!(written("output") > SystemTime::now() - Duration::from_secs(60));
        assert_eq!(std::fs::read(run_dir.join("stderr")).unwrap(), b"");
        assert_eq!(
            std::fs::read(run_dir.join("root-output")).unwrap(),
            out_dir.as_os_str().as_encoded_bytes()
        );
        assert_eq!(std::fs::read(hash_of(&out_dir)).unwrap(), fingerprint, "it read the same");
        assert!(!cargo_would_run(&out_dir));
        assert!(!run_dir.join("ra-jit-checked").exists());
    }

    /// A script that says beside its directory each time it runs.
    const COUNTS: &str =
        "#!/bin/sh\necho run >> \"$OUT_DIR/../runs\"\necho \"cargo:in=$OUT_DIR\"\n";

    /// How many times the script of `out_dir`, [`COUNTS`], ran.
    fn runs(out_dir: &Path) -> usize {
        let runs = std::fs::read_to_string(out_dir.parent().unwrap().join("runs"));
        runs.unwrap_or_default().lines().count()
    }

    /// Has cargo's record of `parent`'s run be of one that came after `native`'s, `name`, as it
    /// is when `parent` depends on a package with that `links`, and returns the edge.
    fn depends_on(parent: &Path, native: &Path, name: &str) -> Dependency {
        use std::hash::{Hash as _, Hasher};

        let unit = records::Unit::of(parent).unwrap();
        let recorded = unit.recorded().unwrap();
        let native_hash = records::Unit::of(native).unwrap().recorded().unwrap().hash_u64();
        let mut hasher = rustc_stable_hash::hashers::StableSipHasher128::new();
        name.hash(&mut hasher);
        let mut json = serde_json::to_value(&recorded).unwrap();
        json["deps"].as_array_mut().unwrap().push(serde_json::json!([
            Hasher::finish(&hasher),
            "build_script_build",
            false,
            native_hash
        ]));
        unit.record(&serde_json::from_value(json).unwrap()).unwrap();
        Dependency {
            package: name.to_owned(),
            links: "probe".to_owned(),
            out_dir: native.to_owned(),
            parents: vec![parent.to_owned()],
            targets: Vec::new(),
        }
    }

    #[test]
    fn a_native_dependency_rerun_invalidates_its_parent_even_with_identical_metadata() {
        let native = ran_script("native-edge", COUNTS, "");
        let parent = ran_script("parent-edge", COUNTS, "");
        let dependencies = [depends_on(&parent, &native, "native-edge")];
        let cargo = Cargo { dependencies: &dependencies, ..CARGO };
        let built = Run::of(&parent).unwrap();
        // Native's first output is older than the parent's. Only native's watched input
        // changes; the parent has no changed file or variable of its own.
        set_written(
            &native.parent().unwrap().join("output"),
            SystemTime::now() - Duration::from_secs(4000),
        );
        changed(&native);
        assert_eq!(current(&parent, &cargo), Ok(built.clone()));
        assert_eq!((runs(&native), runs(&parent)), (1, 1));
        assert_eq!(current(&parent, &cargo), Ok(built.clone()));
        assert_eq!((runs(&native), runs(&parent)), (1, 1));
        changed(&native);
        assert_eq!(current(&parent, &cargo), Ok(built));
        assert_eq!((runs(&native), runs(&parent)), (2, 2));
    }

    #[test]
    fn a_dependency_cargo_gave_no_edge_is_not_run() {
        // A package that declares an optional dependency on a `links` package, left off, has no
        // run-unit edge to it in cargo's record. The daemon neither runs that package's script
        // for it nor fails for the missing edge, which handed every build of it to cargo.
        let native = ran_script("native-off", COUNTS, "");
        let parent = ran_script("parent-off", COUNTS, "");
        let dependencies = [Dependency {
            package: "native-off".to_owned(),
            links: "probe".to_owned(),
            out_dir: native.clone(),
            parents: vec![parent.clone()],
            targets: Vec::new(),
        }];
        let cargo = Cargo { dependencies: &dependencies, ..CARGO };
        let built = Run::of(&parent).unwrap();
        changed(&native);
        assert_eq!(current(&parent, &cargo), Ok(built));
        assert_eq!((runs(&native), runs(&parent)), (0, 0));
    }

    #[test]
    fn a_dependency_that_stops_printing_a_metadata_key_leaves_its_parent_without_it() {
        // Cargo gives a script `DEP_<LINKS>_<KEY>` from the dependency's last run alone. The
        // parent last ran with a key the dependency printed then; the dependency, run again,
        // does not print it, so the parent's run must not see it, or the daemon keeps a program
        // cargo does not build.
        let native_script = "#!/bin/sh\necho \"cargo:in=$OUT_DIR\"\n\
                             [ -f \"$OUT_DIR/../avx\" ] && echo cargo::metadata=features=avx\nexit 0\n";
        let parent_script = "#!/bin/sh\necho \"${DEP_PROBE_FEATURES-unset}\" >> \"$OUT_DIR/../saw\"\n\
                             echo \"cargo:saw=${DEP_PROBE_FEATURES-unset}\"\n";
        let native = ran_script("native-meta", native_script, "");
        let parent = ran_script("parent-meta", parent_script, "");
        let dependencies = [depends_on(&parent, &native, "native-meta")];
        let cargo = Cargo { dependencies: &dependencies, ..CARGO };
        let mut ran = ran_of(&parent);
        ran.env.push(("DEP_PROBE_FEATURES".to_owned(), "avx".to_owned()));
        let json = serde_json::to_vec(&ran).unwrap();
        std::fs::write(parent.parent().unwrap().join(RAN), json).unwrap();
        // Native's watched input changed; its rerun prints no `features`.
        set_written(
            &native.parent().unwrap().join("output"),
            SystemTime::now() - Duration::from_secs(4000),
        );
        changed(&native);
        current(&parent, &cargo).unwrap();
        let saw = std::fs::read_to_string(parent.parent().unwrap().join("saw")).unwrap();
        assert_eq!(saw.trim(), "unset");
    }

    #[test]
    fn the_last_run_since_what_the_script_reads_changed_is_not_made_again() {
        let out_dir = ran_script("current", COUNTS, "");
        let built = Run::of(&out_dir).unwrap();
        // Cargo's run is after the last change: cargo would not run it again, and it is the one.
        assert_eq!(current(&out_dir, &CARGO), Ok(built.clone()));
        assert_eq!(runs(&out_dir), 0);
        // A change after cargo's run: a run now, which is cargo's, and the one until the next
        // change.
        changed(&out_dir);
        assert_eq!(current(&out_dir, &CARGO), Ok(built.clone()));
        assert_eq!(runs(&out_dir), 1);
        assert_eq!(current(&out_dir, &CARGO), Ok(built.clone()));
        assert_eq!(runs(&out_dir), 1);
        assert!(!cargo_would_run(&out_dir));
        changed(&out_dir);
        assert_eq!(current(&out_dir, &CARGO), Ok(built));
        assert_eq!(runs(&out_dir), 2);
    }

    /// The verifier's case of round 3: a script that counts its runs in what it generates. It
    /// runs once for one change, as under cargo, and cargo's next build finds it run.
    #[test]
    fn a_script_that_counts_its_runs_runs_once_for_a_change() {
        let lines = "#!/bin/sh\ncount=$(cat \"$OUT_DIR/count\" 2>/dev/null || echo 0)\ncount=$((count + 1))\necho $count > \"$OUT_DIR/count\"\necho \"pub const RUNS: u32 = $count;\" > \"$OUT_DIR/generated.rs\"\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("counter", lines, "");
        let count = || std::fs::read_to_string(out_dir.join("count")).unwrap();
        std::fs::write(out_dir.join("count"), "1\n").unwrap();
        let built = Run::of(&out_dir).unwrap();
        for edit in 2..4 {
            changed(&out_dir);
            let check = current(&out_dir, &CARGO).and_then(|it| it.same_as(&built));
            assert_eq!(check, Err("wrote something else".to_owned()));
            assert_eq!(count(), format!("{edit}\n"));
            // Cargo, which builds what the run wrote, finds it run, and so does the next check.
            assert!(!cargo_would_run(&out_dir));
            let again = current(&out_dir, &CARGO).and_then(|it| it.same_as(&built));
            assert_eq!(again, Err("wrote something else".to_owned()));
            assert_eq!(count(), format!("{edit}\n"));
        }
    }

    /// Has another process hold `lock` exclusively until the returned one is done with, as a
    /// cargo holds its lock on a build directory. A lock this process held would be held as
    /// well by a process another test starts meanwhile, until it runs what it was started for.
    fn held(lock: &Path) -> std::process::Child {
        let holds = "open(my $f, '<', shift) or die; flock($f, LOCK_EX) or die; $| = 1; print qq(held\n); <STDIN>;";
        let mut cargo = Command::new("perl")
            .args(["-MFcntl=:flock", "-e", holds])
            .arg(lock)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut held = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(cargo.stdout.take().unwrap()),
            &mut held,
        )
        .unwrap();
        assert_eq!(held, "held\n");
        cargo
    }

    #[test]
    fn a_run_while_a_cargo_builds_in_the_directory_is_cargos() {
        let out_dir = ran_script("busy", COUNTS, "");
        let dest = out_dir.parent().unwrap().parent().unwrap().parent().unwrap();
        let mut cargo = held(&dest.join(".cargo-build-lock"));
        // Even a run that looks fresh cannot be read while cargo owns the directory: cargo
        // can replace its records or OUT_DIR immediately after the freshness check.
        let result = current(&out_dir, &CARGO);
        assert!(result.as_ref().is_err_and(|it| it.contains("a cargo is building")), "{result:?}");
        // It is not, and cargo may be running the script now.
        changed(&out_dir);
        let result = current(&out_dir, &CARGO);
        assert!(result.as_ref().is_err_and(|it| it.contains("a cargo is building")), "{result:?}");
        assert_eq!(runs(&out_dir), 0, "the script did not run");
        drop(cargo.stdin.take());
        assert!(cargo.wait().unwrap().success());
        assert_eq!(current(&out_dir, &CARGO), Run::of(&out_dir));
        assert_eq!(runs(&out_dir), 1);
        // An older cargo, which holds `.cargo-lock` alone, is building too.
        let mut cargo = held(&dest.join(".cargo-lock"));
        changed(&out_dir);
        let result = current(&out_dir, &CARGO);
        assert!(result.as_ref().is_err_and(|it| it.contains("a cargo is building")), "{result:?}");
        drop(cargo.stdin.take());
        assert!(cargo.wait().unwrap().success());
        assert_eq!(runs(&out_dir), 1);
    }

    #[test]
    fn a_baseline_is_not_read_while_cargo_owns_its_directory() {
        let out_dir = ran_script("baseline-busy", COUNTS, "");
        let dest = out_dir.parent().unwrap().parent().unwrap().parent().unwrap();
        for name in [".cargo-build-lock", ".cargo-lock"] {
            let mut cargo = held(&dest.join(name));
            let result = Run::of(&out_dir);
            drop(cargo.stdin.take());
            assert!(cargo.wait().unwrap().success());
            assert!(
                result.as_ref().is_err_and(|it| it.contains("a cargo is building")),
                "{name}: {result:?}"
            );
            assert!(Run::of(&out_dir).is_ok());
        }
        assert_eq!(runs(&out_dir), 0);
    }

    #[test]
    fn a_baseline_without_a_cargo_lock_is_not_read() {
        let out_dir = ran_script("baseline-no-lock", COUNTS, "");
        let dest = out_dir.parent().unwrap().parent().unwrap().parent().unwrap();
        std::fs::remove_file(dest.join(".cargo-build-lock")).unwrap();
        let result = Run::of(&out_dir);
        assert!(result.as_ref().is_err_and(|it| it.contains("no lock")), "{result:?}");
    }

    #[test]
    fn a_baseline_from_an_unfinished_run_is_not_read() {
        let out_dir = ran_script("baseline-incomplete", COUNTS, "");
        std::fs::write(hash_of(&out_dir), "").unwrap();
        let result = Run::of(&out_dir);
        assert!(
            result.as_ref().is_err_and(|it| it.contains("no record of a run that ended")),
            "{result:?}"
        );
    }

    #[test]
    fn a_fresh_run_without_a_cargo_lock_is_not_read() {
        let out_dir = ran_script("fresh-no-lock", COUNTS, "");
        let dest = out_dir.parent().unwrap().parent().unwrap().parent().unwrap();
        std::fs::remove_file(dest.join(".cargo-build-lock")).unwrap();
        let result = current(&out_dir, &CARGO);
        assert!(result.as_ref().is_err_and(|it| it.contains("no lock")), "{result:?}");
        assert_eq!(runs(&out_dir), 0, "the script did not run");
    }

    /// The verifier's case of round 3: with `build.build-dir`, cargo 1.99 holds
    /// `.cargo-build-lock` in the build directory while it runs a script there, and its
    /// `.cargo-lock` is in the other directory, the one of what it builds.
    #[test]
    fn a_run_while_a_cargo_builds_in_a_build_directory_set_apart_is_cargos() {
        let out_dir = ran_script("build-dir", COUNTS, "");
        let dest = out_dir.parent().unwrap().parent().unwrap().parent().unwrap();
        std::fs::remove_file(dest.join(".cargo-lock")).unwrap();
        let mut cargo = held(&dest.join(".cargo-build-lock"));
        changed(&out_dir);
        let result = current(&out_dir, &CARGO);
        assert!(result.as_ref().is_err_and(|it| it.contains("a cargo is building")), "{result:?}");
        assert_eq!(runs(&out_dir), 0, "the script did not run");
        drop(cargo.stdin.take());
        assert!(cargo.wait().unwrap().success());
        assert_eq!(current(&out_dir, &CARGO), Run::of(&out_dir));
        assert_eq!(runs(&out_dir), 1);
        // Where cargo keeps no lock, nothing keeps a cargo out, and nothing is run.
        std::fs::remove_file(dest.join(".cargo-build-lock")).unwrap();
        changed(&out_dir);
        let result = current(&out_dir, &CARGO);
        assert!(result.as_ref().is_err_and(|it| it.contains("no lock")), "{result:?}");
        assert_eq!(runs(&out_dir), 1);
    }

    #[test]
    fn a_run_that_differs_is_different_until_cargo_builds_again() {
        let out_dir = ran("differs", "fn made() {}");
        let made = out_dir.join("made.rs");
        std::fs::write(&made, "fn made_before() {}").unwrap();
        let built = Run::of(&out_dir).unwrap();
        let check = |unchanged: bool| {
            if !unchanged {
                changed(&out_dir);
            }
            current(&out_dir, &CARGO).and_then(|it| it.same_as(&built))
        };
        assert_eq!(check(false), Err("wrote something else".to_owned()));
        // The directory is as the run left it, as it is after cargo's.
        assert_eq!(std::fs::read_to_string(&made).unwrap(), "fn made() {}");
        // Read again with nothing changed since, the run is still not the one cargo built with:
        // the build after the check that sent it to cargo may never have come.
        assert_eq!(check(true), Err("wrote something else".to_owned()));
    }

    #[test]
    fn a_run_is_the_same_as_another_but_for_its_directory() {
        let out_dir = ran("run", "fn made() {}");
        let other = ran("run-other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        let named = || Run::of(&out_dir).and_then(|it| it.named(&out_dir, &other));
        assert_eq!(named(), Run::of(&other));
        std::fs::write(other.join("made.rs"), "fn made_before() {}").unwrap();
        assert_ne!(named(), Run::of(&other));
    }

    #[test]
    fn a_rerun_is_compared_with_what_the_other_run_left() {
        let out_dir = ran("matches", "fn made() {}");
        let other = ran("matches-other", "fn made() {}");
        let rerun = |loaded: &Run| {
            changed(&out_dir);
            let run = current(&out_dir, &CARGO)?.named(&out_dir, &other)?;
            run.same_as(loaded)
        };
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(rerun(&Run::of(&other).unwrap()), Ok(()));
        std::fs::write(other.join("made.rs"), "fn made_for_the_load() {}").unwrap();
        assert_eq!(rerun(&Run::of(&other).unwrap()), Err("wrote something else".to_owned()));
    }

    /// Has the script of `out_dir` be one rustc compiled from `sources`, as `.d` says.
    fn compiled(out_dir: &Path, sources: &str) {
        let run_dir = out_dir.parent().unwrap();
        let mut ran: Ran =
            serde_json::from_slice(&std::fs::read(run_dir.join(RAN)).unwrap()).unwrap();
        let linked = run_dir.join("build_script_build-0123");
        if ran.real != linked.with_extension("real") {
            std::fs::copy(&ran.real, linked.with_extension("real")).unwrap();
        }
        ran.real = linked.with_extension("real");
        // A copy on macOS keeps the time of the file it was copied from.
        let real = std::fs::File::options().write(true).open(&ran.real).unwrap();
        real.set_modified(SystemTime::now()).unwrap();
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        std::fs::write(linked.with_extension("d"), format!("{}: {sources}\n", linked.display()))
            .unwrap();
    }

    #[test]
    fn a_script_newer_than_its_source_is_as_cargo_would_compile_it() {
        let out_dir = ran("fresh", "fn made() {}");
        let source = out_dir.parent().unwrap().join("build source.rs");
        std::fs::write(&source, "fn main() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        compiled(&out_dir, "build\\ source.rs");
        let workspace = out_dir.parent().unwrap();
        assert!(compiled_from_what_is_there(&out_dir, workspace));
        assert!(
            !compiled_from_what_is_there(&out_dir, Path::new("/elsewhere")),
            "a source that is not there"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&source, "fn main() { }").unwrap();
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
    }

    #[test]
    fn a_script_that_read_a_variable_or_has_no_dep_info_is_cargos() {
        let out_dir = ran("env", "fn made() {}");
        let workspace = out_dir.parent().unwrap();
        assert!(!compiled_from_what_is_there(&out_dir, workspace), "no .d");
        let source = out_dir.parent().unwrap().join("build.rs");
        std::fs::write(&source, "fn main() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        compiled(&out_dir, "build.rs\n\n# env-dep:SOME_VARIABLE=1");
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
        // One of cargo's own is as cargo set it to run the script, or not.
        let at = workspace.join(RAN);
        let mut ran: Ran = serde_json::from_slice(&std::fs::read(&at).unwrap()).unwrap();
        ran.env.push(("CARGO_PKG_VERSION".to_owned(), "1.0.0".to_owned()));
        std::fs::write(&at, serde_json::to_vec(&ran).unwrap()).unwrap();
        compiled(&out_dir, "build.rs\n\n# env-dep:CARGO_PKG_VERSION=1.0.0");
        assert!(compiled_from_what_is_there(&out_dir, workspace));
        compiled(&out_dir, "build.rs\n\n# env-dep:CARGO_PKG_VERSION=2.0.0");
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
    }

    /// cargo's second layout of its build directory keeps what a script printed in
    /// `run/stdout`, not in `output`. It is nightly's, and a run there is not made: cargo's
    /// records of it are not read.
    #[test]
    fn a_run_in_the_second_layout_is_read_from_run_stdout() {
        let out_dir = ran("second", "fn made() {}");
        let run_dir = out_dir.parent().unwrap();
        std::fs::create_dir(run_dir.join("run")).unwrap();
        std::fs::rename(run_dir.join("output"), run_dir.join("run/stdout")).unwrap();
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(printed(&out_dir), Some(run_dir.join("run/stdout")));
        assert!(current(&out_dir, &CARGO).is_err());
        let other = ran("second-other", "fn made() {}");
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(Run::of(&out_dir).and_then(|it| it.named(&out_dir, &other)), Run::of(&other));
        assert!(in_out_dir(Path::new("/t/debug/build/app/0123/out/made.rs")));
        assert!(in_out_dir(Path::new("/t/debug/build/app-0123/out/made.rs")));
        assert!(!in_out_dir(Path::new("/t/debug/deps/app-0123.d")));
    }

    /// A script that builds into its `OUT_DIR` once, as one that runs a frontend's build does,
    /// and says so on the way: the tool's lines have the time it took and the name of the
    /// directory it wrote in, and a run that finds what the first built does not run it.
    const BUILDS_ONCE: &str = "#!/bin/sh
if [ ! -f \"$OUT_DIR/built\" ]; then
  echo \"built in $SECONDS s by process $$, into $OUT_DIR\"
  echo built > \"$OUT_DIR/built\"
fi
echo \"cargo:rerun-if-changed=input.txt\"
echo \"cargo:rustc-env=INPUT=$(cat input.txt)\"
";

    /// Has `out_dir`'s script run as cargo runs it: in its directory, which it fills, and again
    /// in the same directory, which the second run finds as the first left it.
    fn run_as_cargo_does(out_dir: &Path, times: usize) {
        let ran = ran_of(out_dir);
        for _ in 0..times {
            let mut script = Command::new(&ran.real);
            script.env_clear().envs(ran.env.iter().map(|(name, value)| (name, value)));
            let printed = script.current_dir(&ran.cwd).output().unwrap();
            assert!(printed.status.success());
            recorded(out_dir, &printed.stdout);
        }
    }

    #[test]
    fn a_rerun_finds_in_its_directory_what_the_last_run_left_as_cargos_does() {
        let out_dir = ran_script("builds-once", BUILDS_ONCE, "");
        let run_dir = out_dir.parent().unwrap();
        std::fs::write(run_dir.join("input.txt"), "one").unwrap();
        // The first run built, and said so; cargo runs the script again in the same directory
        // for a change, and the second run found what the first had built and said nothing.
        run_as_cargo_does(&out_dir, 1);
        let cold = std::fs::read_to_string(run_dir.join("output")).unwrap();
        assert!(cold.starts_with("built in "), "{cold}");
        run_as_cargo_does(&out_dir, 1);
        let rerun = std::fs::read_to_string(run_dir.join("output")).unwrap();
        assert_eq!(
            rerun,
            "cargo:rerun-if-changed=input.txt\ncargo:rustc-env=INPUT=one\ncargo:rerun-if-changed=watched\n"
        );
        // A run into a directory with nothing in it would build again and say so; one in
        // cargo's own finds what the last run built, as cargo's does.
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        let left: Vec<_> = std::fs::read_dir(&out_dir).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(std::fs::read_to_string(out_dir.join("built")).unwrap(), "built\n");
        // What the script prints for cargo is another matter.
        std::fs::write(run_dir.join("input.txt"), "two").unwrap();
        assert_eq!(rerun_is_same(&out_dir), Err("printed something else".to_owned()));
    }

    #[test]
    fn what_a_script_says_that_cargo_does_not_read_is_not_compared() {
        let out_dir = ran_script("says-more", BUILDS_ONCE, "");
        let run_dir = out_dir.parent().unwrap();
        std::fs::write(run_dir.join("input.txt"), "one").unwrap();
        // The output cargo kept is of the first run, which said how long it took.
        run_as_cargo_does(&out_dir, 1);
        let other = ran_script("says-more-other", BUILDS_ONCE, "");
        std::fs::write(other.parent().unwrap().join("input.txt"), "one").unwrap();
        run_as_cargo_does(&other, 2);
        let named = || Run::of(&out_dir).and_then(|it| it.named(&out_dir, &other));
        assert_eq!(named(), Run::of(&other));
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        assert_eq!(named(), Run::of(&other));
    }

    #[test]
    fn only_the_lines_cargo_reads_are_directives() {
        let printed =
            b"  cargo:a=1 \nbuilt in 3 s\ncargo::b=2\r\n\xff\xfecargo:c=3\n cargo\ncargo:d=4";
        assert_eq!(directives(printed), ["cargo:a=1", "cargo::b=2", "cargo:d=4"]);
        assert!(directives(b"").is_empty());
        // A line that is not text is no line, whatever else is on it; the bytes are not decoded
        // as text first, which would give a line of the bad bytes' replacement.
        assert_eq!(directives(b"cargo:rustc-env=KEY=\xff\ncargo:b=2\n"), ["cargo:b=2"]);
        assert_eq!(
            directives("cargo:rustc-env=KEY=\u{fffd}\n".as_bytes()),
            ["cargo:rustc-env=KEY=\u{fffd}"]
        );
    }

    #[test]
    fn the_name_of_the_directory_a_script_ran_in_is_replaced_in_the_lines_cargo_reads() {
        let printed = b"cargo:rustc-link-search=/t/copy/lib\nbuilt in /t/copy\n cargo::rerun-if-changed=/t/copy/a \n";
        let directives = directives(printed).into_iter().map(str::to_owned).collect();
        let run = Run { directives, wrote: Vec::new() };
        let named = run.named(Path::new("/t/copy"), Path::new("/t/out")).unwrap();
        assert_eq!(
            named.directives,
            ["cargo:rustc-link-search=/t/out/lib", "cargo::rerun-if-changed=/t/out/a"]
        );
    }

    #[test]
    fn the_digest_of_a_directory_is_of_what_a_compiler_reads_there() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = std::env::temp_dir().join(format!("ra-jit-digest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("out");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/f"), "x").unwrap();
        std::fs::write(root.join("outside"), "o").unwrap();
        symlink("sub/f", dir.join("alias")).unwrap();
        symlink("sub", dir.join("dir")).unwrap();
        symlink(".", dir.join("self")).unwrap();
        symlink("nowhere", dir.join("gone")).unwrap();
        symlink("../outside", dir.join("outside")).unwrap();
        symlink("b", dir.join("a")).unwrap();
        symlink("a", dir.join("b")).unwrap();
        let first = tree_digest(&dir).unwrap();
        // Neither times nor permissions are read.
        set_written(&dir.join("sub/f"), SystemTime::UNIX_EPOCH);
        std::fs::set_permissions(dir.join("sub/f"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert_eq!(tree_digest(&dir).unwrap(), first);
        // What a link leads to is, inside the directory and out of it.
        std::fs::write(dir.join("alias"), "y").unwrap();
        assert_ne!(tree_digest(&dir).unwrap(), first);
        std::fs::write(dir.join("sub/f"), "x").unwrap();
        assert_eq!(tree_digest(&dir).unwrap(), first);
        std::fs::write(root.join("outside"), "p").unwrap();
        assert_ne!(tree_digest(&dir).unwrap(), first);
        std::fs::write(root.join("outside"), "o").unwrap();
        // And so is where a link leads, with the same bytes there.
        std::fs::write(dir.join("sub/g"), "x").unwrap();
        let with_g = tree_digest(&dir).unwrap();
        std::fs::remove_file(dir.join("alias")).unwrap();
        symlink("sub/g", dir.join("alias")).unwrap();
        assert_ne!(tree_digest(&dir).unwrap(), with_g);
        // The same tree in another directory has the same digest.
        let again = root.join("again");
        assert!(Command::new("cp").arg("-R").arg(&dir).arg(&again).status().unwrap().success());
        assert_eq!(tree_digest(&again).unwrap(), tree_digest(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Has `path`, a file or a directory, be last written at `time`.
    fn set_written(path: &Path, time: SystemTime) {
        std::fs::File::open(path).unwrap().set_modified(time).unwrap();
    }

    fn running_as_root() -> bool {
        let id = Command::new("id").arg("-u").output().unwrap();
        String::from_utf8_lossy(&id.stdout).trim() == "0"
    }

    /// A script that makes `data` again when `kept`, which it keeps in its `OUT_DIR`, is older
    /// than `input`, which is beside the directory: as a script that calls `make` does.
    fn makes_again_when_older(name: &str, kept: &str) -> PathBuf {
        let lines = format!(
            "#!/bin/sh\nif [ \"$OUT_DIR/{kept}\" -ot input ]; then printf %s new > \"$OUT_DIR/data\"; fi\necho \"cargo:in=$OUT_DIR\"\n"
        );
        let out_dir = ran_script(name, &lines, "");
        std::fs::write(out_dir.join("data"), "old").unwrap();
        let input = out_dir.parent().unwrap().join("input");
        std::fs::write(&input, "changed").unwrap();
        set_written(&input, SystemTime::now() - Duration::from_secs(1800));
        out_dir
    }

    /// The result of a check of `out_dir`'s script, which writes `new` to `data` where it was
    /// `old` when it finds what cargo's run finds, and what is in `data` after it: what cargo's
    /// run in the directory leaves, as the run was.
    fn check_as_cargos_run(out_dir: &Path) -> (Result<(), String>, String) {
        let result = rerun_is_same(out_dir);
        (result, std::fs::read_to_string(out_dir.join("data")).unwrap())
    }

    #[test]
    fn a_directory_older_than_the_input_is_as_old_as_under_cargo() {
        let out_dir = makes_again_when_older("dir-time", "cache");
        let cache = out_dir.join("cache");
        std::fs::create_dir(&cache).unwrap();
        set_written(&cache, SystemTime::now() - Duration::from_secs(3600));
        let wrote = check_as_cargos_run(&out_dir);
        assert_eq!(wrote, (Err("wrote something else".to_owned()), "new".to_owned()));
    }

    #[test]
    fn a_read_only_file_older_than_the_input_is_as_old_as_under_cargo() {
        use std::os::unix::fs::PermissionsExt as _;
        let out_dir = makes_again_when_older("file-time", "stamp");
        let stamp = out_dir.join("stamp");
        std::fs::write(&stamp, "").unwrap();
        set_written(&stamp, SystemTime::now() - Duration::from_secs(3600));
        std::fs::set_permissions(&stamp, std::fs::Permissions::from_mode(0o444)).unwrap();
        let wrote = check_as_cargos_run(&out_dir);
        assert_eq!(wrote, (Err("wrote something else".to_owned()), "new".to_owned()));
    }

    #[test]
    fn a_directory_that_cannot_be_written_to_cannot_be_written_to_as_under_cargo() {
        use std::os::unix::fs::PermissionsExt as _;
        if running_as_root() {
            return;
        }
        let lines = "#!/bin/sh\nif [ ! -w \"$OUT_DIR/sealed\" ]; then printf %s new > \"$OUT_DIR/data\"; fi\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("dir-mode", lines, "");
        std::fs::write(out_dir.join("data"), "old").unwrap();
        let sealed = out_dir.join("sealed");
        std::fs::create_dir(&sealed).unwrap();
        std::fs::write(sealed.join("inside"), "x").unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o555)).unwrap();
        let wrote = check_as_cargos_run(&out_dir);
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(wrote, (Err("wrote something else".to_owned()), "new".to_owned()));
    }

    /// The verifier's case of round 2 (docs/reports/BUILDSCRIPT.md): two names of one file, which
    /// a copy makes two files.
    #[test]
    fn two_names_of_one_file_are_one_file_as_under_cargo() {
        let lines = "#!/bin/sh\nif [ \"$OUT_DIR/a\" -ef \"$OUT_DIR/b\" ]; then printf %s new > \"$OUT_DIR/data\"; fi\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("hard-link", lines, "");
        std::fs::write(out_dir.join("a"), "x").unwrap();
        std::fs::hard_link(out_dir.join("a"), out_dir.join("b")).unwrap();
        std::fs::write(out_dir.join("data"), "old").unwrap();
        let wrote = check_as_cargos_run(&out_dir);
        assert_eq!(wrote, (Err("wrote something else".to_owned()), "new".to_owned()));
    }

    /// The verifier's case of round 2: a link to a link, the middle one of which the script
    /// points elsewhere and back, which a copy made a link to where the last one led.
    #[test]
    fn a_link_to_a_link_leads_through_it_as_under_cargo() {
        let lines = "#!/bin/sh\nrm \"$OUT_DIR/middle\"\nln -s b \"$OUT_DIR/middle\"\nif [ \"$(cat \"$OUT_DIR/alias\")\" = B ]; then printf %s new > \"$OUT_DIR/data\"; fi\nrm \"$OUT_DIR/middle\"\nln -s a \"$OUT_DIR/middle\"\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("link-chain", lines, "");
        std::fs::write(out_dir.join("a"), "A").unwrap();
        std::fs::write(out_dir.join("b"), "B").unwrap();
        std::fs::write(out_dir.join("data"), "old").unwrap();
        std::os::unix::fs::symlink("a", out_dir.join("middle")).unwrap();
        std::os::unix::fs::symlink("middle", out_dir.join("alias")).unwrap();
        let wrote = check_as_cargos_run(&out_dir);
        assert_eq!(wrote, (Err("wrote something else".to_owned()), "new".to_owned()));
    }

    /// The verifier's case of round 2: an extended attribute, which a copy of the bytes loses.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_extended_attribute_is_there_as_under_cargo() {
        let lines = "#!/bin/sh\nif [ \"$(xattr -p com.blaze.test \"$OUT_DIR/a\" 2>/dev/null)\" = yes ]; then printf %s new > \"$OUT_DIR/data\"; fi\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("xattr", lines, "");
        std::fs::write(out_dir.join("a"), "x").unwrap();
        std::fs::write(out_dir.join("data"), "old").unwrap();
        let mut set = Command::new("xattr");
        set.args(["-w", "com.blaze.test", "yes"]).arg(out_dir.join("a"));
        assert!(set.status().unwrap().success());
        let wrote = check_as_cargos_run(&out_dir);
        assert_eq!(wrote, (Err("wrote something else".to_owned()), "new".to_owned()));
    }

    /// A script that writes through `alias` in its `OUT_DIR`.
    const WRITES_THROUGH_ALIAS: &str =
        "#!/bin/sh\nprintf %s new > \"$OUT_DIR/alias\"\necho \"cargo:in=$OUT_DIR\"\n";

    #[test]
    fn a_write_through_a_link_out_of_the_directory_is_a_difference() {
        for (name, relative) in [("relative", true), ("absolute", false)] {
            let out_dir = ran_script(&format!("link-out-{name}"), WRITES_THROUGH_ALIAS, "");
            let state = out_dir.parent().unwrap().join("state");
            std::fs::write(&state, "old").unwrap();
            let target = if relative { PathBuf::from("../state") } else { state.clone() };
            std::os::unix::fs::symlink(target, out_dir.join("alias")).unwrap();
            let result = rerun_is_same(&out_dir);
            // The script wrote through the link, as it does under cargo, and what the link leads
            // to is what is compared.
            assert_eq!(std::fs::read_to_string(&state).unwrap(), "new", "{name}");
            assert_eq!(result, Err("wrote something else".to_owned()), "{name}");
        }
    }

    #[test]
    fn a_write_through_a_link_back_in_by_another_name_is_a_difference() {
        let out_dir = ran_script("link-back", WRITES_THROUGH_ALIAS, "");
        std::fs::write(out_dir.join("data"), "old").unwrap();
        // `../out/data` is the data of the directory by a name that goes out of it.
        std::os::unix::fs::symlink("../out/data", out_dir.join("alias")).unwrap();
        let (result, data) = check_as_cargos_run(&out_dir);
        assert_eq!((result, data.as_str()), (Err("wrote something else".to_owned()), "new"));
    }

    #[test]
    fn a_write_of_the_same_through_links_inside_the_directory_is_the_same() {
        let lines = "#!/bin/sh\nfor name in alias absolute dir/f; do printf %s same > \"$OUT_DIR/$name\"; done\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("link-in", lines, "");
        std::fs::create_dir(out_dir.join("sub")).unwrap();
        let (data, f) = (out_dir.join("data"), out_dir.join("sub/f"));
        for file in [&data, &f] {
            std::fs::write(file, "same").unwrap();
        }
        std::os::unix::fs::symlink("data", out_dir.join("alias")).unwrap();
        std::os::unix::fs::symlink(&data, out_dir.join("absolute")).unwrap();
        std::os::unix::fs::symlink("sub", out_dir.join("dir")).unwrap();
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
    }

    /// The result of a check of a script that does nothing, in a directory with `link` leading
    /// to `target` in it.
    fn rerun_with_link(name: &str, link: &str, target: &str) -> Result<(), String> {
        let out_dir = ran_script(name, "#!/bin/sh\necho \"cargo:in=$OUT_DIR\"\n", "");
        std::os::unix::fs::symlink(target, out_dir.join(link)).unwrap();
        rerun_is_same(&out_dir)
    }

    #[test]
    fn a_link_that_leads_nowhere_or_in_a_circle_is_read_as_its_text() {
        assert_eq!(rerun_with_link("link-gone", "gone", "nowhere"), Ok(()));
        let out_dir = ran_script("link-circle", "#!/bin/sh\necho \"cargo:in=$OUT_DIR\"\n", "");
        std::os::unix::fs::symlink("b", out_dir.join("a")).unwrap();
        std::os::unix::fs::symlink("a", out_dir.join("b")).unwrap();
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
    }

    #[test]
    fn a_link_to_the_directory_itself_is_fine() {
        assert_eq!(rerun_with_link("link-self", "self", "."), Ok(()));
    }

    #[test]
    fn a_link_the_script_points_elsewhere_is_a_difference() {
        let lines = "#!/bin/sh\nrm -f \"$OUT_DIR/alias\"\nln -s b \"$OUT_DIR/alias\"\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("link-repoint", lines, "");
        for name in ["a", "b"] {
            std::fs::write(out_dir.join(name), "x").unwrap();
        }
        std::os::unix::fs::symlink("a", out_dir.join("alias")).unwrap();
        assert_eq!(rerun_is_same(&out_dir), Err("wrote something else".to_owned()));
    }

    /// Has the script of `out_dir` run with `name` set to `value`, as cargo's run had.
    fn with_variable(out_dir: &Path, name: &str, value: &Path) {
        let run_dir = out_dir.parent().unwrap();
        let mut ran: Ran =
            serde_json::from_slice(&std::fs::read(run_dir.join(RAN)).unwrap()).unwrap();
        ran.env.push((name.to_owned(), value.to_str().unwrap().to_owned()));
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
    }

    #[test]
    fn a_write_into_the_directory_by_another_path_is_a_difference() {
        let lines =
            "#!/bin/sh\nprintf %s new > \"$CARGO_MANIFEST_PATH\"\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("by-path", lines, "");
        std::fs::write(out_dir.join("data"), "old").unwrap();
        with_variable(&out_dir, "CARGO_MANIFEST_PATH", &out_dir.join("data"));
        assert_eq!(check_as_cargos_run(&out_dir).0, Err("wrote something else".to_owned()));
    }

    #[test]
    fn a_file_written_again_with_the_bytes_it_had_is_the_same() {
        // It is newer than what rustc read of it, and cargo, whose next build runs the script
        // again, compiles the package again then whatever the script wrote; what rustc reads of
        // it is what it read.
        let lines = "#!/bin/sh\nprintf %s old > \"$OUT_DIR/data\"\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("touched", lines, "");
        let data = out_dir.join("data");
        std::fs::write(&data, "old").unwrap();
        set_written(&data, SystemTime::now() - Duration::from_secs(3600));
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        let written = std::fs::metadata(&data).unwrap().modified().unwrap();
        assert!(written > SystemTime::now() - Duration::from_secs(60), "the script wrote it");
    }

    #[test]
    fn a_line_that_is_not_text_is_not_a_line_cargo_reads_in_either_run() {
        let out_dir = ran_script("utf8", "#!/bin/sh\nprintf 'cargo:rustc-env=KEY=\\377\\n'\n", "");
        recorded(
            &out_dir,
            "cargo:rustc-env=KEY=\u{fffd}\ncargo:rerun-if-changed=watched\n".as_bytes(),
        );
        let result = rerun_is_same(&out_dir);
        assert_eq!(result, Err("printed something else".to_owned()), "cargo has no KEY now");
        // The same of two runs that are compared.
        let (ours, theirs) = (ran("utf8-ours", ""), ran("utf8-theirs", ""));
        let print =
            |dir: &Path, bytes: &[u8]| std::fs::write(printed(dir).unwrap(), bytes).unwrap();
        let same =
            |dir: &Path, other: &Path| Run::of(dir)?.named(dir, other)?.same_as(&Run::of(other)?);
        print(&ours, b"cargo:rustc-env=KEY=\xff\n");
        print(&theirs, "cargo:rustc-env=KEY=\u{fffd}\n".as_bytes());
        assert!(same(&ours, &theirs).is_err());
        assert!(same(&theirs, &ours).is_err());
        // Cargo reads neither line.
        print(&theirs, b"cargo:rustc-env=KEY=\xfe\n");
        assert_eq!(same(&ours, &theirs), Ok(()));
    }

    #[test]
    fn a_directory_that_is_missing_is_made_as_cargo_makes_it() {
        let out_dir = ran("missing", "");
        let built = Run::of(&out_dir).unwrap();
        std::fs::remove_dir(&out_dir).unwrap();
        changed(&out_dir);
        let check = current(&out_dir, &CARGO).and_then(|it| it.same_as(&built));
        assert_eq!(check, Err("wrote something else".to_owned()));
        assert_eq!(std::fs::read_to_string(out_dir.join("made.rs")).unwrap(), "");
    }

    #[test]
    fn a_script_that_fails_is_an_error_and_leaves_a_record_of_a_run_that_did_not_end() {
        let out_dir = ran_script("failed", "#!/bin/sh\nexit 42\n", "");
        let output = std::fs::read(out_dir.parent().unwrap().join("output")).unwrap();
        assert!(rerun_is_same(&out_dir).is_err());
        // As cargo leaves it: an empty hash, so the next build runs it again, and what the run
        // before printed.
        assert_eq!(std::fs::read(hash_of(&out_dir)).unwrap(), b"");
        assert_eq!(std::fs::read(out_dir.parent().unwrap().join("output")).unwrap(), output);
        assert!(cargo_would_run(&out_dir));
        let again = current(&out_dir, &CARGO);
        assert!(
            again.as_ref().is_err_and(|it| it.contains("no record of a run that ended")),
            "{again:?}"
        );
    }

    #[test]
    fn an_executed_failure_is_not_permission_to_retry_this_build() {
        let out_dir = ran_script(
            "fail-on-second",
            "#!/bin/sh\necho run >> \"$OUT_DIR/../runs\"\nif [ \"$(wc -l < \"$OUT_DIR/../runs\")\" -eq 2 ]; then echo 'second run fails' >&2; exit 3; fi\n",
            "",
        );
        run_as_cargo_does(&out_dir, 1);
        changed(&out_dir);
        let failure = judge(&out_dir, &CARGO).unwrap_err();
        assert!(matches!(failure, JudgementError::Failed(_)), "{failure:?}");
        assert!(failure.to_string().contains("second run fails"));
        assert_eq!(runs(&out_dir), 2);
        assert_eq!(std::fs::read(hash_of(&out_dir)).unwrap(), b"");
    }

    #[test]
    fn a_run_that_drops_watches_keeps_the_before_run_fingerprint_and_a_build_result() {
        let out_dir = ran_script(
            "dropper",
            "#!/bin/sh\necho run >> \"$OUT_DIR/../runs\"\nprintf 'pub const RUNS: u32 = 2;\\n' > \"$OUT_DIR/generated.rs\"\n",
            "",
        );
        let mut ran = ran_of(&out_dir);
        ran.real = ran.real.with_extension("inner");
        std::fs::write(out_dir.parent().unwrap().join(RAN), serde_json::to_vec(&ran).unwrap())
            .unwrap();
        let before = records::Unit::of(&out_dir).unwrap().recorded().unwrap();
        changed(&out_dir);
        let mut completed = Vec::new();
        judge_for(&out_dir, &CARGO, &mut completed).unwrap();
        assert_eq!(runs(&out_dir), 1);
        assert_eq!(completed.len(), 1);
        assert_eq!(records::Unit::of(&out_dir).unwrap().recorded().unwrap(), before);
        let mut command = Command::new("sh");
        let continuation = Continuation::for_command(&mut command, &completed).unwrap().unwrap();
        let path = continuation.path();
        assert!(path.join("runs").exists());
        drop(continuation);
        assert!(!path.exists(), "a subsequent build cannot consume this run");
    }

    #[test]
    fn a_script_that_logs_an_error_or_prints_what_cargo_does_not_take_has_no_record_of_a_run() {
        let lines = "#!/bin/sh\necho \"cargo:in=$OUT_DIR\"\necho cargo::error=broken\n";
        let out_dir = ran_script("logs-error", lines, "");
        let output = std::fs::read(out_dir.parent().unwrap().join("output")).unwrap();
        assert!(rerun_is_same(&out_dir).unwrap_err().starts_with("logged an error"));
        assert_eq!(std::fs::read(hash_of(&out_dir)).unwrap(), b"");
        assert_eq!(std::fs::read(out_dir.parent().unwrap().join("output")).unwrap(), output);
        // A key cargo does not know: cargo 1.99.0 fails the build for `cargo::unknown-key=1`
        // ("Unknown key", exit 101, measured), writing what it printed and no fingerprint. The old
        // syntax, `cargo:unknown-key=1`, is metadata and cargo takes it.
        let lines = "#!/bin/sh\necho \"cargo:in=$OUT_DIR\"\necho cargo::unknown-key=1\n";
        let out_dir = ran_script("not-taken", lines, "");
        let result = rerun_is_same(&out_dir);
        assert!(result.as_ref().is_err_and(|it| it.contains("does not take")), "{result:?}");
        assert_eq!(std::fs::read(hash_of(&out_dir)).unwrap(), b"");
        let output = std::fs::read_to_string(out_dir.parent().unwrap().join("output")).unwrap();
        assert!(output.contains("cargo::unknown-key=1"), "{output}");
    }

    #[test]
    fn a_variable_the_script_reads_that_changed_is_cargos_to_run_it_for() {
        let lines = "#!/bin/sh\necho \"cargo:in=$OUT_DIR\"\necho cargo:rerun-if-env-changed=RA_JIT_TEST_UNSET_VARIABLE\n";
        let out_dir = ran_script("variable", lines, "");
        run_as_cargo_does(&out_dir, 1);
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        // The cargo that builds next has another value than the one cargo's record has.
        let set = |name: &str| (name == "RA_JIT_TEST_UNSET_VARIABLE").then(|| "1".to_owned());
        let cargo = Cargo { env: &set, ..CARGO };
        changed(&out_dir);
        let result = current(&out_dir, &cargo);
        assert!(
            result.as_ref().is_err_and(|it| it.contains("RA_JIT_TEST_UNSET_VARIABLE")),
            "{result:?}"
        );
        assert!(cargo_would_run(&out_dir), "nothing was run, and cargo runs it");
    }

    #[test]
    fn a_newly_watched_variable_has_the_current_requests_value() {
        let lines = r#"#!/bin/sh
echo run >> "$OUT_DIR/../runs"
n=0
if [ "$(cat watched)" = 8 ]; then
    echo cargo:rerun-if-env-changed=VP5_VAR
    n=${#VP5_VAR}
fi
printf 'pub const RUNS: u32 = %s;\n' "$n" > "$OUT_DIR/generated.rs"
printf %s "${VP5_VAR+x}" > "$OUT_DIR/present"
"#;
        for (case, before, value, expected) in [
            ("added", None, Some("new"), 3),
            ("changed", Some("old-value"), Some("new"), 3),
            ("removed", Some("old-value"), None, 0),
            ("empty", Some("old-value"), Some(""), 0),
        ] {
            let out_dir = ran_script(&format!("newenv-{case}"), lines, "");
            let run_dir = out_dir.parent().unwrap();
            if let Some(before) = before {
                with_variable(&out_dir, "VP5_VAR", Path::new(before));
            }
            std::fs::write(run_dir.join("watched"), "7").unwrap();
            run_as_cargo_does(&out_dir, 1);
            let built = Run::of(&out_dir).unwrap();
            std::fs::write(run_dir.join("watched"), "8").unwrap();
            changed(&out_dir);
            let env = |name: &str| match name {
                "VP5_VAR" => value.map(str::to_owned),
                _ => cargo_env(name),
            };
            let names = vec!["VP5_VAR".to_owned()];
            let cargo = Cargo { env: &env, env_names: &names, ..CARGO };
            let rerun = current(&out_dir, &cargo).unwrap();
            assert_ne!(rerun, built, "{case}: the edited source changes the run");
            assert_eq!(
                std::fs::read_to_string(out_dir.join("generated.rs")).unwrap(),
                format!("pub const RUNS: u32 = {expected};\n"),
                "{case}"
            );
            assert_eq!(
                std::fs::read_to_string(out_dir.join("present")).unwrap(),
                if value.is_some() { "x" } else { "" },
                "{case}"
            );
            assert_eq!(runs(&out_dir), 2, "{case}: one initial run and one edit run");
            assert_eq!(current(&out_dir, &cargo), Ok(rerun), "{case}");
            assert_eq!(runs(&out_dir), 2, "{case}: the recorded run stays fresh");
        }
    }

    #[test]
    fn a_script_with_no_rerun_lines_runs_once_per_package_edit() {
        let lines = r#"#!/bin/sh
echo run >> "$OUT_DIR/../runs"
echo cargo:rustc-cfg=quiet_cfg
printf 'pub const RUNS: u32 = 1;\n' > "$OUT_DIR/generated.rs"
"#;
        let out_dir = ran_script("quiet", lines, "");
        let run_dir = out_dir.parent().unwrap();
        // The usual fixture puts the manifest in the run directory. Its records would
        // then be package inputs, so a quiet script needs a separate source directory.
        let package = run_dir.ancestors().nth(4).unwrap().join("package");
        std::fs::create_dir_all(package.join("src")).unwrap();
        let manifest = package.join("Cargo.toml");
        let source = package.join("src/main.rs");
        std::fs::write(
            &manifest,
            "[package]\nname = \"quiet\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(&source, "fn main() { println!(\"7\"); }\n").unwrap();
        let before = SystemTime::now() - Duration::from_secs(3600);
        set_written(&manifest, before);
        set_written(&source, before);
        let mut ran = ran_of(&out_dir);
        ran.real = ran.real.with_extension("inner");
        ran.cwd = package.clone();
        for (name, value) in &mut ran.env {
            if name == "CARGO_MANIFEST_DIR" {
                *value = package.to_str().unwrap().to_owned();
            }
        }
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        let unit = records::Unit::of(&out_dir).unwrap();
        let began = unit.begin().unwrap();
        let initial = Command::new(&ran.real)
            .env_clear()
            .envs(ran.env.iter().map(|(name, value)| (name, value)))
            .current_dir(&ran.cwd)
            .output()
            .unwrap();
        assert!(initial.status.success());
        unit.ran(began, &initial.stdout, &initial.stderr).unwrap();
        records::tests::recorded_as_cargo_would(&out_dir, &package, &initial.stdout);
        let built = Run::of(&out_dir).unwrap();
        assert_eq!(runs(&out_dir), 1);
        assert_eq!(current(&out_dir, &CARGO), Ok(built.clone()));
        assert_eq!(runs(&out_dir), 1);
        for number in 8..10 {
            std::fs::write(&source, format!("fn main() {{ println!(\"{number}\"); }}\n")).unwrap();
            set_written(&source, before + Duration::from_secs(number as u64));
            assert_eq!(current(&out_dir, &CARGO), Ok(built.clone()));
            assert_eq!(runs(&out_dir), number - 6);
            assert_eq!(current(&out_dir, &CARGO), Ok(built.clone()));
            assert_eq!(runs(&out_dir), number - 6);
        }
    }

    #[test]
    fn an_immutable_quiet_packages_version_is_not_replaced_by_file_times() {
        let out_dir = ran_script("quiet-registry", COUNTS, "");
        recorded(&out_dir, format!("cargo:in={}\n", out_dir.display()).as_bytes());
        let unit = records::Unit::of(&out_dir).unwrap();
        let version = vec![records::Local::Precalculated("1.2.3".to_owned())];
        unit.record(&unit.recorded().unwrap().with_local(version.clone())).unwrap();
        let before = Run::of(&out_dir).unwrap();
        assert_eq!(current(&out_dir, &CARGO), Ok(before));
        assert_eq!(runs(&out_dir), 0);
        assert_eq!(unit.recorded().unwrap().local, version);
    }

    #[test]
    fn a_quiet_run_that_starts_watching_files_records_cargos_output_path() {
        let out_dir = ran_script("quiet-add-watch", COUNTS, "");
        let unit = records::Unit::of(&out_dir).unwrap();
        let recorded = unit.recorded().unwrap();
        let named = unit.output_named(&recorded).unwrap();
        std::fs::write(unit.output(), format!("cargo:in={}\n", out_dir.display())).unwrap();
        changed(&out_dir);
        assert!(current(&out_dir, &CARGO).is_ok());
        let after = unit.recorded().unwrap();
        assert_eq!(
            after.local,
            vec![records::Local::RerunIfChanged {
                output: named,
                paths: vec![PathBuf::from("watched")],
            }]
        );
        assert_eq!(runs(&out_dir), 1);
        assert!(current(&out_dir, &CARGO).is_ok());
        assert_eq!(runs(&out_dir), 1);
    }

    #[test]
    fn a_file_that_is_no_file_directory_or_link_is_not_read() {
        let out_dir = ran("fifo", "");
        assert!(Command::new("mkfifo").arg(out_dir.join("pipe")).status().unwrap().success());
        // Opening a pipe to read it waits for whoever writes to it.
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || _ = send.send(rerun_is_same(&out_dir)));
        let result = receive.recv_timeout(Duration::from_secs(20));
        assert!(matches!(result, Ok(Err(_))), "{result:?}");
    }

    /// What a check costs, by the step, on a directory of the size of the one tracedecay's CLI
    /// build script leaves (125 files, 4.5 MB, by docs/reports/BUILDSCRIPT.md), for a script
    /// that does nothing.
    /// A measurement, not a test: `cargo test --release -p ra-jit --bin ra-jit -- --ignored
    /// --nocapture the_cost`.
    #[test]
    #[ignore = "a measurement"]
    fn the_cost_of_a_check_on_a_directory_the_size_of_tracedecays() {
        use std::time::Instant;
        let out_dir = ran_script("cost", "#!/bin/sh\necho \"cargo:in=$OUT_DIR\"\n", "");
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut bytes = |len: usize| -> Vec<u8> {
            let mut random = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            };
            (0..len).map(|_| random()).collect()
        };
        let mut total = 0;
        for at in 0..125 {
            let len = match at {
                0 => 1_000_000,
                1 => 900_000,
                2 => 800_000,
                _ => 15_000,
            };
            let dir = out_dir.join(format!("dir{}", at % 7));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("file{at}")), bytes(len)).unwrap();
            total += len;
        }
        println!("125 files, {total} bytes");
        let phases = [
            "the run the build had (Run::of, at the build)",
            "the check: the script and the digest after it",
        ];
        let mut times: Vec<Vec<f64>> = vec![Vec::new(); phases.len()];
        for _ in 0..10 {
            let began = Instant::now();
            let built = Run::of(&out_dir).unwrap();
            let checking = Instant::now();
            changed(&out_dir);
            current(&out_dir, &CARGO).unwrap().same_as(&built).unwrap();
            let done = Instant::now();
            times[0].push((checking - began).as_secs_f64() * 1000.0);
            times[1].push((done - checking).as_secs_f64() * 1000.0);
        }
        for (what, mut times) in phases.iter().zip(times) {
            times.sort_by(f64::total_cmp);
            println!(
                "{what:48} min {:6.1} ms, median {:6.1} ms, max {:6.1} ms (10 runs)",
                times[0], times[5], times[9]
            );
        }
    }
}
