#!/usr/bin/env python3
"""crossmachine.py: the same commit built on two machines, and whether they agree on every key.

    crossmachine.py [--all | FIXTURE...] [--blaze-bin DIR] [--work DIR] [--online] [--keep] [--json FILE]
    crossmachine.py --self-test
    crossmachine.py --list

Unit C4 of docs/distributed.md, section 1. A store is shared between machines only if the key of
each thing in it is a function of that thing's content: the same source built on a machine with
another home, another cargo home, another checkout path and another target directory has to give
the same keys, and the same bytes under them. Anything else is a miss on the other machine at best.

Two machines are made on this one (`Machine`): each has a `HOME`, a `CARGO_HOME` of its own (the
registry crates are extracted again below it, so their paths differ), a checkout at a path of another
depth and length, its own target directories, its own store and its own daemon directory. For each
fixture the harness then builds:

  A        machine A, empty store: `cargo ra-jit build`, the program run; the store is sealed and
           pushed to a directory remote (`ra-jit cache push`).
  B-solo   machine B, empty store, no remote: the same build. Its store is set against A's, entry by
           entry (`ra-jit cache digests`: the key on disk and the SHA-256 of the value). This is the
           test of the section: the same commit gives equal entry digests.
  B-fed    machine B, an empty store that has A's pulled from the remote: the same build again. What
           it compiled, ran and expanded is what A's keys did not give it, and each is named with
           its cause. B's program has to print what cargo's prints on B.

Every build is `RA_JIT_RUSTC_BASE=0`, as warmstart.py's: the daemon compiles the whole closure itself,
which is where a key made from a path matters. `RA_JIT_KEYS_LOG` (hir-ty-codegen) lists the entries a
build added with their stable names, so a key that differs is named with the crate and the file it is
of, and `ra-jit cache digests` (ra-jit-store) lists what the packs hold. The report says, for each
fixture: functions held by both stores, held by one, and held by both with different bytes; then the
same for every other record (frozen definition maps, expansions, build-script runs, types); what B-fed
compiled, found, ran and expanded; and whether each program printed what cargo's does on its machine.

A difference is named: the key, and why. A difference whose fix is another unit's is `pending` in the
table (PENDING below) and does not fail the run; every other difference does. The exit status is 0
when every fixture has equal entry digests, a fed build that compiled nothing and programs that print
what cargo's do, up to what is pending.

Fixtures (`--list`): hello and ripgrep are harness/dist/fixtures.toml's, fetched as fetch.py does;
paths, vendored and macros are harness/dist/fixtures/paths, one repository copied whole to each
machine: a path dependency outside the workspace, a registry dependency that panics inside its own
source, `file!()`, an `OUT_DIR` file, a vendored crate, and proc macros.

The memory guard of standing order 14 applies to every run: RA_JIT_MEMORY_MAX=16G,
RA_JIT_OWN_BASE_MEMORY=12G and RA_JIT_SPARE=0, and the watchdog of harness/tracedecay.py over the
run's whole process tree. Heavy work: run it as
`with-heavy-lock.sh c4-keys python3 -I harness/dist/crossmachine.py --all`.

Python 3 standard library only. macOS and Linux.
"""
import argparse, hashlib, json, os, re, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
ROOT = os.path.dirname(HARNESS)
for path in (HARNESS, HERE):
    if path not in sys.path:
        sys.path.insert(0, path)

# Where the unit's binaries are kept (the shared builds directory moved from ~/dev to ~/code on 2026-10-26).
DEFAULT_BIN = next((it for it in ("/Users/bytedance/code/blaze-targets/c4-keys/bin",
                                  "/Users/bytedance/dev/blaze-targets/c4-keys/bin") if os.path.isdir(it)),
                   os.path.join(ROOT, "target", "release"))
DEFAULT_WORK = "/tmp/c4xm"
FIXTURE_DIR = os.path.join(HERE, "fixtures", "paths")
TOOLCHAIN = open(os.path.join(HARNESS, "toolchain")).read().strip()

# What the memory guard sets on every blaze run (standing order 14); nothing here raises it.
GUARD_ENV = {"RA_JIT_MEMORY_MAX": "16G", "RA_JIT_OWN_BASE_MEMORY": "12G", "RA_JIT_SPARE": "0"}

# The fixtures. `source` is where the repository comes from; `workspace` is the cargo workspace in
# it; `package` and `run` are what is built and run; `env` is set on every ra-jit build of it.
FIXTURES = {
    "hello": dict(source="pinned", workspace=".", package="hello", run=[],
                  about="one binary, one library: nothing but the program's own crates"),
    "paths": dict(source="dir", workspace="workspace", package="app", run=[],
                  about="a path dependency outside the workspace, a registry crate that panics inside its own "
                        "source, an OUT_DIR file, file!(), panics, the checkout's path as data"),
    "vendored": dict(source="dir", workspace="vendored", package="app", run=[],
                     about="a vendored crate (source replacement, as `cargo vendor` lays it out)"),
    "macros": dict(source="dir", workspace="macros", package="app", run=[],
                   env={"RA_JIT_PROC_MACRO_CACHE_SKIP": "Embedded"},
                   about="a proc-macro crate: two pure macros and a derive that reads a file while it expands"),
    "ripgrep": dict(source="pinned", workspace=".", package="ripgrep", run=None,
                    about="13 crates, two build scripts, registry dependencies"),
    # Acceptance names it; it is 165,752 functions and the project whose memory G0 failed, so it is
    # asked for by name and not part of `--all`.
    "nushell": dict(source="pinned", workspace=".", package="nu", run=None,
                    about="the scale arm: 165,752 functions; asked for by name, not in --all"),
}
# `--all` runs these in this order: the small ones first, so a break shows early.
ALL = ["hello", "paths", "vendored", "macros", "ripgrep"]

