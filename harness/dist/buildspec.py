#!/usr/bin/env python3
"""buildspec.py: unit C2's acceptance (docs/distributed.md, "C2 `BuildSpec` from cargo's
resolution"): a command's `BuildSpec` differs where a flag changes the build and is the same where
it does not, blaze's unit graph is cargo's, some commands are refused by name, and running a spec
locally does what the command does.

    buildspec.py                            every fixture and case, every arm
    buildspec.py --only basic --only ws/build-p-a    only those fixtures or cases (what a case is
                                            compared with runs too, and is not reported)
    buildspec.py --no-local                 skip the local runs (arm 9: each takes a cold build twice)
    buildspec.py --no-cargo                 skip the comparison with cargo's unit graph (arm 8)
    buildspec.py --bin PATH --work DIR --keep --json FILE --timeout SECONDS --shared-store --no-audit
    buildspec.py --list                     the fixtures and cases, and what each is expected to do
    buildspec.py --self-test                the harness's own logic, against a fake ra-jit

`--bin` is the ra-jit under test (default `$RA_JIT_BIN`, else `target/release/ra-jit`). It exits 0
only when every expectation holds; an arm that is skipped is printed as skipped, with why.

The contract it is written against (the lead's crates/ra-jit/src/cargo/spec.rs):

    ra-jit cargo spec [--unit-graph] [--out FILE] <check|build|run|test> [cargo flags] [-- args]

prints one JSON object {refused, digests{spec,platform,front_end,config,job_content}, summary
{command,asked,units}, spec, platform} and exits 0 (a spec), 2 (refused: the object is still
printed) or 1 (any other error: a message on stderr); with `--unit-graph` it prints blaze's unit
graph in the shape of `cargo build --unit-graph -Z unstable-options`; `--out` writes the spec's
canonical CBOR. `ra-jit cargo spec --run FILE` derives the flags and the environment from the spec
alone, recomputes the spec, refuses (exit 2, `blaze: the tree does not match the spec: <field>`)
when its digest differs and otherwise does what `ra-jit cargo <command>` does.

The arms:

  spec (1-7)    Fixtures are small cargo projects the script writes into a temp directory (path
                dependencies only, a `Cargo.lock` written out, each `git init`ed and committed with a
                fixed identity and date: a spec names a commit). Each case is a command (words,
                environment, optional edits of the fixture) with an expectation against another
                case: `same`, `changed`, or `refused:<what>`. Every expectation has the reason in
                cargo's semantics beside it. Configuration edits are committed and compared on the
                spec JSON without `source` (the "fingerprint"), and the table says so. Source cases
                compare the complete spec and job identity: a body edit, including an uncommitted
                one, and selecting either of two sibling workspaces must name the actual inputs.
  units (8)     `ra-jit cargo spec --unit-graph <command>` against `cargo +<toolchain> <command>
                --unit-graph -Z unstable-options` (RUSTC_BOOTSTRAP=1, --locked --offline), as sets of
                units keyed by package, target, mode, features, platform and profile with the
                dependency indexes mapped to those keys and the paths ignored. cargo is the ORACLE
                here and nowhere else. `run` has no unit graph of its own in cargo and is left out;
                cargo's `doctest` units are left out, as blaze-metadata's `Mode` has none (doc tests
                are `RunSpec.doc`).
  local (9)     `ra-jit cargo <command>` in a fresh copy of the fixture, and `ra-jit cargo spec --run
                <spec.cbor>` in another fresh copy of the same commit (with none of the case's
                environment: the spec alone), each with a fresh CARGO_TARGET_DIR and RA_JIT_HOME:
                the programs built, the tests run, the doc tests run, what the programs and the
                custom harness printed, and the exit status have to be equal. Both runs are under
                tracedecay.py's memory guard (RA_JIT_MEMORY_MAX=16G, RA_JIT_OWN_BASE_MEMORY=12G,
                RA_JIT_SPARE=0 and its watchdog) and audited with norustc.py's stand-in rustc,
                rustdoc and cargo: blaze's cold build still hands work to cargo (docs/no-rustc.md,
                a known bug), so the number of cargo runs is reported as `cargo dispatched: N` and
                never fails a case. The daemon of each run is stopped and its directories deleted.

The harness never runs cargo or rustc for the thing under test. Python 3.9+, standard library only
(and `tracedecay.py` and `norustc.py` beside it, which it imports for the guard and the stand-ins:
they run nothing at import).
"""
import argparse
import collections
import contextlib
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
ROOT = os.path.dirname(HARNESS)
# `python3 -I` leaves the script's directory out of sys.path; tracedecay.py and norustc.py are the
# harness directory's, and tracedecay.py takes dist/fetch.py from here.
for _dir in (HARNESS, HERE):
    if _dir not in sys.path:
        sys.path.insert(0, _dir)
import norustc  # noqa: E402
import tracedecay  # noqa: E402

TOOLCHAIN = open(os.path.join(HARNESS, "toolchain")).read().strip()
DEFAULT_BIN = os.environ.get("RA_JIT_BIN") or os.path.join(ROOT, "target", "release", "ra-jit")

# The five digests the contract prints, and what `spec` is made of (docs/distributed.md, "Data shapes").
DIGESTS = ("spec", "platform", "front_end", "config", "job_content")
SPEC_FIELDS = ("source", "lock", "units", "asked", "profile", "platform", "rustflags", "config_env",
               "script_env", "cwd", "command", "toolchain")
UNIT_FIELDS = ("package", "target", "features", "cfg", "edition", "deps", "test", "host", "profile")
PROFILE_FIELDS = ("name", "opt_level", "debug", "debug_assertions", "overflow_checks", "panic", "strip",
                  "split_debuginfo", "backend", "mir_opt", "backend_opt", "backend_emit")
RUN_FIELDS = ("filters", "exact", "skip", "ignored", "no_run", "no_fail_fast", "test_threads",
              "harness_args", "env", "doc")
COMMANDS = {0: "check", 1: "build", 2: "test", 3: "run"}
KINDS = {0: "lib", 1: "bin", 2: "test", 3: "example", 4: "bench", 5: "build-script", 6: "proc-macro"}

# A command that only makes a spec does not take longer than this (a local run has --timeout).
SPEC_TIMEOUT = 300

# The environment a case starts from: everything of the process's that is not about the build.
SCRUB_PREFIXES = ("RA_JIT_", "CARGO_", "RUSTC", "RUSTDOC", "CC_", "CXX_", "TARGET_CC", "HOST_CC")
SCRUB_NAMES = {"RUSTFLAGS", "RUSTUP_TOOLCHAIN", "CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "CPPFLAGS",
               "LDFLAGS", "ARFLAGS", "MACOSX_DEPLOYMENT_TARGET", "IPHONEOS_DEPLOYMENT_TARGET",
               "CRATE_CC_NO_DEFAULTS"}
# What a fixture commit is made with: a spec names a commit, so the commit must not change with
# the day it is made, the user or the machine.
GIT_ENV = {"GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
           "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
           "GIT_AUTHOR_DATE": "2026-01-01T00:00:00+0000", "GIT_COMMITTER_DATE": "2026-01-01T00:00:00+0000",
           "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0"}


class HarnessError(Exception):
    """A mistake of this script or of the machine (a fixture edit that did not apply, no git), not a
    verdict on ra-jit."""


def sha256(data):
    return hashlib.sha256(data if isinstance(data, bytes) else data.encode()).hexdigest()


