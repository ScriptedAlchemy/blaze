//! Running what was built, as cargo does: the program of `cargo run`, the test executables
//! of `cargo test`.

use std::{
    path::Path,
    process::{Command, ExitStatus},
};

use anyhow::Context as _;

use crate::plan::Selection;

use super::{
    affected::{self, Pick},
    cli::Invocation,
    sandboxed, say,
    state::{Held, Kind, Program, State, as_built, cargo_path},
};

/// The exit status of cargo when a test fails or a program can't be run.
const FAILED: i32 = 101;

/// Runs the program `asked` names, and ends as it ended. `None` when cargo has to say
/// which program that is: there are several, or none.
pub(crate) fn program(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
    held: Option<Held>,
) -> anyhow::Result<Option<i32>> {
    let runnable = |it: &&Program| !it.test && matches!(it.kind, Kind::Bin | Kind::Example);
    let mut programs: Vec<&Program> = state.programs.iter().filter(runnable).collect();
    if let Some(name) = asked.flags.bin.as_ref().or(asked.flags.example.as_ref()) {
        programs.retain(|it| it.name == *name);
    } else if let [default] = programs.iter().filter(|it| it.default_run).collect::<Vec<_>>()[..] {
        // Cargo runs a `default-run` when one package it builds has one.
        programs = vec![*default];
    }
    let &[program] = programs.as_slice() else { return Ok(None) };
    let shown = shown_path(&program.executable, cwd);
    let args = asked.program_args().map(|it| format!(" {it}")).collect::<String>();
    say::status(asked.flags.quiet, "Running", &format!("`{shown}{args}`"));
    let mut command = Command::new(&program.executable);
    configured(&mut command, state);
    command.args(asked.program_args()).envs(program.env.iter().cloned());
    as_built(&mut command)?;
    let status =
        ended(&mut command, held, state).with_context(|| format!("could not execute {shown}"))?;
    Ok(Some(exit_code(status)))
}

/// Sets for `command` what the `[env]` of cargo's configuration sets, as cargo does: what
/// cargo sets for the program itself goes over it.
fn configured(command: &mut Command, state: &State) {
    for it in &state.configured {
        if it.force || std::env::var_os(&it.name).is_none() {
            command.env(&it.name, &it.value);
        }
    }
}

/// Runs `command` to its end, started under `held` or under cargo's locks on the build
/// directories taken now: a cargo building there meanwhile removes the executables and
/// writes them again, and one started then is not there.
fn ended(command: &mut Command, held: Option<Held>, state: &State) -> std::io::Result<ExitStatus> {
    let held = held.unwrap_or_else(|| state.hold());
    let started = started(command);
    drop(held);
    started?.wait()
}

/// The most of what a program of tests printed last that is kept to read its failures from:
/// one whose list of them is longer is taken to have said none.
const PRINTED_KEPT: usize = 4 << 20;

/// Runs `command` as [`ended`] does, with what it prints passed on as it comes, and the tests
/// libtest's summary at its end says failed: `None` when it says none, as a program that
/// crashed, or exited in a test, does not. libtest colors its output as it would on this
/// terminal, which it does not see through the pipe.
fn listing_failures(
    command: &mut Command,
    held: Option<Held>,
    state: &State,
) -> std::io::Result<(ExitStatus, Option<Vec<String>>)> {
    use std::io::{IsTerminal as _, Read as _, Write as _};
    let out = std::io::stdout();
    let colored = command.get_args().any(|it| {
        let it = it.to_string_lossy();
        it == "--color" || it.starts_with("--color=")
    });
    if !colored && out.is_terminal() {
        command.arg("--color=always");
    }
    command.stdout(std::process::Stdio::piped());
    let held = held.unwrap_or_else(|| state.hold());
    let child = started(command);
    drop(held);
    let mut child = child?;
    let mut printed = Vec::new();
    if let Some(mut from) = child.stdout.take() {
        let mut chunk = [0; 8192];
        loop {
            let read = match from.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            // A reader that went away does not stop the tests, as it would not under cargo.
            let mut out = out.lock();
            _ = out.write_all(&chunk[..read]).and_then(|()| out.flush());
            printed.extend_from_slice(&chunk[..read]);
            if printed.len() > PRINTED_KEPT {
                printed.drain(..printed.len() - PRINTED_KEPT);
            }
        }
    }
    let status = child.wait()?;
    Ok((status, failures(&String::from_utf8_lossy(&printed))))
}

