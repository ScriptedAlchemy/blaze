#!/usr/bin/env python3
"""Build the matching proc-macro server executable through blaze, for source_macro.py.

The server's executable is compiled and linked by blaze, including source proc_macro.
Its own bootstrap macro expansions still use the existing compile-time-deps handoff.
Record every bootstrap compiler/Cargo call, without hiding compilation lines. This is
not a strict no-rustc bootstrap gate. Never use the toolchain server as the owned server.
Run under the heavy lock and memory watchdog.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
import norustc

SERVER = HARNESS.parent / "vendor" / "proc-macro-srv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--blaze", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--default-sdk", action="store_true")
    args = parser.parse_args()
    for name, value in {"RA_JIT_MEMORY_MAX": "16G", "RA_JIT_OWN_BASE_MEMORY": "12G",
                        "RA_JIT_SPARE": "0"}.items():
        assert os.environ.get(name) == value, f"run under memory watchdog with {name}={value}"
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    log = work / "bootstrap-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(log)))
    env = {**os.environ, "PATH": str(audit / "bin") + os.pathsep + os.environ["PATH"],
           "RUSTUP_TOOLCHAIN": str(audit), "RUSTC": str(audit / "bin" / "rustc"),
           "CARGO": str(audit / "bin" / "cargo"), "RA_JIT_STD_FROM": "off",
           "RA_JIT_ANSWERS": "own", "BLAZE_ANSWERS": "own",
           "RA_JIT_OWN_RESOLVE": "1", "BLAZE_OWN_RESOLVE": "1",
           "RA_JIT_HOME": str(work / "home"), "RA_JIT_CACHE": str(work / "store"),
           "RA_JIT_PROC_MACRO_SERVER": "0", "RA_JIT_WHOLE_BUILD": "1"}
    if args.default_sdk:
        env.pop("SDKROOT", None)
    executable = work / "blaze-proc-macro-srv"
    command = [str(args.blaze.resolve()), str(SERVER), "--crate",
               "rust_analyzer_proc_macro_srv", "--crate", "proc_macro",
               "--out", str(work / "server.o"), "--exe", str(executable)]
    result = subprocess.run(command, env=env, capture_output=True, text=True)
    (work / "build.log").write_text("$ " + " ".join(command) + "\n" +
                                    result.stdout + result.stderr)
    calls = norustc.runs(str(log))
    (work / "bootstrap-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
    assert result.returncode == 0, f"server build exit {result.returncode}; see {work / 'build.log'}"
    assert executable.is_file(), "blaze did not link its server"
    assert "0 functions in it ra-jit could not compile" in result.stderr
    pins = {name: dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest())
            for name, path in [("blaze", args.blaze.resolve()), ("server", executable)]}
    (work / "image-pins.json").write_text(json.dumps(pins, indent=2) + "\n")
    check = subprocess.run([str(executable), "--version"], capture_output=True, text=True,
                           env={**env, "RUST_ANALYZER_INTERNALS_DO_NOT_USE": "1"})
    assert check.returncode == 0, check.stderr
    print(f"{executable}: {check.stdout.strip()}; {len(calls)} counted bootstrap calls "
          "(retained, NOT a strict gate pass)")


if __name__ == "__main__":
    main()
