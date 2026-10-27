#!/usr/bin/env python3
"""tests.py: the harness of the distributed program's tests phase (docs/distributed.md, section 6).

    python3 -I harness/dist/tests.py --contract [--before DIR]   unit T1: Bazel's test-result caching
    python3 -I harness/dist/tests.py --self-test                 the tables and the parsing, no daemon

RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it. `--before DIR` also runs the
opt-out and declared-input steps with the ra-jit and cargo-ra-jit in DIR (the commit before T1),
for what they did without the unit.

`--contract` runs `cargo ra-jit test` with `RA_JIT_TESTS=affected` on `harness/dist/fixtures/tests`,
edited a step at a time, and checks which tests run after each. It holds blaze to what Bazel's
`cache_test_results=auto` does and no more (docs/distributed.md, section 6, unit T1):

- a pass is reused while the test, the inputs its owner declared (`data` files, `env` variables of
  `blaze-tests.toml`) and how it is run (`--test-threads`) are the same, and only then;
- the test's code, a declared file, a declared variable or the run configuration changing reruns;
- `no_cache` and `external` in the manifest, Bazel's tags, make a test run every time;
- nothing is kept of a failed run, whatever the code goes back to;
- a test that fails runs again while it has attempts left (`flaky` in the manifest, three, and
  `RA_JIT_FLAKY_TEST_ATTEMPTS`), a pass that took more than one attempt is reported `Flaky` and is
  kept as a successful result, as Bazel does.

A test that reaches files or the environment runs every time where no sandbox records what it read
(macOS), unless the manifest declares `data`: then the declaration is what is trusted, as in
Bazel. Where a sandbox records (Linux), it is the sandbox that decides, as before the unit.

Every blaze run sets the guard of standing order 14 (`RA_JIT_MEMORY_MAX=16G`,
`RA_JIT_OWN_BASE_MEMORY=12G`, `RA_JIT_SPARE=0`) and is under `tracedecay.py`'s watchdog.

Pending, not failing: the steps do not run under P0-A's no-rustc audit (`norustc.py`); blaze's build
of a test program still starts from cargo's until that gate is met.

Python 3, stdlib only.
"""
import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
FIXTURE = os.path.join(HERE, "fixtures", "tests")
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HARNESS, "toolchain")).read().strip())
TIMEOUT = 900
RAN = re.compile(r"^test (\S+) \.\.\. (ok|ignored|FAILED)", re.M)
SANDBOXED = sys.platform == "linux" and os.environ.get("RA_JIT_SANDBOX") not in ("off", "0")

ALL = {"tests::unchanged", "tests::other", "tests::reads_data", "tests::reads_env", "tests::opted_out"}
# Reach files and the environment: run every time where nothing records what they read.
WORLD = {"tests::reads_data", "tests::reads_env"}
NONE = set()
MANIFEST = 'data = ["cache/data.txt"]\nenv = ["T1_DECLARED"]\n'
FLAKY_MANIFEST = 'data = ["flaky/data.txt"]\nflaky = ["tests::flaky_once"]\n'

