#!/usr/bin/env python3
"""PR15 F2 repros from reviews/premerge-pr15-0d5357ff.md, issuecomment-6469622516.

The real wrapper must stay ready through no-change producer reloads and a restart,
and through plain Cargo after enable. Run under the heavy lock and watchdog.
Reference Cargo runs belong to this harness only. Every daemon is stopped.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
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
    parser.add_argument("--scenario", choices=["B", "D", "all"], default="all")
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
        if name == "D":
            env = cargo_ra_jit.enabled_home(str(work / "enabled"), env)
        side = Side(ws, env, "plain" if name == "D" else True)
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
        for name in ["B", "D"] if args.scenario == "all" else [args.scenario]:
            run_scenario(name)
        print("F2: no-change reload/restart and plain-cargo warmth pass", flush=True)
    finally:
        (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        shutil.rmtree(crossmachine.DAEMON_ROOT)


if __name__ == "__main__":
    main()
