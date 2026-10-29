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
import argparse, base64, hashlib, json, os, re, secrets, shutil, stat, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
ROOT = os.path.dirname(HARNESS)
for path in (HARNESS, HERE):
    if path not in sys.path:
        sys.path.insert(0, path)

# Where the unit's binaries are kept: the builds directory of the shared Mac (it moved from ~/dev to ~/code
# on 2026-10-26), else a release build of this tree.
DEFAULT_BIN = next((it for it in ("/Users/bytedance/code/blaze-targets/c4-keys/bin",) if os.path.isdir(it)),
                   os.path.join(ROOT, "target", "release"))
DEFAULT_WORK = "/tmp/c4xm"
DAEMON_ROOT = None


def daemon_home(fixture, label):
    """Daemon sockets have a separate, owned short root independent of --work."""
    path = os.path.join(DAEMON_ROOT, fixture[:4] + "-" + label)
    socket = os.path.join(path, "workers", "f" * 16, "sock.new", "s")
    if len(socket.encode()) >= 104:
        raise RuntimeError(f"daemon socket exceeds platform budget: {socket}")
    return path
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
    "vendor-edit": dict(source="dir", workspace="vendored", package="app", run=[], vendor_edit=True,
                        about="negative invalidation: B changes a vendored function body and checksum"),
    "vendor-payload": dict(source="dir", workspace="vendored", package="app", run=[], payload_edit=True,
                           about="negative invalidation: B changes only an included binary payload"),
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
ALL = ["hello", "paths", "vendored", "vendor-edit", "vendor-payload", "macros", "ripgrep"]

