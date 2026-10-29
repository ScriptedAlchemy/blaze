#!/usr/bin/env python3
"""Owned metadata-only producer probes compared with Python's rustc oracle."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import producer_plan
from accepted_probes import ACCEPTED


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--blaze", required=True, type=Path)
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    norustc = producer_plan.norustc
    real = Path(norustc.real_toolchain())
    audit_log = work / "product-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(audit_log)))
    env = dict(os.environ)
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_BOOTSTRAP",
                 "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "SDKROOT"):
        env.pop(name, None)
    product = dict(env, RUSTUP_TOOLCHAIN=str(audit),
        PATH=str(audit / "bin") + os.pathsep + env["PATH"],
        RA_JIT_ANSWERS="own", BLAZE_ANSWERS="own",
        RA_JIT_STD_FROM="off", RA_JIT_PROC_MACRO_SERVER="0",
        RA_JIT_HOME=str(work / "home"), RA_JIT_CACHE=str(work / "store"),
        BLAZE_PRODUCER_PROBE_FAILURE=str(work / "unsupported"))
    cases = [
        ("valid", "pub fn value() -> u32 { 1 }\n", [], False),
        ("unreachable-type-error", 'pub fn value() -> u32 { "wrong" }\n', [], False),
        ("unreachable-name-error", "pub fn value() -> u32 { absent }\n", [], False),
        ("syntax-error", "pub fn value( {\n", [], False),
        ("compile-error", 'compile_error!("this probe must fail");\n', [], False),
        ("unresolved-extern", "extern crate absent;\n", [], False),
        ("cfg-off", '#![cfg_attr(enabled, feature(proc_macro_span))]\npub fn value() {}\n', [], False),
        ("cfg-on", '#![cfg_attr(enabled, feature(proc_macro_span))]\npub fn value() {}\n',
         ["--cfg=enabled"], False),
        ("bootstrap", '#![feature(proc_macro_span)]\npub fn value() {}\n', [], True),
        ("stable-span", "extern crate proc_macro;\npub fn line(s: proc_macro::Span) -> usize { s.line() }\n",
         [], False),
        # Errors rust-analyzer finds in lowering, which the borrow checker never sees.
        ("destructuring-record-update", "pub struct S { pub a: u8 }\n"
         "pub fn f(s: S) -> u8 { let a; S { a, ..s } = S { a: 1 }; a }\n", [], False),
        ("missing-body", "pub fn f();\n", [], False),
    ]
    rows = []
    cached = Path(producer_plan.source_macro.source("proc-macro2")).parent / "probe"
    for name in ("proc_macro_span", "proc_macro_span_location", "proc_macro_span_file"):
        cases.append((name, (cached / (name + ".rs")).read_text(),
                      ["--cfg=procmacro2_build_probe"], False))
    # Programs rustc accepts: blaze may refuse one loudly, never answer it with a "no".
    cases.extend(("accepted-" + name, text, [], False) for name, text in ACCEPTED)
    for name, text, flags, bootstrap in cases:
        source = work / (name + ".rs")
        source.write_text(text)
        out = work / (name + "-out")
        out.mkdir(exist_ok=True)
        argv = ["--crate-name=probe", "--crate-type=lib", "--edition=2021",
                "--cap-lints=allow", "--emit=metadata", "--out-dir", str(out),
                str(source), *flags]
        ours_env = dict(product)
        oracle_env = dict(env, RUSTUP_TOOLCHAIN=str(real))
        if bootstrap:
            ours_env["RUSTC_BOOTSTRAP"] = oracle_env["RUSTC_BOOTSTRAP"] = "1"
        ours = subprocess.run([str(args.blaze.resolve()), "producer-rustc", *argv],
            env=ours_env, cwd=work, capture_output=True, text=True)
        oracle = subprocess.run([str(real / "bin/rustc"), *argv], env=oracle_env,
            cwd=work, capture_output=True, text=True)
        (work / (name + "-owned.log")).write_text(ours.stdout + ours.stderr)
        (work / (name + "-oracle.log")).write_text(oracle.stdout + oracle.stderr)
        rows.append(dict(case=name, ours=ours.returncode, oracle=oracle.returncode))
        (work / "rows.json").write_text(json.dumps(rows, indent=2) + "\n")
        if bootstrap:
            assert ours.returncode != 0
            assert (work / "unsupported").exists(), ours.stderr
            assert "owned release feature catalog" in ours.stderr, ours.stderr
            (work / "unsupported").unlink()
        elif (work / "unsupported").exists():
            assert ours.returncode != 0
            assert "owned compiler probe acceptance is not proven" in ours.stderr, ours.stderr
            rows[-1]["verdict"] = "undecided-refused"
            (work / "unsupported").unlink()
        else:
            assert (ours.returncode == 0) == (oracle.returncode == 0), rows[-1]
            assert not (work / "unsupported").exists(), ours.stderr
    calls = norustc.runs(str(audit_log))
    (work / "product-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
    assert not calls, calls
    refused = sum(row.get("verdict") == "undecided-refused" for row in rows)
    print(f"{len(rows)} source probes: oracle parity or loud undecided refusal ({refused} refused); "
          "zero product compilers")


if __name__ == "__main__":
    main()