/// The tests libtest's summary lists as failed in what a program of tests printed: the names
/// under the last `failures:` before `test result: FAILED`.
fn failures(printed: &str) -> Option<Vec<String>> {
    let plain = without_colors(printed);
    let end = plain.rfind("\ntest result: FAILED")?;
    let start = plain[..end].rfind("\nfailures:\n")? + "\nfailures:\n".len();
    let names: Vec<String> = plain[start..end]
        .lines()
        .filter_map(|it| it.strip_prefix("    "))
        .map(str::to_owned)
        .collect();
    (!names.is_empty()).then_some(names)
}

/// `text` without the escapes that color a terminal's text.
fn without_colors(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// Starts `command` once the daemon is not writing its program (`splice::unwritten`): one
/// started while the file is open for writing is refused ("text file busy").
pub(crate) fn started(command: &mut Command) -> std::io::Result<std::process::Child> {
    #[cfg(unix)]
    let _unwritten = crate::splice::unwritten(command.get_program().as_ref());
    command.spawn()
}

/// Runs the test executables one after another, each in the directory of its package, and
/// stops at the first that fails unless told not to. The result is cargo's exit status. Under
/// `RA_JIT_TESTS=affected` each runs only the tests an edit reached since they passed
/// ([`affected`]).
pub(crate) fn tests(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
    mut held: Option<Held>,
) -> anyhow::Result<i32> {
    let mut failed = Vec::new();
    let (mut left, mut always_run) = (0, 0);
    let sandbox = sandboxed::wanted(affected::on());
    let declared_env = affected::declared_env(state);
    let attempts = affected::attempts(state);
    if let Some(Err(why)) = &sandbox {
        say::fallback(
            &format!("tests run outside a sandbox: {why}"),
            "those that reach files, the environment, processes or the network run every time",
        );
    }
    for test in state.programs.iter().filter(|it| it.test) {
        let source = test.source.strip_prefix(&test.package_dir).unwrap_or(&test.source);
        let unit = if matches!(test.kind, Kind::Test | Kind::Bench) { "" } else { "unittests " };
        let shown = shown_path(&test.executable, cwd);
        let mut running = format!("{unit}{} ({shown})", source.display());
        let mut picked = Vec::new();
        let mut command = test_command(state, test)?;
        let sandbox_env =
            matches!(sandbox, Some(Ok(()))).then(|| sandboxed::env(&command, &declared_env));
        let identity = sandbox_env.as_deref().map(sandboxed::identity_of);
        let pick = affected::pick(asked, state, test, identity.as_deref(), &command);
        if let Pick::Some { names: Some(names), total, .. } = &pick {
            left += total - names.len();
            if names.is_empty() {
                let unchanged = format!("{}, unchanged since the last pass", counted(*total));
                say::status(asked.flags.quiet, "Skipping", &format!("{running}: {unchanged}"));
                continue;
            }
            running.push_str(&format!(": {} of {}", names.len(), counted(*total)));
            picked.push("--exact".to_owned());
            picked.extend(names.iter().cloned());
        }
        say::status(asked.flags.quiet, "Running", &running);
        if let Pick::Some { always, apart, .. } = &pick {
            always_run += always.len();
            for test in always.iter().filter(|_| asked.flags.verbose) {
                let by = test.path.as_ref().map(|it| format!(", by {it}")).unwrap_or_default();
                say::status(
                    asked.flags.quiet,
                    "Always",
                    &format!("{}: {}{by}", test.name, test.why),
                );
            }
            for (name, why) in apart.iter().filter(|_| asked.flags.verbose) {
                say::status(asked.flags.quiet, "Sandboxed", &format!("{name}: {why}"));
            }
            let names: Vec<String> = apart.iter().map(|(name, _)| name.clone()).collect();
            if let (Some(env), Some(dir), false) =
                (sandbox_env, state.plan_dir(test), names.is_empty())
            {
                sandboxed::prepare(state, test, dir, &mut command, env, &names)
                    .context("writing what the sandbox of the tests is")?;
            }
        }
        command.args(asked.program_args()).args(&picked);
        // The tests a sandbox runs, which a run again of some of them is given too.
        let sandbox_list = command
            .get_envs()
            .find(|(name, _)| *name == crate::test_harness::SANDBOXED)
            .and_then(|(_, list)| list.map(|it| it.to_owned()));
        let listing = matches!(pick, Pick::Some { .. }) || attempts.any();
        let ran = match listing {
            true => listing_failures(&mut command, held.take(), state),
            false => ended(&mut command, held.take(), state).map(|it| (it, None)),
        };
        let (mut status, mut failures) =
            ran.with_context(|| format!("could not execute {shown}"))?;
        // A test that fails runs again while it has attempts left (Bazel's
        // `flaky_test_attempts`), and only the tests that failed do.
        let (mut attempt, mut retried) = (1, Vec::new());
        let mut exhausted = Vec::new();
        while !status.success() {
            let Some(failed) = failures.take() else { break };
            let (retry, done) = retryable(&attempts, &test.package, failed, attempt);
            exhausted.extend(done);
            if retry.is_empty() {
                failures = Some(exhausted.clone());
                break;
            }
            attempt += 1;
            say::status(
                asked.flags.quiet,
                "Retrying",
                &format!("{running}: {} (attempt {attempt})", counted(retry.len())),
            );
            retried.extend(retry.iter().cloned());
            let mut again = test_command(state, test)?;
            if let Some(list) = &sandbox_list {
                again.env(crate::test_harness::SANDBOXED, list);
            }
            // Each attempt is built from what the command asked, never from the argv of the
            // attempt before: libtest refuses an option it is given twice.
            again.args(retry_args(&asked.program_args().cloned().collect::<Vec<_>>(), &retry));
            (status, failures) = listing_failures(&mut again, None, state)
                .with_context(|| format!("could not execute {shown}"))?;
        }
        if status.success() && !exhausted.is_empty() {
            failures = Some(exhausted);
        } else if let Some(failures) = failures.as_mut() {
            failures.extend(exhausted);
            failures.sort();
            failures.dedup();
        }
        let success = status.success() && failures.as_ref().is_none_or(Vec::is_empty);
        // Those that passed on a later attempt are flaky, but still passed: Bazel's auto
        // caching reuses a successful result even when it took retries.
        retried.sort();
        retried.dedup();
        if !status.success() && failures.is_none() {
            // A crashed retry has no summary proving which of its tests passed.
            retried.clear();
        }
        retried.retain(|it| !failures.as_ref().is_some_and(|failed| failed.contains(it)));
        if !retried.is_empty() {
            let names = retried.join(", ");
            say::status(
                asked.flags.quiet,
                "Flaky",
                &format!("{names}: passed on attempt {attempt}"),
            );
        }
        match pick {
            Pick::Some { passed, names, .. } if !success => {
                passed.failed(names.as_deref(), failures.as_deref())
            }
            Pick::Some { passed, .. } => passed.passed(),
            Pick::Asked => {}
        }
        if !success {
            failed.push(rerun_flags(test));
            if !asked.flags.no_fail_fast {
                break;
            }
        }
    }
    let why = if asked.flags.verbose { "said above" } else { "`-v` says why" };
    let always = format!("{} run every time ({why})", counted(always_run));
    if left > 0 {
        let all = format!("`{}=all` runs every test", affected::VAR);
        let unchanged = format!("{}, unchanged since the last pass", counted(left));
        let unchanged = match always_run {
            0 => format!("{unchanged}; {all}"),
            _ => format!("{unchanged}; {always}; {all}"),
        };
        say::status(asked.flags.quiet, "Skipped", &unchanged);
    } else if always_run > 0 {
        say::status(asked.flags.quiet, "Skipped", &format!("no test; {always}"));
    }
    match failed.as_slice() {
        [] => Ok(0),
        [one] => {
            say::error(&format!("test failed, to rerun pass `{one}`"));
            Ok(FAILED)
        }
        several => {
            say::error(&format!("{} targets failed:", several.len()));
            several.iter().for_each(|it| eprintln!("    `{it}`"));
            Ok(FAILED)
        }
    }
}

/// A failure that exhausted its own attempts must not prevent another test from retrying.
fn retryable(
    attempts: &affected::Attempts,
    package: &str,
    failed: Vec<String>,
    attempt: usize,
) -> (Vec<String>, Vec<String>) {
    failed.into_iter().partition(|name| attempts.of(package, name) > attempt)
}

/// What a retry of the tests `retry` runs the test executable with, built from what the
/// command asked the tests (`asked`: the words before `--` and after it) rather than from the
/// argv of the attempt that failed. libtest refuses an option it is given twice, so the
/// command's own `--exact` and the names it selected are replaced by the one selection of
/// these tests: `cargo ra-jit test -- --exact tests::flaky_once` retries with one `--exact`.
/// The options that say how the tests run, and the ones that pick them out of the selection
/// (`--skip`, `--ignored`, `--include-ignored`), stay the command's, with their values.
fn retry_args(asked: &[String], retry: &[String]) -> Vec<String> {
    // libtest's options that take a value, which is the next word when it is not `=joined`:
    // `--skip` is given several times, the rest once.
    const VALUED: [&str; 7] =
        ["--logfile", "--test-threads", "--skip", "--color", "--format", "-Z", "--shuffle-seed"];
    let mut args = Vec::new();
    let mut words = asked.iter();
    while let Some(word) = words.next() {
        // After libtest's own `--` every word is a filter, which the retry's names replace.
        if word == "--" {
            break;
        }
        let (name, joined) = match word.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value)),
            _ => (word.as_str(), None),
        };
        if !name.starts_with('-') {
            // A filter naming tests: the tests of this retry name themselves.
            continue;
        }
        // The command's own selection is given once, below.
        if name == "--exact" {
            continue;
        }
        args.push(word.clone());
        if joined.is_none() && VALUED.contains(&name) {
            if let Some(value) = words.next() {
                args.push(value.clone());
            }
        }
    }
    args.push("--exact".to_owned());
    args.extend(retry.iter().cloned());
    args
}

