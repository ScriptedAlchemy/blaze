#!/usr/bin/env python3
"""Source-only proc-macro compiler boundary, not a cold-path or host-resolver gate.

The fixture supplies a resolved host plan, with real syn/quote sources and a shared
dependency compiled under different host/target features. No wrapper record or oracle
artifact enters that plan. Product compilation is audited with norustc's stand-ins.
Reference compilation/expansion happens separately, only in this harness.

Run under the heavy lock and memory watchdog. --prepare writes the plan only.
--build-only audits a native build but does not claim expansion parity.
The full test requires --owned-server built by blaze, with its matching proc_macro ABI.
"""
import argparse
import glob
import json
import os
from pathlib import Path
import subprocess
import sys

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
import norustc

FIXTURE = Path(__file__).resolve().parent / "fixtures" / "source-macro"
VERSIONS = {"syn": "2.0.119", "quote": "1.0.41",
            "proc-macro2": "1.0.106", "unicode-ident": "1.0.24"}
FEATURES = {"syn": ["derive", "parsing", "printing", "clone-impls"]}


def source(package):
    paths = glob.glob(str(Path.home() / ".cargo" / "registry" / "src" / "*" /
                          f"{package}-{VERSIONS[package]}" / "src" / "lib.rs"))
    if len(paths) != 1:
        raise RuntimeError(f"need one cached source of {package} {VERSIONS[package]}: {paths}")
    return str(Path(paths[0]).resolve())


