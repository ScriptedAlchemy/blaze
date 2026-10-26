#!/usr/bin/env python3
"""memgrowth.py PROJECT [--work DIR] [--pairs N] [--reclaim-every K] [--arms 0,1] [--runs N]
[--edits KIND,...] [--env NAME=VALUE]... [--local] [--recycle]: what a worker holds as a
program is edited and put back, by holder, and what an idle reclaim gives back.

Two measurements of [docs/research/memory.md](../docs/research/memory.md) section 5, M1 and M4.

M1, edit/undo growth with a per-holder breakdown. The daemon is started with `RA_JIT_MEMORY` and
one worker, and waited for until it has built its own base (`edit-to-binary:` of it, as
`memsplit.py` waits). Then `--pairs` edit/undo pairs are made, each of a seeded kind (body,
signature, new function, new file, generic, a move of an item), each returning the text to the
bytes it started with, and after each the daemon is waited for until it has built it. Every
`--reclaim-every` pairs it records what the worker holds: the tree's `VmRSS`, `RssAnon`,
`RssFile` and `VmHWM` on Linux (its physical footprint on macOS); jemalloc's `stats.allocated`,
`resident`, `mapped` and `retained`; the ingredient table; and what the daemon reports of its
own: the lengths of the `intern` tables per type, the checkouts, the files hashed, the symbols
of rustc's executables it has read, and the maps a patch keeps. Every edit is built by the
daemon, and the program is run after the edit and after its undoing: a build that fails, is
left to cargo, or a program that says something else than it said first, ends the run.

At the end, the arm's figure is read, then one more edit is made and its undoing: that is the
edit's time with the reclaim's page faults in it. With `--recycle` a worker recycle follows, which
is the daemon stopped and started again on the same store and target directory; its figure is
the floor.

M4, idle reclaim A/B. The arms are `RA_JIT_RECLAIM`'s values: `0` (nothing) and `1` (the type
GC, the solver caches on every thread, and the purge), or any of `gc`, `cache` and `purge` for
one step alone, separated by commas. Each arm is its own daemon from the same source and store,
makes the same `--pairs` pairs, and records what the worker holds after its last pair: the arms
are the same state at the start and differ in what the reclaim gave back, so the difference
between two arms is the reclaim's. `--arms 0,gc,purge,1` reads what each step alone gives back,
in the order M1's section 5 asks for. `--runs` (at least 2) interleaves the arms, and the
medians are printed with the samples.

Writes the steps as JSON on stdout's last line. Linux and macOS. The projects are `bench.py`'s,
fetched as it fetches them, or a directory of this repository with `--local` (for example
`demo/mid`).

`RA_JIT_MEMORY` slows edits to about 1.5 s (lean-frontend.md), so the edit times here are not
timings of the daemon: they are what the reclaim costs, measured against the same edit in the
arm without it.
"""
import glob, json, os, platform, re, shutil, statistics, subprocess, sys, time

import bench, cargo_ra_jit, memory
from cargo_ra_jit import Side, fetch, wait_warm

DARWIN = platform.system() == "Darwin"

# The kinds of edit, in the order they are seeded and made.
EDITS = ("body", "signature", "newfn", "newfile", "generic", "move")


def local_spec(project, work):
    """A `bench.PROJECTS` entry for a directory of this repository, `demo/mid` or `demo/hot`:
    they are small and need no network. The copy is what is edited. A project that is one
    crate of a workspace (`demo/hot`, which `demo/app` depends on) is copied with the workspace
    above it, so that a build of it finds the others; one that is a workspace of its own
    (`demo/mid`, which says `[workspace]`) is copied alone."""
    source = os.path.join(cargo_ra_jit.ROOT, project)
    if not os.path.isdir(source):
        raise SystemExit(f"{project} is not a directory of this repository")
    manifest = open(os.path.join(source, "Cargo.toml")).read()
    above = os.path.dirname(source)
    if "[workspace]" not in manifest and os.path.exists(os.path.join(above, "Cargo.toml")):
        source = above
    ws = os.path.realpath(os.path.join(work, "source"))
    shutil.copytree(source, ws, symlinks=True,
                    ignore=shutil.ignore_patterns("target", "target-ref", ".git"))
    # A project inside a workspace is built and run as itself.
    member = os.path.relpath(os.path.join(cargo_ra_jit.ROOT, project), source)
    if member != ".":
        manifest = open(os.path.join(ws, member, "Cargo.toml")).read()
        package = re.search(r"^name = \"([^\"]+)\"", manifest, re.M)[1]
        return dict(ws=ws, env={}, run=[], build=["-p", package], bin=package)
    return dict(ws=ws, env={}, run=[], build=[])


