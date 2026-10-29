#!/usr/bin/env python3
"""frontimage.py: what a worker holds, and how long it takes to be ready, when the front end of the
dependency crates comes from nothing, from the store or from a mapped image (D3,
docs/research/front-end-image.md).

    frontimage.py --td DIR --package NAME --blaze-bin DIR --work DIR --cargo-home-from DIR
                  [--runs 3] [--arms off,store,image] [--prof-bin DIR] [--toolchain 1.99.0]

Every run is `harness/loadmem.py` (the memory guard of the distributed program, a watchdog, a
private home, store and CARGO_HOME) stopped a few seconds after the worker says `memory after
resolve` and `front-end ready`: the worker has loaded the workspace and resolved the names
of every crate it will build. It is held there, before codegen, while sampled. The steps:

  build   `blaze build` of the package to the end, so that cargo's target directory is whole and the
          build scripts' output is what every later run reads (the frozen keys count it)
  seed    a worker on an empty store with `RA_JIT_FRONT_IMAGE` naming a file that is not there: it
          resolves everything, freezes the dependency crates into the store, writes the image,
          and reports what the workspace holds by kind of crate (`RA_JIT_FRONT_STATS=census`)
  arms    `--runs` rounds of the arms, in turn within a round, each on a copy of the seed's store:
            off    `RA_JIT_FROZEN=off`: every crate is resolved
            store  the frozen parts are read out of the store into the heap (what a worker does today)
            image  `RA_JIT_FRONT_IMAGE`: the frozen parts are ranges of a mapped file
  prof    with `--prof-bin` (a build with the `heap-profile` feature), one more run of `off` and of
          `store` with jemalloc's profile: the files are for harness/heapfold.py and
          loadmem_holders.py

Writes WORK/summary.json and prints a table: per arm the seconds the worker spent loading and
resolving (its own timers), what reading the parts took, and what it held when it was ready (the
physical footprint on macOS, which leaves out a mapped file's clean pages, and the resident set,
which counts them).
"""
import argparse, hashlib, importlib.util, json, os, re, shutil, statistics, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
LOADMEM = os.path.join(os.path.dirname(HERE), "loadmem.py")
READY = r"front-end ready"
UNITS = {"ns": 1e-9, "us": 1e-6, "µs": 1e-6, "ms": 1e-3, "s": 1.0}
RUNTIME_ROOT = "/tmp"  # Short, portable, and deliberately independent of TMPDIR.


def archive_runtime(home, run):
    """Collect again after stop, using exactly the collector's archive rules."""
    spec = importlib.util.spec_from_file_location("frontimage_loadmem", LOADMEM)
    collector = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(collector)
    collector.archive_logs(collector.runtime_logs(home), home, os.path.join(run, "logs"))

def arm_env(arm, image):
    """Timed arms never run the all-workspace attribution census."""
    return {"off": ["RA_JIT_FROZEN=off"], "store": [],
            "image": [f"RA_JIT_FRONT_IMAGE={image}", "RA_JIT_FRONT_IMAGE_WATCH=500"]}[arm]


def comparison_kind(arms):
    return "comparative" if set(arms) == {"off", "store", "image"} else "diagnostic"


def seconds(number, unit):
    return float(number) * UNITS[unit]


def sh(args, **kw):
    kw.setdefault("text", True)
    kw.setdefault("stdout", subprocess.PIPE)
    kw.setdefault("stderr", subprocess.STDOUT)
    return subprocess.run(args, **kw)


def clone(src, dst):
    """A copy that shares blocks where the file system can (APFS), else a plain copy."""
    if os.path.exists(dst):
        shutil.rmtree(dst)
    if sh(["cp", "-cR", src, dst]).returncode != 0:
        shutil.rmtree(dst, ignore_errors=True)
        shutil.copytree(src, dst)


