"""Apply reviewed CI changes to an exact, separately checked-out PR tree."""
from pathlib import Path
import hashlib
import json
import sys

root = Path(sys.argv[1])

def edit(name, replacements):
    path = root / name
    text = path.read_text()
    for before, after in replacements:
        assert text.count(before) == 1, (name, before[:100])
        text = text.replace(before, after)
    path.write_text(text)

edit('.github/scripts/bundle.py', [
    ('    if not want or not os.path.isfile(path) or sha256_file(path) != want["sha256"]:',
     '    if (not want or not want.get("executable") or not os.path.isfile(path)\n            or not os.access(path, os.X_OK) or os.path.getsize(path) != want["size"]\n            or sha256_file(path) != want["sha256"]):'),
    ('    row = {"exe": os.path.realpath(argv[0]), "args": argv[1:], "cwd": os.getcwd(), "env": env}',
     '    removed = sorted(k for k in base if k not in os.environ and not k.startswith("BUNDLE_RECORD"))\n    row = {"exe": os.path.realpath(argv[0]), "args": argv[1:], "cwd": os.getcwd(),\n           "env": env, "env_remove": removed}'),
    ('''    with tarfile.open(fileobj=zstd.stdin, mode="w|") as tar:
        for key in keys:
            tar.add(from_key(key, base), arcname=key, recursive=False)
    zstd.stdin.close()
    if zstd.wait() != 0:''', '''    completed = False
    try:
        # Cargo may hardlink two artifact names. The wire contract admits regular
        # members only, so pack their bytes, not tar's implicit hardlink entries.
        with tarfile.open(fileobj=zstd.stdin, mode="w|", dereference=True) as tar:
            for key in keys:
                tar.add(artifact_path(key, base), arcname=key, recursive=False)
        zstd.stdin.close()
        completed = True
    finally:
        if not completed and zstd.poll() is None:
            zstd.terminate()
        try:
            zstd.stdin.close()
        except BrokenPipeError:
            pass
        finally:
            status = zstd.wait()
    if status != 0:'''),
    ('''                seen.add(member.name)
        completed = True''', '''                seen.add(member.name)
        # tar stops at its end marker, before zstd necessarily finishes writing
        # valid padding or validating its frame checksum. Drain to EOF before
        # closing the pipe, rather than turning a valid archive into SIGPIPE.
        while zstd.stdout.read(1 << 20):
            pass
        completed = True'''),
    ('        code = subprocess.run(command, cwd=run["cwd"], env={**os.environ, **run["env"]}).returncode',
     '        env = dict(os.environ)\n        for name in run.get("env_remove", []):\n            env.pop(name, None)\n        env.update(run["env"])\n        code = subprocess.run(command, cwd=run["cwd"], env=env).returncode'),
])
edit('.github/scripts/changes.py', [
    ('from fnmatch import fnmatchcase', 'import argparse\nfrom fnmatch import fnmatchcase'),
    ('''def main():
    areas''', '''def comparison_paths(document):
    """GitHub caps compare.files at 300; unknown/truncated input selects everything."""
    files = document.get("files") if isinstance(document, dict) else None
    if not isinstance(files, list) or len(files) >= 300:
        return []
    paths = set()
    for entry in files:
        if not isinstance(entry, dict) or not isinstance(entry.get("filename"), str) or not entry["filename"]:
            return []
        paths.add(entry["filename"])
        if "previous_filename" in entry:
            previous = entry["previous_filename"]
            if not isinstance(previous, str) or not previous:
                return []
            paths.add(previous)
    return sorted(paths)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compare-json", action="store_true")
    args = parser.parse_args()
    areas'''),
    ('    files = [line.strip() for line in sys.stdin if line.strip()]', '''    if args.compare_json:
        text = sys.stdin.read()
        try:
            files = comparison_paths(json.loads(text)) if text.strip() else []
        except json.JSONDecodeError:
            print("::warning::unreadable changed-file response; selecting every area", file=sys.stderr)
            files = []
    else:
        files = [line.strip() for line in sys.stdin if line.strip()]'''),
])
area = root / '.github/ci/areas.json'
data = json.loads(area.read_text())
for name in ('dist', 'serve'):
    data[name]['paths'] = ['crates/**', 'vendor/**'] + [p for p in data[name]['paths'] if not p.startswith(('crates/', 'vendor/'))]
    data[name]['ignore'] = ['crates/ra-jit-bench/**', 'crates/ra-jit-fuzz/**', 'crates/ra-jit-wasm/**']
