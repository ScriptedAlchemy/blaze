#!/usr/bin/env python3
"""Compile or loudly refuse the token producer with real registry defaults.

The sources and macro body are source_macro's existing fixture. Cargo/rustc are
reference-only. All product compilation and expansion use the owned compiler
and matching server, under the caller's heavy lock and watchdog.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
import subprocess

sys.path.insert(0, str(Path(__file__).resolve().parent))
import producer_plan
import source_macro


def fixture(work):
    root = producer_plan.fixture(work)
    derive = root / "derive"
    text = (derive / "Cargo.toml").read_text()
    for name in ("syn", "quote"):
        text += f'{name} = "={source_macro.VERSIONS[name]}"\n'
    (derive / "Cargo.toml").write_text(text)
    (derive / "src/lib.rs").write_text((source_macro.FIXTURE / "derive.rs").read_text())
    # Pin the closure already used by source_macro rather than resolve newer
    # cached versions. These are real registry dependencies with defaults on.
    with (derive / "Cargo.toml").open("a") as out:
        for name in ("proc-macro2", "unicode-ident"):
            out.write(f'{name} = "={source_macro.VERSIONS[name]}"\n')
    return root


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--blaze", required=True, type=Path)
    parser.add_argument("--owned-server", required=True, type=Path)
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    assert not (work / "producer").exists(), "use a fresh witness directory"
    root = fixture(work)
    norustc = producer_plan.norustc
    real = Path(norustc.real_toolchain())
    host = "aarch64-apple-darwin"
    assert sys.platform == "darwin"
    env = dict(os.environ, RUSTUP_TOOLCHAIN=str(real))
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER",
                 "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET", "SDKROOT",
                 "RUSTC_BOOTSTRAP"):
        env.pop(name, None)
    run = producer_plan.run
    run([real / "bin/cargo", "generate-lockfile", "--offline"],
        root, env, work / "lock-oracle.log")
    selection = work / "selection.json"
    selection.write_text(json.dumps(dict(profile="dev", features=[], no_default_features=False,
        all_features=False, packages=["app"], workspace=False,
        target=None, dev_deps=False, config=[])))
    audit_log = work / "product-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(audit_log)))
    product = dict(env, RUSTUP_TOOLCHAIN=str(audit),
        PATH=str(audit / "bin") + os.pathsep + env["PATH"],
        RA_JIT_ANSWERS="own", BLAZE_ANSWERS="own",
        RA_JIT_OWN_RESOLVE="1", BLAZE_OWN_RESOLVE="1",
        RA_JIT_STD_FROM="off", RA_JIT_PROC_MACRO_SERVER="0",
        RA_JIT_HOME=str(work / "home"), RA_JIT_CACHE=str(work / "store"))
    try:
        plan = json.loads(run([args.blaze.resolve(), "proc-macro-plan",
            root / "Cargo.toml", selection, host, host],
            root, product, work / "product-plan.log"))
        host_units = {it["package"]: it for it in plan["units"] if it["host"]}
        (work / "host-units.json").write_text(json.dumps(host_units, indent=2) + "\n")
        for name in ("syn", "quote", "proc-macro2"):
            unit = next(it for key, it in host_units.items()
                        if key.split("#")[-1].startswith(name + "@"))
            assert "default" in unit["features"], unit
            if name != "syn":
                assert "proc-macro" in unit["features"], unit
        argv = [args.blaze.resolve(), "proc-macro-build",
                root / "Cargo.toml", selection, host, host, work / "producer"]
        result = subprocess.run(list(map(str, argv)), cwd=root, env=product,
                                capture_output=True, text=True)
        (work / "product-build.log").write_text(result.stdout + result.stderr)
        refused = result.returncode != 0
        if refused:
            assert "could not compile a producer's compiler probe" in result.stderr, result.stderr
            assert "owned compiler probe acceptance is not proven" in result.stderr, result.stderr
            assert not result.stdout.strip(), "a refused build published an image"
            assert not list((work / "producer").rglob("complete.json")), "refused image admitted"
        else:
            built = json.loads(result.stdout)
            assert len(built) == 1, built
            library = Path(built[0]["library"])
            ours = source_macro.expansions(args.owned_server.resolve(), library, work, product,
                work / "owned-transcript.json")
    finally:
        calls = norustc.runs(str(audit_log))
        (work / "product-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
        assert not calls, calls
    run([real / "bin/cargo", "build", "--offline", "-p", "app"],
        root, dict(env, CARGO_TARGET_DIR=str(work / "oracle-target")),
        work / "oracle-build.log")
    if refused:
        (work / "verdict.json").write_text(json.dumps({
            "status": "loud-refusal", "oracle": "accepted", "product_calls": [],
            "reason": "compiler probe acceptance remains unproven; no image admitted",
        }, indent=2) + "\n")
        print("registry defaults: oracle accepts, product refuses unproven probes; no image, zero product calls")
        return
    def outputs(top, package):
        paths = list(top.rglob("output"))
        return [path.read_text() for path in paths if path.parent.name.startswith(package + "-")]
    for package in ("proc-macro2", "quote"):
        owned = outputs(work / "producer/scripts", package)
        reference = outputs(work / "oracle-target/debug/build", package)
        assert len(owned) == len(reference) == 1, (owned, reference)
        assert owned == reference, (package, owned, reference)
    images = list((work / "oracle-target/debug/deps").glob("libsource_derive*.dylib"))
    assert len(images) == 1, images
    theirs = source_macro.expansions(real / "libexec/rust-analyzer-proc-macro-srv",
        images[0], work, transcript=work / "oracle-transcript.json")
    (work / "expansions.json").write_text(json.dumps(dict(ours=ours, oracle=theirs), indent=2) + "\n")
    assert ours == theirs, (ours, theirs)
    pins = {str(path): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in (args.blaze.resolve(), args.owned_server.resolve(), library, images[0])}
    (work / "pins.json").write_text(json.dumps(pins, indent=2) + "\n")
    print("real syn/quote/proc-macro2 defaults compiled and expanded: equal oracle; zero product compilers")


if __name__ == "__main__":
    main()