def loadmem(a, name, env=(), stop=True, prof=False, blaze_bin=None, store_from=None):
    """One guarded run below WORK/name; the directory of what loadmem.py recorded."""
    work = os.path.join(a.work, name)
    # Old worker logs can contain the readiness marker before this run even starts.
    os.makedirs(work, exist_ok=False)
    if store_from:
        clone(store_from, os.path.join(work, "store.noindex"))
    # macOS's staging socket appends /daemon.sock.new/s. Artifact paths can be
    # arbitrarily long; runtime homes must not inherit that nesting or TMPDIR.
    runtime = tempfile.mkdtemp(prefix=".fi-", dir=getattr(a, "runtime_root", RUNTIME_ROOT))
    with open(os.path.join(work, "runtime-home.txt"), "w") as file:
        file.write(runtime + "\n")
    cmd = [sys.executable, "-I", LOADMEM, "--td", a.td, "--blaze-bin", blaze_bin or a.blaze_bin, "--work", work,
           "--home", runtime,
           "--name", "r", "--cargo-home-from", a.cargo_home_from, "--target-name", "d3m.noindex",
           "--timeout", str(a.timeout),
           "--memory-every", "0.5", "--every", "0.5", "--env", f"RUSTUP_TOOLCHAIN={a.toolchain}",
           "--env", "RA_JIT_FRONT_STATS=1", "--env", "RA_JIT_FROZEN=on",
           "--env", "RA_JIT_FRONT_IMAGE=off", "--env", "RA_JIT_FRONT_IMAGE_WATCH=0",
           "--env", "RA_JIT_FRONT_HOLD_MS=0"]
    for kv in env:
        cmd += ["--env", kv]
    if stop:
        cmd += ["--env", "RA_JIT_FRONT_HOLD_MS=30000"]
        cmd += ["--stop-after", READY, "--stop-delay", "8" if prof else "4"]
    if prof:
        cmd += ["--prof", "--step-mb", "2048"]
    cmd += ["--", "blaze", "build", "--locked", "-p", a.package]
    print("+", " ".join(cmd[3:]), flush=True)
    out = None
    error = None
    interrupted = False
    cleanup = {"collector_exit": None, "stop_exit": None, "runtime_home": runtime}
    try:
        out = sh(cmd)
        cleanup["collector_exit"] = out.returncode
        if out.returncode:
            error = f"loadmem collector exited {out.returncode}: {out.stdout}"
    except KeyboardInterrupt:
        interrupted = True
        error = "loadmem collector interrupted"
    except Exception as why:
        error = f"loadmem collector failed: {why}"
    finally:
        # Stop only this run's private daemon; other lanes may use the same binary.
        try:
            stopped = sh([os.path.join(os.path.abspath(blaze_bin or a.blaze_bin), "ra-jit"), "stop"],
                         env=dict(os.environ, RA_JIT_HOME=runtime), timeout=20)
            cleanup["stop_exit"] = stopped.returncode
            if stopped.returncode:
                raise RuntimeError(f"daemon stop exited {stopped.returncode}: {stopped.stdout}")
        except (Exception, KeyboardInterrupt) as why:
            interrupted |= isinstance(why, KeyboardInterrupt)
            error = f"{error + '; ' if error else ''}stop failed: {why}"
        # Even a failed stop may have written useful diagnostics. Collection does
        # not imply that the process is stopped or that its home may be removed.
        try:
            archive_runtime(runtime, os.path.join(work, "r"))
        except (Exception, KeyboardInterrupt) as why:
            interrupted |= isinstance(why, KeyboardInterrupt)
            error = f"{error + '; ' if error else ''}{why}"
        cleanup["error"] = error
        result_path = os.path.join(work, "r", "result.json")
        if os.path.exists(result_path):
            try:
                with open(result_path) as collected:
                    result = json.load(collected)
                cleanup["source_client_pid"] = result.get("client_pid")
                cleanup["source_daemon_pids"] = result.get("daemon_pids", [])
                cleanup["source_exit"] = result.get("exit")
            except (OSError, ValueError) as why:
                error = f"{error + '; ' if error else ''}unreadable collector provenance: {why}"
                cleanup["error"] = error
        with open(os.path.join(work, "cleanup.json"), "w") as kept:
            json.dump(cleanup, kept, indent=1)
        if out is not None:
            with open(os.path.join(work, "loadmem.out"), "w") as kept:
                kept.write(out.stdout)
        if error:
            message = f"{error}; runtime home retained: {runtime}"
            with open(os.path.join(work, "runtime-error.txt"), "w") as kept:
                kept.write(message + "\n")
            if interrupted:
                raise KeyboardInterrupt(message)
            raise RuntimeError(message)
        shutil.rmtree(runtime)
    return os.path.join(work, "r")


def worker_log(run):
    """The text of the worker's log among the logs loadmem.py copied: the one that got ready."""
    best = ""
    logs = os.path.join(run, "logs")
    for name in sorted(os.listdir(logs)) if os.path.isdir(logs) else []:
        with open(os.path.join(logs, name), errors="replace") as file:
            text = file.read()
        if re.search(READY, text) and len(text) > len(best):
            best = text
    return best


