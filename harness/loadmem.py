#!/usr/bin/env python3
"""loadmem.py: what a blaze worker holds while it loads a workspace, under the memory guard.

    loadmem.py --td DIR --blaze-bin DIR --work DIR [options] [-- COMMAND...]

Runs COMMAND (default `blaze build --locked -p tracedecay-cli`) in DIR with a private
`RA_JIT_HOME`, store and `CARGO_HOME` below --work, and the memory guard of the distributed
program: `RA_JIT_MEMORY_MAX=16G`, `RA_JIT_OWN_BASE_MEMORY=12G`, `RA_JIT_SPARE=0`, and a watchdog
that kills blaze's daemon, workers and what they started when their summed physical footprint
passes --watchdog-mb (24576), or the client's cargo and rustc (not part of that sum) pass --client-mb
(32768), or both together pass --total-mb (40960), or when swap has grown by 1 GB and less than 3 GB of it is free, or
by 8 GB. The footprint is macOS `footprint`, which counts compressed and swapped-out memory that
RSS leaves out; elsewhere it is RSS. macOS grows its swap files on demand, so "free swap" alone
is not a rule (it is under 3 GB before any run), hence the growth condition.

What it records, below --work/NAME (NAME defaults to the time):

    series.jsonl    every --every seconds: the footprint of each blaze process over 100 MB, their
                    sum (the daemon and what it started), the client tree's sum (cargo and rustc of the build
                    being timed, with the ra-jit wrapper and build-script processes), swap
    phases.jsonl    each new line of the worker logs and the client's output, with the time it
                    was seen and the footprint then: which phase the worker was in
    snap/           `footprint` with its categories of the biggest process, when the sum first
                    passes 2, 4, 8, 12, 16, 20 and 24 GB, and at the end
    mem/            what the worker writes (`RA_JIT_MEMORY`): ticks.tsv, `at-<MB>.prof` heap
                    profiles with --prof, the ingredient tables if it gets to a build
    logs/           copies of the worker and daemon logs
    result.json     the outcome (ok, failed, watchdog-killed), the peak and when the marks were passed

The worker is `ra-jit` started by the daemon; the ticks are `RA_JIT_MEMORY_EVERY` (--memory-every
seconds), which needs a binary with `memory::ticks` (docs/reports/LOADMEM.md). Heap profiles need
the `heap-profile` feature. Hold the heavy lock while this runs (standing order 15). Python 3
standard library only; macOS and Linux.
"""
import argparse, json, os, platform, re, shutil, signal, subprocess, sys, threading, time

GB = 1024
DARWIN = platform.system() == "Darwin"
GUARD = {"RA_JIT_MEMORY_MAX": "16G", "RA_JIT_OWN_BASE_MEMORY": "12G", "RA_JIT_SPARE": "0"}
MARKS = [2, 4, 8, 12, 16, 20, 24]


def sh(args, **kw):
    kw.setdefault("text", True)
    kw.setdefault("stdout", subprocess.PIPE)
    kw.setdefault("stderr", subprocess.STDOUT)
    return subprocess.run(args, **kw)


def ps_table():
    rows = []
    for line in sh(["ps", "-Ao", "pid=,ppid=,rss=,command="]).stdout.splitlines():
        parts = line.split(None, 3)
        if len(parts) == 4:
            try:
                rows.append((int(parts[0]), int(parts[1]), int(parts[2]), parts[3]))
            except ValueError:
                pass
    return rows


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


def footprints_mb(pids, rows):
    """pid -> MB held. macOS: `footprint`, which counts compressed and swapped memory. Else RSS."""
    if not pids:
        return {}
    if DARWIN and shutil.which("footprint"):
        args = ["footprint", "--noCategories"]
        for pid in sorted(pids):
            args += ["-p", str(pid)]
        found = {}
        for m in re.finditer(r"\[(\d+)\]: \S+\s+Footprint: ([\d.]+) (KB|MB|GB)", sh(args).stdout):
            found[int(m.group(1))] = float(m.group(2)) * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[m.group(3)]
        return found
    return {r[0]: r[2] / 1024 for r in rows if r[0] in pids}