def edits_for(ws):
    """The seeded edits, made of what the project has. The result is {kind: changes}, where a
    change is (pattern, old, new), and `None` as the old text appends the new one. A kind the
    project has nothing for is left out."""
    files = []
    for root, dirs, names in os.walk(ws):
        dirs[:] = [d for d in dirs if d not in ("target", "target-ref", ".git", "node_modules")]
        files += [os.path.join(root, n) for n in names if n.endswith(".rs")]
    files.sort()
    text = {f: open(f, errors="replace").read() for f in files}
    found = {}

    def first(pattern):
        for f in files:
            m = re.search(pattern, text[f], re.M)
            if m:
                return f, m
        return None, None

    def opening(pattern):
        f, m = first(pattern)
        return (f, m.group(0).rstrip("\n")) if f else (None, None)

    body = opening(r"^(pub(\([^)]*\))?\s+)?fn \w+\([^)]*\)[^\n{]*\{\n")
    if body[0]:
        f, opening_text = body
        found["body"] = [(os.path.relpath(f, ws), opening_text,
                          opening_text + "\n    let _memgrowth = 1u32;")]
    method = opening(r"^(pub(\([^)]*\))?\s+)?fn \w+\(&self\)[^\n{]*\{\n")
    if method[0]:
        f, opening_text = method
        name = re.search(r"fn (\w+)", opening_text)[1]
        found["signature"] = [
            (os.path.relpath(f, ws), opening_text,
             opening_text.replace("(&self)", "(&self, _memgrowth: ())")),
            ("**/*.rs", f".{name}()", f".{name}(())", "calls"),
        ]
    else:
        # A free function that something calls: its declaration takes another parameter, and
        # every call passes it. `()` changes no output.
        for f in files:
            for m in re.finditer(r"^(pub(\([^)]*\))?\s+)?fn (\w+)\(([^)]*)\)([^\n{]*)\{\n", text[f], re.M):
                name, params, rest = m.group(3), m.group(4), m.group(5)
                if not params.strip() or "->" not in rest:
                    continue
                if not any(re.search(rf"(?<![\w.]){name}\(", text[g]) for g in files):
                    continue
                old = m.group(0).rstrip("\n")
                new = f"{m.group(1) or ''}fn {name}(_memgrowth: (), {params}){rest}{{"
                # Every call of it, except the declaration's own text: the argument goes in
                # after the call's opening parenthesis, which is the first place, and comes
                # out again by the reverse.
                found["signature"] = [
                    (os.path.relpath(f, ws), old, new),
                    ("**/*.rs", f"{name}(", f"{name}((), ", "calls"),
                ]
                break
            if "signature" in found:
                break
    newfn = opening(r"^(pub(\([^)]*\))?\s+)?fn \w+\([^)]*\)[^\n{]*\{\n")
    if newfn[0]:
        f, opening_text = newfn
        found["newfn"] = [
            (os.path.relpath(f, ws), opening_text,
             opening_text + "\n    let _memgrowth = _memgrowth_added(1u32);"),
            (os.path.relpath(f, ws), None,
             "\n#[inline(never)]\nfn _memgrowth_added(x: u32) -> u32 {\n    x.wrapping_mul(31)\n}\n"),
        ]
    generic = opening(r"^(pub(\([^)]*\))?\s+)?fn \w+<[^>]*>\([^)]*\)[^\n{]*\{\n")
    if generic[0]:
        f, opening_text = generic
        found["generic"] = [(os.path.relpath(f, ws), opening_text,
                             opening_text + "\n    let _memgrowth = 1u32;")]
    f, m = first(r"^(pub(\([^)]*\))?\s+)?struct \w+[^\n]*\{\n(?:[^\n]*\n)*?\}\n")
    if f:
        # The item is taken out of where it is written, with the attributes and the comments
        # that are its own, and put at the end of the file: a move, so that the text of the
        # file before and after it, and nothing else, changes.
        whole = text[f]
        start = m.start()
        # The attributes and comments written just above it are its own.
        while start > 0 and whole[:start].endswith("\n"):
            above = whole[:start - 1].rsplit("\n", 1)[-1]
            if not re.match(r"\s*(#\[|///|//)", above):
                break
            start -= len(above) + 1
        item = whole[start:m.end()]
        rest = whole[m.end():]
        if rest.startswith("\n"):
            rest = rest[1:]
        found["move"] = [(os.path.relpath(f, ws), whole, whole[:start] + rest + "\n" + item, "whole")]
    roots = [f for f in files if os.path.basename(f) in ("main.rs", "lib.rs")]
    mods = [(f, re.findall(r"^mod (\w+);$", text[f], re.M)) for f in files]
    mods = [(f, names) for f, names in mods if names]
    if mods or roots:
        f = max(mods, key=lambda it: len(it[1]))[0] if mods else roots[0]
        name = "_memgrowth_module"
        path = os.path.join(os.path.dirname(f), name + ".rs")
        found["newfile"] = [(os.path.relpath(path, ws), None, "pub fn added() -> u32 { 1 }\n"),
                            (os.path.relpath(f, ws), None, f"\nmod {name};\n")]
    return found