# A difference whose fix is another unit's, or that waits on a thing no unit here owns. A kind is
# the `kind` of a Difference; the value is why it is not failing this run. Nothing else passes.
PENDING = {
    "expansion-library": "the macro library is built by rustc on each machine, and its bytes hold that machine's paths "
                         "and signature; a key by the library's content needs the crate's content identity "
                         "(own proc-macro server, no-rustc.md step J1, where the library is restored from the store "
                         "by key) or recorded reads (#147)",
    "script-run": "build-script run keys hash the script's executable (cargo/script.rs run_key); "
                  "unit p05r owns script.rs and own_script.rs (docs/distributed.md, P0.5), and the key is "
                  "fixed after it lands",
}


class Difference:
    """One thing that differs between the machines: which key, and why."""

    def __init__(self, fixture, kind, key, why, count=1, examples=()):
        self.fixture, self.kind, self.key, self.why = fixture, kind, key, why
        self.count, self.examples = count, list(examples)

    @property
    def pending(self):
        return PENDING.get(self.kind)

    def row(self):
        return dict(fixture=self.fixture, kind=self.kind, key=self.key, why=self.why, count=self.count,
                    examples=self.examples[:3], pending=self.pending)


# --- stores, as `ra-jit cache digests` lists them -------------------------------------------------------

def read_digests(text):
    """`KEY LENGTH SHA256` lines as {key: (length, digest)}."""
    found = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) != 3 or not re.fullmatch(r"[0-9a-f]{64}", parts[0]) or not re.fullmatch(r"[0-9a-f]{64}", parts[2]):
            raise ValueError(f"not a digest line: {line!r}")
        found.setdefault(parts[0], set()).add((int(parts[1]), parts[2]))
    return found


def read_keys_log(text):
    """`KEY<TAB>VALUE-DIGEST<TAB>NAME` lines of RA_JIT_KEYS_LOG as {key: (value digest, name)}."""
    found = {}
    for line in text.splitlines():
        parts = line.split("\t", 2)
        if len(parts) == 3 and re.fullmatch(r"[0-9a-f]{64}", parts[0]):
            found[parts[0]] = (parts[1], parts[2])
    return found


def read_macro_log(text):
    """The `macro` and `macro-env` lines of RA_JIT_KEYS_LOG (ra-jit-expand): each call with the parts of its
    key and what became of it, and the environments the calls had, by their hash."""
    calls, envs = [], {}
    for line in text.splitlines():
        parts = line.split("\t")
        if parts[0] == "macro" and len(parts) >= 9:
            parts_of = dict(p.split("=", 1) for p in parts[4:8] if "=" in p)
            calls.append(dict(kind=parts[1], name=parts[2], key=parts[3], outcome=parts[8], **parts_of))
        elif parts[0] == "macro-env" and len(parts) >= 3:
            envs[parts[1]] = dict(p.split("=", 1) for p in parts[2:] if "=" in p)
    return calls, envs


def compare_macros(fixture, a_log, b_log):
    """What B expanded that A had kept or could have, by which part of the key differs. A call is told by
    what is not machine's: the macro's kind and name and its input."""
    (a_calls, a_envs), (b_calls, b_envs) = a_log, b_log
    a_by = {(c["kind"], c["name"], c.get("input")): c for c in a_calls}
    causes, kinds = {}, {}
    for c in b_calls:
        if c["outcome"] != "expanded":
            continue
        a = a_by.get((c["kind"], c["name"], c.get("input")))
        who = f"{c['kind']} {c['name']}"
        if a is None:
            cause, kind = "no call of that macro with that input on A", "expansion"
        else:
            differing = [k for k in ("dylib", "env", "cwd") if a.get(k) != c.get(k)]
            if not differing:
                cause, kind = f"the key is A's, A did not keep it: {a['outcome']}", "expansion"
            else:
                bits = []
                if "env" in differing:
                    ours, theirs = a_envs.get(a.get("env"), {}), b_envs.get(c.get("env"), {})
                    names = sorted(n for n in set(ours) | set(theirs) if ours.get(n) != theirs.get(n))
                    bits.append("environment " + ", ".join(f"{n} ({ours.get(n)!r} on A, {theirs.get(n)!r} on B)" for n in names[:4]))
                if "cwd" in differing:
                    bits.append("current directory")
                if "dylib" in differing:
                    bits.append("macro library bytes")
                cause = "key differs by " + "; ".join(bits)
                kind = "expansion-library" if differing == ["dylib"] else "expansion"
        causes.setdefault((cause, kind), []).append(who)
    diffs = []
    for (cause, kind), whos in sorted(causes.items(), key=lambda it: -len(it[1])):
        diffs.append(Difference(fixture, kind, f"{len(whos)} macro expansions B ran with A's store", cause, len(whos),
                                sorted(set(whos))))
    return diffs


# A crate in a stable name is a run of hex, as `OF_CHECKOUT`, `OF_LIBRARY` or rustc's id spell it.
HEX_ID = re.compile(r"[0-9a-f]{16,64}")


def skeleton(name):
    """A stable name with every crate id taken out: two names with one skeleton are one function."""
    return HEX_ID.sub("#", name)


def ids_of(name):
    return HEX_ID.findall(name)


def of_file(name):
    """The file a stable name says its function is written in, for naming a cause."""
    m = re.search(r"[0-9a-f]{16,64}:([^#<]*)#", name)
    return m.group(1) if m else "?"


