#!/usr/bin/env python3
"""aheadready.py: a function the daemon compiles ahead, while it reads the symbols rustc built,
does not hold up readiness once they are read.

usage: aheadready.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

A build the daemon was not told of ahead (`--bin` picks its targets) is loaded once cargo is
done, and the worker compiles the workspace's functions until the symbols are read. In bevy's
second warm-store run the names resolved first, and one of the functions it began was a
derived `release_state` of a read-only query struct, which takes the trait solver about two
minutes: the worker finished it before it said it was ready, 148 s after the build in place of
26. Here the symbol reader waits at a file barrier until a real prefetch job has been dequeued.
`RA_JIT_AHEAD_PAUSE_MS` has each job take ten minutes. After the barrier is released, the
daemon has to be ready within READY_SECS, and the builds after it, an edit's too, have to be the daemon's and
run as cargo's.

Python 3, stdlib only.
"""
import glob, os, re, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import Side

AHEAD_PAUSE_MS = 600_000
# Loading the workspace, the pause and what is left: far short of one function compiled ahead.
READY_SECS = 120
FILES = {
    "Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n',
    "src/main.rs": "".join(f"pub fn part_{i}() -> u32 {{\n    {i}\n}}\n\n" for i in range(8))
                   + 'fn main() {\n    println!("{}", part_3() + part_5());\n}\n',
}


def worker_log(home):
    """The log of the worker that compiled ahead, if one did."""
    for log in glob.glob(os.path.join(home, "*", "*", "log")):
        if re.search(r"^compiling ahead: paused$", open(log, errors="replace").read(), re.M):
            return log
    return None


def wait_for(process, condition, seconds, what):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if value := condition():
            return value
        if process.poll() is not None:
            raise RuntimeError(f"client ended {process.returncode} before {what}")
        time.sleep(0.1)
    raise TimeoutError(f"no {what} within {seconds}s")


def tails(home, lines=40):
    """The last lines of each of the daemon's logs, for a failure on a machine whose files are
    gone once it ends."""
    logs = sorted(glob.glob(os.path.join(home, "*.log")) + glob.glob(os.path.join(home, "*", "*", "log")))
    return "".join(f"\n--- {log}\n" + "".join(open(log, errors="replace").readlines()[-lines:]) for log in logs)


def build(side, step, home):
    _, ran = side.cargo("build", "--bin", "app")
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{(ran.stdout + ran.stderr)[-3000:]}{tails(home)}")
    return ran.stdout + ran.stderr


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "cargo-ra-jit")):
        raise SystemExit(f"no cargo-ra-jit beside {os.environ['RA_JIT_BIN']}")
    # Short, for the daemon's sockets under its home.
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-ar-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    for name in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL", "RA_JIT_SPARE"):
        env.pop(name, None)
    barrier = os.path.join(root, "barrier")
    os.makedirs(barrier)
    open(os.path.join(barrier, "symbols.hold"), "w").close()
    env.update(RA_JIT_SCRIPT_BARRIER_DIR=barrier, RA_JIT_AHEAD_PAUSE_MS=str(AHEAD_PAUSE_MS))
    side, reference = Side(ws, env, True), Side(ws, env, False)
    program = os.path.join(ws, "target", "debug", "app")
    wrong = []
    cold = None
    try:
        with open(os.path.join(root, "cold.stdout"), "w") as out, open(os.path.join(root, "cold.stderr"), "w") as err:
            cold = subprocess.Popen(side.argv("build", "--bin", "app"), cwd=ws, env=env,
                                    stdout=out, stderr=err, **cargo_ra_jit.OWN_GROUP)
            marks = wait_for(cold, lambda: glob.glob(os.path.join(barrier, "symbols.*.reached")), 300, "symbol barrier")
            worker_pid = open(marks[0]).read().strip()
            wait_for(cold, lambda: worker_log(home), 300, "a dequeued prefetch job")
            if glob.glob(os.path.join(barrier, "*.timeout")):
                raise SystemExit(f"the symbol barrier expired{tails(home)}")
            released = time.monotonic()
            open(marks[0].removesuffix(".reached") + ".go", "w").close()
            cold.wait(timeout=READY_SECS)
        if cold.returncode != 0:
            raise SystemExit(f"cold build ended {cold.returncode}{tails(home)}")
        remaining = READY_SECS - (time.monotonic() - released)
        if remaining <= 0:
            raise SystemExit(f"readiness exceeded {READY_SECS}s{tails(home)}")
        _, status = side.cargo("status", timeout=remaining)
        states = re.findall(rf"^    {re.escape(os.path.realpath(ws))}: (.*)$", status.stdout, re.M)
        took = time.monotonic() - released
        if states != ["ready"] or f"worker: pid {worker_pid}," not in status.stdout:
            raise SystemExit(f"the daemon did not get ready: {states}{tails(home)}")
        log = worker_log(home)
        if log is None:
            raise SystemExit(f"the worker compiled nothing ahead, which is what this tests{tails(home)}")
        said = open(log, errors="replace").read()
        ahead = re.search(r"^compiled ahead: (\d+) from (\d+) functions", said, re.M)
        if ahead is None or int(ahead[2]) == 0:
            raise SystemExit(f"no completed prefetch census{tails(home)}")
        if took > READY_SECS:
            wrong.append(f"ready {took:.1f} s after the build: a function compiled ahead held it up")
        elif ahead[1] != "0":
            wrong.append(f"{ahead[1]} functions compiled ahead, though each takes {AHEAD_PAUSE_MS} ms")
        print(f"{'ready':30} {'ok' if not wrong else 'WRONG':8} {took:.1f} s, {ahead[0]}")
        for step, old, new in [("served", None, None), ("an edit", "    5\n", "    50\n")]:
            if old:
                cargo_ra_jit.rewrite(os.path.join(ws, "src/main.rs"), old, new)
            out = build(side, step, home)
            ran = subprocess.run([program], capture_output=True, text=True, timeout=60)
            _, theirs = reference.cargo("run", "-q", "--bin", "app")
            _, status = side.cargo("status")
            same_worker = f"worker: pid {worker_pid}," in status.stdout
            right = same_worker and "[ra-jit]" in out and ran.returncode == theirs.returncode == 0 and ran.stdout == theirs.stdout
            if not same_worker:
                wrong.append(f"{step}: the worker was replaced")
            elif "[ra-jit]" not in out:
                wrong.append(f"{step}: the build was not the daemon's")
            elif ran.returncode != 0 or theirs.returncode != 0 or ran.stdout != theirs.stdout:
                wrong.append(f"{step}: the program printed {ran.stdout.strip()!r}, cargo's {theirs.stdout.strip()!r}")
            print(f"{step:30} {'ok' if right else 'WRONG':8} {ran.stdout.strip()}")
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