def write_edit(ws, changes, back):
    """`bench.write_edit`: every change is made, or taken back, and a change whose old text is
    `None` appends its new text to a file it creates when it is not there. A change may name
    only the first place its old text is found, which an item that moves needs. Taking the
    changes back is done in the reverse order they were made. The result is how many places
    changed."""
    places = 0
    for change in (reversed(changes) if back else changes):
        pattern, old, new = change[:3]
        how = change[3] if len(change) > 3 else ""
        if how == "whole":
            # The file is the change: what it is written as, and what it started as.
            path = os.path.join(ws, pattern)
            text = open(path).read()
            was, becomes = (new, old) if back else (old, new)
            assert text == was, f"{path} is not what this change was made to"
            open(path, "w").write(becomes)
            places += 1
            continue
        if old is None:
            path = os.path.join(ws, pattern)
            text = open(path).read() if os.path.exists(path) else ""
            if back:
                assert text.endswith(new), f"{path} does not end with what was appended"
                text = text[:-len(new)]
            else:
                text += new
            open(path, "w").write(text)
            places += 1
            continue
        old, new = (new, old) if back else (old, new)
        for path in sorted(glob.glob(os.path.join(ws, pattern), recursive=True)):
            text = open(path).read()
            if old not in text:
                continue
            if how == "calls":
                # A call site: the argument goes after the opening parenthesis, and no
                # declaration is one, so a line that opens a function is left alone.
                lines, out = text.splitlines(keepends=True), []
                for line in lines:
                    if not re.match(r"\s*(pub(\([^)]*\))?\s+)?fn ", line):
                        line = line.replace(old, new)
                    out.append(line)
                open(path, "w").write("".join(out))
                places += 1
                continue
            places += text.count(old)
            open(path, "w").write(text.replace(old, new))
    return places


def logs(daemon):
    """What every worker of the daemon has logged so far."""
    return "".join(open(log, errors="replace").read() for _, log in daemon.workers() if os.path.exists(log))


