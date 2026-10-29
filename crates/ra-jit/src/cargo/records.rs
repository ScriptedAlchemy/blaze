//! What cargo 1.99 keeps of a run of a build script, read and written as cargo does, so that a
//! run ra-jit makes in cargo's `OUT_DIR` is cargo's run of it and not one more: cargo's next
//! build finds the script run, as it finds a run of its own, and does not run it again
//! (docs/reports/BUILDSCRIPT.md, "Round 4").
//!
//! Read from rust-lang/cargo at 5f94df478, which is cargo 1.99.0: `src/compiler/fingerprint/mod.rs`
//! (the fingerprint of a run, its hash, and when cargo runs the script again),
//! `src/compiler/custom_build.rs` (`build_work`: what a run writes, in which order),
//! `src/compiler/layout.rs` (where, and the locks) and `crates/cargo-util/src/paths.rs`.
//!
//! Only the layout of the build directory a stable cargo 1.99 writes is read:
//! `<profile>/build/<package>-<hash>/out`, the files of its run beside it, and its fingerprint
//! in `<profile>/.fingerprint/<package>-<hash>/`. The second layout is nightly's
//! (`-Zbuild-dir-new-layout`), and a run there is cargo's.

use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize, de::Deserializer, ser::Serializer};

/// What a run unit is called in its fingerprint's directory: `run-` and the kind and name of
/// the target, `run-build-script-build-script-build` for `build.rs`.
const RUN: &str = "run-build-script-";

/// The files of the run of one build script in cargo's build directory.
pub(crate) struct Unit {
    out_dir: PathBuf,
    /// `<profile>/build/<package>-<hash>`: `invoked.timestamp`, `output`, `stderr`,
    /// `root-output` and `out`.
    run_dir: PathBuf,
    /// `<build-dir>/[<target>/]<profile>`, where cargo keeps its lock on the build directory.
    dest: PathBuf,
    /// The fingerprint's hash, `.fingerprint/<package>-<hash>/run-build-script-<target>`; the
    /// fingerprint itself is beside it, with `.json`.
    hash: PathBuf,
}

impl Unit {
    /// The run that has `out_dir`.
    pub(crate) fn of(out_dir: &Path) -> Result<Unit, String> {
        let not_cargos =
            || format!("{}: not where a stable cargo runs a script", out_dir.display());
        let run_dir =
            out_dir.parent().filter(|_| out_dir.ends_with("out")).ok_or_else(not_cargos)?;
        let build = run_dir.parent().filter(|it| it.ends_with("build")).ok_or_else(not_cargos)?;
        let dest = build.parent().ok_or_else(not_cargos)?;
        let name = run_dir.file_name().ok_or_else(not_cargos)?;
        let fingerprints = dest.join(".fingerprint").join(name);
        let entries = std::fs::read_dir(&fingerprints).map_err(|_| not_cargos())?;
        let mut runs = entries.flatten().map(|it| it.path()).filter(|it| {
            let name = it.file_name().and_then(|it| it.to_str()).unwrap_or_default();
            name.starts_with(RUN) && it.extension().is_none_or(|it| it != "json")
        });
        let (Some(hash), None) = (runs.next(), runs.next()) else {
            return Err(format!("{}: no one record of a run", fingerprints.display()));
        };
        Ok(Unit {
            out_dir: out_dir.to_owned(),
            run_dir: run_dir.to_owned(),
            dest: dest.to_owned(),
            hash,
        })
    }

    /// Where cargo keeps what the script printed, stamped with when the run began.
    pub(crate) fn output(&self) -> PathBuf {
        self.run_dir.join("output")
    }

    pub(crate) fn lock_dir(&self) -> &Path {
        &self.dest
    }

    /// The locks cargo 1.99 takes for a build in this build directory (`Layout::new`):
    /// `.cargo-build-lock` there, exclusively, and `.cargo-lock`, shared, which cargo takes in
    /// the directory of what it builds for an older cargo, and which is this one when the build
    /// directory is not set apart (`build.build-dir`). Not waited for: a cargo that holds them
    /// may be waiting for what asked for this.
    pub(crate) fn lock(&self) -> Lock {
        let build = self.dest.join(".cargo-build-lock");
        let Ok(build) = std::fs::File::open(&build) else { return Lock::Absent };
        if build.try_lock().is_err() {
            return Lock::Busy;
        }
        let mut held = Held(vec![build]);
        if let Ok(artifacts) = std::fs::File::open(self.dest.join(".cargo-lock")) {
            if artifacts.try_lock_shared().is_err() {
                return Lock::Busy;
            }
            held.0.push(artifacts);
        }
        Lock::Held(held)
    }