def compare_stores(fixture, a_digests, b_digests, a_names, b_names):
    """Sets A's store against B's. Returns (numbers, differences). The functions are the entries
    `RA_JIT_KEYS_LOG` named; the rest are the other records: frozen definition maps, expansions,
    build-script runs, type sets."""
    numbers, diffs = {}, []
    a_keys, b_keys = set(a_digests), set(b_digests)
    for label, in_class in (("functions", lambda k: k in a_names or k in b_names), ("other", lambda k: not (k in a_names or k in b_names))):
        a_k = {k for k in a_keys if in_class(k)}
        b_k = {k for k in b_keys if in_class(k)}
        both = a_k & b_k
        different = {k for k in both if a_digests[k] != b_digests[k]}
        numbers[label] = dict(a=len(a_k), b=len(b_k), both=len(both), only_a=len(a_k - b_k), only_b=len(b_k - a_k),
                              differing=len(different))
        if label != "functions":
            if a_k != b_k:
                diffs.append(Difference(fixture, "other-record", "records that are not functions",
                                        f"{len(a_k - b_k)} only on A, {len(b_k - a_k)} only on B: frozen definition maps, "
                                        "expansions, build-script runs and type sets carry no name in the log, so the "
                                        "cause is read from the fed build's counts below",
                                        len(a_k ^ b_k)))
            continue
        # Functions that only one machine made: the same function under another id is a key that
        # holds something of the machine; a function on one side alone is another thing.
        a_only = {k: a_names[k][1] for k in a_k - b_k if k in a_names}
        b_only = {k: b_names[k][1] for k in b_k - a_k if k in b_names}
        by_skeleton_a, by_skeleton_b = {}, {}
        for name in a_only.values():
            by_skeleton_a.setdefault(skeleton(name), []).append(name)
        for name in b_only.values():
            by_skeleton_b.setdefault(skeleton(name), []).append(name)
        causes = {}
        for sk in sorted(set(by_skeleton_a) | set(by_skeleton_b)):
            left, right = sorted(by_skeleton_a.get(sk, [])), sorted(by_skeleton_b.get(sk, []))
            if left and right:
                for l, r in zip(left, right):
                    for x, y in zip(ids_of(l), ids_of(r)):
                        if x != y:
                            causes.setdefault((x[:12], y[:12], of_file(l)), []).append(l)
                extra = abs(len(left) - len(right))
                if extra:
                    causes.setdefault(("count", str(extra), of_file((left or right)[0])), []).append((left or right)[0])
            else:
                who = "A" if left else "B"
                causes.setdefault((who + " alone", "", of_file((left or right)[0])), []).extend(left or right)
        for (x, y, where), names in sorted(causes.items(), key=lambda it: -len(it[1])):
            if y and x != "count":
                diffs.append(Difference(fixture, "crate-id", f"crate id {x}.. on A, {y}.. on B, in {where}",
                                        "the same function is under two crate ids: the id of a crate holds "
                                        "something that differs between the machines (a path, a home)",
                                        len(names), names))
            elif x == "count":
                diffs.append(Difference(fixture, "function-count", f"{where}",
                                        f"one machine made {y} more of the same function than the other",
                                        len(names), names))
            else:
                diffs.append(Difference(fixture, "function-alone", f"{x} in {where}",
                                        "a function one machine made and the other did not: not a key that differs "
                                        "but a function that is not in the other's build", len(names), names))
        if different:
            sample = sorted(different)[:3]
            named = [a_names.get(k, b_names.get(k, ("", k)))[1] for k in sample]
            diffs.append(Difference(fixture, "value", "the bytes under a key both hold",
                                    "the same key holds different code: something of the machine is in the entry "
                                    "(a path in data, a layout, a symbol)", len(different), named))
    return numbers, diffs


# --- the machines --------------------------------------------------------------------------------------

def sh(args, **kw):
    kw.setdefault("text", True)
    kw.setdefault("stdout", subprocess.PIPE)
    kw.setdefault("stderr", subprocess.STDOUT)
    return subprocess.run(args, **kw)


class Machine:
    """A home, a cargo home, a checkout path, target directories, stores and a daemon directory of
    its own. Everything under it has a path that differs from the other machine's, in depth as well
    as in name, so a path that leaks into a key cannot be the same by chance."""

    def __init__(self, top, name, depth_words, seed_cargo_home, rust_home):
        self.name = name
        self.top = os.path.join(top, name)
        self.home = os.path.join(self.top, "u", *depth_words, "home")
        self.cargo_home = os.path.join(self.home, ".cargo")
        self.checkouts = os.path.join(self.top, "w", *depth_words)
        self.rust_home = rust_home
        os.makedirs(self.home)
        os.makedirs(self.checkouts)
        # What cargo downloaded, and not what it extracted: the sources are extracted again below this
        # cargo home, where the path of every registry crate is this machine's.
        os.makedirs(os.path.join(self.cargo_home, "registry"))
        for kept in ("cache", "index"):
            src = os.path.join(seed_cargo_home, "registry", kept)
            if os.path.isdir(src):
                clone(src, os.path.join(self.cargo_home, "registry", kept))

    def checkout(self, fixture, source):
        """The fixture's repository at this machine's path for it."""
        dest = os.path.join(self.checkouts, fixture + ("-repo" if self.name == "b" else ""))
        shutil.copytree(source, dest, symlinks=True, ignore=shutil.ignore_patterns(".git", "target", "target-*"))
        return dest

    def env(self, bin_dir, store, ra_home, extra=None):
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("RA_JIT_", "CARGO_", "RUSTC", "AWS_")) and k not in ("RUSTFLAGS", "CARGO_TARGET_DIR")}
        env.update(
            HOME=self.home, CARGO_HOME=self.cargo_home, RUSTUP_HOME=self.rust_home, RUSTUP_TOOLCHAIN=TOOLCHAIN,
            PATH=bin_dir + os.pathsep + env.get("PATH", ""),
            RA_JIT_HOME=ra_home, RA_JIT_CACHE=store, RA_JIT_RUSTC_BASE="0", RA_JIT_STATS="1",
            RA_JIT_CACHE_SEAL_AT="1", RA_JIT_REMOTE_UNSIGNED="1",
            RUST_BACKTRACE="0", CARGO_TERM_COLOR="never", CARGO_NET_OFFLINE="true", CARGO_INCREMENTAL="0")
        env.update(GUARD_ENV)
        env.update(extra or {})
        return env


def clone(src, dest):
    """A copy that costs no disk on APFS and a plain copy elsewhere."""
    flag = ["-c"] if sys.platform == "darwin" else ["--reflink=auto"]
    ran = sh(["cp", *flag, "-R", src, dest])
    if ran.returncode:
        shutil.copytree(src, dest, symlinks=True)