def wait_base(daemon, timeout=3600, still=20):
    """Waits until the build of the daemon's own base is over, as `memsplit.py` does: the line
    it starts with, then the `edit-to-binary:` line once it is linked, or the line that says it
    was interrupted or given up. A daemon whose store already has the base builds nothing and
    says nothing: its memory still for `still` seconds is the end. The result is the seconds it
    took and what said so."""
    began, last_change, last = time.time(), time.time(), 0
    while time.time() - began < timeout:
        text = logs(daemon)
        at = text.rfind("compiling the program for edits to patch")
        if at >= 0 and re.search(r"^(edit-to-binary: |interrupted: |not compiling the program)", text[at:], re.M):
            return time.time() - began, text[at:].splitlines()[0]
        now = daemon.resident_mb()
        if abs(now - last) > 32:
            last, last_change = now, time.time()
        elif time.time() - last_change > still:
            return time.time() - began, "nothing to build" if at < 0 else "still for a while"
        time.sleep(0.25)
    return time.time() - began, "timed out"


def pages(daemon):
    """What the daemon's tree holds by kind of page, summed: `/proc`'s fields on Linux, and on
    macOS the summed physical footprint, which counts compressed pages too."""
    pid = daemon.pid()
    pids = memory.tree(pid) if pid else []
    if DARWIN:
        return dict(footprint=memory.resident_mb(pids), **dict.fromkeys(("VmRSS", "RssAnon", "RssFile", "VmHWM")))
    total = dict.fromkeys(("VmRSS", "RssAnon", "RssFile", "VmHWM"), 0)
    for pid in pids:
        try:
            status = open(f"/proc/{pid}/status").read()
        except OSError:
            continue
        for field in total:
            m = re.search(rf"^{field}:\s+(\d+) kB", status, re.M)
            if m:
                total[field] += int(m[1]) // 1024
    return dict(footprint=None, **total)


def newest_report(memory_dir):
    """The last report the daemon wrote: (its name, its rows by kind), or (None, None)."""
    found = glob.glob(os.path.join(memory_dir, "*.tsv"))
    if not found:
        return None, None
    path = max(found, key=os.path.getmtime)
    rows = {"query": {}, "struct": {}, "own": {}, "interned": {}, "alloc": {}}
    for line in open(path):
        cells = line.rstrip("\n").split("\t")
        if len(cells) < 6 or cells[0] == "kind":
            continue
        kind, count, fields, metadata, heap, name = cells
        if kind in ("own", "interned"):
            rows[kind][name] = int(count)
        elif kind == "alloc":
            rows["alloc"][name] = int(count)
        elif kind in rows:
            rows[kind][name] = dict(count=int(count), fields=int(fields), metadata=int(metadata),
                                    heap=int(heap) if heap else None)
    return os.path.basename(path), rows


def holders(rows, top=6):
    """The ingredient rows that hold the most, as (name, count, bytes) each."""
    if not rows:
        return []
    found = []
    for kind in ("query", "struct"):
        for name, it in rows[kind].items():
            found.append((f"{kind} {name}", it["count"],
                          it["fields"] + it["metadata"] + (it["heap"] or 0)))
    found.sort(key=lambda it: -it[2])
    return found[:top]


def figure(daemon, memory_dir, **extra):
    """What a worker holds at one step, and which report says it."""
    name, rows = newest_report(memory_dir)
    return dict(report=name, pages=pages(daemon), alloc=(rows or {}).get("alloc", {}),
                own=(rows or {}).get("own", {}), interned=(rows or {}).get("interned", {}),
                holders=holders(rows), **extra)


def start(spec, work, label, extra_env):
    """A daemon from nothing for the copy of the project, with a home and a store of its own,
    and the first build of the program."""
    ws = spec["ws"]
    env = dict(cargo_ra_jit.base_env(os.path.join(work, f"home-{label}")), **spec.get("env", {}), **extra_env)
    env["PATH"] = os.path.dirname(bench.ra_jit_of("cranelift")) + os.pathsep + env["PATH"]
    env.update(bench.BACKENDS["cranelift"], RA_JIT_STATS="1", RA_JIT_CACHE=os.path.join(work, f"store-{label}"))
    env.pop("RUSTC_WRAPPER", None)
    side = Side(ws, env, True)
    subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
    took, ran = side.cargo("build", *spec.get("build", []))
    if ran.returncode != 0:
        raise SystemExit(f"{label}: the first build failed:\n{ran.stderr[-3000:]}")
    warm, states = wait_warm(side, ws)
    return side, bench.Daemon(side), env, took, warm, states