    /// Holds cargo's directory while reading or replacing a run. A missing lock does not
    /// certify that no cargo is writing: it is an unsupported directory.
    pub(crate) fn hold(&self) -> Result<Held, String> {
        match self.lock() {
            Lock::Held(held) => Ok(held),
            Lock::Busy => {
                Err("was not read or run again: a cargo is building in its directory".to_owned())
            }
            Lock::Absent => {
                Err("was not read or run again: its directory has no lock of cargo's".to_owned())
            }
        }
    }

    /// The fingerprint cargo wrote when the last run ended, when it is one cargo 1.99 wrote: its
    /// `.json` is what this reads it as, byte for byte, and its hash is what cargo wrote beside
    /// it. Cargo empties the hash before it runs the script again, so an empty one is a run
    /// that did not end, which cargo runs again.
    pub(crate) fn recorded(&self) -> Result<Fingerprint, String> {
        let said = |it: std::io::Error| format!("{}: {it}", self.hash.display());
        let hash = std::fs::read_to_string(&self.hash).map_err(said)?;
        if hash.is_empty() {
            return Err("has no record of a run that ended: cargo runs it again".to_owned());
        }
        let json = std::fs::read(self.json()).map_err(said)?;
        let fingerprint: Fingerprint = serde_json::from_slice(&json).map_err(|it| {
            format!("{}: not a fingerprint of cargo 1.99's: {it}", self.json().display())
        })?;
        if serde_json::to_vec(&fingerprint).ok().as_ref() != Some(&json)
            || to_hex(fingerprint.hash_u64()) != hash
        {
            return Err(format!("{}: not a fingerprint of cargo 1.99's", self.hash.display()));
        }
        Ok(fingerprint)
    }

    fn json(&self) -> PathBuf {
        self.hash.with_extension("json")
    }

    /// `output`, as cargo names it in the fingerprint: from its build directory (the one of
    /// `build.build-dir`, without the target's or profile's directory), as the record has it,
    /// or as the fingerprint of a crate in the same directory names its dep-info from there.
    pub(crate) fn output_named(&self, recorded: &Fingerprint) -> Result<PathBuf, String> {
        let output = self.output();
        let from = |root: &Path| output.strip_prefix(root).ok().map(Path::to_owned);
        let mut roots = recorded.local.iter().filter_map(|it| match it {
            Local::RerunIfChanged { output: named, .. } => output.ends_with(named).then_some(named),
            _ => None,
        });
        if let Some(named) = roots.next() {
            return Ok(named.clone());
        }
        let fingerprints = self.dest.join(".fingerprint");
        let entries = std::fs::read_dir(&fingerprints).into_iter().flatten().flatten();
        for json in
            entries.flat_map(|it| std::fs::read_dir(it.path()).into_iter().flatten().flatten())
        {
            let json = json.path();
            if json.extension().is_none_or(|it| it != "json") {
                continue;
            }
            let Some(other) = std::fs::read(&json)
                .ok()
                .and_then(|it| serde_json::from_slice::<Fingerprint>(&it).ok())
            else {
                continue;
            };
            for local in &other.local {
                let Local::CheckDepInfo { dep_info, .. } = local else { continue };
                let Some(dir) = json.parent() else { continue };
                let at = dir.join(dep_info.file_name().unwrap_or_default());
                let root = at
                    .to_str()
                    .and_then(|at| at.strip_suffix(dep_info.to_str()?).map(PathBuf::from));
                if let Some(named) = root.as_deref().and_then(from) {
                    return Ok(named);
                }
            }
        }
        Err(format!("{}: where cargo's build directory is, is not known", self.dest.display()))
    }

