//! Tests run in a sandbox of their own ([`crate::sandbox`]): under `RA_JIT_TESTS=affected`,
//! each test the daemon says reaches the world outside its code (files, the environment,
//! processes, the network) runs alone in a process of its own, with the machine's files
//! read-only, the workspace and target directory as they are but its writes kept to itself,
//! an empty `/tmp`, a fixed environment and no network. What it touched is recorded, and once
//! it passes, what was at each of those paths is kept: the test runs again when its code
//! changed or one of them did, and is skipped otherwise, as a test that reaches nothing
//! outside is. `RA_JIT_SANDBOX=off` runs them as cargo does, every time.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest as _, Sha256};

use super::state::{Program, State};
use crate::sandbox::{self, Spec};

pub(crate) const VAR: &str = "RA_JIT_SANDBOX";

/// Names of more variables of the environment a sandboxed test is given, by commas.
pub(crate) const PASS: &str = "RA_JIT_TEST_ENV";

/// The file beside a program of tests with what each sandboxed test that passed touched, and
/// what was there when it passed: the test's name, the path, what [`stat`] said of it and
/// what [`digest`] did, by tabs, one path a line.
const INPUTS: &str = "tests-inputs";

/// Where the sandbox writes what each test touched as this run of the program goes.
const RECORDS: &str = "tests-records";

/// The variables of the environment a sandboxed test keeps besides those cargo and ra-jit set
/// for it: what finds programs and the toolchain, who and where the user is, the locale, and
/// what decides how a failure is shown.
const KEPT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LANGUAGE",
    "TZ",
    "CARGO",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "LD_LIBRARY_PATH",
    "NIX_LD",
    "NIX_LD_LIBRARY_PATH",
    "RUST_BACKTRACE",
    "RUST_LIB_BACKTRACE",
    "RUST_MIN_STACK",
];

/// Whether this command runs the tests that reach the world outside their code sandboxed:
/// `Ok` when it does, `Err` with why when it would and can't. `None` when it does not.
pub(crate) fn wanted(affected: bool) -> Option<Result<(), String>> {
    match std::env::var(VAR).as_deref() {
        Ok("off" | "0") => None,
        _ if affected => Some(sandbox::available()),
        _ => None,
    }
}

/// The environment the test `command` runs would have, cut down to what a sandboxed test
/// keeps: what the command sets itself, and of the rest [`KEPT`], [`PASS`] and manifest declarations.
pub(crate) fn env(command: &Command, declared: &[String]) -> Vec<(String, String)> {
    let passed = std::env::var(PASS).unwrap_or_default();
    env_from(command, declared, &passed, std::env::vars_os())
}

fn env_from(
    command: &Command,
    declared: &[String],
    passed: &str,
    inherited: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(String, String)> {
    let kept = |name: &str| {
        KEPT.contains(&name)
            || name.starts_with("LC_")
            || passed.split(',').any(|it| it.trim() == name)
            || declared.iter().any(|it| it == name)
    };
    let mut env: BTreeMap<String, String> = inherited
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| kept(name))
        .collect();
    for (name, value) in command.get_envs() {
        let Some(name) = name.to_str() else { continue };
        match value.and_then(|it| it.to_str()) {
            Some(value) => _ = env.insert(name.to_owned(), value.to_owned()),
            None => _ = env.remove(name),
        }
    }
    env.into_iter().collect()
}

#[cfg(test)]
mod env_tests {
    use super::*;

