#!/usr/bin/env python3
"""tracedecay.py: blaze against cargo on tracedecay's Rust workspace, on one machine, and the
harness of the Phase 0 predicates of the distributed program (docs/distributed.md, unit P0.1).

    tracedecay.py --td DIR [--blaze-bin DIR] [--work DIR] COMMAND [options]
    tracedecay.py --self-test
    tracedecay.py --td DIR --predicate P0-x [--expect-fail] [options]

    --self-test                       the guard's three rules, each on a synthetic allocator
    --predicate P0-A|P0-B|...|P0-H|all
                                      run that predicate's scenarios; exit 0 only if its bar holds
                                      (--expect-fail: exit 0 only if it does not, and say why)
    prepare                           the three one-time steps below, each skipped when done
    versions                          what is being measured (commits, toolchains, machine)
    cold   --tool T [--runs N]        a build of the CLI from an empty target directory
    noop   --tool T [--runs N]        a build with nothing changed, after a cold one
    edit   --tool T --edit E [--runs N]
                                      N edits of one kind, each built; then the edit is reverted
    test   --tool T --package P [--runs N] [--cold]
                                      `test -p P` (cold: from an empty target directory)
    status --tool T                   `blaze status` for the tool's daemon
    stop   --tool T                   stop the tool's daemon and list the blaze processes left
    watch  --tool T [--seconds N]     print the footprint of this blaze's processes every 2 s, under the watchdog
    summary FILE...                   median, min, max of each (tool, scenario) in the results

Memory guard (every blaze run; it cannot be turned off): blaze's documented memory bounds do not
apply to a worker that is loading or to the one asked last (docs/reports/TRACEDECAY.md, item 7),
and loading tracedecay took a worker past 56 GB on a 48 GiB Mac. So every blaze command runs with
RA_JIT_MEMORY_MAX=16G, RA_JIT_OWN_BASE_MEMORY=12G and RA_JIT_SPARE=0, and a watchdog thread, every
--interval seconds (2 by default), sums the physical footprint (macOS `footprint`, which counts
compressed and swapped-out memory; RSS does not; elsewhere RSS) of the run's whole process tree:
every process of the run's blaze, the daemon and workers it started, and what those started,
found by the binary directory, by the `RA_JIT_HOME` of this harness's daemons (so a daemon an
earlier run started is still this harness's) and by `RA_JIT_RUN_ID`, which is set on every child
of the run, so a daemon reparented to pid 1 or a child started outside the binary's directory is
still counted; a process that says it belongs to another run or another home is not. It kills
them all, and the client, when the sum passes --watchdog-mb (default 24576), or when swap has
grown by 1 GB and less than --swap-free-mb (default 3072) is free, or swap has grown by 8 GB. The
kill is a result: "outcome": "watchdog-killed" in results.jsonl, with the reason and the numbers,
and the footprint of every sample in logs/watchdog-series.jsonl. Every record also has
"rajit_footprint_peak_mb".

No flag turns the guard off, and no flag raises one of its limits: --memory-max, --own-base-memory
and the watchdog's limits may only be *lowered*, for a test (`--memory-max 2G`, `--watchdog-mb
512`), which is how --self-test proves each rule fires. `--self-test` runs a synthetic tree (never
blaze) under the watchdog at a 512 MB limit, its two swap rules on a scripted swap series (and the
free-space rule not firing when the same growth leaves room), checks that another run's memory is
not counted, and refuses the flags that would weaken the guard. It writes `selftest.json` to
`--work`, or to `$TMPDIR/ra-jit-tracedecay-selftest`.

The audit (P0-A): `norustc.py`'s stand-in `rustc`, `rustdoc` and `cargo` are on `PATH` and in
`RUSTC`, `RUSTDOC` and `CARGO` for every blaze run, and each run of one appends a line with the
tool, its pid, its parent's command line, its argv and the run it belongs to, then runs the real
tool. The audit of a scenario is the lines that appeared while it ran; `--predicate P0-A` fails
naming each compiler run. The cargo and cargo199 baselines are not audited: they are the
reference, not the subject, and they run as their developers run them.

Predicates (`--predicate`), each as section 0 of docs/distributed.md states its bar. The bars are
what units P0.2 to P0.8 close; this harness runs their scenarios and says which part failed. A
predicate that needs a project other than --td's, or a platform this is not, says so and exits 2.
A record of every run goes to --work/results.jsonl, as before.

Setup (`prepare` does 2 to 4; the checkout is a detached `git worktree` of tracedecay at the
commit measured, with `pnpm install --frozen-lockfile` run in it for the dashboard its CLI embeds):
    1. `pnpm install` also writes .pnpm/crates/config.toml, which replaces every crates.io and git
       source with a directory of symlinks into pnpm's store. Cargo then hashes every file of every
       crate it builds at each start; with a virus scanner opening files that took one to four
       minutes. The file is moved aside, so the registry and git caches of CARGO_HOME are used.
    2. A private CARGO_HOME is cloned from ~/.cargo (copy on write).
    3. Rust 1.99.0, the compiler blaze builds with, stops tracedecay-code-index-runtime with
       "queries overflow the depth limit"; the line `#![recursion_limit = "256"]` at the top of its
       lib.rs fixes it (two other crates of the workspace have it). All tools build the patched tree.
    4. The checkout must not be edited by anything else.

Tools (T):
    cargo         cargo with the toolchain the project pins (nightly-2026-10-18), as its developers run it
    cargo199      cargo with rustc 1.99.0, the compiler blaze builds with
    blaze         `blaze build`: eligible programs use the owned whole build by default
    blaze-whole   the historical whole-build label; now the same owned route as blaze
    blaze-nightly `BLAZE_OTHER_TOOLCHAIN=1`: blaze builds with the project's own toolchain

Each tool builds into a directory of its own below the tracedecay checkout's `target/`, the one
directory its .gitignore names. That matters: the CLI's build script watches every file git does
not ignore, so a target directory anywhere else in the checkout changes what the script prints on
every build, and blaze refuses to trust a script that prints something else on a second run. The
directories end in `.noindex` so that Spotlight does not index them. Blaze's daemon directory
and store are under --work (default: `<td>/target/.tracedecay.noindex`), one pair per tool, so no
two tools share state. Every tool uses one private CARGO_HOME (--work/cargo-home, a copy of
~/.cargo/registry and ~/.cargo/git), so a cargo run by someone else on the machine does not hold
the package cache lock this one waits on.

Edits (E):
    tracedecay-body  a number in `daemon_project_status`, an async function with
                     `#[tracing::instrument]`, in crates/tracedecay
    mcp-body         the cap in `bounded_failure_reason`, in crates/tracedecay-mcp
    signature        a parameter added to `database_recovery_guidance` in crates/tracedecay, its
                     type changed on each run
Each run applies the next step of the chain to the previous one, so every run is a real change; the
file is restored with `git checkout` at the end and built once more (`...-revert`).

Every run writes one line of JSON to --work/results.jsonl: wall seconds, the exit status, the outcome
(ok, failed or watchdog-killed), the line
the tool finished with, the largest single process (`/usr/bin/time -l`), the most memory the
build's process tree and blaze's daemon and workers held together at a one-second sample (RSS: it
leaves out memory swapped out; `blaze_footprint_mb_after` is macOS `footprint` of blaze's processes
after the run, which counts it), the swap in use, and what else was using the machine: cores of other
builds (`foreign_build_cores`, mean and peak) and of the security scanner and tracedecay's own
daemon (`noise_cores`). A run in which `foreign_build_cores` peaked above 1 shared the machine with
another build and is labelled contended in the report.

Python 3 standard library only. Runs on macOS and Linux; the memory sampling reads `ps`.
"""
import argparse, json, os, platform, re, shutil, signal, statistics, subprocess, sys, tempfile, threading, time

# `python3 -I` (which the verify lines use) leaves the script's directory out of sys.path, and the
# stand-ins are `norustc.py`'s.
HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)
import norustc
if os.path.join(HERE, "dist") not in sys.path:
    sys.path.insert(0, os.path.join(HERE, "dist"))
import fetch

CLI_PACKAGE = "tracedecay-cli"
TOOLS = ("cargo", "cargo199", "blaze", "blaze-whole", "blaze-nightly")
PREDICATES = ("P0-A", "P0-B", "P0-C", "P0-D", "P0-E", "P0-F", "P0-G", "P0-H")

# Processes that are the machine's own noise on the Mac this was written on.
NOISE = re.compile(r"wdavdaemon|epsext|tracedecay daemon run|WindowServer|CorpLink|flinco|kernel_task|mds_stores|Spotlight")
# Other people's builds: what makes a timing contended.
BUILDERS = re.compile(r"/(rustc|cargo|cc1|clang|clang\+\+|ld|ld64|cc|c\+\+|bazel|java|node|pnpm|rustup)( |$)|ra-jit|rust-lld|buildscript|build-script")


def sh(args, check=False, **kw):
    kw.setdefault("text", True)
    kw.setdefault("stdout", subprocess.PIPE)
    kw.setdefault("stderr", subprocess.STDOUT)
    r = subprocess.run(args, **kw)
    if check and r.returncode:
        sys.exit(f"{args[0]} failed ({r.returncode}): {r.stdout}")
    return r


def parse_mb(text):
    """`16G`, `24576`, `2G` and `512M` as MB of 1,024 KB. Raises ValueError."""
    m = re.fullmatch(r"\s*(\d+(?:\.\d+)?)\s*([KMG]?)[Bb]?\s*", str(text))
    if not m:
        raise ValueError(f"{text!r} is not a size (16G, 512M, 24576)")
    return float(m.group(1)) * {"": 1 / 1024, "K": 1 / 1024, "M": 1, "G": 1024}[m.group(2)]


MB = 1
GB = 1024 * MB

# Every blaze command runs with these (the daemon reads them once, when it starts). Workers together
# hold at most MEMORY_MAX, and a worker building its own base at most OWN_BASE_MEMORY. Neither bounds
# a worker that is loading or the one asked last (docs/reports/TRACEDECAY.md, item 5), which is what
# the watchdog below is for. These are the guard's values (standing order 14): no flag raises them.
GUARD_ENV = {"RA_JIT_MEMORY_MAX": "16G", "RA_JIT_OWN_BASE_MEMORY": "12G", "RA_JIT_SPARE": "0"}
WATCHDOG_MB = 24 * GB
SWAP_FREE_MIN_MB = 3 * GB
SWAP_GROWTH_MIN_MB = 1 * GB
SWAP_GROWTH_MAX_MB = 8 * GB
# The variable set on every child of a run, so that what the run started is found however it was
# reparented (a daemon outlives the client that started it).
RUN_ID_ENV = norustc.RUN_ID_ENV
# What `ps` shows of a process's environment: `NAME=value`, appended after the command line.
ENV_TOKEN = re.compile(r"(?:^|\s)(RA_JIT_RUN_ID|RA_JIT_HOME)=(\S*)")