    /// Why cargo 1.99 would run the script again, which `local` says it reads, when it would:
    /// `output` is not there, the script was compiled after it, or a file it names was written
    /// after the run began (`check_filesystem` and `find_stale_file`: a time equal to it is
    /// not after it). What it names below cargo's home, in `git` and `registry`, is never
    /// newer. `None` when cargo would not run it.
    pub(crate) fn stale(
        &self,
        local: &[Local],
        package: &Path,
        script: &Path,
        cargo_home: Option<PathBuf>,
    ) -> Option<String> {
        let output = self.output();
        let Ok(began) = written(&output) else {
            return Some(format!("{} is not there", output.display()));
        };
        if written(script).is_ok_and(|it| it > began) {
            return Some("was compiled after it last ran".to_owned());
        }
        let immutable: Vec<PathBuf> =
            cargo_home.into_iter().flat_map(|it| [it.join("git"), it.join("registry")]).collect();
        for local in local {
            let Local::RerunIfChanged { paths, .. } = local else { continue };
            for path in paths.iter().map(|it| package.join(it)) {
                if immutable.iter().any(|it| path.starts_with(it)) {
                    continue;
                }
                match written_below(&path) {
                    Some(at) if at <= began => {}
                    Some(_) => return Some(format!("{} changed", path.display())),
                    None => return Some(format!("{} is not there", path.display())),
                }
            }
        }
        None
    }

    pub(crate) fn stale_dependencies(&self, edges: &[Edge<'_>]) -> Option<String> {
        let began = written(&self.output()).ok()?;
        edges.iter().find_map(|edge| match written(&edge.output) {
            Ok(at) if at <= began => None,
            _ => Some(format!("the run of {} is missing or newer", edge.package)),
        })
    }

    /// What cargo does before it runs the script (`prepare_target`, then `build_work`): it
    /// empties the hash of the fingerprint, so that a run that does not end is run again,
    /// makes `OUT_DIR`, and writes `invoked.timestamp`, whose time is when the run began.
    pub(crate) fn begin(&self) -> Result<SystemTime, String> {
        std::fs::write(&self.hash, b"").map_err(at(&self.hash))?;
        std::fs::create_dir_all(&self.out_dir).map_err(at(&self.out_dir))?;
        let timestamp = self.run_dir.join("invoked.timestamp");
        std::fs::write(&timestamp, "This file has an mtime of when this was started.")
            .map_err(at(&timestamp))?;
        written(&timestamp).map_err(at(&timestamp))
    }

    /// What cargo writes once the script has run and logged no error: what it printed, as of
    /// when it began, what it said, and the name of `OUT_DIR`.
    pub(crate) fn ran(
        &self,
        began: SystemTime,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<(), String> {
        let output = self.output();
        std::fs::write(&output, stdout).map_err(at(&output))?;
        // Cargo sets both times and goes on if it cannot (`set_file_time_no_err`).
        let times = std::fs::FileTimes::new().set_accessed(began).set_modified(began);
        let _ =
            std::fs::File::options().write(true).open(&output).and_then(|it| it.set_times(times));
        let stderr_file = self.run_dir.join("stderr");
        std::fs::write(&stderr_file, stderr).map_err(at(&stderr_file))?;
        let root = self.run_dir.join("root-output");
        std::fs::write(&root, self.out_dir.as_os_str().as_encoded_bytes()).map_err(at(&root))
    }

    /// Writes the fingerprint of the run, as cargo does when the run unit is done
    /// (`write_fingerprint`): its hash, then itself.
    pub(crate) fn record(&self, fingerprint: &Fingerprint) -> Result<(), String> {
        std::fs::write(&self.hash, to_hex(fingerprint.hash_u64())).map_err(at(&self.hash))?;
        let json = serde_json::to_vec(fingerprint).map_err(|it| it.to_string())?;
        std::fs::write(self.json(), json).map_err(at(&self.json()))
    }
}

/// An error of what was done with `path`, which names it.
fn at(path: &Path) -> impl FnOnce(std::io::Error) -> String + '_ {
    move |it| format!("{}: {it}", path.display())
}

/// Cargo's locks on a build directory, as [`Unit::lock`] finds them.
pub(crate) enum Lock {
    Held(Held),
    /// A cargo holds them.
    Busy,
    /// There is none: the directory is not one a cargo 1.99 built in, or is on NFS, where cargo
    /// takes none. Nothing keeps a cargo out, so nothing is run there.
    Absent,
}

/// Release the advisory locks when the run ends, even if a concurrent fork has inherited
/// one of the open descriptions and has not reached exec yet. Closing only our descriptors
/// leaves a flock held until that process closes its copies too.
pub(crate) struct Held(Vec<std::fs::File>);

impl Drop for Held {
    fn drop(&mut self) {
        for file in &self.0 {
            let _ = file.unlock();
        }
    }
}

/// When `path` was last written, as cargo reads it (`paths::mtime`, which follows a link).
fn written(path: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

/// When `path` or anything below it was last written, following links, as cargo reads a
/// directory named by `rerun-if-changed` (`paths::mtime_recursive`): a link counts by its own
/// time and by its target's, and what cannot be read is left out. `None` when `path` is not
/// there.
pub(super) fn written_below(path: &Path) -> Option<SystemTime> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_dir() {
        return metadata.modified().ok();
    }
    walkdir::WalkDir::new(path)
        .follow_links(true)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            if entry.path_is_symlink() {
                let link = std::fs::symlink_metadata(entry.path()).ok()?.modified().ok()?;
                Some(
                    entry
                        .metadata()
                        .ok()
                        .and_then(|it| it.modified().ok())
                        .map_or(link, |it| it.max(link)),
                )
            } else {
                entry.metadata().ok()?.modified().ok()
            }
        })
        .max()
}