def prepare(work, light=False):
    # Explicit resolved units are the API input being tested. Resolving host units from
    # an application's manifest is a later gate, not simulated here with unified metadata.
    def crate(name, root, deps=(), features=()):
        return dict(name=name, root=str(Path(root).resolve()), edition="2021",
                    deps=list(deps), cfg=[f'feature="{f}"' for f in features],
                    codegen_flags=[], env={})
    if light:
        plan = [
            crate("source_derive", FIXTURE / "light.rs", [("shared", 1)]),
            crate("shared", FIXTURE / "shared.rs", features=["host"]),
        ]
        (work / "host-plan.json").write_text(json.dumps(plan, indent=2) + "\n")
        return plan
    plan = [
        crate("source_derive", FIXTURE / "derive.rs", [("syn", 1), ("quote", 2), ("shared", 5)]),
        crate("syn", source("syn"), [("proc_macro2", 3), ("quote", 2), ("unicode_ident", 4)],
              FEATURES["syn"]),
        crate("quote", source("quote"), [("proc_macro2", 3)]),
        crate("proc_macro2", source("proc-macro2"), [("unicode_ident", 4)]),
        crate("unicode_ident", source("unicode-ident")),
        crate("shared", FIXTURE / "shared.rs", features=["host"]),
    ]
    (work / "host-plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    return plan


def run(argv, log, env=None):
    result = subprocess.run(list(map(str, argv)), capture_output=True, text=True, env=env)
    log.write_text("$ " + " ".join(map(str, argv)) + "\n" + result.stdout + result.stderr)
    if result.returncode:
        raise RuntimeError(f"exit {result.returncode}; see {log}")
    return result.stdout.strip()


def oracle(plan, sysroot, work):
    out = work / "oracle"
    out.mkdir(exist_ok=True)
    rustc = sysroot / "bin" / "rustc"
    built = {}
    # Reference-only topological compilation. No cargo, target cache or record is reused.
    for at in [1, 0] if len(plan) == 2 else [4, 3, 2, 1, 5, 0]:
        unit = plan[at]
        path = out / f"lib{unit['name']}.rlib"
        if at == 0:
            path = out / f"libsource_derive{'.dylib' if sys.platform == 'darwin' else '.so'}"
        args = [rustc, unit["root"], "--crate-name", unit["name"], "--edition=2021",
                "--crate-type", "proc-macro" if at == 0 else "rlib", "-o", path, "-L", out]
        for cfg in unit["cfg"]:
            args += ["--cfg", cfg]
        for name, dep in unit["deps"]:
            args += ["--extern", f"{name}={built[dep]}"]
        run(args, out / f"{unit['name']}.log")
        built[at] = path
    # Also compile the other unit of shared under target-only cfg and run it.
    target = out / "libshared_target.rlib"
    run([rustc, FIXTURE / "shared.rs", "--crate-name", "shared", "--crate-type", "rlib",
         "--cfg", 'feature="target"', "-o", target], out / "shared-target.log")
    main = out / "target.rs"
    main.write_text('fn main() { assert_eq!(shared::label(), "target-only"); }\n')
    run([rustc, main, "--edition=2021", "--extern", f"shared={target}", "-o", out / "target"],
        out / "target-compile.log")
    run([out / "target"], out / "target-run.log")
    return built[0]


def expansions(server, library, work):
    env = {**os.environ, "RUST_ANALYZER_INTERNALS_DO_NOT_USE": "1"}
    process = subprocess.Popen([str(server)], env=env, stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    def request(value):
        process.stdin.write(json.dumps(value) + "\n")
        process.stdin.flush()
        line = process.stdout.readline()
        if not line:
            raise RuntimeError(f"server exited: {process.stderr.read()}")
        return json.loads(line)
    try:
        assert request({"ApiVersionCheck": {}})["ApiVersionCheck"] == 6
        listed = request({"ListMacros": {"dylib_path": str(library)}})["ListMacros"]
        assert "Ok" in listed, listed
        assert sorted(listed["Ok"]) == [["Describe", "CustomDerive"], ["host_label", "FuncLike"]]
        empty = dict(subtree=[0, 0, 0, 0, 0], literal=[], punct=[], ident=[],
                     token_tree=[], text=[])
        derive = {**empty, "subtree": [0, 0, 0, 0, 3], "ident": [0, 0, 0, 0, 1, 0],
                  "punct": [0, ord(";"), 0], "token_tree": [3, 7, 2],
                  "text": ["struct", "Point"]}
        outputs = {}
        for name, body in [("Describe", derive), ("host_label", empty)]:
            response = request({"ExpandMacro": dict(lib=str(library), env=[],
                current_dir=str(work), macro_body=body, macro_name=name, attributes=None)})
            tree = response["ExpandMacro"]
            assert "Ok" in tree, tree
            outputs[name] = normalize(tree["Ok"])
        return outputs
    finally:
        process.stdin.close()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def normalize(tree):
    """Compare token structure/text, ignoring only server-local span ids."""
    def child(tagged):
        index, tag = tagged >> 2, tagged & 3
        if tag == 0:
            _, _, kind, first, last = tree["subtree"][index * 5:index * 5 + 5]
            return [kind, [child(it) for it in tree["token_tree"][first:last]]]
        if tag == 1:
            _, text, kind, suffix = tree["literal"][index * 4:index * 4 + 4]
            return ["literal", tree["text"][text],
                    "" if suffix == 0xffffffff else tree["text"][suffix], kind]
        if tag == 2:
            _, char, spacing = tree["punct"][index * 3:index * 3 + 3]
            return ["punct", char, spacing]
        _, text, raw = tree["ident"][index * 3:index * 3 + 3]
        return ["ident", tree["text"][text], raw]
    return child(0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--blaze", type=Path)
    parser.add_argument("--owned-server", type=Path)
    parser.add_argument("--prepare", action="store_true")
    parser.add_argument("--build-only", action="store_true")
    parser.add_argument("--oracle-only", action="store_true",
                        help="exercise only the separate rustc reference and target feature witness")
    parser.add_argument("--light", action="store_true", help="path-only producer before registry closure")
    parser.add_argument("--default-sdk", action="store_true",
                        help="test the default SDK, not Python's injected SDKROOT")
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    plan = prepare(work, args.light)
    if args.prepare:
        print(f"prepared {len(plan)} source-only host units; no compiler run")
        return
    if args.oracle_only:
        real = Path(norustc.real_toolchain())
        reference = oracle(plan, real, work)
        theirs = expansions(real / "libexec" / "rust-analyzer-proc-macro-srv", reference, work)
        (work / "oracle-expansions.json").write_text(json.dumps(theirs, indent=2) + "\n")
        print("oracle derive/function-like expansions and target-only shared unit pass")
        return
    assert args.blaze
    for name, value in {"RA_JIT_MEMORY_MAX": "16G", "RA_JIT_OWN_BASE_MEMORY": "12G",
                        "RA_JIT_SPARE": "0"}.items():
        assert os.environ.get(name) == value, f"run under the memory watchdog with {name}={value}"
    audit_log = work / "product-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(audit_log)))
    env = {**os.environ, "PATH": str(audit / "bin") + os.pathsep + os.environ["PATH"],
           "RUSTUP_TOOLCHAIN": str(audit), "RA_JIT_STD_FROM": "off",
           "RA_JIT_HOME": str(work / "home"), "RA_JIT_CACHE": str(work / "store"),
           "RA_JIT_PROC_MACRO_SERVER": "0"}
    if args.default_sdk:
        env.pop("SDKROOT", None)
    (work / "environment.json").write_text(json.dumps(
        {"SDKROOT": env.get("SDKROOT"), "default_sdk": args.default_sdk}, indent=2) + "\n")
    before = len(norustc.runs(str(audit_log)))
    try:
        library = Path(run([args.blaze.resolve(), "proc-macro-source", audit,
                           work / "host-plan.json", work / "producer"],
                          work / "product-build.log", env))
    finally:
        calls = norustc.runs(str(audit_log))[before:]
        (work / "product-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
        assert not calls, f"product ran counted tools: {calls}"
    if args.build_only:
        print(f"built {library}; zero rustc/cargo calls; expansion NOT CHECKED")
        return
    assert args.owned_server, "full gate needs a matching blaze-built owned server"
    real = Path(norustc.real_toolchain())  # reference-only toolchain query
    reference = oracle(plan, real, work)
    theirs = expansions(real / "libexec" / "rust-analyzer-proc-macro-srv", reference, work)
    ours = expansions(args.owned_server.resolve(), library, work)
    (work / "expansions.json").write_text(json.dumps(dict(ours=ours, oracle=theirs), indent=2))
    assert ours == theirs, "owned expansion differs from rustc oracle"
    print("derive/function-like expansions equal oracle; separate host/target features; no product cargo/rustc")


if __name__ == "__main__":
    main()
