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
    sync::atomic::{AtomicU64, Ordering},
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
    let places = Places::of(&env);
    let key = |places: Option<&Places>| {
        out_dir.as_ref().and_then(|_| run_key(&real, &cwd, &args, &env, places))
    };
    let (shared, here) = (key(Some(&places)), key(None));
    if let Some(run_dir) = out_dir.as_deref().and_then(Path::parent) {
        let ran = Ran { real: real.clone(), cwd: cwd.clone(), env };
        // Without it the script is not run again by the daemon, and cargo builds.
        let _ = std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran)?);
    }
    let mut script = Command::new(&real);
    script.args(&args).env_remove(REAL);
    ra_jit_store::compiles_against(RUNS);
    let store =
        here.and_then(|_| ra_jit_store::Store::global_or(super::daemon::store().as_deref()));
    let (Some(shared), Some(here), Some(store), Some(out_dir)) = (shared, here, store, out_dir)
    else {
        return become_it(&mut script);
    };
    // A run another checkout or target directory kept, or, when what it left names where it
    // ran in a file that is no text, one of this place.
    let kept = match store.get(&shared).and_then(|it| Kept::read(&it)) {
        Some(kept) => Some(kept.placed(&places)),
        None => store.get(&here).and_then(|it| Kept::read(&it)),
    };
    if let Some(kept) = kept.filter(|it| it.still(&cwd)) {
        kept.restore(&out_dir)?;
        // Cargo keeps what a script printed to stderr beside its `OUT_DIR`.
        if std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!("ra-jit: a run the store kept");
        }
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
        match kept.unplaced(&places) {
            Some(unplaced) => store.insert(&shared, &unplaced.bytes()),
            None => store.insert(&here, &kept.bytes()),
        }
        store.flush();
    }
    std::process::exit(ran.status.code().unwrap_or(1))
}

/// What the store's runs of build scripts are kept under in place of a toolchain: what a run
/// prints and writes is told by the script, which its key has.
const RUNS: &str = "build script runs 1";

/// The variables of a run that change nothing it prints or writes: the jobs it may start, and
/// the directories the shell that ran cargo was in, which cargo leaves as they were (it runs
/// the script in its package's directory).
const UNREAD: [&str; 6] = ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS", "NUM_JOBS", "PWD", "OLDPWD"];

/// The most a run may leave for the store to keep it.
const MOST_KEPT: usize = 256 << 20;

/// The key of a run of the script `real` in `cwd` with `args` and `env`. With `places`, what
/// names the checkout and the target directory names them by what stands for them, so a run
/// in another checkout or target directory has the same key. `None` when the script cannot
/// be read.
fn run_key(
    real: &Path,
    cwd: &Path,
    args: &[OsString],
    env: &[(String, String)],
    places: Option<&Places>,
) -> Option<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    field(RUNS.as_bytes());
    let text = |bytes: &[u8]| match places {
        Some(places) => places.unplace(bytes),
        None => bytes.to_vec(),
    };
    if places.is_some() {
        field(b"places apart");
    }
    field(&Sha256::digest(std::fs::read(real).ok()?));
    field(&text(cwd.as_os_str().as_encoded_bytes()));
    for arg in args {
        field(&text(arg.as_encoded_bytes()));
    }
    let mut env: Vec<&(String, String)> =
        env.iter().filter(|(name, _)| !UNREAD.contains(&name.as_str())).collect();
    env.sort();
    for (name, value) in env {
        field(name.as_bytes());
        field(&text(value.as_bytes()));
    }
    Some(hash.finalize().into())
}

