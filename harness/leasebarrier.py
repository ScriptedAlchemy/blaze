#!/usr/bin/env python3
"""leasebarrier.py: two builds of one script's output, held apart by the build's leases, through
blaze's daemon and workers (docs/reports/SCRIPTEDITS.md, "What a build holds of a script").

usage: leasebarrier.py [--work DIR] [--json OUT] [--source-sha SHA] [scenario ...]
scenarios: same-worker, cross-worker, failure, archive-control     (all, by default)

RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it. Linux, where the whole
build works (`RA_JIT_WHOLE_BUILD=1`; #255 is macOS's). Cargo is the oracle only: it builds the
same project in a directory of its own, for the output each value of `GEN` must give.

The project is a package of two programs whose build script writes a file the programs
`include!`, a `cfg`, a variable for rustc and a static library it compiles (`cc`, `ar`) that the
programs link, all of one value, `GEN`; and names `GEN` in `rerun-if-env-changed`. A build with
another `GEN` has to run the script again, over the output of the first.

same-worker   One daemon, one worker, two clients at once. The worker is held (the file barrier
              of `scripts/barrier.rs`) after build A's scripts are judged and before its compile
              and link. B asks with another `GEN` and has to be put off, not to run the script.
              Then A is let go, answers its client, and the worker is held again as the daemon
              says A is over: B still has run nothing. Both programs of A are run and have to be
              A's, link metadata (the static library) and all. A is retired; B runs and has to
              build B's.
cross-worker  Two daemons (two `RA_JIT_HOME`) over one project and one target directory: the
              same, but B is another worker's, waits for the lease and is let in when A retires.
failure       One worker, one build with two programs whose script fails: the same failure from
              each request and one run of the script; then a build that works; then the failure
              again as a build of its own.

archive-control  Not about leases, no concurrency: GEN=a then GEN=c with a static archive of one
              name whose contents change. What blaze prints differing from cargo's is recorded as a
              gap, not a failure of the lease scenarios.

Every scenario records what it ran under `--work`: the worker's pid and log, the build numbers,
the barrier's files, the source and binary hashes, the order of the log's `script-lease:` lines
against the clients' ends, and the programs' output. A step that cannot be taken (a build that
does not start, a barrier not reached in time) fails the scenario by name; nothing is retried
and no scenario falls back to anything else.

Python 3, stdlib only.
"""
import argparse, glob, hashlib, json, os, re, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from cargo_ra_jit import RA_JIT, base_env

# Every wait is bounded; a wait that ends is a failure, named.
WAIT = float(os.environ.get("LEASEBARRIER_WAIT", "600"))
# How long the held build's output is watched for a change that must not come.
WATCH = float(os.environ.get("LEASEBARRIER_WATCH", "4"))

BUILD_RS = r'''use std::{env, fs, io::Write, path::PathBuf, process::{exit, Command}};

fn main() {
    let gen_ = env::var("GEN").unwrap_or_else(|_| "none".to_owned());
    println!("cargo:rerun-if-env-changed=GEN");
    println!("cargo:rerun-if-env-changed=FAIL");
    println!("cargo:rerun-if-env-changed=GEN_ARCHIVE");
    let mut log = fs::OpenOptions::new().create(true).append(true).open("@LOG@").unwrap();
    writeln!(log, "{gen_}").unwrap();
    if env::var("FAIL").as_deref() == Ok("1") {
        eprintln!("script failed for {gen_}");
        exit(3);
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let value = gen_.bytes().next().unwrap_or(b'?') as i32;
    // The library is named for the value, so that a link line of another build's cannot find
    // the symbol; with GEN_ARCHIVE=same it has one name and only its contents change.
    let same = env::var("GEN_ARCHIVE").as_deref() == Ok("same");
    let (lib, symbol) = if same { ("gen".to_owned(), "gen_value".to_owned()) } else { (format!("gen_{gen_}"), format!("gen_value_{gen_}")) };
    fs::write(out.join("gen.rs"), format!("pub const GEN: &str = {gen_:?};\nextern \"C\" {{ fn {symbol}() -> i32; }}\npub fn native() -> i32 {{ unsafe {{ {symbol}() }} }}\n")).unwrap();
    fs::write(out.join("gen.c"), format!("int {symbol}(void) {{ return {value}; }}\n")).unwrap();
    let status = Command::new("cc").args(["-c", "-o"]).arg(out.join("gen.o")).arg(out.join("gen.c")).status().unwrap();
    assert!(status.success());
    let _ = fs::remove_file(out.join(format!("lib{lib}.a")));
    let status = Command::new("ar").arg("rcs").arg(out.join(format!("lib{lib}.a"))).arg(out.join("gen.o")).status().unwrap();
    assert!(status.success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static={lib}");
    println!("cargo:rustc-env=GEN_VALUE={gen_}");
    println!("cargo:rustc-cfg=gen_{gen_}");
    println!("cargo:rustc-check-cfg=cfg(gen_a,gen_b,gen_c,gen_bad,gen_none)");
}
'''

