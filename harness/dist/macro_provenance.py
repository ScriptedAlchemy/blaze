#!/usr/bin/env python3
"""PR15 F2/F3 repros from the P3 and P4 scoped reviews.

The real wrapper must stay ready through no-change producer reloads and a restart,
and through plain Cargo after enable. Run under the heavy lock and watchdog.
Reference Cargo runs belong to this harness only. Every daemon is stopped.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import cargo_ra_jit
import crossmachine
from cargo_ra_jit import Side, program_output, wait_warm


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--blaze-bin", type=Path, required=True)
    parser.add_argument("--scenario", choices=["B", "D", "E", "F", "H", "all"], default="all")
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    binary = str(args.blaze_bin.resolve())
    cargo_ra_jit.RA_JIT = str(Path(binary) / "ra-jit")
    cargo_ra_jit.WARMUP_SECS = 90
    crossmachine.DAEMON_ROOT = tempfile.mkdtemp(prefix="c4f2-", dir="/tmp")
    seed = work / "seed"
    (seed / "registry").mkdir(parents=True, exist_ok=True)
    report = {}

    def run_scenario(name):
        machine = crossmachine.Machine(str(work / name), "a", ["alice", "src"],
                                       str(seed), os.path.expanduser("~/.rustup"))
        ws = os.path.join(machine.checkout("macro-" + name, crossmachine.FIXTURE_DIR), "macros")
        env = machine.env(binary, os.path.join(machine.top, "store"),
                          crossmachine.daemon_home("f2" + name, "a"),
                          crossmachine.FIXTURES["macros"].get("env"))
        if name in ("D", "E"):
            env = cargo_ra_jit.enabled_home(str(work / ("enabled-" + name)), env)
        side = Side(ws, env, "plain" if name in ("D", "E") else True)
        rows = []

        def step(label):
            _, built = side.cargo("build", "-p", "app")
            _, states = wait_warm(side, ws)
            _, ran = side.cargo("run", "-p", "app")
            row = dict(label=label, build_rc=built.returncode, states=states,
                       output=program_output(ran), worker=crossmachine.workers_log(side),
                       records=sorted(str(p.relative_to(ws)) for p in Path(ws).glob(
                           "target/rust-analyzer/producer-records/*/artifact-*")))
            rows.append(row)
            print(label, states, flush=True)
            assert built.returncode == 0 and all(s == "ready" for s in states) and states, row
            return row

        try:
            if name in ("E", "F"):
                # The P4 review's real-wrapper repro: Cargo runs outside the workspace,
                # but invokes rustc in the workspace root.
                parent = str(Path(ws).parent)
                manifest = str(Path(ws).name + "/Cargo.toml")
                argv = side.argv("build", "--manifest-path", manifest, "-p", "app")
                built = subprocess.run(argv, cwd=parent, env=env, capture_output=True, text=True)
                row = dict(label=name + "1 relative manifest", build_rc=built.returncode,
                           stdout=built.stdout, stderr=built.stderr)
                rows.append(row)
                print(row["label"], built.returncode, flush=True)
                assert built.returncode == 0, row
                _, states = wait_warm(side, ws)
                row["states"] = states
                assert states and all(s == "ready" for s in states), row
                return
            if name == "H":
                # Cargo has already resolved the workspace before it invokes its wrapper.
                # Corrupt just the metadata input during derive's compile to inject a
                # capture-reader error, not a compiler error. Restore it even on failure.
                shim = work / "capture-error-wrapper"
                shim.write_text("#!/bin/bash\nset -eu\n"
                    'case " $* " in\n*\" --crate-name derive \"*)\n'
                    '  cp Cargo.toml Cargo.toml.capture-save\n'
                    '  trap \'mv Cargo.toml.capture-save Cargo.toml\' EXIT\n'
                    '  printf \'invalid = [\\n\' > Cargo.toml\n'
                    '  ;;\nesac\n' +
                    shlex.quote(str(Path(binary) / "ra-jit")) + ' rustc-wrapper "$@"\n')
                shim.chmod(0o755)
                error_env = {**env, "RUSTC_WORKSPACE_WRAPPER": str(shim),
                             "RA_JIT_CARGO_DRIVES": "1"}
                argv = ["cargo", "build", "--manifest-path", str(Path(ws) / "Cargo.toml"), "-p", "derive"]
                built = subprocess.run(argv, cwd=str(Path(ws).parent), env=error_env,
                                       capture_output=True, text=True)
                row = dict(label="H1 capture error", build_rc=built.returncode,
                           stdout=built.stdout, stderr=built.stderr)
                rows.append(row)
                print(row["label"], built.returncode, flush=True)
                assert built.returncode == 0, row
                assert "macro provenance: warm load unavailable:" in built.stderr, row
                assert not list(Path(ws).glob("target/rust-analyzer/producer-records/*/artifact-*")), row
                return
            first = step(name + "1")
            if name == "B":
                for n in range(2):
                    with open(os.path.join(ws, "app/Cargo.toml"), "a") as manifest:
                        manifest.write(f"\n# no-change producer reload {n}\n")
                    time.sleep(3)
                    assert step(f"B2.{n}")["output"] == first["output"]
                side.cargo("stop")
                assert step("B3 restart")["output"] == first["output"]
            else:
                time.sleep(3)
                assert step("D2")["output"] == first["output"]
        finally:
            side.cargo("stop")
            report[name] = rows

    try:
        for name in ["B", "D", "E", "F", "H"] if args.scenario == "all" else [args.scenario]:
            run_scenario(name)
        print("F2/F3: warmth, relative manifest and nonfatal capture errors pass", flush=True)
    finally:
        (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        shutil.rmtree(crossmachine.DAEMON_ROOT)


if __name__ == "__main__":
    main()