def edit(side, daemon, spec, changes, back, timeout=300):
    """One edit, or its undoing: the text is written, `cargo ra-jit build` asks the daemon for
    it and waits for it, and what it took, how it was built and what the daemon logged are the
    result. The build is over when the command returns, so nothing waits after it."""
    write_edit(spec["ws"], changes, back)
    took, ran = side.cargo("build", *spec.get("build", []), timeout=timeout)
    built = cargo_ra_jit.how_built(ran.stderr)
    said = daemon.said()
    return took, ran, built, said


def program_of(spec, ws):
    """The program the build wrote: the package's own binary in `target/debug`, named by the
    package, which is the crate's name with `-` for `_`. It is found once and remembered, and
    nothing of `target/debug/build` or `deps` is taken for it."""
    if spec.get("program"):
        return spec["program"]
    package = spec.get("bin")
    if not package:
        manifest = open(os.path.join(ws, "Cargo.toml")).read()
        package = re.search(r'^name = "([^"]+)"', manifest, re.M)[1]
    for name in (package, package.replace("-", "_")):
        path = os.path.join(ws, "target", "debug", name)
        if os.path.exists(path) and os.access(path, os.X_OK):
            spec["program"] = path
            return path
    return None


def run_program(spec, ws):
    """Runs the program the build wrote, with the arguments the project says, and its status
    and output. `None` when no program of it is there, which is a failure of the build."""
    named = program_of(spec, ws)
    if named is None:
        return None, "the build wrote no program"
    try:
        ran = subprocess.run([named] + spec.get("run", []), capture_output=True, text=True, timeout=300)
    except (OSError, subprocess.TimeoutExpired) as why:
        return None, f"the program did not run: {why}"
    return ran.returncode, ran.stdout


