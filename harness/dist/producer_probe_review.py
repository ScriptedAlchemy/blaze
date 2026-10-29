#!/usr/bin/env python3
"""P8 review's 22 compiler probes: real oracle parity, including accepted probes.

The cases are unchanged from Claude Opus 5.5's premerge-pr15-62f2fb91
review. The harness owns its Python rustc oracle. A refusal is an explicit
compiler gap, not a parity pass or a successful producer build.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import norustc


PM = "extern crate proc_macro;\n"
CASES = [
    ("unstable-api-no-feature-pm", PM + "pub fn f() -> proc_macro::Span { proc_macro::Span::def_site() }\n", [], "2021"),
    ("unstable-api-no-feature-core", "pub fn f() -> bool { core::intrinsics::likely(true) }\n", [], "2021"),
    ("unstable-api-str-as-str", "pub fn f(s: &str) -> &str { s.as_str() }\n", [], "2021"),
    ("stable-api-io-other", "pub fn f() -> std::io::Error { std::io::Error::other(\"x\") }\n", [], "2021"),
    ("stable-api-is-none-or", "pub fn f(o: Option<u8>) -> bool { o.is_none_or(|x| x > 1) }\n", [], "2021"),
    ("trait-bound-unsatisfied", "fn f<T: Copy>(_: T) {}\npub fn g() { f(String::new()) }\n", [], "2021"),
    ("use-after-move", "pub fn g() { let s = String::new(); let t = s; drop(t); drop(s); }\n", [], "2021"),
    ("private-fn", "mod m { fn f() {} }\npub fn g() { m::f() }\n", [], "2021"),
    ("missing-trait-item", "pub struct S;\nimpl Iterator for S {}\n", [], "2021"),
    ("async-fn-2015", "pub async fn f() {}\n", [], None),
    ("let-else-2021", "pub fn f(o: Option<u8>) -> u8 { let Some(x) = o else { return 0 }; x }\n", [], "2021"),
    ("wrong-arg-count", "fn f(_: u8) {}\npub fn g() { f(1, 2) }\n", [], "2021"),
    ("deny-warnings-capped", "#![deny(warnings)]\nfn unused() {}\npub fn g() {}\n", [], "2021"),
    ("rustflags-Dwarnings", "pub fn g() {}\n", ["-Dwarnings"], "2021"),
    ("rustflags-cfg", "#[cfg(not(foo))] compile_error!(\"no foo\");\npub fn g() {}\n", ["--cfg", "foo"], "2021"),
    ("rustflags-check-cfg", "pub fn g() {}\n", ["--check-cfg=cfg(foo)"], "2021"),
    ("stdin-valid", None, [], "2021"),
    ("unresolved-macro", "pub fn g() { not_a_macro!(); }\n", [], "2021"),
    ("unresolved-type", "pub fn g(_: NoSuchType) {}\n", [], "2021"),
    ("unresolved-path-expr", "pub fn g() { let _ = std::nope::X; }\n", [], "2021"),
    ("const-eval-error", "pub const X: u8 = 255 + 1;\n", [], "2021"),
    ("dyn-not-object-safe", "pub fn g(_: &dyn Clone) {}\n", [], "2021"),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--blaze", required=True, type=Path)
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    real = Path(norustc.real_toolchain())
    audit_log = work / "product-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(audit_log), block=True))
    env = dict(os.environ)
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_BOOTSTRAP",
                 "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "SDKROOT"):
        env.pop(name, None)
    marker = work / "unsupported"
    product = dict(env, RUSTUP_TOOLCHAIN=str(audit),
        PATH=str(audit / "bin") + os.pathsep + env["PATH"],
        RUSTC=str(audit / "bin/rustc"), RUSTDOC=str(audit / "bin/rustdoc"),
        CARGO=str(audit / "bin/cargo"),
        RA_JIT_ANSWERS="own", BLAZE_ANSWERS="own", RA_JIT_STD_FROM="off",
        RA_JIT_PROC_MACRO_SERVER="0", RA_JIT_HOME=str(work / "home"),
        RA_JIT_CACHE=str(work / "store"), BLAZE_PRODUCER_PROBE_FAILURE=str(marker))
    rows = []
    for name, text, flags, edition in CASES:
        out = work / (name + "-out")
        out.mkdir()
        argv = ["--crate-name", "probe", "--crate-type", "lib", "--cap-lints", "allow",
                "--emit", "dep-info,metadata", "--out-dir", str(out)]
        if edition:
            argv += ["--edition=" + edition]
        stdin = None
        if text is None:
            argv += ["-"]
            stdin = "pub fn g() -> u32 { 1 }\n"
        else:
            source = work / (name + ".rs")
            source.write_text(text)
            argv += [str(source)]
        argv += flags
        ours = subprocess.run([str(args.blaze.resolve()), "producer-rustc", *argv],
            env=product, cwd=work, capture_output=True, text=True, input=stdin)
        oracle = subprocess.run([str(real / "bin/rustc"), *argv],
            env=dict(env, RUSTUP_TOOLCHAIN=str(real)), cwd=work,
            capture_output=True, text=True, input=stdin)
        refused = marker.exists()
        reason = marker.read_text() if refused else None
        if refused:
            marker.unlink()
        parity = (ours.returncode == 0) == (oracle.returncode == 0)
        safe = (ours.returncode != 0 and refused and bool(reason)) or parity
        rows.append(dict(case=name, ours=ours.returncode, oracle=oracle.returncode,
            verdict="undecided-refused" if refused else ("equal" if parity else "DIFFERENT"),
            safe=safe, reason=reason))
        (work / (name + "-owned.log")).write_text(ours.stdout + ours.stderr)
        (work / (name + "-oracle.log")).write_text(oracle.stdout + oracle.stderr)
    calls = norustc.runs(str(audit_log))
    (work / "rows.json").write_text(json.dumps(dict(rows=rows, audit=calls), indent=2))
    assert len(rows) == 22 and all(row["safe"] for row in rows), rows
    assert all(row["verdict"] == "equal" for row in rows), "refusals do not establish acceptance parity"
    assert not calls, calls
    print(f"22 review probes: {sum(row['verdict'] == 'equal' for row in rows)} parity, "
          f"{sum(row['verdict'] == 'undecided-refused' for row in rows)} undecided refusals; "
          f"{sum(row['ours'] == 0 for row in rows)} accepted probes, zero product calls")
    swallowed_probe_build(work / "e2e", args.blaze.resolve(), real, env, product, audit_log)


def swallowed_probe_build(work, blaze, real, env, product, audit_log):
    """The review's four API probes must not publish wrong cfgs when errors are swallowed."""
    def write(path, text):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def fixture(root):
        write(root / "Cargo.toml", '[workspace]\nmembers=["app","derive"]\nresolver="2"\n')
        write(root / "derive/Cargo.toml",
              '[package]\nname="probe_derive"\nversion="0.1.0"\nedition="2021"\n'
              '[lib]\nproc-macro=true\n')
        write(root / "derive/build.rs", r'''use std::process::Command;
fn probe(name: &str, code: &str) -> bool {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let file = out.join(format!("{name}.rs"));
    std::fs::write(&file, code).unwrap();
    Command::new(std::env::var_os("RUSTC").unwrap()).args(["--edition=2021",
        "--crate-name", name, "--crate-type", "lib", "--cap-lints", "allow",
        "--emit", "dep-info,metadata", "--out-dir"]).arg(&out).arg(&file)
        .status().map_or(false, |s| s.success())
}
fn main() {
    for (cfg, code) in [
        ("has_api", "pub fn f() -> std::io::Error { std::io::Error::other(\"x\") }"),
        ("has_nope", "pub fn f() { let _ = std::nope::X; }"),
        ("has_nope_type", "pub fn f(_: std::nope::Y) {}"),
        ("has_def_site", "extern crate proc_macro; pub fn f() -> proc_macro::Span { proc_macro::Span::def_site() }"),
    ] {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
        if probe(cfg, code) { println!("cargo::rustc-cfg={cfg}"); }
    }
}
''')
        write(root / "derive/src/lib.rs", r'''extern crate proc_macro;
#[proc_macro]
pub fn label(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let s = format!("api={} nope={} nope_type={} def_site={}",
        cfg!(has_api), cfg!(has_nope), cfg!(has_nope_type), cfg!(has_def_site));
    format!("{s:?}").parse().unwrap()
}
''')
        write(root / "app/Cargo.toml",
              '[package]\nname="app"\nversion="0.1.0"\nedition="2021"\n'
              '[dependencies]\nprobe_derive={path="../derive"}\n')
        write(root / "app/src/main.rs", 'fn main() { println!("{}", probe_derive::label!()); }\n')
        return root

    ours, oracle = fixture(work / "ours"), fixture(work / "oracle")
    for root in (ours, oracle):
        subprocess.run([real / "bin/cargo", "generate-lockfile", "--offline"],
                       cwd=root, env=env, check=True, capture_output=True)
    selection = work / "selection.json"
    selection.write_text(json.dumps(dict(profile="dev", features=[], no_default_features=False,
        all_features=False, packages=["app"], workspace=False, target=None,
        dev_deps=False, config=[])))
    host = subprocess.run([real / "bin/rustc", "-vV"], env=env, capture_output=True,
                          text=True, check=True).stdout
    host = next(line.removeprefix("host: ") for line in host.splitlines() if line.startswith("host: "))
    run = subprocess.run([blaze, "proc-macro-build", ours / "Cargo.toml", selection,
        host, host, work / "producer"], cwd=ours, env=product, capture_output=True, text=True)
    reference = subprocess.run([real / "bin/cargo", "run", "--offline", "-q", "-p", "app"],
        cwd=oracle, env=dict(env, CARGO_TARGET_DIR=str(work / "oracle-target")),
        capture_output=True, text=True)
    (work / "owned.log").write_text(run.stdout + run.stderr)
    (work / "oracle.log").write_text(reference.stdout + reference.stderr)
    assert reference.returncode == 0, reference.stderr
    assert reference.stdout.strip() == "api=true nope=false nope_type=false def_site=false"
    assert run.returncode == 0, run.stderr
    images = json.loads(run.stdout)
    assert len(images) == 1 and Path(images[0]["library"]).is_file(), images
    def cfgs(top):
        outputs = [path for path in top.rglob("output") if path.parent.name.startswith("probe_derive-")]
        assert len(outputs) == 1, outputs
        return sorted(line for line in outputs[0].read_text().splitlines()
                      if line.startswith(("cargo::rustc-cfg=", "cargo:rustc-cfg=")))
    assert cfgs(work / "producer/scripts") == cfgs(work / "oracle-target/debug/build") == ["cargo::rustc-cfg=has_api"]
    assert not norustc.runs(str(audit_log))
    print("swallowed probe build publishes only Cargo's proven cfgs; zero product calls; expansion is a separate server gate")


if __name__ == "__main__":
    main()