def seed_cargo_home(work, fixtures, sources, online):
    """One cargo home that has fetched what every fixture needs. The machines clone its downloads."""
    seed = os.path.join(work, "seed-cargo-home")
    users = os.environ.get("CARGO_HOME") or os.path.expanduser("~/.cargo")
    os.makedirs(os.path.join(seed, "registry"), exist_ok=True)
    for kept in ("cache", "index"):
        src, dest = os.path.join(users, "registry", kept), os.path.join(seed, "registry", kept)
        if os.path.isdir(src) and not os.path.isdir(dest):
            clone(src, dest)
    env = dict(os.environ, CARGO_HOME=seed, RUSTUP_TOOLCHAIN=TOOLCHAIN, CARGO_NET_OFFLINE="false" if online else "true",
               RUST_BACKTRACE="0")
    for name in fixtures:
        spec = FIXTURES[name]
        ws = os.path.join(sources[name], spec["workspace"])
        ran = sh(["cargo", "fetch", "--locked"], cwd=ws, env=env)
        if ran.returncode:
            raise SystemExit(f"{name}: cargo fetch --locked in {ws} failed (use --online to download):\n{ran.stdout[-1500:]}")
    return seed


def source_of(name, work, offline=True):
    """Where a fixture's repository is: pinned ones as fetch.py leaves them, the others in the tree."""
    spec = FIXTURES[name]
    if spec["source"] == "dir":
        return FIXTURE_DIR
    import fetch
    pinned = {f["name"]: f for f in fetch.fixtures()}[name]
    dest = os.path.join(os.path.expanduser("~"), "work", name)
    if pinned["kind"] == "repo":
        fetch.fetch_repo(pinned, dest, offline)
    else:
        fetch.fetch_project(pinned, dest, offline)
    return dest


def pinned_run(name):
    import fetch
    return list({f["name"]: f for f in fetch.fixtures()}[name].get("program_args", []))


# --- one build -----------------------------------------------------------------------------------------

class Run:
    """Everything one build of a fixture on one machine said and left."""

    def __init__(self, label):
        self.label = label
        self.out = None           # (returncode, stdout, stderr) of the program
        self.expected = None      # the same, from plain cargo on the same machine
        self.said = ""            # the workers' log, and cargo's stderr
        self.digests = {}
        self.names = {}
        self.seconds = 0.0
        self.error = None

    @property
    def right(self):
        return self.error is None and self.out is not None and self.out == self.expected


def counts(said):
    """What a build said of the store, the scripts and the macros."""
    functions = re.findall(r"^store: (\d+) functions found, (\d+) compiled and added$", said, re.M)
    expanded = re.findall(r"^proc macros: (\d+) expanded in [\d.]+s, (\d+) from the store", said, re.M)
    frozen = re.search(r"^frozen, of \d+ crates: def maps of (\d+).* from the store, (\d+) written", said, re.M)
    return dict(
        found=sum(int(f) for f, _ in functions), compiled=sum(int(c) for _, c in functions),
        expanded=sum(int(e) for e, _ in expanded), expansions_found=sum(int(f) for _, f in expanded),
        scripts_restored=len(re.findall(r"a run the store kept", said)),
        frozen_found=int(frozen[1]) if frozen else 0, frozen_written=int(frozen[2]) if frozen else 0)


def reasons(said, label):
    """The `not kept: [("why", n), ...]` and `not found: [...]` lines a build printed, summed:
    why instances with a stable name were not added to the store, and why they were compiled."""
    found = {}
    for line in re.findall(rf"^{label}: \[(.*)\]$", said, re.M):
        for why, n in re.findall(r'\("([^"]*)", (\d+)\)', line):
            found[why] = found.get(why, 0) + int(n)
    return found


def entries_line(said):
    """`entries: F found, A added, S as stored, D not as stored, U instances and R requests without a
    stable name`, summed over the builds."""
    total = dict(found=0, added=0, same=0, differing=0, unnamed=0, unnamed_requests=0)
    for m in re.finditer(r"^entries: (\d+) found, (\d+) added, (\d+) as stored, (\d+) not as stored, "
                         r"(\d+) instances and (\d+) requests without a stable name$", said, re.M):
        for key, n in zip(total, m.groups()):
            total[key] += int(n)
    return total


def workers_log(side):
    """What the workers of a side's daemon logged, whole."""
    _, status = side.cargo("status")
    text = ""
    for log in re.findall(r"worker: pid \d+, .*, log (.*)$", status.stdout, re.M):
        if os.path.exists(log):
            text += open(log, errors="replace").read()
    return text


def build_and_run(machine, bin_dir, ws, spec, label, store, ra_home, keys_log, extra_env, progress):
    """`cargo ra-jit build`, the program, the daemon stopped (so the store is whole). A `Run`."""
    import cargo_ra_jit
    from cargo_ra_jit import Side, program_output, wait_warm
    run = Run(label)
    env = machine.env(bin_dir, store, ra_home, dict(extra_env, RA_JIT_KEYS_LOG=keys_log))
    side = Side(ws, env, True)
    package = ["-p", spec["package"]]
    args = spec["run"] if spec["run"] is not None else []
    began = time.time()
    try:
        took, built = side.cargo("build", *package, timeout=1800)
        run.said += built.stderr
        if built.returncode:
            run.error = f"cargo ra-jit build failed ({built.returncode}): {built.stderr[-1500:]}"
            return run
        wait_warm(side, ws)
        _, ran = side.cargo("run", *package, "--", *args, timeout=600)
        run.out = program_output(ran)
        run.said += workers_log(side)
    except Exception as e:  # a harness bug or a timeout is a result of the run, not a crash of the others
        run.error = f"{type(e).__name__}: {e}"
        return run
    finally:
        side.cargo("stop")
        run.seconds = time.time() - began
    progress(f"    {label}: build {took:.1f}s, total {run.seconds:.1f}s")
    return run


