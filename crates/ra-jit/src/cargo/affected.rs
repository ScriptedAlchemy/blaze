//! `RA_JIT_TESTS=affected`: `cargo ra-jit test` runs, of each program of tests the daemon
//! built, the tests whose end an edit could have changed since they last passed, and says how
//! many it left. The daemon writes beside the program a hash of each test's code with all it
//! reaches ([`plan::REACH`]). A test runs again when its hash is not the one it passed with,
//! when it reaches what its code does not decide (files, the environment, processes, the
//! network, code ra-jit can't compile), and when it has not passed. Unset, or `all`, runs
//! every test, as cargo does. A test that reaches the world outside its code runs in a
//! sandbox that records what it touched, and runs again only when that changed too
//! ([`sandboxed`]); where there is no sandbox, it runs every time.
//!
//! This is Bazel's `cache_test_results=auto` (docs/distributed.md, section 6): a pass is reused
//! while the test, the inputs its owner declared and the way it is run are the same. The owner
//! declares them, and opts tests out, in [`MANIFEST`]:
//!
//! - `no_cache` (or `external`, which Bazel gives the same effect) names tests that run every
//!   time, and a pass is never kept of a test that failed;
//! - `data` and `env` are what every test is given, as Bazel's `data` and `--test_env` are:
//!   they are part of what a pass was under, and a test that reaches files runs without a
//!   sandbox when `data` is declared, trusting that declaration as Bazel does;
//! - `flaky` names tests that get several attempts, and [`ATTEMPTS`] says how many a test gets
//!   in all, as Bazel's `flaky = True` and `--flaky_test_attempts` do.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::{
    cli::Invocation,
    plan, sandboxed,
    state::{Kind, Program, State},
};

pub(crate) const VAR: &str = "RA_JIT_TESTS";

/// The file in the root of the workspace where its owner opts tests out of the cache and
/// declares what they are given ([`Manifest`]).
pub(crate) const MANIFEST: &str = "blaze-tests.toml";

/// How many times a test that fails is run in all, before it fails the run: a number, or
/// `default` (Bazel's `--flaky_test_attempts` default) for [`FLAKY_ATTEMPTS`] of a test the
/// manifest marks `flaky` and one for the others.
pub(crate) const ATTEMPTS: &str = "RA_JIT_FLAKY_TEST_ATTEMPTS";

/// Additional inherited variables the owner declares for the sandboxed test environment.
pub(crate) fn declared_env(state: &State) -> Vec<String> {
    Manifest::read(&state.root).0.map(|it| it.env).unwrap_or_default()
}

/// The attempts of a test marked `flaky`, as Bazel gives it.
const FLAKY_ATTEMPTS: usize = 3;

/// The file beside a program of tests with the tests that passed, one a line: what
/// [`plan::REACH`] said of the test then, a tab and its name.
const PASSED: &str = "tests-passed";

/// The most bytes of names a program of tests is given to pick its tests by; past it, it runs
/// them all.
const NAMES_AT_MOST: usize = 64 << 10;

/// Whether this command runs only the tests an edit reached.
pub(crate) fn on() -> bool {
    std::env::var(VAR).is_ok_and(|it| it == "affected")
}

/// Has the daemon write [`plan::REACH`] at its builds of the programs of tests of `state`
/// while only the tests an edit reached are run, and stop when every test is.
pub(crate) fn ask(state: &State) {
    let on = on();
    for program in state.programs.iter().filter(|it| it.test) {
        let Some(dir) = state.plan_dir(program) else { continue };
        let wanted = dir.join(plan::REACH_WANTED);
        match on {
            true if wanted.is_file() => {}
            true => _ = std::fs::write(&wanted, ""),
            false => {
                _ = std::fs::remove_file(&wanted);
                _ = std::fs::remove_file(dir.join(plan::REACH));
            }
        }
    }
}

/// Forgets what the daemon wrote of the programs of `state`, which cargo built since.
pub(crate) fn forget(state: &State) {
    for program in state.programs.iter().filter(|it| it.test) {
        if let Some(dir) = state.plan_dir(program) {
            _ = std::fs::remove_file(dir.join(plan::REACH));
        }
    }
}

