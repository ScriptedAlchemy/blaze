#!/usr/bin/env python3
"""leasebarrier.py: two builds of one script's output, held apart by the build's leases, through
blaze's daemon and workers (docs/reports/SCRIPTEDITS.md, "What a build holds of a script").

usage: leasebarrier.py [--work DIR] [--json OUT] [--source-sha SHA] [scenario ...]
scenarios: same-worker, cross-worker, failure, lost-wake, reload-busy, direct, coab, internal,
           groups, overlay, worlds, join, once, once-failed, recovery, recovery-detached, link-control, archive-control
           (all, by default)

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

link-control  Not about leases, no concurrency: GEN=a then GEN=b, a static library named for the
              value. Whether a warm worker links the program with the library the script now
              names. What differs from cargo is recorded as a gap.
lost-wake     One worker. A is answered and its retire is held at the worker; B asks and is put off,
              and the worker is held between the put-off and the queue (`deferred`). A's retire is
              let go there, so that the wake it makes is taken before B is queued. B has to be
              built with no other request to wake the worker.
reload-busy   One worker. A is answered, its retire held. A forced `[env] GEN` in the cargo
              configuration makes B's request a reload of the workspace, which runs the script A
              reads the output of: B is put off, not failed, and built after the retire.
direct        Two direct runs (`ra-jit <project> --crate .. --exe ..`, no daemon, build 0) of one
              project and target directory. A is held after its scripts and before its compile
              and link; B, with another `GEN`, has to wait for A's process, not to rewrite the
              output A is to read.
coab          Three daemons over one project and target directory and the review's opposite orders
              (five packages p0..p4: scripts in p2 and p4; p1 and p3 have p2 as a dependency and p4
              as a build dependency): C reads p2's output and its retire is held; A (-p p1) runs p4's
              script and then needs p2's, which C reads; B (-p p3) finds p2 fresh and needs p4's.
              Neither may wait for the other: with no deadline that ends them, they all end when C
              is retired.
internal      An editor's `edit` of a source file, which the worker builds for a client that names no
              build. It is held before its compile and link while another worker, with another
              `GEN`, asks for the scripts; it must wait.
groups        Two direct runs (`ra-jit <R> --also <S>` and `ra-jit <S> --also <R>`), the same target
              directory, another `GEN` for each, scripts that run every build and a guard that
              fails the second execution of one for one value in one package. The first holds its
              first group (it has the admission of both); the second asks, and waits. Each script
              runs once for each build, and both are built.
generation    Another worker runs a script over a file it watches (one fingerprint, other output)
              while A's database is of the first run; an editor's edit to A reads the generated
              file in a body. A's program is what cargo builds of the tree: include, variable and
              `cfg` of the value it has now, the world that moved loaded again for the edit.
unrelated     A worker has R and S loaded, another runs S's script again, and an edit to R is
              built: S's moved world is not R's to refuse.
overlay       `internal` with `RA_JIT_OVERLAY=1` (which builds without the background path) and an
              edit that reads the generated file with `include_bytes!`.
worlds        A worker that has R and S loaded and an editor's edit of R, held before its compile
              and link: another worker's `GEN` for R's scripts has to wait (the edit reads R's
              output, not the last script group that was judged, S's).
join          A worker started by a build in R; a build in S joins S; then S again with another
              `GEN`: the scripts of S have to run for it (the join's leases are that request's, and
              ended with it).
once          (B also asks for S before it is retired, with its own `GEN`; S's script, a control that is always
              stale, runs once for A and once for B, and B's request for R does not run it: S is executed and
              compared to cargo's for each of the two values.) One build (an explicit `RA_JIT_BUILD`, several requests, as the daemon sends them) of a worker
              started by hand: A's first request runs R and is answered; B, another build, asks for R with
              another `GEN` and is put off; A's next request joins S and then asks R again. A script that runs
              every build with a guard that fails its second execution for one build (`ONCE_TAG`): A runs R
              once, B has run nothing while A's build lasts, and B is built after A is retired.
once-failed   The same with a script that fails for A: the failure is A's for good, each request of
              it fails alike, the script of R runs once for A, and B's build goes on meanwhile.
recovery      A's database is of one run of a script, another worker (B) runs it again and is held after
              its scripts for longer than the worker takes turns, and an editor's edit to A needs the world
              restored: the edit is neither failed nor asked about at every turn while B works, and is
              built when B ends, with no second request.
recovery-detached  `recovery` with a checkout read as another, so that the builds are detached and their links end
              on a thread of their own: the request that carries the recovery is held after its owner is released
              and before its token is gone, and the edit has to be answered when it goes, with no other request.
first-join    A worker started by hand with R loaded (`GEN=a`; its script is valid for `a`, fails for `b`, and
              keeps a trail), and ONE first request for S, another package that R does not consume, with `GEN=b`
              and a build number of its own: S is built as cargo builds it and R's script is not run for `b`.
first-join-private  The same with V file-alike to R (the same bytes), `RA_JIT_SPLIT=0` and `RA_JIT_OVERLAY=0`, and a script that
              fails for `b` in R's directory only: V is read through R by neither policy, so R's script is not run
              for it, V's own is, and V is built, as cargo builds it.
quick-choice  `RA_JIT_SPLIT=0`, quick overlay: a request for R is held as it enters its link (the executable was taken
              away), V's first request arrives (R is no candidate while it links), the worker settles and the link is let
              go: V is loaded on its own and judged for its variables, not read through R's world.
first-join-shared  The same with V, a checkout of R's repository with R's scripts: V reads R's scripts, which are
              judged for `b` (the trail says so), and fail as cargo's would.
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
    println!("cargo:rerun-if-env-changed=GEN");
    println!("cargo:rerun-if-env-changed=FAIL");
    println!("cargo:rerun-if-env-changed=GEN_ARCHIVE");
    println!("cargo:rerun-if-env-changed=GEN_FROM_FILE");
    println!("cargo:rerun-if-env-changed=ALWAYS");
    println!("cargo:rerun-if-env-changed=ONCE_DIR");
    println!("cargo:rerun-if-env-changed=ONCE_TAG");
    println!("cargo:rerun-if-env-changed=ONCE_BY_DIR");
    println!("cargo:rerun-if-env-changed=ONCE_ONLY");
    println!("cargo:rerun-if-env-changed=FAIL_PKG_VALUE");
    println!("cargo:rerun-if-env-changed=FAIL_DIR_VALUE");
    // The value is the variable's, or the content of a file that the script watches: the same
    // fingerprint (a watched path) for a run that leaves other output.
    let gen_ = match env::var("GEN_FROM_FILE").as_deref() {
        Ok("1") => {
            println!("cargo:rerun-if-changed=value.txt");
            fs::read_to_string("value.txt").unwrap().trim().to_owned()
        }
        _ => env::var("GEN").unwrap_or_else(|_| "none".to_owned()),
    };
    // A script cargo runs every build.
    if env::var("ALWAYS").as_deref() == Ok("1") {
        println!("cargo:rerun-if-changed=missing.txt");
    }
    let mut log = fs::OpenOptions::new().create(true).append(true).open("@LOG@").unwrap();
    writeln!(log, "{gen_}").unwrap();
    // A script that fails the second time it runs for one value, in one package: the extra
    // execution of a build is an effect that is not undone by anything that follows.
    let dir_name = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).file_name().unwrap().to_string_lossy().into_owned();
    // ONCE_ONLY names the one checkout whose script is guarded (every checkout is, without it).
    let guarded = env::var("ONCE_ONLY").map(|it| it == dir_name).unwrap_or(true);
    if let (Ok(once), true) = (env::var("ONCE_DIR"), guarded) {
        // Named for the package, or with ONCE_BY_DIR for the checkout it is in (two checkouts of
        // one package), and with ONCE_TAG for the build that asks (a build is its requests' tag).
        let whose = match env::var("ONCE_BY_DIR").as_deref() {
            Ok("1") => PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).file_name().unwrap().to_string_lossy().into_owned(),
            _ => env::var("CARGO_PKG_NAME").unwrap(),
        };
        let tag = env::var("ONCE_TAG").map(|it| format!("-{it}")).unwrap_or_default();
        let mark = format!("{once}/{whose}-{gen_}{tag}");
        if fs::OpenOptions::new().write(true).create_new(true).open(&mark).is_err() {
            eprintln!("script ran twice: {mark}");
            exit(7);
        }
    }
    // The values and tags of every run, in order, for the checkouts the guard does not fail.
    if let Ok(once) = env::var("ONCE_DIR") {
        let mut trail = fs::OpenOptions::new().create(true).append(true).open(format!("{once}/trail-{dir_name}")).unwrap();
        writeln!(trail, "{gen_} {}", env::var("ONCE_TAG").unwrap_or_default()).unwrap();
    }
    // "<package>:<value>": the script of that package fails for that value (valid for the others).
    if let Ok(spec) = env::var("FAIL_PKG_VALUE") {
        if spec == format!("{}:{gen_}", env::var("CARGO_PKG_NAME").unwrap()) {
            eprintln!("script failed for {spec}");
            exit(3);
        }
    }
    // "<directory>:<value>": the script fails for that value in the checkout of that name only (a
    // script may depend on where it is: its manifest directory).
    if let Ok(spec) = env::var("FAIL_DIR_VALUE") {
        if spec == format!("{dir_name}:{gen_}") {
            eprintln!("script failed for {spec}");
            exit(3);
        }
    }
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
name = "@PKG@"
version = "0.1.0"
edition = "2021"
build = "build.rs"

[[bin]]
name = "@PKG@-one"
path = "src/one.rs"

[[bin]]
name = "@PKG@-two"
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
        os.makedirs(self.ws)
        self.target = os.path.join(self.ws, "target")
        self.runs_log = os.path.join(self.dir, "runs.log")
        self.failures, self.notes, self.timeline, self.gaps = [], {}, [], []
        self.daemons = []

    # -- the project and the oracle
    def project(self, ws=None, config=None, pkg="gen"):
        ws = ws or self.ws
        os.makedirs(os.path.join(ws, "src"), exist_ok=True)
        for name, text in {"Cargo.toml": MANIFEST.replace("@PKG@", pkg),
                           "build.rs": BUILD_RS.replace("@LOG@", self.runs_log),
                           "src/one.rs": PROGRAM.replace("@NAME@", f"{pkg}-one"),
                           "src/two.rs": PROGRAM.replace("@NAME@", f"{pkg}-two")}.items():
            with open(os.path.join(ws, name), "w") as f:
                f.write(text)
        if config is not None:
            os.makedirs(os.path.join(ws, ".cargo"), exist_ok=True)
            with open(os.path.join(ws, ".cargo", "config.toml"), "w") as f:
                f.write(config)
        open(self.runs_log, "w").close()
        subprocess.run(["cargo", "generate-lockfile"], cwd=ws, env=self.env("x"), capture_output=True, check=True)

    def oracle(self, gen_, extra=None, config=None, pkg="gen"):
        """What cargo's build gives for `GEN=gen_`: the output each program must have."""
        ws = os.path.join(self.dir, "oracle-ws")
        shutil.rmtree(ws, ignore_errors=True)
        saved, self.runs_log = self.runs_log, os.path.join(self.dir, "oracle-runs.log")
        self.project(ws, config, pkg)
        self.runs_log = saved
        env = dict(self.env("oracle"), GEN=gen_, **(extra or {}))
        env["CARGO_TARGET_DIR"] = os.path.join(self.dir, "oracle-target")
        env.pop("RA_JIT_WHOLE_BUILD", None)
        ran = subprocess.run(["cargo", "build"], cwd=ws, env=env, capture_output=True, text=True, timeout=WAIT)
        if ran.returncode != 0:
            self.fail(f"the oracle (cargo, GEN={gen_}) did not build: {ran.stderr[-400:]}")
            return None
        out = {}
        for name in (f"{pkg}-one", f"{pkg}-two"):
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
        # The deadline of a build that waits, in the binaries that have one (it is a failure there).
        env["RA_JIT_SCRIPT_LEASE_WAIT_SECS"] = "30"
        os.makedirs(env["RA_JIT_SCRIPT_BARRIER_DIR"], exist_ok=True)
        env.update(extra or {})
        return env

    def client(self, home, args=(), cwd=None, **env):
        """A `cargo ra-jit build` that starts now: its process, and when it started."""
        p = subprocess.Popen(["cargo", "ra-jit", "build", *args], cwd=cwd or self.ws, env=self.env(home, env),
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
            # What the client's children still hold open is not waited for.
            try:
                out, err = p.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                out, err = "", "(the client was killed; its output was not read)"
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

    def events(self, home, text=None):
        """The `script-lease:` lines of a worker's log: (what, build, at_us, rest)."""
        found = []
        for line in (self.log_of(home) if text is None else text).splitlines():
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
        found = self.run_dirs()
        return found[0] if found else None

    def run_dirs(self):
        """The `OUT_DIR` of every group's script below the target directory."""
        return sorted(glob.glob(os.path.join(self.target, "ra-jit", "build-scripts", "debug", "build", "gen*-*", "out")))

    def hash_all(self):
        h = hashlib.sha256()
        for d in self.run_dirs():
            h.update(d.encode())
            h.update(tree_hash(d).encode())
        return h.hexdigest()

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

    def check_programs(self, tag, kept, expected, gen_, gap_native=None):
        """Each program is `gen_`'s build and, with `expected`, cargo's. With `gap_native`, a
        program whose only difference is that value of `native` is the warm worker's gap that
        `link-control` shows without any second build: recorded, not a failure of the leases."""
        for name in PROGRAMS:
            got = self.program(name, kept.get(name))
            want = expected[name] if expected is not None else None
            if want is not None and got != want:
                stale = want.replace(f"native={ord(gen_)}", f"native={gap_native}") if gap_native else None
                if got is not None and got == stale:
                    self.gaps.append(f"{tag}: {name} linked the library of the build before: {got!r}, cargo's {want!r}")
                else:
                    self.fail(f"{tag}: {name} printed {got!r}, the oracle's {want!r}")
            elif got is None or f"include={gen_} env={gen_} cfg={gen_}" not in got:
                self.fail(f"{tag}: {name} is not GEN={gen_}'s build: {got!r}")
            self.notes.setdefault("programs", {})[f"{tag}/{name}"] = got

    def unchanged(self, before, what, p=None):
        """For WATCH seconds the output the script wrote is what it was, and no script ran."""
        runs, end = len(self.runs()), time.time() + WATCH
        while time.time() < end:
            now = self.hash_all()
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
            try:
                subprocess.run(["cargo", "ra-jit", "stop"], cwd=self.ws, env=self.env(home), capture_output=True,
                               timeout=120)
            except Exception as why:  # a daemon that does not stop is not the next scenario's to find out
                print(f"  (stop {home}: {why})", flush=True)


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
    # One archive name whose contents are the build's: a script of another build that writes
    # over it while this one links is what the leases are to prevent.
    extra = {"GEN_ARCHIVE": "same"}
    exp_a, exp_b = sc.oracle("a", extra), sc.oracle("b", extra)
    sc.project()
    sc.arm(a_home, "consume")
    A = sc.client(a_home, GEN="a", **extra)
    ba = sc.reached(a_home, "consume", A)
    if ba is None:
        sc.finish(A, "A", 5)
        return
    sc.disarm(a_home, "consume")
    sc.arm(a_home, "retire")
    sc.timeline.append((time.time_ns() // 1000, f"A (build {ba}) held at consume ({a_home})"))
    held = sc.hash_all()
    sc.notes["a-output-hash"] = held
    if sc.runs() != ["a"]:
        sc.fail(f"A's script did not run once before the barrier: {sc.runs()}")
    # B asks, with another value of the variable the script watches, while A is held between
    # its scripts and its compile and link. (In one worker B's checkout is loaded after A's
    # build, which the worker settles first; in another's it is loaded at once, and waits for
    # the lease.)
    B = sc.client(b_home, GEN="b", **extra)
    if cross:
        sc.notes["b-waits-event"] = sc.wait_event(b_home, "waits")
    sc.unchanged(held, "B asking while A is held before its compile and link", B)
    sc.release(a_home, "consume", ba)
    sc.finish(A, "A")
    if A.returncode != 0:
        sc.fail(f"A's build exited {A.returncode}: {A.err[-600:]}")
    if "[ra-jit]" not in A.err:
        sc.fail("A's build was not the daemon's whole build")
    kept_a = sc.keep("a")
    # A's client has its answer. The daemon's word that A is over is held at the worker.
    sc.reached(a_home, "retire", B)
    bb = None
    if not cross:
        put = sc.wait_event(a_home, "put-off")
        sc.notes["b-put-off-event"] = put
        bb = put[1] if put else None
        if bb == ba:
            sc.fail("the build that was put off is A's own")
    sc.check_programs("A", kept_a, exp_a, "a")
    sc.unchanged(held, "B asking after A's client was answered, before A is retired", B)
    if B.poll() is not None:
        sc.fail("B's client ended before A was retired")
    sc.release(a_home, "retire", ba)
    sc.disarm(a_home, "retire")
    sc.finish(B, "B")
    if B.returncode != 0:
        sc.fail(f"B's build exited {B.returncode}: {B.err[-600:]}")
    kept_b = sc.keep("b")
    sc.check_programs("B", kept_b, exp_b, "b", gap_native=None if cross else ord("a"))
    # A's copy is still A's: nothing B did reached it.
    sc.check_programs("A-after-B", kept_a, exp_a, "a")
    if sc.runs() != ["a", "b"]:
        sc.fail(f"the script ran {sc.runs()}, once for each build, 'a' then 'b'")
    sc.timeouts()
    ev_a, ev_b = sc.events(a_home), sc.events(b_home)
    retire_reached = next((e[3] for e in ev_a if e[0] == "barrier-reached" and e[1] == ba and "retire" in e[4]), None)
    common = [
        ("A held at consume", first(ev_a, "barrier-reached", ba)),
        ("A released", first(ev_a, "barrier-released", ba)),
        ("A's client ended", A.ended),
    ]
    if not cross:
        order(sc, a_home, common[:2] + [
            ("retire held", retire_reached),
            ("A retired", first(ev_a, "retired", ba)),
            ("B ran the script", next((e[3] for e in ev_a if e[0] == "runs" and e[1] == bb), None)),
        ])
        order(sc, a_home, [common[2], ("A retired", first(ev_a, "retired", ba))])
        order(sc, a_home, [("B put off", first(ev_a, "put-off", bb) if bb else None),
                           ("A retired", first(ev_a, "retired", ba))])
    else:
        order(sc, a_home, common[:2] + [("retire held", retire_reached), ("A retired", first(ev_a, "retired", ba))])
        order(sc, a_home, [common[2], ("A retired", first(ev_a, "retired", ba))])
        order(sc, b_home, [("B waits", next((e[3] for e in ev_b if e[0] == "waits"), None)),
                           ("A retired", first(ev_a, "retired", ba)),
                           ("B ran the script", next((e[3] for e in ev_b if e[0] == "runs"), None))])
    sc.notes["builds"] = {"A": ba, "B": bb}


def failure(sc, source_sha):
    sc.project()
    ok = sc.client("a", GEN="a", GEN_ARCHIVE="same")
    sc.finish(ok, "warm")
    if ok.returncode != 0:
        sc.fail(f"the build that was to warm the worker exited {ok.returncode}: {ok.err[-600:]}")
        return
    messages = []
    for tag, env in (("F1", {"GEN": "bad", "FAIL": "1"}), ("G", {"GEN": "c"}), ("F2", {"GEN": "bad", "FAIL": "1"})):
        p = sc.client("a", GEN_ARCHIVE="same", **env)
        sc.finish(p, tag)
        sc.notes[f"{tag}-rc"] = p.returncode
        if tag == "G":
            if p.returncode != 0:
                sc.fail(f"G, after a failure, exited {p.returncode}: {p.err[-600:]}")
            # What G printed is cargo's or it is a gap of the warm worker's link (link-control).
            for name in PROGRAMS:
                got = sc.program(name)
                if got is None or f"include=c env=c cfg=c native=99" not in got:
                    sc.gaps.append(f"G after a failed build: {name} printed {got!r}")
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


LOST = float(os.environ.get("LEASEBARRIER_LOST", "120"))
CONFIG_B = '[env]\nGEN = { value = "b", force = true }\n'


def settled_a(sc, extra, args=()):
    """A (GEN=a) built and answered, its retire held at the worker: the window in which A's
    build is over for its client and the worker has not heard it."""
    sc.project()
    sc.arm("a", "retire")
    A = sc.client("a", args, GEN="a", **extra)
    sc.finish(A, "A")
    if A.returncode != 0:
        sc.fail(f"A's build exited {A.returncode}: {A.err[-600:]}")
        return None
    ba = sc.reached("a", "retire")
    return (A, ba) if ba is not None else None


def lost_wake(sc, source_sha):
    extra = {"GEN_ARCHIVE": "same"}
    exp_b = sc.oracle("b", extra)
    # One program, so one request of B's: no other request is there to wake the worker.
    one = ("--bin", "gen-one")
    held = settled_a(sc, extra, one)
    if held is None:
        return
    A, ba = held
    sc.arm("a", "deferred")
    B = sc.client("a", one, GEN="b", **extra)
    bb = sc.reached("a", "deferred", B)
    if bb is None:
        sc.finish(B, "B", 5)
        return
    sc.notes["b-put-off"] = sc.wait_event("a", "put-off", bb, within=30)
    # A's retirement arrives while the worker is held between B's put-off and its queue.
    sc.release("a", "retire", ba)
    sc.wait_event("a", "retired", ba, within=30)
    sc.disarm("a", "deferred")
    sc.release("a", "deferred", bb)
    sc.finish(B, "B", within=LOST)
    if B.returncode != 0:
        sc.fail(f"B's build exited {B.returncode} (a request nothing woke is killed, not ended): {B.err[-600:]}")
        return
    got = sc.program("gen-one")
    want = exp_b["gen-one"]
    stale = want.replace(f"native={ord('b')}", f"native={ord('a')}")
    if got == stale:
        sc.gaps.append(f"B: gen-one linked the library of the build before: {got!r}, cargo's {want!r}")
    elif got != want:
        sc.fail(f"B: gen-one printed {got!r}, the oracle's {want!r}")
    if sc.runs() != ["a", "b"]:
        sc.fail(f"the script ran {sc.runs()}, once for each build, 'a' then 'b'")
    sc.timeouts()


def reload_busy(sc, source_sha):
    extra = {"GEN_ARCHIVE": "same"}
    exp_b = sc.oracle("b", extra, config=CONFIG_B)
    held = settled_a(sc, extra)
    if held is None:
        return
    A, ba = held
    # The configuration forces GEN=b, whatever the process says: the workspace is loaded again
    # for B's request, and its script is to run again over the output A reads.
    os.makedirs(os.path.join(sc.ws, ".cargo"), exist_ok=True)
    with open(os.path.join(sc.ws, ".cargo", "config.toml"), "w") as f:
        f.write(CONFIG_B)
    held_hash = sc.hash_all()
    B = sc.client("a", GEN="a", **extra)
    put = sc.wait_event("a", "put-off", within=60)
    sc.notes["b-put-off"] = put
    if put and "reload:" not in put[4]:
        sc.fail(f"B was put off by the stale check, not by a reload: {put[4]}")
    sc.unchanged(held_hash, "B asking for a reload while A is not retired", B)
    if B.poll() is not None:
        sc.fail(f"B's client ended ({B.returncode}) before A was retired: {B.communicate()[1][-400:]}")
    sc.release("a", "retire", ba)
    sc.disarm("a", "retire")
    sc.finish(B, "B", within=LOST)
    if B.returncode != 0:
        sc.fail(f"B's build exited {B.returncode}: {B.err[-600:]}")
        return
    sc.check_programs("B", {}, exp_b, "b", gap_native=ord("a"))
    if sc.runs() != ["a", "b"]:
        sc.fail(f"the script ran {sc.runs()}, once for each build, 'a' then 'b'")
    sc.timeouts()


def direct(sc, source_sha):
    extra = {"GEN_ARCHIVE": "same"}
    exp_a, exp_b = sc.oracle("a", extra), sc.oracle("b", extra)
    sc.project()

    def run(tag, gen_, home):
        env = sc.env(home, dict(GEN=gen_, **extra))
        env.pop("RA_JIT_WHOLE_BUILD", None)
        exe, obj = os.path.join(sc.dir, f"{tag}.bin"), os.path.join(sc.dir, f"{tag}.o")
        err = open(os.path.join(sc.dir, f"direct-{tag}.err"), "w")
        p = subprocess.Popen([RA_JIT, sc.ws, "--crate", "gen_one", "--out", obj, "--exe", exe],
                             cwd=sc.ws, env=env, stdout=subprocess.DEVNULL, stderr=err, stdin=subprocess.DEVNULL)
        p.exe, p.errfile, p.started = exe, err.name, time.time_ns() // 1000
        return p

    sc.arm("a", "consume")
    A = run("A", "a", "a")
    ba = sc.reached("a", "consume", A)
    if ba is None:
        A.kill()
        return
    sc.disarm("a", "consume")
    held = sc.hash_all()
    B = run("B", "b", "b")
    sc.unchanged(held, "B (direct) asking while A (direct) is held before its compile and link", B)
    if B.poll() is not None:
        sc.fail(f"B ended ({B.returncode}) while A was held")
    sc.release("a", "consume", ba)
    for tag, p in (("A", A), ("B", B)):
        try:
            p.wait(timeout=WAIT)
        except subprocess.TimeoutExpired:
            p.kill()
            sc.fail(f"{tag} (direct) did not end in {WAIT:.0f}s")
        sc.timeline.append((time.time_ns() // 1000, f"direct {tag} ended rc={p.returncode}"))
        if p.returncode != 0:
            sc.fail(f"{tag} (direct) exited {p.returncode}: {open(p.errfile, errors='replace').read()[-600:]}")
    for tag, p, want, gen_ in (("A", A, exp_a, "a"), ("B", B, exp_b, "b")):
        got = subprocess.run([p.exe], capture_output=True, text=True).stdout if os.path.exists(p.exe) else None
        expect = want["gen-one"]
        if got != expect:
            sc.fail(f"{tag} (direct): printed {got!r}, the oracle's {expect!r}")
        sc.notes.setdefault("programs", {})[f"{tag}/direct"] = got
    if sc.runs() != ["a", "b"]:
        sc.fail(f"the script ran {sc.runs()}, once for each process, 'a' then 'b'")
    ev_a = sc.events("a", open(A.errfile, errors="replace").read())
    ev_b = sc.events("b", open(B.errfile, errors="replace").read())
    sc.notes["direct-events-a"], sc.notes["direct-events-b"] = ev_a, ev_b
    if not any(e[0] == "waits" for e in ev_b):
        sc.fail("B (direct) did not wait for A's process")
    sc.timeouts()


X_SCRIPT = r'''use std::{env, fs, io::Write, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=@VAR@");
    let v = env::var("@VAR@").unwrap_or_else(|_| "none".to_owned());
    let mut log = fs::OpenOptions::new().create(true).append(true).open("@LOG@").unwrap();
    writeln!(log, "@TAG@:{v}").unwrap();
    fs::write(PathBuf::from(env::var("OUT_DIR").unwrap()).join("val.rs"), format!("pub const VAL: &str = {v:?};\n")).unwrap();
}
'''
FROM_Y_SCRIPT = r'''use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("from_y.rs"), format!("pub const FROM_Y: &str = {:?};\n", p4::VAL)).unwrap();
}
'''


def coab_project(sc, ws):
    def put(name, text):
        path = os.path.join(ws, name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)

    put("Cargo.toml", '[workspace]\nmembers = ["p0", "p1", "p2", "p3", "p4"]\nresolver = "2"\n')
    for lib, tag, var in (("p2", "x", "XVAL"), ("p4", "y", "YVAL")):
        put(f"{lib}/Cargo.toml", f'[package]\nname = "{lib}"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n')
        put(f"{lib}/build.rs", X_SCRIPT.replace("@VAR@", var).replace("@TAG@", tag).replace("@LOG@", sc.runs_log))
        put(f"{lib}/src/lib.rs", 'include!(concat!(env!("OUT_DIR"), "/val.rs"));\n')
    put("p0/Cargo.toml", '[package]\nname = "p0"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\np2 = { path = "../p2" }\n')
    put("p0/src/main.rs", 'fn main() { println!("p0 x={}", p2::VAL); }\n')
    for prog in ("p1", "p3"):
        put(f"{prog}/Cargo.toml", f'[package]\nname = "{prog}"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n'
            '[dependencies]\np2 = { path = "../p2" }\n[build-dependencies]\np4 = { path = "../p4" }\n')
        put(f"{prog}/build.rs", FROM_Y_SCRIPT)
        put(f"{prog}/src/main.rs", 'include!(concat!(env!("OUT_DIR"), "/from_y.rs"));\n'
            f'fn main() {{ println!("{prog} x={{}} y={{}}", p2::VAL, FROM_Y); }}\n')
    open(sc.runs_log, "w").close()
    subprocess.run(["cargo", "generate-lockfile"], cwd=ws, env=sc.env("x"), capture_output=True, check=True)


def coab(sc, source_sha):
    coab_project(sc, sc.ws)
    # What cargo builds for each: the oracle, in a copy of the project.
    expected = {}
    for prog, x, y in (("p0", "x1", None), ("p1", "x2", "y2"), ("p3", "x1", "y3")):
        shutil.rmtree(os.path.join(sc.dir, "oracle-ws"), ignore_errors=True)
        saved, sc.runs_log = sc.runs_log, os.path.join(sc.dir, "oracle-runs.log")
        coab_project(sc, os.path.join(sc.dir, "oracle-ws"))
        sc.runs_log = saved
        env = dict(sc.env("oracle"), XVAL=x, CARGO_TARGET_DIR=os.path.join(sc.dir, "oracle-target"))
        if y:
            env["YVAL"] = y
        env.pop("RA_JIT_WHOLE_BUILD", None)
        ran = subprocess.run(["cargo", "build", "-p", prog], cwd=os.path.join(sc.dir, "oracle-ws"), env=env,
                             capture_output=True, text=True, timeout=WAIT)
        if ran.returncode != 0:
            sc.fail(f"the oracle (cargo, {prog}) did not build: {ran.stderr[-400:]}")
            return
        expected[prog] = subprocess.run([os.path.join(sc.dir, "oracle-target", "debug", prog)],
                                        capture_output=True, text=True).stdout
    open(sc.runs_log, "w").close()
    sc.arm("c", "retire")
    # C reads p2's output, its build is answered and its retire is held.
    C = sc.client("c", ("-p", "p0"), XVAL="x1")
    sc.finish(C, "C")
    if C.returncode != 0:
        sc.fail(f"C's build exited {C.returncode}: {C.err[-600:]}")
        return
    bc = sc.reached("c", "retire")
    if bc is None:
        return
    # A runs p4's script, then needs p2's, which C reads.
    A = sc.client("a", ("-p", "p1"), XVAL="x2", YVAL="y2")
    sc.notes["a-waits"] = sc.wait_event("a", "waits", within=WAIT)
    # B finds p2 fresh, and needs p4's script, which A has run.
    B = sc.client("b", ("-p", "p3"), XVAL="x1", YVAL="y3")
    sc.notes["b-waits"] = sc.wait_event("b", "waits", within=WAIT)
    time.sleep(WATCH)
    for tag, p in (("A", A), ("B", B)):
        if p.poll() is not None:
            sc.fail(f"{tag}'s client ended ({p.returncode}) while C was not retired: {p.communicate()[1][-400:]}")
    sc.release("c", "retire", bc)
    sc.disarm("c", "retire")
    sc.finish(A, "A", within=LOST)
    kept_a = os.path.join(sc.dir, "a-p1")
    if os.path.exists(os.path.join(sc.ws, "target", "debug", "p1")):
        shutil.copy2(os.path.join(sc.ws, "target", "debug", "p1"), kept_a)
    sc.finish(B, "B", within=LOST)
    for tag, p in (("A", A), ("B", B)):
        if p.returncode != 0:
            sc.fail(f"{tag}'s build exited {p.returncode} (no deadline may end it): {p.err[-600:]}")
    got = subprocess.run([kept_a], capture_output=True, text=True).stdout if os.path.exists(kept_a) else None
    if got != expected["p1"]:
        sc.fail(f"A: p1 printed {got!r}, the oracle's {expected['p1']!r}")
    got = sc.program("p3")
    if got != expected["p3"]:
        sc.fail(f"B: p3 printed {got!r}, the oracle's {expected['p3']!r}")
    sc.notes["runs"] = sc.runs()
    for want in ("x:x1", "y:y2", "x:x2", "y:y3"):
        if want not in sc.runs():
            sc.fail(f"the script that logs {want!r} did not run: {sc.runs()}")
    sc.timeouts()


def send_edit(home_dir, path, text, sock="daemon.sock"):
    """An editor's `edit` of a source file, as the daemon takes it (docs/live.md): the whole text on
    one line. Its answer, or the error of not getting one."""
    import socket
    escaped = text.replace("\\", "\\\\").replace("\n", "\\n").replace("\t", "\\t").replace("\r", "\\r")
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(WAIT)
        s.connect(os.path.join(home_dir, sock))
        s.sendall(f"edit\t{path}\t{escaped}\n\n".encode())  # a worker reads the request and the process's variables
        return s.makefile().readline().strip()


def internal(sc, source_sha):
    """An edit is built by the worker for a client that names no build: its compile and link read
    the scripts' output as the process last judged it, and another worker must not change it."""
    import threading
    extra = {"GEN_ARCHIVE": "same"}
    one = ("--bin", "gen-one")
    exp_b = sc.oracle("b", extra)
    sc.project()
    A1 = sc.client("a", one, GEN="a", **extra)
    sc.finish(A1, "A1")
    if A1.returncode != 0:
        sc.fail(f"the first build exited {A1.returncode}: {A1.err[-600:]}")
        return
    sc.arm("a", "consume")
    path = os.path.join(sc.ws, "src", "one.rs")
    text = open(path).read() + "// an edit\n"
    answer = {}
    editor = threading.Thread(target=lambda: answer.update(said=send_edit(sc.env("a")["RA_JIT_HOME"], path, text)))
    editor.start()
    nobody = sc.reached("a", "consume", None, within=120)
    if nobody is None:
        return
    sc.disarm("a", "consume")
    held = sc.hash_all()
    B = sc.client("b", one, GEN="b", **extra)
    sc.notes["b-waits"] = sc.wait_event("b", "waits")
    sc.unchanged(held, "B asking while an edit is held before its compile and link", B)
    if B.poll() is not None:
        sc.fail(f"B ended ({B.returncode}) while the edit was held")
    sc.release("a", "consume", nobody)
    editor.join(WAIT)
    sc.notes["edit-said"] = answer.get("said")
    if not str(answer.get("said", "")).startswith("ok"):
        sc.fail(f"the edit was answered {answer.get('said')!r}")
    sc.finish(B, "B", within=LOST)
    if B.returncode != 0:
        sc.fail(f"B's build exited {B.returncode}: {B.err[-600:]}")
    got = sc.program("gen-one")
    if got != exp_b["gen-one"]:
        sc.fail(f"B: gen-one printed {got!r}, the oracle's {exp_b['gen-one']!r}")
    mine = [e for e in sc.events("a") if e[0] in ("pins", "holds") and e[1] == nobody]
    sc.notes["edit-holds"] = mine
    if not mine:
        sc.fail("the edit's build did not log that it reads the scripts' output (a build with no number "
                "holds nothing in the binary under test)")
    sc.timeouts()


def shared_projects(sc, worktrees=False):
    """Two projects that build in one target directory; their scripts are two groups. Of other
    names (so that neither is the other's split) for two direct runs; with `worktrees`, two
    checkouts of one repository (what a worker joins: the daemon has one worker for them) whose
    build scripts differ, so that the second is loaded whole and not as a split of the first."""
    sc.r, sc.s = os.path.join(sc.dir, "r"), os.path.join(sc.dir, "s")
    sc.target = os.path.join(sc.dir, "target-shared")
    saved = sc.runs_log
    if not worktrees:
        sc.project(sc.r, pkg="gena")
        sc.runs_log = saved
        sc.project(sc.s, pkg="genb")
        return {"GEN_ARCHIVE": "same", "CARGO_TARGET_DIR": sc.target}
    sc.project(sc.r)
    def git(*args, cwd=sc.r):
        subprocess.run(["git", "-c", "user.name=probe", "-c", "user.email=probe@localhost", *args],
                       cwd=cwd, capture_output=True, check=True)
    git("init", "-q")
    with open(os.path.join(sc.r, ".gitignore"), "w") as f:
        f.write("target/\n")
    git("add", "-A")
    git("commit", "-qm", "probe")
    git("worktree", "add", "-q", "-b", "other", sc.s)
    with open(os.path.join(sc.s, "build.rs"), "a") as f:
        f.write("// this checkout's script is another's\n")
    return {"GEN_ARCHIVE": "same", "CARGO_TARGET_DIR": sc.target}


def direct_run(sc, tag, project, also, crate, gen_, home, extra):
    env = sc.env(home, dict(GEN=gen_, **extra))
    env.pop("RA_JIT_WHOLE_BUILD", None)
    exe, obj = os.path.join(sc.dir, f"{tag}.bin"), os.path.join(sc.dir, f"{tag}.o")
    err = open(os.path.join(sc.dir, f"direct-{tag}.err"), "w")
    argv = [RA_JIT, project, *(["--also", also] if also else []), "--crate", crate, "--out", obj, "--exe", exe]
    p = subprocess.Popen(argv, cwd=project, env=env, stdout=subprocess.DEVNULL, stderr=err, stdin=subprocess.DEVNULL)
    p.exe, p.errfile, p.started = exe, err.name, time.time_ns() // 1000
    return p


def groups(sc, source_sha):
    """Two direct runs with `--also` in opposite orders over one target directory, scripts that
    run every build, and an external guard that fails the second execution of one for one value.
    The first holds its first group, with the admission, while the second asks: the second has
    nothing for the first to wait for, waits for the admission, and each runs each script once."""
    extra = shared_projects(sc)
    extra = dict(extra, ALWAYS="1", ONCE_DIR=os.path.join(sc.dir, "once"))
    os.makedirs(extra["ONCE_DIR"])
    exp_a, exp_b = sc.oracle("a", {"GEN_ARCHIVE": "same"}, pkg="gena"), sc.oracle("b", {"GEN_ARCHIVE": "same"}, pkg="genb")
    sc.arm("a", "group")
    A = direct_run(sc, "A", sc.r, sc.s, "gena_one", "a", "a", extra)
    ba = sc.reached("a", "group", A)
    if ba is None:
        A.kill()
        return
    sc.disarm("a", "group")
    B = direct_run(sc, "B", sc.s, sc.r, "genb_one", "b", "b", extra)
    deadline = time.time() + 60
    while time.time() < deadline and not any(
            e[0] == "waits" for e in sc.events("b", open(os.path.join(sc.dir, "direct-B.err"), errors="replace").read())):
        time.sleep(0.2)
    waits = [e for e in sc.events("b", open(os.path.join(sc.dir, "direct-B.err"), errors="replace").read()) if e[0] == "waits"]
    sc.notes["b-waits"] = waits[:1]
    if not waits:
        sc.fail("B (direct) did not wait for A's admission")
    sc.notes["held"] = sc.hash_all()
    sc.unchanged(sc.notes["held"], "B asking while A holds the admission of both groups", B)
    sc.release("a", "group", ba)
    for tag, p in (("A", A), ("B", B)):
        try:
            p.wait(timeout=LOST)
        except subprocess.TimeoutExpired:
            p.kill()
            sc.fail(f"{tag} (direct) did not end in {LOST:.0f}s")
        sc.timeline.append((time.time_ns() // 1000, f"direct {tag} ended rc={p.returncode}"))
        if p.returncode != 0:
            sc.fail(f"{tag} (direct) exited {p.returncode}: {open(p.errfile, errors='replace').read()[-500:]}")
    for tag, p, want, name in (("A", A, exp_a, "gena-one"), ("B", B, exp_b, "genb-one")):
        got = subprocess.run([p.exe], capture_output=True, text=True).stdout if os.path.exists(p.exe) else None
        if got != want[name]:
            sc.fail(f"{tag} (direct): printed {got!r}, the oracle's {want[name]!r}")
        sc.notes.setdefault("programs", {})[tag] = got
    ran = sorted(it for it in os.listdir(extra["ONCE_DIR"]) if not it.startswith("trail-"))
    sc.notes["executions"] = ran
    if ran != ["gena-a", "gena-b", "genb-a", "genb-b"]:
        sc.fail(f"each script was to run once for each build: {ran}")
    sc.timeouts()


EDIT_BYTES = (
    '\nstatic GENERATED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gen.rs"));\n'
    '#[used]\nstatic KEEP: &[&[u8]] = &[GENERATED];\n'
)


def edit_while_held(sc, extra, ws, pkg, build_cwd, other_cwd, other_pkg, other_gen, bytes_in_edit, label,
                    home_dir=None, sock="daemon.sock"):
    """An editor's edit of `ws`'s `one.rs`, held before its compile and link, while a worker of
    another `GEN` asks for the scripts of `other_cwd`: it waits for the edit."""
    import threading
    path = os.path.join(ws, "src", "one.rs")
    text = open(path).read() + ("// an edit\n" if not bytes_in_edit else EDIT_BYTES)
    sc.arm("a", "consume")
    answer = {}
    home_dir = home_dir or sc.env("a")["RA_JIT_HOME"]
    editor = threading.Thread(target=lambda: answer.update(said=send_edit(home_dir, path, text, sock)))
    editor.start()
    nobody = sc.reached("a", "consume", None, within=120)
    if nobody is None:
        return None
    sc.disarm("a", "consume")
    held = sc.hash_all()
    B = sc.client("b", ("--bin", f"{other_pkg}-one"), cwd=other_cwd, GEN=other_gen, **extra)
    sc.notes[f"{label}-b-waits"] = sc.wait_event("b", "waits")
    sc.unchanged(held, f"B asking while {label} is held before its compile and link", B)
    if B.poll() is not None:
        sc.fail(f"B ended ({B.returncode}) while the edit was held")
    sc.release("a", "consume", nobody)
    editor.join(WAIT)
    sc.notes[f"{label}-edit-said"] = answer.get("said")
    if not str(answer.get("said", "")).startswith("ok"):
        sc.fail(f"the edit was answered {answer.get('said')!r}")
    sc.finish(B, "B", within=LOST)
    if B.returncode != 0:
        sc.fail(f"B's build exited {B.returncode}: {B.err[-600:]}")
    return nobody


def overlay(sc, source_sha):
    extra = {"GEN_ARCHIVE": "same", "RA_JIT_OVERLAY": "1"}
    sc.project()
    A1 = sc.client("a", ("--bin", "gen-one"), GEN="a", **extra)
    sc.finish(A1, "A1")
    if A1.returncode != 0:
        sc.fail(f"the first build exited {A1.returncode}: {A1.err[-600:]}")
        return
    nobody = edit_while_held(sc, extra, sc.ws, "gen", sc.ws, sc.ws, "gen", "b", True, "overlay")
    if nobody is not None and not any(e[0] in ("pins", "holds") and e[1] == nobody for e in sc.events("a")):
        sc.fail("the edit's build did not log that it reads the scripts' output")
    sc.timeouts()


def by_hand(sc, extra, gen_="a", crate="gen_one"):
    """A whole-build worker started by hand (`ra-jit <R> --exe .. --socket ..`), which has R
    loaded for its own load and serves: the way a checkout is joined (the daemon has a worker for
    each directory a build is asked in). Its pid and log are saved with the scenario."""
    sock = os.path.join(sc.dir, "worker.sock")
    env = sc.env("a", dict(GEN=gen_, **extra))
    env.pop("RA_JIT_WHOLE_BUILD", None)
    err = open(os.path.join(sc.dir, "worker.log"), "w")
    argv = [RA_JIT, sc.r, "--crate", crate, "--out", os.path.join(sc.dir, "w.o"), "--exe",
            os.path.join(sc.dir, "w.bin"), "--socket", sock]
    worker = subprocess.Popen(argv, cwd=sc.r, env=env, stdout=subprocess.DEVNULL, stderr=err,
                              stdin=subprocess.PIPE, start_new_session=True)
    sc.notes["worker-pid"] = worker.pid
    return worker, sock, env


def sync(sc, sock, env, *flags, within=None, **vars):
    """`ra-jit sync <socket> ..`: one request to the worker, as a client of its own. The process,
    killed (and said so) when it does not end in `within`."""
    env = dict(env, **vars)
    p = subprocess.Popen([RA_JIT, "sync", sock, *flags], cwd=sc.r, env=env, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True, errors="replace", stdin=subprocess.DEVNULL)
    try:
        out, err = p.communicate(timeout=within or WAIT)
    except subprocess.TimeoutExpired:
        p.kill()
        try:
            out, err = p.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            out, err = "", ""
        return None, "(killed: it did not end in %ss)" % (within or WAIT)
    return p.returncode, (out + err)


def serving(sc, sock, env):
    """Waits for the worker to serve: its first program built."""
    deadline = time.time() + WAIT
    while time.time() < deadline:
        if os.path.exists(sock):
            rc, said = sync(sc, sock, env, within=WAIT)
            if rc == 0:
                return True
        time.sleep(1)
    sc.fail("the worker started by hand did not serve")
    return False


def join_s(sc, sock, env, gen_, within=None):
    s_out = os.path.join(sc.dir, "s.o")
    s_exe = os.path.join(sc.dir, "s.bin")
    return sync(sc, sock, env, "--project", sc.s, "--crate", "gen_one", "--out", s_out, "--exe", s_exe,
                within=within, GEN=gen_)


def worlds(sc, source_sha):
    extra = shared_projects(sc, worktrees=True)
    worker, sock, env = by_hand(sc, extra)
    try:
        if not serving(sc, sock, env):
            return
        rc, said = join_s(sc, sock, env, "a")
        if rc != 0:
            sc.fail(f"S did not join (rc {rc}): {said[-400:]}")
            return
        log = open(os.path.join(sc.dir, "worker.log"), errors="replace").read()
        sc.notes["joined"] = [l for l in log.splitlines() if l.startswith("join ")]
        if not sc.notes["joined"]:
            sc.fail("S was not joined: the case was not exercised")
            return
        if len(sc.run_dirs()) < 2:
            sc.fail(f"the scripts of R and S did not both run: {sc.run_dirs()}")
            return
        # S was judged last. The edit is of R, and it goes to the worker by hand.
        nobody = edit_while_held(sc, extra, sc.r, "gen", sc.r, sc.r, "gen", "b", True, "worlds",
                                 home_dir=sc.dir, sock="worker.sock")
        if nobody is not None:
            pinned = [e for e in sc.events("a", log + open(os.path.join(sc.dir, "worker.log"), errors="replace").read())
                      if e[0] == "pins" and e[1] == nobody]
            sc.notes["pins"] = pinned
            # The programs of R read R's scripts, and a world of S's is none of theirs.
            if not pinned or "world=1" not in pinned[0][4]:
                sc.fail(f"the edit did not read the output of R's scripts: {pinned}")
        sc.timeouts()
    finally:
        worker.kill()


def join(sc, source_sha):
    extra = shared_projects(sc, worktrees=True)
    worker, sock, env = by_hand(sc, extra)
    try:
        if not serving(sc, sock, env):
            return
        rc, said = join_s(sc, sock, env, "a")
        if rc != 0:
            sc.fail(f"S did not join (rc {rc}): {said[-400:]}")
            return
        log = open(os.path.join(sc.dir, "worker.log"), errors="replace").read()
        if not any(l.startswith("join ") for l in log.splitlines()):
            sc.fail("S was not joined: the case was not exercised")
            return
        # S again, with another value of what its script watches: it runs for this request.
        rc, said = join_s(sc, sock, env, "c", within=LOST)
        sc.notes["again"] = (rc, said[-300:])
        if rc != 0:
            sc.fail(f"S asked again did not end well (rc {rc}; what its join took is not released): {said[-300:]}")
        if "c" not in sc.runs():
            sc.fail(f"the script of S did not run for GEN=c: {sc.runs()}")
        sc.timeouts()
    finally:
        worker.kill()


def edited_program(pkg):
    """`one.rs` as an editor writes it when it adds a read of the generated file in the body
    (`include_bytes!`), and prints the byte that is the value (`pub const GEN: &str = "<v>"`)."""
    text = PROGRAM.replace("@NAME@", f"{pkg}-one")
    old = 'native());'
    assert old in text
    # Only what the executable has: a patch needs the functions of std it calls to be in it
    # (`bool`'s `Display` is not), and an `i32` is printed.
    text = text.replace(
        'native={}", env!("GEN_VALUE"), native());',
        'native={} bytes={}", env!("GEN_VALUE"), native(), GENERATED.get(23).copied().unwrap_or(0) as i32);')
    return text + '\nstatic GENERATED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gen.rs"));\n'


def oracle_of(sc, files, env_extra):
    """What cargo builds of the project with `files` over it: the output of `gen-one`."""
    ws = os.path.join(sc.dir, "oracle-ws")
    shutil.rmtree(ws, ignore_errors=True)
    saved, sc.runs_log = sc.runs_log, os.path.join(sc.dir, "oracle-runs.log")
    sc.project(ws)
    sc.runs_log = saved
    for name, text in files.items():
        with open(os.path.join(ws, name), "w") as f:
            f.write(text)
    env = dict(sc.env("oracle"), **env_extra)
    env["CARGO_TARGET_DIR"] = os.path.join(sc.dir, "oracle-target")
    env.pop("RA_JIT_WHOLE_BUILD", None)
    ran = subprocess.run(["cargo", "build"], cwd=ws, env=env, capture_output=True, text=True, timeout=WAIT)
    if ran.returncode != 0:
        sc.fail(f"the oracle (cargo) did not build: {ran.stderr[-400:]}")
        return None
    return subprocess.run([os.path.join(sc.dir, "oracle-target", "debug", "gen-one")],
                          capture_output=True, text=True).stdout


def generation(sc, source_sha):
    """A reruns nothing: another worker runs the script again over a file it watches (one
    fingerprint, other output), and an editor's edit to A then reads the generated file in a
    body. What A prints has to be what cargo builds of the tree, include, variable and `cfg`
    all of the value the tree has now."""
    extra = {"GEN_ARCHIVE": "same", "GEN_FROM_FILE": "1"}
    one = ("--bin", "gen-one")
    sc.project()
    value = os.path.join(sc.ws, "value.txt")
    with open(value, "w") as f:
        f.write("a\n")
    A = sc.client("a", one, **extra)
    sc.finish(A, "A")
    if A.returncode != 0:
        sc.fail(f"A's build exited {A.returncode}: {A.err[-600:]}")
        return
    time.sleep(1.2)
    with open(value, "w") as f:
        f.write("b\n")
    # B builds the other program of the package, so that it does not write the executable that
    # A's edit is spliced into: the script is one, and runs again for it.
    B = sc.client("b", ("--bin", "gen-two"), **extra)
    sc.finish(B, "B")
    if B.returncode != 0:
        sc.fail(f"B's build exited {B.returncode}: {B.err[-600:]}")
        return
    if sc.runs() != ["a", "b"]:
        sc.fail(f"the script was to run for each of the two values: {sc.runs()}")
    text = edited_program("gen")
    want = oracle_of(sc, {"src/one.rs": text, "value.txt": "b\n"}, extra)
    said = send_edit(sc.env("a")["RA_JIT_HOME"], os.path.join(sc.ws, "src", "one.rs"), text)
    sc.notes["edit-said"] = said
    if not said.startswith("ok"):
        sc.fail(f"the edit of A was answered {said!r}: a world that moved is loaded again for it")
        return
    # The edit is spliced into the executable the worker linked, not the copy the client
    # installed, when the edits have stopped for a moment.
    time.sleep(2)
    spliced = os.path.join(sc.ws, "target", "ra-jit", "whole", "gen-one")
    got = sc.program("gen-one", spliced)
    sc.notes["edited-a"] = [got, want]
    if got is None:
        sc.fail(f"the edited program of A is not at {spliced}")
    strip = lambda it: re.sub(r"native=\d+", "native=*", it or "")
    if strip(got) != strip(want):
        sc.fail(f"A's edited program printed {got!r}, what cargo builds of the tree {want!r}")
    elif got != want:
        sc.gaps.append(f"A's edited program printed {got!r}, cargo's {want!r} (the warm worker's native link)")
    sc.notes["refused"] = [e for e in sc.events("a") if e[0] == "refused"]
    sc.timeouts()


def retire(sc, sock, build):
    """The daemon's last word about a build: every request of it is answered (`retire <build>`)."""
    import socket
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(60)
        s.connect(os.path.join(sc.dir, sock))
        s.sendall(f"retire\t{build}\n\n".encode())
        return s.makefile().readline().strip()


def in_background(fn, *args, **kwargs):
    """`fn` on a thread; the thread's `.result` is its return value once it has ended."""
    import threading
    t = threading.Thread(target=lambda: setattr(t, "result", fn(*args, **kwargs)))
    t.result = None
    t.start()
    return t


def worker_events(sc):
    return sc.events("a", open(os.path.join(sc.dir, "worker.log"), errors="replace").read())


def same_build(sc, source_sha, fails):
    """One build (an explicit `RA_JIT_BUILD`, as the daemon numbers a build and sends every request
    of it) of a worker started by hand, over the review's sequence. The build is A, and its scripts
    run every build (`ALWAYS`) with a guard that fails the second execution of a checkout's script
    for one value and one build (`ONCE_TAG`):

    1. A's first request is answered: the script of R ran for A.
    2. B, another build (another number, its tag), asks for R with another `GEN`: it has to run the
       script after A's build is over, and is put off.
    3. A's next request joins S (another checkout, a group it has not judged). Nothing may stop A:
       it does not run R again, and B has run nothing.
    4. A is retired (every request answered): B runs, and is built.

    With `fails` the script of A fails (`FAIL`) in R: the failure is A's for good (each request of
    it gets the same message) and runs once, B's build goes on meanwhile."""
    extra = shared_projects(sc, worktrees=True)
    once = os.path.join(sc.dir, "once")
    os.makedirs(once)
    # S is not guarded: the worker judges a checkout it has loaded again for every build, with the
    # variables that checkout was asked with, so B's build runs S's script again with A's. What S's
    # first run was is read from its trail.
    extra = dict(extra, ALWAYS="1", ONCE_DIR=once, ONCE_BY_DIR="1", ONCE_ONLY="r")
    oracle = {g: sc.oracle(g, {"GEN_ARCHIVE": "same"}) for g in ("a", "b")}
    sc.notes["oracle"] = oracle
    # The worker's own load has a value and a tag of its own.
    worker, sock, env = by_hand(sc, dict(extra, ONCE_TAG="load"), gen_="load")
    A, B = "1001", "2002"
    try:
        if not serving(sc, sock, dict(env, ONCE_TAG="serve")):
            return
        env_a = dict(env, ONCE_TAG="A", RA_JIT_BUILD=A, GEN="a", **({"FAIL": "1"} if fails else {}))
        rc1, said1 = sync(sc, sock, env_a, within=LOST)
        sc.notes["a1"] = (rc1, said1[-300:])
        if fails:
            if rc1 == 0:
                sc.fail(f"A's first request was to fail: {rc1} {said1[-300:]!r}")
        elif rc1 != 0:
            sc.fail(f"A's first request exited {rc1}: {said1[-400:]}")
            return
        kept_a = os.path.join(sc.dir, "a1.bin")
        if os.path.exists(os.path.join(sc.dir, "w.bin")):
            shutil.copy2(os.path.join(sc.dir, "w.bin"), kept_a)
        # B: another build, another value; it needs the output A holds (when A ran the script).
        env_b = dict(env, ONCE_TAG="B", RA_JIT_BUILD=B, GEN="b")
        s_out, s_exe = os.path.join(sc.dir, "s.o"), os.path.join(sc.dir, "s.bin")
        s_trail = os.path.join(once, "trail-s")
        trail_now = lambda: open(s_trail).read().split("\n")[:-1] if os.path.exists(s_trail) else []

        def b_build():
            # As the daemon ends a build: the worker's answer, then `retire` at once. B's build asks for
            # S as well, before it is over (a later request of one build): its own variables.
            result = sync(sc, sock, env_b, within=LOST)
            if not fails and result[0] == 0:
                # B's request for R has judged what B reads: S, which only A read, is not B's yet.
                sc.notes["s-trail-after-b-r"] = trail_now()
                sc.notes["b-s"] = sync(sc, sock, env_b, "--project", sc.s, "--crate", "gen_one", "--out", s_out,
                                       "--exe", s_exe, within=LOST)
                if os.path.exists(s_exe):
                    shutil.copy2(s_exe, os.path.join(sc.dir, "b-s.bin"))
            sc.notes["retire-b"] = retire(sc, sock, B)
            return result
        b = in_background(b_build)
        if not fails:
            deadline = time.time() + 120
            while time.time() < deadline and not any(e[0] == "put-off" and e[1] == int(B) for e in worker_events(sc)):
                time.sleep(0.2)
            sc.notes["b-put-off"] = [e for e in worker_events(sc) if e[0] == "put-off" and e[1] == int(B)][:1]
            if not sc.notes["b-put-off"]:
                sc.fail("B was not put off while A's build held the output of R")
            held = sorted(os.listdir(once))
            if any(m.endswith("-B") for m in held):
                sc.fail(f"B ran a script while A's build was not over: {held}")
        # A's next request joins S, a group it has not judged.
        s_out, s_exe = os.path.join(sc.dir, "s.o"), os.path.join(sc.dir, "s.bin")
        rc2, said2 = sync(sc, sock, env_a, "--project", sc.s, "--crate", "gen_one", "--out", s_out, "--exe", s_exe,
                          within=LOST)
        sc.notes["a2"] = (rc2, said2[-400:])
        if fails:
            # S is independent of R: A's join is judged on S's own script (which fails for A's variables
            # too) and says S; the failure of R is A's for good and is said again for R (the third).
            if rc2 == 0 or "/s)" not in said2:
                sc.fail(f"A's join was to fail with S's message: {rc2} {said2[-300:]!r}, first {said1[-300:]!r}")
        elif rc2 != 0:
            sc.fail(f"A's request that joins S exited {rc2}: {said2[-500:]}")
        # And once more, the same request as the first.
        rc3, said3 = sync(sc, sock, env_a, within=LOST)
        sc.notes["a3"] = (rc3, said3[-300:])
        if (rc3 == 0) == fails or (fails and said3 != said1):
            sc.fail(f"A's third request ended {rc3}: {said3[-300:]!r}")
        if not fails and not os.path.exists(s_out):
            sc.fail("A's join of S built no object")
        if not fails and (rc2 == 0 and rc3 == 0):
            # S's first run is the request's: GEN=a, whatever the worker was started with.
            for tag, exe, want in (("A-R", kept_a, oracle["a"]["gen-one"]), ("A-S", s_exe, oracle["a"]["gen-one"])):
                got = subprocess.run([exe], capture_output=True, text=True).stdout if os.path.exists(exe) else None
                sc.notes.setdefault("programs", {})[tag] = got
                if got is None:
                    sc.fail(f"{tag}: no program")
                elif got != want:
                    strip = lambda it: re.sub(r"native=\d+", "native=*", it or "")
                    if strip(got) == strip(want):
                        sc.gaps.append(f"{tag}: {got!r}, cargo's {want!r} (the warm worker's native link)")
                    else:
                        sc.fail(f"{tag} printed {got!r}, cargo's {want!r}")
        if not fails and b.is_alive():
            sc.notes["b-after-a-requests-alive"] = True
        sc.notes["retire-a"] = retire(sc, sock, A)
        b.join(LOST)
        if b.is_alive() or b.result is None:
            sc.fail("B (put off behind A's build) did not end after A was retired")
            return
        rcb, saidb = b.result
        sc.notes["b"] = (rcb, saidb[-300:])
        if rcb != 0:
            sc.fail(f"B exited {rcb}: {saidb[-400:]}")
        # The worker's own account: no build ran one script twice.
        runs = {}
        for e in worker_events(sc):
            if e[0] == "runs":
                runs.setdefault((e[1], e[4]), 0)
                runs[(e[1], e[4])] += 1
        sc.notes["runs-by-build"] = {f"{b} {d.rsplit('/', 1)[-1]}": n for (b, d), n in runs.items()}
        twice = {k: n for k, n in runs.items() if n > 1}
        if twice:
            sc.fail(f"a build ran a script more than once: {sc.notes['runs-by-build']}")
        ran = sorted(os.listdir(once))
        sc.notes["executions"] = ran
        want = ["r-a-A", "r-b-B"]
        have = [m for m in ran if m.endswith(("-A", "-B"))]
        if not fails:
            trail = open(os.path.join(once, "trail-s")).read().split("\n") if os.path.exists(os.path.join(once, "trail-s")) else []
            sc.notes["s-trail"] = trail
            if not trail or trail[0] != "a A":
                sc.fail(f"S's first run was to be A's request's (GEN=a, tag A): its trail is {trail}")
        if have != sorted(want):
            sc.fail(f"each script was to run once in each build ({sorted(want)}): {have}")
        if not fails and rcb == 0:
            trail_b = sc.notes.get("s-trail-after-b-r")
            if trail_b != ["a A"]:
                sc.fail(f"B's request for R was not to run S (only A read it): S's trail after it is {trail_b}")
            bs = sc.notes.get("b-s") or (None, "")
            if bs[0] != 0:
                sc.fail(f"B's request for S exited {bs[0]}: {str(bs[1])[-300:]}")
            else:
                got = subprocess.run([os.path.join(sc.dir, "b-s.bin")], capture_output=True, text=True).stdout
                sc.notes.setdefault("programs", {})["B-S"] = got
                want = oracle["b"]["gen-one"]
                strip = lambda it: re.sub(r"native=\d+", "native=*", it or "")
                if strip(got) != strip(want):
                    sc.fail(f"B-S printed {got!r}, cargo's for GEN=b {want!r}")
                elif got != want:
                    sc.gaps.append(f"B-S: {got!r}, cargo's {want!r} (the warm worker's native link)")
                if sc.notes.get("s-trail") is not None and trail_now() != ["a A", "b B"]:
                    sc.fail(f"S was to run once for A and once for B: its trail is {trail_now()}")
            got = subprocess.run([os.path.join(sc.dir, "w.bin")], capture_output=True, text=True).stdout
            sc.notes.setdefault("programs", {})["B-R"] = got
            strip = lambda it: re.sub(r"native=\d+", "native=*", it or "")
            if strip(got) != strip(oracle["b"]["gen-one"]):
                sc.fail(f"B-R printed {got!r}, cargo's {oracle['b']['gen-one']!r}")
        sc.timeouts()
    finally:
        worker.kill()
        sc.notes["worker-log-tail"] = open(os.path.join(sc.dir, "worker.log"), errors="replace").read()[-3000:]


def once_ran(sc, source_sha):
    same_build(sc, source_sha, False)


def once_failed(sc, source_sha):
    same_build(sc, source_sha, True)


def recovery(sc, source_sha):
    """A's database is of one run of a script; another worker (B) runs it again and is held after
    its scripts, before its compile and link, for longer than the worker takes turns; an editor's
    edit to A needs the world restored (A's script run again for A's own `GEN`, which B's output is not). The edit
    waits for B's build and is built when it ends, with no second request from the editor: it is
    not asked again for the same world at every turn, and not failed while B is working."""
    import threading
    extra = {"GEN_ARCHIVE": "same"}
    sc.project()
    A = sc.client("a", ("--bin", "gen-one"), GEN="a", **extra)
    sc.finish(A, "A")
    if A.returncode != 0:
        sc.fail(f"A's build exited {A.returncode}: {A.err[-600:]}")
        return
    sc.arm("b", "consume")
    B = sc.client("b", ("--bin", "gen-two"), GEN="b", **extra)
    held_b = sc.reached("b", "consume", B, within=120)
    if held_b is None:
        return
    text = edited_program("gen")
    want = oracle_of(sc, {"src/one.rs": text}, dict(extra, GEN="a"))
    answer, began = {}, time.time()
    path = os.path.join(sc.ws, "src", "one.rs")
    editor = threading.Thread(target=lambda: answer.update(said=send_edit(sc.env("a")["RA_JIT_HOME"], path, text),
                                                           at=time.time()))
    editor.start()
    sc.wait_event("a", "refused", within=120)
    # B is held for as long as it takes the worker to take many turns (it takes them in
    # milliseconds): the edit is neither answered nor asked about again and again.
    hold = float(os.environ.get("LEASEBARRIER_RECOVERY_HOLD", "12"))
    time.sleep(hold)
    refused = [e for e in sc.events("a") if e[0] == "refused"]
    sc.notes["refused-while-b-held"] = len(refused)
    sc.notes["answered-while-b-held"] = answer.get("said")
    if "said" in answer:
        sc.fail(f"the edit was answered {answer['said']!r} after {answer['at'] - began:.1f}s, while B's build was held")
    if len(refused) > 1:
        sc.fail(f"the edit's world was looked at and refused {len(refused)} times while nothing had moved")
    sc.release("b", "consume", held_b)
    sc.finish(B, "B")
    if B.returncode != 0:
        sc.fail(f"B's build exited {B.returncode}: {B.err[-600:]}")
    editor.join(WAIT)
    said = answer.get("said")
    sc.notes["edit-said"] = said
    if not str(said).startswith("ok"):
        sc.fail(f"the edit was answered {said!r} once B's build was over (with no second request)")
        return
    time.sleep(2)
    spliced = os.path.join(sc.ws, "target", "ra-jit", "whole", "gen-one")
    got = sc.program("gen-one", spliced)
    sc.notes["edited-a"] = [got, want]
    strip = lambda it: re.sub(r"native=\d+", "native=*", it or "")
    if got is None or strip(got) != strip(want):
        sc.fail(f"A's edited program printed {got!r}, what cargo builds of the tree {want!r}")
    elif got != want:
        sc.gaps.append(f"A's edited program printed {got!r}, cargo's {want!r} (the warm worker's native link)")
    sc.notes["refused"] = [e for e in sc.events("a") if e[0] == "refused"]
    sc.timeouts()


def recovery_detached(sc, source_sha):
    """The recovery of `recovery`, with a checkout that is read as another (a worktree V of S's, with
    S's script) so that every build the worker takes is detached: its link ends on a thread of its
    own and sends the loop nothing. The request that carries the recovery is held after its owner is
    released (the wake the loop takes while the recovery is under way) and before its token is gone;
    the edit has to be answered when it goes, with no other request."""
    import threading
    extra = shared_projects(sc, worktrees=True)
    v = os.path.join(sc.dir, "v")
    subprocess.run(["git", "-c", "user.name=probe", "-c", "user.email=probe@localhost", "worktree", "add", "-q", "-b",
                    "third", v], cwd=sc.r, capture_output=True, check=True)
    shutil.copy2(os.path.join(sc.s, "build.rs"), os.path.join(v, "build.rs"))
    # The script reads the value from a file it watches: one fingerprint, other output. B's run of it
    # leaves the world the recovery loads, which is not the program's: the recovery rebuilds and links.
    extra = dict(extra, RA_JIT_OVERLAY="1", GEN_FROM_FILE="1")
    for checkout in (sc.r, sc.s, v):
        with open(os.path.join(checkout, "value.txt"), "w") as f:
            f.write("a\n")
    worker, sock, env = by_hand(sc, extra, gen_="a")
    try:
        if not serving(sc, sock, env):
            return
        rc, said = join_s(sc, sock, env, "a")
        if rc != 0:
            sc.fail(f"S did not join (rc {rc}): {said[-400:]}")
            return
        rc, said = sync(sc, sock, env, "--project", v, "--crate", "gen_one", "--out", os.path.join(sc.dir, "v.o"),
                        "--exe", os.path.join(sc.dir, "v.bin"), within=LOST)
        log = open(os.path.join(sc.dir, "worker.log"), errors="replace").read()
        sc.notes["view-lines"] = [l for l in log.splitlines() if l.startswith("view:")]
        if rc != 0 or not sc.notes["view-lines"]:
            sc.fail(f"V was not read as S (rc {rc}): the case was not exercised: {said[-300:]}")
            return
        # Another process runs R's script over the file, changed: the worker's world of R moves.
        time.sleep(1.2)
        with open(os.path.join(sc.r, "value.txt"), "w") as f:
            f.write("b\n")
        B = sc.client("b", ("--bin", "gen-two"), cwd=sc.r, **extra)
        sc.finish(B, "B", within=LOST)
        if B.returncode != 0:
            sc.fail(f"B's build exited {B.returncode}: {B.err[-600:]}")
            return
        sc.arm("a", "released")
        path = os.path.join(sc.r, "src", "one.rs")
        text = open(path).read() + "// an edit\n"
        answer = {}
        editor = threading.Thread(target=lambda: answer.update(said=send_edit(sc.dir, path, text, "worker.sock")))
        editor.start()
        held = sc.reached("a", "released", None, within=120)
        if held is None:
            return
        sc.disarm("a", "released")
        # The loop takes the wake the release made, finds the recovery under way, and sleeps.
        time.sleep(float(os.environ.get("LEASEBARRIER_RECOVERY_HOLD", "8")) / 2)
        if "said" in answer:
            sc.fail(f"the edit was answered {answer['said']!r} while its recovery's request was held")
        sc.release("a", "released", held)
        editor.join(LOST)
        sc.notes["edit-said"] = answer.get("said")
        if editor.is_alive() or not str(answer.get("said")).startswith("ok"):
            sc.fail(f"the edit was not answered ok after its recovery ended, with no other request: {answer.get('said')!r}")
        sc.notes["refused"] = [e for e in worker_events(sc) if e[0] == "refused"]
        sc.timeouts()
    finally:
        worker.kill()


def first_join(sc, source_sha, shared, private=False):
    """A worker started by hand with R loaded (`GEN=a`), whose script is valid for `a` and fails for
    `b` and keeps a trail of its runs. ONE first request for a project that is not loaded, with `GEN=b`
    and a build number of its own, no wake and no restart.
    Independent (`shared` false): S, another package with a script of its own that R does not
    consume: S is built, the way cargo builds it, and R's script is not run for `b`.
    Shared (`shared`): V, a checkout of R's repository with the same scripts: it reads R's, so R's
    script IS judged for `b` (once, the trail says so) and V is built from it, as cargo builds it."""
    once = os.path.join(sc.dir, "once")
    os.makedirs(once)
    extra = shared_projects(sc, worktrees=shared)
    extra = dict(extra, ONCE_DIR=once, ONCE_ONLY="none", ONCE_BY_DIR="1")
    if private:
        # V is file-alike to R (the same bytes) and read through R by neither policy: its script is its
        # own, and fails for `b` in R's directory only.
        extra["FAIL_DIR_VALUE"] = "r:b"
        extra["RA_JIT_SPLIT"] = "0"
        extra["RA_JIT_OVERLAY"] = "0"
    if shared:
        v = os.path.join(sc.dir, "v")
        subprocess.run(["git", "-c", "user.name=probe", "-c", "user.email=probe@localhost", "worktree", "add", "-q",
                        "-b", "third", v], cwd=sc.r, capture_output=True, check=True)
        target, crate, pkg = v, "gen_one", "gen"
    else:
        extra["FAIL_PKG_VALUE"] = "gena:b"
        target, crate, pkg = sc.s, "genb_one", "genb"
    oracle = sc.oracle("b", {"GEN_ARCHIVE": "same"}, pkg=pkg)
    worker, sock, env = by_hand(sc, dict(extra, ONCE_TAG="load"), gen_="a", crate="gen_one" if shared else "gena_one")
    try:
        if not serving(sc, sock, dict(env, ONCE_TAG="serve")):
            return
        r_trail = os.path.join(once, "trail-r")
        trail = lambda: open(r_trail).read().split("\n")[:-1] if os.path.exists(r_trail) else []
        before = trail()
        sc.notes["r-trail-before"] = before
        out, exe = os.path.join(sc.dir, "first.o"), os.path.join(sc.dir, "first.bin")
        env_b = dict(env, ONCE_TAG="B", RA_JIT_BUILD="3001", GEN="b")
        rc, said = sync(sc, sock, env_b, "--project", target, "--crate", crate, "--out", out, "--exe", exe,
                        within=LOST)
        sc.notes["first"] = (rc, said[-300:])
        after = trail()
        sc.notes["r-trail-after"] = after
        log = open(os.path.join(sc.dir, "worker.log"), errors="replace").read()
        sc.notes["failed-lines"] = [l[:200] for l in log.splitlines() if "failed:" in l and "build script" in l]
        if private:
            v_trail = os.path.join(once, "trail-v")
            sc.notes["v-trail"] = open(v_trail).read().split("\n")[:-1] if os.path.exists(v_trail) else []
            if after != before:
                sc.fail(f"R's script was run for the request of V, which neither policy reads through R: {before} -> {after}")
            if rc != 0:
                sc.fail(f"V (its own script succeeds) was not built (rc {rc}): {said[-300:]}")
            elif sc.notes["v-trail"] != ["b B"]:
                sc.fail(f"V's own script was to run once, for b: its trail is {sc.notes['v-trail']}")
            else:
                got = subprocess.run([exe], capture_output=True, text=True).stdout if os.path.exists(exe) else None
                want = oracle["gen-one"]
                sc.notes.setdefault("programs", {})["V"] = got
                strip = lambda it: re.sub(r"native=\d+", "native=*", it or "")
                if strip(got) != strip(want):
                    sc.fail(f"V printed {got!r}, cargo's {want!r}")
                elif got != want:
                    sc.gaps.append(f"V: {got!r}, cargo's {want!r} (the warm worker's native link)")
            sc.notes["retire"] = retire(sc, sock, "3001")
            return
        if shared:
            # V reads R's scripts: R is judged for `b`, once, and V is built from it.
            if after[len(before):] != ["b B"]:
                sc.fail(f"V shares R's scripts: R's script was to run once for b: its trail is {after[len(before):]}")
            if rc != 0:
                sc.fail(f"V was not built (rc {rc}): {said[-300:]}")
            elif not os.path.exists(out):
                sc.fail("V was read as R and no object was built")
            # V is read as R: the program is R's, linked by the worker; what it prints is recorded.
            w = os.path.join(sc.dir, "w.bin")
            sc.notes["r-program"] = subprocess.run([w], capture_output=True, text=True).stdout if os.path.exists(w) else None
            sc.notes["retire"] = retire(sc, sock, "3001")
            return
        if rc != 0:
            sc.fail(f"S (independent of R) was not built (rc {rc}): {said[-300:]}")
        if after != before:
            sc.fail(f"R's script was run for the request of S: its trail went from {before} to {after}")
        if rc == 0:
            got = subprocess.run([exe], capture_output=True, text=True).stdout if os.path.exists(exe) else None
            want = oracle["genb-one"]
            sc.notes.setdefault("programs", {})["S"] = got
            strip = lambda it: re.sub(r"native=\d+", "native=*", it or "")
            if strip(got) != strip(want):
                sc.fail(f"S printed {got!r}, cargo's {want!r}")
            elif got != want:
                sc.gaps.append(f"S: {got!r}, cargo's {want!r} (the warm worker's native link)")
        sc.notes["retire"] = retire(sc, sock, "3001")
        sc.timeouts()
    finally:
        worker.kill()


def first_join_independent(sc, source_sha):
    first_join(sc, source_sha, False)


def first_join_shared(sc, source_sha):
    first_join(sc, source_sha, True)


def first_join_private(sc, source_sha):
    first_join(sc, source_sha, True, private=True)


def quick_choice(sc, source_sha):
    """`RA_JIT_SPLIT=0` and the default (quick) overlay; R loaded `GEN=a`; V file-alike to R with the same
    script, which succeeds for `a` and `b` and writes its value into the program. A request for R under
    `GEN=a` is relinking in the background (the executable was taken away) and is held as it enters the
    link; V's first request, `GEN=b`, arrives then. R is no overlay candidate while it links, so nothing of
    R is judged for V; the worker settles (it waits for the link) and turns to `b`, and when the link is
    let go R is quick again. V has to be loaded on its own and judged for `b`, not read through R's `a`."""
    import threading
    once = os.path.join(sc.dir, "once")
    os.makedirs(once)
    extra = shared_projects(sc, worktrees=True)
    v = os.path.join(sc.dir, "v")
    subprocess.run(["git", "-c", "user.name=probe", "-c", "user.email=probe@localhost", "worktree", "add", "-q",
                    "-b", "third", v], cwd=sc.r, capture_output=True, check=True)
    extra = dict(extra, ONCE_DIR=once, ONCE_ONLY="none", ONCE_BY_DIR="1", RA_JIT_SPLIT="0")
    oracle = sc.oracle("b", {"GEN_ARCHIVE": "same"})
    worker, sock, env = by_hand(sc, dict(extra, ONCE_TAG="load"), gen_="a")
    try:
        if not serving(sc, sock, dict(env, ONCE_TAG="serve")):
            return
        r_trail, v_trail = os.path.join(once, "trail-r"), os.path.join(once, "trail-v")
        trail = lambda f: open(f).read().split("\n")[:-1] if os.path.exists(f) else []
        before = trail(r_trail)
        # The executable is taken away: the next request for R links again.
        os.remove(os.path.join(sc.dir, "w.bin"))
        sc.arm("a", "linking")
        said = {}
        def r_build():
            # As the daemon ends a build: the worker's answer, then `retire` (R's request ran R's script: its
            # build holds the scripts' admission until then).
            said["r"] = sync(sc, sock, dict(env, ONCE_TAG="A", RA_JIT_BUILD="4001"), within=LOST)
            said["retire-r"] = retire(sc, sock, "4001")

        r_req = threading.Thread(target=r_build)
        r_req.start()
        held = sc.reached("a", "linking", None, within=120)
        if held is None:
            return
        sc.disarm("a", "linking")
        out, exe = os.path.join(sc.dir, "first.o"), os.path.join(sc.dir, "first.bin")
        env_b = dict(env, ONCE_TAG="B", RA_JIT_BUILD="4002", GEN="b")
        v_req = threading.Thread(target=lambda: said.update(v=sync(sc, sock, env_b, "--project", v, "--crate",
                                                                    "gen_one", "--out", out, "--exe", exe, within=LOST)))
        v_req.start()
        # The worker settles on R's link: V's request waits for it.
        time.sleep(4)
        sc.notes["v-waited-for-the-link"] = v_req.is_alive()
        if not v_req.is_alive():
            sc.fail("V's request was not waiting for R's link: the case was not exercised")
        sc.release("a", "linking", held)
        r_req.join(LOST)
        v_req.join(LOST)
        sc.notes["r-request"], sc.notes["v-request"] = said.get("r"), said.get("v")
        log = open(os.path.join(sc.dir, "worker.log"), errors="replace").read()
        sc.notes["view-lines"] = [l for l in log.splitlines() if l.startswith("view:")]
        sc.notes["r-trail"], sc.notes["v-trail"] = trail(r_trail), trail(v_trail)
        if not said.get("r") or said["r"][0] != 0:
            sc.fail(f"R's request (the link that was held) did not end well: {said.get('r')}")
        rv = said.get("v")
        if not rv or rv[0] != 0:
            sc.fail(f"V's request did not end well: {rv}")
        if sc.notes["view-lines"]:
            sc.fail(f"V was read through R, whose scripts were not judged for b: {sc.notes['view-lines']}")
        if any(line.startswith("b ") for line in trail(r_trail)):
            sc.fail(f"R's script ran for b, the request of V's variables: {trail(r_trail)}")
        if sc.notes["v-trail"] != ["b B"]:
            sc.fail(f"V's own script was to run once, for b: its trail is {sc.notes['v-trail']}")
        got = subprocess.run([exe], capture_output=True, text=True).stdout if os.path.exists(exe) else None
        sc.notes["programs"] = {"V": got}
        want = oracle["gen-one"]
        strip = lambda it: re.sub(r"native=\d+", "native=*", it or "")
        if strip(got) != strip(want):
            sc.fail(f"V printed {got!r}, cargo's {want!r}")
        elif got != want:
            sc.gaps.append(f"V: {got!r}, cargo's {want!r} (the warm worker's native link)")
        sc.notes["retire"] = retire(sc, sock, "4002")
        sc.timeouts()
    finally:
        worker.kill()


def unrelated(sc, source_sha):
    """A worker has R and S loaded, and another worker runs S's script again. An edit to R,
    which reads nothing of S's, is built: S's moved world is not R's to wait for or to refuse."""
    extra = shared_projects(sc, worktrees=True)
    worker, sock, env = by_hand(sc, extra)
    try:
        if not serving(sc, sock, env):
            return
        rc, said = join_s(sc, sock, env, "a")
        if rc != 0:
            sc.fail(f"S did not join (rc {rc}): {said[-400:]}")
            return
        B = sc.client("b", ("--bin", "gen-one"), cwd=sc.s, GEN="b", **extra)
        sc.finish(B, "B (S)")
        if B.returncode != 0:
            sc.fail(f"B's build in S exited {B.returncode}: {B.err[-400:]}")
            return
        if sc.runs().count("b") != 1:
            sc.fail(f"the script of S was to run once for B: {sc.runs()}")
        path = os.path.join(sc.r, "src", "one.rs")
        text = open(path).read() + "// an edit\n"
        said = send_edit(sc.dir, path, text, "worker.sock")
        sc.notes["edit-said"] = said
        if not said.startswith("ok"):
            sc.fail(f"the edit of R was answered {said!r}: S's world moved, and R reads nothing of S's")
        sc.timeouts()
    finally:
        worker.kill()


def link_control(sc, source_sha):
    """No concurrency, no barrier: GEN=a, then GEN=b, a static library named for the value. Not
    about leases: whether a warm worker, which finds only the script's output changed, links the
    program with the library that output names."""
    sc.project()
    expected = {"a": sc.oracle("a"), "b": sc.oracle("b")}
    sc.project()
    for gen_ in ("a", "b"):
        p = sc.client("a", GEN=gen_)
        sc.finish(p, gen_)
        if p.returncode != 0:
            sc.fail(f"the build of GEN={gen_} exited {p.returncode}: {p.err[-600:]}")
            return
        for name in PROGRAMS:
            got = sc.program(name)
            if got != (expected[gen_] or {}).get(name):
                sc.gaps.append(f"GEN={gen_}: {name} printed {got!r}, cargo's {(expected[gen_] or {}).get(name)!r}")
    sc.notes["gap-count"] = len(sc.gaps)


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
    "lost-wake": lost_wake,
    "reload-busy": reload_busy,
    "direct": direct,
    "coab": coab,
    "internal": internal,
    "groups": groups,
    "generation": generation,
    "unrelated": unrelated,
    "once": once_ran,
    "once-failed": once_failed,
    "recovery": recovery,
    "recovery-detached": recovery_detached,
    "first-join": first_join_independent,
    "first-join-shared": first_join_shared,
    "first-join-private": first_join_private,
    "quick-choice": quick_choice,
    "overlay": overlay,
    "worlds": worlds,
    "join": join,
    "archive-control": archive_control,
    "link-control": link_control,
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
    names = args.scenarios or [n for n in SCENARIOS if not n.endswith("-control")] + ["link-control", "archive-control"]
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
