//! When a build script runs again, as cargo 1.99 decides it for its `RunCustomBuild` unit:
//! `calculate_run_custom_build`, `build_script_local_fingerprints`, `local_fingerprints_deps`,
//! `check_filesystem` and `find_stale_item` of `src/compiler/fingerprint/mod.rs` in
//! rust-lang/cargo at 5f94df478. The records are blaze's, not cargo's: a [`Fingerprint`] of a
//! run that ended, kept beside its `OUT_DIR`, and nothing of cargo's target directory.
//!
//! A build makes a fingerprint of the run it would do: what the run depends on (the compile of
//! the script and the runs of the dependencies with `links` it is given the metadata of, each
//! as a hash; the flags for rustc; the compiler), and what it reads itself ([`Local`]), from
//! what the last run said: the files it named with `rerun-if-changed` and the value of each
//! variable it named with `rerun-if-env-changed`, or the newest file of its package when it
//! named neither. The script runs again when that fingerprint is not the one of the last run
//! that ended, and else when the output of that run is not there, a dependency was made after
//! it, or a file it named is not there or was written after the run began. The fingerprint of
//! a run that ended reads what that run said, or, when it named nothing, what the fingerprint
//! made before it read: a script that says other than it did runs once more for it.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde_derive::{Deserialize, Serialize};

use crate::output::Reads;

/// What a run reads itself, as cargo's `LocalFingerprint` of a build script has it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Local {
    /// `Precalculated`: the package's newest file, its time and its path from the package's
    /// directory, for a script that named nothing it reads. A package of a registry is
    /// immutable: its fingerprint is its version.
    Package { fingerprint: String },
    /// `RerunIfChanged`: the files named, relative to the package's directory when inside it.
    Changed { paths: Vec<PathBuf> },
    /// `RerunIfEnvChanged`: a variable, and its value as `[env]` or the environment has it.
    Env { var: String, val: Option<String> },
}

/// The fingerprint of a run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fingerprint {
    /// What the compiler says of itself (`rustc -vV`).
    pub rustc: String,
    /// The flags for rustc of the unit.
    pub rustflags: Vec<String>,
    /// What the run depends on, by name: the compile of the script, and the run of each
    /// dependency with `links`, each as the hash of its fingerprint.
    pub deps: Vec<(String, u64)>,
    pub local: Vec<Local>,
}

impl Fingerprint {
    /// A hash of all of it, the same in every process and on every machine.
    pub fn hash(&self) -> u64 {
        hash_bytes(&serde_json::to_vec(self).unwrap_or_default())
    }
}

/// FNV-1a: a hash that is the same wherever it is computed, which `std`'s is not promised to be.
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// What the run reads itself, from what the run before it `said` (`None` when no run said
/// anything that is kept): the files and the variables it named, each variable's value as
/// `value` has it, or what `package` says of the package's files when it named neither.
pub fn local(
    said: Option<&Reads>,
    package_dir: &Path,
    value: &dyn Fn(&str) -> Option<String>,
    package: &dyn Fn() -> String,
) -> Vec<Local> {
    match said {
        Some(said) if !said.files.is_empty() || !said.env.is_empty() => {
            local_of(said, package_dir, value)
        }
        _ => vec![Local::Package { fingerprint: package() }],
    }
}

/// `local_fingerprints_deps`: what `said` names.
fn local_of(
    said: &Reads,
    package_dir: &Path,
    value: &dyn Fn(&str) -> Option<String>,
) -> Vec<Local> {
    let mut local = Vec::new();
    if !said.files.is_empty() {
        let paths = said
            .files
            .iter()
            .map(|it| it.strip_prefix(package_dir).unwrap_or(it).to_owned())
            .collect();
        local.push(Local::Changed { paths });
    }
    local.extend(said.env.iter().map(|var| Local::Env { var: var.clone(), val: value(var) }));
    local
}

/// The fingerprint of a run that ended, made `before` it: what it reads is what it `said`,
/// when it named anything, and else what was read before it.
pub fn after_run(
    before: &Fingerprint,
    said: &Reads,
    package_dir: &Path,
    value: &dyn Fn(&str) -> Option<String>,
) -> Fingerprint {
    let mut after = before.clone();
    if !said.files.is_empty() || !said.env.is_empty() {
        after.local = local_of(said, package_dir, value);
    }
    after
}

/// The fingerprint of a package's files, as cargo's `PathSource::fingerprint` writes it: the
/// time of the newest file and its path from the package's directory.
pub fn package_fingerprint(package_dir: &Path, newest: (SystemTime, PathBuf)) -> String {
    let (time, path) = newest;
    let since = time.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let path = path.strip_prefix(package_dir).unwrap_or(&path).to_owned();
    format!("{}.{:09}s ({})", since.as_secs(), since.subsec_nanos(), path.display())
}

/// Why a script runs again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stale {
    /// No run of it ended.
    New,
    /// The fingerprint of the run it would do is not the last one's.
    Other,
    /// The output of the last run is not there.
    Output(PathBuf),
    /// A dependency was made after the last run.
    Dependency(String),
    /// A file it named is not there.
    Missing(PathBuf),
    /// A file it named was written after the last run began.
    Newer(PathBuf),
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stale::New => write!(f, "it has not run"),
            Stale::Other => write!(f, "what it depends on changed"),
            Stale::Output(path) => write!(f, "{} is not there", path.display()),
            Stale::Dependency(name) => write!(f, "{name} was made after it ran"),
            Stale::Missing(path) => write!(f, "{} is not there", path.display()),
            Stale::Newer(path) => write!(f, "{} changed", path.display()),
        }
    }
}

/// Whether the run fingerprinted `now` is to be done, when the last run that ended was `was`
/// and printed `output`, which is dated when that run began. `deps` are when each dependency
/// was last made. `None` when the last run stands.
pub fn stale(
    was: Option<&Fingerprint>,
    now: &Fingerprint,
    output: &Path,
    deps: &[(String, Option<SystemTime>)],
    package_dir: &Path,
) -> Option<Stale> {
    let Some(was) = was else { return Some(Stale::New) };
    if was.hash() != now.hash() {
        return Some(Stale::Other);
    }
    // `check_filesystem`: the outputs first, then the dependencies, then what it reads.
    let Ok(ran) = std::fs::metadata(output).and_then(|it| it.modified()) else {
        return Some(Stale::Output(output.to_owned()));
    };
    for (name, made) in deps {
        match made {
            Some(made) if *made <= ran => {}
            _ => return Some(Stale::Dependency(name.clone())),
        }
    }
    for local in &now.local {
        let Local::Changed { paths } = local else { continue };
        for path in paths {
            let path = package_dir.join(path);
            match newest(&path) {
                None => return Some(Stale::Missing(path)),
                // A time equal to the run's is not after it (`find_stale_file`).
                Some(written) if written > ran => return Some(Stale::Newer(path)),
                Some(_) => {}
            }
        }
    }
    None
}

/// The newest time of the file at `path`, or of the directory there and what is below it, as
/// cargo's `mtime_recursive` takes it; nothing when it is not there.
fn newest(path: &Path) -> Option<SystemTime> {
    crate::apply::newest(path)
}

#[cfg(test)]
mod tests;