/// What of a program of tests this command runs.
pub(crate) enum Pick {
    /// What the command asks, every test unless it names some: nothing is known of what an
    /// edit reached, or the command picks the tests itself.
    Asked,
    /// The tests `names` names, of the `total` the program has, or all of them when `None`;
    /// `always` are those of them run every time, and `apart` those run each in a sandbox of
    /// its own, and why each runs.
    Some {
        names: Option<Vec<String>>,
        total: usize,
        always: Vec<Always>,
        apart: Vec<(String, String)>,
        passed: Passed,
    },
}

/// A test that is run every time, and why.
pub(crate) struct Always {
    pub(crate) name: String,
    pub(crate) why: String,
    /// The calls that take the test to what it reaches, from the test on, joined by ` -> `.
    pub(crate) path: Option<String>,
}

/// What is remembered of a program of tests once it passed.
pub(crate) struct Passed {
    file: PathBuf,
    /// Each test that passed before and was not run, and each that ran, by name.
    tests: BTreeMap<String, String>,
    /// What the sandboxed tests touched, when tests are sandboxed.
    kept: Option<sandboxed::Kept>,
}

impl Passed {
    /// Forgets that the tests `failed` passed, or, when the program did not say which failed,
    /// all the tests `ran` (all of them when `None`): each runs until it passes again, even
    /// as its code goes back to what passed before. The others that ran passed.
    pub(crate) fn failed(mut self, ran: Option<&[String]>, failed: Option<&[String]>) {
        match (failed, ran) {
            (Some(failed), _) => failed.iter().for_each(|it| _ = self.tests.remove(it)),
            (None, Some(ran)) => ran.iter().for_each(|it| _ = self.tests.remove(it)),
            (None, None) => self.tests.clear(),
        }
        self.passed();
    }