data['ci']['paths'] += ['.github/scripts/test_changes.py', '.github/scripts/test_bundle*.py']
data['_doc'] += ' Runtime consumers include all compiler crates and vendored dependencies, excluding benchmark, fuzz, and browser-only tooling.'
area.write_text(json.dumps(data, indent=2) + '\n')
edit('.github/workflows/pr.yaml', [
    ('      - id: areas\n', '''      - name: Check CI helper contracts before compiling
        run: |
          python3 -I .github/scripts/test_changes.py
          python3 -I .github/scripts/test_bundle_integrity.py
          python3 -I .github/scripts/test_bundle_transport.py
          python3 -I .github/scripts/test_bench_pair.py
          python3 -I .github/scripts/test_bench_verdict.py
      - id: areas
'''),
    ('''files=$(gh api "repos/$GITHUB_REPOSITORY/compare/$BASE...$HEAD" --jq 'if (.files | length) >= 300 then "" else .files[].filename end') || files=;;''',
     '''files=$(gh api "repos/$GITHUB_REPOSITORY/compare/$BASE...$HEAD") || files=;;'''),
    ('echo "$files" | python3 -I .github/scripts/changes.py | tee',
     'echo "$files" | python3 -I .github/scripts/changes.py --compare-json | tee'),
])
edit('.github/scripts/bench_verdict.py', [
    ('        if block:\n            name = block.group(1)\n            continue',
     '        if block:\n            # Duplicate results or a header without its mean indicate incomplete\n            # evidence; never let a later block overwrite a previous measurement.\n            if name is not None or block.group(1) in found:\n                return {}\n            name = block.group(1)\n            continue'),
    ('    return found\n\n\ndef slow', '    return found if name is None else {}\n\n\ndef slow'),
    ('def judge(aa, attempts, cpu="", aa_seconds=None):', 'def judge(aa, attempts, cpu="", aa_seconds=None, exit_codes=None, aa_exit_code=None):'),
    ('    aa_slow = slow(aa)\n    parsed = [bool(means(text)) for text in attempts]',
     '    control = means(aa)\n    measured = [means(text) for text in attempts]\n    expected = set(control) or next((set(rows) for rows in measured if rows), set())\n    aa_slow = slow(aa)\n    parsed = [bool(rows) and set(rows) == expected for rows in measured]'),
    ('    if not parsed[-1]:\n        verdict, status = "error", 1',
     '''    process_failed = exit_codes is not None and (
        len(exit_codes) != len(attempts) or not exit_codes
        or exit_codes[-1] not in (0, 1) or (exit_codes[-1] == 1 and not slows[-1]))
    if process_failed:
        lines.append(f"comparison process exit statuses: {exit_codes}")
    if process_failed or not parsed or not parsed[-1]:
        verdict, status = "error", 1
        lines.append("last comparison has missing, duplicate, or incomplete measurements")'''),
    ('    if aa_slow and verdict != "error":', '    if aa_slow and aa_exit_code in (None, 0, 1) and verdict != "error":'),
    ('    parser.add_argument("--cpu-file", help="the job\'s cpu.txt")',
     '    parser.add_argument("--cpu-file", help="the job\'s cpu.txt")\n    parser.add_argument("--exit-code", type=int, action="append", help="one exit status per comparison")\n    parser.add_argument("--aa-exit-code", type=int, help="the A/A process exit status")'),
    ('Path(args.aa).read_text(), [Path(it).read_text() for it in args.attempts], cpu, args.aa_seconds',
     'Path(args.aa).read_text(), [Path(it).read_text() for it in args.attempts], cpu, args.aa_seconds,\n        args.exit_code, args.aa_exit_code'),
])
edit('.github/workflows/bench.yaml', [
    ('          started=$SECONDS\n', '          started=$SECONDS\n          aa_status=0\n'),
    ('--dump "$tango/aa" | tee "$tango/aa.txt" || true', '--dump "$tango/aa" | tee "$tango/aa.txt" || aa_status=$?'),
    ('          attempts=()\n', '          attempts=()\n          exit_codes=()\n'),
    ('            attempts+=("$tango/compare-$attempt.txt")', '            attempts+=("$tango/compare-$attempt.txt")\n            exit_codes+=(--exit-code "$status")'),
    ('--cpu-file "$tango/cpu.txt" "${attempts[@]}"', '--cpu-file "$tango/cpu.txt" --aa-exit-code "$aa_status" "${exit_codes[@]}" "${attempts[@]}"'),
    ('*error) echo "::error::tango measured nothing in its last comparison"', '*error) echo "::error::tango comparison failed or produced incomplete measurements"'),
])
for name in ('test_bundle_transport.py', 'test_changes.py', 'test_bench_verdict.py'):
    (root / '.github/scripts' / name).write_bytes(Path(__file__).with_name(name).read_bytes())

# Confirm the remote preparation produced exactly the Python trees tested locally.
expected = json.loads(Path(__file__).with_name('deep-ci-hashes.json').read_text())
for name, digest in expected.items():
    assert hashlib.sha256((root / name).read_bytes()).hexdigest() == digest, name