class Ctx:
    def __init__(self, a):
        self.td = os.path.abspath(a.td)
        self.blaze_bin = os.path.abspath(a.blaze_bin)
        self.work = os.path.abspath(a.work)
        os.makedirs(self.work, exist_ok=True)
        self.results = os.path.join(self.work, "results.jsonl")
        self.logs = os.path.join(self.work, "logs")
        os.makedirs(self.logs, exist_ok=True)
        self.host = sh(["rustc", "+1.99.0", "-vV"]).stdout
        self.triple = re.search(r"host: (\S+)", self.host).group(1)
        self.rustc199 = "1.99.0-" + self.triple
        self.bootstrap = a.rustc_bootstrap
        self.guard = dict(GUARD_ENV)
        if a.memory_max:
            self.guard["RA_JIT_MEMORY_MAX"] = a.memory_max
        if a.own_base_memory:
            self.guard["RA_JIT_OWN_BASE_MEMORY"] = a.own_base_memory
        self.watchdog = None
        self.run_id = "%s-%d" % (time.strftime("%m%d%H%M%S"), os.getpid())
        self.audit_log = os.path.join(self.work, "audit.log")
        self._standins = None

    def target(self, tool):
        return os.path.join(self.td, "target", tool + ".noindex")

    def blaze_home(self, tool):
        return os.path.join(self.work, "home-" + tool + ".noindex")

    def store(self, tool):
        return os.path.join(self.work, "store-" + tool + ".noindex")

    def standins(self):
        """The `bin` directory of the stand-in rustc, rustdoc and cargo of every blaze run of this
        work directory, built once: a copy of Rust 1.99.0 whose three counted programs log to
        `audit.log` first."""
        if self._standins is None:
            os.makedirs(os.path.join(self.work, "standins"), exist_ok=True)
            self._standins = os.path.join(norustc.stand_ins(os.path.join(self.work, "standins"), self.audit_log), "bin")
        return self._standins

    def env(self, tool):
        e = dict(os.environ)
        e["RUST_BACKTRACE"] = "0"
        e["CARGO_HOME"] = os.path.join(self.work, "cargo-home")
        e["CARGO_TARGET_DIR"] = self.target(tool)
        e.pop("RUSTUP_TOOLCHAIN", None)
        if tool != "cargo":
            e["RUSTUP_TOOLCHAIN"] = self.rustc199
        if self.bootstrap and tool != "cargo":
            e["RUSTC_BOOTSTRAP"] = "1"
        if tool.startswith("blaze"):
            standins = self.standins()
            e["PATH"] = standins + os.pathsep + self.blaze_bin + os.pathsep + e["PATH"]
            for name, exe in (("RUSTC", "rustc"), ("RUSTDOC", "rustdoc"), ("CARGO", "cargo")):
                e[name] = os.path.join(standins, exe)
            e["RA_JIT_HOME"] = self.blaze_home(tool)
            e["RA_JIT_CACHE"] = self.store(tool)
            e.update(self.guard)
            e[RUN_ID_ENV] = self.run_id
            e.pop("RUSTUP_TOOLCHAIN", None)  # blaze names 1.99.0 itself
            if self.bootstrap:
                e["RUSTC_BOOTSTRAP"] = "1"
            if tool == "blaze-nightly":
                e["BLAZE_OTHER_TOOLCHAIN"] = "1"
        return e

    def exe(self, tool):
        return "blaze" if tool.startswith("blaze") else "cargo"

    def binary(self, tool):
        return os.path.join(self.target(tool), "debug", "tracedecay")

    def homes(self):
        """The `RA_JIT_HOME` values of the blaze tools of this work directory: a daemon this
        harness started in an earlier run of the same directory names one."""
        return [self.blaze_home(tool) for tool in TOOLS if tool.startswith("blaze")]

    def audit_offset(self):
        try:
            return os.path.getsize(self.audit_log)
        except OSError:
            return 0

    def audit_since(self, offset):
        """The counted runs of rustc, rustdoc and cargo appended to the audit log since `offset`."""
        try:
            with open(self.audit_log, "rb") as f:
                f.seek(offset)
                data = f.read()
        except OSError:
            return []
        path = self.audit_log + ".slice"
        with open(path, "wb") as f:
            f.write(data)
        try:
            return norustc.runs(path)
        finally:
            os.unlink(path)


class ProcessSamplingError(RuntimeError):
    """The process table cannot safely identify this harness's processes."""


def ps_table(env=True):
    """pid, ppid, rss (KB) and command line; with `env`, the process's environment is appended to
    the command line, which is how a run's processes are found after they were reparented
    (macOS `ps -Eww`, Linux `ps eww`; both need the wide form, or the environment is cut off).
    The wide form costs about 20 ms more than a plain `ps` on this Mac, which is why the
    one-second sampler asks without it."""
    system = platform.system()
    if system == "Linux":
        # Some sandboxes expose the host's procfs from a child PID namespace. `ps`
        # then either fails looking up itself or returns host PIDs that can collide
        # with unrelated local PIDs. Such rows cannot safely be measured or killed.
        try:
            proc_pid = int(os.readlink("/proc/self"))
        except (OSError, ValueError):
            raise ProcessSamplingError("process sampler unavailable: cannot identify Linux /proc/self PID") from None
        if proc_pid != os.getpid():
            raise ProcessSamplingError("process sampler unavailable: Linux /proc uses a different PID namespace")
    flags = ["-Eww" if env else "-ww", "-Ao"] if system == "Darwin" else (["eww", "-Ao"] if env else ["-Ao"])
    # A process environment can contain native Unix bytes. Losing that one
    # sample disables the watchdog precisely while native-value tests run.
    try:
        ran = sh(["ps"] + flags + ["pid=,ppid=,rss=,command="],
                 errors="surrogateescape", stderr=subprocess.PIPE)
    except OSError:
        raise ProcessSamplingError("process sampler unavailable: could not run ps") from None
    if ran.returncode:
        # Do not put either output stream in an error: process environments can
        # contain secrets, and a failing ps is not an empty process table.
        raise ProcessSamplingError(f"process sampler unavailable: ps exited with status {ran.returncode}")
    rows = []
    for line in ran.stdout.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 4:
            continue
        try:
            rows.append((int(parts[0]), int(parts[1]), int(parts[2]), parts[3]))
        except ValueError:
            pass
    if not any(pid == os.getpid() for pid, _, _, _ in rows):
        raise ProcessSamplingError("process sampler unavailable: ps omitted the current process")
    return rows


def env_of(cmd):
    """The `RA_JIT_RUN_ID` and `RA_JIT_HOME` values `ps -Eww` shows for a process, if any."""
    found = {}
    for name, value in ENV_TOKEN.findall(cmd):
        found[name] = value
    return found


def swap_used_mb():
    out = sh(["sysctl", "-n", "vm.swapusage"]).stdout
    m = re.search(r"used = ([\d.]+)M", out)
    return float(m.group(1)) if m else 0.0


def blaze_footprint_mb(prefix):
    """The physical footprint (macOS `footprint`, which counts memory swapped out or compressed)
    of blaze's processes now, in MB. RSS leaves that memory out."""
    pids = [r[0] for r in ps_table() if r[3].startswith(prefix)]
    total = 0.0
    for pid in pids:
        out = sh(["footprint", "-p", str(pid), "--noCategories"]).stdout
        m = re.search(r"phys_footprint: ([\d.]+) (KB|MB|GB)", out)
        if m:
            total += float(m.group(1)) * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[m.group(2)]
    return round(total)


def swap_mb():
    """(used, free) swap in MB. macOS grows its swap files on demand, so `free` is what the files
    that exist have left, not what the disk could give; Linux reads /proc/meminfo."""
    if platform.system() == "Darwin":
        out = sh(["sysctl", "-n", "vm.swapusage"]).stdout
        used = re.search(r"used = ([\d.]+)M", out)
        free = re.search(r"free = ([\d.]+)M", out)
        return (float(used.group(1)) if used else 0.0, float(free.group(1)) if free else 0.0)
    try:
        info = dict(re.findall(r"(\w+):\s+(\d+) kB", open("/proc/meminfo").read()))
        total, free = int(info["SwapTotal"]) / 1024, int(info["SwapFree"]) / 1024
        return (total - free, free)
    except (OSError, KeyError):
        return (0.0, float("inf"))


def footprints_mb(pids):
    """pid -> physical footprint in MB. macOS `footprint` counts memory that is compressed or
    swapped out, which RSS leaves out and which was most of the 56 GB; elsewhere RSS."""
    if not pids:
        return {}
    if shutil.which("footprint"):
        args = ["footprint", "--noCategories"]
        for pid in sorted(pids):
            args += ["-p", str(pid)]
        out = sh(args).stdout
        found = {}
        for m in re.finditer(r"\[(\d+)\]: \S+\s+Footprint: ([\d.]+) (KB|MB|GB)", out):
            found[int(m.group(1))] = float(m.group(2)) * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[m.group(3)]
        return found
    return {r[0]: r[2] / 1024 for r in ps_table(env=False) if r[0] in pids}


def descendants(rows, roots):
    kids = {}
    for pid, ppid, _, _ in rows:
        kids.setdefault(ppid, []).append(pid)
    seen, todo = set(), list(roots)
    while todo:
        p = todo.pop()
        if p not in seen:
            seen.add(p)
            todo.extend(kids.get(p, []))
    return seen


def kill_pids(pids, sig=signal.SIGKILL):
    for pid in pids:
        try:
            os.kill(pid, sig)
        except OSError:
            pass


