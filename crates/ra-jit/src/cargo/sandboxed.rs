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

/// An owned command already contains its complete environment; the worker's variables
/// are not inputs of a saved replay.
pub(crate) fn env_exact(
    command: &Command,
    declared: &[String],
    passed: &str,
) -> Vec<(String, String)> {
    env_from(command, declared, passed, std::iter::empty())
}

/// Retains the caller or saved base environment by the sandbox policy, then applies
/// authoritative Cargo and configured overlays. An owned command's complete environment
/// cannot express that distinction after `env_clear` and `envs` have flattened it.
pub(crate) fn env_owned(
    command: &Command,
    declared: &[String],
    passed: &str,
    inherited: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Result<Vec<(String, String)>, String> {
    let mut env: BTreeMap<String, std::ffi::OsString> = inherited
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
        .filter(|(name, _)| keeps(name, declared, passed))
        .collect();
    for (name, value) in command.get_envs() {
        let name = name.to_str().ok_or("sandbox environment name is not UTF-8")?;
        match value {
            Some(value) => _ = env.insert(name.to_owned(), value.to_owned()),
            None => _ = env.remove(name),
        }
    }
    env.into_iter()
        .map(|(name, value)| {
            value
                .into_string()
                .map(|value| (name.clone(), value))
                .map_err(|_| format!("sandbox environment value `{name}` is not UTF-8"))
        })
        .collect()
}

fn keeps(name: &str, declared: &[String], passed: &str) -> bool {
    KEPT.contains(&name)
        || name.starts_with("LC_")
        || passed.split(',').any(|it| it.trim() == name)
        || declared.iter().any(|it| it == name)
}

fn env_from(
    command: &Command,
    declared: &[String],
    passed: &str,
    inherited: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = inherited
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| keeps(name, declared, passed))
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
    fn owned_commands_do_not_inherit_worker_environment() {
        let mut command = Command::new("owned-test");
        command.env_clear();
        assert!(env_exact(&command, &["PATH".into()], "HOME,USER").is_empty());
        command.env("PATH", "recorded").env("DECLARED", "value");
        assert_eq!(
            env_exact(&command, &["DECLARED".into()], "PATH"),
            [("DECLARED".into(), "value".into()), ("PATH".into(), "recorded".into())]
        );
    }

    #[test]
    fn owned_sandbox_filters_ambient_values_but_preserves_authoritative_overlays() {
        let inherited = || {
            [
                ("PATH".into(), "recorded-path".into()),
                ("T1_DECLARED".into(), "recorded-declaration".into()),
                ("T1_PASSED".into(), "recorded-passed".into()),
                ("RA_JIT_RUN_ID".into(), "volatile".into()),
                ("UNDECLARED".into(), "hidden".into()),
            ]
            .into_iter()
        };
        let mut overlays = Command::new("owned-test");
        overlays.env("CONFIGURED", "authoritative").env("T1_DECLARED", "override");
        let env =
            env_owned(&overlays, &["T1_DECLARED".into()], " T1_PASSED ", inherited()).unwrap();
        assert_eq!(
            env,
            [
                ("CONFIGURED".into(), "authoritative".into()),
                ("PATH".into(), "recorded-path".into()),
                ("T1_DECLARED".into(), "override".into()),
                ("T1_PASSED".into(), "recorded-passed".into()),
            ]
        );
        overlays.env_remove("T1_PASSED");
        assert!(
            !env_owned(&overlays, &[], "T1_PASSED", inherited())
                .unwrap()
                .iter()
                .any(|(name, _)| name == "T1_PASSED")
        );
    }

    #[cfg(unix)]
    #[test]
    fn owned_sandbox_cannot_silently_drop_native_values() {
        use std::os::unix::ffi::OsStringExt as _;
        let inherited = || {
            [("T1_NATIVE".into(), std::ffi::OsString::from_vec(b"native-\xff".to_vec()))]
                .into_iter()
        };
        let mut overlays = Command::new("owned-test");
        assert_eq!(env_owned(&overlays, &[], "", inherited()).unwrap(), []);
        assert!(env_owned(&overlays, &[], "T1_NATIVE", inherited()).is_err());
        overlays.env("T1_NATIVE", "configured");
        assert_eq!(
            env_owned(&overlays, &[], "T1_NATIVE", inherited()).unwrap(),
            [("T1_NATIVE".into(), "configured".into())]
        );
        overlays.env("T1_NATIVE", std::ffi::OsString::from_vec(b"configured-\xfe".to_vec()));
        assert!(env_owned(&overlays, &[], "", inherited()).is_err());
    }

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
    prepare_at(&state.root, &test.package_dir, &test.executable, dir, command, env, names)
}