    /// Remembers that the tests that ran passed, and what the sandboxed ones touched.
    pub(crate) fn passed(mut self) {
        if let Some(kept) = self.kept.take() {
            kept.settle(&mut self.tests);
        }
        let mut text = String::new();
        for (name, identity) in &self.tests {
            text.push_str(&format!("{identity}\t{name}\n"));
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

/// Which tests of `test` this command runs. `sandbox` is what stands for the environment of
/// a sandboxed test ([`sandboxed::identity_of`]) when the tests that reach the world outside
/// their code run sandboxed; otherwise they run every time.
pub(crate) fn pick(
    asked: &Invocation,
    state: &State,
    test: &Program,
    sandbox: Option<&str>,
    command: &Command,
) -> Pick {
    if !on() || !asked.positional.is_empty() || !plain(&asked.trailing) {
        return Pick::Asked;
    }
    let Some(dir) = state.plan_dir(test) else { return Pick::Asked };
    let Ok(reach) = std::fs::read_to_string(dir.join(plan::REACH)) else { return Pick::Asked };
    let file = dir.join(PASSED);
    let before = std::fs::read_to_string(&file).unwrap_or_default();
    let before: BTreeMap<&str, &str> = before
        .lines()
        .filter_map(|it| it.split_once('\t'))
        .map(|(identity, name)| (name, identity))
        .collect();
    let manifest = Manifest::read(&state.root);
    let programs = format!(
        "{}{}",
        programs_run_by(state, test),
        manifest.given(&state.root, &asked.trailing, command)
    );
    let mut kept = sandbox.map(|_| sandboxed::Kept::read(dir));
    let (mut names, mut tests, mut always, mut apart, mut total) =
        (Vec::new(), BTreeMap::new(), Vec::new(), Vec::new(), 0);
    for line in reach.lines() {
        let mut parts = line.split('\t');
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else { continue };
        if hash == "*" {
            return Pick::Asked;
        }
        total += 1;
        if let Some(why) = manifest.opts_out(&test.package, name) {
            names.push(name.to_owned());
            always.push(Always { name: name.to_owned(), why, path: None });
            continue;
        }
        let outside = hash.strip_prefix('~');
        // Without a sandbox to record what a test reads, the files it was declared to be given
        // are all there is to go by.
        let unrecorded = sandbox.is_none() && !manifest.declares_data();
        if hash == "-" || outside.is_some() && unrecorded {
            names.push(name.to_owned());
            let why = parts.next().unwrap_or_default().to_owned();
            let path = parts.next().map(str::to_owned);
            always.push(Always { name: name.to_owned(), why, path });
            continue;
        }
        let identity = match (outside, sandbox) {
            (Some(hash), Some(env)) => format!("{hash}{programs}{env}"),
            _ => format!("{hash}{programs}"),
        };
        let same = before.get(name) == Some(&identity.as_str());
        if let (Some(_), Some(kept)) = (outside, kept.as_mut()) {
            let why = match same {
                false => Some("its code changed, or it has not passed".to_owned()),
                true => kept.changed(name).map(|path| match path {
                    Some(path) => format!("it read `{path}`, which changed"),
                    None => "what it read is not known".to_owned(),
                }),
            };
            if let Some(why) = why {
                names.push(name.to_owned());
                kept.ran.insert(name.to_owned());
                apart.push((name.to_owned(), why));
            }
        } else if !same {
            names.push(name.to_owned());
        }
        tests.insert(name.to_owned(), identity);
    }
    let long = names.iter().map(|it| it.len() + 1).sum::<usize>() > NAMES_AT_MOST;
    let names = (names.len() != total && !long).then_some(names);
    Pick::Some { names, total, always, apart, passed: Passed { file, tests, kept } }
}

/// Whether what follows `--` leaves the tests to the program to pick and says nothing of how
/// a test ends: what only shows the run otherwise. One that runs the ignored tests, or lists
/// them, runs as asked.
fn plain(trailing: &[String]) -> bool {
    let mut args = trailing.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-q" | "--quiet" | "--nocapture" | "--no-capture" | "--show-output" => {}
            "--test-threads" | "--color" if args.next().is_some() => {}
            _ if arg.starts_with("--test-threads=") || arg.starts_with("--color=") => {}
            _ => return false,
        }
    }
    true
}

/// What stands for the programs an integration test of `test`'s package runs, which cargo
/// gives it as `CARGO_BIN_EXE_<name>`: their sizes and times, which a build changes. Empty
/// for a unit test.
fn programs_run_by(state: &State, test: &Program) -> String {
    if !matches!(test.kind, Kind::Test | Kind::Bench) {
        return String::new();
    }
    let mut hash = Sha256::new();
    let runs = |it: &&Program| it.package == test.package && it.kind == Kind::Bin && !it.test;
    for program in state.programs.iter().filter(runs) {
        hash.update(program.executable.as_os_str().as_encoded_bytes());
        let meta = std::fs::metadata(&program.executable).ok();
        let at = meta.as_ref().and_then(|it| it.modified().ok());
        let at = at.and_then(|it| it.duration_since(std::time::UNIX_EPOCH).ok());
        hash.update(meta.map_or(0, |it| it.len()).to_le_bytes());
        hash.update(at.map_or(0, |it| it.as_nanos()).to_le_bytes());
    }
    let hex: String = hash.finalize().iter().take(8).map(|it| format!("{it:02x}")).collect();
    format!("-{hex}")
}

/// What the owner wrote in [`MANIFEST`], or why it can't be read.
struct Manifest(Result<Declared, String>);

/// The keys of [`MANIFEST`]. Each is a list; a test is named by its name as libtest shows it
/// or `package/name`, and a `*` at either end stands for any text.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Declared {
    /// Tests that run every time: Bazel's `no-cache` tag.
    no_cache: Vec<String>,
    /// Tests that run every time: Bazel's `external` tag, which has the same effect here.
    external: Vec<String>,
    /// Files and directories every test is given, relative to the manifest: Bazel's `data`.
    data: Vec<String>,
    /// Variables of the environment every test is given: Bazel's `--test_env`.
    env: Vec<String>,
    /// Tests that get [`FLAKY_ATTEMPTS`]: Bazel's `flaky = True`.
    flaky: Vec<String>,
}

