#!/usr/bin/env python3
"""Plain Build/Run with scripts and macros, tool blockers and pre-cleanup token scans.

The independent Cargo oracle runs before the planted environment exists. Product
requests use the ordinary cargo-ra-jit CLI, with every compiler/Cargo proxy blocked.
On Linux, strace also records the real process tree. The matching owned server must
be freshly built separately; no toolchain server is substituted by this harness.
"""
import argparse
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
import crossmachine
import norustc
from tracedecay import Watchdog


def external_execs(text):
    return [path for path in re.findall(r'execve\("([^"\n]+)".*?= 0$', text, re.M)
            if Path(path).name.removesuffix(".real") in norustc.COUNTED]


def fixture(work):
    root = work / "fixture"
    shutil.copytree(HERE / "fixtures/paths/macros", root)
    derive = root / "derive"
    (derive / "build.rs").write_text('''fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("generated.rs"), "const SCRIPT: &str = \\\"generated\\\";").unwrap();
    println!("cargo::rustc-env=SCRIPT_LABEL=script");
    println!("cargo::rerun-if-changed=build.rs");
}
''')
    source = derive / "src/lib.rs"
    text = source.read_text().replace(
        "let suffix = macro_producer::suffix();",
        'let suffix = format!("{}:{}:{}", macro_producer::suffix(), SCRIPT, env!("SCRIPT_LABEL"));')
    source.write_text(text.replace('use proc_macro::',
        'include!(concat!(env!("OUT_DIR"), "/generated.rs"));\nuse proc_macro::', 1))
    return root


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--owned-server", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    args = parser.parse_args()
    work, bins = args.work.resolve(), args.bin_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    root = fixture(work)
    real = Path(norustc.real_toolchain())
    env = dict(os.environ, RUSTUP_TOOLCHAIN=str(real), RUST_BACKTRACE="0")
    for name in ("RUSTC", "RUSTDOC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                 "RA_JIT_PROC_MACRO_CACHE_SKIP", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    oracle = subprocess.run([str(real / "bin/cargo"), "run", "--locked", "-p", "app"],
        cwd=root, env=dict(env, CARGO_TARGET_DIR=str(work / "oracle-target")),
        capture_output=True, text=True)
    (work / "oracle.log").write_text(oracle.stdout + oracle.stderr)
    assert oracle.returncode == 0, f"oracle failed: {work / 'oracle.log'}"
    audit_log = work / "product-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(audit_log), block=True))
    token = "c4-plain-" + secrets.token_hex(24)
    # Keep socket paths within sockaddr_un's platform budget independently of
    # the caller's evidence/build directory depth.
    home = tempfile.mkdtemp(prefix="c4plain-")
    product = dict(env, PATH=str(audit / "bin") + os.pathsep + str(bins) + os.pathsep + env["PATH"],
        CARGO=str(audit / "bin/cargo"), RUSTC=str(audit / "bin/rustc"),
        RUSTDOC=str(audit / "bin/rustdoc"), RUSTUP_TOOLCHAIN=str(audit),
        RA_JIT_BIN=str(bins / "ra-jit"), RA_JIT_HOME=home,
        RA_JIT_CACHE=str(work / "store"), RA_JIT_PROC_MACRO_SERVER=str(args.owned_server.resolve()),
        RA_JIT_MEMORY_MAX="16G", RA_JIT_OWN_BASE_MEMORY="12G", RA_JIT_SPARE="0",
        RA_JIT_RUN_ID="c4-plain-" + str(os.getpid()), CARGO_TARGET_DIR=str(root / "target"),
        C4_PLANTED_TOKEN=token)
    watchdog = Watchdog(str(bins), run_id=product["RA_JIT_RUN_ID"],
        homes=(product["RA_JIT_HOME"],), series=str(work / "watchdog.jsonl"))
    watchdog.start()
    result = dict(rows=[], server=str(args.owned_server.resolve()))
    roots = [str(work), home]
    try:
        for step, command in [("build", ["build", "--locked", "-p", "app"]),
                              ("run", ["run", "--locked", "-p", "app"])]:
            argv = [str(bins / "cargo-ra-jit"), "ra-jit", *command]
            if sys.platform == "linux":
                argv = ["strace", "-f", "-e", "trace=execve", "-o", str(work / (step + ".exec")), *argv]
            began = time.monotonic()
            ran = subprocess.run(argv, cwd=root, env=product, capture_output=True, text=True)
            (work / (step + ".log")).write_text(ran.stdout + ran.stderr)
            scan = crossmachine.token_scan(roots, token)
            executed = external_execs((work / (step + ".exec")).read_text()) if sys.platform == "linux" else []
            row = dict(step=step, returncode=ran.returncode, seconds=time.monotonic() - began,
                       token_scan=scan, calls=norustc.runs(str(audit_log)), external_execs=executed)
            row["right"] = (ran.returncode == 0 and not row["calls"]
                            and not executed and not crossmachine.recognized_fallback(ran.stderr)
                            and not scan["hits"] and not scan["errors"])
            if step == "run":
                row["right"] &= ran.stdout.strip() == oracle.stdout.strip()
                row["actual"], row["expected"] = ran.stdout.strip(), oracle.stdout.strip()
            result["rows"].append(row)
            (work / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            if not row["right"]:
                break
    finally:
        result["before_stop"] = crossmachine.token_scan(roots, token)
        subprocess.run([str(bins / "ra-jit"), "cargo", "stop"], cwd=root, env=product,
                       capture_output=True)
        result["after_stop"] = crossmachine.token_scan(roots, token)
        watchdog.stop.set()
        watchdog.join(timeout=10)
        result["watchdog"] = watchdog.tripped
        (work / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        shutil.rmtree(home)
    scans = [result["before_stop"], result["after_stop"]]
    passed = (len(result["rows"]) == 2 and all(row["right"] for row in result["rows"])
              and not result["watchdog"] and all(not s["hits"] and not s["errors"] for s in scans))
    print(json.dumps(dict(passed=passed, result=str(work / "result.json"))))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
