"""Temporary source-only repair preparation; does not push or change permissions."""
from pathlib import Path
import hashlib
import json
import os
import subprocess

ROOT = Path.cwd()
OUT = Path(os.environ['RUNNER_TEMP']) / 'runtime-proof'
OUT.mkdir(exist_ok=True)
paths = {
    'crates/ra-jit/src/cargo/run.rs': '4bd359434e79fd4900226f969257f4a30cdbf9c8',
    'crates/ra-jit/src/cargo/affected.rs': '0da3ee3aacc0c81882da959c4b9e873d0132535f',
    'crates/ra-jit/src/remote.rs': '43d54426889e667c66a3370486cdeb0c1d8a565d',
    'crates/ra-jit/src/remote/keeper.rs': '36822e356b8edafe8f305474db68cfbbdf241d44',
}
original = {}
for path, want in paths.items():
    data = Path(path).read_bytes()
    digest = hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest()
    assert digest == want, (path, digest, want)
    original[path] = data.decode()
(OUT / 'baseline.json').write_text(json.dumps(original))


def change(path, before, after):
    p = Path(path)
    text = p.read_text()
    assert text.count(before) == 1, ('source drift', path, before[:90])
    p.write_text(text.replace(before, after))


def probe(text, name, expected_success):
    start = text.index('fn listing_failures(')
    end = text.index('/// Starts `command`', start)
    source = ('use std::process::{Command, ExitStatus};\n'
              'const FAILED: i32 = 101;\nconst PRINTED_KEPT: usize = 4 << 20;\n'
              + text[start:end]
              + '\n#[path = ' + json.dumps(str(ROOT / 'crates/ra-jit/src/cargo/run/output_tests.rs'))
              + '] mod output_tests;\n')
    rust = OUT / (name + '.rs')
    rust.write_text(source)
    exe = OUT / name
    subprocess.run(['rustc', '--edition=2024', '--test', str(rust), '-o', str(exe)], check=True)
    result = subprocess.run([str(exe), '--test-threads=2'], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    (OUT / (name + '.log')).write_text(result.stdout)
    print(result.stdout, flush=True)
    assert (result.returncode == 0) == expected_success, (name, result.returncode)
    if not expected_success:
        for test in ('killed_process_cannot_supply_retry_names', 'abnormal_exit_cannot_supply_retry_names',
                     'successful_process_is_not_failed_by_a_nested_summary',
                     'incomplete_or_duplicate_failure_lists_do_not_authorize_partial_passes'):
            assert ('test output_tests::' + test + ' ... FAILED') in result.stdout, test


probe(original['crates/ra-jit/src/cargo/run.rs'], 'before', False)
run = 'crates/ra-jit/src/cargo/run.rs'
change(run, '                Err(_) => break,', '''                Err(error) => {
                    // A partial transcript cannot certify the tests that did not report.
                    // Closing the reader alone could leave a child blocked on its stdout.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }''')
change(run, '    Ok((status, failures(&String::from_utf8_lossy(&printed))))', '''    // A signal, launcher error or successful process is not a libtest failure, even if
    // a nested process printed something that looks like a failure summary. Only the
    // normal libtest failure exit can authorize selective retries and partial passes.
    let failed = (status.code() == Some(FAILED))
        .then(|| failures(&String::from_utf8_lossy(&printed)))
        .flatten();
    Ok((status, failed))''')
p = Path(run)
s = p.read_text()
a = s.index('fn failures(printed: &str)')
b = s.index('/// `text` without the escapes', a)
s = s[:a] + r'''fn failures(printed: &str) -> Option<Vec<String>> {
    let plain = without_colors(printed);
    let end = plain.rfind("\ntest result: FAILED.")?;
    let summary = plain[end + 1..].trim_end();
    if summary.contains('\n') {
        return None;
    }
    let count = summary
        .strip_prefix("test result: FAILED. ")?
        .split(';')
        .nth(1)?
        .trim()
        .strip_suffix(" failed")?
        .parse::<usize>()
        .ok()?;
    let start = plain[..end].rfind("\nfailures:\n")? + "\nfailures:\n".len();
    let names = plain[start..end]
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.strip_prefix("    ").filter(|name| !name.is_empty()).map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    // A capped transcript can lose the beginning of a long failure list; neither that
    // nor a repeated name proves which of the other tests passed.
    let unique = names.iter().collect::<std::collections::BTreeSet<_>>();
    (count > 0 && names.len() == count && unique.len() == count).then_some(names)
}

''' + s[b:]
p.write_text(s)
change(run, '''pub(crate) fn test_with(
    asked: &Invocation,
    context: &TestRun<'_>,
    mut make_command:''', '''pub(crate) fn test_with(
    asked: &Invocation,
    context: &TestRun<'_>,
    make_command: impl FnMut() -> anyhow::Result<Command>,
    start: impl FnMut(&mut Command) -> std::io::Result<std::process::Child>,
) -> anyhow::Result<TestResult> {
    let result = test_with_inner(asked, context, make_command, start);
    if result.is_err() {
        if let Some(directory) = context.cache.directory {
            affected::forget_passes_at(directory)
                .context("invalidating incomplete test results")?;
        }
    }
    result
}

fn test_with_inner(
    asked: &Invocation,
    context: &TestRun<'_>,
    mut make_command:''')
with p.open('a') as f:
    f.write('''
#[cfg(test)]
#[path = "run/output_tests.rs"]
mod output_tests;

#[cfg(all(test, unix))]
#[path = "run/policy_tests.rs"]
mod policy_tests;
''')
probe(p.read_text(), 'after', True)

# Confirm alias behavior using the pinned libtest, rather than assuming it.
fixture = OUT / 'alias.rs'
fixture.write_text('#[test] fn passes() {}\n')
subprocess.run(['rustc', '--test', str(fixture), '-o', str(OUT / 'alias')], check=True)
alias = subprocess.run([str(OUT / 'alias'), '--nocapture', '--no-capture'], capture_output=True, text=True)
(OUT / 'libtest-alias.log').write_text(alias.stdout + alias.stderr)
if alias.returncode:
    path = 'crates/ra-jit/src/cargo/affected.rs'
    change(path, '            "-q" => "--quiet",', '            "-q" => "--quiet",\n            "--no-capture" => "--nocapture",')
    change(path, '            vec!["--nocapture", "--nocapture"],', '''            vec!["--nocapture", "--nocapture"],
            vec!["--nocapture", "--no-capture"],
            vec!["--no-capture", "--nocapture"],''')
else:
    print('Pinned libtest accepts the alias pair; no alias-policy change is made.', flush=True)

path = 'crates/ra-jit/src/remote.rs'
change(path, '''        let done = self.runtime.block_on(async {
            self.asked.see(self.manifest_now().await);
            self.pull_as_set().await
        });''', '''        let done = self.runtime.block_on(async {
            self.pull_and_remember(self.manifest_now().await).await
        });''')
change(path, '''    /// A pull the way the daemon is set to pull: of every pack, or of what says where the
    /// keys are.
    async fn pull_as_set''', '''    /// Remember only a completed pull. A failed transfer must remain retryable even
    /// when the remote's manifest has not changed since it was first observed.
    async fn pull_and_remember(&self, observed: Option<Digest>) -> anyhow::Result<String> {
        let done = self.pull_as_set().await;
        self.asked.see(if done.is_ok() { observed } else { None });
        done
    }

    /// A pull the way the daemon is set to pull: of every pack, or of what says where the
    /// keys are.
    async fn pull_as_set''')
change(path, '''        for fetched in fetched {
            match fetched {
                Ok((pack, listed, keys)) => lazy.add(pack, listed, &keys),
                Err(e) => eprintln!("remote: {e:#}"),
            }
        }
        Ok(lazy)''', '''        let mut failed = 0;
        for fetched in fetched {
            match fetched {
                Ok((pack, listed, keys)) => lazy.add(pack, listed, &keys),
                Err(e) => {
                    failed += 1;
                    eprintln!("remote: {e:#}");
                }
            }
        }
        // Do not replace a working lookup table with a partial one or certify the
        // manifest as fetched when an index was unavailable. The next look can retry
        // the same manifest; normal compilation remains the fallback meanwhile.
        anyhow::ensure!(failed == 0, "{failed} remote pack key lists could not be loaded");
        Ok(lazy)''')
path = 'crates/ra-jit/src/remote/keeper.rs'
change(path, 'if now.is_some() && self.asked.see(now) {', 'if now.is_some() && !self.asked.saw(now) {')
change(path, 'let done = self.pull_as_set().await;', 'let done = self.pull_and_remember(now).await;')
with Path(path).open('a') as f:
    f.write('\n#[cfg(test)]\n#[path = "keeper/recovery_tests.rs"]\nmod recovery_tests;\n')
new = ['crates/ra-jit/src/cargo/run/output_tests.rs', 'crates/ra-jit/src/cargo/run/policy_tests.rs',
       'crates/ra-jit/src/remote/keeper/recovery_tests.rs']
subprocess.run(['rustfmt', '--edition', '2024', '--config', 'skip_children=true', *paths, *new], check=True)
subprocess.run(['git', 'diff', '--check'], check=True)
(OUT / 'repaired-sources.json').write_text(json.dumps({p: Path(p).read_text() for p in paths}))
print('Source-only candidate prepared and fast subprocess proof passed.', flush=True)