class Watchdog(threading.Thread):
    """Every `interval` seconds, the summed footprint of the run's whole process tree, and the
    largest process of it. A process is the run's when:

    - it carries `RA_JIT_RUN_ID` of this run (every child of the run has it, so a daemon it
      started and left reparented to pid 1, or a child started outside the binary's directory, is
      still counted), or
    - it carries the `RA_JIT_HOME` of one of this harness's daemons (a daemon an earlier run of
      the same work directory started), or
    - its executable is in `blaze_bin`, and it says it belongs to no other run or home.

    It kills them all, and the client's tree, when the sum passes `limit_mb`, or when swap has
    grown by SWAP_GROWTH_MIN_MB or more since the watchdog started and fewer than
    `swap_free_min_mb` of it is left (or swap grew by `swap_growth_max_mb`, free or not). The kill
    is an outcome, not an error: it is in `tripped` and `take()` hands it to the run's record.

    Swap `free` alone cannot be the rule on macOS: its swap files grow on demand, and on this Mac
    it was 0.9 of 8.2 GB before any run started, from the previous run's pages."""

    def __init__(self, blaze_bin, limit_mb=WATCHDOG_MB, swap_free_min_mb=SWAP_FREE_MIN_MB, interval=2.0,
                 series=None, run_id=None, homes=(), swap_fn=swap_mb, swap_growth_min_mb=SWAP_GROWTH_MIN_MB,
                 swap_growth_max_mb=SWAP_GROWTH_MAX_MB, process_env=True):
        super().__init__(daemon=True)
        self.series = open(series, "a", buffering=1) if series else None  # one JSON line per sample
        self.prefix = blaze_bin + os.sep
        self.limit_mb, self.swap_free_min_mb, self.interval = limit_mb, swap_free_min_mb, interval
        self.run_id, self.homes = run_id, tuple(homes)
        # A one-shot harness can identify its own client tree/session in `pids`
        # without inspecting the environments of unrelated processes.
        self.process_env = process_env
        self.swap_fn = swap_fn
        self.swap_growth_min_mb, self.swap_growth_max_mb = swap_growth_min_mb, swap_growth_max_mb
        self.client = None  # callable returning the pid of the `blaze` client being timed
        self.stop = threading.Event()
        self.tripped = None
        self.peak_mb = 0.0
        self.last_mb = 0.0
        self.last_top = (0.0, None)
        self.peak_top = (0.0, None)
        self.samples = 0
        self.swap_start_used, _ = swap_fn()
        self.peak_swap_growth_mb = 0.0

    def pids(self, rows, client_tree):
        """The pids of the run's tree: every process of it and what those started, minus the
        client's own cargo and rustc (and what they started), which are the build's reference
        and not what the guard is about (docs/reports/TRACEDECAY.md, "the watchdog").

        A process is of the run when it carries this run's `RA_JIT_RUN_ID`, or the `RA_JIT_HOME`
        of one of this harness's daemons, or is a blaze program and says it belongs to neither
        another run nor another home. blaze's own tree is then every blaze program and what it
        started; a process the client started that is not in it is the client's."""
        mine, blaze_own = set(), set()
        for pid, _, _, cmd in rows:
            env = env_of(cmd)
            blaze = cmd.startswith(self.prefix)
            if blaze:
                blaze_own.add(pid)
            if (self.run_id and env.get(RUN_ID_ENV) == self.run_id) \
                    or (self.homes and env.get("RA_JIT_HOME") in self.homes) \
                    or (blaze and not env):
                mine.add(pid)
        blaze_tree = blaze_own | descendants(rows, blaze_own)
        theirs = (client_tree | descendants(rows, client_tree)) - blaze_tree
        return (mine | descendants(rows, mine)) - theirs

    def sample(self):
        rows = ps_table(env=self.process_env)
        client = self.client() if self.client else None
        client_tree = descendants(rows, [client]) if client else set()
        pids = self.pids(rows, client_tree)
        held = footprints_mb(sorted(pids))
        total = sum(held.values())
        top = max(held.items(), key=lambda it: it[1], default=(None, 0.0))
        used, free = self.swap_fn()
        growth = used - self.swap_start_used
        self.last_mb = total
        self.last_top = (top[1], top[0])
        self.peak_mb = max(self.peak_mb, total)
        if top[1] > self.peak_top[0]:
            self.peak_top = (top[1], top[0])
        self.peak_swap_growth_mb = max(self.peak_swap_growth_mb, growth)
        self.samples += 1
        if self.series and pids:
            self.series.write(json.dumps({"t": time.strftime("%T"), "mb": round(total), "pids": len(pids),
                                          "top_mb": round(top[1]), "top_pid": top[0],
                                          "swap_used_mb": round(used), "swap_free_mb": round(free)}) + "\n")
        reason = None
        if total > self.limit_mb:
            reason = f"footprint of the run's processes {total:.0f} MB ({total / GB:.1f} GB) passed the {self.limit_mb:.0f} MB ({self.limit_mb / GB:.1f} GB) limit"
        elif free < self.swap_free_min_mb and growth >= self.swap_growth_min_mb:
            reason = f"swap free {free:.0f} MB under the {self.swap_free_min_mb:.0f} MB floor, grown {growth:.0f} MB"
        elif growth >= self.swap_growth_max_mb:
            reason = f"swap grown {growth:.0f} MB"
        if reason and not self.tripped:
            self.tripped = {"reason": reason, "footprint_mb": round(total), "swap_used_mb": round(used),
                            "swap_free_mb": round(free), "time": time.strftime("%FT%T"), "pids": len(pids),
                            "top_mb": round(top[1]), "top_pid": top[0]}
            print("WATCHDOG:", reason, f"- killing {len(pids)} processes", flush=True)
            kill_pids(pids)
            if client:
                kill_pids(client_tree, signal.SIGTERM)
                time.sleep(2)
                kill_pids(client_tree & {r[0] for r in ps_table(env=self.process_env)})
        return self.tripped

    def run(self):
        while not self.stop.is_set():
            try:
                self.sample()
            except Exception as e:  # a failed sample must not end the watch
                print("WATCHDOG: sample failed:", e, flush=True)
            self.stop.wait(self.interval)

    def reopen(self, series):
        """Write the samples from now on to `series`, and start this run's peak over: a predicate
        that reads a series of its own. The watch itself never stops."""
        if self.series:
            self.series.close()
        self.series = open(series, "a", buffering=1)
        self.take()

    def take(self):
        """The peak footprint and the kill, if any, since the last call."""
        got = {"rajit_footprint_peak_mb": round(self.peak_mb), "watchdog_limit_mb": round(self.limit_mb),
               "watchdog_samples": self.samples, "swap_growth_peak_mb": round(self.peak_swap_growth_mb),
               "footprint_last_mb": round(self.last_mb), "footprint_top_mb": round(self.peak_top[0]),
               "footprint_top_pid": self.peak_top[1]}
        if self.tripped:
            got["watchdog"] = self.tripped
        self.peak_mb = self.last_mb
        self.samples = 0
        self.peak_swap_growth_mb = 0.0
        self.peak_top = (self.last_top[0], self.last_top[1])
        self.tripped = None
        return got


class Sampler(threading.Thread):
    """Once a second: the memory of the build and of blaze's processes, and the other load."""

    def __init__(self, root_pid_fn, blaze_prefix):
        super().__init__(daemon=True)
        self.root_pid_fn = root_pid_fn
        self.blaze_prefix = blaze_prefix
        self.stop = threading.Event()
        self.peak_tree_kb = 0
        self.peak_blaze_kb = 0
        self.peak_total_kb = 0
        self.foreign, self.noise = [], []
        self.swap_start_mb = self.peak_swap_mb = swap_used_mb()

    def run(self):
        me = os.getpid()
        while not self.stop.is_set():
            rows = ps_table(env=False)
            root = self.root_pid_fn()
            tree = descendants(rows, [root]) if root else set()
            tree_kb = sum(r[2] for r in rows if r[0] in tree)
            blaze_pids = {r[0] for r in rows if self.blaze_prefix and r[3].startswith(self.blaze_prefix)}
            blaze_kb = sum(r[2] for r in rows if r[0] in blaze_pids)
            total_kb = sum(r[2] for r in rows if r[0] in tree | blaze_pids)
            foreign = noise = 0.0
            for pid, _, _, cmd in rows:
                if pid in tree or pid in blaze_pids or pid == me:
                    continue
                if NOISE.search(cmd):
                    noise += cpu_of(pid)
                elif BUILDERS.search(cmd + " "):
                    busy = cpu_of(pid)
                    if busy >= 5:
                        foreign += busy
            self.peak_tree_kb = max(self.peak_tree_kb, tree_kb)
            self.peak_blaze_kb = max(self.peak_blaze_kb, blaze_kb)
            self.peak_total_kb = max(self.peak_total_kb, total_kb)
            self.foreign.append(foreign / 100)
            self.noise.append(noise / 100)
            self.peak_swap_mb = max(self.peak_swap_mb, swap_used_mb())
            self.stop.wait(1.0)


def cpu_of(pid):
    """A process's CPU share now, in percent, from `ps`; 0 when it is gone."""
    out = sh(["ps", "-o", "pcpu=", "-p", str(pid)]).stdout.strip()
    try:
        return float(out)
    except ValueError:
        return 0.0


def timed(ctx, tool, scenario, args, run, extra=None, cwd=None, check=False, record=True, foot=False):
    """Runs `tool args` under /usr/bin/time -l and, for a blaze tool, under the guard and the
    audit; returns the record."""
    env = ctx.env(tool)
    log = os.path.join(ctx.logs, f"{tool}-{scenario}-{run}.log")
    # BSD time's `-l`, GNU time's `-v`: both say the largest resident set.
    cmd = ["/usr/bin/time", "-l" if platform.system() == "Darwin" else "-v", ctx.exe(tool)] + args
    blaze_prefix = os.path.join(ctx.blaze_bin, "ra-jit") if tool.startswith("blaze") else None
    load = os.getloadavg()
    holder = {}
    t0 = time.monotonic()
    before = ctx.audit_offset() if blaze_prefix else 0
    with open(log, "w") as out:
        p = subprocess.Popen(cmd, cwd=cwd or ctx.td, env=env, stdout=out, stderr=subprocess.STDOUT)
        holder["pid"] = p.pid
        if ctx.watchdog:
            ctx.watchdog.client = lambda: holder.get("pid")
            between = ctx.watchdog.take()  # this run's peak starts here
            if "watchdog" in between:  # it killed something while no run was timed: still a result
                with open(ctx.results, "a") as f:
                    f.write(json.dumps({"tool": tool, "scenario": "between-runs", "outcome": "watchdog-killed", **between}) + "\n")
        s = Sampler(lambda: holder.get("pid"), blaze_prefix)
        s.start()
        code = p.wait()
        wall = time.monotonic() - t0
        s.stop.set()
        s.join(timeout=3)
        if ctx.watchdog:
            time.sleep(ctx.watchdog.interval + 1)  # one more look after the client is gone: the daemon is not
            ctx.watchdog.client = None
    text = open(log, errors="replace").read()
    fin = [l.strip() for l in text.splitlines() if re.search(r"^\s*Finished|test result:|error(\[|:)", l)]
    mrss = re.search(r"(\d+)\s+maximum resident set size", text)
    if not mrss and (gnu := re.search(r"Maximum resident set size \(kbytes\): (\d+)", text)):
        mrss = re.match(r"(\d+)", str(int(gnu.group(1)) * 1024))
    rec = {
        "tool": tool, "scenario": scenario, "run": run, "wall_s": round(wall, 2), "exit": code,
        "finished": fin[-1] if fin else None,
        "max_process_rss_mb": round(int(mrss.group(1)) / 1048576) if mrss else None,
        "peak_tree_rss_mb": round(s.peak_tree_kb / 1024),
        "peak_blaze_rss_mb": round(s.peak_blaze_kb / 1024),
        "peak_total_rss_mb": round(s.peak_total_kb / 1024),
        "foreign_build_cores_mean": round(statistics.fmean(s.foreign), 2) if s.foreign else None,
        "foreign_build_cores_peak": round(max(s.foreign), 2) if s.foreign else None,
        "noise_cores_mean": round(statistics.fmean(s.noise), 2) if s.noise else None,
        "swap_used_mb_start": round(s.swap_start_mb), "swap_used_mb_peak": round(s.peak_swap_mb),
        "load1_before": round(load[0], 1), "log": log, "time": time.strftime("%FT%T"),
    }
    if blaze_prefix:
        rec["rajit_finished"] = bool(re.search(r"Finished .*\[ra-jit\]", text))
        rec["cargo_built"] = "building with cargo" in text
        runs = ctx.audit_since(before)
        rec["audit"] = {
            "compiler": [r for r in runs if r["tool"] in ("rustc", "rustdoc")],
            "cargo": [r for r in runs if r["tool"] == "cargo"],
        }
    if ctx.watchdog:
        rec.update(ctx.watchdog.take())
        rec["guard"] = ctx.guard
        rec["outcome"] = "watchdog-killed" if "watchdog" in rec else ("ok" if code == 0 else "failed")
    else:
        rec["outcome"] = "ok" if code == 0 else "failed"
    if foot and blaze_prefix:
        rec["blaze_footprint_mb_after"] = blaze_footprint_mb(blaze_prefix)
    if extra:
        rec.update(extra)
    if record:
        with open(ctx.results, "a") as f:
            f.write(json.dumps(rec) + "\n")
    print(json.dumps({k: rec[k] for k in ("tool", "scenario", "run", "wall_s", "exit", "outcome", "finished",
                                          "foreign_build_cores_peak", "rajit_footprint_peak_mb") if k in rec}), flush=True)
    if check and code:
        sys.exit(f"{tool} {scenario} failed ({code}); see {log}")
    return rec


def du_mb(path):
    if not os.path.exists(path):
        return 0
    out = sh(["du", "-sk", path]).stdout.split()
    return round(int(out[0]) / 1024) if out else 0


def start_watchdog(ctx, a, series=None):
    """The guard's watchdog for this harness's blaze, with the run's id and homes. A limit may be
    lower than the guard's, never higher (validate says so). One watchdog per run, started once:
    a predicate that wants a series of its own reopens the same watch."""
    if ctx.watchdog:
        if series:
            ctx.watchdog.reopen(series)
        return ctx.watchdog
    ctx.watchdog = Watchdog(ctx.blaze_bin, a.watchdog_mb, a.swap_free_mb, a.interval, series=series,
                            run_id=ctx.run_id, homes=ctx.homes(), swap_growth_min_mb=a.swap_grow_mb,
                            swap_growth_max_mb=a.swap_grow_hard_mb)
    ctx.watchdog.start()
    return ctx.watchdog