pub(crate) fn prepare_at(
    root: &Path,
    package_dir: &Path,
    executable: &Path,
    dir: &Path,
    command: &mut Command,
    env: Vec<(String, String)>,
    names: &[String],
) -> anyhow::Result<()> {
    let records = dir.join(RECORDS);
    _ = std::fs::remove_dir_all(&records);
    std::fs::create_dir_all(&records)?;
    let mut overlays = vec![root.to_owned()];
    if let Some(target) = target_of(dir).filter(|it| !it.starts_with(root)) {
        overlays.push(target.to_owned());
    }
    let mut not_inputs = vec![executable.to_owned()];
    not_inputs.extend(std::fs::canonicalize(executable).ok());
    let spec =
        Spec { cwd: package_dir.to_owned(), env, overlays, record: Some(records), not_inputs };
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
            let (_, digest_now) = match self.seen.get(path) {
                Some(it) => it.clone(),
                None => {
                    let before = (stat_then.clone(), digest_then.clone());
                    let Ok(now) = observed(Path::new(path), Some(&before)) else {
                        return Some(Some(path.clone()));
                    };
                    self.seen.insert(path.clone(), now.clone());
                    now
                }
            };
            if digest_now != *digest_then {
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
            let now = observed(Path::new(path), known.get(path))?;
            known.insert(path.to_owned(), now.clone());
            Ok::<_, std::io::Error>(now)
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
                    let (stat, digest) = now(path)?;
                    Ok((path.to_owned(), stat, digest))
                })
                .collect::<std::io::Result<Vec<_>>>();
            let Ok(paths) = paths else {
                passed.remove(name);
                self.tests.remove(name);
                continue;
            };
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

/// A content-cache stamp includes Unix change time: restoring a file's mtime does not
/// restore its ctime. Permission changes also invalidate the stamp and the input digest.
/// A symlink needs both its own stamp and its target's: either can change what was read.
fn stat(path: &Path) -> std::io::Result<String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("-".to_owned()),
        Err(error) => return Err(error),
    };
    let stamp = metadata_stamp(&meta);
    if !meta.is_symlink() {
        return Ok(stamp);
    }
    let target = match std::fs::metadata(path) {
        Ok(meta) => metadata_stamp(&meta),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "-".to_owned(),
        Err(error) => return Err(error),
    };
    Ok(format!("link:{stamp}:{target}"))
}

fn metadata_stamp(meta: &std::fs::Metadata) -> String {
    let at = meta.modified().ok().and_then(|it| it.duration_since(std::time::UNIX_EPOCH).ok());
    #[cfg(unix)]
    let node = {
        use std::os::unix::fs::MetadataExt as _;
        format!(
            "{}:{}:{}:{}:{}",
            meta.dev(),
            meta.ino(),
            meta.ctime(),
            meta.ctime_nsec(),
            meta.mode()
        )
    };
    #[cfg(not(unix))]
    let node = 0;
    format!("{}:{}:{node}", meta.len(), at.map_or(0, |it| it.as_nanos()))
}

