#!/usr/bin/env python3
"""edges.py: a package whose build script runs after another package's (`links`) is run as often
as cargo runs it, whatever the edge: normal, renamed, optional and on or off, for the platform or
another, a dev-dependency (no edge) and a build-dependency (no edge, but cargo builds the script
again, unless it is for another platform), and is built by whom each variant says (`EXPECTED`):
blaze's own build, or, where cargo alone does more than for an edit, blaze's own build or a
handoff to cargo that says why. A build cargo did without saying so is wrong in every variant.

usage: edges.py [VARIANT...]    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

Each variant is a parent package whose script logs its runs beside `OUT_DIR` and generates a
constant, so that its rerun prints and writes the same and the build can stay the daemon's, and a
member `native` with `links = "probe"` whose script watches `data.txt` (and, in `normal-changing`,
prints it as its metadata). Two edits of
`native/data.txt` and one of the parent's `src/main.rs` are built with `cargo ra-jit`, and after
each the program and the number of runs of both scripts are those of cargo alone in `target-ref`.
A run is counted wherever the build ran the script: in cargo's `target/debug/build`, or in
blaze's own `target/ra-jit/build-scripts`, where the owned build runs it (`scriptrerun.runs`).

Python 3, stdlib only.
"""
import os, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import Side, how_built, wait_warm
from scriptrerun import build, differences, not_built_as, runs, tails

PARENT_BUILD = r"""use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo::rerun-if-changed=src/main.rs");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let log = out.join("../runs");
    let runs = fs::read_to_string(&log).ok().and_then(|it| it.parse::<u32>().ok()).unwrap_or(0) + 1;
    fs::write(&log, runs.to_string()).unwrap();
    let dep = env::var("DEP_PROBE_VAL").unwrap_or_else(|_| "none".into());
    fs::write(out.join("generated.rs"), format!("pub const DEP: &str = {dep:?};\n")).unwrap();
}
"""
NATIVE_BUILD = r"""use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo::rerun-if-changed=data.txt");
    println!("cargo::metadata=val=1");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let log = out.join("../runs");
    let runs = fs::read_to_string(&log).ok().and_then(|it| it.parse::<u32>().ok()).unwrap_or(0) + 1;
    fs::write(&log, runs.to_string()).unwrap();
}
"""
MAIN = 'include!(concat!(env!("OUT_DIR"), "/generated.rs"));\n\nfn number() -> u32 {\n    7\n}\n\n' \
       'fn main() {\n    println!("{} {}", DEP, number());\n}\n'

# name: (dependency section of the parent, workspace section)
VARIANTS = {
    # A renamed normal dependency: the edge is real (cargo_metadata's `name` is the package's).
    "renamed": ('[dependencies]\nnat = { package = "native", path = "native" }\n', '[workspace]\n'),
    # A plain normal dependency, as the PR's links row, plus a parent edit after.
    "normal": ('[dependencies]\nnative = { path = "native" }\n', '[workspace]\n'),
    # An optional dependency not enabled: cargo has no edge. native is still built (default-members).
    "optional-off": ('[dependencies]\nnative = { path = "native", optional = true }\n',
                     '[workspace]\ndefault-members = [".", "native"]\n'),
    # An optional dependency enabled by a default feature: the edge is real.
    "optional-on": ('[features]\ndefault = ["native"]\n\n[dependencies]\nnative = { path = "native", optional = true }\n',
                    '[workspace]\n'),
    # A dependency only on another platform: cargo has no edge on macOS.
    "target-off": ("[target.'cfg(windows)'.dependencies]\nnative = { path = \"native\" }\n",
                   '[workspace]\ndefault-members = [".", "native"]\n'),
    # A dependency on this platform: the edge is real.
    "target-on": ("[target.'cfg(unix)'.dependencies]\nnative = { path = \"native\" }\n", '[workspace]\n'),
    # A dev-dependency only: no edge for a build (cargo skips dev edges).
    "dev": ('[dev-dependencies]\nnative = { path = "native" }\n', '[workspace]\ndefault-members = [".", "native"]\n'),
    # A build-dependency only: no direct links edge, but the script is rebuilt through native's lib.
    "build-dep": ('[build-dependencies]\nnative = { path = "native" }\n', '[workspace]\n'),
    # The same, with the parent's run count compiled into the program.
    "build-dep-counter": ('[build-dependencies]\nnative = { path = "native" }\n', '[workspace]\n'),
    # A build-dependency only on another platform, of a member built anyway: no edge, and the
    # parent's script is not compiled with it, so the build stays the daemon's.
    "build-dep-off": ("[target.'cfg(windows)'.build-dependencies]\nnative = { path = \"native\" }\n",
                      '[workspace]\ndefault-members = [".", "native"]\n'),
    # A normal dependency whose metadata is what `data.txt` says: a dropped edge leaves the
    # parent's program with the old value.
    "normal-changing": ('[dependencies]\nnative = { path = "native" }\n', '[workspace]\n'),
}