def no_rustc_violation(run):
    """Existing fallback bugs invalidate measurements, even when the client exits zero."""
    logs = os.path.join(run, "logs")
    for name in os.listdir(logs) if os.path.isdir(logs) else []:
        with open(os.path.join(logs, name), errors="replace") as file:
            text = file.read()
        if ("this build is cargo's" in text or "rustc compiles the build script" in text
                or "rustc compiles the proc macro" in text):
            return True
    return False


def parse(run):
    """What one run says: the worker's timers and counts, and what loadmem.py saw of its footprint."""
    with open(os.path.join(run, "result.json")) as file:
        result = json.load(file)
    log = worker_log(run)
    row = {"outcome": result.get("outcome"), "tripped": result.get("tripped"), "ready_logged": bool(log),
           "swap_growth_mb": result.get("peak_swap_growth_mb")}
    row["no_rustc_violation"] = no_rustc_violation(run)
    row["workload"] = {"td": result.get("td"), "command": result.get("command")}
    stopped = result.get("stopped_after") or {}
    row.update(ready_footprint_mb=stopped.get("footprint_mb"), ready_rss_mb=stopped.get("rss_mb"))
    row["ready_seen_s"] = stopped.get("ready_seen_s")

    def find(pattern, *, last=False):
        found = re.findall(pattern, log, re.M)
        return (found[-1] if last else found[0]) if found else None

    m = find(r"^load workspace: ([\d.]+)(\S+)$")
    row["load_s"] = seconds(*m) if m else None
    m = find(r"^resolve: (\d+) crates in ([\d.]+)(\S+)$")
    row["crates"], row["resolve_s"] = (int(m[0]), seconds(m[1], m[2])) if m else (None, None)
    m = find(r"^front-end serve: ([\d.]+)(\S+) in all, ([\d.]+)(\S+) of it reading parts, (\d+) of (\d+) parts from the image")
    if m:
        row.update(serve_s=seconds(m[0], m[1]), fetch_s=seconds(m[2], m[3]), from_image=int(m[4]), parts=int(m[5]))
    m = find(r"^front-end image usage: (\d+) attempted, (\d+) accepted parts")
    if m:
        row.update(image_attempted=int(m[0]), image_accepted=int(m[1]))
    m = find(r"^front-end parts: def maps ([\d.]+) MB, impls ([\d.]+) MB, lang items ([\d.]+) MB")
    if m:
        row["parts_mb"] = dict(zip(("def_maps", "impls", "lang_items"), map(float, m)))
    m = find(r"^front-end image: .* (\d+) of (\d+) pages in memory, process holds (\d*) MB")
    if m:
        row["image_pages"] = [int(m[0]), int(m[1])]
    m = find(r"^front-end image: wrote (\d+) parts,")
    if m:
        row["image_written_parts"] = int(m)
    m = find(r"^frozen, of (\d+) crates: def maps of (\d+).* from the store, (\d+) written")
    if m:
        row["frozen"] = dict(crates=int(m[0]), def_maps=int(m[1]), written=int(m[2]))
    m = find(r"^memory after resolve: (\d+) MB resident(?:, (\d+) MB live)?")
    if m:
        row["after_resolve_resident_mb"], row["after_resolve_live_mb"] = int(m[0]), int(m[1] or 0)
    ticks = re.findall(r"^memory tick \d+ at [\d.]+s: (\d+) MB footprint, (\d+) MB live", log, re.M)
    if ticks:
        row["peak_footprint_mb"] = max(int(t[0]) for t in ticks)
    row["census"] = {name: {k: float(v) for k, v in zip(("crates", "modules", "files", "text_mb"), vals)}
                     for name, *vals in re.findall(
                         r"^front-end census: ([\w, ]+?): (\d+) crates, (\d+) modules, (\d+) files, ([\d.]+) MB", log, re.M)}
    return row


def invalid(row, arm):
    """A ready worker is not enough: each arm must exercise the state it claims to measure."""
    if row.get("no_rustc_violation"):
        return "the run used an existing rustc/cargo fallback"
    if row.get("tripped") or row.get("outcome") != "stopped":
        return "the guarded run did not reach ready"
    if not row.get("ready_logged") or row.get("ready_seen_s") is None:
        return "no worker readiness evidence"
    if any(row.get(key) is None for key in ("load_s", "resolve_s", "ready_footprint_mb", "ready_rss_mb")):
        return "missing readiness timings or memory samples"
    if arm == "seed" and not row.get("image_written_parts", 0):
        return "the seed wrote no frozen parts"
    if arm == "image" and (not row.get("image_accepted", 0)
                           or row["image_accepted"] > row.get("parts", 0)):
        return "the image arm read no mapped parts"
    if arm == "store" and (row.get("image_accepted", 0) or row.get("from_image", 0) or not row.get("parts", 0)):
        return "the store arm did not read frozen parts exclusively from the store"
    if arm == "off" and (row.get("parts", 0) or row.get("frozen", {}).get("def_maps", 0)):
        return "the off arm used frozen parts"
    return None