/// Cargo's fingerprint of a unit (`Fingerprint`), as its `.json` has it: the fields cargo
/// keeps, in the order it keeps them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Fingerprint {
    rustc: u64,
    features: String,
    declared_features: String,
    target: u64,
    profile: u64,
    path: u64,
    deps: Vec<Dep>,
    pub(crate) local: Vec<Local>,
    rustflags: Vec<String>,
    config: u64,
    compile_kind: u64,
}

impl Fingerprint {
    /// Cargo's `util::hash_u64` of it: its `Hash`, through `rustc-stable-hash`'s
    /// `StableSipHasher128`, the hasher of cargo's `util/hasher.rs` (both 0.1.2).
    pub(crate) fn hash_u64(&self) -> u64 {
        let mut hasher = rustc_stable_hash::hashers::StableSipHasher128::new();
        self.hash(&mut hasher);
        Hasher::finish(&hasher)
    }

    /// It with `local` in place of what it says the run read: cargo's fingerprint after a run
    /// that read that, everything else being what cargo reckoned before it.
    pub(crate) fn with_local(&self, local: Vec<Local>) -> Fingerprint {
        Fingerprint { local, ..self.clone() }
    }

    pub(crate) fn with_dependencies(&self, edges: &[Edge<'_>]) -> Result<Fingerprint, String> {
        let mut now = self.clone();
        for edge in edges {
            let Some(dep) = now.deps.iter_mut().find(|it| it.pkg_id == edge.pkg_id) else {
                return Err(format!("no cargo run-unit dependency for {}", edge.package));
            };
            dep.fingerprint = edge.fingerprint;
        }
        Ok(now)
    }

    /// Declared dependencies can be optional or target-gated. Only Cargo's recorded
    /// run-unit graph says which native dependencies this particular script run used, each
    /// by the id cargo keys its package by ([`package_id_hash`]).
    pub(crate) fn depends_on(&self, pkg_id: u64) -> bool {
        self.deps.iter().any(|it| it.pkg_id == pkg_id)
    }

    /// The ids of the units it depends on, a unit once for each (`DepFingerprint::pkg_id`).
    pub(crate) fn dependency_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.deps.iter().map(|it| it.pkg_id)
    }
}

/// The run of a package with `links` that a run is built after, as cargo keeps it in its record
/// of that run: by the id of the package ([`package_id_hash`]) and the hash of the fingerprint of
/// the run, whose `output` is when it began.
pub(crate) struct Edge<'a> {
    pub(crate) package: &'a str,
    pub(crate) pkg_id: u64,
    pub(crate) fingerprint: u64,
    pub(crate) output: PathBuf,
}