def reference(machine, bin_dir, ws, spec, store, ra_home):
    """What plain cargo's program prints on this machine."""
    from cargo_ra_jit import Side, program_output
    env = machine.env(bin_dir, store, ra_home)
    # Plain cargo: nothing of ra-jit's, but the directory `Side` keeps its tally in.
    for k in [k for k in env if k.startswith("RA_JIT_") and k != "RA_JIT_HOME"]:
        env.pop(k)
    side = Side(ws, env, False)
    package = ["-p", spec["package"]]
    _, built = side.cargo("build", *package, timeout=1800)
    if built.returncode:
        raise SystemExit(f"cargo build failed on machine {machine.name} in {ws}:\n{built.stderr[-2000:]}")
    _, ran = side.cargo("run", *package, "--", *(spec["run"] or []))
    return program_output(ran)


def digests_of(bin_dir, store, env):
    ran = sh([os.path.join(bin_dir, "ra-jit"), "cache", "digests"], env=dict(env, RA_JIT_CACHE=store))
    if ran.returncode:
        raise SystemExit(f"ra-jit cache digests failed on {store}: {ran.stdout[-800:]}")
    return read_digests(ran.stdout)


# --- one fixture ---------------------------------------------------------------------------------------

def fixture_report(name, args, work, bin_dir, machines, sources, progress):
    spec = dict(FIXTURES[name])
    if spec["run"] is None:
        spec["run"] = pinned_run(name)
    a, b = machines
    top = os.path.join(work, name)
    os.makedirs(top)
    remote = os.path.join(top, "remote")
    os.makedirs(remote)
    extra = dict(spec.get("env", {}))
    result = dict(fixture=name, about=spec["about"], runs={}, numbers={}, differences=[])
    runs = {}
    wss = {}
    for m in (a, b):
        wss[m.name] = os.path.join(m.checkout(name, sources[name]), spec["workspace"])
    stores = {"a": os.path.join(a.top, "store-" + name), "b-solo": os.path.join(b.top, "store-solo-" + name),
              "b-fed": os.path.join(b.top, "store-fed-" + name)}
    # Short: a daemon's socket lives below its directory, and a socket path has 104 bytes on macOS.
    homes = {k: os.path.join(work, "h", f"{name[:4]}-{k}") for k in stores}
    for k in stores:
        os.makedirs(stores[k], exist_ok=True)

    progress(f"  {name}: reference builds with cargo")
    expected = {m.name: reference(m, bin_dir, wss[m.name], spec, stores["a"], homes["a"]) for m in (a, b)}

    progress(f"  {name}: A")
    runs["a"] = build_and_run(a, bin_dir, wss["a"], spec, "A", stores["a"], homes["a"], os.path.join(top, "keys-a.log"), extra, progress)
    runs["a"].expected = expected["a"]
    env_a = a.env(bin_dir, stores["a"], homes["a"])
    pushed = sh([os.path.join(bin_dir, "ra-jit"), "cache", "push", remote], env=env_a)
    result["pushed"] = pushed.stdout.strip()[-200:]

    progress(f"  {name}: B (own store, no remote)")
    runs["b-solo"] = build_and_run(b, bin_dir, wss["b"], spec, "B-solo", stores["b-solo"], homes["b-solo"],
                                   os.path.join(top, "keys-b-solo.log"), extra, progress)
    runs["b-solo"].expected = expected["b"]

    progress(f"  {name}: B (store pulled from A's remote)")
    env_b = b.env(bin_dir, stores["b-fed"], homes["b-fed"])
    pulled = sh([os.path.join(bin_dir, "ra-jit"), "cache", "pull", remote], env=env_b)
    result["pulled"] = pulled.stdout.strip()[-200:]
    runs["b-fed"] = build_and_run(b, bin_dir, wss["b"], spec, "B-fed", stores["b-fed"], homes["b-fed"],
                                  os.path.join(top, "keys-b-fed.log"), dict(extra, RA_JIT_REMOTE=remote, RA_JIT_REMOTE_READONLY="1"),
                                  progress)
    runs["b-fed"].expected = expected["b"]

    for k, r in runs.items():
        with open(os.path.join(top, f"said-{k}.txt"), "w") as f:
            f.write(r.said)
        result["runs"][k] = dict(right=r.right, error=r.error, seconds=round(r.seconds, 1), **counts(r.said))
    broken = [r for r in runs.values() if r.error]
    if broken:
        for r in broken:
            result["differences"].append(Difference(name, "build", r.label, r.error).row())
        return result

    def read_log(label):
        path = os.path.join(top, f"keys-{label}.log")
        return read_keys_log(open(path).read()) if os.path.exists(path) else {}

    a_names, solo_names, fed_names = read_log("a"), read_log("b-solo"), read_log("b-fed")
    d_a = digests_of(bin_dir, stores["a"], env_a)
    d_solo = digests_of(bin_dir, stores["b-solo"], b.env(bin_dir, stores["b-solo"], homes["b-solo"]))
    numbers, diffs = compare_stores(name, d_a, d_solo, a_names, solo_names)
    result["numbers"] = numbers
    for d in diffs:
        result["differences"].append(d.row())

    # What B compiled with A's entries in its store: each is a function A's key did not give it.
    fed = runs["b-fed"]
    c = counts(fed.said)
    a_by_name = {n: k for k, (_, n) in a_names.items()}
    compiled = []
    for k, (digest, n) in fed_names.items():
        if n in a_by_name:
            compiled.append(("key equal, entry refused", n))
        elif skeleton(n) in {skeleton(x) for x in a_by_name}:
            compiled.append(("crate id differs", n))
        else:
            compiled.append(("no such function on A", n))
    kinds = {}
    for why, n in compiled:
        kinds.setdefault(why, []).append(n)
    result["fed"] = dict(found=c["found"], compiled=c["compiled"], logged=len(fed_names),
                         causes={why: len(ns) for why, ns in kinds.items()}, entries=entries_line(fed.said),
                         not_kept=reasons(fed.said, "not kept"), not_found=reasons(fed.said, "not found"))
    # What B compiled that no entry could be kept for is not a key that differs: it is a function the
    # store does not hold on either machine, and the build says why.
    for why, n in sorted(result["fed"]["not_kept"].items(), key=lambda it: -it[1]):
        result["differences"].append(Difference(name, "not-kept", why, "compiled on every machine, by design or "
                                                "until a rebinding exists for it", n).row())
    for why, ns in kinds.items():
        kind = {"key equal, entry refused": "entry-refused", "crate id differs": "crate-id-fed",
                "no such function on A": "function-alone"}[why]
        result["differences"].append(Difference(name, kind, f"{len(ns)} functions B compiled with A's store",
                                                why, len(ns), ns).row())
    # Build scripts: a run the store kept is a script B did not run.
    scripts = build_scripts(b, bin_dir, wss["b"], spec["package"], stores["b-fed"], homes["b-fed"])
    restored = min(len(scripts), c["scripts_restored"])
    result["fed"]["scripts"] = len(scripts)
    result["fed"]["scripts_restored"] = restored
    result["fed"]["scripts_run"] = len(scripts) - restored
    if len(scripts) > restored:
        result["differences"].append(Difference(
            name, "script-run", f"{len(scripts) - restored} of {len(scripts)} build-script runs B did not find in A's store",
            "the run key hashes the script's executable and its environment (cargo/script.rs run_key)",
            len(scripts) - restored, scripts).row())
    # Macros: expansions B did not find, by the part of the key that differs.
    result["fed"]["expanded"] = c["expanded"]
    result["fed"]["expansions_found"] = c["expansions_found"]
    a_macros = read_macro_log(open(os.path.join(top, "keys-a.log")).read()) if os.path.exists(os.path.join(top, "keys-a.log")) else ([], {})
    fed_macros = read_macro_log(open(os.path.join(top, "keys-b-fed.log")).read()) if os.path.exists(os.path.join(top, "keys-b-fed.log")) else ([], {})
    if c["expanded"]:
        if fed_macros[0]:
            for d in compare_macros(name, a_macros, fed_macros):
                result["differences"].append(d.row())
        else:
            result["differences"].append(Difference(
                name, "expansion", f"{c['expanded']} macro expansions B ran with A's store", expansion_cause(fed.said, spec),
                c["expanded"]).row())
    for k, r in runs.items():
        if not r.right:
            result["differences"].append(Difference(name, "program", r.label, program_diff(r)).row())
    return result


