//! Build scripts of the workspace. Cargo runs one again when what it said it reads has
//! changed, or when anything in its package has if it said nothing, and then compiles the
//! package again whatever the script printed. The daemon runs the script the same way and
//! looks at what it printed and wrote: when that is what it was, the crates compiled with
//! it are still right, and the build stays the daemon's.
//!
//! To run a script as cargo does, the wrapper puts a few lines of shell where rustc linked
//! it. Cargo runs those, and they run `ra-jit build-script`, which writes down the
//! environment names and directory cargo ran it in and becomes the script. Values
//! are held by the daemon only for its lifetime.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The variable that names the script itself to `ra-jit build-script`.
const REAL: &str = "RA_JIT_BUILD_SCRIPT";

/// The file next to a script's `OUT_DIR` that says how cargo ran it.
const RAN: &str = "ra-jit-ran.json";

/// The file next to a script's `OUT_DIR` whose time is when [`rerun_is_same`] last found it
/// the same.
const CHECKED: &str = "ra-jit-checked";

/// How cargo ran a build script.
#[derive(Serialize, Deserialize)]
struct Ran {
    real: PathBuf,
    cwd: PathBuf,
    env_names: Vec<String>,
}

static ENVIRONMENTS: std::sync::LazyLock<
    std::sync::Mutex<BTreeMap<PathBuf, crate::process::Native>>,
> = std::sync::LazyLock::new(Default::default);

pub(crate) fn remember(dir: PathBuf, env: crate::process::Native) {
    crate::lock(&ENVIRONMENTS).insert(dir, env);
}

pub(crate) fn remembered(dir: &Path) -> Option<crate::process::Native> {
    crate::lock(&ENVIRONMENTS).get(dir).cloned()
}