/// Runs the doc tests with cargo, which hands them to ra-jit in rustdoc's place: ra-jit
/// builds each example as its own program over its code of the library.
pub(crate) fn doc_tests(asked: &Invocation) -> anyhow::Result<i32> {
    let mut args = vec!["--doc".to_owned()];
    args.extend(asked.build_args.iter().cloned());
    if asked.flags.no_fail_fast {
        args.push("--no-fail-fast".to_owned());
    }
    args.extend(asked.positional.iter().cloned());
    args.push("--".to_owned());
    args.extend(asked.trailing.iter().cloned());
    only_doc_tests(asked, &args)
}

/// Runs `cargo test` with `args`, which ask for the doc tests alone, with ra-jit in
/// rustdoc's place. ra-jit is told the build `asked` is, whose features cargo builds the
/// libraries with. Cargo's run of rustdoc for each library is only written down, and they are
/// made once cargo is done, by one run of ra-jit that loads the workspace once for them all.
pub(crate) fn only_doc_tests(asked: &Invocation, args: &[String]) -> anyhow::Result<i32> {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};
    let rustdoc = std::env::var_os("RUSTDOC").unwrap_or_else(|| "rustdoc".into());
    let cwd = std::env::current_dir()?;
    let asked_in = asked.flags.manifest_path.as_ref().map_or(cwd.clone(), |it| cwd.join(it));
    let build = Selection { asked_in: Some(asked_in), ..asked.selection() };
    let replay = crate::doctest::Replay {
        dir: std::env::temp_dir().join(format!("ra-jit-doctests-{}", std::process::id())),
        no_fail_fast: asked.flags.no_fail_fast,
        quiet: asked.flags.quiet,
    };
    std::fs::create_dir_all(&replay.dir)?;
    let mut cargo = crate::doctest::as_asked(cargo_path());
    cargo
        .env("RUSTDOC", std::env::current_exe().context("finding ra-jit itself")?)
        .env(crate::doctest::ENABLED, rustdoc)
        .env(crate::doctest::BUILD, serde_json::to_string(&build)?)
        .env(crate::doctest::RECORD, &replay.dir);
    // The daemon's store, as its workers have it: what the examples link of the standard
    // library is compiled once, not for each library each run.
    match super::daemon::store() {
        Some(store) => cargo.env("RA_JIT_CACHE", store),
        None => cargo.env_remove("RA_JIT_CACHE"),
    };
    // Cargo says `Doc-tests` of every library before any of them runs: the run that makes
    // them says it of each as it does. Its colors stay as they are on a terminal.
    cargo.stderr(std::process::Stdio::piped());
    let terminal = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if terminal && std::env::var_os("CARGO_TERM_COLOR").is_none() {
        cargo.env("CARGO_TERM_COLOR", "always");
    }
    cargo.arg("test").args(args);
    as_built(&mut cargo)?;
    let mut cargo = cargo.spawn().context("running `cargo test --doc`")?;
    let said = cargo.stderr.take().context("cargo's standard error is not read")?;
    let mut stderr = std::io::stderr().lock();
    for line in std::io::BufReader::new(said).split(b'\n') {
        let line = line?;
        if !says_doc_tests(&line) {
            stderr.write_all(&line)?;
            stderr.write_all(b"\n")?;
        }
    }
    drop(stderr);
    let code = exit_code(cargo.wait().context("running `cargo test --doc`")?);
    let replayed = match code {
        0 => replay.command().and_then(|ra_jit| {
            ra_jit
                .map(|mut it| Ok(exit_code(it.status().context("running the doctests")?)))
                .transpose()
        }),
        _ => Ok(None),
    };
    _ = std::fs::remove_dir_all(&replay.dir);
    Ok(replayed?.unwrap_or(code))
}