def spread(values):
    values = [v for v in values if v is not None]
    if not values:
        return "-"
    mean = statistics.mean(values)
    return f"{mean:.2f} ({min(values):.2f}-{max(values):.2f})" if mean < 100 else f"{mean:.0f} ({min(values):.0f}-{max(values):.0f})"


def finish_summary(summary):
    bad = [(arm, i, why) for arm, rows in summary["arms"].items()
           for i, row in enumerate(rows, 1) if (why := invalid(row, arm))]
    bad += [(f"prof-{arm}", 1, why) for arm, row in summary.get("prof", {}).items()
            if (why := invalid(row, arm))]
    if "requested_arms" in summary:
        for arm in summary["requested_arms"]:
            if len(summary["arms"].get(arm, [])) != summary["runs"]:
                bad.append((arm, 0, "missing requested rounds"))
    identities = {(json.dumps(row.get("workload"), sort_keys=True), row.get("crates"))
                  for rows in summary["arms"].values() for row in rows}
    if len(identities) != 1 or any(row.get("crates") is None or not row.get("workload")
                                 for rows in summary["arms"].values() for row in rows):
        bad.append(("coverage", 0, "requested arms differ in workload or resolved-crate count"))
    summary["valid"] = not bad
    summary["comparable"] = not bad and summary["comparison"] == "comparative"
    summary["mapping_scope"] = "partial accepted mapping; not full-image semantic state"
    summary["coverage_limiter"] = "resolved count and requested workload only; exact resolved-root IDs unavailable"
    summary["invalid_measurements"] = bad
    return bad


def coverage(row):
    """Accepted mapped / attempted mapped / total installed; not full-image coverage."""
    return f"{row.get('image_accepted', 0)}/{row.get('image_attempted', 0)}/{row.get('parts', 0)}"


def write_summary(work, summary):
    """Replace the progress record atomically; an interrupt leaves the preceding record."""
    with tempfile.NamedTemporaryFile(mode="w", dir=work, prefix=".summary-", delete=False) as file:
        temporary = file.name
        try:
            json.dump(summary, file, indent=1)
            file.flush()
            os.replace(temporary, os.path.join(work, "summary.json"))
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)


def source_digest(root):
    """Rust sources/manifests, excluding generated targets and VCS state."""
    digest = hashlib.sha256()
    for base, dirs, files in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d not in ("target", ".git"))
        for name in sorted(files):
            if name.endswith(".rs") or name in ("Cargo.toml", "Cargo.lock"):
                path = os.path.join(base, name)
                digest.update(os.path.relpath(path, root).encode() + b"\0")
                with open(path, "rb") as file:
                    digest.update(file.read())
    return digest.hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--td", required=True, help="the checkout (a fixture of harness/dist/fixtures.toml)")
    ap.add_argument("--package", required=True, help="the package `blaze build -p` builds: ripgrep, nu")
    ap.add_argument("--blaze-bin", required=True)
    ap.add_argument("--prof-bin", help="a build with the heap-profile feature, for one run of off and of store")
    ap.add_argument("--work", required=True)
    ap.add_argument("--cargo-home-from", required=True)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--timeout", type=int, default=900, help="seconds per guarded run")
    ap.add_argument("--arms", default="off,store,image")
    ap.add_argument("--toolchain", default="1.99.0")
    ap.add_argument("--skip-build", action="store_true", help="cargo's target directory is whole already")
    ap.add_argument("--runtime-root", default=RUNTIME_ROOT,
                    help="short directory for private temporary daemon homes (not artifact storage)")
    a = ap.parse_args()
    arms = a.arms.split(",")
    if a.runs < 1 or not arms or len(arms) != len(set(arms)) or set(arms) - {"off", "store", "image"}:
        ap.error("--runs must be positive and --arms must name distinct off, store or image arms")
    a.td, a.work = os.path.abspath(a.td), os.path.abspath(a.work)
    # Reserve the whole invocation before writing progress or deleting its image.
    # Per-arm checks are too late to preserve an earlier run's evidence and cannot
    # stop two invocations from racing on the shared seed and summary.
    os.makedirs(a.work, exist_ok=False)
    image = os.path.join(a.work, "front.image")
    summary = {"td": a.td, "package": a.package, "runs": a.runs, "arms": {},
               "requested_arms": arms,
               "comparison": comparison_kind(arms), "valid": False, "comparable": False,
               "status": "incomplete", "invalid_measurements": ["incomplete"]}
    write_summary(a.work, summary)
    try:
        summary["source_digest"] = source_digest(a.td)
        result = run_measurements(a, arms, image, summary)
        if source_digest(a.td) != summary["source_digest"]:
            summary["valid"] = summary["comparable"] = False
            summary["invalid_measurements"].append(("source", 0, "source changed during invocation"))
            result = 1
        summary["status"] = "complete" if result == 0 else "failed"
        write_summary(a.work, summary)
        return result
    except (Exception, KeyboardInterrupt) as error:
        summary.update(status="interrupted" if isinstance(error, KeyboardInterrupt) else "failed",
                       valid=False, comparable=False, error=f"{type(error).__name__}: {error}")
        write_summary(a.work, summary)
        raise


