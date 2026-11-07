#!/usr/bin/env python3
"""forkfixes.py: programs from the fixes of the fork that was rebased onto v2, each with what rustc
1.99 does with it, built by the blaze under test.

The fork's fixes were made to V1's code; each of these states the behaviour one of them was for,
so that what v2 does is a result rather than a reading of its code. Input, one JSON value:

    blaze    the blaze binary under test
    scratch  a directory for the projects and the daemon's home; it is removed first
    only     (optional) the names of the cases to run
    stall_bound  (optional) seconds the daemon's loop may make no progress (`blaze serve --stall-bound`)

Each case is `harness/forkfixes/cases.py`'s: its sources, and either the lines its program prints
or the text its error has. Every case runs, and the report names each that did not do as stated.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "forkfixes"))
from cases import CASES  # noqa: E402


def write(root: Path, files: dict[str, str]) -> None:
    for name, text in files.items():
        at = root / name
        at.parent.mkdir(parents=True, exist_ok=True)
        at.write_text(text)


def env_for(scratch: Path) -> dict:
    env = {k: v for k, v in os.environ.items() if not k.startswith(("CARGO", "RUST", "BLAZE_"))}
    env.update(BLAZE_HOME=str(scratch / "home"), TMPDIR=str(scratch / "tmp"), RUST_BACKTRACE="0")
    (scratch / "tmp").mkdir(parents=True, exist_ok=True)
    return env


def run_case(blaze: str, scratch: Path, case: dict) -> tuple[bool, str]:
    root = scratch / "cases" / case["name"]
    files = dict(case["files"])
    files.setdefault(
        "Cargo.toml",
        f'[package]\nname = "{case["name"].replace("-", "_")}"\nversion = "0.1.0"\nedition = "2024"\n\n[workspace]\n',
    )
    write(root, files)
    env = env_for(scratch)
    if case.get("test"):
        tested = subprocess.run([blaze, "test"], cwd=root, env=env, capture_output=True, text=True, timeout=1800)
        said = tested.stdout + tested.stderr
        return tested.returncode == 0 and case["test"] in said, said[-900:]
    target = scratch / "targets" / case["name"]
    env["CARGO_TARGET_DIR"] = str(target)
    built = subprocess.run([blaze, "build"], cwd=root, env=env, capture_output=True, text=True, timeout=1800)
    said = built.stdout + built.stderr
    if "error" in case:
        if built.returncode == 0:
            return False, f"it built, and rustc refuses it with `{case['error']}`"
        return (case["error"] in said), (said[-600:] if case["error"] not in said else "refused as stated")
    if built.returncode != 0:
        return False, f"it did not build, and rustc builds it:\n{said[-900:]}"
    program = target / "debug" / case.get("program", case["name"].replace("-", "_"))
    ran = subprocess.run([str(program)], capture_output=True, text=True, timeout=120)
    if ran.returncode != 0:
        return False, f"its program failed: {ran.returncode}\n{ran.stderr[-400:]}"
    got = ran.stdout.splitlines()
    return got == case["prints"], f"printed {got}, rustc's prints {case['prints']}"


def main(given: dict) -> int:
    scratch = Path(given["scratch"])
    shutil.rmtree(scratch, ignore_errors=True)
    scratch.mkdir(parents=True)
    only = set(given.get("only", ()))
    results = {}
    if "stall_bound" in given:
        log = open(scratch / "daemon.log", "w")
        subprocess.Popen(
            [given["blaze"], "serve", "--stall-bound", str(given["stall_bound"])],
            cwd=scratch,
            env=env_for(scratch),
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        # A build before it listens would start another daemon, of the default bound.
        deadline = time.time() + 600
        while time.time() < deadline:
            status = subprocess.run([given["blaze"], "status", "--json"], cwd=scratch, env=env_for(scratch), capture_output=True, text=True)
            if status.returncode == 0 and '"pid"' in status.stdout:
                break
            time.sleep(1)
    try:
        for case in CASES:
            if only and case["name"] not in only:
                continue
            try:
                ok, why = run_case(given["blaze"], scratch, case)
            except subprocess.TimeoutExpired as timed_out:
                ok, why = False, f"timed out: {timed_out.cmd}"
            results[case["name"]] = {"ok": ok, "why": why, "fixes": case["fixes"]}
            print(("ok   " if ok else "FAIL ") + f"{case['name']} ({', '.join(case['fixes'])}): {why if not ok else ''}", flush=True)
    finally:
        subprocess.run([given["blaze"], "stop"], cwd=scratch, env=env_for(scratch), capture_output=True)
    (scratch / "results.json").write_text(json.dumps(results, indent=1))
    failed = [name for name, it in results.items() if not it["ok"]]
    print(f"forkfixes: {len(results) - len(failed)} of {len(results)} as stated", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    sys.exit(main(json.loads(sys.argv[1])))
