#!/usr/bin/env python3
"""latelisted.py: what a file an `include!` names declares is listed in its crate, though the
daemon listed the crate beside cargo's build before the file was loaded.

usage: latelisted.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

A library includes `exports.in` (not a `.rs` file) from GEN_DIR set by its owned build
script. An armed file barrier seeds the real cached listing before include hydration and
holds that worker. The harness requires zero exports and the include absent from loaded sources then, releases
normal hydration, and requires all EXPORTS declarations in the refreshed listing. A function
added afterward must be a small native splice by the same worker, with exactly Cargo's output.
The hook and barrier are inactive during ordinary compilation.

Python 3, stdlib only.
"""
import glob, os, re, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import Side, wait_warm
from aheadready import wait_for

EXPORTS = 64
FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "gen"]\nresolver = "2"\n',
    "gen/Cargo.toml": '[package]\nname = "gen"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n',
    "gen/build.rs": 'fn main() {\n'
                    '    let dir = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("generated");\n'
                    '    println!("cargo:rerun-if-changed=generated/exports.in");\n'
                    '    println!("cargo:rustc-env=GEN_DIR={}", dir.display());\n'
                    '}\n',
    "gen/generated/exports.in": "".join(f'#[no_mangle]\npub extern "C" fn late_export_{i}() -> u32 {{\n    {i}\n}}\n\n'
                                        for i in range(EXPORTS)),
    "gen/src/lib.rs": 'include!(concat!(env!("GEN_DIR"), "/exports.in"));\n\n'
                      'pub fn value() -> u32 {\n    1\n}\n',
    "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n'
                      '[dependencies]\ngen = { path = "../gen" }\n',
    "app/src/main.rs": 'fn main() {\n    println!("{} {}", gen::value(), gen::late_export_3());\n}\n',
}


def patches(home):
    """The `patch: N functions` lines of the workers' logs, in order."""
    found = []
    for log in sorted(glob.glob(os.path.join(home, "*", "*", "log"))):
        found += [int(n) for n in re.findall(r"^patch: (\d+) functions$", open(log, errors="replace").read(), re.M)]
    return found


def listings(home, phase):
    found = []
    for log in glob.glob(os.path.join(home, "*", "*", "log")):
        found += re.findall(
            rf"^script-lease: {phase} build=(\d+) pid=(\d+) at_us=\d+ crate=gen functions=(\d+) exports=(\d+) included_sources=(\d+)/(\d+)$",
            open(log, errors="replace").read(), re.M)
    return found


def tails(home, lines=40):
    """The last lines of each of the daemon's logs, for a failure on a machine whose files are
    gone once it ends."""
    logs = sorted(glob.glob(os.path.join(home, "*.log")) + glob.glob(os.path.join(home, "*", "*", "log")))
    return "".join(f"\n--- {log}\n" + "".join(open(log, errors="replace").readlines()[-lines:]) for log in logs)


def build(side, step, home):
    _, ran = side.cargo("build")
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{(ran.stdout + ran.stderr)[-3000:]}{tails(home)}")
    return ran.stdout + ran.stderr


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "cargo-ra-jit")):
        raise SystemExit(f"no cargo-ra-jit beside {os.environ['RA_JIT_BIN']}")
    # Short, for the daemon's sockets under its home.
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-ll-", dir="/tmp"))
    ws, home = (os.path.join(root, n) for n in ("ws", "home"))
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    for name in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL", "RA_JIT_SPARE"):
        env.pop(name, None)
    env["RA_JIT_RUSTC_BASE"] = "0"
    barrier = os.path.join(root, "barrier")
    os.makedirs(barrier)
    point = "listed-before-includes"
    open(os.path.join(barrier, point + ".hold"), "w").close()
    env["RA_JIT_SCRIPT_BARRIER_DIR"] = barrier
    side = Side(ws, env, True)
    reference = Side(ws, env, False)
    program = os.path.join(ws, "target", "debug", "app")
    wrong = []
    cold = None
    try:
        with open(os.path.join(root, "cold.stdout"), "w") as out, open(os.path.join(root, "cold.stderr"), "w") as err:
            cold = subprocess.Popen(side.argv("build"), cwd=ws, env=env, stdout=out, stderr=err,
                                    **cargo_ra_jit.OWN_GROUP)
            marks = wait_for(cold, lambda: glob.glob(os.path.join(barrier, point + ".*.reached")), 300, point)
            before = listings(home, point)
            if len(before) != 1 or before[0][2:] != ("1", "0", "0", "1"):
                raise SystemExit(f"the initial cached listing was not missing its include: {before}{tails(home)}")
            owner, pid, *_ = before[0]
            if open(marks[0]).read().strip() != pid:
                raise SystemExit("the barrier and cached listing belong to different workers")
            open(marks[0].removesuffix(".reached") + ".go", "w").close()
            cold.wait(timeout=300)
        if cold.returncode != 0 or glob.glob(os.path.join(barrier, "*.timeout")):
            raise SystemExit(f"the owned cold build failed or its barrier expired{tails(home)}")
        after = listings(home, "listed-after-includes")
        if after != [(owner, pid, str(EXPORTS + 1), str(EXPORTS), "1", "1")]:
            raise SystemExit(f"hydration did not refresh the cached listing: {after}{tails(home)}")
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"the daemon did not get ready: {states}{tails(home)}")
        print(f"late includes: 0 -> {EXPORTS} exports in worker {pid}", flush=True)
        log = next(path for path in glob.glob(os.path.join(home, "*", "*", "log"))
                   if f"pid={pid} " in open(path, errors="replace").read())
        log_offset = os.path.getsize(log)
        seen = len(patches(home))
        lib = os.path.join(ws, "gen/src/lib.rs")
        text = open(lib).read()
        open(lib, "w").write(text.replace("pub fn value() -> u32 {\n    1\n}",
                                          "pub fn value() -> u32 {\n    1 + added()\n}\n\nfn added() -> u32 {\n    40\n}"))
        said = build(side, "a function added", home)
        made = patches(home)[seen:]
        ran = subprocess.run([program], capture_output=True, text=True, timeout=60)
        _, theirs = reference.cargo("run", "-q", "-p", "app")
        if "[ra-jit]" not in said:
            wrong.append("the edit was not served by the daemon")
        elif not made:
            wrong.append("the edit was not patched")
        elif made[-1] >= EXPORTS:
            wrong.append(f"the patch is of {made[-1]} functions: the {EXPORTS} the included file declares were not listed")
        with open(log, "rb") as worker:
            worker.seek(log_offset)
            edited_log = worker.read().decode(errors="replace")
        _, status = side.cargo("status")
        if f"worker: pid {pid}," not in status.stdout:
            wrong.append("the edit replaced the worker")
        if not re.search(r"^link: .*\(spliced: ", edited_log, re.M):
            wrong.append("the edit did not splice its native executable")
        if ran.returncode != 0 or theirs.returncode != 0 or ran.stdout != theirs.stdout:
            wrong.append(f"the program printed {ran.stdout.strip()!r}, cargo's {theirs.stdout.strip()!r}")
        print(f"{'a function added':30} {'ok' if not wrong else 'WRONG':8} patch of {made[-1] if made else '-'} functions")
    finally:
        for mark in glob.glob(os.path.join(barrier, "*.reached")):
            open(mark.removesuffix(".reached") + ".go", "w").close()
        if cold is not None and cold.poll() is None:
            cargo_ra_jit.kill_all(cold)
            cold.wait(timeout=10)
        side.cargo("stop")
    for why in wrong:
        print(f"  {why}")
    if wrong:
        print(f"under {root}{tails(home)}")
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