def one_arm(spec, work, label, reclaim, pairs, every, kinds, build, output):
    """One arm: a daemon, M1's pairs, then M4's figures and one more edit timed."""
    reports = os.path.join(work, "reports", label)
    shutil.rmtree(reports, ignore_errors=True)
    os.makedirs(reports)
    env = dict(output["env"], RA_JIT_MEMORY=reports, RA_JIT_RECLAIM=reclaim)
    side, daemon, env, took, warm, states = start(spec, work, label, env)
    arm = dict(label=label, reclaim=reclaim, first_build_s=round(took, 1), warm_s=round(warm, 1),
               states=states, samples=[], run=output["run"])
    try:
        with memory.Sampler() as sampler:
            sampler.add(daemon.pid())
            waited, how = wait_base(daemon)
        arm["base_s"], arm["base_how"], arm["peak_mb"] = round(waited, 1), how, sampler.peak_mb
        print(f"{label}: base in {waited:.1f} s ({how}), peak {sampler.peak_mb} MB", flush=True)
        first = None
        for at in range(pairs):
            kind = kinds[at % len(kinds)]
            took, ran, built, said = edit(side, daemon, spec, output["edits"][kind], False)
            if ran.returncode != 0:
                raise SystemExit(f"{label} pair {at + 1} ({kind}): the build failed:\n{ran.stderr[-3000:]}")
            if built != "daemon":
                raise SystemExit(f"{label} pair {at + 1} ({kind}): built by {built}:\n{ran.stderr[-2000:]}")
            said_first = run_program(spec, spec["ws"])
            if first is None:
                first = said_first
            elif said_first != first:
                raise SystemExit(f"{label} pair {at + 1} ({kind}): the program says "
                                 f"{said_first[1][:400]!r}, not {first[1][:400]!r}")
            back_s, ran, built, said = edit(side, daemon, spec, output["edits"][kind], True)
            if ran.returncode != 0 or built != "daemon":
                raise SystemExit(f"{label} pair {at + 1} ({kind}, back): built by {built} "
                                 f"(exit {ran.returncode}):\n{ran.stderr[-2000:]}")
            said_back = run_program(spec, spec["ws"])
            if said_back != first:
                raise SystemExit(f"{label} pair {at + 1} ({kind}, back): the program says "
                                 f"{said_back[1][:400]!r}, not {first[1][:400]!r}")
            if (at + 1) % every == 0 or at + 1 == pairs:
                got = figure(daemon, reports, pair=at + 1, kind=kind,
                             edit_s=round(took, 3), undo_s=round(back_s, 3))
                arm["samples"].append(got)
                top = ", ".join(f"{n} {c}" for n, c, _ in got["holders"][:3])
                print(f"  pair {at + 1:3} {kind:9} edit {took:5.2f} s undo {back_s:5.2f} s  "
                      f"rss {got['pages'].get('VmRSS') or got['pages'].get('footprint')} MB  "
                      f"anon {got['pages'].get('RssAnon')}  hwm {got['pages'].get('VmHWM')}  "
                      f"allocated {got['alloc'].get('allocated', 0) >> 20} MB  "
                      f"ty {got['interned'].get('TyInterned')} args {got['interned'].get('GenericArgs')}  "
                      f"top {top}", flush=True)
        # M4: the state the arm ends in, and the next edit, which the reclaim's page faults
        # are in. The arms name the reclaim each run makes, so the arm that names one step
        # (`--arms 0,gc,purge,1`) says what that step alone gave back.
        arm["end"] = figure(daemon, reports, pair=pairs, kind="end")
        took, ran, built, said = edit(side, daemon, spec, output["edits"][kinds[0]], False)
        if ran.returncode != 0 or built != "daemon":
            raise SystemExit(f"{label}: the edit after the last pair was built by {built}")
        said_after = run_program(spec, spec["ws"])
        if said_after != first:
            raise SystemExit(f"{label}: the program after the reclaim says "
                             f"{said_after[1][:400]!r}, not {first[1][:400]!r}")
        back_s, ran, built, said = edit(side, daemon, spec, output["edits"][kinds[0]], True)
        arm["next_edit_s"] = round(took, 3)
        arm["next_undo_s"] = round(back_s, 3)
        arm["after_next"] = figure(daemon, reports, pair=pairs, kind="after-next-edit")
        if output["recycle"]:
            # The floor: the daemon stopped and started again on the same store and target
            # directory, which is what a recycle costs and what it gets back.
            side.cargo("stop")
            time.sleep(1)
            again = f"{label}-recycled"
            side, daemon, env, took, warm, states = start(spec, work, again, env)
            try:
                with memory.Sampler() as sampler:
                    sampler.add(daemon.pid())
                    waited, how = wait_base(daemon)
                arm["recycle"] = dict(warm_s=round(warm, 1), base_s=round(waited, 1), how=how,
                                      peak_mb=sampler.peak_mb,
                                      figure=figure(daemon, os.path.join(work, "reports", again)))
            finally:
                side.cargo("stop")
    finally:
        side.cargo("stop")
    return arm


def fit(samples, field):
    """A least-squares line of `field` against the pair number, and its r²: what M1's leak rule
    reads. `None` when there are fewer than three samples, or the line is flat."""
    points = [(it["pair"], it[field]) for it in samples if it.get(field) is not None]
    if len(points) < 3:
        return None
    n = len(points)
    mean_x = sum(x for x, _ in points) / n
    mean_y = sum(y for _, y in points) / n
    spread = sum((x - mean_x) ** 2 for x, _ in points)
    if spread == 0:
        return None
    slope = sum((x - mean_x) * (y - mean_y) for x, y in points) / spread
    line = [mean_y + slope * (x - mean_x) for x, _ in points]
    total = sum((y - mean_y) ** 2 for _, y in points)
    residual = sum((y - fitted) ** 2 for (_, y), fitted in zip(points, line))
    r2 = 1 - residual / total if total else 0.0
    return dict(slope=round(slope, 2), r2=round(r2, 3), n=n,
                per_1000=round(slope * 1000))