/// The places of one checkout a run of a build script names, each with what stands for it in
/// what the store keeps: the target directory its `OUT_DIR` is below, up to cargo's `build`
/// directory, and its package's directory, where cargo runs it. No path holds a NUL.
#[derive(Debug)]
struct Places(Vec<(Vec<u8>, &'static [u8])>);

impl Places {
    const TARGET: &[u8] = b"\0TARGET\0";
    const PACKAGE: &[u8] = b"\0PACKAGE\0";

    fn of(env: &[(String, String)]) -> Places {
        let var = |name: &str| {
            let value = env.iter().find(|it| it.0 == name)?.1.trim_end_matches(['/', '\\']);
            (!value.is_empty()).then_some(value)
        };
        let mut places = Vec::new();
        if let Some(out) = var("OUT_DIR")
            && let Some(at) = out.replace('\\', "/").rfind("/build/")
            && at > 0
        {
            places.push((out.as_bytes()[..at].to_vec(), Self::TARGET));
        }
        if let Some(package) = var("CARGO_MANIFEST_DIR") {
            places.push((package.as_bytes().to_vec(), Self::PACKAGE));
        }
        // The longer first: the target directory is often in the package's.
        places.sort_by_key(|it| std::cmp::Reverse(it.0.len()));
        Places(places)
    }

    /// `bytes` with what stands for each place in place of it.
    fn unplace(&self, bytes: &[u8]) -> Vec<u8> {
        let mut bytes = bytes.to_vec();
        for (place, mark) in &self.0 {
            bytes = replaced(&bytes, place, mark);
        }
        bytes
    }

    /// `bytes` with each place where [`Places::unplace`] put what stands for it.
    fn place(&self, bytes: &[u8]) -> Vec<u8> {
        let mut bytes = bytes.to_vec();
        for (place, mark) in &self.0 {
            bytes = replaced(&bytes, mark, place);
        }
        bytes
    }

    /// Whether `bytes` name a place.
    fn named_in(&self, bytes: &[u8]) -> bool {
        self.0.iter().any(|(place, _)| holds(bytes, place))
    }

    /// Whether `bytes` hold what stands for a place, and so could not be told from bytes that
    /// named it.
    fn marked(&self, bytes: &[u8]) -> bool {
        self.0.iter().any(|(_, mark)| holds(bytes, mark))
    }
}

fn holds(bytes: &[u8], part: &[u8]) -> bool {
    !part.is_empty() && bytes.windows(part.len()).any(|it| it == part)
}

/// `bytes` with every `from` in them `to`.
fn replaced(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while !from.is_empty()
        && let Some(at) = rest.windows(from.len()).position(|it| it == from)
    {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(to);
        rest = &rest[at + from.len()..];
    }
    out.extend_from_slice(rest);
    out
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

    /// It with what stands for each place in place of it, for a run in any checkout or target
    /// directory to stand for. `None` when a file that is no text names a place, whose bytes
    /// a place of another length would break, or when something it holds could not be told
    /// from what stands for one.
    fn unplaced(&self, places: &Places) -> Option<Kept> {
        let texts = [&self.stdout, &self.stderr].into_iter();
        let files = self.files.iter().map(|it| &it.2);
        let paths = self.watched.iter().map(|it| it.0.as_bytes());
        let names = self.files.iter().map(|it| it.0.as_bytes());
        if texts
            .chain(files)
            .map(Vec::as_slice)
            .chain(paths)
            .chain(names)
            .any(|it| places.marked(it))
        {
            return None;
        }
        let mut files = Vec::with_capacity(self.files.len());
        for (name, executable, bytes) in &self.files {
            let bytes = match places.named_in(bytes) {
                true if std::str::from_utf8(bytes).is_err() => return None,
                true => places.unplace(bytes),
                false => bytes.clone(),
            };
            files.push((name.clone(), *executable, bytes));
        }
        let text = |it: &str| String::from_utf8(places.unplace(it.as_bytes())).ok();
        let watched = self.watched.iter().map(|(path, digest)| Some((text(path)?, digest.clone())));
        Some(Kept {
            stdout: places.unplace(&self.stdout),
            stderr: places.unplace(&self.stderr),
            files,
            watched: watched.collect::<Option<_>>()?,
        })
    }

    /// What [`Kept::unplaced`] made of a run, with the places of this one.
    fn placed(self, places: &Places) -> Kept {
        let text = |it: &str| String::from_utf8_lossy(&places.place(it.as_bytes())).into_owned();
        let files = self.files.into_iter().map(|(name, executable, bytes)| {
            let bytes = if places.marked(&bytes) { places.place(&bytes) } else { bytes };
            (name, executable, bytes)
        });
        Kept {
            stdout: places.place(&self.stdout),
            stderr: places.place(&self.stderr),
            files: files.collect(),
            watched: self
                .watched
                .iter()
                .map(|(path, digest)| (text(path), digest.clone()))
                .collect(),
        }
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

/// Runs the script that wrote `out_dir` again, as cargo ran it but in a copy of the directory:
/// what cargo's holds is read by the compilers of a build under way, and cargo compiles the
/// package again when a file there is newer than it was. `Ok` when the script printed (the
/// lines cargo reads) and wrote what it did then; `Err` is what is different, which a script
/// that writes the name of its directory into a file always is.
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
    let written = |dir: &Path| tree_digest(dir, Look::Contents).ok();
    directives_named(&ours, names, named) == directives(&theirs)
        && written(out_dir).is_some_and(|it| written(other) == Some(it))
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

/// The [`directives`] of what a script printed that ran in the directory `names`, with its name
/// replaced by `named`, as cargo does when it moves a build directory.
fn directives_named(printed: &[u8], names: &str, named: &str) -> Vec<String> {
    directives(printed).into_iter().map(|line| line.replace(names, named)).collect()
}

/// Runs the script that wrote `out_dir` again as [`rerun_is_same`] does, in a copy of `other`,
/// and compares what it prints and writes with what the run of the same script that left
/// `other` did: another build of the package, which ran it with the same source and inputs.
///
/// `Err` when the directory cannot be copied so that nothing the script writes reaches
/// `other`: a link that leads out of it, or a file that is no file, directory or link. The run
/// is then not told to be the same, and is the one cargo is left to make.
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
    // A process runs more than one at once, each in its own directory.
    static COPIES: AtomicU64 = AtomicU64::new(0);
    let copy = COPIES.fetch_add(1, Ordering::Relaxed);
    let again_dir = run_dir.join(format!("ra-jit-out-{}-{copy}", std::process::id()));
    remove_copy(&again_dir);
    // Cargo runs a script again in the directory it ran it in, with what that run left there,
    // and a script that keeps what it built there, as a tool's output or a record of what it
    // was built from, finds it and does not build it again.
    let copied = copy_tree(other, &again_dir);
    let same = copied
        .map_err(|it| format!("{}: {it}", other.display()))
        .and_then(|()| rerun_into(&ran, &again_dir, other, &printed));
    remove_copy(&again_dir);
    same
}

/// Copies the directory `from` to `to`, which is not there yet, as a script that runs in it
/// finds `from`: every file and directory with the times and permissions it has, so that a
/// script that tests how old one is, or whether it can write to it, does what it does in `from`.
/// A link leads where it led, from `to`: one that leads to what is below `from` leads to the
/// same below `to`, so that a script that writes through it writes in the copy. It is an error
/// for one that leads anywhere else (out of `from`, nowhere, or round in a circle), because
/// writing through that is writing in what is not the copy. A file that is no file, directory
/// or link is an error too: a pipe is waited for.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    copy_below(from, to, &std::fs::canonicalize(from)?, to)
}

/// [`copy_tree`] for the directory `from` below `root`, which is where `to_root` is in the copy.
fn copy_below(from: &Path, to: &Path, root: &Path, to_root: &Path) -> std::io::Result<()> {
    let metadata = std::fs::metadata(from)?;
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_below(&source, &target, root, to_root)?;
        } else if kind.is_symlink() {
            let led_to = std::fs::canonicalize(&source)?;
            let Ok(inside) = led_to.strip_prefix(root) else {
                let it = format!("{}: a link that leads out of the directory", source.display());
                return Err(std::io::Error::other(it));
            };
            copy_link(&to_root.join(inside), &target)?;
        } else if kind.is_file() {
            copy_file(&source, &target, &entry.metadata()?)?;
        } else {
            let it = format!("{}: neither a file, a directory nor a link", source.display());
            return Err(std::io::Error::other(it));
        }
    }
    // After what is in it, which makes it newer, and what it cannot be written to for.
    let directory = std::fs::File::open(to)?;
    directory.set_times(times_of(&metadata)?)?;
    std::fs::set_permissions(to, metadata.permissions())
}