# What each step of `--contract` does to the workspace, how it is run, and the tests that run.
# (what the step is, edits, manifest, args, extra environment, tests that run, outcome)
#   edits:    [(file, old, new)]       manifest: its text, or None to leave it as it is
#   outcome:  True passes, False fails
CACHE = [
    ("nothing edited, no manifest", [], None, [], {}, NONE if SANDBOXED else WORLD, True),
    ("a manifest declaring data and env", [], MANIFEST, [], {}, ALL, True),
    ("nothing edited", [], None, [], {}, NONE, True),
    ("a function the tests call (the code changed)", [("cache/src/lib.rs", "x * 2", "x + x")], None, [], {},
     {"tests::unchanged", "tests::opted_out"}, True),
    ("nothing edited", [], None, [], {}, NONE, True),
    ("a declared data file edited", [("cache/data.txt", "declared", "declared again")], None, [], {}, ALL, True),
    ("nothing edited", [], None, [], {}, NONE, True),
    ("a declared variable set", [], None, [], {"T1_DECLARED": "1"}, ALL, True),
    ("the same value again", [], None, [], {"T1_DECLARED": "1"}, NONE, True),
    ("the variable unset", [], None, [], {}, ALL, True),
    ("nothing edited", [], None, [], {}, NONE, True),
    ("another run configuration (--test-threads=2)", [], None, ["--test-threads=2"], {}, ALL, True),
    ("that configuration again", [], None, ["--test-threads=2"], {}, NONE, True),
    ("the first configuration again", [], None, [], {}, ALL, True),
    ("nothing edited", [], None, [], {}, NONE, True),
    ("a test opted out with no_cache", [], MANIFEST + 'no_cache = ["tests::opted_out"]\n', [], {},
     {"tests::opted_out"}, True),
    ("nothing edited, the opted out test still runs", [], None, [], {}, {"tests::opted_out"}, True),
    ("the test opted out with external", [], MANIFEST + 'external = ["tests::opted_out"]\n', [], {},
     {"tests::opted_out"}, True),
    ("the opt-out taken away", [], MANIFEST, [], {}, {"tests::opted_out"}, True),
    ("nothing edited", [], None, [], {}, NONE, True),
    ("a test that fails", [("cache/src/lib.rs", "double(2), 4", "double(2), 5")], None, [], {},
     {"tests::unchanged"}, False),
    ("nothing edited after it failed", [], None, [], {}, {"tests::unchanged"}, False),
    ("its code back to what passed before", [("cache/src/lib.rs", "double(2), 5", "double(2), 4")], None, [], {},
     {"tests::unchanged"}, True),
    ("nothing edited", [], None, [], {}, NONE, True),
    ("configured declared environment", [(".cargo/config.toml", 'T1_DECLARED = "first"', 'T1_DECLARED = "second"')],
     None, [], {}, ALL, True),
    ("configured environment unchanged", [], None, [], {}, NONE, True),
]
# What has to be said (-v) at a step, by its position in CACHE.
SAYS = {
    15: [r"Always tests::opted_out: `blaze-tests\.toml` opts it out of the cache \(`no_cache`: `tests::opted_out`\)"],
    16: [r"Always tests::opted_out: `blaze-tests\.toml` opts it out of the cache \(`no_cache`"],
    17: [r"Always tests::opted_out: `blaze-tests\.toml` opts it out of the cache \(`external`"],
}


class Run:
    def __init__(self, code, out):
        self.code, self.out = code, out
        self.outcomes = RAN.findall(out)
        self.terminal = dict(self.outcomes)
        self.ran = {name for name, _ in self.outcomes}
        # Retry output includes a failure followed by a pass of the same test. Its final
        # outcome, not whether it ever failed, decides the job's result.
        self.failed = {name for name, end in dict(self.outcomes).items() if end == "FAILED"}