def build_scripts(machine, bin_dir, ws, package, store, ra_home):
    """The packages with a build script that the build of `package` runs: cargo's resolution for
    this host, from the root over the dependencies that are built (not the dev-dependencies)."""
    env = machine.env(bin_dir, store, ra_home)
    for k in [k for k in env if k.startswith("RA_JIT_")]:
        env.pop(k)
    host = sh(["rustc", "-vV"], env=env).stdout
    triple = re.search(r"host: (\S+)", host)[1]
    ran = sh(["cargo", "metadata", "--format-version", "1", "--offline", "--filter-platform", triple],
             cwd=ws, env=env, stderr=subprocess.DEVNULL)
    if ran.returncode:
        raise SystemExit(f"cargo metadata failed in {ws}")
    meta = json.loads(ran.stdout)
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = next(i for i, p in packages.items() if p["name"] == package and i in meta["workspace_members"])
    seen, todo = set(), [root]
    while todo:
        at = todo.pop()
        if at in seen:
            continue
        seen.add(at)
        for dep in nodes[at]["deps"]:
            if any(k["kind"] != "dev" for k in dep["dep_kinds"]):
                todo.append(dep["pkg"])
    return sorted(f"{packages[i]['name']} {packages[i]['version']}" for i in seen
                  if any("custom-build" in t["kind"] for t in packages[i]["targets"]))


def expansion_cause(said, spec):
    skipped = spec.get("env", {}).get("RA_JIT_PROC_MACRO_CACHE_SKIP")
    why = ("the key holds the macro library's bytes, the crate's environment (CARGO_MANIFEST_DIR, OUT_DIR) and the "
           "current directory (ra-jit-expand)")
    if skipped:
        why += f"; {skipped} is named in RA_JIT_PROC_MACRO_CACHE_SKIP and is expanded every time by design"
    m = re.search(r"^proc macros: .*$", said, re.M)
    return why + (f" [{m.group(0)}]" if m else "")


def program_diff(run):
    if run.out is None:
        return "the program did not run"
    got, want = run.out, run.expected
    lines = []
    for label, g, w in zip(("status", "stdout", "stderr"), got, want):
        if g != w:
            lines.append(f"{label}: got {str(g)[:300]!r}, cargo gave {str(w)[:300]!r}")
    return "; ".join(lines) or "differs"


def reference_only(names, work, bin_dir, machines, sources, progress):
    """Plain cargo's build and program on both machines, for each fixture: the part of a run that needs
    no daemon, to see that the machines are set up (cargo home, vendoring, paths) before one is loaded."""
    a, b = machines
    bad = 0
    for name in names:
        spec = dict(FIXTURES[name])
        if spec["run"] is None:
            spec["run"] = pinned_run(name)
        outs = {}
        for m in (a, b):
            ws = os.path.join(m.checkout(name, sources[name]), spec["workspace"])
            store, home = os.path.join(m.top, "store-ref-" + name), os.path.join(work, "h", name[:4] + "-ref" + m.name)
            outs[m.name] = reference(m, bin_dir, ws, spec, store, home)
            progress(f"  {name} on {m.name}: {ws}\n    status {outs[m.name][0]}, stdout {len(outs[m.name][1])} bytes")
            for line in outs[m.name][1].splitlines()[:12]:
                progress(f"      | {line[:200]}")
        same = outs["a"] == outs["b"]
        progress(f"  {name}: the two machines' programs print {'the same' if same else 'different things (paths)'}")
        bad += outs["a"][0] != 0 or outs["b"][0] != 0
    return bad


# --- report --------------------------------------------------------------------------------------------