/// Copies the file `from`, which `metadata` is of, to `to`, with its times and permissions. A
/// copy has the time of the copy, and a script, or a tool it runs, may read when a file was
/// written. They are set through the handle that wrote the copy, so a file with no permission to
/// write gets them too, and without opening the copy again, which costs 2.5 ms a file on macOS
/// (125 files, 10 runs).
fn copy_file(from: &Path, to: &Path, metadata: &std::fs::Metadata) -> std::io::Result<()> {
    let mut copy = std::fs::File::create_new(to)?;
    std::io::copy(&mut std::fs::File::open(from)?, &mut copy)?;
    copy.set_times(times_of(metadata)?)?;
    copy.set_permissions(metadata.permissions())
}

/// When what `metadata` is of was written and read.
fn times_of(metadata: &std::fs::Metadata) -> std::io::Result<std::fs::FileTimes> {
    let mut times = std::fs::FileTimes::new().set_modified(metadata.modified()?);
    if let Ok(accessed) = metadata.accessed() {
        times = times.set_accessed(accessed);
    }
    Ok(times)
}

#[cfg(unix)]
fn copy_link(link: &Path, at: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(link, at)
}

/// Links need a privilege on Windows: the run is the one cargo is left to make.
#[cfg(not(unix))]
fn copy_link(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("a link in a build script's directory"))
}

/// Removes the copy `dir` of a directory, which has the directories that cannot be written to
/// that the original has.
fn remove_copy(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut pending = vec![dir.to_owned()];
        while let Some(below) = pending.pop() {
            let Ok(metadata) = std::fs::symlink_metadata(&below) else { continue };
            if !metadata.is_dir() {
                continue;
            }
            let mode = metadata.permissions().mode();
            if mode & 0o700 != 0o700 {
                _ = std::fs::set_permissions(&below, std::fs::Permissions::from_mode(mode | 0o700));
            }
            pending.extend(
                std::fs::read_dir(&below).into_iter().flatten().flatten().map(|it| it.path()),
            );
        }
    }
    _ = std::fs::remove_dir_all(dir);
}

/// Runs the script in `again_dir`, the copy of `out_dir` (the directory the lines `printed` are
/// of), and tells what is different.
fn rerun_into(ran: &Ran, again_dir: &Path, out_dir: &Path, printed: &[u8]) -> Result<(), String> {
    let (Some(named), Some(names)) = (out_dir.to_str(), again_dir.to_str()) else {
        return Err("has a directory whose name is not text".to_owned());
    };
    let look = |dir: &Path, how: Look| {
        tree_digest(dir, how).map_err(|it| format!("{}: {it}", dir.display()))
    };
    // What the script finds, before it runs: what its writes are compared with, and not what
    // is in either directory after them, which a write through a link that was missed changes
    // in both. Cargo's is looked at too, in a way that finds a write of the same bytes.
    let before = look(again_dir, Look::Contents)?;
    let untouched = look(out_dir, Look::Times)?;
    let mut script = Command::new(&ran.real);
    script.env_clear().envs(ran.env.iter().map(|(name, value)| (name, value)));
    script.env("OUT_DIR", again_dir).current_dir(&ran.cwd).stdin(Stdio::null());
    let again = script.output().map_err(|it| it.to_string())?;
    if !again.status.success() {
        return Err(format!("failed ({})", again.status));
    }
    if directives_named(&again.stdout, names, named) != directives(printed) {
        return Err("printed something else".to_owned());
    }
    if look(again_dir, Look::Contents)? != before {
        return Err("wrote something else".to_owned());
    }
    // Cargo's own is as it was: nothing the script did reached it, and cargo did not write to it
    // meanwhile.
    if look(out_dir, Look::Times)? != untouched || look(out_dir, Look::Contents)? != before {
        return Err("found cargo's directory written to while it ran".to_owned());
    }
    Ok(())
}