/// Whether cargo's line is the `Doc-tests <crate>` it says as it runs rustdoc for a library.
fn says_doc_tests(line: &[u8]) -> bool {
    let (text, mut plain) = (String::from_utf8_lossy(line), String::new());
    let mut chars = text.chars();
    while let Some(char) = chars.next() {
        match char {
            // A color: `ESC [ ... m`.
            '\x1b' => _ = chars.by_ref().find(char::is_ascii_alphabetic),
            _ => plain.push(char),
        }
    }
    let name = plain.trim().strip_prefix("Doc-tests ");
    name.is_some_and(|it| !it.is_empty() && !it.contains(' '))
}

/// The command that runs the test executable of `test` as cargo does.
fn test_command(state: &State, test: &Program) -> anyhow::Result<Command> {
    let mut command = Command::new(&test.executable);
    configured(&mut command, state);
    command.envs(test.env.iter().cloned());
    command.current_dir(&test.package_dir);
    as_built(&mut command)?;
    Ok(command)
}

fn counted(count: usize) -> String {
    match count {
        1 => "1 test".to_owned(),
        _ => format!("{count} tests"),
    }
}

/// The flags that have cargo run this test target alone.
fn rerun_flags(test: &Program) -> String {
    let target = match test.kind {
        Kind::Lib => "--lib".to_owned(),
        Kind::Bin => format!("--bin {}", test.name),
        Kind::Example => format!("--example {}", test.name),
        Kind::Test => format!("--test {}", test.name),
        Kind::Bench => format!("--bench {}", test.name),
    };
    format!("-p {} {target}", test.package)
}