impl Manifest {
    fn read(root: &Path) -> Manifest {
        let file = root.join(MANIFEST);
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Manifest(Ok(Declared::default()));
            }
            Err(error) => return Manifest(Err(format!("{}: {error}", file.display()))),
        };
        // A key it does not have is refused rather than ignored: a typo would leave a test
        // cached that its owner opted out.
        Manifest(toml::from_str(&text).map_err(|error| format!("{}: {error}", file.display())))
    }

    /// Why the owner has the test `name` of `package` run every time, if it does. A manifest
    /// that can't be read opts every test out: nothing is known of which it marks.
    fn opts_out(&self, package: &str, name: &str) -> Option<String> {
        let declared = match &self.0 {
            Ok(declared) => declared,
            Err(why) => return Some(format!("`{MANIFEST}` can't be read ({why})")),
        };
        let tags = [("no_cache", &declared.no_cache), ("external", &declared.external)];
        tags.into_iter().find_map(|(tag, names)| {
            let named = names.iter().find(|it| names_test(it, package, name))?;
            Some(format!("`{MANIFEST}` opts it out of the cache (`{tag}`: `{named}`)"))
        })
    }

    /// Whether the owner declared files the tests are given.
    fn declares_data(&self) -> bool {
        self.0.as_ref().is_ok_and(|it| !it.data.is_empty())
    }

    /// What stands for how the tests are run and what they are given: `--test-threads`, the
    /// contents of the files of `data` (and which are not there), and the value of each
    /// variable of `env` and of [`sandboxed::PASS`]. A pass was under it, and a test whose
    /// pass was under another runs.
    fn given(&self, root: &Path, trailing: &[String], command: &Command) -> String {
        let Ok(declared) = &self.0 else { return "+unreadable".to_owned() };
        let mut hash = Sha256::new();
        hash.update(b"threads\0");
        hash.update(test_threads(trailing).unwrap_or_default().as_bytes());
        hash.update([0]);
        hash.update(std::env::var_os("RUST_TEST_THREADS").unwrap_or_default().as_encoded_bytes());
        hash.update(b"\0data\0");
        for path in &declared.data {
            hash.update(path.as_bytes());
            hash.update([0]);
            feed(&mut hash, &root.join(path));
        }
        let passed = std::env::var(sandboxed::PASS).unwrap_or_default();
        let mut names: Vec<&str> =
            declared.env.iter().map(String::as_str).chain(passed.split(',')).collect();
        names.retain(|it| !it.trim().is_empty());
        names.sort_unstable();
        names.dedup();
        hash.update(b"env\0");
        for name in names {
            hash.update(name.as_bytes());
            hash.update([0]);
            let value = command.get_envs().find(|(key, _)| *key == name);
            let value = value
                .map_or_else(|| std::env::var_os(name), |(_, value)| value.map(|it| it.to_owned()));
            match value {
                Some(value) => {
                    hash.update(b"set\0");
                    hash.update(value.as_encoded_bytes());
                }
                None => hash.update(b"unset"),
            }
            hash.update([0]);
        }
        let hex: String = hash.finalize().iter().take(8).map(|it| format!("{it:02x}")).collect();
        format!("+{hex}")
    }

    fn marks_flaky(&self, package: &str, name: &str) -> bool {
        let flaky = self.0.as_ref().map(|it| &it.flaky[..]).unwrap_or_default();
        flaky.iter().any(|it| names_test(it, package, name))
    }
}

/// Whether `pattern`, a name as libtest shows it or `package/name`, names the test.
fn names_test(pattern: &str, package: &str, name: &str) -> bool {
    glob(pattern, name) || glob(pattern, &format!("{package}/{name}"))
}

/// Whether `text` is `pattern`, which may have a `*` at its start or at its end for any text.
fn glob(pattern: &str, text: &str) -> bool {
    match (pattern.strip_prefix('*'), pattern.strip_suffix('*')) {
        _ if pattern == "*" => true,
        (Some(tail), _) if !tail.contains('*') => text.ends_with(tail),
        (_, Some(head)) if !head.contains('*') => text.starts_with(head),
        _ => pattern == text,
    }
}

/// The value after `--test-threads` in what follows `--`, which is how the tests are run that
/// can change how they end.
fn test_threads(trailing: &[String]) -> Option<&str> {
    let mut args = trailing.iter();
    while let Some(arg) = args.next() {
        match arg.strip_prefix("--test-threads=") {
            Some(value) => return Some(value),
            None if arg == "--test-threads" => return args.next().map(String::as_str),
            None => {}
        }
    }
    None
}