# Who builds each edit (`data 2`, `data 3`, `main 8`): blaze ("daemon"), or, where a parent's
# script that printed or wrote something else or a build-dependency's that ran again has cargo
# alone do more than for an edit ("cargo"), blaze's own build or a handoff that says why
# (`scriptrerun.not_built_as`). A build cargo did without saying so is wrong in every variant.
EXPECTED = dict.fromkeys(VARIANTS, ("daemon",) * 3)
EXPECTED.update({
    "build-dep": ("cargo", "cargo", "daemon"),
    "build-dep-counter": ("cargo",) * 3,
    "normal-changing": ("cargo", "cargo", "daemon"),
})


def variant(bin_dir, name):
    deps, workspace = VARIANTS[name]
    files = {
        "Cargo.toml": f'[package]\nname = "parent"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n{deps}\n{workspace}',
        "build.rs": PARENT_BUILD.replace('format!("pub const DEP: &str = {dep:?};\\n")', 'format!("pub const DEP: &str = \\"{dep}{runs}\\";\\n")') if name.endswith("-counter") else PARENT_BUILD,
        "src/main.rs": MAIN,
        "native/Cargo.toml": '[package]\nname = "native"\nversion = "0.1.0"\nedition = "2021"\nlinks = "probe"\n',
        "native/src/lib.rs": "pub fn value() -> u32 { 1 }\n",
        "native/build.rs": NATIVE_BUILD.replace(
            'println!("cargo::metadata=val=1");',
            'println!("cargo::metadata=val={}", fs::read_to_string("data.txt").unwrap().trim());',
        ) if name == "normal-changing" else NATIVE_BUILD,
        "native/data.txt": "1\n",
    }
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-ed-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        with open(os.path.join(ws, path), "w") as out:
            out.write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    env["RA_JIT_CACHE"] = "off"
    for variable in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL"):
        env.pop(variable, None)
    side, reference = Side(ws, env, True), Side(ws, env, False)
    wrong, rows = [], []

    def compare(step, how, expected=None):
        ours = subprocess.run([os.path.join(ws, "target/debug/parent")], capture_output=True)
        theirs = subprocess.run([os.path.join(ws, "target-ref/debug/parent")], capture_output=True)
        problems = differences(ours, theirs, built)
        # A build cargo did without saying why is a dropped or spurious edge, whatever it built.
        built_as = expected and not_built_as(how, expected)
        if built_as:
            problems.append(built_as)
        counts = tuple(runs(os.path.join(ws, t), p) for p in ("parent", "native") for t in ("target", "target-ref"))
        if counts[0] != counts[1] or counts[2] != counts[3]:
            problems.append(f"runs parent {counts[0]}/{counts[1]}, native {counts[2]}/{counts[3]} (ours/cargo)")
        wrong.extend(f"{name}: {step}: {p}" for p in problems)
        rows.append(f"{name:13} {step:12} {'ok' if not problems else 'WRONG':6} "
                    f"{ours.stdout.decode().strip()!r}/{theirs.stdout.decode().strip()!r} "
                    f"parent {counts[0]}/{counts[1]} native {counts[2]}/{counts[3]} {how}")
        print(rows[-1], flush=True)

    try:
        build(side, f"{name}: cold", home)
        _, built = reference.cargo("build", "-q")
        if built.returncode:
            raise SystemExit(built.stderr)
        compare("cold", "")
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"{name}: not ready: {states}{tails(home)}")
        steps = [("data 2", "native/data.txt", "1\n", "2\n"), ("data 3", "native/data.txt", "2\n", "3\n"),
                 ("main 8", "src/main.rs", "    7\n", "    8\n")]
        for (step, path, old, new), expected in zip(steps, EXPECTED[name]):
            cargo_ra_jit.rewrite(os.path.join(ws, path), old, new)
            how = how_built(build(side, f"{name}: {step}", home))
            _, built = reference.cargo("build", "-q")
            compare(step, how, expected)
    finally:
        side.cargo("stop")
        if wrong:
            print(f"kept {root}{tails(home, 15)}")
        else:
            shutil.rmtree(root, ignore_errors=True)
    return wrong


def run(bin_dir, names=None):
    """What is wrong in each of `names` (all, by default), as strings."""
    wrong = []
    for name in names or VARIANTS:
        try:
            wrong += variant(bin_dir, name)
        except SystemExit as it:
            wrong.append(f"{name}: aborted: {str(it)[:2000]}")
            print(f"{name}: aborted: {str(it)[:2000]}", flush=True)
    return wrong


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    unknown = [it for it in sys.argv[1:] if it not in VARIANTS]
    if unknown:
        raise SystemExit(f"no variant {unknown}; they are {list(VARIANTS)}")
    wrong = run(bin_dir, sys.argv[1:])
    for why in wrong:
        print("  " + why)
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