    #[test]
    fn declared_inherited_poison_is_forwarded_and_command_override_wins() {
        let inherited = || {
            [("T1_DECLARED".into(), "poison".into()), ("UNDECLARED".into(), "hidden".into())]
                .into_iter()
        };
        let declared = vec!["T1_DECLARED".to_owned()];
        let mut command = Command::new("test");
        let forwarded = env_from(&command, &declared, "", inherited());
        assert_eq!(forwarded, vec![("T1_DECLARED".to_owned(), "poison".to_owned())]);
        #[cfg(unix)]
        let rejects_poison = |env: &[(String, String)]| {
            Command::new("/bin/sh")
                .args(["-c", "test \"$T1_DECLARED\" != poison"])
                .env_clear()
                .envs(env.iter().cloned())
                .status()
                .unwrap()
                .success()
        };
        #[cfg(unix)]
        assert!(!rejects_poison(&forwarded));
        command.env("T1_DECLARED", "forced");
        let forwarded = env_from(&command, &declared, "", inherited());
        assert_eq!(forwarded, vec![("T1_DECLARED".to_owned(), "forced".to_owned())]);
        #[cfg(unix)]
        assert!(rejects_poison(&forwarded));
        command.env_remove("T1_DECLARED");
        assert!(env_from(&command, &declared, "", inherited()).is_empty());
    }
}

