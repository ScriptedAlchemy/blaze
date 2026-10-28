#!/usr/bin/env python3
"""d0export.py: rustc's build of the exported fixture's source, as the oracle of the D0 round trip.

usage: d0export.py [--work DIR] [--evidence DIR]

`crates/hir-ty-codegen`'s
`independently_loaded_processes_produce_identical_entry_digests_and_import` exports a batch
from two separately loaded databases, checks the two receipts' entries are the same bytes,
imports them into a fresh generation, assembles a program from the imported entries alone and
links it. It records that program's standard output, standard error and exit status under
`BLAZE_D0_EVIDENCE`, beside the fixture's source (`oracle.rs`) and the C driver and object it
linked (`main.c`, `imported.o`).

This harness is the reference half. It compiles the same source with rustc 1.99.0, runs the
result, and compares standard output, standard error and exit status with the imported
program's. The reference build is the harness's alone because `crates/` starts no rustc
(docs/no-rustc.md). The crate's test does the product work; this only compares.

    cargo +1.99.0 test --release -p hir-ty-codegen export:: -- --test-threads=1
"""
import argparse, os, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
# What the test records that the comparison reads, and what an evidence directory keeps.
RECORDED = ["oracle.rs", "imported.o", "main.c", "program.stdout", "program.stderr", "program.status"]


def run(cmd, **kw):
    print(" ".join(map(str, cmd)), flush=True)
    return subprocess.run(cmd, **kw)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--work", help="the directory the crate's test writes (default: a fresh one)")
    ap.add_argument("--evidence", help="copy what the run kept to this directory")
    args = ap.parse_args()
    work = os.path.abspath(args.work or tempfile.mkdtemp(prefix="ra-jit-d0export-"))
    os.makedirs(work, exist_ok=True)
    env = {**os.environ, "BLAZE_D0_EVIDENCE": work, "RUST_BACKTRACE": "0"}
    test = run(["cargo", f"+{TOOLCHAIN}", "test", "--release", "-p", "hir-ty-codegen", "export::",
                "--", "--test-threads=1"], cwd=ROOT, env=env)
    if test.returncode != 0:
        print(f"d0export: the export tests failed ({test.returncode}); nothing to compare", file=sys.stderr)
        return 1
    missing = [name for name in RECORDED if not os.path.isfile(os.path.join(work, name))]
    if missing:
        print(f"d0export: the test recorded no {', '.join(missing)} in {work}", file=sys.stderr)
        return 1
    oracle = run(["rustc", f"+{TOOLCHAIN}", "--edition=2024", os.path.join(work, "oracle.rs"),
                  "-o", os.path.join(work, "oracle")])
    if oracle.returncode != 0:
        print(f"d0export: rustc could not build the oracle ({oracle.returncode})", file=sys.stderr)
        return 1
    reference = run([os.path.join(work, "oracle")], capture_output=True)
    program = (open(os.path.join(work, "program.stdout"), "rb").read(),
               open(os.path.join(work, "program.stderr"), "rb").read(),
               int(open(os.path.join(work, "program.status")).read().strip()))
    print(f"imported program: exit {program[2]}, stdout {program[0]!r}, stderr {program[1]!r}")
    print(f"rustc oracle:     exit {reference.returncode}, stdout {reference.stdout!r}, "
          f"stderr {reference.stderr!r}")
    if args.evidence:
        os.makedirs(args.evidence, exist_ok=True)
        for name in RECORDED + ["oracle"]:
            path = os.path.join(work, name)
            if os.path.isfile(path):
                shutil.copy2(path, os.path.join(args.evidence, name))
        open(os.path.join(args.evidence, "oracle.stdout"), "wb").write(reference.stdout)
        open(os.path.join(args.evidence, "oracle.stderr"), "wb").write(reference.stderr)
        open(os.path.join(args.evidence, "oracle.status"), "w").write(str(reference.returncode))
    agree = (program[2] == reference.returncode
             and program[0] == reference.stdout and program[1] == reference.stderr)
    if not agree:
        print("d0export: the imported program and the rustc oracle disagree", file=sys.stderr)
        return 1
    print("d0export: imported program and rustc oracle agree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