def guarded(ctx, tool, args, cwd=None):
    """A blaze command that is not timed (stop, status): still under the guard's environment and
    the watchdog."""
    if not tool.startswith("blaze"):
        return sh([ctx.exe(tool)] + args, env=ctx.env(tool), cwd=cwd or ctx.td)
    holder = {}
    wd = Watchdog(ctx.blaze_bin, ctx.watchdog.limit_mb if ctx.watchdog else WATCHDOG_MB,
                  ctx.watchdog.swap_free_min_mb if ctx.watchdog else SWAP_FREE_MIN_MB,
                  ctx.watchdog.interval if ctx.watchdog else 2.0, run_id=ctx.run_id, homes=ctx.homes(),
                  swap_growth_min_mb=ctx.watchdog.swap_growth_min_mb if ctx.watchdog else SWAP_GROWTH_MIN_MB,
                  swap_growth_max_mb=ctx.watchdog.swap_growth_max_mb if ctx.watchdog else SWAP_GROWTH_MAX_MB)
    p = subprocess.Popen([ctx.exe(tool)] + args, env=ctx.env(tool), cwd=cwd or ctx.td,
                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    holder["pid"] = p.pid
    wd.client = lambda: holder.get("pid")
    wd.start()
    out, _ = p.communicate()
    time.sleep(wd.interval + 1)
    wd.client = None
    wd.stop.set()
    wd.join(timeout=3)
    trip = wd.take()
    if "watchdog" in trip:
        with open(ctx.results, "a") as f:
            f.write(json.dumps({"tool": tool, "scenario": "guarded", "outcome": "watchdog-killed", **trip}) + "\n")
    return subprocess.CompletedProcess(args, p.returncode, out, "")


def stop_daemon(ctx, tool):
    if tool.startswith("blaze"):
        guarded(ctx, tool, ["stop"])


def clean(ctx, tool):
    stop_daemon(ctx, tool)
    time.sleep(1)
    for d in (ctx.target(tool), ctx.store(tool), ctx.blaze_home(tool)):
        shutil.rmtree(d, ignore_errors=True)
    if tool.startswith("blaze"):
        os.makedirs(ctx.blaze_home(tool), mode=0o700, exist_ok=True)


EXTRA_FLAGS = []


def build_args(tool, package):
    return ["build", "--locked", "-p", package] + EXTRA_FLAGS


def disk(ctx, tool):
    return {
        "target_mb": du_mb(ctx.target(tool)),
        "store_mb": du_mb(ctx.store(tool)),
        "home_mb": du_mb(ctx.blaze_home(tool)),
    }


def cmd_prepare(ctx, a):
    config = os.path.join(ctx.td, ".pnpm", "crates", "config.toml")
    if os.path.exists(config):
        os.rename(config, config + ".off")
        print("moved aside", config)
    home = os.path.join(ctx.work, "cargo-home")
    if not os.path.exists(home):
        os.makedirs(home)
        for name in ("registry", "git"):
            src = os.path.expanduser(os.path.join("~/.cargo", name))
            sh(["cp", "-cR" if platform.system() == "Darwin" else "-r", src, os.path.join(home, name)], check=True)
        print("cloned", home)
    lib = os.path.join(ctx.td, "crates", "tracedecay-code-index-runtime", "src", "lib.rs")
    if os.path.exists(lib):
        text = open(lib).read()
        if "recursion_limit" not in text:
            open(lib, "w").write('#![recursion_limit = "256"]\n' + text)
            print("patched", lib)


def cmd_versions(ctx, a):
    def v(*args, **kw):
        return sh(list(args), **kw).stdout.strip()

    print("machine:", platform.platform(), v("sysctl", "-n", "machdep.cpu.brand_string"), v("sysctl", "-n", "hw.ncpu"), "cpus")
    print("blaze:", v("git", "-C", os.path.dirname(os.path.abspath(__file__)), "rev-parse", "HEAD"))
    print("tracedecay:", v("git", "-C", ctx.td, "rev-parse", "HEAD"))
    print("cargo (pinned):", v("cargo", "-V", cwd=ctx.td), "| rustc:", v("rustc", "-V", cwd=ctx.td))
    print("blaze toolchain:", v(os.path.join(ctx.blaze_bin, "blaze"), "toolchain", env=ctx.env("blaze"), cwd=ctx.td))
    print("load:", os.getloadavg())


def cmd_cold(ctx, a):
    last = a.first_run + a.runs - 1
    for run in range(a.first_run, last + 1):
        clean(ctx, a.tool)
        rec = timed(ctx, a.tool, "cold", build_args(a.tool, a.package), run, cwd=ctx.td, foot=True)
        rec.update(disk(ctx, a.tool))
        with open(ctx.results, "a") as f:
            f.write(json.dumps({"tool": a.tool, "scenario": "cold-disk", "run": run, **disk(ctx, a.tool)}) + "\n")
        print("disk:", json.dumps(disk(ctx, a.tool)), flush=True)
        if a.status and a.tool.startswith("blaze"):
            cmd_status(ctx, a)
        if run < last or a.stop:
            stop_daemon(ctx, a.tool)


def cmd_noop(ctx, a):
    for run in range(1, a.runs + 1):
        timed(ctx, a.tool, "noop", build_args(a.tool, a.package), run, cwd=ctx.td)


def edit_file(ctx, name):
    return {
        "tracedecay-body": "crates/tracedecay/src/doctor.rs",
        "mcp-body": "crates/tracedecay-mcp/src/tool_analytics.rs",
        "signature": "crates/tracedecay/src/doctor.rs",
    }[name]


SIG_TYPES = ["u8", "u16", "u32", "u64", "usize", "u128", "i8", "i16"]


def apply_edit(ctx, name, i):
    path = os.path.join(ctx.td, edit_file(ctx, name))
    src = open(path).read()
    if name == "tracedecay-body":
        new, n = re.subn(r"std::time::Duration::from_secs\((\d+)\);\n(\s+)let result = crate::daemon::call_default_tool_within",
                         lambda m: f"std::time::Duration::from_secs({90 + i});\n{m.group(2)}let result = crate::daemon::call_default_tool_within", src)
    elif name == "mcp-body":
        new, n = re.subn(r"\.take\(FAILURE_REASON_MAX_CHARS( - \d+)?\)\.collect\(\)", f".take(FAILURE_REASON_MAX_CHARS - {i}).collect()", src)
    elif name == "signature":
        ty = SIG_TYPES[(i - 1) % len(SIG_TYPES)]
        new, n = re.subn(r"fn database_recovery_guidance\(db_path: &Path(, _edit: \w+)?\) -> String", f"fn database_recovery_guidance(db_path: &Path, _edit: {ty}) -> String", src)
        new, m = re.subn(r"database_recovery_guidance\(db_path(, 0)?\)\.lines\(\)", "database_recovery_guidance(db_path, 0).lines()", new)
        n = min(n, m)
    if n != 1:
        sys.exit(f"edit {name} step {i}: the anchor matched {n} times in {path}")
    open(path, "w").write(new)


def revert_edit(ctx, name):
    sh(["git", "checkout", "--", edit_file(ctx, name)], cwd=ctx.td)


def cmd_edit(ctx, a):
    status = sh(["git", "status", "--porcelain", "--", edit_file(ctx, a.edit)], cwd=ctx.td).stdout.strip()
    if status:
        sys.exit("the file to edit has edits already:\n" + status)
    # one build to settle: the state the first edit is made on
    timed(ctx, a.tool, "settle", build_args(a.tool, a.package), 0, cwd=ctx.td, record=False)
    try:
        for i in range(1, a.runs + 1):
            apply_edit(ctx, a.edit, i)
            rec = timed(ctx, a.tool, "edit-" + a.edit, build_args(a.tool, a.package), i, cwd=ctx.td, foot=(i == a.runs))
            if rec["exit"] == 0:
                out = sh([ctx.binary(a.tool), "--version"], env=ctx.env(a.tool)).stdout.strip()
                print("  binary runs:", out.splitlines()[0] if out else "(no output)", flush=True)
    finally:
        revert_edit(ctx, a.edit)
    timed(ctx, a.tool, "edit-" + a.edit + "-revert", build_args(a.tool, a.package), 0, cwd=ctx.td)
    if a.status and a.tool.startswith("blaze"):
        cmd_status(ctx, a)


def cmd_test(ctx, a):
    for run in range(1, a.runs + 1):
        if a.cold:
            clean(ctx, a.tool)
        timed(ctx, a.tool, "test-cold" if a.cold else "test", ["test", "--locked", "-p", a.package] + a.rest, run, cwd=ctx.td)


def cmd_stop(ctx, a):
    stop_daemon(ctx, a.tool)
    print("stopped; blaze processes left:", [r[0] for r in ps_table() if r[3].startswith(ctx.blaze_bin + os.sep)])


def cmd_watch(ctx, a):
    """Prints the footprint of this blaze's processes every 2 s for --seconds (or until none is left);
    the watchdog kills them as in any run. For a daemon that outlives the command that started it."""
    end = time.monotonic() + a.seconds
    while time.monotonic() < end:
        time.sleep(2)
        wd = ctx.watchdog
        print(time.strftime("%T"), f"footprint {wd.last_mb:.0f} MB, peak {wd.peak_mb:.0f} MB, swap growth {wd.peak_swap_growth_mb:.0f} MB", flush=True)
        if wd.tripped or (wd.samples > 2 and wd.last_mb == 0):
            break
    rec = {"tool": a.tool, "scenario": "watch", "outcome": "watchdog-killed" if ctx.watchdog.tripped else "ok", **ctx.watchdog.take()}
    with open(ctx.results, "a") as f:
        f.write(json.dumps(rec) + "\n")
    print(json.dumps(rec))


def cmd_status(ctx, a):
    r = guarded(ctx, a.tool, ["status"])
    print(r.stdout)
    with open(os.path.join(ctx.work, f"status-{a.tool}.txt"), "a") as f:
        f.write(time.strftime("%FT%T") + "\n" + r.stdout + "\n")


def cmd_summary(ctx, a):
    groups = {}
    for path in a.files or [ctx.results]:
        for line in open(path):
            r = json.loads(line)
            if "wall_s" in r:
                groups.setdefault((r["tool"], r["scenario"]), []).append(r)
    print(f"{'tool':12} {'scenario':26} {'n':>2} {'ok':>2} {'killed':>6} {'median':>8} {'min':>8} {'max':>8} {'contended':>9}")
    for (tool, scen), rs in sorted(groups.items()):
        ok = [r for r in rs if r["exit"] == 0]
        w = [r["wall_s"] for r in ok] or [float("nan")]
        cont = sum(1 for r in rs if (r["foreign_build_cores_peak"] or 0) > 1)
        killed = sum(1 for r in rs if r.get("outcome") == "watchdog-killed")
        print(f"{tool:12} {scen:26} {len(rs):2} {len(ok):2} {killed:6} {statistics.median(w):8.2f} {min(w):8.2f} {max(w):8.2f} {cont:9}")


# ---------------------------------------------------------------------------------------------
# The Phase 0 predicates (docs/distributed.md, section 0).


class Report:
    """What a predicate found: its bar, whether it holds, what failed, and each compiler run."""

    def __init__(self, name, bar):
        self.name, self.bar, self.ok = name, bar, True
        self.notes, self.named, self.asked, self.facts = [], [], [], []

    def fail(self, line):
        self.ok = False
        self.notes.append(line)

    def name_run(self, run):
        """Every compiler run of a scenario, named. A run that only asked a question is named
        with the question; a run that compiled is named with its crate, its parent chain and its
        argv, since it is the one the bar is about."""
        if only_asks(run["argv"]):
            self.asked.append(f"{run['tool']} pid {run['pid']} {shorten(run['argv'], 60)}")
        else:
            self.named.append(f"{run['tool']} pid {run['pid']} COMPILED: parent {shorten(run['parent'], 90)} "
                              f"argv {shorten(run['argv'], 200)}")

    def print(self):
        print(f"{self.name}: {'PASS' if self.ok else 'FAIL'} - {self.bar}", flush=True)
        for fact in self.facts:
            print("  " + fact, flush=True)
        for name in self.named:
            print("  compiler ran: " + name, flush=True)
        for line in self.asked:
            print("  only asked: " + line, flush=True)
        for note in self.notes:
            print("  FAILED: " + note, flush=True)


def shorten(text, limit):
    text = text.strip()
    return text if len(text) <= limit else text[:limit - 3] + "..."


def only_asks(argv):
    """Whether a rustc run only asked a question (`--version`, `--print=cfg`, ...), as opposed to
    compiling: norustc.py's rule, which leaves out the two prints that are part of a link."""
    words = argv.split()
    kinds = [w.split("=", 1)[1] for w in words if w.startswith("--print=")]
    kinds += [b for a, b in zip(words, words[1:]) if a == "--print"]
    printing = bool(kinds) and not any(k.startswith(("link-args", "native-static-libs")) for k in kinds)
    return bool(printing or {"-vV", "-V", "--version"} & set(words))


def cargo_only_asks(argv):
    """Whether a cargo run only asked a question (`--version`, `config get`, `metadata`, a stand-in `rustc`
    `--print`), as opposed to building, checking, testing or running something."""
    words = argv.split()
    if {"--version", "-V", "-vV"} & set(words[:2]):
        return True
    # `-Z unstable-options` is a flag and its value, not the subcommand.
    rest = [w for w in words if not w.startswith("-") and w != "unstable-options"]
    first = rest[0] if rest else ""
    if first in ("metadata", "locate-project", "pkgid", "tree", "read-manifest", "verify-project"):
        return True
    if first == "config" and "get" in rest[:2]:
        return True
    if first == "rustc" and any(w.startswith("--print") or w == "--print" for w in words):
        return True
    return False


def cargo_summary(runs):
    """`N cargo runs, M of them only asking`: what tells a query of cargo from a build by cargo."""
    asking = sum(1 for run in runs if cargo_only_asks(run["argv"]))
    return f"{len(runs)} cargo runs, {asking} of them only asking, {len(runs) - asking} compiling or otherwise running things"


def compiler_summary(runs):
    """`N compiler runs, M of them only asking`, which is what tells a probe from a compile."""
    asking = sum(1 for run in runs if only_asks(run["argv"]))
    return f"{len(runs)} compiler runs, {asking} of them only asking, {len(runs) - asking} compiling"


def root_manifests(ctx):
    """The name of every package whose manifest is at the root or one level down, which is what
    says whether --td is the project a predicate needs."""
    return package_names(ctx.td)


def package_names(td):
    """The package names of `td`'s manifests at its root and one level down, without building
    anything. A `target` directory or `.git` below `td` is not searched, and neither is anything
    deeper: a workspace's members are one level down, its dependencies' are not in it."""
    names = set()
    for dirpath, dirnames, filenames in os.walk(td):
        rel = os.path.relpath(dirpath, td)
        depth = 0 if rel == "." else rel.count(os.sep) + 1
        if depth > 2 or any(part in ("target", ".git", ".pnpm", "node_modules") for part in rel.split(os.sep)):
            dirnames[:] = []
            continue
        if "Cargo.toml" in filenames:
            try:
                text = open(os.path.join(dirpath, "Cargo.toml"), errors="replace").read()
            except OSError:
                continue
            m = re.search(r'^\s*name\s*=\s*"([^"]+)"', text, re.M)
            if m:
                names.add(m.group(1))
    return names


def need_package(ctx, packages):
    """True when --td has one of `packages`; otherwise says which projects the predicate is
    measured on, and why. A predicate whose bar is about tracedecay's size is not measured on a
    hello-world: it would hold for a reason the bar is not about."""
    if isinstance(packages, str):
        packages = (packages,)
    have = root_manifests(ctx)
    if have & set(packages):
        return True
    print(f"--td {ctx.td} has no package of {', '.join(packages)}, which this predicate is measured on "
          f"(it has {', '.join(sorted(have)) or 'none'})", flush=True)
    return False


def audit_names(report, rec):
    for run in rec.get("audit", {}).get("compiler", []):
        report.name_run(run)


def compiler_count(rec):
    return len(rec.get("audit", {}).get("compiler", []))

def predicate_a(ctx, a):
    """P0-A, no rustc: the build, the program, the test binary and the doctests with 0 compiler
    execs in the tree. Section 0's positive fixture; the negative ones are P0.2's. With
    `--whole`, the default owned build, which builds and runs programs only:
    the build and the program, with 0 compiler execs and 0 cargo execs."""
    if a.whole:
        return predicate_a_whole(ctx, a)
    r = Report("P0-A", "the build, the program, the test binary and a doctest succeed with 0 compiler execs in the tree")
    clean(ctx, "blaze")
    build = timed(ctx, "blaze", "p0a-build", build_args("blaze", a.package), 1, cwd=ctx.td)
    r.facts.append(f"build: exit {build['exit']}, {build['finished']!r}, {compiler_summary(build['audit']['compiler'])}")
    if build["exit"] != 0:
        r.fail(f"the build failed ({build['exit']}): {build['finished']}")
    audit_names(r, build)
    program = os.path.join(ctx.target("blaze"), "debug", a.program)
    if build["exit"] == 0 and os.path.exists(program):
        ran = sh([program] + a.program_args, env=ctx.env("blaze"), cwd=ctx.td)
        r.facts.append(f"program: exit {ran.returncode}, {ran.stdout.strip()!r}")
        if ran.returncode != 0:
            r.fail(f"the program exited {ran.returncode}")
    else:
        r.fail(f"no program at {program}")
    test_build = timed(ctx, "blaze", "p0a-test-build", ["test", "--locked", "-p", a.package, "--no-run"], 1, cwd=ctx.td)
    r.facts.append(f"test binary: exit {test_build['exit']}, {compiler_summary(test_build['audit']['compiler'])}")
    if test_build["exit"] != 0:
        r.fail(f"the test binary failed to build ({test_build['exit']}): {test_build['finished']}")
    audit_names(r, test_build)
    test = timed(ctx, "blaze", "p0a-test", ["test", "--locked", "-p", a.package], 1, cwd=ctx.td)
    r.facts.append(f"tests: exit {test['exit']}, {compiler_summary(test['audit']['compiler'])}")
    if test["exit"] != 0:
        r.fail(f"the tests failed ({test['exit']}): {test['finished']}")
    audit_names(r, test)
    if a.doc:
        doc = timed(ctx, "blaze", "p0a-doc", ["test", "--locked", "--doc", "-p", a.package], 1, cwd=ctx.td)
        r.facts.append(f"doctests: exit {doc['exit']}, {compiler_summary(doc['audit']['compiler'])}")
        if doc["exit"] != 0:
            r.fail(f"the doctests failed ({doc['exit']}): {doc['finished']}")
        audit_names(r, doc)
    runs = sum(compiler_count(rec) for rec in (build, test_build, test))
    if runs:
        asking = sum(1 for rec in (build, test_build, test) for run in rec["audit"]["compiler"] if only_asks(run["argv"]))
        r.fail(f"{runs} compiler execs in the tree ({asking} of them only asking, {runs - asking} compiling); "
               "P0-A's bar is 0")
    stop_daemon(ctx, "blaze")
    return r


def predicate_a_whole(ctx, a):
    """P0-A of the whole build: a cold build, the program, an edit of each crate the build
    compiles a build script with, and a build with nothing changed, each with no rustc, rustdoc
    or cargo started by blaze: the stand-ins log every one."""
    tool = "blaze-whole"
    r = Report("P0-A", "the whole build and its program succeed with 0 compiler execs and 0 cargo execs in the tree")
    clean(ctx, tool)
    records = [timed(ctx, tool, "p0a-whole-build", build_args(tool, a.package), 1, cwd=ctx.td)]
    for i, (path, old, new) in enumerate(a.whole_edit or []):
        file = os.path.join(ctx.td, path)
        text = open(file).read()
        if old not in text:
            r.fail(f"{path} has no {old!r} to edit")
            continue
        with open(file, "w") as f:
            f.write(text.replace(old, new, 1))
        records.append(timed(ctx, tool, f"p0a-whole-edit-{i + 1}", build_args(tool, a.package), 1, cwd=ctx.td))
    records.append(timed(ctx, tool, "p0a-whole-noop", build_args(tool, a.package), 1, cwd=ctx.td))
    for rec in records:
        compilers, cargos = rec["audit"]["compiler"], rec["audit"]["cargo"]
        r.facts.append(f"{rec['scenario']}: exit {rec['exit']}, {rec['finished']!r}, {compiler_summary(compilers)}, "
                       f"{cargo_summary(cargos)}")
        if rec["exit"] != 0:
            r.fail(f"{rec['scenario']} failed ({rec['exit']}): {rec['finished']}")
        if not rec.get("rajit_finished"):
            r.fail(f"{rec['scenario']} was not blaze's build ([ra-jit])")
        audit_names(r, rec)
        for run in cargos:
            r.fail(f"{rec['scenario']}: cargo ran: {shorten(run['argv'], 200)} (parent {shorten(run['parent'], 120)})")
    program = os.path.join(ctx.target(tool), "debug", a.program)
    if os.path.exists(program):
        ran = sh([program] + a.program_args, env=ctx.env(tool), cwd=ctx.td)
        r.facts.append(f"program: exit {ran.returncode}, {ran.stdout.strip()!r}")
        if ran.returncode != 0:
            r.fail(f"the program exited {ran.returncode}")
    else:
        r.fail(f"no program at {program}")
    compilers = sum(compiler_count(rec) for rec in records)
    cargos = sum(len(rec["audit"]["cargo"]) for rec in records)
    if compilers or cargos:
        every = [run for rec in records for run in rec["audit"]["cargo"]]
        r.fail(f"{compilers} compiler execs and {cargo_summary(every)} in the tree; the whole build's bar is 0 and 0")
    stop_daemon(ctx, tool)
    return r


def predicate_b(ctx, a):
    """P0-B, memory bounded: with RA_JIT_MEMORY_MAX=M the workers together hold at most 1.05 M for
    no more than 2 s in any state and no single process passes it, and the watchdog never fires."""
    limit = parse_mb(ctx.guard["RA_JIT_MEMORY_MAX"])
    r = Report("P0-B", f"the workers together hold at most {limit * 1.05:.0f} MB for no more than 2 s, none passes {limit:.0f} MB, the watchdog never fires")
    if not need_package(ctx, (CLI_PACKAGE, "nu")):
        return None
    clean(ctx, "blaze")
    series = os.path.join(ctx.logs, "p0b-series.jsonl")
    if os.path.exists(series):
        os.unlink(series)
    start_watchdog(ctx, a, series=series)
    rec = timed(ctx, "blaze", "p0b-cold", build_args("blaze", a.package), 1, cwd=ctx.td)
    stop_daemon(ctx, "blaze")
    samples = [json.loads(line) for line in open(series)] if os.path.exists(series) else []
    over = [s for s in samples if s["mb"] > 1.05 * limit]
    longest = 0
    for i, s in enumerate(samples):
        if s["mb"] <= 1.05 * limit:
            continue
        j = i
        while j + 1 < len(samples) and samples[j + 1]["mb"] > 1.05 * limit:
            j += 1
        longest = max(longest, j - i + 1)
    seconds = longest * a.interval
    peak = max((s["mb"] for s in samples), default=0)
    top = max((s["top_mb"] for s in samples), default=0)
    r.facts.append(f"peak of the run's tree {peak} MB, largest process {top} MB, {len(over)} samples over {1.05 * limit:.0f} MB, longest {seconds:.0f} s")
    if seconds > 2:
        r.fail(f"the workers held over 1.05 x {limit:.0f} MB for {seconds:.0f} s, the bar allows 2 s")
    if top > limit:
        r.fail(f"one process held {top} MB, over the {limit:.0f} MB of RA_JIT_MEMORY_MAX")
    if "watchdog" in rec:
        r.fail("the harness watchdog fired: " + rec["watchdog"]["reason"])
    if rec["exit"] != 0:
        r.fail(f"the build failed ({rec['exit']}): {rec['finished']}")
        if re.search(r"RA_JIT_MEMORY_MAX|holds \d+ MB, over", open(rec["log"], errors="replace").read()):
            r.facts.append("the build stopped by the bound and said so")
    if not samples:
        r.fail("the watchdog took no sample: nothing was measured")
    return r


def predicate_c(ctx, a):
    """P0-C, tracedecay fits: a loaded tracedecay-cli worker holds at most 16 GB at ready and
    after 20 edit/undo pairs."""
    limit = parse_mb(ctx.guard["RA_JIT_MEMORY_MAX"])
    r = Report("P0-C", f"a loaded tracedecay-cli worker holds at most {limit:.0f} MB at ready and after {a.pairs} pairs")
    if not need_package(ctx, CLI_PACKAGE):
        return None
    series = os.path.join(ctx.logs, "p0c-series.jsonl")
    if os.path.exists(series):
        os.unlink(series)
    clean(ctx, "blaze")
    start_watchdog(ctx, a, series=series)
    cold = timed(ctx, "blaze", "p0c-cold", build_args("blaze", a.package), 1, cwd=ctx.td)
    r.facts.append(f"cold: exit {cold['exit']}, {cold['finished']!r}, {cold.get('footprint_last_mb')} MB at the end")
    if cold["exit"] != 0:
        r.fail(f"the cold build failed ({cold['exit']}): {cold['finished']}")
    ready = cold.get("footprint_last_mb", 0)
    if ready > limit:
        r.fail(f"the loaded worker held {ready} MB at ready, over {limit:.0f} MB")
    try:
        for i in range(1, a.pairs + 1):
            apply_edit(ctx, a.edit, i)
            edited = timed(ctx, "blaze", f"p0c-pair-{i}", build_args("blaze", a.package), i, cwd=ctx.td)
            revert_edit(ctx, a.edit)
            reverted = timed(ctx, "blaze", f"p0c-pair-{i}-revert", build_args("blaze", a.package), i, cwd=ctx.td)
            if edited["exit"] or reverted["exit"]:
                r.fail(f"pair {i} failed: edit {edited['exit']}, revert {reverted['exit']}")
                break
    finally:
        revert_edit(ctx, a.edit)
    end = ctx.watchdog.take().get("footprint_last_mb", 0)
    r.facts.append(f"{a.pairs} pairs: {end} MB at the end")
    if end > limit:
        r.fail(f"after {a.pairs} pairs the worker held {end} MB, over {limit:.0f} MB")
    samples = [json.loads(line) for line in open(series)] if os.path.exists(series) else []
    r.facts.append(f"peak over the samples {max((s['mb'] for s in samples), default=0)} MB")
    stop_daemon(ctx, "blaze")
    return r


def predicate_d(ctx, a):
    """P0-D, edits are the daemon's: 5 runs x 3 edit kinds x an edit and its revert, 30 of 30 the
    daemon's, 0 `building with cargo`, after one cold and one warm build that are not counted."""
    r = Report("P0-D", f"{a.runs * 3 * 2} of {a.runs * 3 * 2} counted builds are the daemon's, 0 `building with cargo`")
    if not need_package(ctx, CLI_PACKAGE):
        return None
    clean(ctx, "blaze")
    cold = timed(ctx, "blaze", "p0d-cold", build_args("blaze", a.package), 0, cwd=ctx.td)
    warm = timed(ctx, "blaze", "p0d-warm", build_args("blaze", a.package), 0, cwd=ctx.td)
    r.facts.append(f"the two preparation builds (not counted): cold exit {cold['exit']}, warm exit {warm['exit']}")
    if cold["exit"] or warm["exit"]:
        r.fail("the cold or warm build before the counted ones failed")
    counted = rajit = cargo = 0
    for name in ("tracedecay-body", "mcp-body", "signature"):
        for i in range(1, a.runs + 1):
            apply_edit(ctx, name, i)
            edit = timed(ctx, "blaze", f"p0d-{name}", build_args("blaze", a.package), i, cwd=ctx.td)
            revert_edit(ctx, name)
            back = timed(ctx, "blaze", f"p0d-{name}-revert", build_args("blaze", a.package), i, cwd=ctx.td)
            for rec in (edit, back):
                counted += 1
                if rec["exit"] != 0:
                    r.fail(f"{rec['scenario']} {i} failed ({rec['exit']}): {rec['finished']}")
                if rec.get("cargo_built"):
                    cargo += 1
                    r.fail(f"{rec['scenario']} {i} built with cargo")
                if rec.get("rajit_finished"):
                    rajit += 1
                else:
                    r.fail(f"{rec['scenario']} {i} did not finish as the daemon's: {rec['finished']}")
    r.facts.append(f"{counted} counted builds, {rajit} the daemon's, {cargo} `building with cargo`")
    if counted != a.runs * 3 * 2:
        r.fail(f"{counted} counted builds, {a.runs * 3 * 2} expected")
    stop_daemon(ctx, "blaze")
    return r


def predicate_e(ctx, a):
    """P0-E, the default owned build on macOS: blaze builds the projects and their programs
    print what cargo's do, with 0 `without a rustc symbol`."""
    r = Report("P0-E", "the whole build builds the projects, their programs print what cargo's do, 0 `without a rustc symbol`")
    projects = [(ctx.td, a.package)]
    names = root_manifests(ctx)
    if "ripgrep" in names:
        projects.append((ctx.td, "ripgrep"))
    for td, package in projects:
        r.facts.append(f"{package}: {td}")
    if "ripgrep" not in names:
        r.facts.append("ripgrep is not this checkout; P0-E's ripgrep arm needs --td of ripgrep (harness/dist/fetch.py)")
    for td, package in projects:
        clean(ctx, "blaze-whole")
        whole = timed(ctx, "blaze-whole", f"p0e-whole-{package}", ["build", "--locked", "-p", package], 1, cwd=td)
        text = open(whole["log"], errors="replace").read()
        r.facts.append(f"{package} whole: exit {whole['exit']}, {whole['finished']!r}")
        if whole["exit"] != 0:
            r.fail(f"{package}: the whole build failed ({whole['exit']}): {whole['finished']}")
        if "without a rustc symbol" in text:
            r.fail(f"{package}: `without a rustc symbol` in the build")
        binary = os.path.join(ctx.target("blaze-whole"), "debug", a.program if package == a.package else "rg")
        if whole["exit"] == 0 and os.path.exists(binary):
            ours = sh([binary] + a.program_args, env=ctx.env("blaze-whole"), cwd=td)
            cargo_rec = timed(ctx, "cargo", f"p0e-cargo-{package}", ["build", "--locked", "-p", package], 1, cwd=td)
            theirs = sh([binary] + a.program_args, env=ctx.env("cargo"), cwd=td)
            r.facts.append(f"{package} program: whole {ours.returncode} {ours.stdout.strip()[:80]!r}, cargo {theirs.returncode} {theirs.stdout.strip()[:80]!r}")
            if (ours.returncode, ours.stdout) != (theirs.returncode, theirs.stdout):
                r.fail(f"{package}: the whole build's program does not print what cargo's does")
    stop_daemon(ctx, "blaze-whole")
    return r


def predicate_f(ctx, a):
    """P0-F, a cold build's peak: the watchdog's 2 s series, at most 4 GB. The attribution of the
    peak to the script compiles is P0.7's; this is the whole cold build's peak, which is stricter."""
    bar = 4 * GB
    r = Report("P0-F", f"a cold build's footprint at most {bar / GB:.0f} GB at any 2 s sample")
    if not need_package(ctx, (CLI_PACKAGE, "nu")):
        return None
    series = os.path.join(ctx.logs, "p0f-series.jsonl")
    if os.path.exists(series):
        os.unlink(series)
    clean(ctx, "blaze")
    start_watchdog(ctx, a, series=series)
    rec = timed(ctx, "blaze", "p0f-cold", build_args("blaze", a.package), 1, cwd=ctx.td)
    stop_daemon(ctx, "blaze")
    samples = [json.loads(line) for line in open(series)] if os.path.exists(series) else []
    peak = max((s["mb"] for s in samples), default=0)
    r.facts.append(f"peak {peak} MB over {len(samples)} samples of {a.interval:.0f} s; the bar is the whole cold build's, stricter than the script compiles'")
    if rec["exit"] != 0:
        r.fail(f"the cold build failed ({rec['exit']}): {rec['finished']}")
    if peak > bar:
        r.fail(f"the cold build held {peak} MB, over {bar / GB:.0f} GB")
    if not samples:
        r.fail("the watchdog took no sample")
    return r


def predicate_g(ctx, a):
    """P0-G, Linux at scale: P0-A to P0-F on Linux x86-64."""
    r = Report("P0-G", "P0-A to P0-F met on Linux x86-64")
    if platform.system() != "Linux":
        print("P0-G: unsupported here: it needs a Linux host of at least 64 GB (the owner's linux-host gate); "
              "this is %s" % platform.system(), flush=True)
        return None
    for fn in (predicate_a, predicate_b, predicate_c, predicate_d, predicate_e, predicate_f):
        got = fn(ctx, a)
        if got is None:
            r.fail(f"{fn.__name__} cannot run here")
            continue
        if not got.ok:
            r.fail(f"{got.name}: {got.notes}")
    return r


def predicate_h(ctx, a):
    """P0-H, G0 again: blaze's cold whole build and each edit kind against cargo, n >= 3, the
    limiter named. No speed bar."""
    r = Report("P0-H", f"cold, no-op and the three edit kinds, blaze and cargo, n >= {a.runs}, limiter named")
    if not need_package(ctx, CLI_PACKAGE):
        return None
    if a.runs < 3:
        r.fail(f"--runs {a.runs}: the bar needs at least 3 runs per arm")
    arms = {}
    for tool in ("cargo", "blaze"):
        arms[tool] = {"cold": [], "noop": [], "edit": []}
        for run in range(1, a.runs + 1):
            # Every run of the cold arm is cold: its target directory, store and daemon home are
            # empty, as TRACEDECAY.md's are, so the arm is n cold builds and not one and n - 1 warm.
            clean(ctx, tool)
            arms[tool]["cold"].append(timed(ctx, tool, "p0h-cold", build_args(tool, a.package), run, cwd=ctx.td))
            arms[tool]["noop"].append(timed(ctx, tool, "p0h-noop", build_args(tool, a.package), run, cwd=ctx.td))
        stop_daemon(ctx, tool)
    for name in ("tracedecay-body", "mcp-body", "signature"):
        for tool in ("cargo", "blaze"):
            for run in range(1, a.runs + 1):
                apply_edit(ctx, name, run)
                rec = timed(ctx, tool, f"p0h-{name}", build_args(tool, a.package), run, cwd=ctx.td)
                revert_edit(ctx, name)
                arms[tool]["edit"].append(rec)
    stop_daemon(ctx, "blaze")
    for tool, by in arms.items():
        for scenario, recs in by.items():
            ok = [x for x in recs if x["exit"] == 0]
            if len(ok) < a.runs:
                r.fail(f"{tool} {scenario}: {len(ok)} of {len(recs)} runs finished")
            if ok:
                walls = [x["wall_s"] for x in ok]
                contended = sum(1 for x in ok if (x["foreign_build_cores_peak"] or 0) > 1)
                limiter = "contended" if contended else "none named (no other build above 1 core)"
                r.facts.append(f"{tool} {scenario}: n={len(ok)} median {statistics.median(walls):.2f} s "
                               f"min {min(walls):.2f} max {max(walls):.2f}, limiter {limiter}")
    return r


PREDICATE_FNS = {"P0-A": predicate_a, "P0-B": predicate_b, "P0-C": predicate_c, "P0-D": predicate_d,
                 "P0-E": predicate_e, "P0-F": predicate_f, "P0-G": predicate_g, "P0-H": predicate_h}


def run_predicates(ctx, a):
    names = PREDICATES if a.predicate == "all" else (a.predicate,)
    if a.predicate == "all":
        names = tuple(name for name in PREDICATES if name != "P0-G" or platform.system() == "Linux")
    reports = []
    for name in names:
        got = PREDICATE_FNS[name](ctx, a)
        if got is None:
            return 2  # the predicate said why it cannot run here; that is not the bar's answer
        got.print()
        reports.append(got)
    failed = [r for r in reports if not r.ok]
    print(f"predicate {a.predicate}: {len(reports) - len(failed)} of {len(reports)} pass", flush=True)
    if a.expect_fail:
        # The unit's acceptance for a predicate no unit has closed yet: exit 0 when it does not hold,
        # and name what it found. A predicate that holds is a surprise, and a failure here.
        if failed:
            print(f"--expect-fail: {len(failed)} predicate(s) do not hold, as expected", flush=True)
            return 0
        print("--expect-fail: every predicate holds; nothing to expect", flush=True)
        return 1
    return 1 if failed else 0


# ---------------------------------------------------------------------------------------------
# The guard's self-test: synthetic allocators, never blaze.

ALLOCATE = r"""
import os, sys, time
mb, marker = int(sys.argv[1]), sys.argv[2]
b = bytearray(mb << 20)
for i in range(0, len(b), 4096):
    b[i] = 1
open(marker, "w").write("%d\n" % os.getpid())
while True:
    time.sleep(0.5)
"""

# A launcher that starts a small tree - the allocator and a sibling that holds nothing - in a
# session of its own and exits, so both are reparented to pid 1 and neither is a descendant of
# this harness any more: only the run's id in their environment can find them.
LAUNCH = r"""
import subprocess, sys
quiet = dict(stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
big = subprocess.Popen([sys.executable, "-c", sys.argv[1], sys.argv[2], sys.argv[3]],
                       start_new_session=True, **quiet)
small = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(600)"], **quiet)
open(sys.argv[3] + ".launched", "w").write("%d %d\n" % (big.pid, small.pid))
"""


class Case:
    def __init__(self, name, what):
        self.name, self.what, self.ok, self.said = name, what, True, []

    def fail(self, line):
        self.ok = False
        self.said.append(line)


def allocator(work, mb, env):
    """A synthetic tree of the run, reparented to pid 1: an allocator of `mb` MB, which holds the
    memory until it is killed, and a sibling that holds nothing. Returns the allocator's pid and
    the sibling's, once the allocator says it holds the memory."""
    os.makedirs(work, exist_ok=True)
    marker = os.path.join(work, "alloc-%d" % mb)
    launcher = os.path.join(work, "launch.py")
    with open(launcher, "w") as f:
        f.write(LAUNCH)
    subprocess.run([sys.executable, launcher, ALLOCATE, str(mb), marker], env=env, timeout=60)
    for _ in range(240):
        if os.path.exists(marker) and os.path.exists(marker + ".launched"):
            break
        time.sleep(0.25)
    if not os.path.exists(marker):
        raise SystemExit(f"the synthetic allocator did not start holding {mb} MB")
    big, small = (int(pid) for pid in open(marker + ".launched").read().split())
    return big, small


def wait_trip(wd, limit_s=30):
    began = time.monotonic()
    while time.monotonic() - began < limit_s:
        if wd.tripped:
            return wd.tripped
        time.sleep(0.25)
    return None


def alive(pid):
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    state = sh(["ps", "-o", "stat=", "-p", str(pid)]).stdout.strip()
    return bool(state) and "Z" not in state


def selftest(a):
    """The guard, on synthetic allocators and never on blaze: its footprint rule on a tree outside
    the binary's directory and reparented to pid 1 (the whole tree is killed at a 512 MB test
    limit), its two swap rules on a scripted swap series (and the free-space rule not firing when
    the same growth leaves room), another run's memory not counted, and the flags that would turn
    the guard off or raise a limit refused."""
    # A fixed directory under the temporary one, so the record of the last self-test is found
    # again: `selftest.json` there says which rules fired and on what.
    work = os.path.abspath(a.work) if a.work else \
        os.path.join(tempfile.gettempdir(), "ra-jit-tracedecay-selftest")
    os.makedirs(work, exist_ok=True)
    cases = []
    run_id = "selftest-%d" % os.getpid()
    env = dict(os.environ, **{RUN_ID_ENV: run_id})
    blaze_bin = os.path.join(work, "no-blaze-bin")  # nothing of the run is in it

    # 1. The footprint rule, on an allocator the run started outside the binary's directory and
    #    left reparented to pid 1 (ra-N4: "a child started outside the binary's directory that
    #    allocates past the test limit gets the tree killed").
    case = Case("footprint", "a tree outside the binary's directory, reparented to pid 1, past a 512 MB limit: "
                             "the whole tree is killed")
    wd = Watchdog(blaze_bin, limit_mb=512, interval=0.5, run_id=run_id)
    wd.start()
    pid, sibling = allocator(work, 1536, env)
    case.said.append(f"allocator pid {pid} and sibling pid {sibling}, {os.path.basename(sys.executable)}")
    rows = ps_table()
    if {pid, sibling} & descendants(rows, [os.getpid()]):
        case.fail("the tree is still a descendant of this harness: the case proves nothing")
    found = wd.pids(rows, set())
    if not {pid, sibling} <= found:
        case.fail(f"the watchdog did not find pids {pid} and {sibling} by their run id (found {sorted(found)})")
    trip = wait_trip(wd)
    if not trip:
        case.fail("the watchdog did not fire at the 512 MB limit")
    else:
        case.said.append(trip["reason"])
        if "512" not in trip["reason"]:
            case.fail(f"the reason does not name the 512 MB limit: {trip['reason']}")
    wd.stop.set()
    wd.join(timeout=5)
    kill_pids({pid, sibling})  # if the watchdog did not: so the next case starts on a clean machine
    time.sleep(1.5)
    for name, one in (("allocator", pid), ("sibling", sibling)):
        if alive(one):
            case.fail(f"the {name} {one} is still alive after the kill")
        else:
            case.said.append(f"{name} pid {one} killed")
    cases.append(case)

    # 1b. Another run's processes are not this run's: the other half of matching by the run's id,
    #     which is what keeps a neighbour's build from tripping this run's guard.
    case = Case("other-runs", "another run's allocator, and one of no run, are not counted")
    other = dict(env, **{RUN_ID_ENV: run_id + "-other"})
    other_pid, other_sibling = allocator(os.path.join(work, "other"), 1024, other)
    nobody = subprocess.Popen([sys.executable, "-c", ALLOCATE, "1024", os.path.join(work, "nobody")],
                              env={k: v for k, v in env.items() if k != RUN_ID_ENV},
                              stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1.5)
    wd = Watchdog(blaze_bin, limit_mb=512, interval=0.5, run_id=run_id)
    wd.start()
    trip = wait_trip(wd, 3)
    found = wd.pids(ps_table(), set())
    wd.stop.set()
    wd.join(timeout=5)
    if trip:
        case.fail(f"it fired on another run's memory: {trip['reason']}")
    if {other_pid, other_sibling, nobody.pid} & found:
        case.fail(f"it counted another run's pids ({sorted(found & {other_pid, other_sibling, nobody.pid})})")
    else:
        case.said.append(f"another run's pids {other_pid} and {other_sibling}, and pid {nobody.pid} of no run, not counted")
    kill_pids({other_pid, other_sibling, nobody.pid})
    cases.append(case)

    # 2. The swap rule: 1 GB grown since the start while under 3 GB is free.
    case = Case("swap-free", "swap grew 1.5 GB since the start and 2 GB is free, under the 3 GB floor")
    series = [(1000.0, 8000.0), (2500.0, 2000.0)]
    wd = Watchdog(blaze_bin, limit_mb=24576, interval=0.3, run_id=run_id,
                  swap_fn=lambda series=iter(series): next(series, (2500.0, 2000.0)))
    wd.start()
    trip = wait_trip(wd, 10)
    if not trip or "swap free" not in trip["reason"]:
        case.fail(f"the swap rule did not fire: {trip}")
    else:
        case.said.append(trip["reason"])
    wd.stop.set()
    wd.join(timeout=5)
    # ... and not when the same growth leaves 8 GB free: free swap alone is not the rule.
    case2 = Case("swap-free-floor", "the same 1.5 GB growth with 8 GB free does not fire")
    series = [(1000.0, 8000.0), (2500.0, 8000.0)]
    wd2 = Watchdog(blaze_bin, limit_mb=24576, interval=0.3, run_id=run_id,
                   swap_fn=lambda series=iter(series): next(series, (2500.0, 8000.0)))
    wd2.start()
    trip2 = wait_trip(wd2, 2)
    wd2.stop.set()
    wd2.join(timeout=5)
    if trip2:
        case2.fail(f"it fired: {trip2['reason']}")
    cases.append(case)
    cases.append(case2)

    # 3. The swap rule: 8 GB grown, free or not.
    case = Case("swap-growth", "swap grew 8.5 GB since the start, 8 GB free")
    series = [(1000.0, 8000.0), (9500.0, 8000.0)]
    wd = Watchdog(blaze_bin, limit_mb=24576, interval=0.3, run_id=run_id,
                  swap_fn=lambda series=iter(series): next(series, (9500.0, 8000.0)))
    wd.start()
    trip = wait_trip(wd, 10)
    if not trip or "swap grown" not in trip["reason"]:
        case.fail(f"the 8 GB rule did not fire: {trip}")
    else:
        case.said.append(trip["reason"])
    wd.stop.set()
    wd.join(timeout=5)
    cases.append(case)

    # 4. No flag turns the guard off or raises a limit: only lowers one.
    case = Case("guard-flags", "no flag turns the guard off, and none raises one of its limits")
    for argv, why in (
        (["--td", work, "--watchdog-mb", "0", "cold", "--tool", "blaze"], "--watchdog-mb 0"),
        (["--td", work, "--watchdog-mb", str(48 * GB), "cold", "--tool", "blaze"], "--watchdog-mb 48G"),
        (["--td", work, "--memory-max", "32G", "cold", "--tool", "blaze"], "--memory-max 32G"),
        (["--td", work, "--own-base-memory", "24G", "cold", "--tool", "blaze"], "--own-base-memory 24G"),
        (["--td", work, "--no-guard", "cold", "--tool", "blaze"], "--no-guard"),
    ):
        if not refused(argv):
            case.fail(f"{why} was accepted")
        else:
            case.said.append(f"{why} refused")
    if not refused(["--td", work, "--memory-max", "2G", "--watchdog-mb", "512", "--swap-free-mb", "1024",
                    "--swap-grow-mb", "512", "--swap-grow-hard-mb", "2048", "cold", "--tool", "blaze"]):
        case.fail("a lowered limit was refused; only raising is refused")
    else:
        case.said.append("lowered limits accepted")
    cases.append(case)

    record = {"time": time.strftime("%FT%T"), "work": work, "run_id": run_id,
              "cases": [{"name": c.name, "what": c.what, "ok": c.ok, "said": c.said} for c in cases]}
    with open(os.path.join(work, "selftest.json"), "w") as f:
        json.dump(record, f, indent=1)
    for c in cases:
        print(f"self-test {c.name}: {'PASS' if c.ok else 'FAIL'} - {c.what}", flush=True)
        for said in c.said:
            print("  " + said, flush=True)
    print(f"self-test: {sum(1 for c in cases if c.ok)} of {len(cases)} pass; recorded in {os.path.join(work, 'selftest.json')}", flush=True)
    return 0 if all(c.ok for c in cases) else 1


def refused(argv):
    """Whether the argument parser refuses `argv` (it exits, or exits 2 for an unknown flag). The
    parser's usage goes to stderr, which is what a refusal is: it is not printed here."""
    err = sys.stderr
    try:
        sys.stderr = open(os.devnull, "w")
        parse_args(argv)
    except SystemExit as e:
        return e.code not in (0, None)
    finally:
        sys.stderr.close()
        sys.stderr = err
    return False


# ---------------------------------------------------------------------------------------------
# The argument parser, and what may not be weakened.


def parse_args(argv):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--td", help="the project to build (a worktree of its own for tracedecay)")
    p.add_argument("--blaze-bin", help="directory with blaze, cargo-blaze and ra-jit (default: the directory of "
                                       "$RA_JIT_BIN, else <repo>/target/release)")
    p.add_argument("--work", help="results, logs, blaze's daemon directories and stores, the private CARGO_HOME "
                                  "(default: <td>/target/.tracedecay.noindex)")
    p.add_argument("--package", default=None, help="the package to build (default: %s, or the project's own)" % CLI_PACKAGE)
    p.add_argument("--program", default=None, help="the program the build makes, under target/<tool>/debug (default: the package)")
    p.add_argument("--program-args", nargs="*", default=[], help="arguments for the program")
    p.add_argument("--rustc-bootstrap", action="store_true", help="RUSTC_BOOTSTRAP=1 for the 1.99.0 tools")
    p.add_argument("--build-flags", default="", help="more flags for every `build`, e.g. --keep-going")
    p.add_argument("--memory-max", help="RA_JIT_MEMORY_MAX for blaze, at most the guard's " + GUARD_ENV["RA_JIT_MEMORY_MAX"] +
                                       " (lower for a test)")
    p.add_argument("--own-base-memory", help="RA_JIT_OWN_BASE_MEMORY for blaze, at most the guard's " +
                                            GUARD_ENV["RA_JIT_OWN_BASE_MEMORY"] + " (lower for a test)")
    p.add_argument("--watchdog-mb", type=int, default=WATCHDOG_MB,
                   help="kill the run's processes (and the client) when they hold this many MB of physical footprint together "
                        f"(default {WATCHDOG_MB}); never 0, never above the guard's {WATCHDOG_MB}")
    p.add_argument("--swap-free-mb", type=int, default=SWAP_FREE_MIN_MB,
                   help=f"kill when swap has grown 1 GB and less than this is free (default {SWAP_FREE_MIN_MB}; lower only)")
    p.add_argument("--swap-grow-mb", type=int, default=SWAP_GROWTH_MIN_MB,
                   help=f"the swap growth the free-space rule needs (default {SWAP_GROWTH_MIN_MB}; lower only)")
    p.add_argument("--swap-grow-hard-mb", type=int, default=SWAP_GROWTH_MAX_MB,
                   help=f"swap grown by this much kills, free or not (default {SWAP_GROWTH_MAX_MB}; lower only)")
    p.add_argument("--interval", type=float, default=2.0, help="seconds between watchdog samples (2)")
    p.add_argument("--predicate", choices=PREDICATES + ("all",), help="run one Phase 0 predicate's scenarios")
    p.add_argument("--expect-fail", action="store_true",
                   help="with --predicate: exit 0 when the predicate does not hold, naming what it found")
    p.add_argument("--self-test", action="store_true", help="the guard's three rules, each on a synthetic allocator")
    p.add_argument("--doc", action=argparse.BooleanOptionalAction, default=True,
                   help="P0-A builds and runs the doctests too (the default; --no-doc leaves them out "
                        "for a package that has no library)")
    p.add_argument("--whole", action="store_true",
                   help="P0-A of the default owned build: the build and the program, 0 compiler and 0 cargo execs")
    p.add_argument("--whole-edit", nargs=3, action="append", metavar=("FILE", "OLD", "NEW"),
                   help="with --whole: an edit built after the cold build (FILE relative to --td); may be repeated")
    p.add_argument("--pairs", type=int, default=20, help="P0-C's edit/undo pairs (20)")
    p.add_argument("--runs", type=int, default=None,
                   help="how many runs per arm a predicate does (P0-D 5, P0-H 3); with a command, the runs of a scenario")
    sub = p.add_subparsers(dest="cmd")
    for name in ("prepare", "versions", "cold", "noop", "edit", "test", "status", "stop", "watch", "summary"):
        s = sub.add_parser(name)
        if name == "watch":
            s.add_argument("--seconds", type=int, default=60)
        if name in ("cold", "noop", "edit", "test", "status", "stop", "watch"):
            s.add_argument("--tool", choices=TOOLS, required=True)
        if name in ("cold", "noop", "edit", "test"):
            s.add_argument("--runs", type=int, default=None)
        if name in ("cold", "edit"):
            s.add_argument("--status", action="store_true", help="print `blaze status` afterwards")
        if name == "cold":
            s.add_argument("--stop", action="store_true", help="stop the daemon after the last run")
            s.add_argument("--first-run", type=int, default=1, help="number of the first run in the records (a second batch)")
        if name == "edit":
            s.add_argument("--edit", choices=("tracedecay-body", "mcp-body", "signature"), required=True)
        if name == "test":
            s.add_argument("--package", dest="package")
            s.add_argument("--cold", action="store_true")
            s.add_argument("rest", nargs="*")
        if name == "summary":
            s.add_argument("files", nargs="*")
    a = p.parse_args(argv)
    if a.runs is None:
        # P0-D counts 5 runs x 3 edit kinds x an edit and its revert (docs/distributed.md, [rb-N2]);
        # every other predicate and scenario does 3, the bar's n.
        a.runs = 5 if a.predicate == "P0-D" else 3
    validate(a)
    return a


def validate(a):
    """The guard cannot be turned off, and no flag raises one of its limits."""
    if a.watchdog_mb <= 0:
        sys.exit("--watchdog-mb must be positive: blaze never runs without the watchdog")
    if a.watchdog_mb > WATCHDOG_MB:
        sys.exit(f"--watchdog-mb {a.watchdog_mb}: the guard's limit is {WATCHDOG_MB} MB and a flag may only lower it")
    if a.swap_free_mb > SWAP_FREE_MIN_MB:
        sys.exit(f"--swap-free-mb {a.swap_free_mb}: the guard's floor is {SWAP_FREE_MIN_MB} MB and a flag may only lower it")
    if a.swap_grow_mb > SWAP_GROWTH_MIN_MB:
        sys.exit(f"--swap-grow-mb {a.swap_grow_mb}: the guard's growth is {SWAP_GROWTH_MIN_MB} MB and a flag may only lower it")
    if a.swap_grow_hard_mb > SWAP_GROWTH_MAX_MB:
        sys.exit(f"--swap-grow-hard-mb {a.swap_grow_hard_mb}: the guard's growth is {SWAP_GROWTH_MAX_MB} MB and a flag may only lower it")
    for flag, given, ceiling in (("--memory-max", a.memory_max, GUARD_ENV["RA_JIT_MEMORY_MAX"]),
                                 ("--own-base-memory", a.own_base_memory, GUARD_ENV["RA_JIT_OWN_BASE_MEMORY"])):
        if not given:
            continue
        try:
            got, top = parse_mb(given), parse_mb(ceiling)
        except ValueError as e:
            sys.exit(f"{flag}: {e}")
        if got > top:
            sys.exit(f"{flag} {given}: the guard's bound is {ceiling} and a flag may only lower it")
    if a.interval <= 0:
        sys.exit("--interval must be positive")
    if a.self_test:
        return
    if not a.td:
        sys.exit("--td names the project to build (or --self-test)")
    if not a.predicate and not a.cmd:
        sys.exit("a COMMAND or --predicate or --self-test is needed")
    if not a.blaze_bin:
        ra_jit = os.environ.get("RA_JIT_BIN")
        a.blaze_bin = os.path.dirname(os.path.abspath(ra_jit)) if ra_jit else \
            os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release")
    if not os.path.exists(os.path.join(a.blaze_bin, "blaze")):
        # A blaze on PATH, as a release installed with `cargo install` is, with its ra-jit beside it.
        found = shutil.which("blaze")
        if found:
            a.blaze_bin = os.path.dirname(os.path.abspath(found))
    if not os.path.exists(os.path.join(a.blaze_bin, "blaze")):
        sys.exit(f"--blaze-bin {a.blaze_bin} has no blaze: build it, or name it with --blaze-bin, "
                 "$RA_JIT_BIN or PATH")
    if not a.work:
        a.work = os.path.join(os.path.abspath(a.td), "target", ".tracedecay.noindex")
    fixture = fixture_of(a.td)
    if a.package is None:
        if fixture and fixture.get("package"):
            a.package = fixture["package"]
        else:
            names = package_names(a.td)
            a.package = CLI_PACKAGE if CLI_PACKAGE in names else os.path.basename(os.path.abspath(a.td))
    if a.program is None:
        a.program = (fixture or {}).get("program") or a.package
    if not a.program_args and fixture and fixture.get("program_args"):
        a.program_args = list(fixture["program_args"])


def fixture_of(td):
    """The `harness/dist/fixtures.toml` entry for `td`, when `td` is a fixture fetch.py wrote: its
    package, program and program arguments are what the predicates build and run, so a fixture is
    named in one place."""
    base = os.path.basename(os.path.abspath(td).rstrip(os.sep))
    for fix in fetch.fixtures():
        if fix["name"] == base:
            return fix
    return None




def main():
    a = parse_args(None)
    if a.self_test:
        sys.exit(selftest(a))
    ctx = Ctx(a)
    EXTRA_FLAGS.extend(a.build_flags.split())
    if getattr(a, "tool", "cargo").startswith("blaze") or a.cmd == "watch" or a.predicate:
        series = os.path.join(ctx.logs, "watchdog-series.jsonl")
        start_watchdog(ctx, a, series=series)
        print(f"guard: {ctx.guard}, watchdog {a.watchdog_mb} MB every {ctx.watchdog.interval:.0f} s, "
              f"run {ctx.run_id}, audit {ctx.audit_log}", flush=True)
    if a.predicate:
        sys.exit(run_predicates(ctx, a))
    globals()["cmd_" + a.cmd](ctx, a)


if __name__ == "__main__":
    main()