/// The id cargo keys a unit of a package by in the fingerprint of a unit built after it
/// (`DepFingerprint::new`): the hash of its name for a package of a path source, which keeps a
/// renamed directory from building it again, and else of its whole id, its name, version and
/// source (`impl Hash for PackageId`: the source's kind and canonical URL). `spec` is the id as
/// cargo's messages name the package, `path+file:///w/native#0.1.0` or
/// `registry+https://github.com/rust-lang/crates.io-index#probe-sys@0.1.3`. `None` when it is
/// not one cargo writes.
pub(crate) fn package_id_hash(spec: &str) -> Option<u64> {
    use blaze_registry::{GitReference, SourceId, SourceKind};
    use cargo_util_schemas::core::{GitReference as Spec, PackageIdSpec, SourceKind as Kind};
    let spec = PackageIdSpec::parse(spec).ok()?;
    let kind = match spec.kind()? {
        Kind::Path => return Some(package_hash(spec.name())),
        Kind::Git(reference) => SourceKind::Git(match reference {
            Spec::Tag(it) => GitReference::Tag(it.clone()),
            Spec::Branch(it) => GitReference::Branch(it.clone()),
            Spec::Rev(it) => GitReference::Rev(it.clone()),
            Spec::DefaultBranch => GitReference::DefaultBranch,
        }),
        Kind::Registry => SourceKind::Registry,
        Kind::SparseRegistry => SourceKind::SparseRegistry,
        Kind::LocalRegistry => SourceKind::LocalRegistry,
        Kind::Directory => SourceKind::Directory,
    };
    let source = SourceId::new(kind, spec.url()?.clone()).ok()?;
    let mut hasher = rustc_stable_hash::hashers::StableSipHasher128::new();
    spec.name().hash(&mut hasher);
    spec.version()?.hash(&mut hasher);
    source.hash(&mut hasher);
    Some(Hasher::finish(&hasher))
}

pub(crate) fn package_hash(package: &str) -> u64 {
    let mut hasher = rustc_stable_hash::hashers::StableSipHasher128::new();
    package.hash(&mut hasher);
    Hasher::finish(&hasher)
}

/// The ids ([`package_id_hash`]) of the units cargo compiled the script `real` it runs from
/// with, by its fingerprint of that compile (`build-script-build-script-build.json` in
/// `.fingerprint/<package>-<hash>`, the directory `real` is in having that name): a
/// build-dependency for another platform, or one optional and off, is not one of them.
pub(crate) fn compiled_with(real: &Path) -> Result<Vec<u64>, String> {
    #[derive(Deserialize)]
    struct Compiled {
        deps: Vec<Dep>,
    }
    let not_cargos = || format!("{}: not where a stable cargo compiles a script", real.display());
    let dir = real.parent().ok_or_else(not_cargos)?;
    let build = dir.parent().filter(|it| it.ends_with("build")).ok_or_else(not_cargos)?;
    let dest = build.parent().ok_or_else(not_cargos)?;
    let fingerprints = dest.join(".fingerprint").join(dir.file_name().ok_or_else(not_cargos)?);
    let json = fingerprints.join("build-script-build-script-build.json");
    let json = std::fs::read(&json).map_err(at(&json))?;
    let compiled: Compiled = serde_json::from_slice(&json).map_err(|it| it.to_string())?;
    Ok(compiled.deps.iter().map(|it| it.pkg_id).collect())
}

/// Cargo's `impl Hash for Fingerprint`, field for field.
impl Hash for Fingerprint {
    fn hash<H: Hasher>(&self, h: &mut H) {
        (
            self.rustc,
            &self.features,
            &self.declared_features,
            self.target,
            self.path,
            self.profile,
            &self.local,
            self.config,
            self.compile_kind,
            &self.rustflags,
        )
            .hash(h);
        h.write_usize(self.deps.len());
        for dep in &self.deps {
            dep.pkg_id.hash(h);
            // An `InternedString`, which hashes as its `str`.
            dep.name.as_str().hash(h);
            dep.public.hash(h);
            h.write_u64(dep.fingerprint);
        }
    }
}

/// A unit the one of a fingerprint depends on (`DepFingerprint`), as cargo keeps it: the hash
/// of its package, the name of its crate, whether it is public, and its own fingerprint's hash.
#[derive(Clone, Debug, PartialEq)]
struct Dep {
    pkg_id: u64,
    name: String,
    public: bool,
    fingerprint: u64,
}

impl Serialize for Dep {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        (self.pkg_id, &self.name, self.public, self.fingerprint).serialize(s)
    }
}

impl<'de> Deserialize<'de> for Dep {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Dep, D::Error> {
        let (pkg_id, name, public, fingerprint) = <(u64, String, bool, u64)>::deserialize(d)?;
        Ok(Dep { pkg_id, name, public, fingerprint })
    }
}