def canon(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def short(digest, n=12):
    return (digest or "-")[:n]


def clip(text, n=240):
    text = " ".join(str(text).split())
    return text if len(text) <= n else text[:n - 1] + "…"


def scrubbed_env(extra=None):
    """The process environment without anything that changes a build, plus `extra` (a value of
    None removes the name)."""
    env = {k: v for k, v in os.environ.items()
           if k not in SCRUB_NAMES and not k.startswith(SCRUB_PREFIXES)
           and not k.endswith("RUSTFLAGS")}
    env["RUST_BACKTRACE"] = "0"
    env["CARGO_TERM_COLOR"] = "never"
    for name, value in (extra or {}).items():
        if value is None:
            env.pop(name, None)
        else:
            env[name] = value
    return env


def git(directory, *args, check=True):
    ran = subprocess.run(["git", "-C", directory, *args], env=dict(os.environ, **GIT_ENV),
                         stdin=subprocess.DEVNULL, capture_output=True, text=True)
    if check and ran.returncode:
        raise HarnessError(f"git {' '.join(args)} in {directory} failed: {ran.stderr.strip()}")
    return ran.stdout.strip()


def commit_all(directory, message):
    git(directory, "add", "-A")
    git(directory, "-c", "commit.gpgsign=false", "commit", "-q", "-m", message)
    return git(directory, "rev-parse", "HEAD")


def write_files(directory, files):
    for path, text in files.items():
        full = os.path.join(directory, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(text)
        if path.endswith(".sh") or path.startswith("bin/"):
            os.chmod(full, 0o755)


def apply_edit(directory, edit):
    """An edit: ("write", path, text), ("append", path, text), ("replace", path, old, new) or
    ("delete", path)."""
    kind, path = edit[0], os.path.join(directory, edit[1])
    if kind == "write":
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(edit[2])
    elif kind == "append":
        with open(path, "a") as f:
            f.write(edit[2])
    elif kind == "replace":
        text = open(path).read()
        if edit[2] not in text:
            raise HarnessError(f"the edit of {edit[1]} wants {edit[2]!r}, which is not in it")
        with open(path, "w") as f:
            f.write(text.replace(edit[2], edit[3], 1))
    elif kind == "delete":
        os.unlink(path)
    else:
        raise HarnessError(f"an edit of kind {kind!r}")


def lock_text(packages):
    """A Cargo.lock for path-only packages, as cargo 1.99 writes it, so that `--locked --offline`
    needs nothing: `packages` is [(name, [dependency names])], written in name order."""
    out = ["# This file is automatically @generated by Cargo.", "# It is not intended for manual editing.",
           "version = 4", ""]
    for name, deps in sorted(packages):
        out += ["[[package]]", f'name = "{name}"', 'version = "0.1.0"']
        if deps:
            out.append("dependencies = [")
            out += [f' "{d}",' for d in sorted(deps)]
            out.append("]")
        out.append("")
    return "\n".join(out).rstrip("\n") + "\n"


# --- Reading the JSON rendering of a spec --------------------------------------------------------
# blaze-wire's JSON rendering of a struct is an object keyed by its field numbers as text, an enum
# the pair [discriminant, payload], a digest hex, a set an array. The accessors take either the
# numbers or the names, so a rendering that spelled the names would read the same.

def field(obj, number, name):
    if isinstance(obj, dict):
        if str(number) in obj:
            return obj[str(number)]
        return obj.get(name)
    return None


def spec_field(spec, name):
    return field(spec, SPEC_FIELDS.index(name), name)


def unwrap(spec):
    """`{"BuildSpec": {...}}` (blaze-wire's `to_json`) or the bare object."""
    if isinstance(spec, dict) and set(spec) == {"BuildSpec"}:
        return spec["BuildSpec"]
    return spec


class Unit:
    """One UnitSpec of a rendered spec."""

    def __init__(self, raw):
        self.raw = raw
        pkg = field(raw, 0, "package") or ["?", "?", ""]
        tgt = field(raw, 1, "target") or [0, "?"]
        self.package, self.version = pkg[0], pkg[1]
        self.kind_n, self.name = tgt[0], tgt[1]
        self.kind = KINDS.get(self.kind_n, str(self.kind_n))
        self.features = tuple(field(raw, 2, "features") or ())
        self.cfg = tuple(field(raw, 3, "cfg") or ())
        self.edition = field(raw, 4, "edition")
        self.deps = field(raw, 5, "deps") or []
        self.test = field(raw, 6, "test")
        self.host = field(raw, 7, "host")
        self.profile = profile_of(field(raw, 8, "profile"))

    def identity(self):
        """Everything of the unit but its profile and dependencies' indexes: what stays the same
        across an edit of a profile."""
        return (self.package, self.version, self.kind_n, self.name, self.features, self.cfg,
                self.edition, self.test, self.host)

    def label(self):
        return f"{self.package}:{self.kind}:{self.name}{' (host)' if self.host else ''}"


def profile_of(raw):
    return {name: field(raw, i, name) for i, name in enumerate(PROFILE_FIELDS)} if raw is not None else {}


def units_of(spec):
    return [Unit(u) for u in (spec_field(spec, "units") or [])]


def units_by_identity(spec):
    """{identity: unit}; an identity that two units share keeps both, numbered."""
    found = {}
    for unit in units_of(spec):
        key = unit.identity()
        n = 0
        while key + (n,) in found:
            n += 1
        found[key + (n,)] = unit
    return found


def command_of(spec):
    """(name, RunSpec dict or None)."""
    raw = spec_field(spec, "command")
    if not isinstance(raw, list) or not raw:
        return "?", None
    payload = raw[1] if len(raw) > 1 else None
    run = {name: field(payload, i, name) for i, name in enumerate(RUN_FIELDS)} if isinstance(payload, dict) else None
    return COMMANDS.get(raw[0], str(raw[0])), run


def fingerprint(spec):
    """The spec without its `source`: what two commits of one configuration share."""
    spec = unwrap(spec)
    if not isinstance(spec, dict):
        return sha256(canon(spec))
    return sha256(canon({k: v for k, v in spec.items() if k not in ("0", "source")}))


def changed_fields(a, b, compare_source=False):
    """The top-level fields in which two renderings differ, optionally including `source`."""
    a, b = unwrap(a), unwrap(b)
    return {name for name in SPEC_FIELDS if (compare_source or name != "source")
            and canon(spec_field(a, name)) != canon(spec_field(b, name))}


# --- Fixtures and cases --------------------------------------------------------------------------

class Case:
    """One command of a fixture, what it is expected to do against another case, and why.

    expect: "base" (nothing to compare with; it has to make a spec, twice the same), "same",
            "changed", "accepted" (a spec is made: nothing is claimed of it), "refused:<text>" (the
            refusal's `what` has <text> in it, any case), "changed-or-refused:<text>", or
            "llvm:same" / "llvm:changed" (as the case `llvm_of` is: if that was refused for
            RA_JIT_BACKEND this one has to be too, else the second word).
    vs:     the case compared with (the fixture's `base` when None).
    edits:  edits of the fixture before the command; committed unless `commit_edits` is false.
    compare_source: compare the complete spec, including `source`, for source identity cases.
            Otherwise two different sources are compared without `source`, so configuration
            edits can state exactly which settings change.
    only:   the top-level spec fields that may differ from `vs` (the others have to be equal).
    touches: fields that must be among those that differ.
    also:   more relations: ("same"|"differs"|"platform-same"|"platform-differs"|"job-differs"|
            "units-equal"|"asked-same"|"asked-differs", case).
    asked_pkgs: the packages `summary.asked` names, as a set.
    check:  fn(results, case) -> list of problems, for what the relations cannot say.
    lexpect: what the local run (the command's own) has to show; see `check_lexpect`.
    relocate: the command runs in a copy of the base tree at another path.
    """

    def __init__(self, name, words, why, expect="same", vs=None, env=None, edits=(), cwd="", local=False,
                 only=None, touches=(), also=(), asked_pkgs=None, check=None, lexpect=(), relocate=False,
                 units=None, llvm_of=None, skip=None, commit_edits=True, compare_source=False):
        self.name = name
        self.words = shlex.split(words) if isinstance(words, str) else list(words)
        self.why = why
        self.expect, self.vs, self.env, self.edits, self.cwd, self.local = expect, vs, env or {}, list(edits), cwd, local
        self.only, self.touches, self.also, self.asked_pkgs = only, tuple(touches), list(also), asked_pkgs
        self.check, self.lexpect, self.relocate, self.units, self.llvm_of = check, list(lexpect), relocate, units, llvm_of
        self.skip = skip  # fn(ctx) -> reason or None
        self.commit_edits, self.compare_source = commit_edits, compare_source
        self.fixture = None

    @property
    def full(self):
        return f"{self.fixture.name}/{self.name}"

    @property
    def command(self):
        return self.words[0]

    @property
    def tree_edited(self):
        return bool(self.edits)


class Fixture:
    def __init__(self, name, files, cases, setup=None, skip=None):
        self.name, self.files, self.cases = name, files, cases
        self.setup = setup  # fn(ctx) -> None, run before the cases (tools a case's environment names)
        self.skip = skip    # fn(ctx) -> reason or None
        for case in cases:
            case.fixture = self
        self.by_name = {c.name: c for c in cases}
        if len(self.by_name) != len(cases):
            raise HarnessError(f"fixture {name} has two cases of one name")

    def base(self):
        return self.by_name["base"]


GITIGNORE = {".gitignore": "target/\n"}


def pkg_manifest(name, extra="", edition="2021"):
    return f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "{edition}"\n{extra}'


# 1. basic ------------------------------------------------------------------------------------------

BASIC = {
    "Cargo.toml": pkg_manifest("basic", '\n[features]\ndefault = ["std"]\nstd = []\nextra = []\n'),
    "Cargo.lock": lock_text([("basic", [])]),
    "src/lib.rs": '''//! The library of `basic`.

/// Adds.
///
/// ```
/// assert_eq!(basic::add(1, 2), 3);
/// ```
pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[cfg(feature = "std")]
pub fn which() -> &'static str {
    "std"
}

#[cfg(not(feature = "std"))]
pub fn which() -> &'static str {
    "no-std"
}

#[cfg(feature = "extra")]
pub fn extra() -> u32 {
    7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 2), 4);
    }

    #[test]
    #[ignore]
    fn slow() {
        assert_eq!(add(1, 1), 2);
    }
}
''',
    "src/main.rs": '''fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("@@bin basic add={} args={:?}", basic::add(1, 2), args);
}
''',
    "tests/it.rs": '''#[test]
fn integration_adds() {
    assert_eq!(basic::add(1, 2), 3);
}
''',
    "examples/demo.rs": '''fn main() {
    println!("@@example demo {}", basic::add(1, 1));
}
''',
    **GITIGNORE,
}


def basic_cases():
    return [
        Case("base", "test", "the base: `cargo test` of one package", expect="base", local=True,
             lexpect=[("test", "adds", "ok"), ("test", "integration_adds", "ok"), ("no-test", "slow"),
                      ("program", "example", "demo"), ("exit", 0)]),
        # `default = ["std"]` is what `--no-default-features` removes: the lib's features and cfgs
        # change, so its units do.
        Case("no-default-features", "test --no-default-features", "removes the `std` feature", expect="changed",
             only={"units"}, touches=["units"], local=True),
        Case("all-features", "test --all-features", "adds `extra`", expect="changed", only={"units"},
             touches=["units"]),
        # `std` is already on: the unified feature set is the same.
        Case("features-std", "test --features std", "names a feature that is already on", expect="same"),
        # A package with one member: `-p basic` selects what the command selects with no flag.
        Case("p-basic", "test -p basic", "names the only package", expect="same"),
        # What cargo only prints, or how many jobs it runs at once.
        Case("verbose", "test -v", "-v changes what cargo prints", expect="same"),
        Case("color", "test --color never", "--color changes what cargo prints", expect="same"),
        Case("jobs", "test -j 2", "-j is how many jobs run at once", expect="same"),
        Case("quiet", "test -q", "-q changes what cargo prints", expect="same"),
        # RunSpec.no_run: the tests are built and not run.
        Case("no-run", "test --no-run", "builds the tests and does not run them", expect="changed",
             only={"command"}, touches=["command"], local=True,
             lexpect=[("no-test", "adds"), ("no-test", "integration_adds"), ("program", "example", "demo"),
                      ("exit", 0)]),
        # RunSpec.no_fail_fast: the first failing test binary does not stop the rest.
        Case("no-fail-fast", "test --no-fail-fast", "keeps going after a failure", expect="changed",
             only={"command"}, touches=["command"], local=True),
        # RunSpec.ignored = 1 and harness_args.
        Case("ignored", "test -- --ignored", "runs only the ignored tests", expect="changed",
             only={"command"}, touches=["command"], local=True,
             lexpect=[("test", "slow", "ok"), ("no-test", "adds"), ("exit", 0)]),
        # RunSpec.ignored = 2: not the same as `--ignored`.
        Case("include-ignored", "test -- --include-ignored", "runs the ignored tests with the rest",
             expect="changed", only={"command"}, touches=["command"], local=True,
             also=[("differs", "ignored")],
             lexpect=[("test", "slow", "ok"), ("test", "adds", "ok"), ("exit", 0)]),
        Case("test-threads", "test -- --test-threads=1", "runs the tests on one thread", expect="changed",
             only={"command"}, touches=["command"]),
        # RunSpec.filters and `exact`.
        Case("filter", "test adds", "names a test", expect="changed", only={"command"}, touches=["command"]),
        Case("filter-exact", "test adds -- --exact", "names a test and asks for it exactly", expect="changed",
             vs="filter", only={"command"}, touches=["command"]),
        # RunSpec.doc: 0 none, 1 with the unit tests, 2 only these.
        Case("doc", "test --doc", "runs only the doc tests", expect="changed", touches=["command"]),
        # Asked units: only the library's tests.
        Case("lib", "test --lib", "asks only for the library's unit test", expect="changed",
             touches=["asked"]),
        # RUSTFLAGS reach the rustflags of the spec, and the cfgs of the units.
        Case("rustflags-a", "test", "a `--cfg` is a flag blaze honours", env={"RUSTFLAGS": "--cfg a"},
             expect="changed", touches=["rustflags"], only={"rustflags", "units"}),
        Case("rustflags-b", "test", "another `--cfg`", env={"RUSTFLAGS": "--cfg b"}, expect="changed",
             touches=["rustflags"], only={"rustflags", "units"}, also=[("differs", "rustflags-a")]),
        # cargo reads an empty RUSTFLAGS as no flags.
        Case("rustflags-empty", "test", "an empty RUSTFLAGS is no flags", env={"RUSTFLAGS": ""}, expect="same"),
        Case("build", "build", "another command", expect="changed", touches=["command"],
             also=[("differs", "check")]),
        Case("check", "check", "another command", expect="changed", touches=["command"],
             also=[("differs", "build")]),
        Case("release", "test --release", "another profile", expect="changed", touches=["profile"]),
        # `run`: its arguments, before and after `--`, are in the spec as typed.
        Case("run-x", "run -- x", "runs the binary with an argument", expect="changed", touches=["command"],
             local=True, lexpect=[("line", r'@@bin basic add=3 args=\["x"\]'), ("exit", 0)]),
        Case("run-y", "run -- y", "runs it with another", expect="changed", vs="run-x", touches=["command"],
             only={"command"}),
        # A spec names a commit and a relative directory, not a path of this machine.
        Case("moved", "test", "the same tree at another path", expect="same", relocate=True),
    ]


# 2. ws ---------------------------------------------------------------------------------------------

WS = {
    "Cargo.toml": '[workspace]\nresolver = "2"\nmembers = ["crates/a", "crates/b", "crates/c"]\n',
    "Cargo.lock": lock_text([("a", ["b"]), ("b", []), ("c", ["a"])]),
    "crates/a/Cargo.toml": pkg_manifest("a", '\n[dependencies.b]\npath = "../b"\n'),
    "crates/a/src/lib.rs": "pub fn a() -> u32 {\n    b::b() + 1\n}\n",
    "crates/b/Cargo.toml": pkg_manifest("b"),
    "crates/b/src/lib.rs": "pub fn b() -> u32 {\n    1\n}\n",
    "crates/c/Cargo.toml": pkg_manifest("c", '\n[dependencies.a]\npath = "../a"\n'),
    "crates/c/src/main.rs": 'fn main() {\n    println!("@@bin c {}", a::a());\n}\n',
    **GITIGNORE,
}


def ws_cases():
    return [
        # A virtual workspace builds all its members.
        Case("base", "build", "a virtual workspace's default members are all its members", expect="base",
             asked_pkgs={"a", "b", "c"}, local=True, lexpect=[("program", "bin", "c"), ("exit", 0)]),
        Case("build-p-a", "build -p a", "names one member: the units asked are fewer", expect="changed",
             touches=["asked"], asked_pkgs={"a"}, local=True,
             lexpect=[("no-program", "bin", "c"), ("exit", 0)]),
        Case("excl-c", "build --workspace --exclude c", "leaves c out", expect="changed", touches=["asked"],
             asked_pkgs={"a", "b"}, local=True, lexpect=[("no-program", "bin", "c"), ("exit", 0)]),
        # The same two members, named the other way.
        Case("build-p-a-p-b", "build -p a -p b", "names a and b", expect="same", vs="excl-c",
             asked_pkgs={"a", "b"}),
        # Packages are selected as a set: the order they are named in does not matter.
        Case("build-p-b-p-a", "build -p b -p a", "names them the other way round", expect="same",
             vs="build-p-a-p-b", asked_pkgs={"a", "b"}),
        # `--workspace` in a virtual workspace is what `build` does.
        Case("workspace", "build --workspace", "names every member", expect="same", asked_pkgs={"a", "b", "c"}),
        # cargo takes the package whose directory the command runs in, as `-p a`: the units are the
        # same, and the directory the command ran in is the spec's `cwd` field, so the digests differ.
        Case("in-a", "build", "from inside crates/a", cwd="crates/a", vs="build-p-a", expect="changed",
             only={"cwd"}, touches=["cwd"], asked_pkgs={"a"},
             also=[("units-equal", "build-p-a"), ("asked-same", "build-p-a")], local=True,
             lexpect=[("no-program", "bin", "c"), ("exit", 0)]),
        # The process's directory, not the manifest's, is the spec's cwd: same units, same cwd as `-p a`.
        Case("manifest-path", "build --manifest-path crates/a/Cargo.toml", "names a's manifest",
             vs="build-p-a", expect="same", asked_pkgs={"a"}),
        Case("test-ws-excl-c", "test --workspace --exclude c", "tests a and b", expect="changed",
             touches=["command"], asked_pkgs={"a", "b"}),
        # Flags of `--workspace` and `--exclude` can come in either order.
        Case("test-excl-c-ws", "test --exclude c --workspace", "the flags the other way round", expect="same",
             vs="test-ws-excl-c", asked_pkgs={"a", "b"}),
        Case("moved", "build", "the same tree at another path", expect="same", relocate=True),
    ]


# 3. harness = false --------------------------------------------------------------------------------

HARN = {
    "Cargo.toml": pkg_manifest("harn", '\n[[test]]\nname = "custom"\npath = "tests/custom.rs"\nharness = false\n'),
    "Cargo.lock": lock_text([("harn", [])]),
    "src/lib.rs": "pub fn id(x: u32) -> u32 {\n    x\n}\n",
    "tests/custom.rs": '''fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("@@custom args={:?}", args);
}
''',
    "tests/normal.rs": "#[test]\nfn ok() {\n    assert_eq!(harn::id(1), 1);\n}\n",
    **GITIGNORE,
}


def check_harness_units(results, case):
    """A `harness = false` test is a test target built without libtest: kind 2, and cfg(test) is
    not set; a test with the harness has cfg(test) set."""
    problems = []
    res = results.get(case.full)
    if not res or not res.spec:
        return problems
    custom = [u for u in units_of(res.spec) if u.kind == "test" and u.name == "custom"]
    normal = [u for u in units_of(res.spec) if u.kind == "test" and u.name == "normal"]
    if not custom:
        problems.append("no unit of kind test named custom in the spec")
    for unit in custom:
        if unit.test is not False:
            problems.append(f"the custom harness's unit has test={unit.test!r}, wanted false (no libtest, no cfg(test))")
    if not normal:
        problems.append("no unit of kind test named normal in the spec")
    for unit in normal:
        if unit.test is not True:
            problems.append(f"the unit of the test with the harness has test={unit.test!r}, wanted true")
    return problems


def harn_cases():
    return [
        Case("base", "test", "a test with a custom harness and one with libtest", expect="base", local=True,
             check=check_harness_units,
             lexpect=[("test", "ok", "ok"), ("line", r"@@custom args=\[\]"), ("exit", 0)]),
        # The custom harness gets exactly the words after `--`.
        Case("nocapture", "test -- --nocapture", "its harness gets `--nocapture`", expect="changed",
             only={"command"}, touches=["command"], check=check_harness_units, local=True,
             lexpect=[("line", r'@@custom args=\["--nocapture"\]'), ("exit", 0)]),
    ]


# 3b. failfast --------------------------------------------------------------------------------------

FAILFAST = {
    "Cargo.toml": pkg_manifest("ff"),
    "Cargo.lock": lock_text([("ff", [])]),
    "src/lib.rs": "/// ```\n/// assert_eq!(ff::one(), 1);\n/// ```\npub fn one() -> u32 {\n    1\n}\n",
    "tests/a_fail.rs": "#[test]\nfn fails() {\n    assert_eq!(ff::one(), 2);\n}\n",
    "tests/b_ok.rs": "#[test]\nfn passes() {\n    assert_eq!(ff::one(), 1);\n}\n",
    **GITIGNORE,
}


def ff_cases():
    return [
        # cargo runs the test binaries in order and stops at the first that fails: b_ok never runs.
        Case("base", "test", "a failing test binary before a passing one", expect="base", local=True,
             lexpect=[("test", "fails", "FAILED"), ("no-test", "passes"), ("exit-nonzero",)]),
        Case("no-fail-fast", "test --no-fail-fast", "keeps going after the failure", expect="changed",
             only={"command"}, touches=["command"], local=True,
             lexpect=[("test", "fails", "FAILED"), ("test", "passes", "ok"), ("exit-nonzero",)]),
    ]


# 4. [env] in the cargo configuration ---------------------------------------------------------------

ENVCFG = {
    "Cargo.toml": pkg_manifest("envcfg"),
    "Cargo.lock": lock_text([("envcfg", [])]),
    ".cargo/config.toml": '[env]\nGREETING = "hi"\n',
    "src/lib.rs": '''pub fn greeting() -> &'static str {
    option_env!("GREETING").unwrap_or("unset")
}

#[cfg(test)]
mod tests {
    // cargo sets [env] for rustc and for the test it runs: what was compiled in is what the
    // program is given, whatever the value, or neither is there.
    #[test]
    fn compile_time_is_run_time() {
        let run = std::env::var("GREETING").unwrap_or_else(|_| "unset".to_string());
        println!("@@envcfg compile={} run={}", super::greeting(), run);
        assert_eq!(super::greeting(), run);
    }
}
''',
    **GITIGNORE,
}


def envcfg_cases():
    base = "test -- --nocapture"
    return [
        Case("base", base, "[env] GREETING = \"hi\"", expect="base", local=True,
             lexpect=[("line", r"@@envcfg compile=hi run=hi"), ("test", "tests::compile_time_is_run_time", "ok"),
                      ("exit", 0)]),
        # config_env is the [env] table, resolved: a value that changed is a different spec.
        Case("hello", base, "GREETING = \"hello\"", expect="changed", only={"config_env"}, touches=["config_env"],
             edits=[("write", ".cargo/config.toml", '[env]\nGREETING = "hello"\n')], local=True,
             lexpect=[("line", r"@@envcfg compile=hello run=hello"), ("exit", 0)]),
        Case("no-env", base, "no [env] at all", expect="changed", only={"config_env"}, touches=["config_env"],
             edits=[("write", ".cargo/config.toml", "# no [env]\n")], local=True,
             lexpect=[("line", r"@@envcfg compile=unset run=unset"), ("exit", 0)]),
        # The spec carries the whole [env], so that a worker with another configuration builds the same.
        Case("second-entry", base, "an [env] entry the source does not read", expect="changed",
             only={"config_env"}, touches=["config_env"],
             edits=[("write", ".cargo/config.toml", '[env]\nGREETING = "hi"\nOTHER = "x"\n')]),
        # `--config` is merged over the files, as cargo merges it.
        Case("cli-config", "test --config 'env.GREETING=\"cli\"' -- --nocapture", "--config sets the same table",
             expect="changed", only={"config_env"}, touches=["config_env"], units=False),
    ]


# 5. profiles ---------------------------------------------------------------------------------------

PROFILES = {
    "Cargo.toml": pkg_manifest("top", '''build = "build.rs"

[dependencies.dep_a]
path = "dep_a"

[dependencies.dep_b]
path = "dep_b"

[dependencies.passthru]
path = "passthru"

[build-dependencies.bs_helper]
path = "bs_helper"
'''),
    "Cargo.lock": lock_text([("bs_helper", []), ("dep_a", []), ("dep_b", ["dep_a"]), ("passthru", ["bs_helper"]),
                             ("top", ["bs_helper", "dep_a", "dep_b", "passthru"])]),
    "build.rs": "fn main() {\n    bs_helper::mark();\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n",
    "src/main.rs": '''#[passthru::passthru]
fn answer() -> u32 {
    dep_b::b() + dep_a::a()
}

fn main() {
    println!("@@bin top answer={}", answer());
}
''',
    "dep_a/Cargo.toml": pkg_manifest("dep_a"),
    "dep_a/src/lib.rs": "pub fn a() -> u32 {\n    1\n}\n",
    "dep_b/Cargo.toml": pkg_manifest("dep_b", '\n[dependencies.dep_a]\npath = "../dep_a"\n'),
    "dep_b/src/lib.rs": "pub fn b() -> u32 {\n    dep_a::a() + 1\n}\n",
    "bs_helper/Cargo.toml": pkg_manifest("bs_helper"),
    "bs_helper/src/lib.rs": "pub fn mark() {}\n",
    "passthru/Cargo.toml": pkg_manifest("passthru", '\n[lib]\nproc-macro = true\n\n[dependencies.bs_helper]\npath = "../bs_helper"\n'),
    "passthru/src/lib.rs": '''use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn passthru(_attr: TokenStream, item: TokenStream) -> TokenStream {
    bs_helper::mark();
    item
}
''',
    **GITIGNORE,
}


def profile_edit(text):
    return [("append", "Cargo.toml", "\n" + text)]


def check_only_profile_changed(select, what):
    """Against the base: exactly the units `select(unit)` selects differ, and only in their
    profile; the spec's own profile is unchanged."""
    def check(results, case):
        problems = []
        res, ref = results.get(case.full), results.get(case.fixture.base().full)
        if not (res and ref and res.spec and ref.spec):
            return problems
        old, new = units_by_identity(ref.spec), units_by_identity(res.spec)
        if set(old) != set(new):
            problems.append("the units are not the same ones as the base's: " + clip(
                f"only in base {sorted(u.label() for k, u in old.items() if k not in new)}, "
                f"only here {sorted(u.label() for k, u in new.items() if k not in old)}"))
            return problems
        differ = sorted(u.label() for k, u in new.items() if u.profile != old[k].profile)
        wanted = sorted(u.label() for u in new.values() if select(u))
        if differ != wanted:
            problems.append(f"the units whose profile differs are {differ}, wanted exactly {wanted} ({what})")
        if canon(spec_field(res.spec, "profile")) != canon(spec_field(ref.spec, "profile")):
            problems.append("the spec's own profile changed; a package or build-override setting is a unit's")
        return problems
    return check


def only_field(profile_field, value, select, what):
    """The selected units have `profile_field == value`."""
    def check(results, case):
        res = results.get(case.full)
        if not (res and res.spec):
            return []
        got = {u.label(): u.profile.get(profile_field) for u in units_of(res.spec) if select(u)}
        return [f"{label}: {profile_field} is {v!r}, wanted {value!r} ({what})" for label, v in got.items() if v != value]
    return check


def differs_field(profile_field, select, what):
    """The selected units' `profile_field` is not what it is in the base."""
    def check(results, case):
        res, ref = results.get(case.full), results.get(case.fixture.base().full)
        if not (res and ref and res.spec and ref.spec):
            return []
        old, new = units_by_identity(ref.spec), units_by_identity(res.spec)
        return [f"{u.label()}: {profile_field} is still {u.profile.get(profile_field)!r} ({what})"
                for k, u in new.items() if select(u) and k in old
                and u.profile.get(profile_field) == old[k].profile.get(profile_field)]
    return check


def both(*checks):
    def check(results, case):
        return [p for c in checks for p in c(results, case)]
    return check


def profiles_cases():
    release = "build --release"
    return [
        Case("base", "build", "a root package with two path dependencies, a build script and a proc macro",
             expect="base", local=True, lexpect=[("program", "bin", "top"), ("exit", 0)]),
        # [b-M4]: [profile.dev.package.dep_a] is dep_a's units' profile, and only theirs.
        Case("pkg-opt1", "build", "[profile.dev.package.dep_a] opt-level = 1", expect="changed",
             edits=profile_edit("[profile.dev.package.dep_a]\nopt-level = 1\n"), touches=["units"],
             only={"units"}, local=True, lexpect=[("program", "bin", "top"), ("exit", 0)],
             check=both(check_only_profile_changed(lambda u: u.package == "dep_a", "dep_a's units"),
                        only_field("opt_level", 1, lambda u: u.package == "dep_a", "the override"))),
        # [b-M4]: a build override is the profile of the host units: build scripts, proc macros and
        # what they use (here bs_helper, which both the build script and the proc macro use).
        Case("build-override", "build", "[profile.dev.build-override] opt-level = 2", expect="changed",
             edits=profile_edit("[profile.dev.build-override]\nopt-level = 2\n"), touches=["units"],
             only={"units"},
             check=both(check_only_profile_changed(lambda u: bool(u.host), "the host units"),
                        only_field("opt_level", 2, lambda u: bool(u.host), "the override"))),
        # `debug = 1` is every target unit's (a build override has its own default for debug).
        Case("debug1", "build", "[profile.dev] debug = 1", expect="changed",
             edits=profile_edit("[profile.dev]\ndebug = 1\n"), touches=["profile"],
             check=differs_field("debug", lambda u: not u.host, "every target unit")),
        Case("panic-abort", "build", "[profile.dev] panic = \"abort\"", expect="changed",
             edits=profile_edit('[profile.dev]\npanic = "abort"\n'), touches=["profile"]),
        Case("overflow-off", "build", "[profile.dev] overflow-checks = false", expect="changed",
             edits=profile_edit("[profile.dev]\noverflow-checks = false\n"), touches=["profile"]),
        Case("debug-assertions-off", "build", "[profile.dev] debug-assertions = false", expect="changed",
             edits=profile_edit("[profile.dev]\ndebug-assertions = false\n"), touches=["profile"]),
        Case("strip", "build", "[profile.dev] strip = \"debuginfo\"", expect="changed",
             edits=profile_edit('[profile.dev]\nstrip = "debuginfo"\n'), touches=["profile"]),
        # cargo reads CARGO_PROFILE_<NAME>_<KEY> as the same setting.
        Case("env-opt1", "build", "CARGO_PROFILE_DEV_OPT_LEVEL=1 is [profile.dev] opt-level = 1", expect="changed",
             env={"CARGO_PROFILE_DEV_OPT_LEVEL": "1"}, touches=["profile"]),
        # What blaze does not use: incremental compilation is rustc's, codegen units are how rustc
        # splits a crate, and blaze compiles function by function. The spec has no field for either
        # (ProfileSpec), so the build is the same.
        Case("incremental", "build", "[profile.dev] incremental = true", expect="same",
             edits=profile_edit("[profile.dev]\nincremental = true\n")),
        Case("codegen-units", "build", "[profile.dev] codegen-units = 4", expect="same",
             edits=profile_edit("[profile.dev]\ncodegen-units = 4\n")),
        # What the spec does not read of a profile that is not the one built: `build` is dev.
        Case("release-lto-unused", "build", "[profile.release] lto = \"thin\", built as dev", expect="same",
             edits=profile_edit('[profile.release]\nlto = "thin"\n')),
        # RA_JIT_MIR_OPT is blaze's (ProfileSpec.mir_opt); 0 is its default (hir-ty-codegen).
        Case("mir0", "build", "RA_JIT_MIR_OPT=0, the default", env={"RA_JIT_MIR_OPT": "0"}, expect="same"),
        Case("mir1", "build", "RA_JIT_MIR_OPT=1", env={"RA_JIT_MIR_OPT": "1"}, expect="changed",
             touches=["profile"], also=[("differs", "mir2")]),
        Case("mir2", "build", "RA_JIT_MIR_OPT=2", env={"RA_JIT_MIR_OPT": "2"}, expect="changed",
             touches=["profile"], also=[("differs", "mir1")]),
        # Cranelift is the default, and RA_JIT_OPT and RA_JIT_EMIT are LLVM's: under Cranelift they
        # change nothing (ProfileSpec.backend_opt and backend_emit are 0 there).
        Case("cranelift", "build", "RA_JIT_BACKEND=cranelift, the default", env={"RA_JIT_BACKEND": "cranelift"},
             expect="same"),
        Case("cranelift-opt1", "build", "RA_JIT_OPT=1 under Cranelift", env={"RA_JIT_OPT": "1"}, expect="same"),
        Case("cranelift-emit-fast", "build", "RA_JIT_EMIT=fast under Cranelift", env={"RA_JIT_EMIT": "fast"},
             expect="same"),
        # A blaze with LLVM carries the backend; one without refuses it by name; either is right.
        Case("llvm", "build", "RA_JIT_BACKEND=llvm", env={"RA_JIT_BACKEND": "llvm"},
             expect="changed-or-refused:RA_JIT_BACKEND"),
        # Under LLVM the level defaults to 2 and the selection to `optimized` (docs/cargo.md).
        Case("llvm-opt2", "build", "RA_JIT_OPT=2, LLVM's default", env={"RA_JIT_BACKEND": "llvm", "RA_JIT_OPT": "2"},
             expect="llvm:same", vs="llvm", llvm_of="llvm"),
        Case("llvm-opt1", "build", "RA_JIT_OPT=1", env={"RA_JIT_BACKEND": "llvm", "RA_JIT_OPT": "1"},
             expect="llvm:changed", vs="llvm", llvm_of="llvm"),
        Case("llvm-emit-fast", "build", "RA_JIT_EMIT=fast", env={"RA_JIT_BACKEND": "llvm", "RA_JIT_EMIT": "fast"},
             expect="llvm:changed", vs="llvm", llvm_of="llvm"),
        Case("llvm-emit-optimized", "build", "RA_JIT_EMIT=optimized, LLVM's default",
             env={"RA_JIT_BACKEND": "llvm", "RA_JIT_EMIT": "optimized"}, expect="llvm:same", vs="llvm", llvm_of="llvm"),
        # 6. lto and the opt-levels blaze does not have are refused by name; "off" and false are not.
        Case("release", release, "the release profile as it is", expect="accepted"),
        Case("lto-thin", release, "[profile.release] lto = \"thin\"", expect="refused:lto",
             edits=profile_edit('[profile.release]\nlto = "thin"\n'), units=False),
        Case("lto-fat", release, "[profile.release] lto = \"fat\"", expect="refused:lto",
             edits=profile_edit('[profile.release]\nlto = "fat"\n'), units=False),
        Case("lto-true", release, "[profile.release] lto = true, which cargo reads as fat", expect="refused:lto",
             edits=profile_edit("[profile.release]\nlto = true\n"), units=False),
        Case("lto-off", release, "[profile.release] lto = \"off\"", expect="accepted",
             edits=profile_edit('[profile.release]\nlto = "off"\n')),
        Case("lto-false", release, "[profile.release] lto = false, cargo's default", expect="accepted",
             edits=profile_edit("[profile.release]\nlto = false\n")),
        Case("env-lto-thin", release, "CARGO_PROFILE_RELEASE_LTO=thin, the same setting", expect="refused:lto",
             env={"CARGO_PROFILE_RELEASE_LTO": "thin"}, units=False),
        Case("opt-s", "build", "[profile.dev] opt-level = \"s\"", expect="refused:opt-level",
             edits=profile_edit('[profile.dev]\nopt-level = "s"\n'), units=False),
        Case("opt-z", "build", "[profile.dev] opt-level = \"z\"", expect="refused:opt-level",
             edits=profile_edit('[profile.dev]\nopt-level = "z"\n'), units=False),
        Case("pkg-opt-s", "build", "[profile.dev.package.dep_a] opt-level = \"s\"", expect="refused:opt-level",
             edits=profile_edit('[profile.dev.package.dep_a]\nopt-level = "s"\n'), units=False),
        # Command-line and environment refusals: what blaze cannot honour is named.
        Case("target", "build --target aarch64-apple-darwin", "--target", expect="refused:--target", units=False),
        Case("z-flag", "build -Z unstable-options", "-Z", expect="refused:-Z", units=False),
        Case("message-format", "build --message-format json", "--message-format", expect="refused:--message-format",
             units=False),
        Case("rustc-wrapper", "build", "RUSTC_WRAPPER", env={"RUSTC_WRAPPER": "sccache"},
             expect="refused:RUSTC_WRAPPER", units=False),
        Case("target-cpu", "build", "-C target-cpu in RUSTFLAGS", env={"RUSTFLAGS": "-C target-cpu=native"},
             expect="refused:target-cpu", units=False),
        Case("linker", "build", "-C linker in RUSTFLAGS", env={"RUSTFLAGS": "-C linker=/bin/cc"},
             expect="refused:linker", units=False),
        Case("build-target-env", "build", "CARGO_BUILD_TARGET", env={"CARGO_BUILD_TARGET": "aarch64-apple-darwin"},
             expect="refused:target", units=False),
    ]


# 7. native tools -----------------------------------------------------------------------------------

NATIVE = {
    "Cargo.toml": pkg_manifest("cdep", 'build = "build.rs"\n'),
    "Cargo.lock": lock_text([("cdep", [])]),
    "c/answer.c": "int cdep_answer(void) {\n    return 42;\n}\n",
    "build.rs": '''use std::{env, process::Command};

// Compiles c/answer.c with the compiler `CC` names (`cc` if it is not set) and archives it with
// `AR` (`ar`), as the `cc` crate's builds do.
fn main() {
    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let ar = env::var("AR").unwrap_or_else(|_| "ar".to_string());
    let out = env::var("OUT_DIR").unwrap();
    let object = format!("{out}/answer.o");
    let lib = format!("{out}/libanswer.a");
    let status = Command::new(&cc).args(["-c", "c/answer.c", "-o", &object]).status().expect("the C compiler runs");
    assert!(status.success(), "the C compiler failed");
    let status = Command::new(&ar).args(["crs", &lib, &object]).status().expect("the archiver runs");
    assert!(status.success(), "the archiver failed");
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=answer");
    println!("cargo:rerun-if-changed=c/answer.c");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=AR");
}
''',
    "src/lib.rs": '''extern "C" {
    fn cdep_answer() -> i32;
}

pub fn answer() -> i32 {
    unsafe { cdep_answer() }
}
''',
    "src/main.rs": 'fn main() {\n    println!("@@bin cdep answer={}", cdep::answer());\n}\n',
    **GITIGNORE,
}

# Two wrapper scripts per tool whose bytes differ, and a copy of the first at another path.
WRAPPERS = ("cc", "cxx", "ar")
REAL = {"cc": "cc", "cxx": "c++", "ar": "ar"}


def native_setup(ctx):
    for place in ("tools", "tools-copy"):
        os.makedirs(os.path.join(ctx.work, place), exist_ok=True)
    for tool in WRAPPERS:
        for n, remark in ((1, "the first wrapper"), (2, "the second wrapper, whose bytes differ")):
            path = os.path.join(ctx.work, "tools", f"{tool}{n}")
            with open(path, "w") as f:
                f.write(f'#!/bin/sh\n# {remark}\nexec {REAL[tool]} "$@"\n')
            os.chmod(path, 0o755)
        shutil.copy2(os.path.join(ctx.work, "tools", f"{tool}1"), os.path.join(ctx.work, "tools-copy", f"{tool}1"))


def tool_path(tool, n, copy=False):
    return "@TOOLS%s@/%s%d" % ("-COPY" if copy else "", tool, n)


def native_skip(ctx):
    for tool in REAL.values():
        if shutil.which(tool) is None:
            return f"no `{tool}` on PATH: the native fixture runs the compilers"
    return None


def native_cases():
    cases = [Case("base", "build", "CC, CXX and AR not set: `cc`, `c++` and `ar`", expect="base", local=True,
                  lexpect=[("program", "bin", "cdep"), ("exit", 0)])]
    for var, tool in (("CC", "cc"), ("CXX", "cxx"), ("AR", "ar")):
        one, two = tool + "1", tool + "2"
        # The platform is what the tools are: a digest of each executable (ExecPlatform.cc, cxx, ar),
        # so wrappers of other bytes are other platforms, and JobContent (the spec's digest) follows.
        cases += [
            Case(one, "build", f"{var} is a wrapper script", env={var: tool_path(tool, 1)}, expect="changed",
                 touches=["platform"],
                 also=[("platform-differs", "base"), ("platform-differs", two), ("job-differs", "base"), ("job-differs", two)]),
            Case(two, "build", f"{var} is a wrapper script of other bytes", env={var: tool_path(tool, 2)},
                 expect="changed", touches=["platform"],
                 also=[("platform-differs", "base"), ("job-differs", "base")]),
            # The same bytes at another path are the same platform; the value of the variable is
            # carried in the environment build scripts read, so the spec and the job differ.
            Case(one + "-copy", "build", f"{var} is a copy of the first wrapper at another path",
                 env={var: tool_path(tool, 1, copy=True)}, vs=one, expect="changed",
                 also=[("platform-same", one), ("job-differs", one)]),
        ]
    # The deployment target is in ExecPlatform.sdk on macOS. Whether it changes a function's code is
    # what C2 records; the platform digest has to tell two targets apart.
    cases += [
        Case("deploy-11", "build", "MACOSX_DEPLOYMENT_TARGET=11.0", env={"MACOSX_DEPLOYMENT_TARGET": "11.0"},
             expect="changed", touches=["platform"],
             skip=lambda ctx: None if sys.platform == "darwin" else "MACOSX_DEPLOYMENT_TARGET is macOS's (the platform has no deployment target on Linux)"),
        Case("deploy-14", "build", "MACOSX_DEPLOYMENT_TARGET=14.0", env={"MACOSX_DEPLOYMENT_TARGET": "14.0"},
             expect="changed", vs="deploy-11", touches=["platform"],
             also=[("platform-differs", "deploy-11")],
             skip=lambda ctx: None if sys.platform == "darwin" else "MACOSX_DEPLOYMENT_TARGET is macOS's (the platform has no deployment target on Linux)"),
    ]
    return cases


# 8. source identity -------------------------------------------------------------------------------

SOURCE = {
    "Cargo.toml": pkg_manifest("source"),
    "Cargo.lock": lock_text([("source", [])]),
    "src/lib.rs": "pub fn value() -> u32 { 1 }\n",
    **GITIGNORE,
}


def source_cases():
    body = [("replace", "src/lib.rs", "{ 1 }", "{ 2 }")]
    return [
        Case("base", "build", "the committed source tree", expect="base", compare_source=True),
        Case("committed-body", "build", "a body edit changes source and job identity without changing the unit graph",
             expect="changed", edits=body, compare_source=True,
             also=[("job-differs", "base"), ("platform-same", "base")]),
        Case("dirty-body", "build", "an uncommitted body edit must not reuse HEAD's spec or job identity",
             expect="changed", edits=body, commit_edits=False, compare_source=True,
             also=[("job-differs", "base"), ("platform-same", "base")]),
        Case("moved-dirty-body", "build", "the same uncommitted inputs at another path have the same source identity",
             expect="same", vs="dirty-body", edits=body, commit_edits=False, compare_source=True, relocate=True),
    ]


# 9. two Cargo workspaces in one Git checkout -------------------------------------------------------

NESTED = dict(GITIGNORE)
for _workspace, _value in (("one", 1), ("two", 2)):
    NESTED.update({
        f"{_workspace}/Cargo.toml": pkg_manifest("nested", "\n[workspace]\n"),
        f"{_workspace}/Cargo.lock": lock_text([("nested", [])]),
        f"{_workspace}/src/lib.rs": f"pub fn value() -> u32 {{ {_value} }}\n"
                                  f"#[test]\nfn workspace_{_workspace}() {{ assert_eq!(value(), {_value}); }}\n",
    })


def nested_cases():
    return [
        Case("base", "test", "the first workspace is below the Git root; replay from the checkout root must select it",
             expect="base", cwd="one", compare_source=True, local=True,
             lexpect=[("test", "workspace_one", "ok"), ("no-test", "workspace_two"), ("exit", 0)]),
        Case("sibling", "test", "the same Git commit, package names and lockfile select different source in the sibling workspace",
             expect="changed", cwd="two", compare_source=True, local=True,
             also=[("job-differs", "base"), ("platform-same", "base")],
             lexpect=[("test", "workspace_two", "ok"), ("no-test", "workspace_one"), ("exit", 0)]),
        Case("below-sibling", "test", "a directory below the sibling workspace must be replayed below that workspace",
             expect="changed", vs="sibling", cwd="two/src", compare_source=True, only={"cwd"},
             touches=["cwd"], local=True,
             lexpect=[("test", "workspace_two", "ok"), ("no-test", "workspace_one"), ("exit", 0)]),
        Case("moved", "test", "relocating the Git checkout preserves the selected nested workspace's identity",
             expect="same", cwd="one", compare_source=True, relocate=True),
    ]


def fixtures():
    return [
        Fixture("basic", BASIC, basic_cases()),
        Fixture("ws", WS, ws_cases()),
        Fixture("harness", HARN, harn_cases()),
        Fixture("failfast", FAILFAST, ff_cases()),
        Fixture("envcfg", ENVCFG, envcfg_cases()),
        Fixture("profiles", PROFILES, profiles_cases()),
        Fixture("native", NATIVE, native_cases(), setup=native_setup, skip=native_skip),
        Fixture("source", SOURCE, source_cases()),
        Fixture("nested", NESTED, nested_cases()),
    ]


# --- Running ra-jit under the guard --------------------------------------------------------------

class Run:
    def __init__(self, argv, code, out, err, seconds, timed_out=False, watchdog=None):
        self.argv, self.code, self.out, self.err = argv, code, out, err
        self.seconds, self.timed_out, self.watchdog = seconds, timed_out, watchdog


class Blaze:
    """Every run of the ra-jit under test: the guard's environment (cannot be turned off), the
    watchdog of tracedecay.py over the run's whole process tree, a fresh RA_JIT_HOME for the ones
    that start a daemon, and a timeout."""

    def __init__(self, binary, run_id, timeout):
        self.bin = os.path.abspath(binary)
        self.bindir = os.path.dirname(self.bin)
        self.run_id = run_id
        self.timeout = timeout
        self.guard = dict(tracedecay.GUARD_ENV)
        self.homes = []
        self.watchdog = tracedecay.Watchdog(self.bindir, tracedecay.WATCHDOG_MB, tracedecay.SWAP_FREE_MIN_MB, 2.0,
                                            run_id=run_id, homes=())
        self.watchdog.start()

    def new_home(self):
        """A directory for a daemon's socket and state. Under /tmp: a socket's path is short (104
        bytes on macOS) and $TMPDIR's is not."""
        home = tempfile.mkdtemp(prefix="rj-bs-", dir="/tmp")
        os.chmod(home, 0o700)
        self.homes.append(home)
        self.watchdog.homes = tuple(self.homes)
        return home

    def env(self, base, home, target=None, store=None, cargo_home=None, standins=None):
        env = dict(base)
        env.update(self.guard)
        env[tracedecay.RUN_ID_ENV] = self.run_id
        env["RA_JIT_HOME"] = home
        if target:
            env["CARGO_TARGET_DIR"] = target
        if store:
            env["RA_JIT_CACHE"] = store
        if cargo_home:
            env["CARGO_HOME"] = cargo_home
        if standins:
            env["PATH"] = standins + os.pathsep + self.bindir + os.pathsep + env.get("PATH", "")
            for name, exe in (("RUSTC", "rustc"), ("RUSTDOC", "rustdoc"), ("CARGO", "cargo")):
                env[name] = os.path.join(standins, exe)
        return env

    def run(self, args, cwd, env, merge=False, timeout=None):
        argv = [self.bin, *args]
        began = time.monotonic()
        self.watchdog.take()
        proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT if merge else subprocess.PIPE, text=True,
                                errors="replace", start_new_session=True)
        self.watchdog.client = lambda: proc.pid
        timed_out = False
        try:
            out, err = proc.communicate(timeout=timeout or self.timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            with contextlib.suppress(ProcessLookupError, PermissionError):
                os.killpg(proc.pid, signal.SIGKILL)
            out, err = proc.communicate()
        finally:
            self.watchdog.client = None
        trip = self.watchdog.take().get("watchdog")
        return Run(argv, proc.returncode, out or "", err or "", time.monotonic() - began, timed_out, trip)

    def stop(self, home, cwd, env):
        """`ra-jit cargo stop` for the daemon of `home`."""
        return self.run(["cargo", "stop"], cwd, dict(env, RA_JIT_HOME=home), merge=True, timeout=120)

    def close(self):
        self.watchdog.stop.set()
        for home in self.homes:
            shutil.rmtree(home, ignore_errors=True)


# --- A spec command's result ---------------------------------------------------------------------

class CaseResult:
    """What `ra-jit cargo spec` said for one case, parsed, with what is wrong with how it said it."""

    def __init__(self, case, run, tree, cbor):
        self.case, self.run, self.tree, self.cbor = case, run, tree, cbor
        self.obj = self.refused = self.digests = self.summary = self.spec = self.platform = None
        self.error = None
        self.problems = []
        self.second = None
        self._parse()

    def _parse(self):
        run = self.run
        if run.watchdog:
            self.error = "the watchdog killed it: " + run.watchdog["reason"]
            return
        if run.timed_out:
            self.error = f"timed out after {run.seconds:.0f} s"
            return
        if run.code not in (0, 2):
            self.error = f"exit {run.code}: {clip(run.err or run.out, 300)}"
            return
        try:
            obj = json.loads(run.out)
        except ValueError as e:
            self.error = f"stdout is not one JSON object ({e}): {clip(run.out, 200)}"
            return
        if not isinstance(obj, dict):
            self.error = "stdout is not a JSON object"
            return
        self.obj = obj
        self.refused, self.digests = obj.get("refused"), obj.get("digests")
        self.summary, self.platform = obj.get("summary"), obj.get("platform")
        self.spec = unwrap(obj.get("spec")) if obj.get("spec") is not None else None
        missing = [k for k in ("refused", "digests", "summary", "spec", "platform") if k not in obj]
        if missing:
            self.problems.append("the object has no " + ", ".join(missing))
        if run.code == 2 and not self.refused:
            self.problems.append("exit 2 and `refused` is null")
        if run.code == 0 and self.refused:
            self.problems.append("exit 0 and `refused` is set")
        if self.refused and not (isinstance(self.refused, dict) and self.refused.get("what") and self.refused.get("why")):
            self.problems.append("`refused` has no `what` and `why`")
        if not self.refused:
            if not isinstance(self.digests, dict) or any(not re.fullmatch(r"[0-9a-f]{64}", str(self.digests.get(k, ""))) for k in DIGESTS):
                self.problems.append("`digests` lacks a 64-hex digest of each of " + ", ".join(DIGESTS))
            elif self.spec is None:
                self.problems.append("a spec was made and `spec` is null")
            else:
                if isinstance(self.summary, dict):
                    if self.summary.get("command") != self.case.command:
                        self.problems.append(f"summary.command is {self.summary.get('command')!r}, wanted {self.case.command!r}")
                    if isinstance(self.summary.get("units"), int) and self.summary["units"] != len(spec_field(self.spec, "units") or []):
                        self.problems.append(f"summary.units is {self.summary['units']} and the spec has {len(spec_field(self.spec, 'units') or [])}")
                name, _ = command_of(self.spec)
                if name != self.case.command:
                    self.problems.append(f"the spec's command is {name!r}, wanted {self.case.command!r}")

    @property
    def made(self):
        return self.error is None and not self.refused and self.spec is not None

    @property
    def digest(self):
        return (self.digests or {}).get("spec") if self.made else None

    def asked_packages(self):
        return {a.split("@", 1)[0] for a in (self.summary or {}).get("asked", [])}


class Delta:
    """How one spec differs from another."""

    def __init__(self, a, b, compare_source=False):
        self.same_source = canon(spec_field(a.spec, "source")) == canon(spec_field(b.spec, "source"))
        self.fp_same = fingerprint(a.spec) == fingerprint(b.spec)
        self.digest_same = a.digest == b.digest
        self.platform_same = (a.digests or {}).get("platform") == (b.digests or {}).get("platform")
        self.job_same = (a.digests or {}).get("job_content") == (b.digests or {}).get("job_content")
        self.fields = changed_fields(a.spec, b.spec, compare_source=compare_source)
        # Configuration comparisons leave differing sources out. Source identity comparisons
        # include them, so a source-only fix cannot be mistaken for an unchanged spec.
        self.same = self.digest_same if compare_source or self.same_source else self.fp_same
        rendering_same = canon(a.spec) == canon(b.spec) if compare_source else self.fp_same
        self.inconsistent = None
        if (compare_source or self.same_source) and self.digest_same != rendering_same:
            self.inconsistent = ("the spec digests are equal and the JSON renderings differ in "
                                 + ", ".join(sorted(self.fields)) if self.digest_same else
                                 "the spec digests differ and the JSON renderings are equal")


# --- The comparison with cargo's unit graph (arm 8) ----------------------------------------------

def pkg_of(pkg_id):
    """(name, version) of a cargo package id, whatever its spelling of the path: `path+file:///x/p#0.1.0`
    names the package after the directory, `...#name@0.1.0` names it."""
    url, _, frag = pkg_id.partition("#")
    if "@" in frag:
        name, version = frag.split("@", 1)
    else:
        name, version = url.rstrip("/").rsplit("/", 1)[-1], frag
    return name, version


def graph_keys(graph, doctest=False):
    """{key: unit} and the roots as keys. The key is what the task names: package, target kind and
    name, mode, features, platform, profile. Units cargo prints in `doctest` mode are left out
    (blaze-metadata's Mode has none; doc tests are RunSpec.doc)."""
    units = graph.get("units", [])
    keep = [i for i, u in enumerate(units) if doctest or u.get("mode") != "doctest"]
    keys, counts = {}, collections.Counter()
    for i in keep:
        u = units[i]
        name, version = pkg_of(u.get("pkg_id", ""))
        t = u.get("target", {})
        base = (name, version, tuple(t.get("kind", [])), t.get("name"), u.get("mode"),
                tuple(sorted(u.get("features", []))), u.get("platform"), canon(u.get("profile")))
        n = counts[base]
        counts[base] += 1
        keys[i] = base + (n,)
    return units, keys


def compare_graphs(blaze, cargo, doctest=False):
    """The differences between two unit graphs as text; none means equal."""
    diffs = []
    bu, bk = graph_keys(blaze, doctest)
    cu, ck = graph_keys(cargo, doctest)
    if blaze.get("version") != cargo.get("version"):
        diffs.append(f"version {blaze.get('version')!r}, cargo's {cargo.get('version')!r}")

    def detail(units, keys, i):
        u = units[i]
        t = u.get("target", {})
        deps = sorted((keys.get(d["index"], ("<doctest>",)), d.get("extern_crate_name"), bool(d.get("public")),
                       bool(d.get("noprelude"))) for d in u.get("dependencies", []) if d["index"] in keys)
        tgt = {k: (sorted(v) if k == "crate_types" else v) for k, v in t.items() if k != "src_path"}
        return {"target": tgt, "is_std": bool(u.get("is_std")), "dependencies": deps}

    def label(key):
        return f"{key[0]}@{key[1]} {'/'.join(key[2])}:{key[3]} {key[4]}"

    by_blaze = {k: i for i, k in bk.items()}
    by_cargo = {k: i for i, k in ck.items()}
    for key in sorted(set(by_blaze) & set(by_cargo), key=str):
        a, b = detail(bu, bk, by_blaze[key]), detail(cu, ck, by_cargo[key])
        if a != b:
            for part in a:
                if a[part] != b[part]:
                    diffs.append(f"{label(key)}: {part}: blaze {clip(a[part], 160)} / cargo {clip(b[part], 160)}")
    only_blaze = sorted((k for k in by_blaze if k not in by_cargo), key=str)
    only_cargo = sorted((k for k in by_cargo if k not in by_blaze), key=str)
    # Pair what is left by package, target, mode and platform, to say what differs of it.
    reduce = lambda k: (k[0], k[1], k[2], k[3], k[4], k[6])
    for kb in list(only_blaze):
        mates = [kc for kc in only_cargo if reduce(kc) == reduce(kb)]
        if len(mates) == 1 and sum(reduce(k) == reduce(kb) for k in only_blaze) == 1:
            kc = mates[0]
            why = []
            if kb[5] != kc[5]:
                why.append(f"features blaze {list(kb[5])} / cargo {list(kc[5])}")
            if kb[7] != kc[7]:
                pb, pc = json.loads(kb[7]), json.loads(kc[7])
                why.append("profile " + ", ".join(f"{k} blaze {pb.get(k)!r} / cargo {pc.get(k)!r}"
                                                  for k in sorted(set(pb) | set(pc)) if pb.get(k) != pc.get(k)))
            diffs.append(f"{label(kb)}: " + "; ".join(why))
            only_blaze.remove(kb)
            only_cargo.remove(kc)
    diffs += [f"only in blaze: {label(k)} features {list(k[5])}" for k in only_blaze]
    diffs += [f"only in cargo: {label(k)} features {list(k[5])}" for k in only_cargo]
    rb = sorted(bk[i] for i in blaze.get("roots", []) if i in bk)
    rc = sorted(ck[i] for i in cargo.get("roots", []) if i in ck)
    if rb != rc:
        diffs.append(f"roots: blaze {[label(k) for k in rb]} / cargo {[label(k) for k in rc]}")
    return diffs


# --- What a local run did (arm 9) ----------------------------------------------------------------

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
HASH = re.compile(r"-[0-9a-f]{16}$")
RUNNING = re.compile(r"^\s*Running (.*?)(?: \((.+)\))?\s*$")
DOCTESTS = re.compile(r"^\s*Doc-tests (\S+)")
TEST_LINE = re.compile(r"^test (.+?) \.\.\. (.*)$")
RESULT = re.compile(r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored")
OUTCOME = re.compile(r"^(ok|FAILED|ignored|bench)\b")
EXE_SKIP = (".d", ".rlib", ".rmeta", ".dylib", ".so", ".a", ".o", ".dSYM", ".pdb", ".exe", ".dll", ".lib", ".json")


def parse_run(text):
    """From the output of a `cargo test` or `cargo run` (and what stands in for it): the tests
    libtest ran as (binary, test, outcome), the totals of each binary, the doc tests, and the
    lines the fixtures' programs print (`@@...`). Paths, hashes and timings are not kept."""
    text = ANSI.sub("", text)
    exe, docs_of, pending = "?", None, None
    tests, totals, docs, markers = [], [], [], []
    for line in text.splitlines():
        if "@@" in line:  # a program's line; under --nocapture libtest has it follow `test x ... `
            markers.append(line[line.index("@@"):].rstrip())
            line = line[:line.index("@@")]
            if not line.strip():
                continue
        m = RUNNING.match(line)
        if m and not line.lstrip().startswith("Running `"):
            exe, docs_of = m.group(1).strip(), None
            continue
        m = DOCTESTS.match(line)
        if m:
            exe, docs_of = "doc:" + m.group(1), m.group(1)
            continue
        if pending and OUTCOME.match(line.strip()):
            outcome = OUTCOME.match(line.strip()).group(1)
            (docs if docs_of else tests).append((exe, pending, outcome))
            pending = None
            continue
        m = TEST_LINE.match(line)
        if m:
            name, rest = m.group(1), m.group(2).strip()
            o = OUTCOME.match(rest)
            if o:
                (docs if docs_of else tests).append((exe, name, o.group(1)))
            else:
                pending = name
            continue
        m = RESULT.match(line)
        if m:
            totals.append((exe, int(m.group(1)), int(m.group(2)), int(m.group(3))))
    return {"tests": sorted(tests), "totals": sorted(totals), "docs": sorted(docs), "markers": sorted(markers)}


def scan_programs(target):
    """The programs a build left under CARGO_TARGET_DIR/<profile>/: `bin:` at the top, `example:` in
    examples/, `exe:` in deps/ (test binaries and the binaries' own copies, the hash dropped, each as
    often as it is there)."""
    found = collections.Counter()
    if not os.path.isdir(target):
        return []
    examples = set()
    for profile in sorted(os.listdir(target)):
        base = os.path.join(target, profile)
        if profile.startswith(".") or not os.path.isdir(os.path.join(base, "deps")):
            continue
        for sub, kind in (("", "bin"), ("examples", "example"), ("deps", "exe")):
            directory = os.path.join(base, sub) if sub else base
            if not os.path.isdir(directory):
                continue
            for name in sorted(os.listdir(directory)):
                path = os.path.join(directory, name)
                if (name.startswith(".") or name.endswith(EXE_SKIP) or "." in name or not os.path.isfile(path)
                        or not os.access(path, os.X_OK)):
                    continue
                if kind == "example":  # cargo has `demo` and `demo-<hash>` side by side: one program
                    examples.add(HASH.sub("", name))
                    continue
                found[f"{kind}:{HASH.sub('', name) if kind == 'exe' else name}"] += 1
    return sorted([f"{k} x{n}" if n > 1 else k for k, n in found.items()] + [f"example:{e}" for e in examples])


def summarize(output, target, code):
    got = parse_run(output)
    got["programs"] = scan_programs(target)
    got["exit"] = code
    return got


def check_lexpect(summary, expect):
    """What the command's own run has to show: ("test", name, outcome), ("no-test", name) (not
    run, or ignored), ("program", kind, name), ("no-program", kind, name), ("line", regex) of a
    program's `@@` lines, ("exit", n) and ("exit-nonzero",)."""
    problems = []
    named = lambda t, n: t[1] == n or t[1].endswith("::" + n)
    for item in expect:
        kind = item[0]
        if kind == "test":
            if not any(named(t, item[1]) and t[2] == item[2] for t in summary["tests"]):
                problems.append(f"no test {item[1]} ended {item[2]}: ran {[(t[1], t[2]) for t in summary['tests']]}")
        elif kind == "no-test":
            ran = [t for t in summary["tests"] if named(t, item[1]) and t[2] != "ignored"]
            if ran:
                problems.append(f"test {item[1]} ran ({ran[0][2]}) and should not have")
        elif kind == "program":
            if f"{item[1]}:{item[2]}" not in [p.split(" x")[0] for p in summary["programs"]]:
                problems.append(f"no {item[1]} {item[2]} was built: {summary['programs']}")
        elif kind == "no-program":
            if f"{item[1]}:{item[2]}" in [p.split(" x")[0] for p in summary["programs"]]:
                problems.append(f"a {item[1]} {item[2]} was built and should not have been")
        elif kind == "line":
            if not any(re.search(item[1], m) for m in summary["markers"]):
                problems.append(f"no line of the programs matches {item[1]}: {summary['markers']}")
        elif kind == "exit":
            if summary["exit"] != item[1]:
                problems.append(f"exit {summary['exit']}, wanted {item[1]}")
        elif kind == "exit-nonzero":
            if summary["exit"] == 0:
                problems.append("exit 0, wanted a failure")
        else:
            raise HarnessError(f"an expectation of kind {kind!r}")
    return problems


def compare_summaries(a, b):
    """The parts in which the command's run and the spec's run differ: [(part, command, spec)]."""
    return [(part, a[part], b[part]) for part in ("programs", "tests", "totals", "docs", "markers", "exit")
            if a[part] != b[part]]


def count_dispatched(ctx_audit, offset):
    """How many runs of cargo, rustc and rustdoc the stand-ins logged since `offset`."""
    if ctx_audit is None:
        return None
    log, _ = ctx_audit
    try:
        with open(log, "rb") as f:
            f.seek(offset)
            data = f.read()
    except OSError:
        return {"cargo": 0, "rustc": 0, "rustdoc": 0}
    slice_path = log + ".slice"
    with open(slice_path, "wb") as f:
        f.write(data)
    try:
        runs = norustc.runs(slice_path)
    finally:
        os.unlink(slice_path)
    return {tool: sum(1 for r in runs if r["tool"] == tool) for tool in ("cargo", "rustc", "rustdoc")}


# --- The harness ---------------------------------------------------------------------------------

class Ctx:
    def __init__(self, args, fixture_list, quiet=False):
        self.args = args
        self.fixtures = fixture_list
        self.quiet = quiet
        self.made_root = args.work is None
        self.work = os.path.abspath(args.work) if args.work else tempfile.mkdtemp(prefix="rj-bs-work-", dir="/tmp")
        os.makedirs(self.work, exist_ok=True)
        self.run_id = "bs-%s-%d" % (time.strftime("%m%d%H%M%S"), os.getpid())
        self.blaze = Blaze(args.bin, self.run_id, args.timeout)
        self.cargo_home = os.path.join(self.work, "cargo-home")
        os.makedirs(self.cargo_home, exist_ok=True)
        self.spec_home = self.blaze.new_home()
        self.trees, self.results, self.units, self.locals, self.skipped = {}, {}, {}, {}, {}
        self.mismatch = []
        self.audit = None  # (log, standins bin) or None
        self.audit_why = None

    def say(self, *a):
        if not self.quiet:
            print(*a, flush=True)

    # -- fixtures
    def base_tree(self, fixture):
        key = (fixture.name, "base")
        if key not in self.trees:
            directory = os.path.join(self.work, "fix", fixture.name, "base")
            write_files(directory, fixture.files)
            subprocess.run(["git", "init", "-q", directory], env=dict(os.environ, **GIT_ENV), check=True,
                           stdin=subprocess.DEVNULL)
            git(directory, "symbolic-ref", "HEAD", "refs/heads/main")
            commit_all(directory, "fixture " + fixture.name)
            self.trees[key] = directory
        return self.trees[key]

    def tree(self, case):
        """The directory of the checkout the case runs in: the base's, or a copy of it with the case's
        edits (committed unless requested otherwise), or a copy at another path."""
        key = (case.fixture.name, case.name)
        if key in self.trees:
            return self.trees[key]
        base = self.base_tree(case.fixture)
        if case.relocate:
            dest = os.path.join(self.work, "fix", case.fixture.name, "moved-" + case.name, "deeper", "clone")
        elif case.edits:
            dest = os.path.join(self.work, "fix", case.fixture.name, "edited-" + case.name)
        else:
            self.trees[key] = base
            return base
        shutil.copytree(base, dest, symlinks=True)
        for edit in case.edits:
            apply_edit(dest, edit)
        if case.edits and case.commit_edits:
            commit_all(dest, "case " + case.name)
        self.trees[key] = dest
        return dest

    def case_env(self, case):
        out = {}
        for name, value in case.env.items():
            if value is not None:
                value = value.replace("@TOOLS-COPY@", os.path.join(self.work, "tools-copy")).replace(
                    "@TOOLS@", os.path.join(self.work, "tools"))
            out[name] = value
        return out

    # -- the spec arm
    def spec_env(self, case):
        return self.blaze.env(scrubbed_env(self.case_env(case)), home=self.spec_home, cargo_home=self.cargo_home)

    def run_spec(self, case, with_cbor):
        tree = self.tree(case)
        cbor = None
        args = ["cargo", "spec"]
        if with_cbor:
            cbor = os.path.join(self.work, "specs", f"{case.fixture.name}__{case.name}.cbor")
            os.makedirs(os.path.dirname(cbor), exist_ok=True)
            args += ["--out", cbor]
        run = self.blaze.run(args + case.words, os.path.join(tree, case.cwd), self.spec_env(case), timeout=SPEC_TIMEOUT)
        res = CaseResult(case, run, tree, cbor)
        if case.expect == "base" and res.error is None:
            again = self.blaze.run(["cargo", "spec"] + case.words, os.path.join(tree, case.cwd), self.spec_env(case),
                                   timeout=SPEC_TIMEOUT)
            res.second = again
        return res

    def judge(self, case):
        """(observed, failures) for the case's spec."""
        res, fails = self.results[case.full], []
        if res.error:
            return "ERROR", [f"no spec: {res.error}"]
        fails += res.problems
        exp = case.expect

        def other(name):
            return self.results.get(f"{case.fixture.name}/{name}")

        ref = other(case.vs or "base") if exp != "base" else None
        if res.refused:
            observed = "refused: " + str(res.refused.get("what"))
        elif exp == "base":
            observed = "base"
        else:
            observed = "accepted"
        if case.llvm_of:
            llvm = other(case.llvm_of)
            if llvm is None or llvm.error:
                return observed, fails + ["the case it depends on (llvm) has no spec"]
            if llvm.refused:
                exp = "refused:RA_JIT_BACKEND"
            else:
                exp = exp.split(":", 1)[1]
        if exp.startswith("changed-or-refused:"):
            want = exp.split(":", 1)[1]
            if res.refused:
                exp = "refused:" + want
            else:
                exp = "changed"
        if exp.startswith("refused:"):
            want = exp.split(":", 1)[1].lower()
            if not res.refused:
                fails.append(f"wanted a refusal naming {want!r}, and a spec was made")
            elif want not in str(res.refused.get("what", "")).lower():
                fails.append(f"refused for {res.refused.get('what')!r}, wanted a refusal naming {want!r} ({res.refused.get('why')})")
            if res.run.code != 2:
                fails.append(f"a refusal has to exit 2, it exited {res.run.code}")
            return observed, fails
        if res.refused:
            fails.append(f"refused ({res.refused.get('what')}: {res.refused.get('why')}), and a spec was wanted")
            return observed, fails
        if exp == "base":
            if res.second is not None and res.second.out != res.run.out:
                fails.append("two runs of one command printed different specs (no randomness, time or path may be in a spec)")
        elif exp in ("same", "changed"):
            if ref is None or ref.error or ref.refused:
                fails.append(f"it is compared with {case.vs or 'base'}, which made no spec")
                return observed, fails
            delta = Delta(res, ref, compare_source=case.compare_source)
            fp = "" if case.compare_source or delta.same_source else "*"
            if delta.inconsistent:
                fails.append(delta.inconsistent)
            if exp == "same":
                observed = "same" + fp
                if not delta.same:
                    observed = "changed(" + ",".join(sorted(delta.fields) or ["digest only"]) + ")" + fp
                    fails.append(f"wanted the same spec as {case.vs or 'base'}; it differs in {sorted(delta.fields) or 'its digest only'}")
                elif not delta.platform_same:
                    fails.append("the specs are equal and their platform digests differ")
            else:
                observed = "changed(" + ",".join(sorted(delta.fields) or ["digest only"]) + ")" + fp if not delta.same else "same" + fp
                if delta.same:
                    fails.append(f"wanted a different spec from {case.vs or 'base'}, and it is the same")
                else:
                    if case.only is not None and not delta.fields <= set(case.only):
                        fails.append(f"it differs in {sorted(delta.fields - set(case.only))}, which a {case.words[0]} with this flag does not change (only {sorted(case.only)} may)")
                    if case.touches and not set(case.touches) <= delta.fields:
                        fails.append(f"it does not differ in {sorted(set(case.touches) - delta.fields)} (differs in {sorted(delta.fields)})")
        for relation, name in ((r[0], r[1]) for r in case.also):
            mate = other(name)
            if mate is None or mate.error or mate.refused:
                fails.append(f"{relation} {name}: that case made no spec")
                continue
            d = Delta(res, mate, compare_source=case.compare_source)
            ok = {"same": d.same, "differs": not d.same, "platform-same": d.platform_same,
                  "platform-differs": not d.platform_same, "job-differs": not d.job_same,
                  "units-equal": canon(spec_field(res.spec, "units")) == canon(spec_field(mate.spec, "units")),
                  "asked-same": canon(spec_field(res.spec, "asked")) == canon(spec_field(mate.spec, "asked")),
                  "asked-differs": canon(spec_field(res.spec, "asked")) != canon(spec_field(mate.spec, "asked"))}[relation]
            if not ok:
                fails.append(f"it should be {relation} with {name}, and is not (differs in {sorted(d.fields)})")
        if case.asked_pkgs is not None and res.asked_packages() != set(case.asked_pkgs):
            fails.append(f"summary.asked names {sorted(res.asked_packages())}, wanted {sorted(case.asked_pkgs)}")
        if case.check:
            fails += case.check(self.results, case)
        return observed, fails

    # -- the unit graphs
    def run_units(self, case):
        res = self.results[case.full]
        if case.units is False or case.command == "run":
            return {"status": "n/a", "note": "run has no unit graph in cargo" if case.command == "run" else "refused: no graph"}
        if not res.made:
            return {"status": "n/a", "note": "no spec"}
        tree = os.path.join(self.tree(case), case.cwd)
        ran = self.blaze.run(["cargo", "spec", "--unit-graph"] + case.words, tree, self.spec_env(case), timeout=SPEC_TIMEOUT)
        if ran.code != 0:
            return {"status": "ERROR", "diffs": [f"blaze: exit {ran.code}: {clip(ran.err or ran.out, 300)}"]}
        try:
            theirs = json.loads(ran.out)
        except ValueError as e:
            return {"status": "ERROR", "diffs": [f"blaze: stdout is not JSON ({e})"]}
        env = scrubbed_env(self.case_env(case))
        env.update(RUSTC_BOOTSTRAP="1", CARGO_HOME=self.cargo_home)
        cargo = subprocess.run(["cargo", f"+{TOOLCHAIN}", case.command, "--unit-graph", "-Z", "unstable-options",
                                "--locked", "--offline", *case.words[1:]], cwd=tree, env=env, text=True,
                               stdin=subprocess.DEVNULL, capture_output=True, timeout=300)
        if cargo.returncode != 0:
            return {"status": "ERROR", "diffs": [f"the oracle, cargo {case.command} --unit-graph, failed: {clip(cargo.stderr, 300)}"]}
        try:
            ours = json.loads(cargo.stdout)
        except ValueError as e:
            return {"status": "ERROR", "diffs": [f"cargo's unit graph is not JSON ({e})"]}
        diffs = compare_graphs(theirs, ours, doctest=self.args.units_with_doctest)
        return {"status": "DIFF" if diffs else "equal", "diffs": diffs, "units": len(theirs.get("units", []))}

    # -- the local runs
    def audit_offset(self):
        try:
            return os.path.getsize(self.audit[0]) if self.audit else 0
        except OSError:
            return 0

    def start_audit(self):
        if self.args.no_audit:
            self.audit_why = "--no-audit"
            return
        try:
            log = os.path.join(self.work, "audit.log")
            open(log, "w").close()
            standins = os.path.join(norustc.stand_ins(os.path.join(self.work, "standins"), log), "bin")
            self.audit = (log, standins)
        except Exception as e:  # no rustup toolchain, no rustc to copy: the audit is skipped, said so
            self.audit_why = f"the stand-ins could not be made: {clip(e, 160)}"

    def local_run(self, case, args, cwd_in, env_extra, label, shared_store):
        """One run of ra-jit in a fresh copy: returns (summary, run, dispatched)."""
        copy_root, local = cwd_in
        home = self.blaze.new_home()
        target = os.path.join(local, "target")
        store = shared_store or os.path.join(local, "store")
        env = self.blaze.env(scrubbed_env(env_extra), home=home, target=target, store=store,
                             cargo_home=self.cargo_home, standins=self.audit[1] if self.audit else None)
        offset = self.audit_offset()
        run = self.blaze.run(args, copy_root, env, merge=True)
        if self.audit and not run.watchdog:
            norustc.settle(self.audit[0], limit=60)
        self.blaze.stop(home, copy_root, env)
        dispatched = count_dispatched(self.audit, offset)
        summary = summarize(run.out, target, run.code)
        return summary, run, dispatched

    def run_local(self, case):
        res = self.results[case.full]
        if not res.made or not res.cbor or not os.path.exists(res.cbor):
            return {"status": "ERROR", "diffs": ["no spec file to run (the spec arm failed first)"]}
        local = os.path.join(self.work, "local", f"{case.fixture.name}__{case.name}")
        out = {"status": "match", "diffs": []}
        try:
            copy_a, copy_b = os.path.join(local, "a"), os.path.join(local, "b")
            for dest in (copy_a, copy_b):
                shutil.copytree(res.tree, dest, symlinks=True)
            at = lambda root: os.path.join(root, case.cwd) if case.cwd else root
            shared = os.path.join(self.work, "store") if self.args.shared_store else None
            sa, ra, da = self.local_run(case, ["cargo", *case.words], (at(copy_a), os.path.join(local, "run-a")),
                                        self.case_env(case), "command", shared)
            where = copy_b if self.args.spec_run_in == "root" else at(copy_b)
            sb, rb, db = self.local_run(case, ["cargo", "spec", "--run", res.cbor], (where, os.path.join(local, "run-b")),
                                        {}, "spec run", shared)
        finally:
            if not self.args.keep:
                shutil.rmtree(local, ignore_errors=True)
        out["dispatched"] = {"command": da, "spec_run": db}
        out["command"], out["spec_run"] = sa, sb
        out["exit"] = {"command": ra.code, "spec_run": rb.code}
        for which, run in (("the command", ra), ("the spec run", rb)):
            if run.watchdog:
                out["status"] = "KILLED"
                out["diffs"].append(f"{which} was killed by the watchdog: {run.watchdog['reason']}")
            elif run.timed_out:
                out["status"] = "ERROR"
                out["diffs"].append(f"{which} timed out after {run.seconds:.0f} s")
        if out["status"] != "match":
            return out
        if rb.code == 2 and "does not match the spec" in rb.out:
            out["status"] = "MISMATCH"
            out["diffs"].append("the spec run refused its own tree: " + clip(rb.out.strip().splitlines()[-1], 200))
            return out
        for part, a, b in compare_summaries(sa, sb):
            out["diffs"].append(f"{part}: command {clip(a, 200)} / spec run {clip(b, 200)}")
        out["diffs"] += check_lexpect(sa, case.lexpect)
        if out["diffs"]:
            out["status"] = "MISMATCH"
            out["tail"] = {"command": clip(ra.out[-600:], 600), "spec_run": clip(rb.out[-600:], 600)}
        return out

    def run_mismatch(self, fixture):
        """`spec run` of the base's spec in the checkout of another commit refuses, naming a field."""
        base = self.results.get(f"{fixture.name}/base")
        edited = [c for c in fixture.cases if c.edits and self.results.get(c.full) and self.results[c.full].made]
        if not (base and base.cbor and os.path.exists(base.cbor) and edited):
            return None
        case = edited[0]
        copy = os.path.join(self.work, "local", f"{fixture.name}__mismatch")
        shutil.copytree(self.tree(case), copy, symlinks=True)
        home = self.blaze.new_home()
        env = self.blaze.env(scrubbed_env(), home=home, target=os.path.join(copy, "..", "target-mm"),
                             cargo_home=self.cargo_home)
        run = self.blaze.run(["cargo", "spec", "--run", base.cbor], copy, env, merge=True)
        self.blaze.stop(home, copy, env)
        shutil.rmtree(copy, ignore_errors=True)
        said = run.out.strip().splitlines()[-1] if run.out.strip() else ""
        fails = []
        if run.code != 2:
            fails.append(f"spec run of {fixture.name}/base's spec in the checkout of {case.name} exited {run.code}, wanted 2")
        if not re.search(r"does not match the spec: \S+", run.out):
            fails.append(f"it did not say `the tree does not match the spec: <field>`: {clip(said, 200)}")
        return {"case": f"{fixture.name}/{case.name}", "exit": run.code, "said": said, "fails": fails}


# --- Selecting, running every arm, reporting -----------------------------------------------------

def select_cases(fixture_list, only):
    """(reported, to_run): the cases named by --only (all with none), and what they are compared
    with or need, which runs too."""
    by_full = {c.full: c for f in fixture_list for c in f.cases}
    names = {f.name for f in fixture_list}
    reported = []
    if not only:
        reported = [c for f in fixture_list for c in f.cases]
    for want in only or ():
        if want in names:
            reported += [c for c in next(f for f in fixture_list if f.name == want).cases]
        elif want in by_full:
            reported.append(by_full[want])
        else:
            raise HarnessError(f"--only {want}: no such fixture or case (--list shows them)")
    needed, queue = {}, list(reported)
    while queue:
        case = queue.pop()
        if case.full in needed:
            continue
        needed[case.full] = case
        for name in ["base", case.vs, case.llvm_of] + [r[1] for r in case.also]:
            mate = by_full.get(f"{case.fixture.name}/{name}") if name else None
            if mate:
                queue.append(mate)
    to_run = [c for f in fixture_list for c in f.cases if c.full in needed]
    seen, reported_unique = set(), []
    for c in reported:
        if c.full not in seen:
            seen.add(c.full)
            reported_unique.append(c)
    order = {c.full: i for i, c in enumerate(c for f in fixture_list for c in f.cases)}
    return sorted(reported_unique, key=lambda c: order[c.full]), to_run


def run_everything(ctx, reported, to_run):
    args = ctx.args
    reported_set = {c.full for c in reported}
    live = []
    for fixture in ctx.fixtures:
        mine = [c for c in to_run if c.fixture is fixture]
        if not mine:
            continue
        reason = fixture.skip(ctx) if fixture.skip else None
        if reason:
            ctx.skipped[fixture.name] = reason
            continue
        ctx.base_tree(fixture)
        if fixture.setup:
            fixture.setup(ctx)
        live += mine
    ctx.say(f"== spec: {len(live)} commands")
    for case in live:
        reason = case.skip(ctx) if case.skip else None
        if reason:
            ctx.skipped[case.full] = reason
            continue
        want_cbor = not args.no_local and case.local
        ctx.results[case.full] = ctx.run_spec(case, want_cbor)
    judged = {}
    for case in live:
        if case.full in reported_set and case.full in ctx.results:
            judged[case.full] = ctx.judge(case)
    ctx.judged = judged
    if not args.no_cargo:
        ctx.say("== units: blaze's unit graph against cargo's")
        for case in live:
            if case.full in reported_set and case.full in ctx.results:
                ctx.units[case.full] = ctx.run_units(case)
    if not args.no_local:
        ctx.start_audit()
        ctx.say("== local: the command and the spec run, each in a fresh copy")
        for case in live:
            if case.full in reported_set and case.local and case.full in ctx.results:
                ctx.say(f"   {case.full}")
                ctx.locals[case.full] = ctx.run_local(case)
        for fixture in ctx.fixtures:
            if fixture.name in {c.fixture.name for c in reported if c.full in ctx.results}:
                found = ctx.run_mismatch(fixture)
                if found:
                    ctx.mismatch.append(found)
    return live


def failures_of(ctx, case):
    """Every reason the case failed, from every arm."""
    out = []
    judged = ctx.judged.get(case.full)
    if judged:
        out += [("spec", f) for f in judged[1]]
    units = ctx.units.get(case.full)
    if units and units["status"] in ("DIFF", "ERROR"):
        out += [("units", d) for d in units["diffs"][:12]]
        if len(units["diffs"]) > 12:
            out.append(("units", f"... and {len(units['diffs']) - 12} more"))
    local = ctx.locals.get(case.full)
    if local and local["status"] != "match":
        out += [("local", d) for d in local["diffs"]]
    return out


def print_report(ctx, reported):
    args = ctx.args
    rows = []
    for case in reported:
        if case.full in ctx.skipped or case.fixture.name in ctx.skipped:
            rows.append((case.full, "-", expected_text(case), "skipped", "-", "-", None))
            continue
        res = ctx.results.get(case.full)
        observed, _ = ctx.judged.get(case.full, ("?", []))
        units = ctx.units.get(case.full)
        unit_cell = {"equal": "=", "DIFF": "DIFF", "ERROR": "ERROR", "n/a": "n/a"}.get(units["status"], "?") if units else "skipped"
        local = ctx.locals.get(case.full)
        if local:
            local_cell = local["status"]
            d = (local.get("dispatched") or {})
            if self_audit_on(ctx):
                n = [(d.get(k) or {}).get("cargo", 0) for k in ("command", "spec_run")]
                local_cell += f" (cargo dispatched: {n[0]}/{n[1]})"
        else:
            local_cell = "n/a" if not case.local else "skipped"
        rows.append((case.full, short(res.digest) if res else "-", expected_text(case), observed, unit_cell, local_cell,
                     not failures_of(ctx, case)))
    width = [max(len(str(r[i])) for r in rows + [("fixture/case", "spec digest", "expected", "observed", "units = cargo", "local run", True)]) for i in range(6)]
    width = [min(w, lim) for w, lim in zip(width, (42, 12, 34, 52, 13, 44))]
    head = ("fixture/case", "spec digest", "expected", "observed", "units = cargo", "local run")
    line = lambda r: "  ".join(str(c)[:w].ljust(w) for c, w in zip(r, width)) + ("" if r[6] in (True, None) else "  <- FAIL")
    print()
    print(line(head + (True,)))
    print(line(tuple("-" * w for w in width) + (True,)))
    last = None
    for r in rows:
        fx = r[0].split("/")[0]
        if last is not None and fx != last:
            print()
        last = fx
        print(line(r))
    print()
    print("`changed(...)` names the top-level fields of the spec that differ. * : different sources are compared")
    print("without `source` for a configuration comparison. Source identity cases compare the complete spec.")
    for name, why in ctx.skipped.items():
        print(f"skipped {name}: {why}")
    if args.no_cargo:
        print("skipped the units arm: --no-cargo")
    if args.no_local:
        print("skipped the local arm: --no-local")
    elif ctx.audit is None:
        print(f"audit skipped: {ctx.audit_why}")
    refusals = [(c, ctx.results[c.full]) for c in reported if c.full in ctx.results and ctx.results[c.full].refused]
    if refusals:
        print("\nrefusals (what / why):")
        for case, res in refusals:
            print(f"  {case.full}: {res.refused.get('what')} / {res.refused.get('why')}")
    if ctx.mismatch:
        print("\nspec run in a checkout of another commit:")
        for m in ctx.mismatch:
            print(f"  {m['case']}: exit {m['exit']}: {m['said']}{'' if not m['fails'] else '  <- FAIL'}")
    if ctx.audit is not None:
        total = collections.Counter()
        for loc in ctx.locals.values():
            for which in (loc.get("dispatched") or {}).values():
                total.update(which or {})
        if ctx.locals:
            print(f"\ncargo dispatched: {total['cargo']}, rustc runs: {total['rustc']}, rustdoc runs: {total['rustdoc']} "
                  f"over {len(ctx.locals)} local pairs (docs/no-rustc.md: a known bug, reported and not failed)")


def self_audit_on(ctx):
    return ctx.audit is not None


def expected_text(case):
    exp = case.expect
    if exp in ("same", "changed") and case.vs:
        exp += f" vs {case.vs}"
    if exp.startswith("llvm:"):
        exp += f" (if llvm is carried)"
    return exp


def collect_failures(ctx, reported):
    fails = []
    for case in reported:
        if case.full in ctx.skipped or case.fixture.name in ctx.skipped:
            continue
        if case.full not in ctx.results:
            fails.append((case.full, "spec", "the case did not run"))
            continue
        for arm, why in failures_of(ctx, case):
            fails.append((case.full, arm, why))
    for m in ctx.mismatch:
        fails += [(m["case"], "local", f) for f in m["fails"]]
    return fails


def json_results(ctx, reported, fails):
    cases = []
    for case in reported:
        res = ctx.results.get(case.full)
        observed, spec_fails = ctx.judged.get(case.full, (None, []))
        cases.append({
            "case": case.full, "words": case.words, "env": case.env, "edits": len(case.edits), "cwd": case.cwd,
            "commit_edits": case.commit_edits, "compare_source": case.compare_source,
            "expect": case.expect, "vs": case.vs or ("base" if case.expect != "base" else None), "why": case.why,
            "skipped": ctx.skipped.get(case.full) or ctx.skipped.get(case.fixture.name),
            "observed": observed, "failures": [f for f in spec_fails],
            "refused": res.refused if res else None, "digests": res.digests if res else None,
            "error": res.error if res else None, "summary": res.summary if res else None,
            "units": ctx.units.get(case.full), "local": ctx.locals.get(case.full)})
    return {"bin": ctx.args.bin, "toolchain": TOOLCHAIN, "cases": cases, "mismatch": ctx.mismatch,
            "failures": [{"case": c, "arm": a, "why": w} for c, a, w in fails], "ok": not fails}


def list_cases(fixture_list):
    for fixture in fixture_list:
        print(f"{fixture.name}:")
        for case in fixture.cases:
            vs = f" vs {case.vs}" if case.vs and case.expect != "base" else ""
            env = " ".join(f"{k}={v}" for k, v in case.env.items())
            cwd = f" (in {case.cwd})" if case.cwd else ""
            edit = (" [committed edits]" if case.commit_edits else " [uncommitted edits]") if case.edits else ""
            print(f"  {case.name:24} {case.expect}{vs:22} {env + ' ' if env else ''}cargo spec {' '.join(case.words)}{cwd}{edit}"
                  f"{'  [local]' if case.local else ''}\n  {'':24} {case.why}")


def parse_args(argv):
    ap = argparse.ArgumentParser(prog="buildspec.py", description=__doc__.split("\n\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default=DEFAULT_BIN, help="the ra-jit under test (default $RA_JIT_BIN, else target/release/ra-jit)")
    ap.add_argument("--work", help="the work directory (default a new one under /tmp, removed at the end)")
    ap.add_argument("--only", action="append", default=[], metavar="FIXTURE[/CASE]")
    ap.add_argument("--no-local", action="store_true", help="skip arm 9, the local runs")
    ap.add_argument("--no-cargo", action="store_true", help="skip arm 8, the comparison with cargo's unit graph")
    ap.add_argument("--keep", action="store_true", help="keep the work directory")
    ap.add_argument("--self-test", action="store_true", help="the harness's own logic, against a fake ra-jit")
    ap.add_argument("--json", metavar="FILE", help="write every result there")
    ap.add_argument("--list", action="store_true", help="list the fixtures and cases")
    ap.add_argument("--timeout", type=int, default=900, help="seconds a ra-jit run may take (default 900)")
    ap.add_argument("--shared-store", action="store_true", help="local runs share one RA_JIT_CACHE (faster, less independent)")
    ap.add_argument("--no-audit", action="store_true", help="do not put norustc.py's stand-ins on PATH for the local runs")
    ap.add_argument("--units-with-doctest", action="store_true", help="compare cargo's doctest units too")
    ap.add_argument("--spec-run-in", choices=("root", "cwd"), default="root",
                    help="where `spec run` is run in the checkout: its root (default) or the case's directory")
    return ap.parse_args(argv)


def run_harness(args, quiet=False):
    """Everything; returns (exit status, ctx, reported, failures)."""
    fixture_list = fixtures()
    reported, to_run = select_cases(fixture_list, args.only)
    if not os.path.isfile(args.bin) or not os.access(args.bin, os.X_OK):
        raise HarnessError(f"no ra-jit at {args.bin}: build one (`cargo +1.99.0 build -p ra-jit --release`) or set RA_JIT_BIN / --bin")
    ctx = Ctx(args, fixture_list, quiet=quiet)
    fails = []
    try:
        run_everything(ctx, reported, to_run)
        if not quiet:
            print_report(ctx, reported)
        fails = collect_failures(ctx, reported)
        if args.json:
            with open(args.json, "w") as f:
                json.dump(json_results(ctx, reported, fails), f, indent=1, sort_keys=True)
            ctx.say(f"results in {args.json}")
    finally:
        with contextlib.suppress(Exception):
            ctx.blaze.stop(ctx.spec_home, ctx.work, ctx.blaze.env(scrubbed_env(), ctx.spec_home))
        ctx.blaze.close()
        if args.keep:
            print(f"kept {ctx.work}")
        elif ctx.made_root:
            shutil.rmtree(ctx.work, ignore_errors=True)
        else:
            for sub in ("fix", "local", "specs", "standins", "tools", "tools-copy", "store", "cargo-home"):
                shutil.rmtree(os.path.join(ctx.work, sub), ignore_errors=True)
            for leftover in ("audit.log", "audit.log.slice"):
                with contextlib.suppress(OSError):
                    os.unlink(os.path.join(ctx.work, leftover))
    return (1 if fails else 0), ctx, reported, fails


def main(argv=None):
    args = parse_args(argv if argv is not None else sys.argv[1:])
    if args.list:
        list_cases(fixtures())
        return 0
    if args.self_test:
        return self_test(args)
    try:
        status, ctx, reported, fails = run_harness(args)
    except HarnessError as e:
        print(f"buildspec.py: {e}", file=sys.stderr)
        return 1
    if fails:
        print(f"\nFAILED: {len(fails)} problems")
        for case, arm, why in fails:
            print(f"  {case} [{arm}]: {why}")
    else:
        print("\nOK: every expectation holds")
    return status


# --- The fake ra-jit of --self-test --------------------------------------------------------------
# It implements the contract on a small model of cargo's resolution, enough for the fixtures here
# (and no more): path dependencies, a workspace, features, profiles, `[env]`. A spec is made from
# the tree it runs in, the way the contract says; a local run is a simulation that prints what cargo
# prints and writes stand-ins for the programs. FAKE_BUG=<name> makes it wrong in one way, so that
# the self-test can show each way is caught. Nothing in it is blaze's: it is the harness's own
# test double.

FAKE_RA_JIT = r'''#!@PYTHON@
import hashlib, json, os, platform, re, shutil, subprocess, sys

BUG = os.environ.get("FAKE_BUG", "")


def sha(data):
    return hashlib.sha256(data if isinstance(data, bytes) else data.encode()).hexdigest()


def canon(v):
    return json.dumps(v, sort_keys=True, separators=(",", ":"))


class Refused(Exception):
    def __init__(self, what, why):
        Exception.__init__(self, what)
        self.what, self.why = what, why


def toml(path):
    root, cur = {}, None
    cur = root

    def walk(parts, create_list=False):
        node = root
        for part in parts:
            nxt = node.setdefault(part, {})
            node = nxt[-1] if isinstance(nxt, list) else nxt
        return node
    for raw in open(path).read().splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[["):
            parts = [p.strip() for p in line[2:-2].split(".")]
            parent = walk(parts[:-1])
            parent.setdefault(parts[-1], []).append({})
            cur = parent[parts[-1]][-1]
        elif line.startswith("["):
            cur = walk([p.strip() for p in line[1:-1].split(".")])
        else:
            key, _, value = line.partition("=")
            cur[key.strip()] = json.loads(value.strip())
    return root


def parse_words(words):
    o = {"cmd": words[0], "pkgs": [], "workspace": False, "exclude": [], "features": [], "all_features": False,
         "no_default": False, "release": False, "no_run": False, "nff": False, "lib": False, "doc": False,
         "positional": [], "trailing": [], "config": [], "manifest": None, "verbose": False}
    i = 1
    while i < len(words):
        w = words[i]
        if w == "--":
            o["trailing"] = words[i + 1:]
            break
        if w in ("-p", "--package"):
            o["pkgs"].append(words[i + 1]); i += 2
        elif w == "--exclude":
            o["exclude"].append(words[i + 1]); i += 2
        elif w in ("--features", "-F"):
            o["features"] += re.split(r"[ ,]+", words[i + 1]); i += 2
        elif w == "--workspace" or w == "--all":
            o["workspace"] = True; i += 1
        elif w == "--all-features":
            o["all_features"] = True; i += 1
        elif w == "--no-default-features":
            o["no_default"] = True; i += 1
        elif w == "--release":
            o["release"] = True; i += 1
        elif w == "--no-run":
            o["no_run"] = True; i += 1
        elif w == "--no-fail-fast":
            o["nff"] = True; i += 1
        elif w == "--lib":
            o["lib"] = True; i += 1
        elif w == "--doc":
            o["doc"] = True; i += 1
        elif w == "--manifest-path":
            o["manifest"] = words[i + 1]; i += 2
        elif w == "--config":
            o["config"].append(words[i + 1]); i += 2
        elif w == "--target":
            raise Refused("--target", "blaze builds for the machine it runs on")
        elif w == "-Z":
            raise Refused("-Z " + words[i + 1], "no unstable flag is honoured")
        elif w == "--message-format":
            raise Refused("--message-format", "blaze prints its own messages")
        elif w in ("-j", "--jobs", "--color"):
            i += 2
        elif w in ("-v", "--verbose"):
            o["verbose"] = True; i += 1
        elif w in ("-q", "--quiet", "--locked", "--offline", "--frozen"):
            i += 1
        elif w.startswith("-"):
            raise Refused(w, "not a flag blaze honours")
        else:
            o["positional"].append(w); i += 1
    return o


def load_pkg(d):
    t = toml(os.path.join(d, "Cargo.toml"))
    p = t.get("package", {})
    return {"name": p.get("name"), "dir": d, "toml": t, "edition": int(p.get("edition", "2021"))}


def deps_of(pkg, section):
    out = []
    for name, spec in pkg["toml"].get(section, {}).items():
        out.append((name, load_pkg(os.path.normpath(os.path.join(pkg["dir"], spec["path"])))))
    return out


def tool(env, var, default):
    value = env.get(var) or default
    path = value if "/" in value else shutil.which(value)
    if not path or not os.path.exists(path):
        raise Refused(var, "no such tool: " + value)
    with open(os.path.realpath(path), "rb") as f:
        return [os.path.basename(value), "1", sha(f.read())]


def flag_values(env):
    flags = env.get("CARGO_ENCODED_RUSTFLAGS")
    flags = flags.split("\x1f") if flags else (env.get("RUSTFLAGS") or "").split()
    joined = " ".join(flags)
    if "target-cpu" in joined:
        raise Refused("-C target-cpu", "the code generated for another CPU is not what the cache holds")
    if "linker" in joined:
        raise Refused("-C linker", "blaze links with the linker of its platform")
    return flags


def defaults(base):
    if base == "release":
        return {"opt-level": 3, "debug": 0, "debug-assertions": False, "overflow-checks": False}
    return {"opt-level": 0, "debug": 2, "debug-assertions": True, "overflow-checks": True}


def settings(root_toml, base, env, pkg=None, host=False):
    prof = root_toml.get("profile", {}).get(base, {})
    s = dict(defaults(base))
    s.update({k: v for k, v in prof.items() if k not in ("package", "build-override")})
    for key in ("opt-level", "debug", "debug-assertions", "overflow-checks", "panic", "strip", "split-debuginfo", "lto"):
        var = "CARGO_PROFILE_%s_%s" % (base.upper(), key.upper().replace("-", "_"))
        if var in env:
            v = env[var]
            s[key] = int(v) if v.isdigit() else {"true": True, "false": False}.get(v, v)
    if host:
        s.update(prof.get("build-override", {}))
    if pkg and (BUG == "pkg-override-all" or True):
        packages = prof.get("package", {})
        if BUG == "pkg-override-all":
            for v in packages.values():
                s.update(v)
        else:
            s.update(packages.get(pkg, {}))
    return s


def profile_json(name, s, env, base_is_root):
    opt = s["opt-level"]
    if opt not in (0, 1, 2, 3):
        raise Refused("profile.%s.opt-level = %s" % (name, json.dumps(opt)), "blaze has no opt-level s or z")
    if base_is_root and BUG != "no-lto-refusal" and s.get("lto") not in (None, False, "off"):
        raise Refused("profile.%s.lto = %s" % (name, json.dumps(s.get("lto"))), "lto other than off stays on Bazel")
    debug = s["debug"] if isinstance(s["debug"], int) and not isinstance(s["debug"], bool) else \
        {True: 2, False: 0, "none": 0, "line-tables-only": 1, "limited": 1, "full": 2}[s["debug"]]
    backend = env.get("RA_JIT_BACKEND") or "cranelift"
    if backend not in ("cranelift", "llvm"):
        raise Refused("RA_JIT_BACKEND", "unknown backend " + backend)
    llvm = backend == "llvm"
    if llvm and not os.environ.get("FAKE_LLVM"):
        raise Refused("RA_JIT_BACKEND=llvm", "this blaze is built without LLVM")
    return {"0": name, "1": opt, "2": debug, "3": bool(s["debug-assertions"]), "4": bool(s["overflow-checks"]),
            "5": 1 if s.get("panic") == "abort" else 0,
            "6": {"none": 0, "debuginfo": 1, "symbols": 2, True: 2, False: 0}.get(s.get("strip", "none"), 0),
            "7": {"off": 0, "packed": 1, "unpacked": 2}.get(s.get("split-debuginfo", "off"), 0),
            "8": 1 if llvm else 0, "9": int(env.get("RA_JIT_MIR_OPT") or 0),
            "10": int(env.get("RA_JIT_OPT") or 2) if llvm else 0,
            "11": (1 if env.get("RA_JIT_EMIT") == "fast" else 0) if llvm else 0}


def closure(defs, names):
    out, queue = set(), list(names)
    while queue:
        n = queue.pop()
        if n in out:
            continue
        out.add(n)
        queue += [m for m in defs.get(n, []) if m in defs]
    return out


def source_ref(root):
    """A test-double snapshot of these path-only fixtures, not blaze's source implementation."""
    def git(*args):
        return subprocess.check_output(["git", "-C", root, *args])
    checkout = git("rev-parse", "--show-toplevel").decode().strip()
    committed = [0, {"0": 1, "1": git("rev-parse", "HEAD").decode().strip(),
                     "2": git("rev-parse", "HEAD^{tree}").decode().strip()}]
    dirty = git("status", "--porcelain=v1", "--untracked-files=all")
    if BUG == "dirty-source-refused" and dirty:
        raise Refused("source", "uncommitted inputs unsupported")
    if BUG == "head-only-source" or BUG == "sibling-source-same" and root != checkout:
        return committed, checkout
    if not dirty and root == checkout:
        return committed, checkout
    # A Root names the selected workspace's files by content. This models the intended identity
    # contract only; the real snapshot transport and nested Git workspace schema remain work.
    names = sorted(set(git("ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", ".").decode().split("\0")) - {""})
    snapshot = [[name, sha(open(os.path.join(root, name), "rb").read())
                 if os.path.isfile(os.path.join(root, name)) else None] for name in names]
    return [1, {"0": sha(canon(snapshot))}], checkout


def resolve(cwd, words, env):
    o = parse_words(words)
    if BUG == "verbose-changes" and o["verbose"]:
        o["trailing"] = o["trailing"] + ["--verbose-marker"]
    if env.get("RUSTC_WRAPPER"):
        raise Refused("RUSTC_WRAPPER", "a wrapper changes what rustc is")
    if env.get("CARGO_BUILD_TARGET"):
        raise Refused("CARGO_BUILD_TARGET", "blaze builds for the machine it runs on")
    rustflags = flag_values(env)
    start = os.path.abspath(cwd)
    nearest = start
    if o["manifest"]:
        nearest = os.path.dirname(os.path.abspath(os.path.join(start, o["manifest"])))
    while not os.path.exists(os.path.join(nearest, "Cargo.toml")):
        if os.path.dirname(nearest) == nearest:
            raise Refused("manifest", "no Cargo.toml above the working directory")
        nearest = os.path.dirname(nearest)
    root = nearest
    d = nearest
    while True:
        if os.path.exists(os.path.join(d, "Cargo.toml")) and "workspace" in toml(os.path.join(d, "Cargo.toml")):
            root = d
            break
        if os.path.dirname(d) == d:
            break
        d = os.path.dirname(d)
    root_toml = toml(os.path.join(root, "Cargo.toml"))
    members = [load_pkg(os.path.join(root, m)) for m in root_toml.get("workspace", {}).get("members", [])]
    if "package" in root_toml:
        members.append(load_pkg(root))
    nearest_pkg = load_pkg(nearest) if "package" in toml(os.path.join(nearest, "Cargo.toml")) else None
    if o["pkgs"]:
        selected = [m for m in members if m["name"] in o["pkgs"]]
    elif o["workspace"]:
        selected = [m for m in members if m["name"] not in o["exclude"]]
    elif nearest_pkg:
        selected = [nearest_pkg]
    else:
        selected = [m for m in members if m["name"] not in o["exclude"]]
    cmd = o["cmd"]
    base = "release" if o["release"] else "dev"
    pname = base if o["release"] else ("test" if cmd == "test" else "dev")
    top = profile_json(pname, settings(root_toml, base, env), env, True)
    cfgs = []
    it = iter(rustflags)
    for f in it:
        if f == "--cfg":
            cfgs.append(next(it, ""))
    units = {}
    edges = []

    def features(pkg, is_sel):
        defs = pkg["toml"].get("features", {})
        if not is_sel:
            return closure(defs, defs.get("default", []))
        s = set() if o["no_default"] else closure(defs, defs.get("default", []))
        for f in o["features"]:
            if f in defs:
                s |= closure(defs, [f])
        if o["all_features"]:
            s |= closure(defs, [k for k in defs if k != "default"])
        return s - {"default"} if False else s

    def add(pkg, kind, name, host, test, is_sel=False):
        key = (pkg["name"], kind, name, host, test)
        if key not in units:
            feats = sorted(features(pkg, is_sel))
            cfg = sorted(['feature="%s"' % f for f in feats] + cfgs + (["test"] if test else []))
            s = settings(root_toml, base, env, pkg=pkg["name"] if not is_sel or True else None, host=host)
            units[key] = {"pkg": pkg["name"], "kind": kind, "name": name, "host": host, "test": test,
                          "features": feats, "cfg": cfg, "edition": pkg["edition"],
                          "profile": profile_json(pname, s, env, False)}
        return key

    def lib_unit(pkg, host_ctx, is_sel=False):
        pm = bool(pkg["toml"].get("lib", {}).get("proc-macro"))
        host = host_ctx or pm
        key = add(pkg, 6 if pm else 0, pkg["name"], host, False, is_sel)
        for dname, dpkg in deps_of(pkg, "dependencies"):
            edges.append((key, lib_unit(dpkg, host), dname))
        return key

    def build_script(pkg, is_sel):
        key = add(pkg, 5, "build-script-build", True, False, is_sel)
        for dname, dpkg in deps_of(pkg, "build-dependencies"):
            edges.append((key, lib_unit(dpkg, True), dname))
        return key

    asked = []
    for pkg in selected:
        d = pkg["dir"]
        has_lib = os.path.exists(os.path.join(d, "src", "lib.rs"))
        bins = []
        if os.path.exists(os.path.join(d, "src", "main.rs")):
            bins.append(pkg["name"])
        bindir = os.path.join(d, "src", "bin")
        if os.path.isdir(bindir):
            bins += [f[:-3] for f in sorted(os.listdir(bindir)) if f.endswith(".rs")]
        bs = build_script(pkg, True) if os.path.exists(os.path.join(d, "build.rs")) else None
        lib = lib_unit(pkg, False, True) if has_lib else None
        if lib and bs:
            edges.append((lib, bs, "build-script-build"))
        bin_keys = []
        for b in bins:
            k = add(pkg, 1, b, False, False, True)
            bin_keys.append(k)
            if lib:
                edges.append((k, lib, pkg["name"]))
            if bs:
                edges.append((k, bs, "build-script-build"))
            for dname, dpkg in deps_of(pkg, "dependencies"):
                edges.append((k, lib_unit(dpkg, False), dname))
        if cmd in ("check", "build"):
            if lib:
                asked.append(lib)
            if not o["lib"]:
                asked += bin_keys
        elif cmd == "run":
            asked += bin_keys[:1]
        else:
            if lib:
                t = add(pkg, 0, pkg["name"], False, True, True)
                for dname, dpkg in deps_of(pkg, "dependencies"):
                    edges.append((t, lib_unit(dpkg, False), dname))
                asked.append(t)
            if o["lib"]:
                continue
            for b in bins:
                t = add(pkg, 1, b, False, True, True)
                if lib:
                    edges.append((t, lib, pkg["name"]))
                asked.append(t)
            listed = {x.get("name"): x.get("harness", True) for x in pkg["toml"].get("test", [])}
            tdir = os.path.join(d, "tests")
            for f in sorted(os.listdir(tdir)) if os.path.isdir(tdir) else []:
                if f.endswith(".rs"):
                    name = f[:-3]
                    t = add(pkg, 2, name, False, listed.get(name, True) is not False, True)
                    if lib:
                        edges.append((t, lib, pkg["name"]))
                    asked.append(t)
            edir = os.path.join(d, "examples")
            for f in sorted(os.listdir(edir)) if os.path.isdir(edir) else []:
                if f.endswith(".rs"):
                    t = add(pkg, 3, f[:-3], False, False, True)
                    if lib:
                        edges.append((t, lib, pkg["name"]))
                    asked.append(t)
    def unit_doc(u):
        return {"0": [u["pkg"], "0.1.0", sha("path+" + u["pkg"])], "1": [u["kind"], u["name"]], "2": u["features"],
                "3": u["cfg"], "4": u["edition"], "5": [], "6": u["test"], "7": u["host"], "8": u["profile"]}
    order = sorted(units, key=lambda k: canon(unit_doc(units[k])))
    index = {k: i for i, k in enumerate(order)}
    docs = []
    for k in order:
        doc = unit_doc(units[k])
        doc["5"] = sorted([index[b], name] for a, b, name in edges if a == k and b in index)
        docs.append(doc)
    source, checkout = source_ref(root)
    config_env = {}
    d = start
    chain = []
    while True:
        chain.append(d)
        if os.path.dirname(d) == d:
            break
        d = os.path.dirname(d)
    for d in reversed(chain):
        for name in ("config.toml", "config"):
            p = os.path.join(d, ".cargo", name)
            if os.path.exists(p):
                for k, v in toml(p).get("env", {}).items():
                    config_env[k] = v["value"] if isinstance(v, dict) else v
    for c in o["config"]:
        m = re.match(r"env\.(\w+)\s*=\s*(.*)$", c)
        if m:
            config_env[m.group(1)] = json.loads(m.group(2)) if m.group(2).startswith('"') else m.group(2)
    macos = sys.platform == "darwin"
    mac = platform.mac_ver()[0] or "14.0"
    plat = {"0": "host", "1": sys.platform, "2": ["macosx", mac, env.get("MACOSX_DEPLOYMENT_TARGET") or mac] if macos else None,
            "3": None, "4": ["ld", "1", sha("ld")], "5": "native", "6": tool(env, "CC", "cc"), "7": tool(env, "CXX", "c++"),
            "8": tool(env, "AR", "ar"), "9": None}
    plat_digest = sha("platform" + canon(plat))
    ignored = 0
    ta = list(o["trailing"])
    if "--ignored" in ta:
        ignored = 1
    if "--include-ignored" in ta:
        ignored = 2
    threads = None
    for i, a in enumerate(ta):
        if a.startswith("--test-threads="):
            threads = int(a.split("=")[1])
        elif a == "--test-threads" and i + 1 < len(ta):
            threads = int(ta[i + 1])
    skip = [ta[i + 1] for i, a in enumerate(ta) if a == "--skip" and i + 1 < len(ta)]
    run = {"0": o["positional"] if cmd == "test" else [], "1": "--exact" in ta, "2": skip, "3": ignored,
           "4": o["no_run"], "5": o["nff"] and BUG != "nofailfast-same", "6": threads,
           "7": (o["positional"] if cmd == "run" else []) + ta, "8": [], "9": 0 if o["lib"] else (2 if o["doc"] else 1)}
    command = {"check": [0, None], "build": [1, None], "test": [2, run], "run": [3, run]}[cmd]
    lock = open(os.path.join(root, "Cargo.lock"), "rb").read() if os.path.exists(os.path.join(root, "Cargo.lock")) else b""
    script_env = sorted([k, env[k]] for k in ("CC", "CXX", "AR", "MACOSX_DEPLOYMENT_TARGET") if k in env)
    spec = {"0": source, "1": sha(lock), "2": docs, "3": sorted(index[k] for k in asked if k in index),
            "4": top, "5": plat_digest, "6": rustflags, "7": sorted([k, v] for k, v in config_env.items()),
            "8": script_env, "9": "" if os.path.relpath(start, root) == "." else os.path.relpath(start, root),
            "10": command, "11": sha("toolchain")}
    spec_digest = sha("spec" + canon(spec))
    digests = {"spec": spec_digest, "platform": plat_digest, "front_end": sha("front" + canon([docs, spec["1"], rustflags])),
               "config": sha("config" + canon([cmd, sorted(canon(docs[index[k]]) for k in asked), top, plat_digest, rustflags])),
               "job_content": sha(spec_digest + "Main")}
    summary = {"command": cmd, "asked": sorted("%s@0.1.0:%s:%s" % (units[k]["pkg"], ["lib", "bin", "test", "example", "bench", "build-script", "proc-macro"][units[k]["kind"]], units[k]["name"]) for k in asked),
               "units": len(docs)}
    return {"spec": spec, "digests": digests, "summary": summary, "platform": plat, "units": units, "root": root,
            "opts": o, "asked": asked, "config_env": config_env, "start": start,
            "checkout_cwd": os.path.relpath(start, checkout)}


CAPTURED = ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RA_JIT_BACKEND", "RA_JIT_OPT", "RA_JIT_EMIT", "RA_JIT_MIR_OPT", "CC", "CXX", "AR",
            "MACOSX_DEPLOYMENT_TARGET")


def captured_env(env):
    return {k: v for k, v in env.items() if k in CAPTURED or k.startswith("CARGO_PROFILE_")}


def unit_graph(cwd, words, env):
    env = dict(env, RUSTC_BOOTSTRAP="1")
    ran = subprocess.run(["cargo", "+1.99.0", words[0], "--unit-graph", "-Z", "unstable-options", "--locked", "--offline"] + words[1:],
                         cwd=cwd, env=env, capture_output=True, text=True)
    if ran.returncode:
        sys.stderr.write(ran.stderr)
        sys.exit(1)
    graph = json.loads(ran.stdout)
    if BUG == "unit-features":
        for u in graph["units"]:
            if u["features"]:
                u["features"] = u["features"][1:]
                break
    return graph


def hash16(text):
    return sha(text)[:16]


def simulate(res, words, env):
    """What cargo prints for the command and the programs it leaves, written to $CARGO_TARGET_DIR."""
    o, units, root = res["opts"], res["units"], res["root"]
    cmd = o["cmd"]
    target = env.get("CARGO_TARGET_DIR") or os.path.join(root, "target")
    prof = "release" if o["release"] else "debug"
    deps, examples = [os.path.join(target, prof, "deps"), os.path.join(target, prof, "examples")]
    os.makedirs(deps, exist_ok=True)
    os.makedirs(examples, exist_ok=True)
    out = []

    def stub(path):
        with open(path, "w") as f:
            f.write("#!/bin/sh\nexit 0\n")
        os.chmod(path, 0o755)
    exes = {}
    for key, u in sorted(units.items(), key=lambda kv: (kv[1]["pkg"], kv[1]["kind"], kv[1]["name"])):
        h = hash16(repr(key))
        if u["kind"] == 5 and not u["test"]:
            stub(os.path.join(deps, "build_script_build-" + h))
        elif u["kind"] == 1 and not u["test"]:
            stub(os.path.join(deps, u["name"] + "-" + h))
            stub(os.path.join(target, prof, u["name"]))
        elif u["test"] and u["kind"] in (0, 1, 2) or (u["kind"] == 2 and not u["test"]):
            stub(os.path.join(deps, u["name"].replace("-", "_") + "-" + h))
            exes[key] = h
        elif u["kind"] == 3:
            stub(os.path.join(examples, u["name"]))
            stub(os.path.join(examples, u["name"] + "-" + h))
    out.append("   Compiling %s v0.1.0" % ", ".join(sorted({u["pkg"] for u in units.values()})))
    out.append("    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.00s")
    code = 0
    ta = o["trailing"]
    if cmd == "run":
        name = [u for u in units.values() if u["kind"] == 1][0]["name"]
        out.append("     Running `target/debug/%s`" % name)
        out.append("@@bin %s add=3 args=%s" % (name, json.dumps(o["positional"] + ta)))
        return "\n".join(out) + "\n", 0
    if cmd != "test":
        return "\n".join(out) + "\n", 0
    runs = []
    for key, u in sorted(units.items(), key=lambda kv: ({0: 0, 1: 1, 2: 2}.get(kv[1]["kind"], 9), kv[1]["pkg"], kv[1]["name"])):
        if key in exes:
            runs.append((key, u))
    if o["no_run"]:
        for key, u in runs:
            out.append("  Executable %s (target/debug/deps/%s-%s)" % (u["name"], u["name"], exes[key]))
        return "\n".join(out) + "\n", 0
    pkgdir = {}
    for m in [res["root"]]:
        pass
    failed = False
    for key, u in runs:
        d = [p for p in [res["root"], os.path.join(res["root"], "crates", u["pkg"])] if os.path.isdir(os.path.join(p, "src")) or os.path.isdir(os.path.join(p, "tests"))]
        base = d[-1] if d else res["root"]
        if u["kind"] == 0:
            src, label = os.path.join(base, "src", "lib.rs"), "unittests src/lib.rs"
        elif u["kind"] == 1:
            src = os.path.join(base, "src", "main.rs") if os.path.exists(os.path.join(base, "src", "main.rs")) else os.path.join(base, "src", "bin", u["name"] + ".rs")
            label = "unittests " + os.path.relpath(src, base)
        else:
            src, label = os.path.join(base, "tests", u["name"] + ".rs"), "tests/%s.rs" % u["name"]
        out.append("     Running %s (target/debug/deps/%s-%s)" % (label, u["name"], exes[key]))
        if not u["test"]:
            out.append("@@custom args=%s" % json.dumps(o["positional"] + ta))
            continue
        text = open(src).read() if os.path.exists(src) else ""
        found = []
        for m in re.finditer(r"#\[test\]\s*(#\[ignore\]\s*)?fn (\w+)", text):
            found.append((("tests::" if "mod tests" in text[:m.start()] else "") + m.group(2), bool(m.group(1))))
        ran = []
        for name, ign in found:
            if o["positional"] and not any((name == f if "--exact" in ta else f in name) for f in o["positional"]):
                continue
            ran.append((name, ign))
        out.append("")
        out.append("running %d tests" % len(ran))
        passed = fails = ignored = 0
        for name, ign in sorted(ran):
            if ign and "--ignored" not in ta and "--include-ignored" not in ta:
                out.append("test %s ... ignored" % name)
                ignored += 1
            elif "--ignored" in ta and not ign:
                continue
            elif name.endswith("fails"):
                out.append("test %s ... FAILED" % name)
                fails += 1
            else:
                if "--nocapture" in ta and "@@envcfg" in text:
                    g = res["config_env"].get("GREETING", "unset")
                    out.append("test %s ... @@envcfg compile=%s run=%s" % (name, g, g))
                    out.append("ok")
                else:
                    out.append("test %s ... ok" % name)
                passed += 1
        out.append("")
        out.append("test result: %s. %d passed; %d failed; %d ignored; 0 measured; 0 filtered out; finished in 0.00s" % ("FAILED" if fails else "ok", passed, fails, ignored))
        if fails:
            failed = True
            out.append("error: test failed, to rerun pass `--test %s`" % u["name"])
            if not o["nff"]:
                break
    if failed and not o["nff"]:
        return "\n".join(out) + "\n", 101
    # doc tests of the libraries
    if not o["lib"]:
        for key, u in sorted(units.items()):
            if u["kind"] == 0 and not u["test"] and not u["host"] and key[0] in {k[0] for k, _ in runs}:
                lib = [os.path.join(p, "src", "lib.rs") for p in [res["root"], os.path.join(res["root"], "crates", u["pkg"])] if os.path.exists(os.path.join(p, "src", "lib.rs"))]
                text = open(lib[-1]).read() if lib else ""
                lines = text.splitlines()
                opens = [i for i, l in enumerate(lines) if re.match(r"\s*///\s*```\s*$", l)][::2]
                if opens:
                    out.append("   Doc-tests %s" % u["name"])
                    out.append("")
                    out.append("running %d tests" % len(opens))
                    for i in opens:
                        nxt = next((re.search(r"fn (\w+)", l).group(1) for l in lines[i:] if re.search(r"fn (\w+)", l)), "item")
                        out.append("test src/lib.rs - %s (line %d) ... ok" % (nxt, i + 1))
                    out.append("")
                    out.append("test result: ok. %d passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s" % len(opens))
    return "\n".join(out) + "\n", 1 if failed else 0


def log_call(args):
    path = os.environ.get("FAKE_LOG")
    if path:
        keys = ("RA_JIT_MEMORY_MAX", "RA_JIT_OWN_BASE_MEMORY", "RA_JIT_SPARE", "RA_JIT_RUN_ID", "RA_JIT_HOME", "RUST_BACKTRACE",
                "RUSTFLAGS", "CARGO_TARGET_DIR", "CC")
        with open(path, "a") as f:
            f.write(json.dumps({"args": args, "cwd": os.getcwd(), "env": {k: os.environ.get(k) for k in keys}}) + "\n")


def main():
    argv = sys.argv[1:]
    log_call(argv)
    if argv[:1] != ["cargo"]:
        sys.stderr.write("fake ra-jit: only `cargo` is implemented\n")
        return 1
    rest = argv[1:]
    if rest[:1] == ["stop"]:
        return 0
    env = dict(os.environ)
    # `spec --run FILE` runs a spec; `spec run -- y` is the spec of a `run` command.
    if rest[:1] == ["spec"] and rest[1:2] == ["--run"] and len(rest) == 3:
        saved = json.load(open(rest[2]))
        for k in list(env):
            if k in CAPTURED or k.startswith("CARGO_PROFILE_"):
                del env[k]
        env.update(saved["env"])
        words = list(saved["words"])
        if BUG == "spec-run-drops-no-run":
            words = [w for w in words if w != "--no-run"]
        checkout = subprocess.check_output(["git", "rev-parse", "--show-toplevel"]).decode().strip()
        cwd = os.path.join(checkout, saved["spec"]["9"] if BUG == "replay-git-root" else saved["cwd"])
        try:
            res = resolve(cwd, words, env)
        except Refused as r:
            sys.stderr.write("blaze: %s: %s\n" % (r.what, r.why))
            return 1
        for i, name in enumerate(["source", "lock", "units", "asked", "profile", "platform", "rustflags", "config_env", "script_env", "cwd", "command", "toolchain"]):
            if canon(res["spec"][str(i)]) != canon(saved["spec"][str(i)]):
                sys.stderr.write("blaze: the tree does not match the spec: %s\n" % name)
                return 2
        text, code = simulate(res, words, env)
        sys.stdout.write(text)
        return code
    if rest[:1] == ["spec"]:
        words, graph, out = rest[1:], False, None
        while words and words[0].startswith("--") and words[0] in ("--unit-graph", "--out"):
            if words[0] == "--unit-graph":
                graph = True
                words = words[1:]
            else:
                out, words = words[1], words[2:]
        if graph:
            print(json.dumps(unit_graph(os.getcwd(), words, env)))
            return 0
        try:
            res = resolve(os.getcwd(), words, env)
        except Refused as r:
            print(json.dumps({"refused": {"what": r.what, "why": r.why}, "digests": None, "summary": None, "spec": None, "platform": None}))
            return 2
        if out:
            with open(out, "w") as f:
                # As with saved words/environment, this auxiliary path models reproduction in
                # the test double; it does not assert that the current wire has this information.
                json.dump({"words": words, "env": captured_env(env), "cwd": res["checkout_cwd"], "spec": res["spec"]}, f)
        print(json.dumps({"refused": None, "digests": res["digests"], "summary": res["summary"], "spec": res["spec"], "platform": res["platform"]}))
        return 0
    try:
        res = resolve(os.getcwd(), rest, env)
    except Refused as r:
        sys.stderr.write("error: %s: %s\n" % (r.what, r.why))
        return 1
    if os.environ.get("FAKE_DISPATCH") and os.environ.get("CARGO"):
        subprocess.run([os.environ["CARGO"], "--version"], capture_output=True)
    text, code = simulate(res, rest, env)
    sys.stdout.write(text)
    return code


if __name__ == "__main__":
    sys.exit(main())
'''


# --- --self-test ---------------------------------------------------------------------------------

CANNED_TEST_RUN = """\
   Compiling basic v0.1.0 (/tmp/x/basic)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.52s
     Running unittests src/lib.rs (target/debug/deps/basic-0123456789abcdef)

running 2 tests
test tests::slow ... ignored
test tests::adds ... ok

test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/main.rs (target/debug/deps/basic-fedcba9876543210)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/custom.rs (target/debug/deps/custom-aaaaaaaaaaaaaaaa)
@@custom args=["--nocapture"]
     Running tests/it.rs (target/debug/deps/it-bbbbbbbbbbbbbbbb)

running 2 tests
test pending ... \x1b[32m@@envcfg compile=hi run=hi\x1b[0m
ok
test failing ... FAILED

failures:

---- failing stdout ----
thread 'failing' panicked at tests/it.rs:3:5:

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

   Doc-tests basic

running 1 test
test src/lib.rs - add (line 5) ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s
"""


class SelfTest:
    def __init__(self):
        self.bad = 0
        self.total = 0

    def check(self, ok, what, detail=""):
        self.total += 1
        if not ok:
            self.bad += 1
        print(f"  {'ok  ' if ok else 'FAIL'} {what}{'' if ok or not detail else ': ' + clip(detail, 400)}", flush=True)
        return ok


def graph(units, roots=(0,)):
    return {"version": 1, "units": units, "roots": list(roots)}


def gunit(pkg, name, mode="build", features=(), deps=(), kind="lib", profile=None, path="/a/b", **extra):
    unit = {"pkg_id": f"path+file://{path}/{pkg}#{pkg}@0.1.0", "target": {"kind": [kind], "crate_types": [kind], "name": name,
            "src_path": f"{path}/{pkg}/src/lib.rs", "edition": "2021", "doc": True, "doctest": True, "test": True},
            "profile": profile or {"name": "dev", "opt_level": "0", "codegen_units": None, "debuginfo": 2},
            "platform": None, "mode": mode, "features": list(features),
            "dependencies": [{"index": i, "extern_crate_name": n, "public": False, "noprelude": False, "nounused": True}
                             for i, n in deps]}
    unit.update(extra)
    return unit


def self_test_units(t):
    print("the comparison of unit graphs, on canned graphs:")
    base = graph([gunit("top", "top", features=["a"], deps=[(1, "dep"), (2, "mac")]), gunit("dep", "dep"),
                  gunit("mac", "mac", kind="proc-macro")])
    t.check(not compare_graphs(base, json.loads(json.dumps(base))), "a graph equals itself")
    # the same graph with the units in another order, the indexes mapped, other paths and no `nounused`
    shuffled = json.loads(json.dumps(base))
    order = [2, 0, 1]
    shuffled["units"] = [shuffled["units"][i] for i in order]
    where = {old: new for new, old in enumerate(order)}
    for u in shuffled["units"]:
        u["pkg_id"] = u["pkg_id"].replace("/a/b", "/elsewhere")
        u["target"]["src_path"] = "/elsewhere/x.rs"
        for d in u["dependencies"]:
            d["index"] = where[d["index"]]
            d.pop("nounused")
    shuffled["roots"] = [where[0]]
    t.check(not compare_graphs(base, shuffled), "units in another order, with the indexes mapped and the paths spelled otherwise, are equal",
            "; ".join(compare_graphs(base, shuffled)))
    changed = json.loads(json.dumps(base))
    changed["units"][0]["features"] = []
    got = compare_graphs(changed, base)
    t.check(any("features" in d for d in got), "a feature that differs is named", "; ".join(got))
    changed = json.loads(json.dumps(base))
    changed["units"][1]["profile"]["opt_level"] = "1"
    got = compare_graphs(changed, base)
    t.check(any("profile" in d and "opt_level" in d for d in got), "a profile field that differs is named", "; ".join(got))
    changed = json.loads(json.dumps(base))
    changed["units"][0]["dependencies"].pop()
    got = compare_graphs(changed, base)
    t.check(any("dependencies" in d for d in got), "a dependency that is missing is named", "; ".join(got))
    changed = json.loads(json.dumps(base))
    changed["units"].append(gunit("extra", "extra"))
    got = compare_graphs(changed, base)
    t.check(any("only in blaze" in d and "extra" in d for d in got), "a unit only blaze has is named", "; ".join(got))
    got = compare_graphs(base, changed)
    t.check(any("only in cargo" in d and "extra" in d for d in got), "a unit only cargo has is named", "; ".join(got))
    changed = json.loads(json.dumps(base))
    changed["roots"] = [0, 1]
    t.check(any(d.startswith("roots") for d in compare_graphs(changed, base)), "roots that differ are named")
    changed = json.loads(json.dumps(base))
    changed["units"].append(gunit("top", "top", mode="doctest"))
    t.check(not compare_graphs(base, changed), "a doctest unit of cargo's is left out")
    t.check(bool(compare_graphs(base, changed, doctest=True)), "and is compared with --units-with-doctest")
    t.check(pkg_of("path+file:///x/y/p#0.1.0") == ("p", "0.1.0") and pkg_of("registry+https://h/index#serde@1.0.1") == ("serde", "1.0.1"),
            "a package id is read as cargo spells it, with or without the name")


def self_test_parsing(t):
    print("the normalisation of outputs and of what a build leaves:")
    got = parse_run(CANNED_TEST_RUN)
    t.check(("unittests src/lib.rs", "tests::adds", "ok") in got["tests"] and ("unittests src/lib.rs", "tests::slow", "ignored") in got["tests"],
            "libtest's lines are read by the binary they are under", str(got["tests"]))
    t.check(("tests/it.rs", "pending", "ok") in got["tests"] and ("tests/it.rs", "failing", "FAILED") in got["tests"],
            "a result that follows the program's own output (--nocapture, colour) is read", str(got["tests"]))
    t.check(got["docs"] == [("doc:basic", "src/lib.rs - add (line 5)", "ok")], "doc tests are kept apart", str(got["docs"]))
    t.check(got["markers"] == ['@@custom args=["--nocapture"]', "@@envcfg compile=hi run=hi"], "the programs' own lines are kept, from where they start", str(got["markers"]))
    t.check(("unittests src/lib.rs", 1, 0, 1) in got["totals"] and ("tests/it.rs", 1, 1, 0) in got["totals"], "each binary's totals are kept", str(got["totals"]))
    other = CANNED_TEST_RUN.replace("0123456789abcdef", "1111111111111111").replace("0.52s", "9.99s").replace("/tmp/x", "/var/y").replace("0.07s", "1.0s")
    t.check(parse_run(other) == got, "hashes, paths and timings do not change what is read")
    with tempfile.TemporaryDirectory() as tmp:
        for rel in ("debug/basic", "debug/examples/demo", "debug/examples/demo-0123456789abcdef", "debug/deps/basic-0123456789abcdef",
                    "debug/deps/basic-fedcba9876543210", "debug/deps/it-bbbbbbbbbbbbbbbb", "debug/deps/libbasic-0123456789abcdef.rlib",
                    "debug/deps/basic-0123456789abcdef.d", "debug/build/x-1/build-script-build"):
            path = os.path.join(tmp, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as f:
                f.write("x")
            os.chmod(path, 0o755)
        os.chmod(os.path.join(tmp, "debug/deps/it-bbbbbbbbbbbbbbbb"), 0o644)
        got = scan_programs(tmp)
        t.check(got == ["bin:basic", "example:demo", "exe:basic x2"] or got == ["bin:basic", "exe:basic x2", "example:demo"],
                "programs are the executables of the profile directory, hashes dropped, libraries and metadata left out", str(got))
    s = {"tests": [("a", "tests::adds", "ok"), ("a", "tests::slow", "ignored")], "programs": ["bin:basic"], "markers": ["@@bin basic add=3"], "exit": 0}
    t.check(check_lexpect(s, [("test", "adds", "ok"), ("no-test", "slow"), ("program", "bin", "basic"), ("no-program", "bin", "c"),
                              ("line", "add=3"), ("exit", 0)]) == [], "local expectations that hold are not reported")
    bad = check_lexpect(s, [("test", "adds", "FAILED"), ("no-test", "adds"), ("program", "bin", "c"), ("no-program", "bin", "basic"),
                            ("line", "nope"), ("exit", 1), ("exit-nonzero",)])
    t.check(len(bad) == 7, "and each one that does not is", str(bad))
    t.check(compare_summaries({**s, "totals": [], "docs": []}, {**s, "totals": [], "docs": [], "exit": 1}) == [("exit", 0, 1)],
            "two runs are compared part by part")


def self_test_spec_reading(t):
    print("the reading of a spec's JSON rendering:")
    unit = {"0": ["a", "0.1.0", "00"], "1": [2, "custom"], "2": ["x"], "3": [], "4": 2021, "5": [[1, "b"]], "6": False, "7": False,
            "8": {"0": "dev", "1": 0, "2": 2, "3": True, "4": True, "5": 0, "6": 0, "7": 0, "8": 0, "9": 1, "10": 0, "11": 0}}
    u = Unit(unit)
    t.check((u.package, u.kind, u.name, u.test, u.host, u.profile["mir_opt"]) == ("a", "test", "custom", False, False, 1), "a unit is read by field numbers")
    named = {"package": ["a", "0.1.0", "00"], "target": [2, "custom"], "profile": {"name": "dev", "mir_opt": 1}}
    t.check(Unit(named).name == "custom" and Unit(named).profile["mir_opt"] == 1, "and by field names")
    spec = {"0": [0, {}], "10": [2, {"0": [], "1": False, "2": [], "3": 1, "4": False, "5": False, "6": None, "7": ["--ignored"], "8": [], "9": 1}]}
    name, run = command_of(spec)
    t.check(name == "test" and run["ignored"] == 1 and run["harness_args"] == ["--ignored"], "a command and its RunSpec are read")
    a, b = {"0": 1, "2": [1], "9": ""}, {"0": 2, "2": [2], "9": ""}
    t.check(changed_fields(a, b) == {"units"} and fingerprint(a) != fingerprint(b) and fingerprint({"0": 1, "2": [1]}) == fingerprint({"0": 5, "2": [1]}),
            "two specs are told apart by field, and by a fingerprint that leaves the source out")
    before = argparse.Namespace(spec={"0": 1, "2": [1]}, digest="before", digests={})
    after = argparse.Namespace(spec={"0": 2, "2": [1]}, digest="after", digests={})
    full = Delta(before, after, compare_source=True)
    t.check(Delta(before, after).same and not full.same and full.fields == {"source"} and not full.inconsistent,
            "source identity cases count source-only changes while configuration comparisons still omit them")
    after.digest = before.digest
    t.check(Delta(before, after, compare_source=True).inconsistent is not None,
            "a changed source rendering with an unchanged spec digest is caught")
    t.check(unwrap({"BuildSpec": {"0": 1}}) == {"0": 1}, "blaze-wire's `{\"BuildSpec\": ...}` wrapper is taken off")
    t.check(lock_text([("b", []), ("a", ["b"])]).startswith("# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"a\""),
            "a Cargo.lock is written in name order")


def run_fixture_checks(t):
    """The fixtures are what they say: every case is named, compared with a case that exists, and
    every `same` has a reason."""
    print("the fixtures and cases:")
    problems = []
    for fx in fixtures():
        names = set(fx.by_name)
        if "base" not in names:
            problems.append(f"{fx.name} has no base")
        for c in fx.cases:
            for ref in [c.vs, c.llvm_of] + [r[1] for r in c.also]:
                if ref and ref not in names:
                    problems.append(f"{c.full} names {ref}, which is not a case of {fx.name}")
            if not c.why:
                problems.append(f"{c.full} has no reason")
            if c.local and c.expect in ("refused",):
                problems.append(f"{c.full} is refused and local")
    t.check(not problems, f"{sum(len(f.cases) for f in fixtures())} cases of {len(fixtures())} fixtures are coherent", "; ".join(problems))


def self_test_source_trees(t, tmp):
    """Check the materialized reproducers, without starting a fake or a real compiler."""
    print("source identity fixtures:")
    ctx = Ctx.__new__(Ctx)  # These filesystem methods need no executor or watchdog.
    ctx.work, ctx.trees = os.path.join(tmp, "source-trees"), {}
    source = Fixture("source", SOURCE, source_cases())
    base = ctx.tree(source.base())
    dirty = ctx.tree(source.by_name["dirty-body"])
    committed = ctx.tree(source.by_name["committed-body"])
    moved = ctx.tree(source.by_name["moved-dirty-body"])
    body = lambda path: open(os.path.join(path, "src/lib.rs")).read()
    t.check(git(base, "rev-parse", "HEAD") == git(dirty, "rev-parse", "HEAD")
            and body(base) != body(dirty) and git(dirty, "status", "--porcelain")
            and not git(base, "status", "--porcelain"),
            "the dirty-body case changes Rust bytes while preserving HEAD and the clean base")
    t.check(git(base, "rev-parse", "HEAD") != git(committed, "rev-parse", "HEAD")
            and body(committed) == body(dirty) and not git(committed, "status", "--porcelain"),
            "the committed-body case changes HEAD with the same body edit")
    t.check(dirty != moved and body(dirty) == body(moved)
            and git(dirty, "rev-parse", "HEAD") == git(moved, "rev-parse", "HEAD"),
            "the relocated dirty case preserves its inputs and HEAD")
    nested = ctx.tree(Fixture("nested", NESTED, nested_cases()).base())
    same = lambda path: open(os.path.join(nested, "one", path)).read() == open(os.path.join(nested, "two", path)).read()
    t.check(same("Cargo.toml") and same("Cargo.lock") and not same("src/lib.rs")
            and not os.path.exists(os.path.join(nested, "Cargo.toml"))
            and git(os.path.join(nested, "one"), "rev-parse", "HEAD") == git(os.path.join(nested, "two"), "rev-parse", "HEAD"),
            "sibling workspaces share a Git commit and package metadata, but select different Rust source")


def oracle_available():
    try:
        ran = subprocess.run(["cargo", f"+{TOOLCHAIN}", "--version"], capture_output=True, text=True, timeout=30)
        return ran.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def self_test_oracle(t, tmp):
    """cargo, as the oracle, on the fixtures the local arm runs: each builds, and the local
    expectations (what a command's own run shows) hold of cargo's run. cargo is not the thing under
    test here: it is what the expectations are written against."""
    print("\ncargo as the oracle of the fixtures and of what the local runs expect (not blaze):")
    env = scrubbed_env({"CARGO_HOME": os.path.join(tmp, "oracle-cargo-home")})
    for fixture in fixtures():
        fake_ctx = argparse.Namespace(work=tmp)
        reason = fixture.skip(fake_ctx) if fixture.skip else None
        if reason:
            print(f"  skip {fixture.name}: {reason}")
            continue
        base = os.path.join(tmp, "oracle", fixture.name, "base")
        write_files(base, fixture.files)
        if fixture.setup:
            fixture.setup(fake_ctx)
        target = os.path.join(tmp, "oracle", fixture.name, "target")
        for case in fixture.cases:
            if not (case.local and case.lexpect):
                continue
            copy = os.path.join(tmp, "oracle", fixture.name, "case-" + case.name)
            shutil.copytree(base, copy, symlinks=True)
            for edit in case.edits:
                apply_edit(copy, edit)
            shutil.rmtree(target, ignore_errors=True)
            cenv = dict(env, CARGO_TARGET_DIR=target)
            cenv.update({k: (v or "").replace("@TOOLS@", os.path.join(tmp, "tools")) for k, v in case.env.items() if v is not None})
            ran = subprocess.run(["cargo", f"+{TOOLCHAIN}", *case.words], cwd=os.path.join(copy, case.cwd), env=cenv, text=True,
                                 stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=600)
            got = check_lexpect(summarize(ran.stdout, target, ran.returncode), case.lexpect)
            t.check(not got, f"cargo {' '.join(case.words)} on {case.full} shows what the case expects", "; ".join(got) + " | " + clip(ran.stdout[-300:]))


def fake_args(fake, **kw):
    base = dict(bin=fake, work=None, only=[], no_local=True, no_cargo=True, keep=False, self_test=False, json=None, list=False,
                timeout=300, shared_store=False, no_audit=False, units_with_doctest=False, spec_run_in="root")
    base.update(kw)
    return argparse.Namespace(**base)


@contextlib.contextmanager
def environment(**kw):
    saved = {k: os.environ.get(k) for k in kw}
    for k, v in kw.items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
    try:
        yield
    finally:
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


def self_test(args):
    t = SelfTest()
    self_test_spec_reading(t)
    self_test_units(t)
    self_test_parsing(t)
    run_fixture_checks(t)
    tmp = tempfile.mkdtemp(prefix="rj-bs-self-", dir="/tmp")
    try:
        self_test_source_trees(t, tmp)
        if oracle_available():
            self_test_oracle(t, tmp)
        else:
            print(f"\nskip the oracle's check of the local expectations: no cargo +{TOOLCHAIN}")
        fake = os.path.join(tmp, "bin", "ra-jit")
        os.makedirs(os.path.dirname(fake))
        with open(fake, "w") as f:
            f.write(FAKE_RA_JIT.replace("@PYTHON@", sys.executable))
        os.chmod(fake, 0o755)
        log = os.path.join(tmp, "calls.jsonl")
        cargo = oracle_available()
        norustc.STILL = 1  # the audit's settling, which a fake has nothing to settle
        out_json = os.path.join(tmp, "results.json")
        print("\nthe whole harness against the fake ra-jit (the fake's `--unit-graph` is cargo's own, so the units arm "
              + ("compares equal graphs" if cargo else "is skipped: no cargo +%s here" % TOOLCHAIN) + "):")
        with environment(FAKE_LOG=log, FAKE_DISPATCH="1", FAKE_BUG=None, FAKE_LLVM=None):
            status, ctx, reported, fails = run_harness(fake_args(fake, no_local=False, no_cargo=not cargo, json=out_json))
        t.check(status == 0 and not fails, "every case of every fixture, in every arm, meets its expectation", "; ".join(f"{c} [{a}] {w}" for c, a, w in fails[:6]))
        data = json.load(open(out_json))
        expected_cases = {c.full for f in fixtures() for c in f.cases}
        t.check(len(data["cases"]) == len(expected_cases)
                and {c["case"] for c in data["cases"]} == expected_cases and data["ok"],
                "--json has every case", str(len(data["cases"])))
        t.check(sum(1 for c in data["cases"] if c["observed"] and c["observed"].startswith("refused")) >= 14 and
                any(c["case"] == "profiles/llvm" and c["observed"].startswith("refused: RA_JIT_BACKEND") for c in data["cases"]),
                "refusals are in the results, with what blaze said (llvm refused: this fake has none)")
        if cargo:
            t.check(sum(1 for c in data["cases"] if c["units"] and c["units"]["status"] == "equal") > 40, "the units arm compared graphs",
                    str(collections.Counter((c["units"] or {}).get("status") for c in data["cases"])))
        t.check(sum(1 for c in data["cases"] if c["local"] and c["local"]["status"] == "match") >= 12, "the local arm ran its cases and found them equal",
                str(collections.Counter((c["local"] or {}).get("status") for c in data["cases"])))
        identity = {c["case"]: c for c in data["cases"]}
        t.check(all(identity[name]["observed"].startswith("changed") for name in
                    ("source/committed-body", "source/dirty-body", "nested/sibling"))
                and identity["source/moved-dirty-body"]["observed"] == "same"
                and all(identity[name]["local"]["status"] == "match" for name in
                        ("nested/base", "nested/sibling", "nested/below-sibling")),
                "the test double changes source/job identity and reproduces both nested workspaces; this is not a real CLI pass")
        if ctx.audit is not None:
            dispatched = sum((l["dispatched"]["command"] or {}).get("cargo", 0) for l in ctx.locals.values())
            t.check(dispatched >= len(ctx.locals), "cargo dispatched is counted by the stand-in and not a failure", str(dispatched))
        else:
            print(f"  skip the audit: {ctx.audit_why}")
        calls = [json.loads(l) for l in open(log)]
        guard = [c for c in calls if c["args"][:2] != ["cargo", "stop"] or True]
        t.check(all(c["env"]["RA_JIT_MEMORY_MAX"] == "16G" and c["env"]["RA_JIT_OWN_BASE_MEMORY"] == "12G" and c["env"]["RA_JIT_SPARE"] == "0"
                    and c["env"]["RUST_BACKTRACE"] == "0" and c["env"]["RA_JIT_RUN_ID"] for c in guard),
                f"every one of {len(calls)} runs had the memory guard, the run id and RUST_BACKTRACE=0")
        stops = [c for c in calls if c["args"][:2] == ["cargo", "stop"]]
        homes = {c["env"]["RA_JIT_HOME"] for c in calls if c["args"][:3] == ["cargo", "spec", "--run"]}
        t.check(len(stops) >= 2 * len(ctx.locals) and homes <= {s["env"]["RA_JIT_HOME"] for s in stops},
                "the daemon of every local run is stopped, with its own RA_JIT_HOME", f"{len(stops)} stops, {len(ctx.locals)} local pairs")
        t.check(all(not os.path.exists(h) for h in ctx.blaze.homes), "the daemons' homes are deleted")
        t.check(not os.path.exists(ctx.work), "the work directory is deleted")

        print("\nthe harness with LLVM carried (FAKE_LLVM=1): the llvm cases are changed, not refused:")
        with environment(FAKE_LOG=None, FAKE_BUG=None, FAKE_LLVM="1"):
            status, ctx, reported, fails = run_harness(fake_args(fake, only=["profiles"]), quiet=True)
        observed = {c.name: ctx.judged[c.full][0] for c in reported if c.full in ctx.judged}
        t.check(status == 0 and observed["llvm"].startswith("changed") and observed["llvm-opt1"].startswith("changed") and observed["llvm-opt2"].startswith("same"),
                "llvm is changed, opt 1 is changed from it, opt 2 (its default) is the same", f"{observed.get('llvm')} {observed.get('llvm-opt1')} {observed.get('llvm-opt2')} {fails[:3]}")

        print("\nthe harness catches a ra-jit that is wrong (each of these runs the fake with one bug):")
        bugs = [("verbose-changes", ["basic/verbose"], False, "wanted the same spec"),
                ("nofailfast-same", ["basic/no-fail-fast", "failfast"], False, "wanted a different spec"),
                ("no-lto-refusal", ["profiles/lto-thin"], False, "wanted a refusal"),
                ("pkg-override-all", ["profiles/pkg-opt1"], False, "the units whose profile differs"),
                ("head-only-source", ["source/dirty-body"], False, "wanted a different spec"),
                ("dirty-source-refused", ["source/dirty-body"], False, "a spec was wanted"),
                ("sibling-source-same", ["nested/sibling"], False, "wanted a different spec"),
                ("replay-git-root", ["nested/base"], True, "spec run"),
                ("spec-run-drops-no-run", ["basic/no-run"], True, "spec run"),
                ("unit-features", ["basic/base"], False, "features")]
        for bug, only, local, wanted in bugs:
            if bug == "unit-features" and not cargo:
                print(f"  skip {bug}: no cargo to be the oracle")
                continue
            with environment(FAKE_LOG=None, FAKE_BUG=bug, FAKE_LLVM=None):
                status, _, _, fails = run_harness(fake_args(fake, only=only, no_local=not local, no_cargo=bug != "unit-features"), quiet=True)
            hit = [f for f in fails if wanted in f[2]]
            t.check(status == 1 and hit, f"{bug}: the harness fails and says {wanted!r}", "; ".join(f"{c} [{a}] {w}" for c, a, w in fails[:3]) or "it passed")
        with environment(FAKE_LOG=None, FAKE_BUG=None, FAKE_LLVM=None):
            try:
                run_harness(fake_args(fake, only=["nonesuch"]), quiet=True)
                t.check(False, "--only of a fixture that is not there is an error")
            except HarnessError as e:
                t.check("nonesuch" in str(e), "--only of a fixture that is not there is an error")
            status, ctx, reported, fails = run_harness(fake_args(fake, only=["ws/in-a"]), quiet=True)
            t.check(status == 0 and [c.full for c in reported] == ["ws/in-a"] and "ws/build-p-a" in ctx.results,
                    "--only runs what a case is compared with and reports only the case")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"\nself-test: {t.total - t.bad} of {t.total} checks hold")
    return 1 if t.bad else 0


if __name__ == "__main__":
    sys.exit(main())