/// Never keep a partial read or a digest observed while its input changed. Platforms
/// without a change-time stamp read the content each time instead of trusting mtime.
fn observed(path: &Path, before: Option<&(String, String)>) -> std::io::Result<(String, String)> {
    let stamp = stat(path)?;
    if cfg!(unix)
        && let Some((before, digest)) = before
        && *before == stamp
    {
        return Ok((stamp, digest.clone()));
    }
    let digest = digest(path)?;
    if stat(path)? != stamp {
        return Err(std::io::Error::other("test input changed while it was being read"));
    }
    Ok((stamp, digest))
}

/// What is at `path`: nothing, a file's bytes, or a directory's names, by a hash. A symlink
/// also includes the raw text readlink returns, even when another spelling has the same target.
fn digest(path: &Path) -> std::io::Result<String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("-".to_owned()),
        Err(error) => return Err(error),
    };
    if !meta.is_symlink() {
        return followed_digest(path);
    }
    let link = std::fs::read_link(path)?;
    let bytes = link.as_os_str().as_encoded_bytes();
    let mut hash = Sha256::new();
    hash.update(b"link\0");
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    hash.update(followed_digest(path)?.as_bytes());
    Ok(hash.finalize().iter().take(16).map(|it| format!("{it:02x}")).collect())
}

/// Following the path keeps target contents in the key and refuses cyclic links.
fn followed_digest(path: &Path) -> std::io::Result<String> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("-".to_owned()),
        Err(error) => return Err(error),
    };
    let mut hash = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        hash.update((meta.permissions().mode() & 0o7777).to_le_bytes());
    }
    if meta.is_dir() {
        let mut names: Vec<_> = std::fs::read_dir(path)?
            .map(|entry| entry.map(|it| it.file_name()))
            .collect::<std::io::Result<_>>()?;
        names.sort();
        hash.update(b"d");
        for name in names {
            hash.update(name.as_encoded_bytes());
            hash.update([0]);
        }
    } else if meta.is_file() {
        hash.update(b"f");
        std::io::copy(&mut std::fs::File::open(path)?, &mut hash)?;
    } else {
        return Err(std::io::Error::other("not a regular file or directory"));
    }
    Ok(hash.finalize().iter().take(16).map(|it| format!("{it:02x}")).collect())
}

#[cfg(test)]
mod input_tests {
    use super::*;

    fn remember(dir: &Path, input: &Path) -> BTreeMap<String, String> {
        let mut kept = Kept::read(dir);
        std::fs::create_dir_all(&kept.records).unwrap();
        std::fs::write(
            kept.records.join(sandbox::record_name("test")),
            input.display().to_string(),
        )
        .unwrap();
        kept.ran.insert("test".to_owned());
        let mut passed = BTreeMap::from([("test".to_owned(), "code".to_owned())]);
        kept.settle(&mut passed);
        passed
    }