/// A typed tree encoding: counted directories, length-prefixed names, and fixed-size child
/// digests. File bytes are independently digested, so they cannot encode directory boundaries.
fn feed(hash: &mut Sha256, path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else { return hash.update([0]) };
    if meta.is_dir() {
        let names = std::fs::read_dir(path).and_then(|entries| {
            entries.map(|entry| entry.map(|it| it.file_name())).collect::<std::io::Result<Vec<_>>>()
        });
        let Ok(mut names) = names else { return hash.update([3]) };
        hash.update([1]);
        names.sort();
        hash.update((names.len() as u64).to_le_bytes());
        for name in names {
            hash.update((name.as_encoded_bytes().len() as u64).to_le_bytes());
            hash.update(name.as_encoded_bytes());
            let mut child = Sha256::new();
            feed(&mut child, &path.join(name));
            hash.update(child.finalize());
        }
    } else {
        hash.update([2]);
        match std::fs::File::open(path) {
            Ok(mut file) => {
                let mut content = Sha256::new();
                match std::io::copy(&mut file, &mut content) {
                    Ok(len) => {
                        hash.update([0]);
                        hash.update(len.to_le_bytes());
                        hash.update(content.finalize());
                    }
                    Err(_) => hash.update([1]),
                }
            }
            Err(_) => hash.update([1]),
        }
    }
}

/// How many attempts each test gets before it fails the run.
pub(crate) struct Attempts {
    /// What [`ATTEMPTS`] says, when it says a number.
    all: Option<usize>,
    manifest: Manifest,
}

pub(crate) fn attempts(state: &State) -> Attempts {
    let all = std::env::var(ATTEMPTS).ok().and_then(|it| it.trim().parse().ok());
    Attempts { all: all.filter(|&it| it >= 1), manifest: Manifest::read(&state.root) }
}

impl Attempts {
    #[cfg(test)]
    pub(super) fn with_flaky(names: &[&str]) -> Self {
        Self {
            all: None,
            manifest: Manifest(Ok(Declared {
                flaky: names.iter().map(|it| (*it).to_owned()).collect(),
                ..Declared::default()
            })),
        }
    }

    /// The attempts of the test `name` of `package`: the number asked for, or, as Bazel's
    /// `default` has it, three for a test the manifest marks `flaky` and one for the others.
    pub(crate) fn of(&self, package: &str, name: &str) -> usize {
        match self.all {
            Some(all) => all,
            None if self.manifest.marks_flaky(package, name) => FLAKY_ATTEMPTS,
            None => 1,
        }
    }