# A difference whose fix is another unit's, or that waits on a thing no unit here owns. A kind is
# the `kind` of a Difference; the value is why it is not failing this run. Nothing else passes.
PENDING = {
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
                    bits.append("environment " + ", ".join(f"{n} (value hash {ours.get(n)!r} on A, {theirs.get(n)!r} on B)" for n in names[:4]))
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
            if different:
                diffs.append(Difference(fixture, "other-value", "non-function records with unequal bytes",
                                        "the same key holds different record bytes on the two machines",
                                        len(different), sorted(different)))
            continue
        if a_k != b_k:
            diffs.append(Difference(fixture, "function-key-set", "unequal function disk keys",
                                    "key-set inequality fails even when stable names and crate ids agree",
                                    len(a_k ^ b_k), sorted(a_k ^ b_k)))
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
        # The watchdog selects this run by id even though each machine has its own home.
        if "RA_JIT_RUN_ID" in os.environ:
            env["RA_JIT_RUN_ID"] = os.environ["RA_JIT_RUN_ID"]
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
        config = None
        if spec["workspace"] == "vendored":
            dest = os.path.join(work, "source-" + name)
            shutil.copytree(sources[name], dest, ignore=shutil.ignore_patterns("vendor", "target"))
            sources[name] = dest
            ws = os.path.join(dest, spec["workspace"])
            config = os.path.join(ws, ".cargo", "config.toml")
            os.rename(config, config + ".saved")
        try:
            ran = sh(["cargo", "fetch", "--locked"], cwd=ws, env=env)
            if ran.returncode:
                raise RuntimeError(f"{name}: cargo fetch --locked in {ws} failed (use --online to download):\n{ran.stdout[-1500:]}")
            if config:
                # Generate ignored vendor/ in scratch space, never in the lane's checkout.
                ran = sh(["cargo", "vendor", "--locked", "--offline", "vendor"], cwd=ws, env=env)
                if ran.returncode:
                    raise RuntimeError(f"cargo vendor failed: {ran.stdout[-1500:]}")
                vendor_body(ws, 1, append=True)
                vendor_payload(ws, 1)
        finally:
            if config:
                os.rename(config + ".saved", config)
    return seed


def vendor_body(ws, value, append=False):
    """A valid source replacement edit: update body and cargo's checksum manifest."""
    vendor = os.path.join(ws, "vendor", "fastrand")
    source = os.path.join(vendor, "src", "lib.rs")
    text = open(source).read()
    if append:
        text += '\npub fn c4_identity() -> u32 { 1 }\npub fn c4_payload() -> u8 { include_bytes!("payload.bin")[0] }\npub fn c4_text_payload() -> u8 { include_str!("payload.bin").as_bytes()[0] }\n'
    else:
        old = "pub fn c4_identity() -> u32 { 1 }"
        if old not in text:
            raise RuntimeError("negative fixture has no original vendor body")
        text = text.replace(old, f"pub fn c4_identity() -> u32 {{ {value} }}")
    with open(source, "w") as output:
        output.write(text)
    checksum = os.path.join(vendor, ".cargo-checksum.json")
    checks = json.load(open(checksum))
    checks["files"]["src/lib.rs"] = hashlib.sha256(text.encode()).hexdigest()
    with open(checksum, "w") as output:
        json.dump(checks, output, sort_keys=True)


def vendor_payload(ws, value):
    vendor = os.path.join(ws, "vendor", "fastrand")
    path = os.path.join(vendor, "src", "payload.bin")
    with open(path, "wb") as output:
        output.write(bytes([value]))
    checksum = os.path.join(vendor, ".cargo-checksum.json")
    checks = json.load(open(checksum))
    checks["files"]["src/payload.bin"] = hashlib.sha256(bytes([value])).hexdigest()
    with open(checksum, "w") as output:
        json.dump(checks, output, sort_keys=True)


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


def warm_payload_report(work, bin_dir, machines, sources):
    """Same daemon, binary-only mutation/delete/recreate, repeated after relocation."""
    from cargo_ra_jit import Side, program_output, wait_warm, how_built
    spec = dict(FIXTURES["vendor-payload"])
    rows = []
    for machine in machines:
        ws = os.path.join(machine.checkout("warm-payload", sources["vendor-payload"]), "vendored")
        env = machine.env(bin_dir, os.path.join(machine.top, "warm-store"),
                          daemon_home("warm", machine.name))
        side = Side(ws, env, True)
        previous_pids, previous_builds = None, -1
        try:
            for step, value in [("initial", 1), ("binary-edit", 2), ("deleted", None),
                                ("recreated", 3), ("remove-reader", 3), ("obsolete-deleted", None),
                                ("reintroduce-missing", None), ("live-recreated", 4)]:
                source = os.path.join(ws, "vendor", "fastrand", "src", "lib.rs")
                checksum = os.path.join(ws, "vendor", "fastrand", ".cargo-checksum.json")
                if step in ("remove-reader", "reintroduce-missing"):
                    text = open(source).read()
                    if step == "remove-reader":
                        text = text.replace('include_bytes!("payload.bin")[0]', "42")
                        text = text.replace('include_str!("payload.bin").as_bytes()[0]', "42")
                    else:
                        text = text.replace("pub fn c4_payload() -> u8 { 42 }",
                                            'pub fn c4_payload() -> u8 { include_bytes!("payload.bin")[0] }')
                        text = text.replace("pub fn c4_text_payload() -> u8 { 42 }",
                                            'pub fn c4_text_payload() -> u8 { include_str!("payload.bin").as_bytes()[0] }')
                    with open(source, "w") as output:
                        output.write(text)
                    checks = json.load(open(checksum))
                    checks["files"]["src/lib.rs"] = hashlib.sha256(text.encode()).hexdigest()
                    with open(checksum, "w") as output:
                        json.dump(checks, output, sort_keys=True)
                if value is None:
                    payload = os.path.join(ws, "vendor", "fastrand", "src", "payload.bin")
                    if os.path.exists(payload):
                        os.remove(payload)
                    checks = json.load(open(checksum))
                    checks["files"].pop("src/payload.bin", None)
                    with open(checksum, "w") as output:
                        json.dump(checks, output, sort_keys=True)
                else:
                    vendor_payload(ws, value)
                # No Rust source changes and no stop/restart between these builds.
                _, built = side.cargo("build", "-p", "app", timeout=1800)
                _, ready = wait_warm(side, ws)
                status = side.cargo("status")[1].stdout
                missing_live = step in ("deleted", "reintroduce-missing")
                if missing_live:
                    shutil.rmtree(os.path.join(ws, "target-ref"), ignore_errors=True)
                    oracle = sh(["cargo", "build", "--offline", "--locked", "-p", "app"],
                                cwd=ws, env=dict(env, CARGO_TARGET_DIR=os.path.join(ws, "target-ref")))
                    actual, expected = None, None
                    right = missing_input_failure(built.returncode, built.stderr,
                                                  oracle.returncode, oracle.stdout, payload)
                    oracle_code = oracle.returncode
                    oracle_log = oracle.stdout
                else:
                    # Cargo also treats registry source replacement as immutable. A
                    # fresh oracle target is required for each binary-only edit.
                    shutil.rmtree(os.path.join(ws, "target-ref"), ignore_errors=True)
                    expected = reference(machine, bin_dir, ws, spec, env["RA_JIT_CACHE"], env["RA_JIT_HOME"])
                    actual = program_output(side.cargo("run", "-p", "app", "--", timeout=600)[1])
                    wanted = 42 if step in ("remove-reader", "obsolete-deleted") else value
                    right = (built.returncode == 0 and actual == expected
                             and f"vendor payload {wanted} caller {wanted + 10}" in expected[1]
                             and f"text payload {wanted}" in expected[1])
                    oracle_code = expected[0]
                    oracle_log = expected[2]
                pids = re.findall(r"(?:daemon:|worker:) pid (\d+)", status)
                builds = sum(int(n) for n in re.findall(r"(\d+) builds by the daemon", status))
                same_generation = valid_worker_generation(status, ready, pids, previous_pids,
                                                          builds, previous_builds)
                previous_pids, previous_builds = pids, builds
                right &= same_generation
                right &= not recognized_fallback(built.stderr) and not any(
                    int(n) for n in re.findall(r"(\d+) by cargo", status))
                rows.append(dict(machine=machine.name, step=step, right=right,
                                 actual=actual, expected=expected, status=status,
                                 classification=how_built(built.stderr), build_log=built.stderr,
                                 worker_log=workers_log(side), blaze_returncode=built.returncode,
                                 cargo_returncode=oracle_code, pids=pids, daemon_builds=builds,
                                 same_generation=same_generation, ready=ready, cargo_log=oracle_log))
        finally:
            side.cargo("stop")
    return rows


def recognized_fallback(text):
    return bool(re.search(r"building with cargo|this build is cargo's|this is `cargo|"
                          r"nothing has changed since cargo built|^\s*Compiling ", text, re.M))

def valid_macro_mutation(step, same_size, same_mtime, before, after,
                         old_dylibs, dylibs, old_reloads, reloads):
    mutation = step in ("same-size-restored-mtime", "dependency-restored-mtime") or step.startswith("reload-")
    if mutation:
        return (same_size and same_mtime and before != after and old_dylibs != dylibs
                and reloads > old_reloads)
    if step == "unchanged-content":
        return before == after and old_dylibs == dylibs and reloads == old_reloads
    return reloads == old_reloads

def token_scan(roots, token):
    """Scan while the daemon home still exists. Results never contain the token.

    Native load framing is hex CBOR. Also catch plain, JSON-escaped, UTF-16
    and base64 at every byte alignment. Overlapping roots count files once.
    """
    raw = token.encode()
    forms = {"plain": raw, "hex": raw.hex().encode(), "HEX": raw.hex().upper().encode(),
             "utf16le": token.encode("utf-16-le"), "utf16be": token.encode("utf-16-be"),
             "json": json.dumps(token)[1:-1].encode()}
    for index in range(3):
        encoded = base64.b64encode(b"\0" * index + raw)
        forms[f"base64-{index}"] = encoded[4 if index else 0:][:(len(raw) * 4) // 3 - 4]
    hits, errors, seen, directories = [], [], set(), set()
    for root in roots:
        if not os.path.isdir(root):
            errors.append(root)
            continue
        for directory, children, names in os.walk(root, followlinks=True,
                onerror=lambda error: errors.append(error.filename)):
            resolved = os.path.realpath(directory)
            if resolved in directories:
                children[:] = []
                continue
            directories.add(resolved)
            for name in names:
                path = os.path.realpath(os.path.join(directory, name))
                if path in seen:
                    continue
                try:
                    if not stat.S_ISREG(os.stat(path).st_mode):
                        continue
                except OSError:
                    errors.append(path)
                    continue
                seen.add(path)
                try:
                    with open(path, "rb") as source:
                        tail = b""
                        found = set()
                        while True:
                            block = source.read(1024 * 1024)
                            if not block:
                                break
                            data = tail + block
                            found.update(form for form, needle in forms.items() if needle in data)
                            tail = data[-max(map(len, forms.values())):]
                    hits.extend(dict(path=path, encoding=form) for form in sorted(found))
                except OSError:
                    errors.append(path)
    return dict(scanned=len(seen), hits=hits, errors=errors, roots=roots)

def macro_content_report(work, bin_dir, machines, sources, reloads=0):
    from cargo_ra_jit import Side, program_output, wait_warm
    import norustc
    machine = machines[0]
    ws = os.path.join(machine.checkout("macro-content", sources["macros"]), "macros")
    source = os.path.join(ws, "derive/src/lib.rs")
    producer = os.path.join(ws, "producer/src/lib.rs")
    text = open(source).read().replace("{name} was described", "{name} was described ONE")
    with open(source, "w") as output:
        output.write(text)
    env = machine.env(bin_dir, os.path.join(machine.top, "macro-store"), daemon_home("pm", "a"),
                      FIXTURES["macros"].get("env"))
    env.pop("RA_JIT_PROC_MACRO_CACHE_SKIP", None)
    token = "c4-witness-" + secrets.token_hex(24)
    env["C4_PLANTED_TOKEN"] = token
    audit_log = os.path.join(work, "product-tools.log")
    audit_bin = norustc.stand_ins(os.path.join(work, "product-audit"), audit_log, block=True, block_macro_server=True)
    env["PATH"] = os.path.join(audit_bin, "bin") + os.pathsep + env["PATH"]
    for variable, tool in (("RUSTC", "rustc"), ("RUSTDOC", "rustdoc"), ("CARGO", "cargo")):
        env[variable] = os.path.join(audit_bin, "bin", tool)
    # The subject is the product CLI, not Cargo's subcommand launcher. The audit
    # remains around the entire request, including every process the product starts.
    traces = os.path.join(work, "product-execs")
    side = Side(ws, env, True, product_cli=os.path.join(bin_dir, "ra-jit"), exec_trace_dir=traces)
    def exec_calls():
        return [tool for name in os.listdir(traces) if name.endswith(".log")
                for tool in norustc.external_execs(open(os.path.join(traces, name)).read())]
    rows = []
    previous_pids, previous_builds = None, -1
    previous_dylibs = None
    previous_reload_count = 0
    steps = ["initial", "same-size-restored-mtime", "dependency-restored-mtime", "unchanged-content"]
    steps.extend(f"reload-{index:02}" for index in range(reloads))
    try:
        for step in steps:
            audit_before = len(norustc.runs(audit_log))
            edited_source = producer if step == "dependency-restored-mtime" else source
            before = os.stat(edited_source)
            old_digest = hashlib.sha256(open(edited_source, "rb").read()).hexdigest()
            if step == "same-size-restored-mtime":
                text = text.replace("described ONE", "described TWO")
                with open(source, "w") as output:
                    output.write(text)
                os.utime(source, ns=(before.st_atime_ns, before.st_mtime_ns))
            elif step == "dependency-restored-mtime":
                replacement = open(producer).read().replace('"base"', '"edit"')
                with open(producer, "w") as output:
                    output.write(replacement)
                os.utime(producer, ns=(before.st_atime_ns, before.st_mtime_ns))
            elif step.startswith("reload-"):
                replacement = f"described {int(step[-2:]):03}"
                text = re.sub(r"described (?:TWO|\d{3})", replacement, text)
                with open(source, "w") as output:
                    output.write(text)
                os.utime(source, ns=(before.st_atime_ns, before.st_mtime_ns))
            _, built = side.cargo("build", "-p", "app")
            _, ready = wait_warm(side, ws)
            status = side.cargo("status")[1].stdout
            actual = program_output(side.cargo("run", "-p", "app")[1])
            shutil.rmtree(os.path.join(ws, "target-ref"), ignore_errors=True)
            expected = reference(machine, bin_dir, ws, FIXTURES["macros"], env["RA_JIT_CACHE"], env["RA_JIT_HOME"])
            pids = re.findall(r"(?:daemon:|worker:) pid (\d+)", status)
            builds = sum(int(n) for n in re.findall(r"(\d+) builds by the daemon", status))
            generation = valid_worker_generation(status, ready, pids, previous_pids, builds, previous_builds)
            previous_pids, previous_builds = pids, builds
            dylibs = {os.path.relpath(os.path.join(root, name), ws):
                      hashlib.sha256(open(os.path.join(root, name), "rb").read()).hexdigest()
                      for root, _, files in os.walk(os.path.join(ws, "target"))
                      for name in files if "derive" in name and name.endswith((".dylib", ".so", ".dll"))}
            new_digest = hashlib.sha256(open(edited_source, "rb").read()).hexdigest()
            same_size = os.stat(edited_source).st_size == before.st_size
            same_mtime = os.stat(edited_source).st_mtime_ns == before.st_mtime_ns
            mutation = step in ("same-size-restored-mtime", "dependency-restored-mtime") or step.startswith("reload-")
            worker = workers_log(side)
            reload_count = len(re.findall(r"(?m)^reload:", worker))
            reload_valid = (reload_count > previous_reload_count if mutation
                            else reload_count == previous_reload_count)
            mutation_valid = valid_macro_mutation(step, same_size, same_mtime, old_digest, new_digest,
                                                  previous_dylibs, dylibs,
                                                  previous_reload_count, reload_count)
            previous_dylibs = dylibs
            previous_reload_count = reload_count
            retention = {}
            if reloads:
                from tracedecay import ps_table, descendants, footprints_mb
                processes = ps_table(env=False)
                own = descendants(processes, [int(pid) for pid in pids])
                servers = [pid for pid, _, _, command in processes
                           if pid in own and "proc-macro-srv" in command]
                retention = dict(server_pids=servers, server_footprint_mb=footprints_mb(servers),
                                 owned_snapshots=[os.path.relpath(os.path.join(root, name), env["RA_JIT_HOME"])
                                                  for root, _, files in os.walk(os.path.join(env["RA_JIT_HOME"], "macro-snapshots"))
                                                  for name in files if name.startswith("generation-")],
                                 producer_directories=[name for name in os.listdir(os.path.join(ws, "target/rust-analyzer/producer-inputs"))
                                                       if os.path.isdir(os.path.join(ws, "target/rust-analyzer/producer-inputs", name))]
                                 if os.path.isdir(os.path.join(ws, "target/rust-analyzer/producer-inputs")) else [])
            rows.append(dict(step=step, right=built.returncode == 0 and actual == expected and generation and mutation_valid and reload_valid and not recognized_fallback(built.stderr),
                             actual=actual, expected=expected, build_log=built.stderr, status=status, pids=pids,
                             worker_log=worker, old_sha256=old_digest, reload_count=reload_count,
                             reload_valid=reload_valid, blaze_returncode=built.returncode,
                             cargo_returncode=expected[0],
                             source_sha256=new_digest,
                             edited_source=os.path.relpath(edited_source, ws),
                             same_size=same_size, same_mtime=same_mtime, mutation_valid=mutation_valid,
                             retention=retention,
                             product_tool_calls=norustc.runs(audit_log)[audit_before:],
                             external_execs=exec_calls(),
                             dylibs=dylibs))
            # This must precede stop/cleanup, especially DAEMON_ROOT removal in main.
            scan = token_scan([work, env["RA_JIT_HOME"], env["RA_JIT_CACHE"]], token)
            rows[-1]["token_scan"] = scan
            rows[-1]["right"] &= not scan["hits"] and not scan["errors"]
    finally:
        scan = token_scan([work, env["RA_JIT_HOME"], env["RA_JIT_CACHE"]], token)
        with open(os.path.join(work, "token-scan-before-stop.json"), "w") as output:
            json.dump(scan, output, indent=1)
        side.cargo("stop")
        if rows:
            rows[-1]["product_tool_calls"] = norustc.runs(audit_log)
            rows[-1]["external_execs"] = exec_calls()
        scan = token_scan([work, env["RA_JIT_HOME"], env["RA_JIT_CACHE"]], token)
        with open(os.path.join(work, "token-scan-after-stop.json"), "w") as output:
            json.dump(scan, output, indent=1)
        if rows:
            rows[-1]["right"] &= not scan["hits"] and not scan["errors"]
        with open(os.path.join(work, "snapshot-cleanup.json"), "w") as output:
            json.dump(dict(owned_files=[os.path.relpath(os.path.join(root, name), env["RA_JIT_HOME"])
                                       for root, _, files in os.walk(os.path.join(env["RA_JIT_HOME"], "macro-snapshots"))
                                       for name in files]), output)
    return rows

def nonfunction_report(work, bin_dir, machines, sources):
    """Drive newly introduced const/discriminant includes through actual requests."""
    from cargo_ra_jit import Side, program_output, wait_warm
    rows = []
    for machine in machines:
        ws = os.path.join(machine.checkout("nonfunction", sources["vendor-payload"]), "vendored")
        source = os.path.join(ws, "vendor/fastrand/src/lib.rs")
        original = open(source).read()
        payload = os.path.join(ws, "vendor/fastrand/src/absent.bin")
        env = machine.env(bin_dir, os.path.join(machine.top, "nonfunction-store"), daemon_home("nonf", machine.name))
        side = Side(ws, env, True)
        old_pids, old_builds = None, -1
        try:
            for step, declaration, value, fail in [
                ("initial", "const X: &[u8] = &[1];", None, False),
                ("const-missing", 'const X: &[u8] = include_bytes!("absent.bin");', None, True),
                ("const-created", 'const X: &[u8] = include_bytes!("absent.bin");', 2, False),
                ("reader-removed", "const X: &[u8] = &[42];", 2, False),
                ("obsolete-deleted", "const X: &[u8] = &[42];", None, False),
                ("cfg-disabled", '#[cfg(any())] const UNUSED: &[u8] = include_bytes!("absent.bin"); const X: &[u8] = &[42];', None, False),
                ("enum-missing", '#[repr(u8)] enum E { X = include_bytes!("absent.bin")[0] } const X: &[u8] = &[E::X as u8];', None, True),
                ("enum-created", '#[repr(u8)] enum E { X = include_bytes!("absent.bin")[0] } const X: &[u8] = &[E::X as u8];', 3, False),
            ]:
                text = original.replace('pub fn c4_payload() -> u8 { include_bytes!("payload.bin")[0] }',
                                        declaration + '\npub fn c4_payload() -> u8 { X[0] }')
                if value is None:
                    if os.path.exists(payload):
                        os.remove(payload)
                else:
                    with open(payload, "wb") as output:
                        output.write(bytes([value]))
                with open(source, "w") as output:
                    output.write(text)
                checksum = os.path.join(ws, "vendor/fastrand/.cargo-checksum.json")
                checks = json.load(open(checksum))
                checks["files"]["src/lib.rs"] = hashlib.sha256(text.encode()).hexdigest()
                checks["files"].pop("src/absent.bin", None)
                if value is not None:
                    checks["files"]["src/absent.bin"] = hashlib.sha256(bytes([value])).hexdigest()
                with open(checksum, "w") as output:
                    json.dump(checks, output)
                _, built = side.cargo("build", "-p", "app")
                _, ready = wait_warm(side, ws)
                status = side.cargo("status")[1].stdout
                shutil.rmtree(os.path.join(ws, "target-ref"), ignore_errors=True)
                oracle = sh(["cargo", "build", "--locked", "--offline", "-p", "app"],
                            cwd=ws, env=dict(env, CARGO_TARGET_DIR=os.path.join(ws, "target-ref")))
                actual = expected = None
                if fail:
                    semantic = missing_input_failure(built.returncode, built.stderr, oracle.returncode, oracle.stdout, payload)
                else:
                    actual = program_output(side.cargo("run", "-p", "app")[1])
                    reference_env = dict(env, CARGO_TARGET_DIR=os.path.join(ws, "target-ref"))
                    expected = program_output(sh(["cargo", "run", "--locked", "--offline", "-q", "-p", "app"], cwd=ws,
                                                 env=reference_env, stderr=subprocess.PIPE))
                    semantic = built.returncode == oracle.returncode == 0 and actual == expected
                pids = re.findall(r"(?:daemon:|worker:) pid (\d+)", status)
                builds = sum(int(n) for n in re.findall(r"(\d+) builds by the daemon", status))
                generation = valid_worker_generation(status, ready, pids, old_pids, builds, old_builds)
                old_pids, old_builds = pids, builds
                rows.append(dict(machine=machine.name, step=step, semantic=semantic, right=semantic and generation and not recognized_fallback(built.stderr),
                                 blaze_returncode=built.returncode, cargo_returncode=oracle.returncode, actual=actual,
                                 expected=expected, build_log=built.stderr, cargo_log=oracle.stdout, worker_log=workers_log(side),
                                 pids=pids, status=status, ready=ready, daemon_builds=builds))
        finally:
            side.cargo("stop")
    return rows


def valid_worker_generation(status, ready, pids, previous_pids, builds, previous_builds):
    return (bool(pids) and "failed:" not in status and ": ready" in status
            and ready and all(state == "ready" for state in ready)
            and (previous_pids is None or pids == previous_pids) and builds >= previous_builds
            and not any(int(n) for n in re.findall(r"(\d+) by cargo", status)))


def missing_input_failure(blaze_code, blaze_log, cargo_code, cargo_log, payload):
    name = os.path.basename(payload)
    return (blaze_code != 0 and cargo_code != 0 and not recognized_fallback(blaze_log)
            and "could not read included input" in blaze_log and payload in blaze_log
            and name in cargo_log and bool(re.search(r"couldn't read|could not read|No such file", cargo_log)))


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


def fed_coverage(fixture, said, names, log_exists):
    """Fail closed: missing logs and unnamed work do not establish zero compilation."""
    diffs = []
    totals = counts(said)
    entries = entries_line(said)
    if not log_exists:
        diffs.append(Difference(fixture, "fed-coverage", "missing B-fed key log",
                                "zero named additions is not evidence of zero compilation"))
    store_lines = re.findall(r"^store:.*$", said, re.M)
    # `entries: 1.2µs` is a phase timing, not an entry-count record.
    entry_lines = [line for line in re.findall(r"^entries:.*$", said, re.M)
                   if not re.fullmatch(r"entries: [\d.]+(?:ns|µs|ms|s)", line)]
    stats_lines = re.findall(r"^stats:.*$", said, re.M)
    stats_pattern = r"stats: (\d+) of \d+ functions compiled, \d+ reused"
    store_pattern = r"store: \d+ functions found, \d+ compiled and added"
    entry_pattern = (r"entries: \d+ found, \d+ added, \d+ as stored, \d+ not as stored, "
                     r"\d+ instances and \d+ requests without a stable name")
    if (not (store_lines or stats_lines) or not entry_lines
            or any(not re.fullmatch(store_pattern, line) for line in store_lines)
            or any(not re.fullmatch(stats_pattern, line) for line in stats_lines)
            or any(not re.fullmatch(entry_pattern, line) for line in entry_lines)):
        diffs.append(Difference(fixture, "fed-coverage", "missing compute counters",
                                "complete entry and store counters are required"))
    if any(int(match[1]) for line in stats_lines
           if (match := re.fullmatch(stats_pattern, line))):
        diffs.append(Difference(fixture, "fed-compute", "stats report compilation",
                                "the compiler statistics report new work"))
    if totals["compiled"] or entries["added"] or entries["unnamed"] or entries["unnamed_requests"]:
        diffs.append(Difference(fixture, "fed-compute", "B-fed performed or omitted compute",
                                f"compiled={totals['compiled']}, added={entries['added']}, "
                                f"unnamed={entries['unnamed']}, unnamed_requests={entries['unnamed_requests']}"))
    if names and not entries["added"]:
        diffs.append(Difference(fixture, "fed-coverage", "logs contradict counters",
                                "named additions exist without a matching added count"))
    return diffs


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
    from cargo_ra_jit import Side, program_output, wait_warm, how_built
    run = Run(label)
    env = machine.env(bin_dir, store, ra_home, dict(extra_env, RA_JIT_KEYS_LOG=keys_log))
    # An empty *requested* log is valid zero-addition evidence only with complete
    # counters; an absent log is still rejected by fed_coverage.
    open(keys_log, "a").close()
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
        if how_built(built.stderr) != "daemon":
            run.error = f"build was not compiled by blaze: {built.stderr[-1500:]}"
            return run
        _, states = wait_warm(side, ws)
        if states == ["not loaded in time"] or any(s.startswith("left to cargo:") for s in states):
            run.error = f"daemon did not load the workspace: {states}"
            return run
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
    if spec.get("vendor_edit"):
        vendor_body(wss["b"], 2)
    if spec.get("payload_edit"):
        vendor_payload(wss["b"], 2)
    negative = spec.get("vendor_edit") or spec.get("payload_edit")
    stores = {"a": os.path.join(a.top, "store-" + name), "b-solo": os.path.join(b.top, "store-solo-" + name),
              "b-fed": os.path.join(b.top, "store-fed-" + name)}
    # Short: a daemon's socket lives below its directory, and a socket path has 104 bytes on macOS.
    homes = {k: daemon_home(name, k) for k in stores}
    for k in stores:
        os.makedirs(stores[k], exist_ok=True)

    progress(f"  {name}: reference builds with cargo")
    expected = {m.name: reference(m, bin_dir, wss[m.name], spec, stores["a"], homes["a"]) for m in (a, b)}

    progress(f"  {name}: A")
    runs["a"] = build_and_run(a, bin_dir, wss["a"], spec, "A", stores["a"], homes["a"], os.path.join(top, "keys-a.log"), extra, progress)
    runs["a"].expected = expected["a"]
    env_a = a.env(bin_dir, stores["a"], homes["a"])
    pushed = sh([os.path.join(bin_dir, "ra-jit"), "cache", "push", remote], env=env_a)
    if pushed.returncode:
        raise RuntimeError(f"cache push failed: {pushed.stdout[-1500:]}")
    result["pushed"] = pushed.stdout.strip()[-200:]

    progress(f"  {name}: B (own store, no remote)")
    runs["b-solo"] = build_and_run(b, bin_dir, wss["b"], spec, "B-solo", stores["b-solo"], homes["b-solo"],
                                   os.path.join(top, "keys-b-solo.log"), extra, progress)
    runs["b-solo"].expected = expected["b"]

    progress(f"  {name}: B (store pulled from A's remote)")
    env_b = b.env(bin_dir, stores["b-fed"], homes["b-fed"])
    pulled = sh([os.path.join(bin_dir, "ra-jit"), "cache", "pull", remote], env=env_b)
    if pulled.returncode:
        raise RuntimeError(f"cache pull failed: {pulled.stdout[-1500:]}")
    result["pulled"] = pulled.stdout.strip()[-200:]
    runs["b-fed"] = build_and_run(b, bin_dir, wss["b"], spec, "B-fed", stores["b-fed"], homes["b-fed"],
                                  os.path.join(top, "keys-b-fed.log"), dict(extra, RA_JIT_REMOTE=remote, RA_JIT_REMOTE_READONLY="1"),
                                  progress)
    runs["b-fed"].expected = expected["b"]

    for k, r in runs.items():
        with open(os.path.join(top, f"said-{k}.txt"), "w") as f:
            f.write(r.said)
        result["runs"][k] = dict(right=r.right, error=r.error, actual=r.out, expected=r.expected,
                                seconds=round(r.seconds, 1), **counts(r.said))
        if r.error is None and not r.right:
            result["differences"].append(Difference(name, "program", r.label, program_diff(r)).row())
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
    if not a_names or not solo_names:
        diffs.append(Difference(name, "no-function-evidence", "empty function key log",
                                "equal empty stores do not prove cross-machine key reuse"))
    result["numbers"] = numbers
    if negative:
        # Different source inputs should produce different keys. Correctness is checked
        # against B's changed oracle on the fed leg, not equal A/B digest sets.
        diffs = [d for d in diffs if d.kind not in
                 ("function-key-set", "crate-id", "other-record", "function-alone")]
        if d_a == d_solo:
            diffs.append(Difference(name, "vendor-invalidation", "unchanged digests after vendor edit",
                                    "B's changed function body must invalidate cached code"))
    for d in diffs:
        result["differences"].append(d.row())

    # What B compiled with A's entries in its store: each is a function A's key did not give it.
    fed = runs["b-fed"]
    c = counts(fed.said)
    coverage = fed_coverage(name, fed.said, fed_names, os.path.exists(os.path.join(top, "keys-b-fed.log")))
    for d in coverage:
        if negative and d.kind == "fed-compute":
            continue  # Recompilation is required for this negative invalidation arm.
        result["differences"].append(d.row())
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
        if negative:
            continue  # Invalidated names are expected; B must print B's oracle output.
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
            store, home = os.path.join(m.top, "store-ref-" + name), daemon_home(name, "ref" + m.name)
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


def strict_macro_rows(rows):
    """Every row must be right and have no product tool calls. Queries count too."""
    return bool(rows) and all(row["right"] and not row["product_tool_calls"]
                              and not row.get("external_execs") for row in rows)


# --- self test -----------------------------------------------------------------------------------------

def self_test():
    """The comparison and the cause-finding on synthetic stores: no ra-jit, no cargo."""
    ok = True

    def check(what, good):
        nonlocal ok
        ok &= bool(good)
        print(f"  {what:70} {'right' if good else 'WRONG'}")

    from cargo_ra_jit import Side
    from unittest import mock
    env = {"RA_JIT_HOME": "/home/daemon", "CARGO": "/audit/cargo", "RUSTC": "/audit/rustc",
           "PATH": "/audit:/product"}
    direct = Side("/checkout", env, True, product_cli="/product/ra-jit")
    for command, args in [("build", ("-p", "app")), ("status", ()),
                          ("run", ("-p", "app", "--", "argument")), ("stop", ())]:
        check(f"direct {command} keeps the product request",
              direct.argv(command, *args) == ["/product/ra-jit", "cargo", command, *args])
    with mock.patch("cargo_ra_jit.timed", return_value=(0, None)) as timed:
        direct.cargo("build", "-p", "app", timeout=123)
        check("direct dispatch preserves the audited environment and timeout",
              timed.call_args.args == (["/product/ra-jit", "cargo", "build", "-p", "app"],
                                       "/checkout", env, 123))
    check("Cargo launcher coverage elsewhere stays unchanged",
          Side("/checkout", env, True).argv("build", "-p", "app")
          == ["cargo", "ra-jit", "build", "-p", "app"])
    check("reference builds still use Cargo",
          Side("/checkout", env, False).argv("build", "-p", "app")
          == ["cargo", "build", "-p", "app"])
    check("zero product calls pass strict rows",
          strict_macro_rows([dict(right=True, product_tool_calls=[])]))
    for tool in ("cargo", "rustc", "rustdoc"):
        check(f"strict rows never exclude a {tool} query",
              not strict_macro_rows([dict(right=True, product_tool_calls=[
                  dict(tool=tool, argv="--version")])]))
    check("zero-call wrong output still fails strict rows",
          not strict_macro_rows([dict(right=False, product_tool_calls=[])]))
    check("absolute external execs fail strict rows without a proxy call",
          not strict_macro_rows([dict(right=True, product_tool_calls=[], external_execs=["/real/rustc"])]))
    check("missing witness fails strict rows", not strict_macro_rows([]))
    with tempfile.TemporaryDirectory() as root:
        token = "synthetic-scanner-credential"
        with open(os.path.join(root, "load-env"), "wb") as output:
            output.write(token.encode().hex().encode())
        scan = token_scan([root, root], token)
        check("live token scan catches hex CBOR and overlapping roots", scan["scanned"] == 1
              and len(scan["hits"]) == 1 and scan["hits"][0]["encoding"] == "hex")
        os.unlink(os.path.join(root, "load-env"))
        check("clean token scan passes", not token_scan([root], token)["hits"])
        missing = os.path.join(root, "missing-root")
        check("missing scan root fails coverage", token_scan([missing], token)["errors"] == [missing])
        unreadable = os.path.join(root, "unreadable")
        def failed_walk(path, onerror, followlinks):
            onerror(PermissionError(13, "permission denied", unreadable))
            return iter(())
        with mock.patch("os.walk", side_effect=failed_walk):
            check("directory read errors fail scan coverage",
                  token_scan([root], token)["errors"] == [unreadable])
        with tempfile.TemporaryDirectory() as external:
            with open(os.path.join(external, "secret"), "w") as output:
                output.write(token)
            os.symlink(external, os.path.join(root, "linked-output"))
            os.symlink(root, os.path.join(external, "cycle"))
            scan = token_scan([root, external], token)
            check("linked outputs are scanned once even through directory cycles",
                  scan["scanned"] == 1 and bool(scan["hits"]) and not scan["errors"])

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
    check("a function only A made fails and is named", any(x.kind == "function-alone" for x in d) and any(x.kind == "function-key-set" for x in d))

    renamed_digests = {h(f"B key {i}"): {(10 + i, h(f"value {i}"))} for i in range(5)}
    renamed_names = {h(f"B key {i}"): (h(f"value {i}"), f"fn {id_b}:src/lib.rs#{i}<u32>") for i in range(5)}
    n, d = compare_stores("t", same(), renamed_digests, names(id_a), renamed_names)
    cause = next(x for x in d if x.kind == "crate-id")
    check("the same functions under another crate id are one cause", cause.count == 5)
    check("the cause names both ids and the file", id_a[:12] in cause.key and id_b[:12] in cause.key and "src/lib.rs" in cause.key)
    same_names = {k: (h("v"), f"fn {id_a}:src/lib.rs#1<>") for k in [h("a")]}
    other_names = {h("b"): next(iter(same_names.values()))}
    _, d = compare_stores("t", {h("a"): {(5, h("v"))}}, {h("b"): {(5, h("v"))}}, same_names, other_names)
    check("same stable name with different disk keys fails", any(x.kind == "function-key-set" for x in d))
    zero = "store: 10 functions found, 0 compiled and added\nentries: 10 found, 0 added, 10 as stored, 0 not as stored, 0 instances and 0 requests without a stable name\n"
    check("complete zero-compute counters pass coverage", not fed_coverage("t", zero, {}, True))
    check("missing B-fed log fails coverage", bool(fed_coverage("t", zero, {}, False)))
    check("missing counters fail coverage", bool(fed_coverage("t", "", {}, True)))
    check("truncated counters fail coverage", bool(fed_coverage("t", "store: 0 functions found, 7 compiled and\nentries: 0 found, 7 added,\n", {}, True)))
    check("unavailable counters fail coverage", bool(fed_coverage("t", "store: unavailable\nentries: unavailable\n", {}, True)))
    check("a malformed record beside valid counters fails", bool(fed_coverage("t", zero + "store: broken\n", {}, True)))
    stats_zero = zero.split("\n", 1)[1] + "stats: 0 of 3 functions compiled, 3 reused\nentries: 1.2µs\n"
    check("complete compiler stats and phase timing pass coverage", not fed_coverage("t", stats_zero, {}, True))
    check("truncated compiler stats fail coverage", bool(fed_coverage("t", stats_zero.replace("3 reused", ""), {}, True)))
    check("nonzero compiler stats fail coverage", bool(fed_coverage("t", stats_zero.replace("stats: 0", "stats: 1"), {}, True)))
    global DAEMON_ROOT
    DAEMON_ROOT = "/tmp/c4h-test"
    check("independent daemon homes fit the socket budget", len((daemon_home("vendor-payload", "b-solo") + "/workers/" + "f" * 16 + "/sock.new/s").encode()) < 104)
    payload = "/fixture/src/payload.bin"
    missing = f"error: could not read included input `{payload}`"
    oracle = f"error: couldn't read `{payload}`: No such file"
    check("named missing input errors pass", missing_input_failure(101, missing, 101, oracle, payload))
    check("unrelated compiler errors fail", not missing_input_failure(101, "unrelated error", 101, oracle, payload))
    check("missing oracle diagnostic fails", not missing_input_failure(101, missing, 101, "unrelated error", payload))
    check("fallback beside expected error fails", not missing_input_failure(101, missing + "\nthis is `cargo build`", 101, oracle, payload))
    ready_status = "daemon: pid 1\nworker: pid 2\n/ws: ready\n3 builds by the daemon, 0 by cargo"
    check("unchanged ready workers pass", valid_worker_generation(ready_status, ["ready"], ["1", "2"], ["1", "2"], 3, 2))
    check("PID replacement fails", not valid_worker_generation(ready_status, ["ready"], ["1", "3"], ["1", "2"], 3, 2))
    check("real PM edit advances bytes image and reload", valid_macro_mutation("same-size-restored-mtime", True, True, "a", "b", {"a": "image"}, {"b": "image"}, 0, 1))
    check("a no-op PM edit fails", not valid_macro_mutation("same-size-restored-mtime", True, True, "a", "a", {"a": "image"}, {"b": "image"}, 0, 1))
    check("PM changed mtime fails the restored-mtime control", not valid_macro_mutation("same-size-restored-mtime", True, False, "a", "b", {"a": "image"}, {"b": "image"}, 0, 1))
    check("PM changed size fails the same-size control", not valid_macro_mutation("same-size-restored-mtime", False, True, "a", "b", {"a": "image"}, {"b": "image"}, 0, 1))
    check("PM stale image fails", not valid_macro_mutation("dependency-restored-mtime", True, True, "a", "b", {"a": "image"}, {"a": "image"}, 0, 1))
    check("PM missing reload fails", not valid_macro_mutation("reload-00", True, True, "a", "b", {"a": "image"}, {"b": "image"}, 1, 1))
    check("PM unchanged control cannot hide a reload", not valid_macro_mutation("unchanged-content", True, True, "a", "a", {"a": "image"}, {"a": "image"}, 1, 2))
    check("nonready workers fail", not valid_worker_generation(ready_status, ["loading"], ["1", "2"], ["1", "2"], 3, 2))
    check("cargo build count fails", not valid_worker_generation(ready_status.replace("0 by cargo", "1 by cargo"), ["ready"], ["1", "2"], ["1", "2"], 3, 2))
    check("unnamed compute fails coverage", bool(fed_coverage("t", zero.replace("0 instances", "1 instances"), {}, True)))
    check("unnamed requests fail coverage", bool(fed_coverage("t", zero.replace("0 requests", "1 requests"), {}, True)))
    check("compiled functions fail despite empty log", bool(fed_coverage("t", zero.replace("0 compiled", "1 compiled"), {}, True)))

    value_digests = same()
    value_digests[h("key 1")] = {(11, h("another value"))}
    n, d = compare_stores("t", same(), value_digests, names(id_a), names(id_a))
    check("the same key with other bytes is a value difference", [x.kind for x in d] == ["value"] and n["functions"]["differing"] == 1)

    other_a = {h("frozen 1"): {(5, h("x"))}}
    n, d = compare_stores("t", {**same(), **other_a}, same(), names(id_a), names(id_a))
    check("a record that is not a function is counted apart", n["other"]["only_a"] == 1 and any(x.kind == "other-record" for x in d))
    other_b = {h("frozen 1"): {(5, h("different"))}}
    n, d = compare_stores("t", other_a, other_b, {}, {})
    check("unequal bytes of non-function records fail", [x.kind for x in d] == ["other-value"] and bool(failing([{"differences": [x.row() for x in d]}])))

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
    global DAEMON_ROOT
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
    p.add_argument("--warm-payload", action="store_true",
                   help="same-daemon binary payload edit/delete/recreate on two checkout paths")
    p.add_argument("--warm-nonfunction", action="store_true")
    p.add_argument("--warm-macro-content", action="store_true")
    p.add_argument("--macro-reloads", type=int, default=0)
    p.add_argument("--remote", help="the server remote of unit C5; not implemented here")
    p.add_argument("--b-docker", action="store_true", help="machine B in a container of another libc: unit C5")
    p.add_argument("--watchdog-mb", type=int, default=24 * 1024)
    p.add_argument("--warmup-secs", type=int, default=1800,
                   help="maximum wait for a daemon load; timeout fails the fixture")
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
    names = ["macros"] if a.warm_macro_content else (["vendor-payload"] if a.warm_payload or a.warm_nonfunction else (ALL if a.all else a.fixtures))
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
    import cargo_ra_jit
    cargo_ra_jit.WARMUP_SECS = a.warmup_secs
    work = os.path.realpath(a.work)
    run_id = "xm%s-%d" % (time.strftime("%m%d%H%M%S"), os.getpid())
    os.environ["RA_JIT_RUN_ID"] = run_id
    # Every run starts clean; the seed cargo home is the one thing kept, since it costs a download.
    if os.path.isdir(work):
        for entry in os.listdir(work):
            if entry != "seed-cargo-home":
                shutil.rmtree(os.path.join(work, entry), ignore_errors=True)
    os.makedirs(work, exist_ok=True)
    DAEMON_ROOT = tempfile.mkdtemp(prefix="c4h-", dir="/tmp")

    from tracedecay import Watchdog
    watchdog = Watchdog(bin_dir, limit_mb=a.watchdog_mb, run_id=run_id, homes=(),
                        series=os.path.join(work, "watchdog-series.jsonl"))
    watchdog.start()

    def progress(line):
        print(line, flush=True)

    results = []
    try:
        sources = {n: source_of(n, work, offline=not a.online) for n in names}
        seed = seed_cargo_home(work, names, sources, a.online)
        rust_home = os.environ.get("RUSTUP_HOME") or os.path.expanduser("~/.rustup")
        a_m = Machine(os.path.join(work, "m"), "a", ["alice", "src", "co"], seed, rust_home)
        b_m = Machine(os.path.join(work, "m"), "b", ["builder-2", "x"], seed, rust_home)
        progress(f"machine A: home {a_m.home}\n           checkouts {a_m.checkouts}\n"
                 f"machine B: home {b_m.home}\n           checkouts {b_m.checkouts}")
        if a.reference_only:
            return 1 if reference_only(names, work, bin_dir, (a_m, b_m), sources, progress) else 0
        if a.warm_payload or a.warm_nonfunction or a.warm_macro_content:
            runner = macro_content_report if a.warm_macro_content else (nonfunction_report if a.warm_nonfunction else warm_payload_report)
            rows = runner(work, bin_dir, (a_m, b_m), sources, a.macro_reloads) if a.warm_macro_content else runner(work, bin_dir, (a_m, b_m), sources)
            with open(a.json or os.path.join(work, "warm-payload.json"), "w") as output:
                json.dump(dict(rows=rows, built_from=open(os.path.join(bin_dir, "BUILT_FROM")).read().strip()), output, indent=1)
            print(json.dumps(rows, indent=1))
            if a.warm_macro_content:
                return 1 if not strict_macro_rows(rows) or watchdog.tripped else 0
            if a.warm_nonfunction or a.warm_macro_content:
                return 1 if any(not row["right"] for row in rows) or watchdog.tripped else 0
            return 1 if any(
                not r["right"]
                or (r["step"] not in ("deleted", "reintroduce-missing") and r["classification"] != "daemon")
                or recognized_fallback(r["build_log"])
                for r in rows
            ) or watchdog.tripped else 0
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
        shutil.rmtree(DAEMON_ROOT, ignore_errors=True)
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