/// The path as cargo shows it: from the directory the command runs in, when it is below.
fn shown_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// What this command ends with after a process that ended with `status`: its code, or
/// the code a shell gives one a signal ended.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::{failures, retry_args};

    fn retry_of(asked: &[&str], retry: &[&str]) -> Vec<String> {
        let asked: Vec<String> = asked.iter().map(|it| (*it).to_owned()).collect();
        let retry: Vec<String> = retry.iter().map(|it| (*it).to_owned()).collect();
        retry_args(&asked, &retry)
    }

    #[test]
    fn a_retry_selects_its_tests_once_from_what_was_asked() {
        // `cargo ra-jit test -p flaky -- --exact tests::flaky_once`: libtest refuses a second
        // `--exact`, so the command's is the one the retry is given, not a second one.
        assert_eq!(
            retry_of(&["--exact", "tests::flaky_once"], &["tests::flaky_once"]),
            ["--exact", "tests::flaky_once"]
        );
        // A filter that is not exact is the retry's own selection too: the names replace it.
        assert_eq!(
            retry_of(&["flaky_once"], &["tests::flaky_once"]),
            ["--exact", "tests::flaky_once"]
        );
        // How the tests run, and what picks them out of the selection, stay as they were.
        assert_eq!(
            retry_of(
                &[
                    "--test-threads=2",
                    "--skip",
                    "tests::steady",
                    "--ignored",
                    "--exact",
                    "flaky_once"
                ],
                &["tests::flaky_once"]
            ),
            [
                "--test-threads=2",
                "--skip",
                "tests::steady",
                "--ignored",
                "--exact",
                "tests::flaky_once"
            ]
        );
        assert_eq!(
            retry_of(&["--include-ignored", "--color", "never"], &["tests::flaky_once"]),
            ["--include-ignored", "--color", "never", "--exact", "tests::flaky_once"]
        );
        // libtest's own `--` ends its options: the filters after it are replaced, and the
        // retry's `--exact` stays an option rather than becoming a filter.
        assert_eq!(
            retry_of(&["--exact", "--", "tests::flaky"], &["tests::flaky_once"]),
            ["--exact", "tests::flaky_once"]
        );
        assert_eq!(
            retry_of(&["--test-threads", "1", "--", "--odd-name"], &["tests::flaky_once"]),
            ["--test-threads", "1", "--exact", "tests::flaky_once"]
        );
        // Two tests that failed both name themselves.
        assert_eq!(
            retry_of(&[], &["tests::flaky_once", "tests::steady"]),
            ["--exact", "tests::flaky_once", "tests::steady"]
        );
    }

    #[test]
    fn a_non_flaky_failure_does_not_block_a_flaky_tests_attempts() {
        let attempts = super::affected::Attempts::with_flaky(&["flaky"]);
        let failed = ["ordinary", "flaky"].map(str::to_owned).to_vec();
        let (retry, exhausted) = super::retryable(&attempts, "p", failed, 1);
        assert_eq!(retry, ["flaky"]);
        assert_eq!(exhausted, ["ordinary"]);
        let (retry, exhausted) = super::retryable(&attempts, "p", retry, 3);
        assert!(retry.is_empty());
        assert_eq!(exhausted, ["flaky"]);
    }

    #[test]
    fn the_summary_names_the_failures() {
        let pretty = "\nrunning 3 tests\ntest a ... ok\ntest b ... FAILED\ntest c::d ... FAILED\n\n\
            failures:\n\n---- b stdout ----\nfailures:\n    not_a_test\n\n---- c::d stdout ----\n\n\
            failures:\n    b\n    c::d\n\ntest result: FAILED. 1 passed; 2 failed; 0 ignored\n\n";
        assert_eq!(failures(pretty), Some(vec!["b".to_owned(), "c::d".to_owned()]));
        let colored =
            "\nfailures:\n    b\n\ntest result: \x1b[31mFAILED\x1b[0m. 0 passed; 1 failed\n";
        assert_eq!(failures(colored), Some(vec!["b".to_owned()]));
        assert_eq!(
            failures("\nrunning 1 test\ntest a ... ok\n\ntest result: ok. 1 passed\n"),
            None
        );
        // A program that stopped in a test has no summary.
        assert_eq!(failures("\nrunning 2 tests\ntest a ... ok\n"), None);
    }
}