def leak_rule(runs):
    """M1's leak rule, as section 5 proposes it: the source is the same after every pair, so
    what the allocator holds should plateau. It is a leak when the line adds more than 5% of
    the ready size over 1,000 pairs with r² above 0.8. The holder that grows with it is named
    from the samples. Unverified: this is a proposal, not a rule anyone has run."""
    out = {}
    for label, arm in runs.items():
        for field, ready in (("allocated", "allocated"),):
            lines = [fit(arm_run["samples"], field) for arm_run in arm]
            lines = [it for it in lines if it]
            if not lines:
                continue
            slope = statistics.median(it["slope"] for it in lines)
            r2 = statistics.median(it["r2"] for it in lines)
            ready_bytes = [arm_run["samples"][0]["alloc"].get(ready) for arm_run in arm
                           if arm_run["samples"] and arm_run["samples"][0]["alloc"].get(ready)]
            ready_bytes = statistics.median(ready_bytes) if ready_bytes else 0
            over = slope * 1000 / ready_bytes if ready_bytes else 0
            out[label] = dict(field=field, slope_per_pair=slope, r2=r2, over_1000=round(over, 3),
                              leak=over > 0.05 and r2 > 0.8)
    return out


def growth_holders(runs):
    """The holders whose count grew the most over the pairs of a run: what M1 names a leak by.
    The result is (name, first, last, change) for the six that grew most, of the intern tables
    and the ingredient rows alike."""
    out = {}
    for label, arm in runs.items():
        firsts, lasts = {}, {}
        for arm_run in arm:
            for it in arm_run["samples"]:
                for kind in ("interned", "own"):
                    for name, count in it[kind].items():
                        firsts.setdefault(f"interned {name}" if kind == "interned" else f"own {name}", count)
                        lasts[f"interned {name}" if kind == "interned" else f"own {name}"] = count
                for name, count, _ in it["holders"]:
                    firsts.setdefault(f"holder {name}", count)
                    lasts[f"holder {name}"] = count
        grown = [(name, firsts.get(name, 0), lasts.get(name, 0), lasts.get(name, 0) - firsts.get(name, 0))
                 for name in lasts]
        grown.sort(key=lambda it: -it[3])
        out[label] = grown[:6]
    return out


def medians(arms):
    """The median of each arm's figures, over its runs: what the arms are compared by."""
    out = {}
    for label, runs in arms.items():
        got = dict(reclaim=runs[0]["reclaim"], runs=len(runs))
        for field in ("base_s", "peak_mb", "next_edit_s", "next_undo_s"):
            found = [it[field] for it in runs if it.get(field) is not None]
            got[field] = round(statistics.median(found), 3) if found else None
        for field in ("VmRSS", "RssAnon", "RssFile", "VmHWM", "footprint"):
            found = [it["end"]["pages"].get(field) for it in runs if it["end"]["pages"].get(field)]
            got[f"end_{field}"] = int(statistics.median(found)) if found else None
        for field in ("allocated", "resident", "mapped", "retained"):
            found = [it["end"]["alloc"].get(field) for it in runs if it["end"]["alloc"].get(field)]
            got[f"end_{field}"] = int(statistics.median(found)) if found else None
        for field in ("TyInterned", "GenericArgs", "Tys", "Clauses", "PredicateInterned"):
            found = [it["end"]["interned"].get(field) for it in runs
                     if it["end"]["interned"].get(field) is not None]
            got[f"end_{field}"] = int(statistics.median(found)) if found else None
        for field in ("checkouts", "files hashed", "rustc symbols", "rejections", "patch originals"):
            found = [it["end"]["own"].get(field) for it in runs if it["end"]["own"].get(field) is not None]
            got[f"end_{field.replace(' ', '_')}"] = int(statistics.median(found)) if found else None
        out[label] = got
    return out


