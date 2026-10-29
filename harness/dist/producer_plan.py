#!/usr/bin/env python3
"""Compare application-derived host units with an independent Cargo unit-graph oracle.

The product reads manifests with its own resolver and profile planner. No reference unit
graph or artifact enters it. Native, explicit-native and cross-target plans exercise disjoint
features, optional renamed dependencies, host predicates, profiles, config env and rustflags.
The native producer is compiled by blaze and expanded in the matching owned server.
Run under the heavy lock and memory watchdog. This does not switch a cold path.
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
sys.path.insert(0, str(Path(__file__).resolve().parent))
import source_macro


def cross_target(host):
    return "x86_64-unknown-linux-gnu" if host.startswith("aarch64-") else "aarch64-unknown-linux-gnu"


def fixture(work, host=None):
    host = host or norustc.reference_host(norustc.real_toolchain())
    root = work / "app"
    root.mkdir(parents=True, exist_ok=True)
    (root / "Cargo.toml").write_text('''[workspace]
members = ["app", "derive", "shared", "optional"]
resolver = "2"
[profile.dev.build-override]
opt-level = 1
debug-assertions = false
overflow-checks = true
[profile.dev.package.shared]
opt-level = 2
''')
    files = {
        "app": ('''[package]
name = "app"
version = "0.1.0"
edition = "2021"
[dependencies]
source_derive = { path = "../derive" }
shared = { path = "../shared", features = ["target"] }
''', 'fn main() { assert_eq!(shared::label(), "target-only"); }\n'),
        "derive": ('''[package]
name = "source_derive"
version = "0.1.0"
edition = "2021"
[lib]
proc-macro = true
[dependencies]
shared = { path = "../shared", features = ["host", "optional"] }
''', (source_macro.FIXTURE / "light.rs").read_text()),
        "shared": ('''[package]
name = "shared"
version = "0.1.0"
edition = "2021"
[features]
host = []
target = []
optional = ["dep:renamed"]
[target.'cfg(unix)'.dependencies]
renamed = { package = "optional", path = "../optional", optional = true }
''', (source_macro.FIXTURE / "shared.rs").read_text() +
                  '\n#[cfg(all(feature="host", feature="optional"))]\n'
                  'const _: () = assert!(renamed::HOST);\n'),
        "optional": ('''[package]
name = "optional"
version = "0.1.0"
edition = "2021"
''', 'pub const HOST: bool = true;\n'),
    }
    for name, (manifest, source) in files.items():
        path = root / name / "src"
        path.mkdir(parents=True, exist_ok=True)
        (path.parent / "Cargo.toml").write_text(manifest)
        (path / ("main.rs" if name == "app" else "lib.rs")).write_text(source)
    config = root / ".cargo"
    config.mkdir(exist_ok=True)
    (config / "config.toml").write_text(f'''[env]
PRODUCER_CONFIG_ENV = "from-config"
PRODUCER_KEEP_ENV = "ignored"
PRODUCER_FORCE_ENV = {{ value = "forced", force = true }}
[target.{host}]
rustflags = ["--cfg", "native_flag"]
[target.{cross_target(host)}]
rustflags = ["--cfg", "target_only_flag"]
''')
    return root


def run(argv, cwd, env, log):
    result = subprocess.run(list(map(str, argv)), cwd=cwd, env=env,
                            capture_output=True, text=True)
    log.write_text("$ " + " ".join(map(str, argv)) + "\n" + result.stdout + result.stderr)
    assert result.returncode == 0, f"exit {result.returncode}; see {log}"
    return result.stdout


def host_closure(plan):
    units, crates = plan["units"], plan["crates"]
    root = next(i for i, it in enumerate(units) if it["proc_macro"])
    order = []
    def add(at):
        if at in order:
            return
        assert units[at]["host"]
        assert not units[at]["build_script"], "script outputs require the owned scheduler"
        assert at == root or not units[at]["proc_macro"], "nested producers require scheduling"
        order.append(at)
        for dep in units[at]["dependencies"]:
            assert not dep["build"]
            add(dep["unit"])
    add(root)
    slots = {old: new for new, old in enumerate(order)}
    return [{**crates[at], "deps": [(name, slots[dep]) for name, dep in crates[at]["deps"]]}
            for at in order]


def compare(plan, graph, explicit):
    units = plan["units"]
    target_reached = set()
    pending = list(graph["roots"])
    while pending:
        at = pending.pop()
        it = graph["units"][at]
        if at in target_reached or "proc-macro" in it["target"]["kind"]:
            continue
        target_reached.add(at)
        pending += [dep["index"] for dep in it["dependencies"]]
    refs = {(it["pkg_id"], at not in target_reached): it
            for at, it in enumerate(graph["units"]) if it["mode"] == "build"}
    observed = {(it["package"], it["host"]): it for it in units}
    assert observed.keys() == refs.keys(), (observed.keys(), refs.keys())
    for k, it in refs.items():
        ours = observed[k]
        assert sorted(ours["features"]) == sorted(it["features"]), (k, ours, it)
        crate = plan["crates"][units.index(ours)]
        profile = it["profile"]
        assert f'-Copt-level={profile["opt_level"]}' in crate["codegen_flags"], (k, crate, profile)
        assert ("debug_assertions" in crate["cfg"]) == profile["debug_assertions"], (k, crate, profile)
        assert ("overflow_checks" in crate["cfg"]) == profile["overflow_checks"], (k, crate, profile)
        assert f'panic="{profile["panic"]}"' in crate["cfg"], (k, crate, profile)
        deps = {(units[dep["unit"]]["package"], dep["name"])
                for dep in ours["dependencies"] if not dep["build"]}
        expected = {(graph["units"][dep["index"]]["pkg_id"], dep["extern_crate_name"])
                    for dep in it["dependencies"] if graph["units"][dep["index"]]["mode"] == "build"}
        assert deps == expected, (k, deps, expected)
        if k[1]:
            assert ("native_flag" in crate["cfg"]) == (not explicit), (k, crate)
            assert "target_only_flag" not in crate["cfg"], (k, crate)
            assert not any(name.startswith("PRODUCER_") for name in crate["env"])
            assert crate["env"]["CARGO_PKG_NAME"] == it["target"]["name"]
    shared = [it for it, crate in zip(units, plan["crates"]) if crate["name"] == "shared"]
    assert sorted((it["host"], sorted(it["features"])) for it in shared) == [
        (False, ["target"]), (True, ["host", "optional"])]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--blaze", type=Path, required=True)
    parser.add_argument("--owned-server", type=Path, required=True)
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    real = Path(norustc.real_toolchain())
    host = norustc.reference_host(real)
    root = fixture(work, host)
    env = dict(os.environ, RUSTUP_TOOLCHAIN=str(real), RUSTC_BOOTSTRAP="1",
               PRODUCER_KEEP_ENV="caller", PRODUCER_FORCE_ENV="caller")
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER",
                 "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET"):
        env.pop(name, None)
    run([real / "bin/cargo", "generate-lockfile", "--offline"], root, env, work / "lock-oracle.log")
    audit_log = work / "product-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(audit_log)))
    product_env = {**env, "RUSTUP_TOOLCHAIN": str(audit),
                   "PATH": str(audit / "bin") + os.pathsep + env["PATH"],
                   "RA_JIT_PROC_MACRO_SERVER": "0", "RA_JIT_STD_FROM": "off",
                   "RA_JIT_HOME": str(work / "home"), "RA_JIT_CACHE": str(work / "store"),
                   "CARGO_TARGET_DIR": str(work / "target")}
    sdk = product_env.pop("SDKROOT", None)
    (work / "sdk-choice.json").write_text(json.dumps({"default-sdk": True, "inherited": sdk}) + "\n")
    native = None
    for label, explicit, target in [
            ("native", False, host), ("explicit-native", True, host),
            ("cross", True, cross_target(host))]:
        selection = dict(profile="dev", features=[], no_default_features=False,
                         all_features=False, packages=["app"], workspace=False,
                         target=target if explicit else None, dev_deps=False, config=[])
        selected = work / f"{label}-selection.json"
        selected.write_text(json.dumps(selection))
        argv = [real / "bin/cargo", "build", "--unit-graph", "-Zunstable-options", "-p", "app", "--offline"]
        if explicit:
            argv += ["--target", target]
        graph = json.loads(run(argv, root, env, work / f"{label}-oracle.log"))
        plan = json.loads(run([args.blaze.resolve(), "proc-macro-plan", root / "Cargo.toml",
                              selected, host, target], root, product_env, work / f"{label}-product.log"))
        (work / f"{label}-plan.json").write_text(json.dumps(plan, indent=2) + "\n")
        compare(plan, graph, explicit)
        if not explicit:
            native = plan
    closure = host_closure(native)
    description = work / "host-plan.json"
    description.write_text(json.dumps(closure, indent=2) + "\n")
    built = json.loads(run([args.blaze.resolve(), "proc-macro-build", root / "Cargo.toml",
                           work / "native-selection.json", host, host, work / "producer"],
                          root, product_env, work / "build.log"))
    assert len(built) == 1, built
    library = Path(built[0]["library"])
    ours = source_macro.expansions(args.owned_server.resolve(), library, work, product_env,
                                   work / "owned-transcript.json")
    # Independent Cargo compiles the manifests, not the product's serialized plan.
    reference_env = {**env, "CARGO_TARGET_DIR": str(work / "oracle-target")}
    run([real / "bin/cargo", "build", "--offline", "-p", "app"],
        root, reference_env, work / "reference-build.log")
    run([work / "oracle-target/debug/app"], root, reference_env, work / "reference-target.log")
    suffix = ".dylib" if sys.platform == "darwin" else ".so"
    images = list((work / "oracle-target/debug/deps").glob("libsource_derive*" + suffix))
    assert len(images) == 1, images
    reference = images[0]
    theirs = source_macro.expansions(real / "libexec/rust-analyzer-proc-macro-srv", reference, work,
                                     transcript=work / "oracle-transcript.json")
    (work / "expansions.json").write_text(json.dumps(dict(ours=ours, oracle=theirs), indent=2) + "\n")
    assert ours == theirs
    calls = norustc.runs(str(audit_log))
    (work / "product-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
    assert not calls, calls
    pins = {str(path): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (args.blaze.resolve(), args.owned_server.resolve(), library, reference)}
    (work / "pins.json").write_text(json.dumps(pins, indent=2) + "\n")
    print("three unit graphs equal oracle; app-derived producer expansions equal; zero product compilers")


if __name__ == "__main__":
    main()