    /// Whether some test may get more than one, so that failures have to be listed.
    pub(crate) fn any(&self) -> bool {
        self.all.is_some_and(|it| it > 1)
            || self.all.is_none() && self.manifest.0.as_ref().is_ok_and(|it| !it.flaky.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(args: &[&str]) -> bool {
        plain(&args.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn what_only_shows_the_run_is_plain() {
        assert!(of(&[]));
        assert!(of(&["--nocapture", "-q", "--test-threads", "4", "--color=never"]));
        assert!(!of(&["--ignored"]));
        assert!(!of(&["--include-ignored"]));
        assert!(!of(&["--list"]));
        assert!(!of(&["--test-threads"]));
        assert!(!of(&["some_test"]));
    }

    fn manifest(text: &str) -> Manifest {
        Manifest(toml::from_str(text).map_err(|error| error.to_string()))
    }

    #[test]
    fn a_name_opts_a_test_out_the_way_bazels_tags_do() {
        let manifest = manifest(
            "no_cache = [\"tests::clock\", \"slow::*\"]\nexternal = [\"tsel/it_net\", \"*::net\"]\n",
        );
        let why = |package, name| manifest.opts_out(package, name);
        assert!(why("tsel", "tests::clock").unwrap().contains("`no_cache`"));
        assert!(why("other", "slow::sort").is_some());
        assert!(why("tsel", "it_net").unwrap().contains("`external`"));
        assert!(why("other", "it_net").is_none());
        assert!(why("other", "a::net").is_some());
        assert!(why("tsel", "tests::plain").is_none());
        // What can't be read opts every test out.
        let broken = Manifest(Err("expected `=`".to_owned()));
        assert!(broken.opts_out("tsel", "tests::plain").unwrap().contains("can't be read"));
    }

    #[test]
    fn the_manifest_is_read_from_the_root_and_refuses_what_it_does_not_have() {
        let dir = std::env::temp_dir().join(format!("ra-jit-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Manifest::read(&dir).0.is_ok_and(|it| it.no_cache.is_empty()));
        std::fs::write(dir.join(MANIFEST), "no_cache = [\"a\"]\ndata = [\"d\"]\nflaky = [\"f\"]\n")
            .unwrap();
        let read = Manifest::read(&dir);
        assert!(read.declares_data() && read.marks_flaky("p", "f") && !read.marks_flaky("p", "g"));
        std::fs::write(dir.join(MANIFEST), "no_cash = [\"a\"]\n").unwrap();
        assert!(Manifest::read(&dir).0.is_err());
        std::fs::write(dir.join(MANIFEST), "no_cache = [\"a\"\n").unwrap();
        assert!(Manifest::read(&dir).0.is_err());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pass_is_under_the_inputs_the_owner_declared_and_how_the_tests_are_run() {
        let dir = std::env::temp_dir().join(format!("ra-jit-given-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("data/a.txt"), "a").unwrap();
        let declared = manifest("data = [\"data\", \"missing.txt\"]\nenv = [\"T1_GIVEN\"]\n");
        let given = |it: &Manifest, args: &[&str]| {
            let args: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
            it.given(&dir, &args, &Command::new("test"))
        };
        let before = given(&declared, &[]);
        assert_eq!(before, given(&declared, &["--nocapture"]), "display options are not it");
        assert_ne!(before, given(&declared, &["--test-threads=1"]));
        assert_eq!(
            given(&declared, &["--test-threads=1"]),
            given(&declared, &["--test-threads", "1"])
        );
        std::fs::write(dir.join("data/a.txt"), "b").unwrap();
        assert_ne!(before, given(&declared, &[]), "a declared file changed");
        std::fs::write(dir.join("data/a.txt"), "a").unwrap();
        assert_eq!(before, given(&declared, &[]));
        std::fs::write(dir.join("data/new.txt"), "").unwrap();
        assert_ne!(before, given(&declared, &[]), "a file was added to a declared directory");
        std::fs::remove_file(dir.join("data/new.txt")).unwrap();
        std::fs::write(dir.join("missing.txt"), "").unwrap();
        assert_ne!(before, given(&declared, &[]), "a declared path that was absent exists");
        std::fs::remove_file(dir.join("missing.txt")).unwrap();
        assert_eq!(before, given(&declared, &[]));
        assert_ne!(before, given(&manifest(""), &[]));
        let mut command = Command::new("test");
        command.env("T1_GIVEN", "configured");
        assert_ne!(
            before,
            declared.given(&dir, &[], &command),
            "the effective test environment is keyed"
        );
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn moving_a_file_out_of_a_directory_changes_the_declared_key() {
        let dir = std::env::temp_dir().join(format!("ra-jit-tree-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("d/a")).unwrap();
        std::fs::write(dir.join("d/a/b"), "hello").unwrap();
        let declared = manifest("data = [\"d\"]");
        let before = declared.given(&dir, &[], &Command::new("test"));
        std::fs::rename(dir.join("d/a/b"), dir.join("d/b")).unwrap();
        assert_ne!(before, declared.given(&dir, &[], &Command::new("test")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_test_gets_the_attempts_asked_for_or_bazels_default() {
        let flaky = manifest("flaky = [\"tests::net\"]\n");
        let by_default = Attempts { all: None, manifest: flaky };
        assert_eq!(by_default.of("p", "tests::net"), FLAKY_ATTEMPTS);
        assert_eq!(by_default.of("p", "tests::plain"), 1);
        assert!(by_default.any());
        let asked = Attempts { all: Some(2), manifest: manifest("flaky = [\"tests::net\"]\n") };
        assert_eq!((asked.of("p", "tests::net"), asked.of("p", "tests::plain")), (2, 2));
        let once = Attempts { all: Some(1), manifest: manifest("flaky = [\"tests::net\"]\n") };
        assert_eq!(once.of("p", "tests::net"), 1);
        assert!(!once.any() && !Attempts { all: None, manifest: manifest("") }.any());
    }

    #[test]
    fn successful_results_are_kept_and_failures_are_removed() {
        let dir = std::env::temp_dir().join(format!("ra-jit-retried-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tests = [("a", "1"), ("b", "2")].map(|(n, i)| (n.to_owned(), i.to_owned())).into();
        let passed = Passed { file: dir.join(PASSED), tests, kept: None };
        passed.failed(None, Some(&["a".to_owned()]));
        assert_eq!(std::fs::read_to_string(dir.join(PASSED)).unwrap(), "2\tb\n");
        _ = std::fs::remove_dir_all(&dir);
    }
}
