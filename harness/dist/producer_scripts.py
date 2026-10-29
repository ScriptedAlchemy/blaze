#!/usr/bin/env python3
"""Owned host producer scripts, default features and generated inputs against Cargo.

This uses the repo's vendored token producer, not a registry coverage claim.
Cargo/rustc are reference-only. Run under the heavy lock and memory watchdog.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import producer_plan
import source_macro


def fixture(work, nested=False, host=None):
    root = producer_plan.fixture(work, host)
    derive = root / "derive"
    with (derive / "Cargo.toml").open("a") as out:
        out.write('''[features]
default = ["script-feature"]
script-feature = []
[build-dependencies]
renamed_build = { package = "build_helper", path = "../build-helper" }
''')
    with (root / "shared/Cargo.toml").open("a") as out:
        out.write('\n[package.metadata]\nfixture = true\n')
    text = (root / "shared/Cargo.toml").read_text()
    (root / "shared/Cargo.toml").write_text(text.replace('edition = "2021"', 'edition = "2021"\nlinks = "host_bits"'))
    (root / "shared/build.rs").write_text('''fn main() {
    let domain = if std::env::var_os("CARGO_FEATURE_HOST").is_some() {
        assert!(std::env::var_os("CARGO_FEATURE_TARGET").is_none());
        "host-only"
    } else { "target-only" };
    println!("cargo::metadata=domain={}", domain);
    println!("cargo::rerun-if-changed=build.rs");
}
''')
    helper = root / "build-helper/src"
    helper.mkdir(parents=True)
    (helper.parent / "Cargo.toml").write_text('''[package]
name = "build_helper"
version = "0.1.0"
edition = "2021"
''')
    (helper / "lib.rs").write_text('pub fn label() -> &' + "'static str { \"host-only\" }\n")
    (derive / "build.rs").write_text('''fn main() {
    assert!(std::env::var_os("CARGO_FEATURE_SCRIPT_FEATURE").is_some());
    let label = renamed_build::label();
    assert_eq!(std::env::var("DEP_HOST_BITS_DOMAIN").unwrap(), label);
    let output = std::process::Command::new(std::env::var_os("RUSTC").unwrap())
        .arg("--version").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().starts_with("rustc 1.99."));
    let generated = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("label.rs");
    std::fs::write(generated, format!("pub const LABEL: &str = {:?};", label)).unwrap();
    println!("cargo::rustc-cfg=producer_generated");
    println!("cargo::rustc-env=PRODUCER_SCRIPT_LABEL={}", label);
    println!("cargo::rerun-if-changed=build.rs");
}
''')
    text = (derive / "src/lib.rs").read_text().replace(
        "shared::label()", '{ assert_eq!(generated::LABEL, env!("PRODUCER_SCRIPT_LABEL")); shared::label() }')
    (derive / "src/lib.rs").write_text('''#[cfg(not(producer_generated))]
compile_error!("missing producer script cfg");
mod generated { include!(concat!(env!("OUT_DIR"), "/label.rs")); }
const _: () = assert!(env!("PRODUCER_CONFIG_ENV").len() == 11);
const _: () = assert!(env!("PRODUCER_KEEP_ENV").len() == 6);
const _: () = assert!(env!("PRODUCER_FORCE_ENV").len() == 6);
''' + text)
    # Explicit membership also makes the helper's default profile a workspace profile.
    text = (root / "Cargo.toml").read_text().replace('"optional"]', '"optional", "build-helper"]')
    (root / "Cargo.toml").write_text(text)
    if nested:
        # source_derive -> shared -> inner producer; the inner producer's script
        # compiles a helper that itself uses a second producer.
        for name in ("inner", "seed", "script-helper"):
            path = root / name / "src"
            path.mkdir(parents=True)
            macro = name != "script-helper"
            (path.parent / "Cargo.toml").write_text(
                f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
                + ('[lib]\nproc-macro = true\n' if macro else ''))
        (root / "seed/src/lib.rs").write_text('''extern crate proc_macro;
#[proc_macro]
pub fn seed(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    "pub fn label() -> &'static str { \\"host-only\\" }".parse().unwrap()
}
''')
        with (root / "script-helper/Cargo.toml").open("a") as out:
            out.write('[dependencies]\nseed = { path = "../seed" }\n')
        (root / "script-helper/src/lib.rs").write_text("seed::seed!();\n")
        with (root / "inner/Cargo.toml").open("a") as out:
            out.write('[build-dependencies]\nscript-helper = { path = "../script-helper" }\n')
        (root / "inner/build.rs").write_text('''fn main() {
    let path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("label.rs");
    std::fs::write(path, format!("pub const LABEL: &str = {:?};", script_helper::label())).unwrap();
    println!("cargo::rerun-if-changed=build.rs");
}
''')
        (root / "inner/src/lib.rs").write_text('''extern crate proc_macro;
mod generated { include!(concat!(env!("OUT_DIR"), "/label.rs")); }
#[proc_macro]
pub fn label(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    format!("{:?}", generated::LABEL).parse().unwrap()
}
''')
        with (root / "shared/Cargo.toml").open("a") as out:
            out.write('[dependencies]\ninner = { path = "../inner" }\n')
        shared = root / "shared/src/lib.rs"
        shared.write_text(shared.read_text().replace('"host-only"', 'inner::label!()'))
        text = (root / "Cargo.toml").read_text().replace(
            '"build-helper"]', '"build-helper", "inner", "seed", "script-helper"]')
        (root / "Cargo.toml").write_text(text)
    return root


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--blaze", type=Path, required=True)
    parser.add_argument("--owned-server", type=Path, required=True)
    parser.add_argument("--nested", action="store_true")
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    norustc = producer_plan.norustc
    real = Path(norustc.real_toolchain())
    host = norustc.reference_host(real)
    root = fixture(work, args.nested, host)
    env = dict(os.environ, RUSTUP_TOOLCHAIN=str(real), PRODUCER_KEEP_ENV="caller")
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER",
                 "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET", "SDKROOT"):
        env.pop(name, None)
    run = producer_plan.run
    run([real / "bin/cargo", "generate-lockfile", "--offline"], root, env, work / "lock-oracle.log")
    selected = work / "selection.json"
    selected.write_text(json.dumps(dict(profile="dev", features=[], no_default_features=False,
                                       all_features=False, packages=["app"], workspace=False,
                                       target=None, dev_deps=False, config=[])))
    audit_log = work / "product-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(audit_log)))
    product = dict(env, RUSTUP_TOOLCHAIN=str(audit), PATH=str(audit / "bin") + os.pathsep + env["PATH"],
                   RA_JIT_ANSWERS="own", BLAZE_ANSWERS="own",
                   RA_JIT_OWN_RESOLVE="1", BLAZE_OWN_RESOLVE="1",
                   RA_JIT_STD_FROM="off", RA_JIT_PROC_MACRO_SERVER=str(args.owned_server.resolve()),
                   RA_JIT_HOME=str(work / "home"), RA_JIT_CACHE=str(work / "store"),
                   CARGO_TARGET_DIR=str(work / "target"))
    # A synthetic credential, not a real secret. Child processes receive the
    # caller's environment, but producer/link diagnostics must not print it.
    secret = "c4-not-a-real-token-log-regression"
    product["C4_PRIVATE_TOKEN"] = secret
    try:
        built = json.loads(run([args.blaze.resolve(), "proc-macro-build", root / "Cargo.toml",
                               selected, host, host, work / "producer"],
                              root, product, work / "product-build.log"))
        assert len(built) == (3 if args.nested else 1)
        library = Path(next(it["library"] for it in built if "#source_derive@" in it["package"]))
        ours = source_macro.expansions(args.owned_server.resolve(), library, work, product,
                                       work / "owned-transcript.json")
    finally:
        calls = norustc.runs(str(audit_log))
        (work / "product-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
        assert not calls, calls
    reference_env = dict(env, CARGO_TARGET_DIR=str(work / "oracle-target"))
    run([real / "bin/cargo", "build", "--offline", "-p", "app"],
        root, reference_env, work / "reference-build.log")
    suffix = ".dylib" if sys.platform == "darwin" else ".so"
    images = list((work / "oracle-target/debug/deps").glob("libsource_derive*" + suffix))
    assert len(images) == 1, images
    theirs = source_macro.expansions(real / "libexec/rust-analyzer-proc-macro-srv", images[0], work,
                                     transcript=work / "oracle-transcript.json")
    (work / "expansions.json").write_text(json.dumps(dict(ours=ours, oracle=theirs), indent=2) + "\n")
    assert ours == theirs
    pins = {str(path): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (args.blaze.resolve(), args.owned_server.resolve(), library, images[0])}
    (work / "pins.json").write_text(json.dumps(pins, indent=2) + "\n")
    # An unchanged image and its scripts must not be compiled or run again.
    script_files = list((work / "producer/scripts").rglob("build-script-build"))
    run_files = list((work / "producer/scripts").rglob("output"))
    image_files = [library] + list((work / "producer").rglob("producer.inputs"))
    image_files += [path for path in (work / "producer").rglob("ra-jit.log")
                    if "images" in path.parts]
    before = {str(path): (path.stat().st_mtime_ns, hashlib.sha256(path.read_bytes()).hexdigest())
              for path in script_files + run_files + image_files}
    assert len(script_files) == len(run_files) == (3 if args.nested else 2), (script_files, run_files)
    repeat = json.loads(run([args.blaze.resolve(), "proc-macro-build", root / "Cargo.toml",
                            selected, host, host, work / "producer"],
                           root, product, work / "repeat-build.log"))
    assert repeat == built
    after = {str(path): (path.stat().st_mtime_ns, hashlib.sha256(path.read_bytes()).hexdigest())
             for path in script_files + run_files + image_files}
    assert before == after, (before, after)
    assert list((work / "producer").rglob("complete.json")), "no admitted image"
    import subprocess
    # A damaged admitted output must not be overwritten while a reader could hold it.
    saved_image = library.read_bytes()
    library.write_bytes(b"not a library")
    bad = subprocess.run([str(args.blaze.resolve()), "proc-macro-build", str(root / "Cargo.toml"),
                          str(selected), host, host, str(work / "producer")],
                         cwd=root, env=product, capture_output=True, text=True)
    (work / "corrupt-image.log").write_text(bad.stdout + bad.stderr)
    assert bad.returncode != 0 and "completed owned producer image is corrupt" in bad.stderr
    assert library.read_bytes() == b"not a library", "must not replace a leased image"
    library.write_bytes(saved_image)
    # Same-length dependency bytes with the old mtime select another immutable image.
    dependency = root / "shared/src/lib.rs"
    original = dependency.read_text()
    stamp = dependency.stat()
    changed = original.replace("target-only", "target-edit")
    assert len(changed) == len(original) and changed != original
    dependency.write_text(changed)
    os.utime(dependency, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    newer = json.loads(run([args.blaze.resolve(), "proc-macro-build", root / "Cargo.toml",
                           selected, host, host, work / "producer"],
                          root, product, work / "changed-image.log"))
    assert Path(next(it["library"] for it in newer if "#source_derive@" in it["package"])) != library
    assert library.read_bytes() == saved_image
    compiler_logs = list((work / "producer").rglob("ra-jit.log"))
    assert compiler_logs, "the regression must inspect actual producer compiler logs"
    for log in compiler_logs:
        assert secret not in log.read_text(), f"request environment leaked in {log}"
    for plan in (work / "producer").rglob("rust-project.json"):
        assert secret not in plan.read_text(), f"request environment serialized in {plan}"
    rendered = run([args.blaze.resolve(), "proc-macro-plan", root / "Cargo.toml",
                    selected, host, host], root, product, work / "redacted-plan.log")
    assert secret not in rendered, "plan stdout must not expose the caller environment"
    # Deliberately swallow a compile probe failure as registry scripts do. The product
    # must refuse the whole producer build, not compile from that incomplete output.
    script = root / "derive/build.rs"
    script.write_text(script.read_text().replace(
        '    let generated =',
        '''    let status = std::process::Command::new(std::env::var_os("RUSTC").unwrap())
        .arg("--crate-type=lib").arg("probe.rs").status().unwrap();
    assert!(!status.success());
    let generated ='''))
    result = subprocess.run([str(args.blaze.resolve()), "proc-macro-build", str(root / "Cargo.toml"),
                             str(selected), host, host, str(work / "producer")],
                            cwd=root, env=product, capture_output=True, text=True)
    (work / "unsupported-probe.log").write_text(result.stdout + result.stderr)
    assert result.returncode != 0
    assert "could not compile a producer's compiler probe" in result.stderr, result.stderr
    calls = norustc.runs(str(audit_log))
    (work / "product-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
    assert not calls, calls
    print("default-feature producer scripts, renamed build edge, host metadata, cfg/env/include and owned version probe: equal oracle; zero product compilers")


if __name__ == "__main__":
    main()