def run_measurements(a, arms, image, summary):
    """Mutate the published progress object so failures preserve finished arms too."""

    if not a.skip_build:
        run = loadmem(a, "build", stop=False)
        summary["build"] = parse(run) if os.path.exists(os.path.join(run, "result.json")) else None
        if (not summary["build"] or summary["build"]["outcome"] != "ok"
                or summary["build"]["tripped"] or summary["build"]["no_rustc_violation"]):
            print("BUILD DID NOT COMPLETE WITH BLAZE; no measurement arms:", summary["build"], flush=True)
            write_summary(a.work, summary)
            return 1
    if os.path.exists(image):
        os.remove(image)
    seed = loadmem(a, "seed", env=[f"RA_JIT_FRONT_IMAGE={image}", "RA_JIT_FRONT_STATS=census"])
    summary["seed"] = parse(seed)
    seed_error = invalid(summary["seed"], "seed")
    if seed_error or not os.path.exists(image):
        print("SEED FAILED:", seed_error or "no image was written", json.dumps(summary["seed"]), flush=True)
        write_summary(a.work, summary)
        return 1
    summary["image_bytes"] = os.path.getsize(image)
    store = os.path.join(a.work, "seed", "store.noindex")
    env = {arm: arm_env(arm, image) for arm in ("off", "store", "image")}
    rows = {arm: [] for arm in arms}
    summary["arms"] = rows
    for n in range(1, a.runs + 1):
        for arm in arms:
            run = loadmem(a, f"{arm}-{n}", env=env[arm], store_from=store)
            row = parse(run)
            rows[arm].append(row)
            write_summary(a.work, summary)
            print(arm, n, json.dumps(row), flush=True)
    summary["arms"] = rows
    if a.prof_bin:
        summary["prof"] = {}
        for arm in ("off", "store"):
            run = loadmem(a, f"prof-{arm}", env=env[arm], store_from=store, prof=True, blaze_bin=os.path.abspath(a.prof_bin))
            summary["prof"][arm] = parse(run)
            summary["prof"][arm]["dir"] = run
    bad = finish_summary(summary)
    write_summary(a.work, summary)

    print(f"\n{a.package}: {summary['comparison']}, valid={summary['valid']}; mean (min-max) of {a.runs} runs; seconds are the worker's own timers\n")
    print("Coverage is mapped accepted/attempted/total installed per run; partial mapped use is not full image service.")
    print(summary["coverage_limiter"])
    print("| arm | resolved crates (mean, range) | mapped accepted/attempted/total by run | ready seen s | load s | resolve s | serve s | reading parts s | ready footprint MB | ready rss MB | live MB |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for arm in arms:
        r = rows[arm]
        print(f"| {arm} | {spread([x['crates'] for x in r])} | {', '.join(coverage(x) for x in r)} | "
              f"{spread([x['ready_seen_s'] for x in r])} | "
              f"{spread([x['load_s'] for x in r])} | {spread([x['resolve_s'] for x in r])} | "
              f"{spread([x.get('serve_s') for x in r])} | {spread([x.get('fetch_s') for x in r])} | "
              f"{spread([x['ready_footprint_mb'] for x in r])} | {spread([x['ready_rss_mb'] for x in r])} | "
              f"{spread([x.get('after_resolve_live_mb') for x in r])} |")
    if bad:
        print("INVALID MEASUREMENTS:", bad, flush=True)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
