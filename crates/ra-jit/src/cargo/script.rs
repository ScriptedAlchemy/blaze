//! Build scripts of the workspace. Cargo runs one again when what it said it reads has
//! changed, or when anything in its package has if it said nothing, and then compiles the
//! package again whatever the script printed. The daemon runs the script the same way and
//! looks at what it printed and wrote: when that is what it was, the crates compiled with
//! it are still right, and the build stays the daemon's.
//!
//! To run a script as cargo does, the wrapper puts a few lines of shell where rustc linked
//! it. Cargo runs those, and they run `ra-jit build-script`, which writes down the
//! environment and the directory cargo ran it in and becomes the script.

use std::{
    ffi::OsString,
    io::Write as _,
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
    let key = out_dir.as_ref().and_then(|_| run_key(&real, &cwd, &args, &env));
    if let Some(run_dir) = out_dir.as_deref().and_then(Path::parent) {
        let ran = Ran { real: real.clone(), cwd: cwd.clone(), env };
        // Without it the script is not run again by the daemon, and cargo builds.
        let _ = std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran)?);
    }
    let mut script = Command::new(&real);
    script.args(&args).env_remove(REAL);
    ra_jit_store::compiles_against(RUNS);
    let store = key.and_then(|_| ra_jit_store::Store::global_or(super::daemon::store().as_deref()));
    let (Some(key), Some(store), Some(out_dir)) = (key, store, out_dir) else {
        return become_it(&mut script);
    };
    if let Some(kept) = store.get(&key).and_then(|it| Kept::read(&it)).filter(|it| it.still(&cwd)) {
        kept.restore(&out_dir)?;
        std::io::stdout().write_all(&kept.stdout)?;
        std::io::stderr().write_all(&kept.stderr)?;
        return Ok(());
    }
    let ran = script.stdin(Stdio::inherit()).output()?;
    std::io::stdout().write_all(&ran.stdout)?;
    std::io::stderr().write_all(&ran.stderr)?;
    if ran.status.success()
        && let Some(kept) = Kept::of(ran.stdout, ran.stderr, &out_dir, &cwd)
    {
        store.settled();
        store.insert(&key, &kept.bytes());
        store.flush();
    }
    std::process::exit(ran.status.code().unwrap_or(1))
}

/// What the store's runs of build scripts are kept under in place of a toolchain: what a run
/// prints and writes is told by the script, which its key has.
const RUNS: &str = "build script runs 1";

/// The variables of a run that change nothing it prints or writes: the jobs it may start.
const UNREAD: [&str; 4] = ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS", "NUM_JOBS"];

/// The most a run may leave for the store to keep it.
const MOST_KEPT: usize = 256 << 20;

/// The key of a run of the script `real` in `cwd` with `args` and `env`. `None` when the
/// script cannot be read.
fn run_key(
    real: &Path,
    cwd: &Path,
    args: &[OsString],
    env: &[(String, String)],
) -> Option<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    field(RUNS.as_bytes());
    field(&Sha256::digest(std::fs::read(real).ok()?));
    field(cwd.as_os_str().as_encoded_bytes());
    for arg in args {
        field(arg.as_encoded_bytes());
    }
    let mut env: Vec<&(String, String)> =
        env.iter().filter(|(name, _)| !UNREAD.contains(&name.as_str())).collect();
    env.sort();
    for (name, value) in env {
        field(name.as_bytes());
        field(value.as_bytes());
    }
    Some(hash.finalize().into())
}

/// What a run of a build script printed and left in its `OUT_DIR`, and what the files it said
/// it reads had in them: the run stands for another with the same key while they still do.
#[derive(Debug, PartialEq)]
struct Kept {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Each file below `OUT_DIR`, by its path there, whether it is executable, and its bytes.
    files: Vec<(String, bool, Vec<u8>)>,
    /// Each file or directory of `rerun-if-changed`, as the script named it, and the digest
    /// of what it had: nothing when it was not there.
    watched: Vec<(String, Vec<u8>)>,
}