/// What stands for the environment `env` in a sandboxed test's identity.
pub(crate) fn identity_of(env: &[(String, String)]) -> String {
    let mut hash = Sha256::new();
    for (name, value) in env {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    let hex: String = hash.finalize().iter().take(8).map(|it| format!("{it:02x}")).collect();
    format!("~{hex}")
}

/// The target directory a program's directory of the daemon's is in: the parent of its
/// `ra-jit` directory.
fn target_of(dir: &Path) -> Option<&Path> {
    dir.ancestors().find(|it| it.file_name().is_some_and(|it| it == "ra-jit"))?.parent()
}

/// Has the harness of `test` run each of `names` in a sandbox, by the files it reads beside
/// the program: sets [`crate::test_harness::SANDBOXED`] on `command`.
pub(crate) fn prepare(
    state: &State,
    test: &Program,
    dir: &Path,
    command: &mut Command,
    env: Vec<(String, String)>,
    names: &[String],
) -> anyhow::Result<()> {
    let records = dir.join(RECORDS);
    _ = std::fs::remove_dir_all(&records);
    std::fs::create_dir_all(&records)?;
    let mut overlays = vec![state.root.clone()];
    if let Some(target) = target_of(dir).filter(|it| !it.starts_with(&state.root)) {
        overlays.push(target.to_owned());
    }
    let mut not_inputs = vec![test.executable.clone()];
    not_inputs.extend(std::fs::canonicalize(&test.executable).ok());
    let spec =
        Spec { cwd: test.package_dir.clone(), env, overlays, record: Some(records), not_inputs };
    let spec_file = dir.join("tests-sandbox.json");
    std::fs::write(&spec_file, serde_json::to_string(&spec)?)?;
    let exe = std::env::current_exe()?;
    let mut text =
        [exe.as_os_str(), sandbox::COMMAND.as_ref(), spec_file.as_os_str(), "--".as_ref()]
            .map(|it| it.to_string_lossy().into_owned())
            .join("\t");
    text.push('\n');
    for name in names {
        text.push_str(name);
        text.push('\n');
    }
    let list = dir.join("tests-sandboxed");
    std::fs::write(&list, text)?;
    command.env(crate::test_harness::SANDBOXED, list);
    Ok(())
}

/// What is kept of the sandboxed tests of a program of tests: what each that passed touched.
pub(crate) struct Kept {
    file: PathBuf,
    records: PathBuf,
    /// Each test's paths, with what [`stat`] and [`digest`] said of each when it passed.
    tests: BTreeMap<String, Vec<(String, String, String)>>,
    /// The tests run in a sandbox by this command.
    pub(crate) ran: BTreeSet<String>,
    seen: HashMap<String, (String, String)>,
}

impl Kept {
    pub(crate) fn read(dir: &Path) -> Kept {
        let file = dir.join(INPUTS);
        let mut tests: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
        for line in std::fs::read_to_string(&file).unwrap_or_default().lines() {
            let mut parts = line.splitn(4, '\t');
            let (Some(name), Some(stat), Some(digest), Some(path)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let paths = tests.entry(name.to_owned()).or_default();
            paths.push((path.to_owned(), stat.to_owned(), digest.to_owned()));
        }
        Kept { file, records: dir.join(RECORDS), tests, ran: BTreeSet::new(), seen: HashMap::new() }
    }

    /// The first path the test `name` touched when it passed that is not what it was then:
    /// `Some(None)` when nothing is kept of it, `None` when each is as it was.
    pub(crate) fn changed(&mut self, name: &str) -> Option<Option<String>> {
        let Some(paths) = self.tests.get(name) else { return Some(None) };
        for (path, stat_then, digest_then) in paths {
            let (stat_now, digest_now) = match self.seen.get(path) {
                Some(it) => it.clone(),
                None => {
                    let stat_now = stat(Path::new(path));
                    let digest_now = match &stat_now == stat_then {
                        true => digest_then.clone(),
                        false => digest(Path::new(path)),
                    };
                    self.seen.insert(path.clone(), (stat_now.clone(), digest_now.clone()));
                    (stat_now, digest_now)
                }
            };
            if stat_now != *stat_then && digest_now != *digest_then {
                return Some(Some(path.clone()));
            }
        }
        None
    }

    /// Keeps what each test of `passed` that ran in a sandbox touched, and forgets the tests
    /// that are not in `passed`. A test the sandbox wrote nothing for is taken out of
    /// `passed`, so that it runs again: what it read is not known.
    pub(crate) fn settle(mut self, passed: &mut BTreeMap<String, String>) {
        let mut known = std::mem::take(&mut self.seen);
        for (path, stat, digest) in self.tests.values().flatten() {
            known.entry(path.clone()).or_insert_with(|| (stat.clone(), digest.clone()));
        }
        let mut now = |path: &str| {
            let stat_now = stat(Path::new(path));
            if let Some((stat, digest)) = known.get(path).filter(|(stat, _)| *stat == stat_now) {
                return (stat.clone(), digest.clone());
            }
            let digest_now = digest(Path::new(path));
            known.insert(path.to_owned(), (stat_now.clone(), digest_now.clone()));
            (stat_now, digest_now)
        };
        self.tests.retain(|name, _| passed.contains_key(name));
        for name in &self.ran {
            if !passed.contains_key(name) {
                continue;
            }
            let record = self.records.join(sandbox::record_name(name));
            let Ok(text) = std::fs::read_to_string(&record) else {
                passed.remove(name);
                self.tests.remove(name);
                continue;
            };
            let paths = text
                .lines()
                .map(|path| {
                    let (stat, digest) = now(path);
                    (path.to_owned(), stat, digest)
                })
                .collect();
            self.tests.insert(name.clone(), paths);
        }
        let mut text = String::new();
        for (name, paths) in &self.tests {
            for (path, stat, digest) in paths {
                text.push_str(&format!("{name}\t{stat}\t{digest}\t{path}\n"));
            }
        }
        let writing = self.file.with_extension("writing");
        if std::fs::write(&writing, text)
            .and_then(|()| std::fs::rename(&writing, &self.file))
            .is_err()
        {
            _ = std::fs::remove_file(&self.file);
        }
    }
}

/// What is quick to tell of what is at `path`: whether it is there, its size and its last
/// change. Where that is the same, what is there is taken to be.
fn stat(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else { return "-".to_owned() };
    let at = meta.modified().ok().and_then(|it| it.duration_since(std::time::UNIX_EPOCH).ok());
    #[cfg(unix)]
    let node = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let node = 0;
    format!("{}:{}:{node}", meta.len(), at.map_or(0, |it| it.as_nanos()))
}

/// What is at `path`: nothing, a file's bytes, a directory's names, or something else (a
/// device, a socket), by a hash.
fn digest(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else { return "-".to_owned() };
    let mut hash = Sha256::new();
    if meta.is_dir() {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .filter_map(|it| it.ok().map(|it| it.file_name()))
            .collect();
        names.sort();
        hash.update(b"d");
        for name in names {
            hash.update(name.as_encoded_bytes());
            hash.update([0]);
        }
    } else if meta.is_file() {
        hash.update(b"f");
        match std::fs::File::open(path) {
            Ok(mut file) => _ = std::io::copy(&mut file, &mut hash),
            Err(_) => hash.update(b"unreadable"),
        }
    } else {
        hash.update(b"o");
    }
    hash.finalize().iter().take(16).map(|it| format!("{it:02x}")).collect()
}
