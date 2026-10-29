#!/usr/bin/env python3
"""Owned metadata-only producer probes compared with Python's rustc oracle.

Each case runs through blaze's `producer-rustc` and through the reference rustc,
with the same arguments. A decided answer must equal the oracle's. Cases marked
required are the compile probes the owned proc-macro server's bootstrap runs, with
the exact arguments their build scripts pass; they must be decided. Other cases may
be refused loudly as undecided (the producer build fails), never answered wrongly.
"""
import argparse
import glob
import json
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import producer_plan

# The registry probes of vendor/proc-macro-srv's lockfile: every build script there
# that compiles source. autocfg's LLVM IR probes are empty_probe.py's.
REGISTRY = {"proc-macro2": "1.0.106", "thiserror": "2.0.18"}


def registry_file(package, relative):
    paths = glob.glob(str(Path.home() / ".cargo/registry/src/*" /
                          f"{package}-{REGISTRY[package]}" / relative))
    assert paths, f"need the cached source of {package} {REGISTRY[package]}"
    return Path(paths[0]).read_text()


def common(name, edition):
    return [f"--edition={edition}", f"--crate-name={name}", "--crate-type=lib",
            "--cap-lints=allow", "--emit=dep-info,metadata"]


def cases(host):
    rows = []

    def case(name, text, argv=None, env=None, required=False, bootstrap_refusal=False):
        rows.append(dict(name=name, text=text,
                         argv=argv if argv is not None else common("probe", "2021"),
                         env=env or {}, required=required, bootstrap_refusal=bootstrap_refusal))

    # proc-macro2's build.rs: the unstable probe first without RUSTC_BOOTSTRAP, the
    # stable probes keeping whatever the caller set.
    for probe in ("proc_macro_span", "proc_macro_span_location", "proc_macro_span_file"):
        text = registry_file("proc-macro2", f"src/probe/{probe}.rs")
        argv = ["--cfg=procmacro2_build_probe", *common("proc_macro2", "2021"),
                "--target", host]
        case(f"proc-macro2-{probe}", text, argv, required=True)
        if probe != "proc_macro_span":
            case(f"proc-macro2-{probe}-bootstrap", text, argv, {"RUSTC_BOOTSTRAP": "1"},
                 required=True)
            for edition in ("2015", "2018", "2024"):
                case(f"proc-macro2-{probe}-{edition}", text,
                     ["--cfg=procmacro2_build_probe", *common("proc_macro2", edition)])
            # Without the cfg the crate is not no_std; std still has no `Span::file`
            # import conflicts, so the answer must not change.
            case(f"proc-macro2-{probe}-std", text, common("proc_macro2", "2021"))
    case("proc-macro2-proc_macro_span-bootstrap",
         registry_file("proc-macro2", "src/probe/proc_macro_span.rs"),
         ["--cfg=procmacro2_build_probe", *common("proc_macro2", "2021")],
         {"RUSTC_BOOTSTRAP": "1"}, bootstrap_refusal=True)
    thiserror = registry_file("thiserror", "build/probe.rs")
    case("thiserror-probe", thiserror, [*common("thiserror", "2018"), "--target", host],
         required=True)

    case("valid", "pub fn value() -> u32 { 1 }\n")
    case("unreachable-type-error", 'pub fn value() -> u32 { "wrong" }\n')
    case("unreachable-name-error", "pub fn value() -> u32 { absent }\n")
    case("syntax-error", "pub fn value( {\n")
    case("compile-error", 'compile_error!("this probe must fail");\n')
    case("unresolved-extern", "extern crate absent;\n")
    case("cfg-off", '#![cfg_attr(enabled, feature(proc_macro_span))]\npub fn value() {}\n')
    case("cfg-on", '#![cfg_attr(enabled, feature(proc_macro_span))]\npub fn value() {}\n',
         [*common("probe", "2021"), "--cfg=enabled"])
    case("bootstrap", '#![feature(proc_macro_span)]\npub fn value() {}\n',
         env={"RUSTC_BOOTSTRAP": "1"}, bootstrap_refusal=True)

    span = "extern crate proc_macro;\n"
    case("stable-span", span + "pub fn line(s: proc_macro::Span) -> usize { s.line() }\n",
         required=True)
    case("stable-span-chain", span + "use proc_macro::Span;\n"
         "pub fn f(s: &Span) -> usize { s.end().start().column() }\n", required=True)
    case("stable-string", "pub fn f(s: &String) -> usize { s.len() }\n"
         "pub fn g(s: &String) -> bool { s.is_empty() }\n", required=True)
    case("stable-alloc-no-std", "#![no_std]\nextern crate alloc;\nuse alloc::string::String;\n"
         "pub fn f(s: &String) -> usize { s.capacity() }\n", required=True)
    case("unstable-type", "#![no_std]\nuse core::cell::SyncUnsafeCell;\n", required=True)
    case("unstable-method", span + "use proc_macro::Span;\n"
         "pub fn f(s: &Span) -> core::ops::Range<usize> { s.byte_range() }\n")
    case("unstable-join", span + "use proc_macro::Span;\n"
         "pub fn f(s: &Span, o: Span) -> Option<Span> { s.join(o) }\n")
    case("wrong-return", span + "pub fn f(s: &proc_macro::Span) -> u32 { s.line() }\n")
    case("wrong-receiver", span + "pub fn f(s: &proc_macro::Span) -> String { s.line() }\n")
    case("absent-method", span + "pub fn f(s: &proc_macro::Span) -> usize { s.lines() }\n")
    case("absent-import", span + "use proc_macro::Spam;\n")
    case("no-std-without-alloc", "#![no_std]\nuse alloc::string::String;\n")
    case("std-in-no-std", "#![no_std]\npub fn f(s: &std::path::PathBuf) -> bool { s.has_root() }\n")
    case("private-field-type", "pub fn f(s: &String) -> usize { s.vec.len() }\n")
    case("duplicate", span + "use proc_macro::Span;\nuse proc_macro::Span;\n")
    case("moved-twice", "pub fn f(s: String) -> String { s.into_boxed_str().into_string() }\n")
    case("uncapped", span + "pub fn line(s: proc_macro::Span) -> usize { s.line() }\n",
         ["--edition=2021", "--crate-name=probe", "--crate-type=lib", "--emit=metadata"])
    # A target whose standard library is not installed fails with E0463 in rustc.
    for target in ("x86_64-unknown-linux-gnu", "wasm32-unknown-unknown",
                   "x86_64-pc-windows-msvc"):
        case(f"target-{target}", span + "pub fn line(s: proc_macro::Span) -> usize { s.line() }\n",
             [*common("probe", "2021"), "--target", target])
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--blaze", required=True, type=Path)
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    norustc = producer_plan.norustc
    real = Path(norustc.real_toolchain())
    host = subprocess.run([str(real / "bin/rustc"), "--print=host-tuple"],
                          capture_output=True, text=True, check=True).stdout.strip()
    audit_log = work / "product-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(audit_log)))
    env = dict(os.environ)
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_BOOTSTRAP",
                 "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "SDKROOT"):
        env.pop(name, None)
    marker = work / "unsupported"
    product = dict(env, RUSTUP_TOOLCHAIN=str(audit),
        PATH=str(audit / "bin") + os.pathsep + env["PATH"],
        RA_JIT_ANSWERS="own", BLAZE_ANSWERS="own",
        RA_JIT_STD_FROM="off", RA_JIT_PROC_MACRO_SERVER="0",
        RA_JIT_HOME=str(work / "home"), RA_JIT_CACHE=str(work / "store"),
        BLAZE_PRODUCER_PROBE_FAILURE=str(marker))
    rows = []
    for case in cases(host):
        name = case["name"]
        source = work / (name + ".rs")
        source.write_text(case["text"])
        outs = [work / (name + "-owned"), work / (name + "-oracle")]
        for out in outs:
            out.mkdir(exist_ok=True)
        ours_env = dict(product, **case["env"])
        oracle_env = dict(env, RUSTUP_TOOLCHAIN=str(real), **case["env"])
        marker.unlink(missing_ok=True)
        ours = subprocess.run([str(args.blaze.resolve()), "producer-rustc", *case["argv"],
                               "--out-dir", str(outs[0]), str(source)],
                              env=ours_env, cwd=work, capture_output=True, text=True)
        oracle = subprocess.run([str(real / "bin/rustc"), *case["argv"], "--out-dir",
                                 str(outs[1]), str(source)],
                                env=oracle_env, cwd=work, capture_output=True, text=True)
        (work / (name + "-owned.log")).write_text(ours.stdout + ours.stderr)
        (work / (name + "-oracle.log")).write_text(oracle.stdout + oracle.stderr)
        row = dict(case=name, ours=ours.returncode, oracle=oracle.returncode,
                   required=case["required"])
        rows.append(row)
        (work / "rows.json").write_text(json.dumps(rows, indent=2) + "\n")
        if marker.exists():
            assert ours.returncode != 0, row
            if case["bootstrap_refusal"]:
                assert "owned release feature catalog" in ours.stderr, ours.stderr
                row["verdict"] = "bootstrap-refused"
            else:
                assert "owned compiler probe acceptance is not proven" in ours.stderr, ours.stderr
                assert not case["required"], (row, ours.stderr)
                row["verdict"] = "undecided-refused"
        else:
            assert not case["bootstrap_refusal"], row
            assert (ours.returncode == 0) == (oracle.returncode == 0), (row, ours.stderr)
            row["verdict"] = "accepted" if ours.returncode == 0 else "rejected"
        print(f"{name}: owned={ours.returncode} oracle={oracle.returncode} {row['verdict']}",
              flush=True)
    calls = norustc.runs(str(audit_log))
    (work / "product-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
    assert not calls, calls
    decided = sum(row["verdict"] in ("accepted", "rejected") for row in rows)
    print(f"{len(rows)} source probes, {decided} decided with oracle parity, the rest "
          "refused loudly; every required probe decided; zero product compilers")


if __name__ == "__main__":
    main()