/// What a hash of a tree is of.
#[derive(Clone, Copy, PartialEq)]
enum Look {
    /// The bytes of the files.
    Contents,
    /// The size and time written of the files and directories, with no read of a file: a file
    /// written again with the bytes it had is newer, and cargo builds the package again.
    Times,
}

/// A hash of the tree below `root`: the name of each file, directory and link, what it is, the
/// bytes of a file (or, with [`Look::Times`], the size and time written of it and of every
/// directory) and where a link leads, which is a path below `root` when it leads to what is
/// there and else the text of the link. A link is not followed, so that a link back to a
/// directory is no endless tree. It is an error when something there cannot be read, or is no
/// file, directory or link: a tree that was not read is not the same as another.
fn tree_digest(root: &Path, look: Look) -> std::io::Result<Vec<u8>> {
    let canonical = std::fs::canonicalize(root)?;
    let mut hash = Sha256::new();
    let mut pending = vec![root.to_owned()];
    // The time written, in nanoseconds from 1970 and before it.
    let nanos = |metadata: &std::fs::Metadata| -> std::io::Result<Vec<u8>> {
        let nanos = match metadata.modified()?.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(after) => after.as_nanos() as i128,
            Err(before) => -(before.duration().as_nanos() as i128),
        };
        Ok(nanos.to_le_bytes().to_vec())
    };
    if look == Look::Times {
        hash.update(nanos(&std::fs::metadata(root)?)?);
    }
    while let Some(below) = pending.pop() {
        let mut entries = std::fs::read_dir(&below)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|it| it.file_name());
        for entry in entries {
            let path = entry.path();
            let name = path.strip_prefix(root).unwrap_or(&path);
            let kind = entry.file_type()?;
            let (tag, what) = if kind.is_dir() {
                pending.push(path.clone());
                let at = if look == Look::Times { nanos(&entry.metadata()?)? } else { Vec::new() };
                (b'd', at)
            } else if kind.is_symlink() {
                (b'l', led_to(&path, &canonical)?)
            } else if kind.is_file() && look == Look::Times {
                let metadata = entry.metadata()?;
                (b'f', [metadata.len().to_le_bytes().to_vec(), nanos(&metadata)?].concat())
            } else if kind.is_file() {
                (b'f', file_digest(&path)?)
            } else {
                let it = format!("{}: neither a file, a directory nor a link", path.display());
                return Err(std::io::Error::other(it));
            };
            for part in [name.as_os_str().as_encoded_bytes(), &what] {
                hash.update((part.len() as u64).to_le_bytes());
                hash.update(part);
            }
            hash.update([tag]);
        }
    }
    Ok(hash.finalize().to_vec())
}