pub(crate) fn environment(dir: &Path) -> Option<crate::process::Native> {
    remembered(dir).or_else(|| {
        let reply = super::client::ask(
            super::daemon::Request::ScriptEnvironment(dir.to_owned()),
            super::client::Start::Never,
        )
        .ok()??;
        match reply {
            super::daemon::Reply::ScriptEnvironment(env) => env,
            _ => None,
        }
    })
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
    let env = crate::process::Native::of(std::env::vars_os().filter(|(name, _)| name != REAL));
    let out_dir = std::env::var_os("OUT_DIR").map(PathBuf::from);
    let args: Vec<OsString> = args.collect();
    let cwd = std::env::current_dir()?;
    if let Some(run_dir) = out_dir.as_deref().and_then(Path::parent) {
        // This request is memory-only. A daemon restart loses replay eligibility;
        // it never reconstructs a caller snapshot from disk or its own environment.
        super::client::ask(
            super::daemon::Request::RememberScript { dir: run_dir.to_owned(), env: env.clone() },
            super::client::Start::Never,
        )?;
        let ran = Ran {
            real: real.clone(),
            cwd: cwd.clone(),
            env_names: env.vars().map(|(name, _)| crate::env_identity::name(&name)).collect(),
        };
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
    out_dir.parent().is_some_and(|it| it.join(RAN).exists() && environment(it).is_some())
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
    let Some(env) = environment(run_dir) else { return false };
    let env = env.unicode();
    let set = |name: &str| env.get(name);
    if read.env.iter().any(|(name, value)| {
        !name.starts_with("CARGO_")
            || !crate::env_identity::matches(set(name).as_deref(), value.as_deref())
    }) {
        return false;
    }
    !read.files.is_empty()
        && read.files.iter().all(|it| {
            let written = std::fs::metadata(workspace.join(it)).and_then(|it| it.modified());
            written.is_ok_and(|it| it <= compiled)
        })
}

/// Runs the script that wrote `out_dir` again, as cargo ran it but into a directory of its
/// own: what cargo's holds is read by the compilers of a build under way, and cargo
/// compiles the package again when a file there is newer than it was. `Ok` when the script
/// printed and wrote what it did then; `Err` is what is different, which a script that
/// writes the name of its directory into a file always is.
pub(crate) fn rerun_is_same(out_dir: &Path) -> Result<(), String> {
    // A file written as the script reads it can have a time a little before now: the
    // kernel stamps files with a clock that lags by up to a tick.
    let began = SystemTime::now() - 2 * file_clock_tick();
    rerun_matches(out_dir, out_dir)?;
    if let Some(run_dir) = out_dir.parent()
        && let Ok(checked) = std::fs::File::create(run_dir.join(CHECKED))
    {
        let _ = checked.set_modified(began);
    }
    Ok(())
}

/// How far behind the clock the kernel stamps a file it writes can be: Linux takes the time
/// of the last tick.
#[cfg(target_os = "linux")]
fn file_clock_tick() -> Duration {
    let mut tick = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `tick` is a timespec to write to.
    match unsafe { libc::clock_getres(libc::CLOCK_REALTIME_COARSE, &mut tick) } {
        0 => Duration::new(tick.tv_sec as u64, tick.tv_nsec as u32).max(Duration::from_millis(1)),
        _ => Duration::from_millis(20),
    }
}

/// Windows stamps files with the system time, which moves in steps of about 16 ms.
#[cfg(windows)]
fn file_clock_tick() -> Duration {
    Duration::from_millis(16)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn file_clock_tick() -> Duration {
    Duration::from_millis(1)
}

/// Whether the script that wrote `out_dir` was run again by [`rerun_is_same`], and printed
/// and wrote what it had, after `newest`: when nothing it reads is newer, a run now is the
/// same again.
pub(crate) fn checked_after(out_dir: &Path, newest: SystemTime) -> bool {
    let checked = out_dir.parent().map(|it| it.join(CHECKED));
    let checked = checked.and_then(|it| std::fs::metadata(it).and_then(|it| it.modified()).ok());
    checked.is_some_and(|it| newest < it)
}

/// Whether the run of a script that wrote `out_dir` printed and wrote what the run that wrote
/// `other` did, but for the name of its directory.
pub(crate) fn same_run(out_dir: &Path, other: &Path) -> bool {
    let printed = |dir: &Path| std::fs::read(printed(dir)?).ok();
    let (Some(ours), Some(theirs)) = (printed(out_dir), printed(other)) else { return false };
    let (Some(named), Some(names)) = (other.to_str(), out_dir.to_str()) else { return false };
    String::from_utf8_lossy(&ours).replace(names, named).as_bytes() == theirs
        && digest_of(out_dir) == digest_of(other)
}

/// Runs the script that wrote `out_dir` again as [`rerun_is_same`] does, and compares what
/// it prints and writes with what a run of the same script left in `other`: another build of
/// the package, which ran it with the same source and inputs.
pub(crate) fn rerun_matches(out_dir: &Path, other: &Path) -> Result<(), String> {
    let run_dir = out_dir.parent().ok_or("a build script without a directory")?;
    let other_dir = other.parent().ok_or("a build script without a directory")?;
    let read = |dir: &Path, name: &str| {
        std::fs::read(dir.join(name)).map_err(|it| format!("{name}: {it}"))
    };
    let ran: Ran = serde_json::from_slice(&read(run_dir, RAN)?).map_err(|it| it.to_string())?;
    let env = environment(run_dir).ok_or("build-script environment is no longer in memory")?;
    let printed = printed(other).ok_or("a build script without a directory")?;
    let printed = std::fs::read(&printed).map_err(|it| {
        let name = printed.strip_prefix(other_dir).unwrap_or(&printed);
        format!("{}: {it}", name.display())
    })?;
    let again_dir = run_dir.join(format!("ra-jit-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&again_dir);
    std::fs::create_dir(&again_dir).map_err(|it| it.to_string())?;
    let same = rerun_into(&ran, &env, &again_dir, other, &printed);
    let _ = std::fs::remove_dir_all(&again_dir);
    same
}

fn rerun_into(
    ran: &Ran,
    env: &crate::process::Native,
    again_dir: &Path,
    out_dir: &Path,
    printed: &[u8],
) -> Result<(), String> {
    let mut script = Command::new(&ran.real);
    env.apply(&mut script);
    script.env("OUT_DIR", again_dir).current_dir(&ran.cwd).stdin(Stdio::null());
    let again = script.output().map_err(|it| it.to_string())?;
    if !again.status.success() {
        return Err(format!("failed ({})", again.status));
    }
    let (Some(named), Some(names)) = (out_dir.to_str(), again_dir.to_str()) else {
        return Err("has a directory whose name is not text".to_owned());
    };
    if String::from_utf8_lossy(&again.stdout).replace(names, named).as_bytes() != printed {
        return Err("printed something else".to_owned());
    }
    if digest_of(again_dir) != digest_of(out_dir) {
        return Err("wrote something else".to_owned());
    }
    Ok(())
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
    use super::*;

    #[test]
    fn run_record_contains_names_only_and_restart_cannot_replay() {
        let dir = tempfile::tempdir().unwrap();
        let env = crate::process::Native::of([("TOKEN".into(), "synthetic-secret".into())]);
        let ran = Ran {
            real: dir.path().join("script"),
            cwd: dir.path().to_owned(),
            env_names: env.vars().map(|(name, _)| crate::env_identity::name(&name)).collect(),
        };
        let bytes = serde_json::to_vec(&ran).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-secret"));
        std::fs::write(dir.path().join(RAN), bytes).unwrap();
        remember(dir.path().to_owned(), env.clone());
        assert_eq!(remembered(dir.path()), Some(env));
        assert!(can_rerun(&dir.path().join("out")));
        crate::lock(&ENVIRONMENTS).remove(dir.path());
        assert_eq!(remembered(dir.path()), None);
    }

    /// A run directory as cargo and `ra-jit build-script` leave one, for a script that
    /// writes `wrote` and prints where.
    fn ran(name: &str, wrote: &str) -> PathBuf {
        let run_dir =
            std::env::temp_dir().join(format!("ra-jit-script-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&run_dir);
        let out_dir = run_dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let real = run_dir.join("script");
        let lines =
            "#!/bin/sh\nprintf %s \"$WROTE\" > \"$OUT_DIR/made.rs\"\necho \"cargo:in=$OUT_DIR\"\n";
        // A process a test thread starts while this one has the script open to write holds it
        // open too until that process execs, and the script is refused meanwhile ("text file
        // busy"): another process writes it.
        let written = Command::new("sh")
            .args(["-c", "printf %s \"$1\" > \"$0\" && chmod 755 \"$0\""])
            .arg(&real)
            .arg(lines)
            .status()
            .unwrap();
        assert!(written.success());
        let env = vec![
            ("OUT_DIR".to_owned(), out_dir.to_str().unwrap().to_owned()),
            ("WROTE".to_owned(), wrote.to_owned()),
            ("PATH".to_owned(), std::env::var("PATH").unwrap()),
        ];
        let native = crate::process::Native::of(env.into_iter().map(|(k, v)| (k.into(), v.into())));
        let ran = Ran {
            real,
            cwd: run_dir.clone(),
            env_names: native.vars().map(|(k, _)| crate::env_identity::name(&k)).collect(),
        };
        remember(run_dir.clone(), native);
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        std::fs::write(run_dir.join("output"), format!("cargo:in={}\n", out_dir.display()))
            .unwrap();
        out_dir
    }

    #[test]
    fn a_rerun_leaves_what_cargo_has_alone() {
        let out_dir = ran("same", "fn made() {}");
        let made = out_dir.join("made.rs");
        std::fs::write(&made, "fn made() {}").unwrap();
        let written = std::fs::metadata(&made).unwrap().modified().unwrap();
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        assert_eq!(std::fs::metadata(&made).unwrap().modified().unwrap(), written);
        let left: Vec<_> =
            std::fs::read_dir(out_dir.parent().unwrap()).unwrap().flatten().collect();
        assert_eq!(left.len(), 5, "{left:?}");
        let before = SystemTime::now() - Duration::from_secs(1);
        assert!(checked_after(&out_dir, before));
        assert!(!checked_after(&out_dir, SystemTime::now()), "a file written since");
    }

    #[test]
    fn a_run_is_the_same_as_another_but_for_its_directory() {
        let out_dir = ran("run", "fn made() {}");
        let other = ran("run-other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert!(same_run(&out_dir, &other));
        std::fs::write(other.join("made.rs"), "fn made_before() {}").unwrap();
        assert!(!same_run(&out_dir, &other));
    }

    #[test]
    fn a_rerun_is_compared_with_what_the_other_run_left() {
        let out_dir = ran("matches", "fn made() {}");
        let other = ran("matches-other", "fn made() {}");
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(rerun_matches(&out_dir, &other), Ok(()));
        std::fs::write(other.join("made.rs"), "fn made_for_the_load() {}").unwrap();
        assert_eq!(rerun_matches(&out_dir, &other), Err("wrote something else".to_owned()));
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
        let env =
            remembered(workspace).unwrap().over([("CARGO_PKG_VERSION".into(), "1.0.0".into())]);
        remember(workspace.to_owned(), env);
        ran.env_names.push("CARGO_PKG_VERSION".into());
        std::fs::write(&at, serde_json::to_vec(&ran).unwrap()).unwrap();
        compiled(&out_dir, "build.rs\n\n# env-dep:CARGO_PKG_VERSION=1.0.0");
        assert!(compiled_from_what_is_there(&out_dir, workspace));
        compiled(&out_dir, "build.rs\n\n# env-dep:CARGO_PKG_VERSION=2.0.0");
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
    }

    /// cargo's second layout of its build directory keeps what a script printed in
    /// `run/stdout`, not in `output`.
    #[test]
    fn a_run_in_the_second_layout_is_read_from_run_stdout() {
        let out_dir = ran("second", "fn made() {}");
        let run_dir = out_dir.parent().unwrap();
        std::fs::create_dir(run_dir.join("run")).unwrap();
        std::fs::rename(run_dir.join("output"), run_dir.join("run/stdout")).unwrap();
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(printed(&out_dir), Some(run_dir.join("run/stdout")));
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        let other = ran("second-other", "fn made() {}");
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(rerun_matches(&out_dir, &other), Ok(()));
        assert!(same_run(&out_dir, &other));
        assert!(in_out_dir(Path::new("/t/debug/build/app/0123/out/made.rs")));
        assert!(in_out_dir(Path::new("/t/debug/build/app-0123/out/made.rs")));
        assert!(!in_out_dir(Path::new("/t/debug/deps/app-0123.d")));
    }

    #[test]
    fn a_rerun_that_writes_something_else_is_told() {
        let out_dir = ran("other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made_before() {}").unwrap();
        assert_eq!(rerun_is_same(&out_dir), Err("wrote something else".to_owned()));
        assert_eq!(std::fs::read(out_dir.join("made.rs")).unwrap(), b"fn made_before() {}");
    }
}
