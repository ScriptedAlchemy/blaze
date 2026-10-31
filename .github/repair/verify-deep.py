"""Verify reviewed changes and stage candidates without moving either user PR."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib

REPO = Path(os.environ['GITHUB_WORKSPACE'])
TEMP = Path(os.environ['RUNNER_TEMP'])
CI_BASE = '89d814aa7e7102f8285697091a89de0b5d053f7c'
C2_BASE = '937c36922a87ae1360ea10ca1e54b7f7aee92a1f'
COMMON = '2d3292ec71b37e4dd9d50c3b4c7ad4bc8ea5465a'
RUN = os.environ['GITHUB_RUN_ID']


def call(*args, cwd=REPO):
    print('$', ' '.join(map(str, args)), flush=True)
    subprocess.run(list(map(str, args)), cwd=cwd, check=True)


def output(*args, cwd=REPO):
    return subprocess.check_output(list(map(str, args)), cwd=cwd, text=True).strip()


def git(*args, cwd=REPO):
    call('git', *args, cwd=cwd)


def verify(command, logfile, cwd=REPO, timeout=180):
    result = subprocess.run(list(map(str, command)), cwd=cwd, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, timeout=timeout)
    with logfile.open('ab') as log:
        log.write(('$ ' + ' '.join(map(str, command)) + '\n').encode())
        log.write(result.stdout)
    print(result.stdout.decode(errors='replace'), flush=True)
    return result


call('zstd', '--version')
git('config', 'user.name', 'Blaze repair')
git('config', 'user.email', 'noreply@openai.com')
checked = TEMP / 'child-reviewed'
git('worktree', 'add', '--detach', checked, COMMON)
git('apply', REPO / '.github/repair/deep-child.patch', cwd=checked)
files = ['crates/ra-jit/src/child.rs', 'crates/ra-jit/src/cargo/spec.rs']
call('rustfmt', '--edition', '2024', '--config', 'skip_children=true', *files, cwd=checked)
source = checked / files[0]
fixed = source.read_bytes()
tests = fixed[fixed.index(b'#[cfg(all(test, unix))]'):]
original = subprocess.check_output(['git', 'show', 'HEAD:' + files[0]], cwd=checked)
probe = TEMP / 'child-probe'
(probe / 'src').mkdir(parents=True)
version = next(p['version'] for p in tomllib.loads((checked / 'Cargo.lock').read_text())['package'] if p['name'] == 'libc')
(probe / 'Cargo.toml').write_text('[package]\nname="blaze-child-probe"\nversion="0.0.0"\nedition="2024"\n[workspace]\n[dependencies]\nlibc="=' + version + '"\n')
(probe / 'src/lib.rs').write_text('#[path = "' + str(source) + '"]\nmod child;\n')
command = ['cargo', 'test', '--manifest-path', probe / 'Cargo.toml']
try:
    source.write_bytes(original + b'\n' + tests)
    before = verify(command + ['child::tests::drains_output_while_sending_large_input', '--', '--exact', '--nocapture'], TEMP / 'child-before.log')
    assert before.returncode != 0 and b'child input/output deadlocked' in before.stdout, 'expected deadlock was not reproduced'
finally:
    source.write_bytes(fixed)
after = verify(command + ['--', '--test-threads=1'], TEMP / 'child-after.log')
assert after.returncode == 0, 'patched child helper regressions failed'
call('rustfmt', '--edition', '2024', '--check', '--config', 'skip_children=true', *files, cwd=checked)
git('diff', '--check', cwd=checked)
git('add', *files, cwd=checked)
(TEMP / 'child-verified.patch').write_bytes(subprocess.check_output(['git', 'diff', '--cached'], cwd=checked))
git('commit', '-m', 'Drain compiler output while streaming private project input', '-m', 'Send stdin on a scoped writer while wait_with_output drains stdout and stderr. Reuse the SIGPIPE-safe helper for saved replay. A real subprocess regression deadlocks before the fix and passes afterward; verbose early rejection, successful early exit and default SIGPIPE are also covered.', cwd=checked)
shared = output('git', 'rev-parse', 'HEAD', cwd=checked)
ci, c2 = TEMP / 'deep-ci', TEMP / 'deep-c2'
git('worktree', 'add', '--detach', ci, CI_BASE)
git('merge', '--no-ff', shared, '-m', 'Merge verified compiler pipe repair into shared-build CI', cwd=ci)
call(sys.executable, '-I', REPO / '.github/repair/apply_deep_ci.py', ci)
git('worktree', 'add', '--detach', c2, C2_BASE)
git('merge', '--no-ff', shared, '-m', 'Merge verified compiler pipe repair into saved replay', cwd=c2)
marker = '      - name: Test compiler export and import identities\n'
commands = {
    ci: 'nix develop .#bench --command python3 -I .github/scripts/bundle.py test serve-cli -- child:: --test-threads=2',
    c2: 'cargo test --locked -p ra-jit --bin ra-jit -- child:: --test-threads=2',
}
for tree, cmd in commands.items():
    file = tree / '.github/workflows/blaze-serve-review.yaml'
    text = file.read_text()
    assert text.count(marker) == 1, tree
    step = '      - name: Test compiler subprocess input and output\n        if: ${{ !cancelled() }}\n        run: ' + cmd + '\n'
    file.write_text(text.replace(marker, step + marker))
helper_tests = ['test_changes.py', 'test_bundle_integrity.py', 'test_bundle_transport.py', 'test_bench_pair.py', 'test_bench_verdict.py']
for test in helper_tests:
    result = verify([sys.executable, '-I', '.github/scripts/' + test], TEMP / 'ci-deep-checks.log', cwd=ci)
    assert result.returncode == 0, test
for name, tree in [('ci', ci), ('c2', c2)]:
    for test in ['test_buildspec_cleanup.py', 'test_check_nix_workflow.py', 'test_saved_spec_nix_rpath.py']:
        result = verify([sys.executable, '-I', 'harness/dist/' + test], TEMP / (name + '-deep-checks.log'), cwd=tree)
        assert result.returncode == 0, (name, test)
    git('diff', '--check', cwd=tree)
git('add', '.github/ci/areas.json', '.github/scripts', '.github/workflows/bench.yaml', '.github/workflows/pr.yaml', '.github/workflows/blaze-serve-review.yaml', cwd=ci)
git('commit', '-m', 'Fix CI coverage, benchmark verdicts and bundle transport fidelity', '-m', 'Include compiler dependencies and rename origins in runtime selection. Reject incomplete or duplicate benchmark evidence and abnormal process exits. Preserve removed Cargo environment variables, dereference packed hardlinks, drain valid tar padding and verify executable mode. Exercise helper regressions before expensive builds and wire compiler pipe tests into consumers.', cwd=ci)
git('add', '.github/workflows/blaze-serve-review.yaml', cwd=c2)
git('commit', '-m', 'Run compiler pipe regressions in the saved replay gate', cwd=c2)
ci_sha = output('git', 'rev-parse', 'HEAD', cwd=ci)
c2_sha = output('git', 'rev-parse', 'HEAD', cwd=c2)
git('push', 'origin', ci_sha + ':refs/heads/repair/deep-ci-' + RUN, c2_sha + ':refs/heads/repair/deep-c2-' + RUN)
(TEMP / 'deep-receipt.json').write_text(json.dumps({'shared': shared, 'ci': ci_sha, 'c2': c2_sha, 'ci_base': CI_BASE, 'c2_base': C2_BASE}) + '\n')
git('update-ref', 'refs/heads/snapshot-ci', ci_sha)
git('update-ref', 'refs/heads/snapshot-c2', c2_sha)
git('bundle', 'create', TEMP / 'blaze-deep-review.bundle', 'refs/heads/snapshot-ci', 'refs/heads/snapshot-c2')