def render(results):
    out = []
    for r in results:
        out.append(f"== {r['fixture']}: {r['about']}")
        for k, v in r["runs"].items():
            if v.get("error"):
                out.append(f"  {k:7} FAILED: {v['error'][:300]}")
                continue
            out.append(f"  {k:7} {'prints what cargo prints' if v['right'] else 'DIFFERENT OUTPUT':26} {v['seconds']:7.1f}s  "
                       f"functions {v['found']} found {v['compiled']} compiled; "
                       f"macros {v['expanded']} expanded {v['expansions_found']} found; "
                       f"def maps {v['frozen_found']} found {v['frozen_written']} written")
        for label, n in r.get("numbers", {}).items():
            out.append(f"  A against B-solo, {label:9}: A {n['a']}, B {n['b']}, both {n['both']}, "
                       f"only A {n['only_a']}, only B {n['only_b']}, same key different bytes {n['differing']}"
                       f"  -> {'EQUAL' if n['only_a'] == n['only_b'] == n['differing'] == 0 else 'DIFFERENT'}")
        fed = r.get("fed")
        if fed:
            out.append(f"  B-fed (A's store): {fed['found']} functions found, {fed['compiled']} compiled "
                       f"({fed['logged']} named), {fed.get('scripts_run', 0)} of {fed.get('scripts', 0)} scripts run, "
                       f"{fed.get('expanded', 0)} macros expanded")
            if fed.get("not_kept"):
                out.append(f"      not kept on any machine: {fed['not_kept']}")
            if fed.get("not_found"):
                out.append(f"      named, compiled for want of an entry: {fed['not_found']}")
        for d in r["differences"]:
            tag = "PENDING" if d["pending"] else "DIFFERS"
            out.append(f"  {tag}: [{d['kind']}] {d['key']} ({d['count']}): {d['why']}")
            if d["pending"]:
                out.append(f"      pending because {d['pending']}")
            for e in d["examples"]:
                out.append(f"      e.g. {e[:240]}")
    return "\n".join(out)


def failing(results):
    return [d for r in results for d in r["differences"] if not d["pending"]]


# --- self test -----------------------------------------------------------------------------------------

def self_test():
    """The comparison and the cause-finding on synthetic stores: no ra-jit, no cargo."""
    ok = True

    def check(what, good):
        nonlocal ok
        ok &= bool(good)
        print(f"  {what:70} {'right' if good else 'WRONG'}")

    h = lambda s: hashlib.sha256(s.encode()).hexdigest()
    id_a, id_b = h("crate a")[:16], h("crate b")[:16]
    same = lambda: {h(f"key {i}"): {(10 + i, h(f"value {i}"))} for i in range(5)}
    names = lambda crate: {h(f"key {i}"): (h(f"value {i}"), f"fn {crate}:src/lib.rs#{i}<u32>") for i in range(5)}

    n, d = compare_stores("t", same(), same(), names(id_a), names(id_a))
    check("equal stores are equal", n["functions"]["differing"] == 0 and n["functions"]["only_a"] == 0 and not d)

    b_digests = same()
    b_names = names(id_a)
    drop = h("key 2")
    del b_digests[drop], b_names[drop]
    n, d = compare_stores("t", same(), b_digests, names(id_a), b_names)
    check("a function only A made is named, not a key that differs", [x.kind for x in d] == ["function-alone"] and d[0].count == 1)

    renamed_digests = {h(f"B key {i}"): {(10 + i, h(f"value {i}"))} for i in range(5)}
    renamed_names = {h(f"B key {i}"): (h(f"value {i}"), f"fn {id_b}:src/lib.rs#{i}<u32>") for i in range(5)}
    n, d = compare_stores("t", same(), renamed_digests, names(id_a), renamed_names)
    check("the same functions under another crate id are one cause", [x.kind for x in d] == ["crate-id"] and d[0].count == 5)
    check("the cause names both ids and the file", id_a[:12] in d[0].key and id_b[:12] in d[0].key and "src/lib.rs" in d[0].key)

    value_digests = same()
    value_digests[h("key 1")] = {(11, h("another value"))}
    n, d = compare_stores("t", same(), value_digests, names(id_a), names(id_a))
    check("the same key with other bytes is a value difference", [x.kind for x in d] == ["value"] and n["functions"]["differing"] == 1)

    other_a = {h("frozen 1"): {(5, h("x"))}}
    n, d = compare_stores("t", {**same(), **other_a}, same(), names(id_a), names(id_a))
    check("a record that is not a function is counted apart", n["other"]["only_a"] == 1 and any(x.kind == "other-record" for x in d))

    text = f"{h('k')} 12 {h('v')}\n{h('k2')} 3 {h('v2')}\n"
    check("digest lines are read", set(read_digests(text)) == {h('k'), h('k2')})
    try:
        read_digests("not a line\n")
        check("a bad digest line is refused", False)
    except ValueError:
        check("a bad digest line is refused", True)
    log = f"{h('k')}\t{h('v')}\tfn {id_a}:src/a.rs#1<u32>\n"
    check("keys log lines are read", read_keys_log(log)[h("k")][1].endswith("<u32>"))
    check("a skeleton hides every crate id", skeleton(f"fn {id_a}:x#1<{id_b}:y#2>") == "fn #:x#1<#:y#2>")
    macro_log = (f"macro\tderive\tDescribe\t{h('d1')}\tdylib=aaaa\tinput=1111\tenv=e1\tcwd=c1\texpanded\n"
                 f"macro-env\te1\tCARGO_MANIFEST_DIR=$PACKAGE$\tCARGO_PKG_NAME=app\n")
    a_log = read_macro_log(macro_log)
    check("a macro log is read", a_log[0][0]["name"] == "Describe" and a_log[1]["e1"]["CARGO_PKG_NAME"] == "app")
    b_text = macro_log.replace("dylib=aaaa", "dylib=bbbb")
    d = compare_macros("t", a_log, read_macro_log(b_text))
    check("two macro calls that differ by the library only are the library's", [x.kind for x in d] == ["expansion-library"])
    b_text = macro_log.replace("env=e1", "env=e2") + "macro-env\te2\tCARGO_MANIFEST_DIR=/Users/b/x\tCARGO_PKG_NAME=app\n"
    d = compare_macros("t", a_log, read_macro_log(b_text))
    check("a variable that still holds a path is named", d and "CARGO_MANIFEST_DIR" in d[0].why and d[0].kind == "expansion")
    check("a macro line is not a function entry", read_keys_log(macro_log) == {})
    pending = Difference("t", "script-run", "k", "why")
    check("a script-run difference is pending, an expansion is not", pending.pending and not Difference("t", "expansion", "k", "w").pending)
    print("self-test:", "ok" if ok else "FAILED")
    return 0 if ok else 1


