#!/usr/bin/env python3
"""tracedecay.py: blaze against cargo on tracedecay's Rust workspace, on one machine.

    tracedecay.py --td DIR --blaze-bin DIR --work DIR COMMAND [options]

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

Memory guard (blaze tools; it cannot be turned off): blaze's documented memory bounds do not apply to a
worker that is loading or to the one asked last (docs/reports/TRACEDECAY.md, item 7), and loading
tracedecay took a worker past 56 GB on a 48 GiB Mac. So every blaze command runs with
RA_JIT_MEMORY_MAX=16G, RA_JIT_OWN_BASE_MEMORY=12G and RA_JIT_SPARE=0 (--memory-max and
--own-base-memory change the first two), and a watchdog thread, every 2 s, sums the physical footprint
(macOS `footprint`; it counts compressed and swapped-out memory, RSS does not) of the processes whose
executable is in --blaze-bin, and what the daemon and workers started, but not the cargo and rustc of
the client being timed. It kills them all, and the client, when the sum passes --watchdog-mb (default
24576), or when swap has grown by 1 GB and less than --swap-free-mb (default 3072) is free, or swap has
grown by 8 GB. The kill is a result: "outcome": "watchdog-killed" in results.jsonl, with the reason and
the numbers, and the footprint of every sample in logs/watchdog-series.jsonl. Every record also has
"rajit_footprint_peak_mb". Test it on a short run first, with a limit below what a hello-world worker
holds: `--watchdog-mb 512 cold --tool blaze` on a hello-world project is killed after about 3 s.

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
    blaze         `blaze build`: cargo builds first with blaze's wrapper, then the daemon takes over
    blaze-whole   `BLAZE_WHOLE_BUILD=1`: blaze builds the program itself, with no cargo build
    blaze-nightly `BLAZE_OTHER_TOOLCHAIN=1`: blaze builds with the project's own toolchain

Each tool builds into a directory of its own below the tracedecay checkout's `target/`, the one
directory its .gitignore names. That matters: the CLI's build script watches every file git does
not ignore, so a target directory anywhere else in the checkout changes what the script prints on
every build, and blaze refuses to trust a script that prints something else on a second run. The
directories end in `.noindex` so that Spotlight does not index them. Blaze's daemon directory
and store are under --work, one pair per tool, so no two tools share state. Every tool uses
one private CARGO_HOME (--work/cargo-home, a copy of ~/.cargo/registry and ~/.cargo/git), so a cargo
run by someone else on the machine does not hold the package cache lock this one waits on.

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
import argparse, json, os, platform, re, shutil, signal, statistics, subprocess, sys, threading, time

CLI_PACKAGE = "tracedecay-cli"
TOOLS = ("cargo", "cargo199", "blaze", "blaze-whole", "blaze-nightly")

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

    def target(self, tool):
        return os.path.join(self.td, "target", tool + ".noindex")

    def blaze_home(self, tool):
        return os.path.join(self.work, "home-" + tool + ".noindex")

    def store(self, tool):
        return os.path.join(self.work, "store-" + tool + ".noindex")

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
            e["PATH"] = self.blaze_bin + os.pathsep + e["PATH"]
            e["RA_JIT_HOME"] = self.blaze_home(tool)
            e["RA_JIT_CACHE"] = self.store(tool)
            e.update(self.guard)
            e.pop("RUSTUP_TOOLCHAIN", None)  # blaze names 1.99.0 itself
            if self.bootstrap:
                e["RUSTC_BOOTSTRAP"] = "1"
            if tool == "blaze-whole":
                e["BLAZE_WHOLE_BUILD"] = "1"
            if tool == "blaze-nightly":
                e["BLAZE_OTHER_TOOLCHAIN"] = "1"
        return e

    def exe(self, tool):
        return "blaze" if tool.startswith("blaze") else "cargo"

    def binary(self, tool):
        return os.path.join(self.target(tool), "debug", "tracedecay")


def ps_table():
    out = sh(["ps", "-Ao", "pid=,ppid=,pcpu=,rss=,command="]).stdout
    rows = []
    for line in out.splitlines():
        parts = line.split(None, 4)
        if len(parts) < 5:
            continue
        try:
            rows.append((int(parts[0]), int(parts[1]), float(parts[2]), int(parts[3]), parts[4]))
        except ValueError:
            pass
    return rows


def swap_used_mb():
    out = sh(["sysctl", "-n", "vm.swapusage"]).stdout
    m = re.search(r"used = ([\d.]+)M", out)
    return float(m.group(1)) if m else 0.0


def blaze_footprint_mb(prefix):
    """The physical footprint (macOS `footprint`, which counts memory swapped out or compressed)
    of blaze's processes now, in MB. RSS leaves that memory out."""
    pids = [r[0] for r in ps_table() if r[4].startswith(prefix)]
    total = 0.0
    for pid in pids:
        out = sh(["footprint", "-p", str(pid), "--noCategories"]).stdout
        m = re.search(r"phys_footprint: ([\d.]+) (KB|MB|GB)", out)
        if m:
            total += float(m.group(1)) * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[m.group(2)]
    return round(total)