impl Kept {
    /// The run that printed `stdout` and wrote `out_dir`, run in `cwd`. `None` when cargo would
    /// run it again for a change to any file of its package, which the key does not say, or
    /// when it left too much.
    fn of(stdout: Vec<u8>, stderr: Vec<u8>, out_dir: &Path, cwd: &Path) -> Option<Kept> {
        let printed = String::from_utf8_lossy(&stdout);
        let mut watched = Vec::new();
        let mut says_what_it_reads = false;
        for line in printed.lines() {
            let Some(line) = line.strip_prefix("cargo:") else { continue };
            let line = line.strip_prefix(':').unwrap_or(line);
            if let Some(path) = line.strip_prefix("rerun-if-changed=") {
                watched.push((path.to_owned(), watched_digest(&cwd.join(path))));
                says_what_it_reads = true;
            }
            says_what_it_reads |= line.starts_with("rerun-if-env-changed=");
        }
        if !says_what_it_reads {
            return None;
        }
        let mut files = Vec::new();
        let mut size = stdout.len() + stderr.len();
        let mut pending = vec![out_dir.to_owned()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).ok()?.flatten() {
                let path = entry.path();
                let kind = entry.file_type().ok()?;
                if kind.is_dir() {
                    pending.push(path);
                    continue;
                }
                let name = path.strip_prefix(out_dir).ok()?.to_str()?.to_owned();
                let bytes = std::fs::read(&path).ok()?;
                size += bytes.len();
                if size > MOST_KEPT {
                    return None;
                }
                files.push((name, is_executable(&entry.metadata().ok()?), bytes));
            }
        }
        files.sort();
        Some(Kept { stdout, stderr, files, watched })
    }

    /// Whether every file it said it reads has what it had.
    fn still(&self, cwd: &Path) -> bool {
        self.watched.iter().all(|(path, digest)| watched_digest(&cwd.join(path)) == *digest)
    }

    /// Writes the files it left into `out_dir`, as a run would.
    fn restore(&self, out_dir: &Path) -> anyhow::Result<()> {
        for (name, executable, bytes) in &self.files {
            let path = out_dir.join(name);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            if *executable {
                make_executable(&path)?;
            }
        }
        Ok(())
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut put = |bytes: &[u8]| {
            out.extend((bytes.len() as u64).to_le_bytes());
            out.extend(bytes);
        };
        put(RUNS.as_bytes());
        put(&self.stdout);
        put(&self.stderr);
        put(&(self.files.len() as u64).to_le_bytes());
        for (name, executable, bytes) in &self.files {
            put(name.as_bytes());
            put(&[u8::from(*executable)]);
            put(bytes);
        }
        put(&(self.watched.len() as u64).to_le_bytes());
        for (path, digest) in &self.watched {
            put(path.as_bytes());
            put(digest);
        }
        out
    }

    fn read(bytes: &[u8]) -> Option<Kept> {
        let rest = &mut &*bytes;
        let count = |it: &[u8]| Some(u64::from_le_bytes(it.try_into().ok()?) as usize);
        let text = |it: &[u8]| String::from_utf8(it.to_vec()).ok();
        if take(rest)? != RUNS.as_bytes() {
            return None;
        }
        let (stdout, stderr) = (take(rest)?.to_vec(), take(rest)?.to_vec());
        let mut files = Vec::new();
        for _ in 0..count(take(rest)?)? {
            let name = text(take(rest)?)?;
            let executable = take(rest)? == [1];
            files.push((name, executable, take(rest)?.to_vec()));
        }
        let mut watched = Vec::new();
        for _ in 0..count(take(rest)?)? {
            watched.push((text(take(rest)?)?, take(rest)?.to_vec()));
        }
        Some(Kept { stdout, stderr, files, watched })
    }
}

/// The next field of `rest`, which is its length and then its bytes.
fn take<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = usize::try_from(u64::from_le_bytes(rest.get(..8)?.try_into().ok()?)).ok()?;
    let field = rest.get(8..8usize.checked_add(len)?)?;
    *rest = &rest[8 + len..];
    Some(field)
}

/// What the file or directory at `path` has: its bytes', or its names' and files' digest.
/// Nothing when it is not there.
fn watched_digest(path: &Path) -> Vec<u8> {
    match std::fs::metadata(path) {
        Ok(it) if it.is_dir() => digest_of(path),
        Ok(_) => std::fs::read(path).map(|it| Sha256::digest(it).to_vec()).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
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
    let printed = printed(other).ok_or("a build script without a directory")?;
    let printed = std::fs::read(&printed).map_err(|it| {
        let name = printed.strip_prefix(other_dir).unwrap_or(&printed);
        format!("{}: {it}", name.display())
    })?;
    let again_dir = run_dir.join(format!("ra-jit-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&again_dir);
    std::fs::create_dir(&again_dir).map_err(|it| it.to_string())?;
    let same = rerun_into(&ran, &again_dir, other, &printed);
    let _ = std::fs::remove_dir_all(&again_dir);
    same
}

fn rerun_into(ran: &Ran, again_dir: &Path, out_dir: &Path, printed: &[u8]) -> Result<(), String> {
    let mut script = Command::new(&ran.real);
    script.env_clear().envs(ran.env.iter().map(|(name, value)| (name, value)));
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
        let ran = Ran { real, cwd: run_dir.clone(), env };
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

    #[test]
    fn a_kept_run_stands_for_another_while_what_it_reads_is_the_same() {
        let out_dir = ran("kept", "fn made() {}");
        let cwd = out_dir.parent().unwrap().to_owned();
        std::fs::create_dir_all(out_dir.join("deep")).unwrap();
        std::fs::write(out_dir.join("deep/made.rs"), "fn made() {}").unwrap();
        std::fs::write(cwd.join("read.txt"), "one").unwrap();
        let stdout = b"cargo::rerun-if-changed=read.txt\ncargo:rustc-cfg=kept\n".to_vec();
        let kept = Kept::of(stdout, b"a warning".to_vec(), &out_dir, &cwd).unwrap();
        let bytes = kept.bytes();
        assert_eq!(Kept::read(&bytes), Some(kept));
        let kept = Kept::read(&bytes).unwrap();
        assert!(kept.still(&cwd));
        let again = cwd.join("again");
        kept.restore(&again).unwrap();
        assert_eq!(std::fs::read_to_string(again.join("deep/made.rs")).unwrap(), "fn made() {}");
        std::fs::write(cwd.join("read.txt"), "two").unwrap();
        assert!(!kept.still(&cwd));
        assert!(Kept::of(b"cargo:rustc-cfg=x\n".to_vec(), Vec::new(), &out_dir, &cwd).is_none());
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