PROGRAM = r'''include!(concat!(env!("OUT_DIR"), "/gen.rs"));

fn main() {
    let cfgv = if cfg!(gen_a) { "a" } else if cfg!(gen_b) { "b" } else if cfg!(gen_c) { "c" } else { "other" };
    println!("@NAME@ include={GEN} env={} cfg={cfgv} native={}", env!("GEN_VALUE"), native());
}
'''

MANIFEST = '''[package]
name = "gen"
version = "0.1.0"
edition = "2021"
build = "build.rs"

[[bin]]
name = "gen-one"
path = "src/one.rs"

[[bin]]
name = "gen-two"
path = "src/two.rs"
'''

PROGRAMS = ("gen-one", "gen-two")


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def tree_hash(directory):
    """The names and bytes of the files of `directory`, as one hash: what a script wrote."""
    h = hashlib.sha256()
    for path in sorted(glob.glob(os.path.join(directory, "**"), recursive=True)):
        if os.path.isfile(path):
            h.update(os.path.relpath(path, directory).encode())
            with open(path, "rb") as f:
                h.update(f.read())
    return h.hexdigest()


class Scenario:
    def __init__(self, name, work):
        self.name, self.dir = name, os.path.join(work, name)
        shutil.rmtree(self.dir, ignore_errors=True)
        os.makedirs(self.dir)
        self.ws = os.path.join(self.dir, "ws")
        self.runs_log = os.path.join(self.dir, "runs.log")
        self.failures, self.notes, self.timeline, self.gaps = [], {}, [], []
        self.daemons = []

    # -- the project and the oracle
    def project(self, ws=None):
        ws = ws or self.ws
        os.makedirs(os.path.join(ws, "src"), exist_ok=True)
        for name, text in {"Cargo.toml": MANIFEST, "build.rs": BUILD_RS.replace("@LOG@", self.runs_log),
                           "src/one.rs": PROGRAM.replace("@NAME@", "gen-one"),
                           "src/two.rs": PROGRAM.replace("@NAME@", "gen-two")}.items():
            with open(os.path.join(ws, name), "w") as f:
                f.write(text)
        open(self.runs_log, "w").close()
        subprocess.run(["cargo", "generate-lockfile"], cwd=ws, env=self.env("x"), capture_output=True, check=True)

    def oracle(self, gen_, extra=None):
        """What cargo's build gives for `GEN=gen_`: the output each program must have."""
        ws = os.path.join(self.dir, "oracle-ws")
        shutil.rmtree(ws, ignore_errors=True)
        saved, self.runs_log = self.runs_log, os.path.join(self.dir, "oracle-runs.log")
        self.project(ws)
        self.runs_log = saved
        env = dict(self.env("oracle"), GEN=gen_, CARGO_TARGET_DIR=os.path.join(self.dir, "oracle-target"), **(extra or {}))
        env.pop("RA_JIT_WHOLE_BUILD", None)
        ran = subprocess.run(["cargo", "build"], cwd=ws, env=env, capture_output=True, text=True, timeout=WAIT)
        if ran.returncode != 0:
            self.fail(f"the oracle (cargo, GEN={gen_}) did not build: {ran.stderr[-400:]}")
            return None
        out = {}
        for name in PROGRAMS:
            out[name] = subprocess.run([os.path.join(self.dir, "oracle-target", "debug", name)],
                                       capture_output=True, text=True).stdout
        return out

    # -- daemons
    def env(self, home, extra=None):
        env = base_env(os.path.join(self.dir, "home-" + home))
        env["RA_JIT_WHOLE_BUILD"] = "1"
        env.pop("CARGO_TARGET_DIR", None)
        for name in ("RA_JIT_MEMORY_MAX", "RA_JIT_OWN_BASE_MEMORY", "RA_JIT_SPARE"):
            if name in os.environ:
                env[name] = os.environ[name]
        env["RA_JIT_SCRIPT_BARRIER_DIR"] = os.path.join(self.dir, "barrier-" + home)
        os.makedirs(env["RA_JIT_SCRIPT_BARRIER_DIR"], exist_ok=True)
        env.update(extra or {})
        return env

    def client(self, home, **env):
        """A `cargo ra-jit build` that starts now: its process, and when it started."""
        p = subprocess.Popen(["cargo", "ra-jit", "build"], cwd=self.ws, env=self.env(home, env),
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace",
                             stdin=subprocess.DEVNULL)
        p.started = time.time_ns() // 1000
        p.home = home
        if home not in self.daemons:
            self.daemons.append(home)
        return p

    def finish(self, p, what, within=None):
        """The end of client `p`, or a failure by name when it does not end."""
        try:
            out, err = p.communicate(timeout=within or WAIT)
        except subprocess.TimeoutExpired:
            p.kill()
            out, err = p.communicate()
            self.fail(f"{what}: the client did not end in {within or WAIT:.0f}s")
        p.ended = time.time_ns() // 1000
        p.out, p.err = out, err
        for suffix, text in (("out", out), ("err", err)):
            with open(os.path.join(self.dir, f"client-{what}.{suffix}"), "w") as f:
                f.write(text)
        self.timeline.append((p.ended, f"client {what} ended rc={p.returncode}"))
        if "building with cargo" in err:
            self.fail(f"{what}: the build fell back to cargo")
        return p

    def status(self, home):
        ran = subprocess.run(["cargo", "ra-jit", "status"], cwd=self.ws, env=self.env(home),
                             capture_output=True, text=True, timeout=60)
        text = ran.stdout + ran.stderr
        daemon = re.search(r"daemon: pid (\d+)", text)
        worker = re.search(r"worker: pid (\d+), .*?log (\S+)", text)
        return {"daemon_pid": int(daemon[1]) if daemon else None,
                "worker_pid": int(worker[1]) if worker else None,
                "worker_log": worker[2] if worker else None, "text": text}

    def log_of(self, home):
        st = self.notes.get("status-" + home) or self.status(home)
        if st["worker_log"]:
            self.notes["status-" + home] = st
            with open(st["worker_log"], errors="replace") as f:
                return f.read()
        return ""

    def events(self, home):
        """The `script-lease:` lines of a worker's log: (what, build, at_us, rest)."""
        found = []
        for line in self.log_of(home).splitlines():
            m = re.match(r"script-lease: (\S+) build=(\d+) pid=(\d+) at_us=(\d+) ?(.*)", line)
            if m:
                found.append((m[1], int(m[2]), int(m[3]), int(m[4]), m[5]))
        return found

    def wait_event(self, home, what, build=None, within=None):
        deadline = time.time() + (within or WAIT)
        while time.time() < deadline:
            for e in self.events(home):
                if e[0] == what and (build is None or e[1] == build):
                    return e
            time.sleep(0.25)
        self.fail(f"no {what} in {home}'s worker log in {within or WAIT:.0f}s")
        return None

    # -- the barrier
    def barrier(self, home):
        return os.path.join(self.dir, "barrier-" + home)

    def arm(self, home, point):
        os.makedirs(self.barrier(home), exist_ok=True)
        open(os.path.join(self.barrier(home), point + ".hold"), "w").close()

    def disarm(self, home, point):
        try:
            os.remove(os.path.join(self.barrier(home), point + ".hold"))
        except FileNotFoundError:
            pass

    def reached(self, home, point, p=None, within=None):
        """The build number of the first build held at `point`, once it is."""
        deadline = time.time() + (within or WAIT)
        while time.time() < deadline:
            for path in sorted(glob.glob(os.path.join(self.barrier(home), point + ".*.reached"))):
                return int(os.path.basename(path).split(".")[1])
            if p is not None and p.poll() is not None:
                self.fail(f"the client of {home} ended ({p.returncode}) before {point} was reached")
                return None
            time.sleep(0.1)
        self.fail(f"{point} was not reached in {home} in {within or WAIT:.0f}s")
        return None

    def release(self, home, point, build):
        open(os.path.join(self.barrier(home), f"{point}.{build}.go"), "w").close()
        self.timeline.append((time.time_ns() // 1000, f"released {point} build {build} ({home})"))

    def timeouts(self):
        for path in glob.glob(os.path.join(self.dir, "barrier-*", "*.timeout")):
            self.fail(f"a worker's barrier timed out: {os.path.basename(path)}")

    # -- what the output is
    def run_dir(self):
        found = glob.glob(os.path.join(self.ws, "target", "ra-jit", "build-scripts", "debug", "build", "gen-*", "out"))
        return found[0] if found else None

    def runs(self):
        return open(self.runs_log).read().split()

    def program(self, name, exe=None):
        exe = exe or os.path.join(self.ws, "target", "debug", name)
        if not os.path.exists(exe):
            return None
        return subprocess.run([exe], capture_output=True, text=True, timeout=60).stdout

    def keep(self, tag):
        """Copies of both programs, as they are on disk now."""
        kept = {}
        for name in PROGRAMS:
            src = os.path.join(self.ws, "target", "debug", name)
            if os.path.exists(src):
                kept[name] = os.path.join(self.dir, f"{tag}-{name}")
                shutil.copy2(src, kept[name])
        return kept

    def check_programs(self, tag, kept, expected, gen_):
        for name in PROGRAMS:
            got = self.program(name, kept.get(name))
            if expected is not None and got != expected[name]:
                self.fail(f"{tag}: {name} printed {got!r}, the oracle's {expected[name]!r}")
            if got is None or f"include={gen_} env={gen_} cfg={gen_}" not in got:
                self.fail(f"{tag}: {name} is not GEN={gen_}'s build: {got!r}")
            self.notes.setdefault("programs", {})[f"{tag}/{name}"] = got

    def unchanged(self, before, what, p=None):
        """For WATCH seconds the output the script wrote is what it was, and no script ran."""
        runs, end = len(self.runs()), time.time() + WATCH
        while time.time() < end:
            now = tree_hash(self.run_dir()) if self.run_dir() else None
            if now != before:
                self.fail(f"{what}: the output of the script changed while it was held")
                return
            if len(self.runs()) != runs:
                self.fail(f"{what}: a script ran while the output was held: {self.runs()}")
                return
            if p is not None and p.poll() is not None:
                self.fail(f"{what}: the held client ended ({p.returncode})")
                return
            time.sleep(0.1)

    # -- bookkeeping
    def fail(self, why):
        print(f"  FAIL {self.name}: {why}", flush=True)
        self.failures.append(why)

    def evidence(self, source_sha):
        """Saved before the daemons go: pids, logs, hashes."""
        ev = {"source_sha": source_sha, "binary": RA_JIT, "binary_sha256": sha256(RA_JIT),
              "timeline": sorted(self.timeline)}
        for home in self.daemons:
            st = self.notes.get("status-" + home) or self.status(home)
            ev["daemon-" + home] = {k: st[k] for k in ("daemon_pid", "worker_pid", "worker_log")}
            if st["worker_log"] and os.path.exists(st["worker_log"]):
                shutil.copy2(st["worker_log"], os.path.join(self.dir, f"worker-{home}.log"))
            ev["events-" + home] = self.events(home)
            ev["barrier-" + home] = sorted(os.listdir(self.barrier(home))) if os.path.isdir(self.barrier(home)) else []
        ev["gaps"] = self.gaps
        ev["runs"] = self.runs() if os.path.exists(self.runs_log) else []
        ev["notes"] = {k: v for k, v in self.notes.items() if not k.startswith("status-")}
        return ev

    def stop(self):
        for home in self.daemons:
            subprocess.run(["cargo", "ra-jit", "stop"], cwd=self.ws, env=self.env(home), capture_output=True,
                           timeout=120)


def order(sc, home, steps):
    """The events of `home`'s worker log and the clients' ends, in the order the steps say:
    each (label, at_us) is later than the one before it."""
    last = None
    for label, at in steps:
        if at is None:
            sc.fail(f"order: {label} never happened")
            return
        if last is not None and at < last[1]:
            sc.fail(f"order: {label} ({at}) came before {last[0]} ({last[1]})")
        last = (label, at)
    sc.notes.setdefault("order", []).append([(l, a) for l, a in steps])


def first(events, what, build):
    return next((e[3] for e in events if e[0] == what and e[1] == build), None)


def same_or_cross(sc, cross, source_sha):
    a_home, b_home = "a", ("b" if cross else "a")
    exp_a, exp_b = sc.oracle("a"), sc.oracle("b")
    sc.project()
    sc.arm(a_home, "consume")
    A = sc.client(a_home, GEN="a")
    ba = sc.reached(a_home, "consume", A)
    if ba is None:
        sc.finish(A, "A", 5)
        return
    sc.disarm(a_home, "consume")
    sc.arm(a_home, "retire")
    sc.timeline.append((time.time_ns() // 1000, f"A (build {ba}) held at consume ({a_home})"))
    held = tree_hash(sc.run_dir())
    sc.notes["a-output-hash"] = held
    runs = sc.runs()
    if runs != ["a"]:
        sc.fail(f"A's script did not run once before the barrier: {runs}")
    # B asks, with another value of the variable the script watches.
    B = sc.client(b_home, GEN="b")
    if cross:
        waited = sc.wait_event(b_home, "waits", within=WAIT)
        sc.notes["b-waits-event"] = waited
        bb = None
    else:
        put = sc.wait_event(a_home, "put-off")
        sc.notes["b-put-off-event"] = put
        bb = put[1] if put else None
        if bb == ba:
            sc.fail("the build that was put off is A's own")
    sc.unchanged(held, "B asking while A is held", B)
    # A resumes: its compile and link read the output its script wrote.
    sc.release(a_home, "consume", ba)
    sc.finish(A, "A")
    if A.returncode != 0:
        sc.fail(f"A's build exited {A.returncode}: {A.err[-600:]}")
    if "[ra-jit]" not in A.err:
        sc.fail("A's build was not the daemon's whole build")
    kept_a = sc.keep("a")
    retire_at = sc.reached(a_home, "retire", B)
    sc.check_programs("A", kept_a, exp_a, "a")
    # The client has its answer and A is not retired: B has still run nothing.
    sc.unchanged(held, "B asking after A's client was answered, before A is retired", B)
    if B.poll() is not None:
        sc.fail("B's client ended before A was retired")
    sc.release(a_home, "retire", ba)
    sc.disarm(a_home, "retire")
    sc.finish(B, "B")
    if B.returncode != 0:
        sc.fail(f"B's build exited {B.returncode}: {B.err[-600:]}")
    kept_b = sc.keep("b")
    sc.check_programs("B", kept_b, exp_b, "b")
    # A's copy is still A's: nothing B did reached it.
    sc.check_programs("A-after-B", kept_a, exp_a, "a")
    if sc.runs() != ["a", "b"]:
        sc.fail(f"the script ran {sc.runs()}, once for each build, 'a' then 'b'")
    sc.timeouts()
    ev_a = sc.events(a_home)
    ev_b = sc.events(b_home)
    if not cross:
        order(sc, a_home, [
            ("A held at consume", first(ev_a, "barrier-reached", ba)),
            ("B put off", first(ev_a, "put-off", bb) if bb else None),
            ("A released", first(ev_a, "barrier-released", ba)),
            ("A's client ended", A.ended),
            ("retire reached", next((e[3] for e in ev_a if e[0] == "barrier-reached" and e[1] == ba and "retire" in e[4]), None)),
            ("A retired", first(ev_a, "retired", ba)),
            ("B ran the script", next((e[3] for e in ev_a if e[0] == "runs" and e[1] == bb), None)),
        ])
    else:
        order(sc, a_home, [
            ("A held at consume", first(ev_a, "barrier-reached", ba)),
            ("A released", first(ev_a, "barrier-released", ba)),
            ("A's client ended", A.ended),
            ("A retired", first(ev_a, "retired", ba)),
        ])
        ran_b = next((e[3] for e in ev_b if e[0] == "runs"), None)
        waits = next((e[3] for e in ev_b if e[0] == "waits"), None)
        order(sc, b_home, [("B waits", waits), ("A retired", first(ev_a, "retired", ba)), ("B ran the script", ran_b)])
    sc.notes["builds"] = {"A": ba, "B": bb}
    del retire_at


def failure(sc, source_sha):
    sc.project()
    ok = sc.client("a", GEN="a")
    sc.finish(ok, "warm")
    if ok.returncode != 0:
        sc.fail(f"the build that was to warm the worker exited {ok.returncode}: {ok.err[-600:]}")
        return
    messages = []
    for tag, env in (("F1", {"GEN": "bad", "FAIL": "1"}), ("G", {"GEN": "c"}), ("F2", {"GEN": "bad", "FAIL": "1"})):
        p = sc.client("a", **env)
        sc.finish(p, tag)
        sc.notes[f"{tag}-rc"] = p.returncode
        if tag == "G":
            if p.returncode != 0:
                sc.fail(f"G, after a failure, exited {p.returncode}: {p.err[-600:]}")
            sc.check_programs("G", {}, None, "c")
        else:
            if p.returncode == 0 or "failed to run custom build command" not in p.err:
                sc.fail(f"{tag} did not fail with cargo's message for a script: rc {p.returncode}: {p.err[-600:]}")
            messages.append(re.findall(r"script failed for bad", p.err))
            sc.notes[f"{tag}-said"] = [l for l in p.err.splitlines() if "failed" in l][:4]
    ev = sc.events("a")
    per_build = {}
    for e in ev:
        if e[0] == "runs":
            per_build[e[1]] = per_build.get(e[1], 0) + 1
    sc.notes["runs-per-build"] = per_build
    if any(n != 1 for n in per_build.values()):
        sc.fail(f"a script ran more than once in a build: {per_build}")
    counted = sc.runs().count("bad")
    # Two programs ask the worker twice in each build, and the script runs once in each.
    if counted != 2:
        sc.fail(f"the failing script ran {counted} times for 2 builds of 2 programs each: {sc.runs()}")
    if sc.notes.get("F1-said") != sc.notes.get("F2-said"):
        sc.fail(f"the failure said different things: {sc.notes.get('F1-said')} / {sc.notes.get('F2-said')}")
    sc.timeouts()


def archive_control(sc, source_sha):
    """No concurrency: GEN=a, then GEN=c, with a static archive of one name whose contents
    change. Not about leases; it says whether blaze relinks a program whose archive changed."""
    sc.project()
    extra = {"GEN_ARCHIVE": "same"}
    expected = {"a": sc.oracle("a", extra), "c": sc.oracle("c", extra)}
    sc.project()
    for gen_ in ("a", "c"):
        p = sc.client("a", GEN=gen_, **extra)
        sc.finish(p, gen_)
        if p.returncode != 0:
            sc.fail(f"the build of GEN={gen_} exited {p.returncode}: {p.err[-600:]}")
            return
        for name in PROGRAMS:
            got = sc.program(name)
            if got != (expected[gen_] or {}).get(name):
                sc.gaps.append(f"GEN={gen_}: {name} printed {got!r}, cargo's {(expected[gen_] or {}).get(name)!r}")
    sc.notes["gap-count"] = len(sc.gaps)


SCENARIOS = {
    "archive-control": archive_control,
    "same-worker": lambda sc, sha: same_or_cross(sc, False, sha),
    "cross-worker": lambda sc, sha: same_or_cross(sc, True, sha),
    "failure": failure,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", default="/tmp/leasebarrier")
    ap.add_argument("--json")
    ap.add_argument("--source-sha", default=os.environ.get("SOURCE_SHA", "unknown"))
    ap.add_argument("scenarios", nargs="*")
    args = ap.parse_args()
    names = args.scenarios or [n for n in SCENARIOS if n != "archive-control"] + ["archive-control"]
    work = os.path.abspath(args.work)
    os.makedirs(work, exist_ok=True)
    report = {}
    for name in names:
        print(f"== {name}", flush=True)
        sc = Scenario(name, work)
        try:
            SCENARIOS[name](sc, args.source_sha)
        except Exception as e:  # a harness step that could not be taken is the scenario's failure
            sc.fail(f"setup or step failed: {type(e).__name__}: {e}")
        try:
            report[name] = {"ok": not sc.failures, "failures": sc.failures, "evidence": sc.evidence(args.source_sha)}
        except Exception as e:
            report[name] = {"ok": False, "failures": sc.failures + [f"evidence: {e}"], "evidence": {}}
        sc.stop()
        print(f"  {'ok' if not sc.failures else 'FAILED'} {name}", flush=True)
    if args.json:
        with open(args.json, "w") as f:
            json.dump(report, f, indent=1, default=str)
    sys.exit(0 if all(r["ok"] for r in report.values()) else 1)


if __name__ == "__main__":
    main()