# --- main ----------------------------------------------------------------------------------------------

def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("fixtures", nargs="*")
    p.add_argument("--all", action="store_true")
    p.add_argument("--list", action="store_true")
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--blaze-bin", default=os.environ.get("RA_JIT_BIN_DIR") or DEFAULT_BIN,
                   help="the directory with ra-jit, cargo-ra-jit and blaze (default: %(default)s)")
    p.add_argument("--work", default=DEFAULT_WORK)
    p.add_argument("--online", action="store_true", help="let cargo fetch what the registry cache lacks")
    p.add_argument("--keep", action="store_true", help="leave the machines (checkouts, targets, stores) on disk")
    p.add_argument("--json", help="write the results here")
    p.add_argument("--reference-only", action="store_true",
                   help="build and run only with plain cargo on both machines: no daemon is loaded")
    p.add_argument("--remote", help="the server remote of unit C5; not implemented here")
    p.add_argument("--b-docker", action="store_true", help="machine B in a container of another libc: unit C5")
    p.add_argument("--watchdog-mb", type=int, default=24 * 1024)
    a = p.parse_args()
    if a.self_test:
        return self_test()
    if a.list:
        for n in ALL:
            print(f"{n:9} {FIXTURES[n]['about']}")
        return 0
    if a.remote or a.b_docker:
        sys.exit("--remote and --b-docker belong to unit C5 (a server remote, a machine of another libc); "
                 "crossmachine.py has a directory remote and two machines of this host")
    names = ALL if a.all else a.fixtures
    if not names:
        p.error("name a fixture, or --all")
    unknown = [n for n in names if n not in FIXTURES]
    if unknown:
        p.error(f"no fixture {unknown}: {sorted(FIXTURES)}")
    bin_dir = os.path.abspath(a.blaze_bin)
    for tool in ("ra-jit", "cargo-ra-jit"):
        if not os.path.exists(os.path.join(bin_dir, tool)):
            sys.exit(f"{bin_dir} has no {tool}: build it (under the heavy lock), or name the directory with --blaze-bin")
    os.environ["RA_JIT_BIN"] = os.path.join(bin_dir, "ra-jit")
    work = os.path.realpath(a.work)
    run_id = "xm%s-%d" % (time.strftime("%m%d%H%M%S"), os.getpid())
    os.environ["RA_JIT_RUN_ID"] = run_id
    # Every run starts clean; the seed cargo home is the one thing kept, since it costs a download.
    if os.path.isdir(work):
        for entry in os.listdir(work):
            if entry != "seed-cargo-home":
                shutil.rmtree(os.path.join(work, entry), ignore_errors=True)
    os.makedirs(work, exist_ok=True)

    from tracedecay import Watchdog
    watchdog = Watchdog(bin_dir, limit_mb=a.watchdog_mb, run_id=run_id, homes=(),
                        series=os.path.join(work, "watchdog-series.jsonl"))
    watchdog.start()

    def progress(line):
        print(line, flush=True)

    results = []
    try:
        sources = {n: source_of(n, work) for n in names}
        seed = seed_cargo_home(work, names, sources, a.online)
        rust_home = os.environ.get("RUSTUP_HOME") or os.path.expanduser("~/.rustup")
        a_m = Machine(os.path.join(work, "m"), "a", ["alice", "src", "co"], seed, rust_home)
        b_m = Machine(os.path.join(work, "m"), "b", ["builder-2", "x"], seed, rust_home)
        progress(f"machine A: home {a_m.home}\n           checkouts {a_m.checkouts}\n"
                 f"machine B: home {b_m.home}\n           checkouts {b_m.checkouts}")
        if a.reference_only:
            return 1 if reference_only(names, work, bin_dir, (a_m, b_m), sources, progress) else 0
        for n in names:
            try:
                results.append(fixture_report(n, a, work, bin_dir, (a_m, b_m), sources, progress))
            except Exception as e:  # one fixture's break is a result, and the others still run
                import traceback
                traceback.print_exc()
                results.append(dict(fixture=n, about=FIXTURES[n]["about"], runs={}, numbers={}, differences=[
                    Difference(n, "harness", "the fixture's run", f"{type(e).__name__}: {e}").row()]))
            if watchdog.tripped:
                break
    finally:
        watchdog.stop.set()
    print()
    print(render(results))
    if watchdog.tripped:
        print("WATCHDOG killed the run:", watchdog.tripped["reason"])
    summary = dict(results=results, watchdog=watchdog.tripped, bin=bin_dir,
                   built_from=(open(os.path.join(bin_dir, "BUILT_FROM")).read().strip()
                               if os.path.exists(os.path.join(bin_dir, "BUILT_FROM")) else None))
    with open(a.json or os.path.join(work, "crossmachine.json"), "w") as f:
        json.dump(summary, f, indent=1, sort_keys=True)
    bad = failing(results)
    pending = [d for r in results for d in r["differences"] if d["pending"]]
    print(f"\n{len(results)} fixtures: {len(bad)} differences fail, {len(pending)} are pending")
    if not a.keep:
        # The machines are most of the disk; the reports, the logs and the JSON stay.
        shutil.rmtree(os.path.join(work, "m"), ignore_errors=True)
    return 1 if bad or watchdog.tripped else 0


if __name__ == "__main__":
    sys.exit(main())