/// What a unit read that is not another unit (`LocalFingerprint`), in cargo's order of its
/// kinds, which its hash depends on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum Local {
    Precalculated(String),
    CheckDepInfo { dep_info: PathBuf, checksum: bool },
    RerunIfChanged { output: PathBuf, paths: Vec<PathBuf> },
    RerunIfEnvChanged { var: String, val: Option<String> },
}

/// What cargo reckons a run read, from what the script said (`local_fingerprints_deps`): the
/// files of its `rerun-if-changed`, from its package's directory where they are below it, beside
/// `output` as cargo names it, and each variable of its `rerun-if-env-changed` with the value it
/// has for the cargo that builds next (`env`). `None` when it said neither, and cargo reckons
/// it read its whole package: when a file of the package was written last, which this does not
/// reckon (`Precalculated`, from cargo's list of the package's files).
pub(crate) fn local(
    rerun_if_changed: &[PathBuf],
    rerun_if_env_changed: &[String],
    output: &Path,
    package: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<Vec<Local>> {
    if rerun_if_changed.is_empty() && rerun_if_env_changed.is_empty() {
        return None;
    }
    let mut local = Vec::new();
    if !rerun_if_changed.is_empty() {
        let paths = rerun_if_changed
            .iter()
            .map(|it| it.strip_prefix(package).unwrap_or(it).to_owned())
            .collect();
        local.push(Local::RerunIfChanged { output: output.to_owned(), paths });
    }
    local.extend(
        rerun_if_env_changed
            .iter()
            .map(|var| Local::RerunIfEnvChanged { var: var.clone(), val: env(var) }),
    );
    Some(local)
}

/// Cargo's `util::to_hex`: the bytes of `num`, least significant first, in lower-case hex.
fn to_hex(num: u64) -> String {
    num.to_le_bytes().iter().map(|it| format!("{it:02x}")).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_duplicated_description_does_not_keep_a_finished_runs_lock() {
        let dir = std::env::temp_dir().join(format!("ra-jit-record-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".cargo-build-lock"), "").unwrap();
        let unit = Unit {
            out_dir: PathBuf::new(),
            run_dir: PathBuf::new(),
            dest: dir.clone(),
            hash: PathBuf::new(),
        };
        let Lock::Held(held) = unit.lock() else { panic!("the first run holds its lock") };
        // A concurrent fork can inherit this open description until exec closes it. A dup
        // keeps that same description alive deterministically, without a fork timing race.
        let inherited = held.0[0].try_clone().unwrap();
        drop(held);
        let next = unit.lock();
        assert!(matches!(next, Lock::Held(_)), "the finished run released its lock");
        drop(next);
        drop(inherited);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A fingerprint of a run, and its hash, that cargo 1.99.0 wrote for the build script of a
    /// package with one `rerun-if-changed` and one `rerun-if-env-changed`
    /// (`cargo +1.99.0 build` on macOS arm64, in `target/debug/.fingerprint/`; see
    /// docs/reports/BUILDSCRIPT.md, "Round 4"). This one reads it as cargo does, writes it again
    /// byte for byte, and hashes it to what cargo wrote beside it.
    const CARGOS_JSON: &str = include_str!("records/run-build-script-build-script-build.json");
    const CARGOS_HASH: &str = include_str!("records/run-build-script-build-script-build");

    /// The program of the same package, which depends on that run: a fingerprint with the other
    /// fields set, and the run's hash among what it depends on.
    const CARGOS_BIN_JSON: &str = include_str!("records/bin-golden.json");
    const CARGOS_BIN_HASH: &str = include_str!("records/bin-golden");

    #[test]
    fn a_fingerprint_cargo_wrote_is_read_written_and_hashed_as_cargo_does() {
        let run: Fingerprint = serde_json::from_str(CARGOS_JSON).unwrap();
        assert_eq!(serde_json::to_string(&run).unwrap(), CARGOS_JSON);
        assert_eq!(to_hex(run.hash_u64()), CARGOS_HASH);
        let [
            Local::RerunIfChanged { paths, .. },
            Local::RerunIfEnvChanged { val: Some(_), .. },
            Local::RerunIfEnvChanged { val: None, .. },
        ] = &run.local[..]
        else {
            panic!("{:?}", run.local)
        };
        // A path is hashed as `Path` hashes, which leaves out a `.` between separators.
        assert_eq!(paths[1], Path::new("data/./in.txt"));
        let bin: Fingerprint = serde_json::from_str(CARGOS_BIN_JSON).unwrap();
        assert_eq!(serde_json::to_string(&bin).unwrap(), CARGOS_BIN_JSON);
        assert_eq!(to_hex(bin.hash_u64()), CARGOS_BIN_HASH);
        assert_eq!(bin.deps[0].fingerprint, run.hash_u64());
    }

    #[test]
    fn what_a_run_read_is_reckoned_as_cargo_reckons_it() {
        let package = Path::new("/w/pkg");
        let output = Path::new("debug/build/pkg-0123/output");
        let env = |name: &str| (name == "SET").then(|| "1".to_owned());
        assert_eq!(local(&[], &[], output, package, &env), None);
        let changed = [
            PathBuf::from("build.rs"),
            PathBuf::from("/w/pkg/src/a.rs"),
            PathBuf::from("/elsewhere"),
        ];
        let vars = ["SET".to_owned(), "UNSET".to_owned()];
        assert_eq!(
            local(&changed, &vars, output, package, &env),
            Some(vec![
                Local::RerunIfChanged {
                    output: output.to_owned(),
                    paths: ["build.rs", "src/a.rs", "/elsewhere"].map(PathBuf::from).into(),
                },
                Local::RerunIfEnvChanged { var: "SET".to_owned(), val: Some("1".to_owned()) },
                Local::RerunIfEnvChanged { var: "UNSET".to_owned(), val: None },
            ])
        );
        assert_eq!(
            local(&[], &vars[..1], output, package, &env),
            Some(vec![Local::RerunIfEnvChanged {
                var: "SET".to_owned(),
                val: Some("1".to_owned())
            }])
        );
    }

    #[test]
    fn a_package_is_keyed_as_cargo_keys_it() {
        // Read from the record cargo 1.99.0 wrote of a run built after the runs of `native`, a
        // path package outside the workspace, and `probe-sys` 0.1.3 of crates.io, vendored as a
        // directory source that replaces it: cargo's messages name the packages so.
        let path = "path+file:///private/tmp/u8reg-m/outside/native#0.1.0";
        assert_eq!(package_id_hash(path), Some(12039738031040411086));
        assert_eq!(package_hash("native"), 12039738031040411086);
        let registry = "registry+https://github.com/rust-lang/crates.io-index#probe-sys@0.1.3";
        assert_eq!(package_id_hash(registry), Some(813625565111490420));
        assert_eq!(package_id_hash("not an id"), None);
    }

    #[test]
    fn hex_is_of_the_bytes_least_significant_first() {
        assert_eq!(to_hex(0x0123_4567_89ab_cdef), "efcdab8967452301");
    }

    /// Writes a fingerprint for a run of `out_dir`'s script as cargo would after a run that
    /// printed `printed`, which says what it read: for a test, which has no cargo.
    pub(crate) fn recorded_as_cargo_would(
        out_dir: &Path,
        package: &Path,
        printed: &[u8],
    ) -> Fingerprint {
        let mut fingerprint: Fingerprint = serde_json::from_str(CARGOS_JSON).unwrap();
        let run_dir = out_dir.parent().unwrap();
        let dest = run_dir.parent().unwrap().parent().unwrap();
        let name = run_dir.file_name().unwrap();
        let dir = dest.join(".fingerprint").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let output =
            run_dir.join("output").strip_prefix(dest.parent().unwrap()).unwrap().to_owned();
        let said = blaze_build_script::parse(
            printed,
            &blaze_build_script::output::Context {
                package: &Default::default(),
                out_dir_when_generated: out_dir,
                out_dir,
                nightly_features_allowed: false,
                rustc_bootstrap: None,
            },
        )
        .unwrap();
        let env = |name: &str| std::env::var(name).ok();
        fingerprint.local =
            local(&said.rerun_if_changed, &said.rerun_if_env_changed, &output, package, &env)
                .unwrap_or_else(|| {
                    vec![Local::Precalculated(crate::scripts::path_package_fingerprint(package))]
                });
        let unit = Unit {
            out_dir: out_dir.to_owned(),
            run_dir: run_dir.to_owned(),
            dest: dest.to_owned(),
            hash: dir.join(format!("{RUN}build-script-build")),
        };
        unit.record(&fingerprint).unwrap();
        fingerprint
    }
}