class Harness:
    def __init__(self, bin_dir, keep):
        self.bin_dir, self.keep = bin_dir, keep
        self.root = tempfile.mkdtemp(prefix="ra-jit-tests-contract-")
        # The daemon's sockets are under its home, and a socket's path is short (104 bytes on macOS).
        self.home = tempfile.mkdtemp(prefix="rj-tc-", dir="/tmp")
        self.ws = os.path.join(self.root, "tests")
        shutil.copytree(FIXTURE, self.ws)
        self.wrong = self.total = 0
        self.watchdog = self.guard_env = None
        self.env = self.make_env()

    def make_env(self):
        env = dict(os.environ, PATH=f"{self.bin_dir}{os.pathsep}{os.environ['PATH']}", RA_JIT_HOME=self.home,
                   RUST_BACKTRACE="0")
        for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RA_JIT_TESTS",
                     "RA_JIT_FLAKY_TEST_ATTEMPTS", "RA_JIT_TEST_ENV", "T1_DECLARED"):
            env.pop(name, None)
        # The guard of standing order 14, and the watchdog over everything this run starts.
        sys.path.insert(0, HARNESS)
        import tracedecay
        run_id = "t1-%d-%d" % (int(time.time()), os.getpid())
        env.update(tracedecay.GUARD_ENV)
        env[tracedecay.RUN_ID_ENV] = run_id
        self.watchdog = tracedecay.Watchdog(self.bin_dir, tracedecay.WATCHDOG_MB, tracedecay.SWAP_FREE_MIN_MB, 2.0,
                                            run_id=run_id, homes=[self.home])
        self.watchdog.start()
        return env

    def write(self, path, text):
        os.makedirs(os.path.dirname(os.path.join(self.ws, path)), exist_ok=True)
        open(os.path.join(self.ws, path), "w").write(text)

    def edit(self, path, old, new):
        text = open(os.path.join(self.ws, path)).read()
        assert old in text, (path, old)
        open(os.path.join(self.ws, path), "w").write(text.replace(old, new, 1))

    def cargo(self, mode, package, args=(), positional=(), verbose=False, extra=None):
        cmd = ["cargo", "ra-jit", "test", "-p", package, *(["-v"] if verbose else []), *positional]
        if args:
            cmd += ["--", *args]
        proc = subprocess.Popen(cmd, cwd=self.ws, env=dict(self.env, RA_JIT_TESTS=mode, **(extra or {})),
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.watchdog.client = lambda: proc.pid
        try:
            out, _ = proc.communicate(timeout=TIMEOUT)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.communicate()
            raise
        finally:
            self.watchdog.client = None
        if self.watchdog.tripped:
            raise SystemExit(f"THE WATCHDOG KILLED THE RUN: {self.watchdog.tripped}")
        return Run(proc.returncode, out)

    def warm(self, package, positional=(), extra=None):
        """Runs until the daemon serves the build, as testselect.py does, and once more: the
        daemon's first build of the tests is one in which each is new to it."""
        deadline = time.monotonic() + 300
        while time.monotonic() < deadline:
            run = self.cargo("all" if positional else "affected", package, positional=positional, extra=extra)
            if run.code != 0:
                raise SystemExit(f"the tests failed before any edit:\n{run.out[-3000:]}")
            if "[ra-jit]" in run.out:
                break
            time.sleep(0.5)
        else:
            raise SystemExit(f"no build said `[ra-jit]`; the daemon's log is in {self.home}\n{run.out[-4000:]}")
        if not positional:
            self.cargo("affected", package, extra=extra)

    def check(self, what, verdict):
        self.total += 1
        self.wrong += verdict != "ok"
        print(f"{what:62} {verdict}", flush=True)
        return verdict == "ok"

    def verdict(self, run, expected, passes, says=(), served=True, failed=None):
        if failed is None and (passes or len(expected) == 1):
            failed = NONE if passes else expected
        if served and "[ra-jit]" not in run.out:
            return "NOT SERVED BY THE DAEMON"
        if run.ran != expected:
            extra, missing = sorted(run.ran - expected), sorted(expected - run.ran)
            return f"RAN {', '.join(extra) or 'nothing more'}; DID NOT RUN {', '.join(missing) or '-'}"
        if any(run.terminal.get(name) not in ("ok", "FAILED") for name in expected):
            return "EXPECTED EXECUTED TEST WAS IGNORED"
        if failed is not None and run.failed != failed:
            return f"WRONG TERMINAL FAILURES: {sorted(run.failed)}"
        if (run.code == 0) != passes or bool(run.failed) == passes:
            return f"ENDED {run.code} WITH {', '.join(sorted(run.failed)) or 'no test'} FAILED"
        if unsaid := [it for it in says if not re.search(it, run.out, re.M)]:
            return f"DID NOT SAY {' NOR '.join(unsaid)}"
        return "ok"

    def contract(self):
        self.warm("cache")
        for at, (what, edits, manifest, args, extra, expected, passes) in enumerate(CACHE):
            for path, old, new in edits:
                self.edit(path, old, new)
            if manifest is not None:
                self.write("blaze-tests.toml", manifest)
            says = SAYS.get(at, [])
            run = self.cargo("affected", "cache", args, verbose=bool(says), extra=extra)
            if what == "configured declared environment" and "[ra-jit]" not in run.out:
                # Cargo configuration invalidates the whole build state. The cold build
                # must rerun every test; warm its replacement state before checking reuse.
                verdict = self.verdict(run, expected, passes, says, served=False)
                self.check(what, verdict)
                self.warm("cache")
            else:
                self.check(what, self.verdict(run, expected, passes, says))
        self.regressions()
        self.flaky()

    def regressions(self):
        poison = {"T1_DECLARED": "poison"}
        run = self.cargo("affected", "cache", extra=poison)
        self.check("declared inherited poison fails its assertion",
                   self.verdict(run, ALL, False, failed={"tests::reads_env"}))
        run = self.cargo("affected", "cache", extra=poison)
        self.check("poison failure is never reused",
                   self.verdict(run, {"tests::reads_env"}, False, failed={"tests::reads_env"}))
        self.write(".cargo/config.toml", '[env]\nT1_DECLARED = { value = "forced", force = true }\n')
        run = self.cargo("affected", "cache", extra=poison)
        self.check("forced configuration overrides inherited poison",
                   self.verdict(run, ALL, True, served=False, failed=NONE))

        self.write("blaze-tests.toml", 'data = ["tree/d"]\n')
        off = {"RA_JIT_SANDBOX": "off"}
        self.warm("tree", extra=off)
        self.check("unchanged directory tree pass is reused",
                   self.verdict(self.cargo("affected", "tree", extra=off), NONE, True))
        os.rename(os.path.join(self.ws, "tree/d/a/b"), os.path.join(self.ws, "tree/d/b"))
        run = self.cargo("affected", "tree", extra={"RA_JIT_SANDBOX": "off"})
        self.check("directory move with empty child reruns and fails",
                   self.verdict(run, {"tests::nested_file"}, False, failed={"tests::nested_file"}))
        run = self.cargo("affected", "tree", extra={"RA_JIT_SANDBOX": "off"})
        self.check("directory move failure is not cached",
                   self.verdict(run, {"tests::nested_file"}, False, failed={"tests::nested_file"}))

    def flaky(self):
        """A test that fails on its first attempt: retried while the manifest or the environment
        gives it attempts, reported, and kept as a successful result."""
        self.write("blaze-tests.toml", FLAKY_MANIFEST)
        # Linux's sandbox keeps what a test writes to itself, and the test counts its attempts in a file.
        off = {"RA_JIT_SANDBOX": "off"}
        attempts = os.path.join(self.ws, "flaky", "attempts.txt")
        self.warm("flaky", positional=["steady"])
        both = {"tests::flaky_once", "tests::steady"}
        if os.path.exists(attempts):
            os.remove(attempts)
        run = self.cargo("affected", "flaky", verbose=True, extra=off)
        verdict = self.verdict(run, both, True, [r"Retrying .*\(attempt 2\)", r"Flaky .*tests::flaky_once: passed on attempt 2"])
        if verdict == "ok" and [end for name, end in run.outcomes if name == "tests::flaky_once"] != ["FAILED", "ok"]:
            verdict = "THE TEST DID NOT FAIL ONCE AND THEN PASS"
        self.check("a test that fails once is retried, and passes", verdict)
        run = self.cargo("affected", "flaky", verbose=True, extra=off)
        verdict = self.verdict(run, NONE, True)
        if verdict == "ok" and "Retrying" in run.out:
            verdict = "RETRIED"
        self.check("what passed after two attempts is cached", verdict)
        self.check("nothing edited", self.verdict(self.cargo("affected", "flaky", extra=off), NONE, True))
        self.edit("flaky/data.txt", "declared", "declared again")
        os.remove(attempts)
        run = self.cargo("affected", "flaky", verbose=True, extra=dict(off, RA_JIT_FLAKY_TEST_ATTEMPTS="1"))
        verdict = self.verdict(run, both, False, failed={"tests::flaky_once"})
        if verdict == "ok" and ("Retrying" in run.out or run.failed != {"tests::flaky_once"}):
            verdict = "RETRIED OR FAILED THE WRONG TEST"
        self.check("one attempt asked for: it fails", verdict)
        run = self.cargo("affected", "flaky", extra=off)
        self.check("the failure was not kept: it runs, and passes", self.verdict(run, {"tests::flaky_once"}, True))
        self.check("nothing edited", self.verdict(self.cargo("affected", "flaky", extra=off), NONE, True))
        self.edit("flaky/src/lib.rs", "assert_eq!(1 + 1, 2)", "assert_eq!(1 + 1, 3)")
        self.edit("flaky/data.txt", "declared again", "mixed failures")
        os.remove(attempts)
        run = self.cargo("affected", "flaky", verbose=True, extra=off)
        verdict = self.verdict(run, both, False, [r"Retrying .*\(attempt 2\)", r"Flaky .*tests::flaky_once"],
                               failed={"tests::steady"})
        if verdict == "ok" and [end for name, end in run.outcomes if name == "tests::flaky_once"] != ["FAILED", "ok"]:
            verdict = "THE NON-FLAKY FAILURE BLOCKED THE FLAKY RETRY"
        self.check("a non-flaky failure does not block a flaky retry", verdict)
        run = self.cargo("affected", "flaky", extra=off)
        self.check("only the exhausted failure runs again", self.verdict(run, {"tests::steady"}, False))

    def before(self):
        """The opt-out and the declared inputs with the ra-jit before the unit: ignored."""
        self.write("blaze-tests.toml", MANIFEST + 'no_cache = ["tests::opted_out"]\n')
        self.warm("cache")
        what = "before the unit: a manifest that opts a test out"
        run = self.cargo("affected", "cache")
        self.check(what, "ok" if run.ran == (NONE if SANDBOXED else WORLD) else f"RAN {sorted(run.ran)}")
        self.edit("cache/data.txt", "declared", "declared again")
        run = self.cargo("affected", "cache")
        what = "before the unit: a declared data file edited"
        self.check(what, "ok" if run.ran == (NONE if SANDBOXED else WORLD) else f"RAN {sorted(run.ran)}")

    def finish(self):
        subprocess.run(["cargo", "ra-jit", "stop"], cwd=self.ws, env=self.env, capture_output=True, timeout=120)
        self.watchdog.stop.set()
        if not self.wrong and not self.keep:
            shutil.rmtree(self.root, ignore_errors=True)
            shutil.rmtree(self.home, ignore_errors=True)
        return self.wrong


def tools(bin_dir):
    for name in ("cargo-ra-jit", "ra-jit-rustc"):
        if not os.path.exists(os.path.join(bin_dir, name)):
            raise SystemExit(f"no {name} beside {bin_dir}/ra-jit; supply the complete built tool directory")
    if SANDBOXED:
        probe = subprocess.run([os.path.join(bin_dir, "ra-jit"), "sandbox", "--probe"], capture_output=True, text=True)
        if probe.returncode != 0:
            raise SystemExit(f"there is no sandbox for the tests here (`RA_JIT_SANDBOX=off` tests without):\n"
                             f"{probe.stderr.strip()}")


def contract(args):
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    tools(bin_dir)
    h = Harness(bin_dir, args.keep)
    try:
        h.contract()
    finally:
        wrong = h.finish()
    print(f"{h.total - wrong} of {h.total} right; under {h.root}, the daemon's home {h.home}")
    print("pending: the no-rustc audit (P0-A): the steps are not run under norustc.py's stand-ins")
    if args.before:
        before = Harness(os.path.abspath(args.before), args.keep)
        try:
            before.before()
        finally:
            wrong += before.finish()
    sys.exit(1 if wrong else 0)


def self_test():
    """The tables are consistent with the fixture, and the parsing reads what libtest prints."""
    wrong = []
    files = {}
    lib = open(os.path.join(FIXTURE, "cache", "src", "lib.rs")).read()
    tests = {"tests::" + name for name in re.findall(r"#\[test\]\s+fn (\w+)", lib)}
    if tests != ALL:
        wrong.append(f"ALL is not the fixture's tests: {sorted(tests ^ ALL)}")
    if not WORLD <= ALL:
        wrong.append("WORLD is not among the tests")
    # The edits apply in order, each to the text the one before left.
    for what, edits, *_ in CACHE:
        for path, old, new in edits:
            text = files.get(path) or open(os.path.join(FIXTURE, path)).read()
            if old not in text:
                wrong.append(f"{what}: {old!r} is not in {path} at that step")
            files[path] = text.replace(old, new, 1)
    for expected in (row[5] for row in CACHE):
        if not expected <= ALL:
            wrong.append(f"a step expects tests that are not there: {sorted(expected - ALL)}")
    run = Run(0, "test tests::a ... ok\ntest tests::b ... FAILED\ntest tests::c ... ignored\nnoise\n")
    if run.ran != {"tests::a", "tests::b", "tests::c"} or run.failed != {"tests::b"}:
        wrong.append("the parsing of libtest's lines is wrong")
    retried = Run(0, "test a ... FAILED\ntest b ... FAILED\ntest a ... ok\n")
    if retried.failed != {"b"}:
        wrong.append("a passed retry was counted as a final failure")
    h = Harness.__new__(Harness)
    for out, passes in (("test a ... ignored\n", True), ("test a ... FAILED\n", True),
                        ("test a ... ok\n", False)):
        for served in (True, False):
            run = Run(0, "[ra-jit]\n" + out)
            if h.verdict(run, {"a"}, passes, served=served) == "ok":
                wrong.append("ignored/inconsistent terminal outcome accepted")
    if h.verdict(Run(1, "[ra-jit]\ntest a ... FAILED\n"), {"a"}, False, failed={"b"}) == "ok":
        wrong.append("wrong failing test accepted")
    for at in SAYS:
        if not 0 <= at < len(CACHE) or "opt" not in CACHE[at][0]:
            wrong.append(f"SAYS names step {at}, which is not one that opts a test out")
    for line in wrong:
        print("WRONG:", line)
    print("ok" if not wrong else f"{len(wrong)} wrong")
    sys.exit(1 if wrong else 0)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--contract", action="store_true", help="unit T1: Bazel's test-result caching")
    parser.add_argument("--before", metavar="DIR", help="with --contract: also run the opt-out steps with the ra-jit in DIR")
    parser.add_argument("--keep", action="store_true", help="keep the workspace and daemon home even when right")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
    elif args.contract:
        contract(args)
    else:
        parser.error("say what to run: --contract or --self-test")


if __name__ == "__main__":
    main()