def main():
    argv = sys.argv[1:]
    work = os.path.realpath(bench.options(argv, "--work", "/tmp/memgrowth")[0])
    pairs = int(bench.options(argv, "--pairs", "10")[0])
    every = int(bench.options(argv, "--reclaim-every", "5")[0])
    kinds = bench.options(argv, "--edits", ",".join(EDITS))[0].split(",")
    arms = bench.options(argv, "--arms", "0,1")[0].split(",")
    runs_wanted = max(2, int(bench.options(argv, "--runs", "2")[0]))
    extra_env = dict(it.split("=", 1) for it in bench.options(argv, "--env"))
    local = "--local" in argv
    recycle = "--recycle" in argv
    argv = [it for it in argv if it not in ("--recycle", "--local")]
    name, = argv
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    if local:
        spec = local_spec(name, work)
    else:
        spec = dict(bench.PROJECTS[name])
        spec["ws"] = os.path.realpath(os.path.join(work, name))
        shutil.copytree(fetch(name, spec), spec["ws"], symlinks=True,
                        ignore=shutil.ignore_patterns("target", "target-ref", ".git"))
    edits = edits_for(spec["ws"])
    if not edits:
        raise SystemExit(f"{name}: no edit of any kind is possible in {spec['ws']}")
    left_out = [kind for kind in kinds if kind not in edits]
    if left_out:
        print(f"no edit of {', '.join(left_out)}: the project has none", flush=True)
    kinds = [kind for kind in kinds if kind in edits]
    build = spec.get("build", [])
    runs, arms_of = 0, {}
    for run in range(1, runs_wanted + 1):
        for arm in arms:
            label = f"reclaim-{arm.replace(',', '+')}-{run}"
            print(f"run {run}/{runs_wanted}: {label}", flush=True)
            got = one_arm(spec, work, label, arm, pairs, every, kinds, build,
                          dict(env=extra_env, edits=edits, run=run, recycle=recycle))
            arms_of.setdefault(f"reclaim-{arm.replace(',', '+')}", []).append(got)
            runs += 1
    med = medians(arms_of)
    leaks, grew = leak_rule(arms_of), growth_holders(arms_of)
    print("\nmedians over the runs:")
    for label, got in med.items():
        print(f"  {label:16} end rss {got['end_VmRSS'] or got['end_footprint']} MB  "
              f"anon {got['end_RssAnon']}  hwm {got['end_VmHWM']}  "
              f"allocated {(got['end_allocated'] or 0) >> 20} MB  retained {(got['end_retained'] or 0) >> 20} MB  "
              f"ty {got['end_TyInterned']} args {got['end_GenericArgs']}  "
              f"checkouts {got['end_checkouts']}  "
              f"next edit {got['next_edit_s']} s", flush=True)
        print(f"    resident {(got['end_resident'] or 0) >> 20} MB  mapped {(got['end_mapped'] or 0) >> 20} MB  "
              f"interned: tys {got['end_Tys']} clauses {got['end_Clauses']} predicates {got['end_PredicateInterned']}  "
              f"own: files hashed {got['end_files_hashed']} rustc symbols {got['end_rustc_symbols']} "
              f"rejections {got['end_rejections']} patch originals {got['end_patch_originals']}", flush=True)
        if label in leaks:
            rule = leaks[label]
            verdict = "LEAK by the rule" if rule["leak"] else "no leak by the rule"
            print(f"    M1: allocated grows {rule['slope_per_pair']} bytes a pair, "
                  f"{rule['over_1000']:.1%} of the first over 1,000, r² {rule['r2']}: {verdict} "
                  f"(the rule is a proposal, unverified)", flush=True)
        if label in grew:
            named = ", ".join(f"{name} {first} to {last} ({change:+})" for name, first, last, change in grew[label])
            print(f"    grew most: {named}", flush=True)
    print(json.dumps(dict(project=name, pairs=pairs, reclaim_every=every, kinds=kinds,
                          arms=arms, runs=runs, medians=med, leak_rule=leaks, grew=grew,
                          samples=arms_of)))


if __name__ == "__main__":
    main()