    #[test]
    fn recorded_inputs_detect_same_size_and_mtime_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        std::fs::write(&input, b"first").unwrap();
        let stamp = std::fs::metadata(&input).unwrap().modified().unwrap();
        assert!(remember(dir.path(), &input).contains_key("test"));
        assert_eq!(Kept::read(dir.path()).changed("test"), None);
        std::fs::write(&input, b"other").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&input)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp))
            .unwrap();
        assert_eq!(Kept::read(dir.path()).changed("test"), Some(Some(input.display().to_string())));
        std::fs::write(&input, b"first").unwrap();
        assert_eq!(Kept::read(dir.path()).changed("test"), None);
    }

    #[cfg(unix)]
    #[test]
    fn recorded_inputs_include_file_and_directory_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        for directory in [false, true] {
            let input = dir.path().join(if directory { "directory" } else { "tool" });
            if directory {
                std::fs::create_dir(&input).unwrap();
            } else {
                std::fs::write(&input, b"tool").unwrap();
            }
            std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(remember(dir.path(), &input).contains_key("test"));
            std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(
                Kept::read(dir.path()).changed("test"),
                Some(Some(input.display().to_string()))
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn recorded_symlinks_include_raw_target_spelling() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        std::fs::write(dir.path().join("target"), b"contents").unwrap();
        symlink("target", &input).unwrap();
        assert!(remember(dir.path(), &input).contains_key("test"));
        assert_eq!(Kept::read(dir.path()).changed("test"), None);
        std::fs::remove_file(&input).unwrap();
        symlink("./target", &input).unwrap();
        assert_eq!(
            std::fs::read_link(&input).unwrap().into_os_string(),
            std::ffi::OsString::from("./target")
        );
        assert_eq!(Kept::read(dir.path()).changed("test"), Some(Some(input.display().to_string())));
        assert!(remember(dir.path(), &input).contains_key("test"));
        assert_eq!(Kept::read(dir.path()).changed("test"), None);
    }

    #[cfg(unix)]
    #[test]
    fn recorded_symlinks_detect_target_content_changes() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let target = dir.path().join("target");
        std::fs::write(&target, b"first").unwrap();
        symlink("target", &input).unwrap();
        let stamp = std::fs::metadata(&target).unwrap().modified().unwrap();
        assert!(remember(dir.path(), &input).contains_key("test"));
        assert_eq!(Kept::read(dir.path()).changed("test"), None);
        std::fs::write(&target, b"other").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp))
            .unwrap();
        assert_eq!(Kept::read(dir.path()).changed("test"), Some(Some(input.display().to_string())));
        std::fs::write(&target, b"first").unwrap();
        assert_eq!(Kept::read(dir.path()).changed("test"), None);
    }

    #[cfg(unix)]
    #[test]
    fn recorded_dangling_symlinks_include_target_text_and_presence() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        assert!(remember(dir.path(), &input).contains_key("test"));
        symlink("missing", &input).unwrap();
        assert_eq!(Kept::read(dir.path()).changed("test"), Some(Some(input.display().to_string())));
        assert!(remember(dir.path(), &input).contains_key("test"));
        assert_eq!(Kept::read(dir.path()).changed("test"), None);
        std::fs::remove_file(&input).unwrap();
        symlink("./missing", &input).unwrap();
        assert_eq!(Kept::read(dir.path()).changed("test"), Some(Some(input.display().to_string())));
        assert!(remember(dir.path(), &input).contains_key("test"));
        assert_eq!(Kept::read(dir.path()).changed("test"), None);
        std::fs::write(dir.path().join("missing"), b"present").unwrap();
        assert_eq!(Kept::read(dir.path()).changed("test"), Some(Some(input.display().to_string())));
    }

    #[cfg(unix)]
    #[test]
    fn unobservable_inputs_cannot_keep_a_sandboxed_pass() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        std::fs::write(&input, b"readable").unwrap();
        assert!(remember(dir.path(), &input).contains_key("test"));
        std::fs::remove_file(&input).unwrap();
        symlink("input", &input).unwrap();
        assert_eq!(Kept::read(dir.path()).changed("test"), Some(Some(input.display().to_string())));
        assert!(remember(dir.path(), &input).is_empty());
        std::fs::remove_file(&input).unwrap();
        symlink("other", &input).unwrap();
        symlink("input", dir.path().join("other")).unwrap();
        assert!(remember(dir.path(), &input).is_empty());
        assert!(remember(dir.path(), Path::new("/dev/null")).is_empty());
    }

    #[test]
    fn recorded_missing_inputs_are_distinct_from_present_files() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        assert!(remember(dir.path(), &input).contains_key("test"));
        assert_eq!(Kept::read(dir.path()).changed("test"), None);
        std::fs::write(&input, b"").unwrap();
        assert_eq!(Kept::read(dir.path()).changed("test"), Some(Some(input.display().to_string())));
    }
}