def swap_mb():
    """(used, free) swap in MB."""
    if DARWIN:
        out = sh(["sysctl", "-n", "vm.swapusage"]).stdout
        used, free = re.search(r"used = ([\d.]+)M", out), re.search(r"free = ([\d.]+)M", out)
        return (float(used.group(1)) if used else 0.0, float(free.group(1)) if free else 0.0)
    try:
        info = dict(re.findall(r"(\w+):\s+(\d+) kB", open("/proc/meminfo").read()))
        total, free = int(info["SwapTotal"]) / 1024, int(info["SwapFree"]) / 1024
        return (total - free, free)
    except (OSError, KeyError):
        return (0.0, float("inf"))


def kill(pids, sig=signal.SIGKILL):
    for pid in pids:
        try:
            os.kill(pid, sig)
        except OSError:
            pass


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--td", required=True, help="the checkout to build")
    ap.add_argument("--blaze-bin", required=True, help="the directory of blaze, cargo-blaze and ra-jit")
    ap.add_argument("--work", required=True, help="where the home, the store, CARGO_HOME and the results go")
    ap.add_argument("--name", default=time.strftime("%m%d-%H%M%S"))
    ap.add_argument("--cargo-home-from", help="a CARGO_HOME to clone (copy on write) as --work/cargo-home")
    ap.add_argument("--target-name", default="loadmem.noindex", help="below DIR/target, which .gitignore names")
    ap.add_argument("--watchdog-mb", type=int, default=24 * GB)
    ap.add_argument("--client-mb", type=int, default=32 * GB, help="the client's cargo and rustc, summed")
    ap.add_argument("--total-mb", type=int, default=40 * GB, help="blaze's processes and the client's, summed")
    ap.add_argument("--count-client", action="store_true",
                    help="the command is itself the process measured (rust-analyzer): its tree is what the cap counts")
    ap.add_argument("--swap-free-mb", type=int, default=3 * GB)
    ap.add_argument("--every", type=float, default=2.0, help="seconds between samples")
    ap.add_argument("--memory-every", type=float, default=5.0, help="RA_JIT_MEMORY_EVERY; 0 for none")
    ap.add_argument("--step-mb", type=int, default=1024, help="RA_JIT_MEMORY_STEP_MB")
    ap.add_argument("--prof", action="store_true", help="_RJEM_MALLOC_CONF=prof:true (needs --features heap-profile)")
    ap.add_argument("--lg-prof-sample", type=int, default=18)
    ap.add_argument("--timeout", type=int, default=3600)
    ap.add_argument("--memory-max", help="RA_JIT_MEMORY_MAX instead of 16G")
    ap.add_argument("--own-base-memory", help="RA_JIT_OWN_BASE_MEMORY instead of 12G")
    ap.add_argument("--env", action="append", default=[], help="NAME=VALUE for the daemon and the client")
    ap.add_argument("command", nargs="*", help="after --; default: blaze build --locked -p tracedecay-cli")
    a = ap.parse_args()

    td, blaze_bin, work = (os.path.abspath(p) for p in (a.td, a.blaze_bin, a.work))
    out = os.path.join(work, a.name)
    for sub in ("snap", "mem", "logs"):
        os.makedirs(os.path.join(out, sub), exist_ok=True)
    home, store = os.path.join(work, "home.noindex"), os.path.join(work, "store.noindex")
    cargo_home = os.path.join(work, "cargo-home")
    if not os.path.isdir(cargo_home) and a.cargo_home_from:
        sh(["cp", "-cR", a.cargo_home_from, cargo_home], check=True)
    env = dict(os.environ, RUST_BACKTRACE="0", RA_JIT_HOME=home, RA_JIT_CACHE=store, CARGO_HOME=cargo_home,
               CARGO_TARGET_DIR=os.path.join(td, "target", a.target_name),
               PATH=blaze_bin + os.pathsep + os.environ["PATH"], **GUARD)
    env.pop("RUSTUP_TOOLCHAIN", None)
    if a.memory_max:
        env["RA_JIT_MEMORY_MAX"] = a.memory_max
    if a.own_base_memory:
        env["RA_JIT_OWN_BASE_MEMORY"] = a.own_base_memory
    env["RA_JIT_MEMORY"] = os.path.join(out, "mem")
    if a.memory_every > 0:
        env["RA_JIT_MEMORY_EVERY"] = str(a.memory_every)
        env["RA_JIT_MEMORY_STEP_MB"] = str(a.step_mb)
    if a.prof:
        env["_RJEM_MALLOC_CONF"] = f"prof:true,lg_prof_sample:{a.lg_prof_sample}"
    for kv in a.env:
        k, _, v = kv.partition("=")
        env[k] = v
    command = a.command or ["blaze", "build", "--locked", "-p", "tracedecay-cli"]
    if command and command[0] == "--":
        command = command[1:]

    prefix = blaze_bin + os.sep
    series = open(os.path.join(out, "series.jsonl"), "a", buffering=1)
    phases = open(os.path.join(out, "phases.jsonl"), "a", buffering=1)
    swap_start, _ = swap_mb()
    t0 = time.time()
    client_log = open(os.path.join(out, "logs", "client.log"), "w")
    client = subprocess.Popen(command, cwd=td, env=env, stdout=client_log, stderr=subprocess.STDOUT,
                              start_new_session=True)
    result = {"command": command, "td": td, "blaze_bin": blaze_bin, "started": time.strftime("%FT%T"),
              "watchdog_mb": a.watchdog_mb, "marks": {}, "tripped": None, "peak_mb": 0, "peak_client_mb": 0,
              "swap_start_mb": round(swap_start), "guard": {k: env[k] for k in GUARD}}
    seen_lines = {}  # log path -> bytes read
    last_total = 0.0
    snapped = set()

    def logs():
        found = [os.path.join(out, "logs", "client.log")]
        for pattern in ("workers", "spares"):
            root = os.path.join(home, pattern)
            for dirpath, _, files in os.walk(root) if os.path.isdir(root) else []:
                found += [os.path.join(dirpath, f) for f in files if f == "log"]
        daemon = os.path.join(home, "daemon.log")
        return found + ([daemon] if os.path.exists(daemon) else [])

    def tail(total):
        for path in logs():
            try:
                with open(path, "rb") as f:
                    f.seek(seen_lines.get(path, 0))
                    data = f.read()
                    seen_lines[path] = f.tell()
            except OSError:
                continue
            for line in data.decode("utf-8", "replace").splitlines():
                if line.strip():
                    phases.write(json.dumps({"t": round(time.time() - t0, 1), "mb": round(total),
                                             "log": os.path.relpath(path, work), "line": line[:400]}) + "\n")

    def snapshot(label, pid):
        if DARWIN and shutil.which("footprint"):
            text = sh(["footprint", "-p", str(pid)]).stdout
            with open(os.path.join(out, "snap", f"{label}-{pid}.txt"), "w") as f:
                f.write(text)

    try:
        while True:
            rows = ps_table()
            # The daemon is started by the client and is among its descendants until the client
            # ends, so blaze's own tree is the daemon's, and the client's is the rest of its.
            daemons = {r[0] for r in rows if r[3].startswith(prefix + "ra-jit daemon")}
            ours = daemons | descendants(rows, daemons)
            client_tree = descendants(rows, [client.pid]) - ours
            if a.count_client:
                ours, client_tree = set(client_tree), set()
            held = footprints_mb(ours | client_tree, rows)
            total = sum(v for p, v in held.items() if p in ours)
            theirs = sum(v for p, v in held.items() if p in client_tree)
            used, free = swap_mb()
            growth = used - swap_start
            last_total = total
            result["peak_mb"] = max(result["peak_mb"], round(total))
            result["peak_client_mb"] = max(result["peak_client_mb"], round(theirs))
            result["peak_swap_growth_mb"] = max(result.get("peak_swap_growth_mb", 0), round(growth))
            cmds = {r[0]: r[3] for r in rows}
            big = {str(p): [round(v), cmds.get(p, "")[len(prefix):].split(" ")[0:2]] for p, v in held.items()
                   if v > 100 and p in ours}
            names = lambda cmd: [os.path.basename(w) for w in cmd.split(" ")[:3] if w and not w.startswith("-")][:2]
            cbig = {str(p): [round(v), names(cmds.get(p, ""))] for p, v in held.items() if v > 300 and p in client_tree}
            series.write(json.dumps({"t": round(time.time() - t0, 1), "mb": round(total), "client_mb": round(theirs),
                                     "pids": len(ours), "procs": big, "client_procs": cbig, "swap_used_mb": round(used),
                                     "swap_free_mb": round(free)}) + "\n")
            tail(total)
            for mark in MARKS:
                if total >= mark * GB and mark not in snapped:
                    snapped.add(mark)
                    result["marks"][str(mark)] = round(time.time() - t0, 1)
                    top = max((p for p in held if p in ours), key=held.get, default=None)
                    if top:
                        snapshot(f"{mark}gb", top)
            reason = None
            if total > a.watchdog_mb:
                reason = f"footprint of blaze's processes {total / GB:.1f} GB passed {a.watchdog_mb / GB:.1f} GB"
            elif total + theirs > a.total_mb:
                reason = f"footprint of blaze's processes and the client's {(total + theirs) / GB:.1f} GB passed {a.total_mb / GB:.1f} GB"
            elif theirs > a.client_mb:
                reason = f"footprint of the client's cargo and rustc {theirs / GB:.1f} GB passed {a.client_mb / GB:.1f} GB"
            elif free < a.swap_free_mb and growth >= GB:
                reason = f"swap free {free / GB:.1f} GB under {a.swap_free_mb / GB:.1f} GB, grown {growth / GB:.1f} GB"
            elif growth >= 8 * GB:
                reason = f"swap grown {growth / GB:.1f} GB"
            elif time.time() - t0 > a.timeout:
                reason = f"timeout of {a.timeout} s"
            if reason:
                top = max((p for p in held if p in ours), key=held.get, default=None)
                if top:
                    snapshot("end", top)
                result["tripped"] = {"reason": reason, "footprint_mb": round(total), "client_mb": round(theirs),
                                     "at_s": round(time.time() - t0, 1), "swap_used_mb": round(used),
                                     "pids": len(ours)}
                print("WATCHDOG:", reason, flush=True)
                kill(ours)
                kill(client_tree, signal.SIGTERM)
                time.sleep(2)
                kill(client_tree & {r[0] for r in ps_table()})
                break
            if client.poll() is not None:
                top = max((p for p in held if p in ours), key=held.get, default=None)
                if top:
                    snapshot("end", top)
                break
            time.sleep(a.every)
    finally:
        if client.poll() is None:
            kill(descendants(ps_table(), [client.pid]))
        result["exit"] = client.poll()
        result["outcome"] = "watchdog-killed" if result["tripped"] else ("ok" if client.returncode == 0 else "failed")
        result["wall_s"] = round(time.time() - t0, 1)
        result["last_mb"] = round(last_total)
        tail(last_total)
        for path in logs():
            if "/logs/" not in path:
                dest = os.path.join(out, "logs", os.path.relpath(path, home).replace(os.sep, "_"))
                try:
                    shutil.copyfile(path, dest)
                except OSError:
                    pass
        json.dump(result, open(os.path.join(out, "result.json"), "w"), indent=1)
        print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