MB = 1
GB = 1024 * MB

# Every blaze command runs with these (the daemon reads them once, when it starts). Workers together
# hold at most MEMORY_MAX, and a worker building its own base at most OWN_BASE_MEMORY. Neither bounds
# a worker that is loading or the one asked last (docs/reports/TRACEDECAY.md, item 5), which is what
# the watchdog below is for.
GUARD_ENV = {"RA_JIT_MEMORY_MAX": "16G", "RA_JIT_OWN_BASE_MEMORY": "12G", "RA_JIT_SPARE": "0"}
WATCHDOG_MB = 24 * GB
SWAP_FREE_MIN_MB = 3 * GB
SWAP_GROWTH_MIN_MB = 1 * GB
SWAP_GROWTH_MAX_MB = 8 * GB


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
        for pid in pids:
            args += ["-p", str(pid)]
        out = sh(args).stdout
        found = {}
        for m in re.finditer(r"\[(\d+)\]: \S+\s+Footprint: ([\d.]+) (KB|MB|GB)", out):
            found[int(m.group(1))] = float(m.group(2)) * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[m.group(3)]
        return found
    return {r[0]: r[3] / 1024 for r in ps_table() if r[0] in pids}


def descendants(rows, roots):
    kids = {}
    for pid, ppid, _, _, _ in rows:
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
    """Every `interval` seconds, the summed footprint of every process of this blaze (the ones whose
    executable is in `prefix`, the daemon, its workers and wrappers, and what the daemon and workers
    started; not the cargo and rustc that a `blaze build` client runs, and not another checkout's blaze).
    Kills them all, and the client's tree, when the sum passes `limit_mb`, or when swap has grown by
    SWAP_GROWTH_MIN_MB or more since the watchdog started and fewer than `swap_free_min_mb` of it is
    left (or swap grew by SWAP_GROWTH_MAX_MB, free or not). The kill is an outcome, not an error: it is
    in `tripped` and `take()` hands it to the run's record.

    Swap `free` alone cannot be the rule on macOS: its swap files grow on demand, and on this Mac it
    was 0.9 of 8.2 GB before any run started, from the previous run's pages."""

    def __init__(self, prefix, limit_mb=WATCHDOG_MB, swap_free_min_mb=SWAP_FREE_MIN_MB, interval=2.0, series=None):
        super().__init__(daemon=True)
        self.series = open(series, "a", buffering=1) if series else None  # one JSON line per sample
        self.prefix = prefix + os.sep
        self.limit_mb, self.swap_free_min_mb, self.interval = limit_mb, swap_free_min_mb, interval
        self.client = None  # callable returning the pid of the `blaze` client being timed
        self.stop = threading.Event()
        self.tripped = None
        self.peak_mb = 0.0
        self.last_mb = 0.0
        self.samples = 0
        self.swap_start_used, _ = swap_mb()
        self.peak_swap_growth_mb = 0.0

    def pids(self, rows, client_tree):
        mine = {r[0] for r in rows if r[4].startswith(self.prefix)}
        detached = {p for p in mine if p not in client_tree}
        return mine | descendants(rows, detached)

    def sample(self):
        rows = ps_table()
        client = self.client() if self.client else None
        client_tree = descendants(rows, [client]) if client else set()
        pids = self.pids(rows, client_tree)
        total = sum(footprints_mb(sorted(pids)).values())
        used, free = swap_mb()
        growth = used - self.swap_start_used
        self.last_mb = total
        self.peak_mb = max(self.peak_mb, total)
        self.peak_swap_growth_mb = max(self.peak_swap_growth_mb, growth)
        self.samples += 1
        if self.series and pids:
            self.series.write(json.dumps({"t": time.strftime("%T"), "mb": round(total), "pids": len(pids),
                                          "swap_used_mb": round(used), "swap_free_mb": round(free)}) + "\n")
        reason = None
        if total > self.limit_mb:
            reason = f"footprint of the blaze processes {total / GB:.1f} GB passed {self.limit_mb / GB:.1f} GB"
        elif free < self.swap_free_min_mb and growth >= SWAP_GROWTH_MIN_MB:
            reason = f"swap free {free / GB:.1f} GB under {self.swap_free_min_mb / GB:.1f} GB, grown {growth / GB:.1f} GB"
        elif growth >= SWAP_GROWTH_MAX_MB:
            reason = f"swap grown {growth / GB:.1f} GB"
        if reason and not self.tripped:
            self.tripped = {"reason": reason, "footprint_mb": round(total), "swap_used_mb": round(used),
                            "swap_free_mb": round(free), "time": time.strftime("%FT%T"), "pids": len(pids)}
            print("WATCHDOG:", reason, f"- killing {len(pids)} blaze processes", flush=True)
            kill_pids(pids)
            if client:
                kill_pids(client_tree, signal.SIGTERM)
                time.sleep(2)
                kill_pids(client_tree & {r[0] for r in ps_table()})

    def run(self):
        while not self.stop.is_set():
            try:
                self.sample()
            except Exception as e:  # a failed sample must not end the watch
                print("WATCHDOG: sample failed:", e, flush=True)
            self.stop.wait(self.interval)

    def take(self):
        """The peak footprint and the kill, if any, since the last call."""
        got = {"rajit_footprint_peak_mb": round(self.peak_mb), "watchdog_limit_mb": round(self.limit_mb),
               "watchdog_samples": self.samples, "swap_growth_peak_mb": round(self.peak_swap_growth_mb)}
        if self.tripped:
            got["watchdog"] = self.tripped
        self.peak_mb = self.last_mb
        self.samples = 0
        self.peak_swap_growth_mb = 0.0
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
            rows = ps_table()
            root = self.root_pid_fn()
            kids = {}
            for pid, ppid, _, _, _ in rows:
                kids.setdefault(ppid, []).append(pid)
            tree = set()
            if root:
                todo = [root]
                while todo:
                    p = todo.pop()
                    if p not in tree:
                        tree.add(p)
                        todo.extend(kids.get(p, []))
            tree_kb = sum(r[3] for r in rows if r[0] in tree)
            blaze_pids = {r[0] for r in rows if self.blaze_prefix and r[4].startswith(self.blaze_prefix)}
            blaze_kb = sum(r[3] for r in rows if r[0] in blaze_pids)
            total_kb = sum(r[3] for r in rows if r[0] in tree | blaze_pids)
            foreign = noise = 0.0
            for pid, _, cpu, _, cmd in rows:
                if pid in tree or pid in blaze_pids or pid == me:
                    continue
                if NOISE.search(cmd):
                    noise += cpu
                elif cpu >= 5 and BUILDERS.search(cmd + " "):
                    foreign += cpu
            self.peak_tree_kb = max(self.peak_tree_kb, tree_kb)
            self.peak_blaze_kb = max(self.peak_blaze_kb, blaze_kb)
            self.peak_total_kb = max(self.peak_total_kb, total_kb)
            self.foreign.append(foreign / 100)
            self.noise.append(noise / 100)
            self.peak_swap_mb = max(self.peak_swap_mb, swap_used_mb())
            self.stop.wait(1.0)


