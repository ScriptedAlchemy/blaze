#!/usr/bin/env python3
"""Exercise the P67 reviewer's nine-package fixture against the Cargo oracle.

Pass --review-fixture pointing to evidence/premerge-pr15-a31441f8/scripts/attack.py.
Its fixture definition is used unchanged. Cargo/rustc are reference-only.
The review is Claude Opus 5.5's P67-PASS+NOTES, not an independent review of this fix.
"""
import argparse
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import producer_plan
import source_macro


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--review-fixture", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--blaze", type=Path, required=True)
    parser.add_argument("--owned-server", type=Path, required=True)
    parser.add_argument("--raw-env", action="store_true")
    parser.add_argument("--syntax-error", action="store_true")
    args = parser.parse_args()
    work = args.work.resolve()
    assert not work.exists(), "use a fresh witness directory"
    work.mkdir(parents=True)
    saved = sys.argv
    try:
        # The review script reads four command arguments at import time, but its
        # main is not called. Only its fixture constructor is used.
        sys.argv = [str(args.review_fixture), str(work), str(args.blaze),
                    str(args.owned_server), str(work / "unused-trace")]
        reviewer = runpy.run_path(str(args.review_fixture))
    finally:
        sys.argv = saved
    ours = reviewer["fixture"](work / "ours", work / "ours-runs.log")
    oracle = reviewer["fixture"](work / "oracle", work / "oracle-runs.log")
    if args.raw_env:
        for root in (ours, oracle):
            script = root / "derive/build.rs"
            text = script.read_text()
            text = text.replace("std::env::vars().filter",
                "std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v))).filter")
            text = text.replace("    let gen = format!(",
                '    let raw = std::env::var_os("P67_RAW").map(|v| v.len());\n'
                '    println!("cargo::rerun-if-env-changed=P67_RAW");\n'
                '    let gen = format!(')
            text = text.replace('"{os};same=', '"{os};raw={raw:?};same=')
            script.write_text(text)
    real = Path(producer_plan.norustc.real_toolchain())
    env = dict(os.environ, RUSTUP_TOOLCHAIN=str(real))
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER",
                 "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET", "SDKROOT", "RUSTC_BOOTSTRAP"):
        env.pop(name, None)
    run = producer_plan.run
    for root in (ours, oracle):
        run([real / "bin/cargo", "generate-lockfile", "--offline"],
            root, env, work / (root.name + "-lock.log"))
    audit_log = work / "product-tools.log"
    audit = Path(producer_plan.norustc.stand_ins(str(work / "audit"), str(audit_log)))
    product = dict(env, RUSTUP_TOOLCHAIN=str(audit),
        PATH=str(audit / "bin") + os.pathsep + env["PATH"],
        RA_JIT_PROC_MACRO_SERVER="0", RA_JIT_STD_FROM="off",
        RA_JIT_HOME=str(work / "home"), RA_JIT_CACHE=str(work / "store"),
        RA_JIT_ANSWERS="own", BLAZE_ANSWERS="own",
        RA_JIT_OWN_RESOLVE="1", BLAZE_OWN_RESOLVE="1")
    host = "aarch64-apple-darwin"
    rows = []
    if args.syntax_error:
        for root in (ours, oracle):
            script = root / "derive/build.rs"
            script.write_text("fn main() { let broken = 'unterminated; }\n")
        selected = work / "syntax-selection.json"
        selected.write_text(json.dumps(dict(profile="dev", features=[], no_default_features=False,
            all_features=False, packages=["app"], workspace=False,
            target=None, dev_deps=False, config=[])))
        ours_run = subprocess.run([str(args.blaze.resolve()), "proc-macro-build",
            str(ours / "Cargo.toml"), str(selected), host, host, str(work / "producer")],
            cwd=ours, env=product, capture_output=True, text=True)
        oracle_run = subprocess.run([str(real / "bin/cargo"), "build", "--offline", "-p", "app"],
            cwd=oracle, env=dict(env, CARGO_TARGET_DIR=str(work / "oracle-target")),
            capture_output=True, text=True)
        (work / "syntax-owned.log").write_text(ours_run.stdout + ours_run.stderr)
        (work / "syntax-oracle.log").write_text(oracle_run.stdout + oracle_run.stderr)
        assert ours_run.returncode != 0 and oracle_run.returncode != 0
        assert "derive/build.rs" in ours_run.stderr, ours_run.stderr
        assert "linker command failed" not in ours_run.stderr, ours_run.stderr
        assert "error:" in ours_run.stderr, ours_run.stderr
        calls = producer_plan.norustc.runs(str(audit_log))
        (work / "product-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
        assert not calls, calls
        print("P67 script syntax-error fixture: source diagnostic before linking; both builds fail; zero product compilers")
        return
    try:
        steps = [("native", None, None), ("explicit-native", host, None),
                 ("cross", "aarch64-unknown-linux-gnu", None)]
        if args.raw_env:
            steps = [(name, None, value) for name, value in (
                ("raw-first", b"\xff\xfe"), ("raw-same", b"\xff\xfe"),
                ("raw-change", b"\xff\xfe\xfd"),
                ("raw-change-rebuilt", b"\xff\xfe\xfd"), ("raw-removed-rebuilt", None))]
        def counter(path):
            return path.read_text().splitlines() if path.exists() else []
        for name, target, raw in steps:
            if name.endswith("-rebuilt"):
                for root in (ours, oracle):
                    script = root / "derive/build.rs"
                    script.write_text(script.read_text() + "// force the script to read new raw input\n")
            before = [len(counter(work / (side + "-runs.log"))) for side in ("ours", "oracle")]
            product_step, oracle_step = dict(product), dict(env)
            if raw is not None:
                product_step["P67_RAW"] = oracle_step["P67_RAW"] = os.fsdecode(raw)
                product_step[os.fsdecode(b"P67_KEY_\xff")] = os.fsdecode(b"\xfe")
                oracle_step[os.fsdecode(b"P67_KEY_\xff")] = os.fsdecode(b"\xfe")
            domain = "raw" if args.raw_env else name
            selected = work / (name + "-selection.json")
            selected.write_text(json.dumps(dict(profile="dev", features=[], no_default_features=False,
                all_features=False, packages=["app"], workspace=False,
                target=target, dev_deps=False, config=[])))
            built = json.loads(run([args.blaze.resolve(), "proc-macro-build",
                ours / "Cargo.toml", selected, host, target or host, work / ("producer-" + domain)],
                ours, product_step, work / (name + "-product.log")))
            assert len(built) == 1, built
            actual = source_macro.expansions(args.owned_server.resolve(),
                Path(built[0]["library"]), work, product, work / (name + "-owned-transcript.json"))
            argv = [real / "bin/cargo", "check" if name == "cross" else "build", "--offline", "-p", "app"]
            if target:
                argv += ["--target", target]
            out = work / ("oracle-target-" + domain)
            run(argv, oracle, dict(oracle_step, CARGO_TARGET_DIR=str(out)), work / (name + "-oracle.log"))
            images = list((out / "debug/deps").glob("libsource_derive*.dylib"))
            assert len(images) == 1, images
            expected = source_macro.expansions(real / "libexec/rust-analyzer-proc-macro-srv",
                images[0], work, transcript=work / (name + "-oracle-transcript.json"))
            ran = [counter(work / (side + "-runs.log"))[at:] for side, at in
                   zip(("ours", "oracle"), before)]
            host_runs = [line for line in ran[1] if "target=true" not in line]
            rows.append(dict(mode=name, ours=actual, oracle=expected, equal=actual == expected,
                             ours_ran=ran[0], oracle_host_ran=host_runs))
            (work / "rows.json").write_text(json.dumps(rows, indent=2) + "\n")
            assert actual == expected, name
            assert sorted(ran[0]) == sorted(host_runs), rows[-1]
            if name == "raw-same":
                assert not ran[0], rows[-1]
    finally:
        calls = producer_plan.norustc.runs(str(audit_log))
        (work / "product-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
        assert not calls, calls
    print("P67 nine-package producer fixture: native/explicit-native/cross expansions equal oracle; zero product compilers")


if __name__ == "__main__":
    main()