/// Where the link `link` leads: the path below `root` it ends at, or else the text of the link.
fn led_to(link: &Path, root: &Path) -> std::io::Result<Vec<u8>> {
    let ends = std::fs::canonicalize(link).ok();
    Ok(match ends.as_ref().and_then(|it| it.strip_prefix(root).ok()) {
        Some(inside) => [&b"in:"[..], inside.as_os_str().as_encoded_bytes()].concat(),
        None => {
            let text = std::fs::read_link(link)?;
            [&b"out:"[..], text.as_os_str().as_encoded_bytes()].concat()
        }
    })
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
    use super::*;

    /// A run directory as cargo and `ra-jit build-script` leave one, for a script that
    /// writes `wrote` and prints where.
    fn ran(name: &str, wrote: &str) -> PathBuf {
        let lines =
            "#!/bin/sh\nprintf %s \"$WROTE\" > \"$OUT_DIR/made.rs\"\necho \"cargo:in=$OUT_DIR\"\n";
        ran_script(name, lines, wrote)
    }

    /// A run directory as [`ran`] leaves one, for the script `lines`.
    fn ran_script(name: &str, lines: &str, wrote: &str) -> PathBuf {
        let run_dir =
            std::env::temp_dir().join(format!("ra-jit-script-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&run_dir);
        let out_dir = run_dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let real = run_dir.join("script");
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
        // Taken before the check, which a machine that is busy can take more than a second over.
        let before = SystemTime::now() - Duration::from_secs(1);
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        assert_eq!(std::fs::metadata(&made).unwrap().modified().unwrap(), written);
        let left: Vec<_> =
            std::fs::read_dir(out_dir.parent().unwrap()).unwrap().flatten().collect();
        assert_eq!(left.len(), 5, "{left:?}");
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

    #[test]
    fn a_run_in_another_checkout_has_its_key_and_what_it_left_names_that_checkout() {
        let root =
            std::env::temp_dir().join(format!("ra-jit-script-places-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("script");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&real, "a script").unwrap();
        let checkout = |name: &str| {
            let at = root.join(name);
            let (package, out) = (at.join("pkg"), at.join("target/debug/build/pkg-0123/out"));
            std::fs::create_dir_all(&out).unwrap();
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(package.join("read.txt"), "one").unwrap();
            let env = vec![
                ("CARGO_MANIFEST_DIR".to_owned(), package.to_str().unwrap().to_owned()),
                ("OUT_DIR".to_owned(), out.to_str().unwrap().to_owned()),
                ("PATH".to_owned(), "/bin".to_owned()),
                ("PWD".to_owned(), at.to_str().unwrap().to_owned()),
            ];
            (package, out, env)
        };
        let (package, out, env) = checkout("one");
        let (other_package, other_out, other_env) = checkout("other");
        let (places, other_places) = (Places::of(&env), Places::of(&other_env));
        let key = |cwd: &Path, env: &[(String, String)], places: Option<&Places>| {
            run_key(&real, cwd, &[], env, places)
        };
        assert_eq!(
            key(&package, &env, Some(&places)),
            key(&other_package, &other_env, Some(&other_places))
        );
        assert_ne!(key(&package, &env, None), key(&other_package, &other_env, None));
        let named = |out: &Path, package: &Path| Kept {
            stdout: format!(
                "cargo:rustc-link-search=native={}\ncargo:rerun-if-changed={}\n",
                out.display(),
                package.join("read.txt").display()
            )
            .into_bytes(),
            stderr: Vec::new(),
            files: vec![
                (
                    "made.rs".to_owned(),
                    false,
                    format!("include!(\"{}/x.rs\");", out.display()).into_bytes(),
                ),
                ("lib.a".to_owned(), false, vec![0xff, 0, 1]),
            ],
            watched: vec![(
                package.join("read.txt").to_str().unwrap().to_owned(),
                watched_digest(&package.join("read.txt")),
            )],
        };
        let unplaced = named(&out, &package).unplaced(&places).unwrap();
        let read = Kept::read(&unplaced.bytes()).unwrap();
        let placed = read.placed(&other_places);
        assert_eq!(placed, named(&other_out, &other_package));
        assert!(placed.still(&other_package));
        let mut binary = named(&out, &package);
        binary.files[1].2 = [&[0xff][..], out.to_str().unwrap().as_bytes()].concat();
        assert_eq!(binary.unplaced(&places), None, "a file that is no text naming the place");
        let _ = std::fs::remove_dir_all(&root);
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
        let run_dir = out_dir.parent().unwrap();
        let ran: Ran = serde_json::from_slice(&std::fs::read(run_dir.join(RAN)).unwrap()).unwrap();
        for _ in 0..times {
            let mut script = Command::new(&ran.real);
            script.env_clear().envs(ran.env.iter().map(|(name, value)| (name, value)));
            let printed = script.current_dir(&ran.cwd).output().unwrap();
            assert!(printed.status.success());
            std::fs::write(run_dir.join("output"), printed.stdout).unwrap();
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
        assert_eq!(rerun, "cargo:rerun-if-changed=input.txt\ncargo:rustc-env=INPUT=one\n");
        // A run into a directory with nothing in it would build again and say so, but a run
        // in a copy of cargo's is the same.
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        let left: Vec<_> = std::fs::read_dir(run_dir).unwrap().flatten().collect();
        assert_eq!(left.len(), 6, "{left:?}");
        // What cargo's directory has is what it had: the run was in a copy.
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
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        let other = ran_script("says-more-other", BUILDS_ONCE, "");
        std::fs::write(other.parent().unwrap().join("input.txt"), "one").unwrap();
        run_as_cargo_does(&other, 2);
        assert!(same_run(&out_dir, &other));
        assert_eq!(rerun_matches(&out_dir, &other), Ok(()));
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
        assert_eq!(
            directives_named(printed, "/t/copy", "/t/out"),
            ["cargo:rustc-link-search=/t/out/lib", "cargo::rerun-if-changed=/t/out/a"]
        );
    }

    #[test]
    fn a_copy_of_a_directory_has_the_times_and_permissions_of_what_it_is_of() {
        use std::os::unix::fs::PermissionsExt as _;
        let from = std::env::temp_dir().join(format!("ra-jit-copy-from-{}", std::process::id()));
        let to = std::env::temp_dir().join(format!("ra-jit-copy-to-{}", std::process::id()));
        for dir in [&from, &to] {
            remove_copy(dir);
        }
        std::fs::create_dir_all(from.join("deep/er")).unwrap();
        let tool = from.join("deep/er/tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&tool, from.join("inside")).unwrap();
        std::os::unix::fs::symlink("deep/../deep/er/tool", from.join("round")).unwrap();
        let (old, older) = (SystemTime::now() - Duration::from_secs(3600), SystemTime::UNIX_EPOCH);
        set_written(&tool, old);
        set_written(&from.join("deep/er"), older + Duration::from_secs(86400));
        set_written(&from.join("deep"), older + Duration::from_secs(2 * 86400));
        std::fs::set_permissions(from.join("deep"), std::fs::Permissions::from_mode(0o550))
            .unwrap();
        set_written(&from, old - Duration::from_secs(60));
        copy_tree(&from, &to).unwrap();
        let (was, is) = (
            std::fs::metadata(&tool).unwrap(),
            std::fs::metadata(to.join("deep/er/tool")).unwrap(),
        );
        assert_eq!(is.permissions().mode() & 0o777, 0o755);
        assert_eq!(was.modified().unwrap(), is.modified().unwrap());
        for dir in ["", "deep", "deep/er"] {
            let (was, is) = (
                std::fs::metadata(from.join(dir)).unwrap(),
                std::fs::metadata(to.join(dir)).unwrap(),
            );
            assert_eq!(was.modified().unwrap(), is.modified().unwrap(), "{dir:?}");
            assert_eq!(was.permissions().mode(), is.permissions().mode(), "{dir:?}");
        }
        // Links to what is below the directory lead to the same below the copy, by whatever way
        // they led there.
        let copied = std::fs::canonicalize(to.join("deep/er/tool")).unwrap();
        for link in ["inside", "round"] {
            assert_eq!(std::fs::canonicalize(to.join(link)).unwrap(), copied, "{link}");
            assert!(std::fs::read_link(to.join(link)).unwrap().starts_with(&to), "{link}");
        }
        assert!(copy_tree(&from, &to).is_err(), "the destination is not there yet");
        // One that leads out of it is not copied, however it is written.
        remove_copy(&to);
        std::os::unix::fs::symlink("/bin/sh", from.join("outside")).unwrap();
        let outside = copy_tree(&from, &to).unwrap_err();
        assert!(outside.to_string().contains("leads out of the directory"), "{outside}");
        for dir in [&from, &to] {
            remove_copy(dir);
        }
    }

    #[test]
    fn a_rerun_that_writes_something_else_is_told() {
        let out_dir = ran("other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made_before() {}").unwrap();
        assert_eq!(rerun_is_same(&out_dir), Err("wrote something else".to_owned()));
        assert_eq!(std::fs::read(out_dir.join("made.rs")).unwrap(), b"fn made_before() {}");
    }

    /// The copies a rerun made beside `out_dir` and did not remove.
    fn copies_left(out_dir: &Path) -> Vec<String> {
        let names = std::fs::read_dir(out_dir.parent().unwrap()).unwrap().flatten();
        let names = names.filter_map(|it| it.file_name().into_string().ok());
        names.filter(|it| it.starts_with("ra-jit-out-")).collect()
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

    #[test]
    fn a_directory_of_the_copy_is_as_old_as_cargos_is() {
        let out_dir = makes_again_when_older("dir-time", "cache");
        let cache = out_dir.join("cache");
        std::fs::create_dir(&cache).unwrap();
        set_written(&cache, SystemTime::now() - Duration::from_secs(3600));
        let result = rerun_is_same(&out_dir);
        let kept = std::fs::read_to_string(out_dir.join("data")).unwrap();
        // Cargo runs the script in the directory itself, and finds the cache older than the input.
        run_as_cargo_does(&out_dir, 1);
        assert_eq!(std::fs::read_to_string(out_dir.join("data")).unwrap(), "new");
        assert_eq!(kept, "old", "the run was in a copy");
        assert_eq!(result, Err("wrote something else".to_owned()));
    }

    #[test]
    fn a_read_only_file_of_the_copy_is_as_old_as_cargos_is() {
        use std::os::unix::fs::PermissionsExt as _;
        let out_dir = makes_again_when_older("file-time", "stamp");
        let stamp = out_dir.join("stamp");
        std::fs::write(&stamp, "").unwrap();
        set_written(&stamp, SystemTime::now() - Duration::from_secs(3600));
        std::fs::set_permissions(&stamp, std::fs::Permissions::from_mode(0o444)).unwrap();
        let result = rerun_is_same(&out_dir);
        assert_eq!(std::fs::read_to_string(out_dir.join("data")).unwrap(), "old");
        assert_eq!(result, Err("wrote something else".to_owned()));
    }

    #[test]
    fn a_directory_of_the_copy_has_the_permissions_of_cargos_and_is_removed_with_them() {
        use std::os::unix::fs::PermissionsExt as _;
        if running_as_root() {
            return;
        }
        let lines = "#!/bin/sh\nif [ ! -w \"$OUT_DIR/sealed\" ]; then printf sealed > \"$OUT_DIR/saw\"; fi\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("dir-mode", lines, "");
        let sealed = out_dir.join("sealed");
        std::fs::create_dir(&sealed).unwrap();
        std::fs::write(sealed.join("inside"), "x").unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = rerun_is_same(&out_dir);
        let copies = copies_left(&out_dir);
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Cargo's script finds the directory it cannot write to, and says so in a file.
        assert_eq!(result, Err("wrote something else".to_owned()));
        assert!(!out_dir.join("saw").exists());
        assert!(copies.is_empty(), "{copies:?}");
    }

    /// A script that writes through `alias` in its `OUT_DIR`.
    const WRITES_THROUGH_ALIAS: &str =
        "#!/bin/sh\nprintf %s new > \"$OUT_DIR/alias\"\necho \"cargo:in=$OUT_DIR\"\n";

    #[test]
    fn a_link_that_leads_out_of_the_directory_is_not_written_through() {
        for (name, relative) in [("relative", true), ("absolute", false)] {
            let out_dir = ran_script(&format!("link-out-{name}"), WRITES_THROUGH_ALIAS, "");
            let state = out_dir.parent().unwrap().join("state");
            std::fs::write(&state, "old").unwrap();
            let target = if relative { PathBuf::from("../state") } else { state.clone() };
            std::os::unix::fs::symlink(target, out_dir.join("alias")).unwrap();
            let result = rerun_is_same(&out_dir);
            let left = std::fs::read_to_string(&state).unwrap();
            assert!(result.is_err(), "{name}: {result:?}");
            assert_eq!(left, "old", "{name}: cargo's directory is not written to");
            assert_eq!(copies_left(&out_dir), Vec::<String>::new(), "{name}");
        }
    }

    #[test]
    fn a_link_that_leads_back_in_by_another_name_is_written_through_in_the_copy() {
        let out_dir = ran_script("link-back", WRITES_THROUGH_ALIAS, "");
        std::fs::write(out_dir.join("data"), "old").unwrap();
        // `../out/data` from the copy, which is beside `out`, is the data of cargo's directory.
        std::os::unix::fs::symlink("../out/data", out_dir.join("alias")).unwrap();
        let result = rerun_is_same(&out_dir);
        assert_eq!(std::fs::read_to_string(out_dir.join("data")).unwrap(), "old");
        assert_eq!(result, Err("wrote something else".to_owned()));
    }

    #[test]
    fn links_inside_the_directory_are_links_to_the_copy() {
        let lines = "#!/bin/sh\nfor name in alias absolute dir/f; do printf %s same > \"$OUT_DIR/$name\"; done\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("link-in", lines, "");
        std::fs::create_dir(out_dir.join("sub")).unwrap();
        let (data, f) = (out_dir.join("data"), out_dir.join("sub/f"));
        for file in [&data, &f] {
            std::fs::write(file, "same").unwrap();
            set_written(file, SystemTime::now() - Duration::from_secs(3600));
        }
        std::os::unix::fs::symlink("data", out_dir.join("alias")).unwrap();
        std::os::unix::fs::symlink(&data, out_dir.join("absolute")).unwrap();
        std::os::unix::fs::symlink("sub", out_dir.join("dir")).unwrap();
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        for file in [&data, &f] {
            let written = std::fs::metadata(file).unwrap().modified().unwrap();
            assert!(written < SystemTime::now() - Duration::from_secs(3000), "{file:?}");
        }
        assert!(copies_left(&out_dir).is_empty());
    }

    /// The result of a rerun of a script that does nothing, in a directory with `link` leading
    /// to `target` in it.
    fn rerun_with_link(name: &str, link: &str, target: &str) -> Result<(), String> {
        let out_dir = ran_script(name, "#!/bin/sh\necho \"cargo:in=$OUT_DIR\"\n", "");
        std::os::unix::fs::symlink(target, out_dir.join(link)).unwrap();
        let result = rerun_is_same(&out_dir);
        assert!(copies_left(&out_dir).is_empty());
        result
    }

    #[test]
    fn a_link_that_leads_nowhere_or_in_a_circle_is_not_run_through() {
        assert!(rerun_with_link("link-gone", "gone", "nowhere").is_err());
        let out_dir = ran_script("link-circle", "#!/bin/sh\necho \"cargo:in=$OUT_DIR\"\n", "");
        std::os::unix::fs::symlink("b", out_dir.join("a")).unwrap();
        std::os::unix::fs::symlink("a", out_dir.join("b")).unwrap();
        assert!(rerun_is_same(&out_dir).is_err());
        assert!(copies_left(&out_dir).is_empty());
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

    #[test]
    fn what_the_script_left_is_compared_with_what_was_there_before_it_ran() {
        // A script that writes where cargo's directory is by another way than its `OUT_DIR`
        // writes the same in both, and the two are the same after it.
        let lines = "#!/bin/sh\nprintf %s new > \"$LEAK\"\nprintf %s new > \"$OUT_DIR/data\"\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("baseline", lines, "");
        let run_dir = out_dir.parent().unwrap();
        std::fs::write(out_dir.join("data"), "old").unwrap();
        let mut ran: Ran =
            serde_json::from_slice(&std::fs::read(run_dir.join(RAN)).unwrap()).unwrap();
        ran.env.push(("LEAK".to_owned(), out_dir.join("data").to_str().unwrap().to_owned()));
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        assert!(rerun_is_same(&out_dir).is_err());
    }

    #[test]
    fn cargos_directory_is_not_written_to_even_with_what_it_has() {
        // A script that finds cargo's directory by another way than its `OUT_DIR` and writes
        // what is there again leaves the bytes, and makes the file newer than what rustc read
        // of it: cargo builds the package again.
        let lines = "#!/bin/sh\nprintf %s old > \"$LEAK\"\necho \"cargo:in=$OUT_DIR\"\n";
        let out_dir = ran_script("touched", lines, "");
        let run_dir = out_dir.parent().unwrap();
        let data = out_dir.join("data");
        std::fs::write(&data, "old").unwrap();
        set_written(&data, SystemTime::now() - Duration::from_secs(3600));
        let mut ran: Ran =
            serde_json::from_slice(&std::fs::read(run_dir.join(RAN)).unwrap()).unwrap();
        ran.env.push(("LEAK".to_owned(), data.to_str().unwrap().to_owned()));
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        let result = rerun_is_same(&out_dir);
        let written = std::fs::metadata(&data).unwrap().modified().unwrap();
        assert!(written > SystemTime::now() - Duration::from_secs(60), "the script wrote it");
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn a_line_that_is_not_text_is_not_a_line_cargo_reads_in_either_run() {
        let out_dir = ran_script("utf8", "#!/bin/sh\nprintf 'cargo:rustc-env=KEY=\\377\\n'\n", "");
        std::fs::write(out_dir.parent().unwrap().join("output"), "cargo:rustc-env=KEY=\u{fffd}\n")
            .unwrap();
        let result = rerun_is_same(&out_dir);
        assert!(result.is_err(), "cargo ignores the new line and has no KEY: {result:?}");
        assert!(copies_left(&out_dir).is_empty());
        // The same of two runs that are compared.
        let (ours, theirs) = (ran("utf8-ours", ""), ran("utf8-theirs", ""));
        let print =
            |dir: &Path, bytes: &[u8]| std::fs::write(printed(dir).unwrap(), bytes).unwrap();
        print(&ours, b"cargo:rustc-env=KEY=\xff\n");
        print(&theirs, "cargo:rustc-env=KEY=\u{fffd}\n".as_bytes());
        assert!(!same_run(&ours, &theirs));
        assert!(!same_run(&theirs, &ours));
        // Cargo reads neither line.
        print(&theirs, b"cargo:rustc-env=KEY=\xfe\n");
        assert!(same_run(&ours, &theirs));
    }

    #[test]
    fn a_directory_that_is_missing_is_an_error_and_leaves_no_copy() {
        let out_dir = ran("missing", "");
        std::fs::remove_dir(&out_dir).unwrap();
        assert!(rerun_is_same(&out_dir).is_err());
        assert!(copies_left(&out_dir).is_empty());
    }

    #[test]
    fn a_script_that_fails_is_an_error_and_leaves_no_copy() {
        let out_dir = ran_script("failed", "#!/bin/sh\nexit 42\n", "");
        assert!(rerun_is_same(&out_dir).is_err());
        assert!(copies_left(&out_dir).is_empty());
    }

    #[test]
    fn a_file_that_is_not_a_file_is_not_copied_for_a_run() {
        let out_dir = ran("fifo", "");
        assert!(Command::new("mkfifo").arg(out_dir.join("pipe")).status().unwrap().success());
        // Opening a pipe to copy it waits for whoever writes to it.
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
        let run_dir = out_dir.parent().unwrap().to_owned();
        let ran: Ran = serde_json::from_slice(&std::fs::read(run_dir.join(RAN)).unwrap()).unwrap();
        let again = run_dir.join("ra-jit-out-cost");
        let phases = [
            "copy",
            "hash the copy",
            "the script",
            "hash the copy again",
            "hash cargo's",
            "remove the copy",
        ];
        let mut times: Vec<Vec<f64>> = vec![Vec::new(); phases.len() + 1];
        for _ in 0..10 {
            remove_copy(&again);
            let mut stamps = vec![Instant::now()];
            copy_tree(&out_dir, &again).unwrap();
            stamps.push(Instant::now());
            tree_digest(&again, Look::Contents).unwrap();
            stamps.push(Instant::now());
            let mut script = Command::new(&ran.real);
            assert!(script.env("OUT_DIR", &again).output().unwrap().status.success());
            stamps.push(Instant::now());
            tree_digest(&again, Look::Contents).unwrap();
            stamps.push(Instant::now());
            tree_digest(&out_dir, Look::Contents).unwrap();
            stamps.push(Instant::now());
            remove_copy(&again);
            stamps.push(Instant::now());
            for (at, pair) in stamps.windows(2).enumerate() {
                times[at].push((pair[1] - pair[0]).as_secs_f64() * 1000.0);
            }
            let began = Instant::now();
            rerun_is_same(&out_dir).unwrap();
            times[6].push(began.elapsed().as_secs_f64() * 1000.0);
        }
        for (what, mut times) in phases.iter().chain(&["the whole check, rerun_is_same"]).zip(times)
        {
            times.sort_by(f64::total_cmp);
            println!(
                "{what:32} min {:6.1} ms, median {:6.1} ms, max {:6.1} ms (10 runs)",
                times[0], times[5], times[9]
            );
        }
    }
}