def timed(ctx, tool, scenario, args, run, extra=None, cwd=None, check=False, record=True, foot=False):
    """Runs `tool args` under /usr/bin/time -l; returns the record."""
    env = ctx.env(tool)
    log = os.path.join(ctx.logs, f"{tool}-{scenario}-{run}.log")
    cmd = ["/usr/bin/time", "-l", ctx.exe(tool)] + args
    blaze_prefix = os.path.join(ctx.blaze_bin, "ra-jit") if tool.startswith("blaze") else None
    load = os.getloadavg()
    holder = {}
    t0 = time.monotonic()
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
    print(json.dumps({k: rec[k] for k in ("tool", "scenario", "run", "wall_s", "exit", "outcome", "finished", "foreign_build_cores_peak", "rajit_footprint_peak_mb") if k in rec}), flush=True)
    if check and code:
        sys.exit(f"{tool} {scenario} failed ({code}); see {log}")
    return rec


def du_mb(path):
    if not os.path.exists(path):
        return 0
    out = sh(["du", "-sk", path]).stdout.split()
    return round(int(out[0]) / 1024) if out else 0


def stop_daemon(ctx, tool):
    if tool.startswith("blaze"):
        sh([ctx.exe(tool), "stop"], env=ctx.env(tool), cwd=ctx.td)


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
    print("stopped; blaze processes left:", [r[0] for r in ps_table() if r[4].startswith(ctx.blaze_bin + os.sep)])


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
    r = sh([ctx.exe(a.tool), "status"], env=ctx.env(a.tool), cwd=ctx.td)
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


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--td", required=True, help="the tracedecay checkout (a worktree of its own)")
    p.add_argument("--blaze-bin", required=True, help="directory with blaze, cargo-blaze and ra-jit")
    p.add_argument("--work", required=True, help="results, logs, blaze's daemon directories and stores, the private CARGO_HOME")
    p.add_argument("--package", default=CLI_PACKAGE)
    p.add_argument("--rustc-bootstrap", action="store_true", help="RUSTC_BOOTSTRAP=1 for the 1.99.0 tools")
    p.add_argument("--build-flags", default="", help="more flags for every `build`, e.g. --keep-going")
    p.add_argument("--memory-max", help="RA_JIT_MEMORY_MAX for blaze, default " + GUARD_ENV["RA_JIT_MEMORY_MAX"])
    p.add_argument("--own-base-memory", help="RA_JIT_OWN_BASE_MEMORY for blaze, default " + GUARD_ENV["RA_JIT_OWN_BASE_MEMORY"])
    p.add_argument("--watchdog-mb", type=int, default=WATCHDOG_MB,
                   help="kill every blaze process (and the client) when they hold this many MB of physical footprint together "
                        f"(default {WATCHDOG_MB}); cannot be 0 or off for a blaze tool")
    p.add_argument("--swap-free-mb", type=int, default=SWAP_FREE_MIN_MB, help="kill when swap has grown 1 GB and less than this is free")
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("prepare", "versions", "cold", "noop", "edit", "test", "status", "stop", "watch", "summary"):
        s = sub.add_parser(name)
        if name == "watch":
            s.add_argument("--seconds", type=int, default=60)
        if name in ("cold", "noop", "edit", "test", "status", "stop", "watch"):
            s.add_argument("--tool", choices=TOOLS, required=True)
        if name in ("cold", "noop", "edit", "test"):
            s.add_argument("--runs", type=int, default=3)
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
    a = p.parse_args()
    if a.watchdog_mb <= 0:
        sys.exit("--watchdog-mb must be positive: blaze never runs without the watchdog")
    ctx = Ctx(a)
    EXTRA_FLAGS.extend(a.build_flags.split())
    if getattr(a, "tool", "cargo").startswith("blaze") or a.cmd == "watch":
        ctx.watchdog = Watchdog(ctx.blaze_bin, a.watchdog_mb, a.swap_free_mb, series=os.path.join(ctx.logs, "watchdog-series.jsonl"))
        ctx.watchdog.start()
        print(f"guard: {ctx.guard}, watchdog {a.watchdog_mb} MB every {ctx.watchdog.interval:.0f} s", flush=True)
    if not hasattr(a, "package") or a.package is None:
        a.package = CLI_PACKAGE
    globals()["cmd_" + a.cmd](ctx, a)


if __name__ == "__main__":
    main()
