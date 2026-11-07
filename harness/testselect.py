#!/usr/bin/env python3
"""testselect.py: `cargo ra-jit test` with `RA_JIT_TESTS=affected` runs the tests an edit
reached since they last passed, and no others.

usage: testselect.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

A package has a library with unit tests and an integration test of it, and a library of the
workspace beside it that both link, which says under `cfg(test)` what it does not otherwise
(an edit to it is the daemon's, built as rustc built it for the tests): tests that call one
function or another of the library, one in a private module named as a public function beside
it, one that downcasts a `dyn Any` of what one returns (a
`TypeId`, which code generation takes from rustc), one that keeps it in a `HashMap` (seeded at
random), one that prints a backtrace (which reads the environment and the working directory),
one that reads a file, one that calls nothing. It is tested until the daemon serves the build,
then edited a step at a time, and after each the tests that ran have to be the ones the step
names: those whose code or what it calls changed, those added or given other attributes, the
one that reads a file every time, the one that failed until it passes (and not the others that
ran and passed beside it), one added to a program with an `ignore` with a list, one added to a library with a bench (in
a package of its own, built with RUSTC_BOOTSTRAP=1), and with
`RA_JIT_TESTS=all` every test. A test that ran has to end as its source says. With `-v` the
output says why the one that reads a file runs, by the calls from it to the file system, and
how many tests run every time.

Where tests that reach the world outside their code run sandboxed (Linux, unless
`RA_JIT_SANDBOX=off`), the one that reads a file runs again only when the file changed, `-v`
says which file, and a test that writes into the package changes nothing of it outside its run.

Python 3, stdlib only.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from dist.tests import owned_plan
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
TIMEOUT = 900
LIB = """pub fn helper(x: u32) -> u32 {
    x + 1
}

pub fn other(x: u32) -> u32 {
    x * 2
}

// A private module and a public function of one name, the test reached through the module, as
// helix-core's `increment::date_time`.
pub mod shapes {
    mod twice {
        pub fn twice(x: u32) -> u32 {
            2 * x
        }

        #[cfg(test)]
        mod tests {
            #[test]
            fn doubles() {
                assert_eq!(super::twice(2), 4);
            }
        }
    }

    pub fn twice(x: u32) -> u32 {
        twice::twice(x)
    }
}

pub fn based(x: u32) -> u32 {
    tbase::base(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_helper() {
        assert_eq!(helper(1), 2);
    }

    #[test]
    fn uses_other() {
        assert_eq!(other(2), 4);
    }

    #[test]
    fn uses_base() {
        assert_eq!(based(1), 3);
    }

    #[test]
    fn uses_any() {
        let value: Box<dyn std::any::Any> = Box::new(helper(1));
        assert_eq!(value.downcast_ref::<u32>(), Some(&2));
    }

    #[test]
    fn uses_a_map() {
        let mut map = std::collections::HashMap::new();
        map.insert(helper(1), "two");
        assert_eq!(map.get(&2), Some(&"two"));
    }

    #[test]
    fn shows_a_backtrace() {
        let shown = std::backtrace::Backtrace::capture().to_string();
        assert_eq!(helper(shown.len().min(0) as u32), 1);
    }

    #[test]
    fn reads_a_file() {
        assert!(std::fs::read_to_string("data.txt").unwrap().contains("tsel"));
    }

    #[test]
    fn plain() {
        assert_eq!(1 + 1, 2);
    }
}
"""
BASE = """pub fn base(x: u32) -> u32 {
    x + 2 + offset()
}

#[cfg(not(test))]
fn offset() -> u32 {
    0
}

#[cfg(test)]
fn offset() -> u32 {
    100
}
"""
FILES = {
    "Cargo.toml": '[package]\nname = "tsel"\nversion = "0.1.0"\nedition = "2021"\n\n'
                  '[dependencies]\ntbase = { path = "tbase" }\n\n[workspace]\nmembers = ["tbase"]\n',
    "src/lib.rs": LIB,
    "data.txt": "tsel\n",
    # A library of the workspace the unit tests are linked with as rustc built it, without
    # `cfg(test)`, as ripgrep's crates are each other's.
    "tbase/Cargo.toml": '[package]\nname = "tbase"\nversion = "0.1.0"\nedition = "2021"\n',
    "tbase/src/lib.rs": BASE,
    # An `ignore` with a list after it, which rustc reads as one without a message.
    "tests/it.rs": """#![allow(ill_formed_attribute_input)]

#[test]
#[ignore(not_yet)]
fn it_listed() {}

#[test]
fn it_helper() {
    assert_eq!(tsel::helper(0), 1);
}

#[test]
fn it_other() {
    assert_eq!(tsel::other(0), 0);
}

#[test]
fn it_base() {
    assert_eq!(tsel::based(0), 2);
}
""",
}
CALL_HELPER = {"tests::uses_helper", "tests::uses_any", "tests::uses_a_map", "tests::shows_a_backtrace", "it_helper"}
CALL_BASE = {"tests::uses_base", "it_base"}
ALL = CALL_HELPER | CALL_BASE | {"tests::uses_other", "tests::reads_a_file", "tests::plain", "it_helper", "it_other",
                                 "shapes::twice::tests::doubles", "it_listed"}
FILE = "tests::reads_a_file"
WRITES = "tests::writes_a_file"
SANDBOXED = sys.platform == "linux" and os.environ.get("RA_JIT_SANDBOX") not in ("off", "0")
# The test that reads a file, in a step that did not change the file: where it runs sandboxed,
# it is skipped like any other unchanged test.
F = set() if SANDBOXED else {FILE}
# (what the step is, [(file, old, new)], RA_JIT_TESTS, the tests that run, whether they pass)
STEPS = [
    ("nothing edited", [], "affected", F, True),
    ("a function one test calls", [("src/lib.rs", "x + 1", "x + 2 - 1")], "affected",
     CALL_HELPER | F, True),
    ("a function another test calls", [("src/lib.rs", "x * 2", "x + x")], "affected",
     {"tests::uses_other", "it_other"} | F, True),
    ("a library the unit tests link", [("tbase/src/lib.rs", "x + 2", "x + 3 - 1")], "affected",
     CALL_BASE | F, True),
    ("that library again", [("tbase/src/lib.rs", "x + 3 - 1", "x + 4 - 2")], "affected",
     CALL_BASE | F, True),
    ("a test added", [("src/lib.rs", "    #[test]\n    fn plain()",
                       "    #[test]\n    fn added() {\n        assert_eq!(helper(2), 3);\n    }\n\n"
                       "    #[test]\n    fn plain()")], "affected",
     {"tests::added"} | F, True),
    ("a test added beside an ignore with a list",
     [("tests/it.rs", "#[test]\nfn it_base()", "#[test]\nfn it_added() {\n    assert_eq!(1 + 1, 2);\n}\n\n#[test]\nfn it_base()")],
     "affected", {"it_added"} | F, True),
    ("a test ignored", [("src/lib.rs", "    #[test]\n    fn plain()", "    #[test]\n    #[ignore]\n    fn plain()")],
     "affected", {"tests::plain"} | F, True),
    ("a test that fails", [("src/lib.rs", "other(2), 4", "other(2), 5")], "affected",
     {"tests::uses_other"} | F, False),
    ("nothing edited after it failed", [], "affected", {"tests::uses_other"} | F, False),
    ("the test fixed", [("src/lib.rs", "other(2), 5", "other(2), 4")], "affected",
     {"tests::uses_other"} | F, True),
    # The unit tests fail, so cargo runs no integration test after them.
    ("one of several reached failing", [("src/lib.rs", "x + 2 - 1", "x + 3 - 2"),
                                         ("src/lib.rs", "assert_eq!(helper(1), 2);", "assert_eq!(helper(1), 3);")],
     "affected", CALL_HELPER - {"it_helper"} | {"tests::added"} | F, False),
    ("nothing edited after one failed", [], "affected", {"tests::uses_helper"} | F, False),
    ("that one fixed", [("src/lib.rs", "assert_eq!(helper(1), 3);", "assert_eq!(helper(1), 2);")],
     "affected", {"tests::uses_helper", "it_helper"} | F, True),
    ("every test asked for", [], "all", ALL | {"tests::added", "it_added"}, True),
    ("nothing edited again", [], "affected", F, True),
    ("why a test runs every time", [], "affected -v", F, True),
]
if SANDBOXED:
    STEPS += [
        ("the file a test reads edited", [("data.txt", "tsel", "tsel tsel")], "affected -v", {FILE}, True),
        ("nothing edited after the file", [], "affected", set(), True),
        ("a test that writes into the package", [
            ("src/lib.rs", "    #[test]\n    fn added()",
             "    #[test]\n    fn writes_a_file() {\n"
             "        std::fs::write(\"data.txt\", \"written\").unwrap();\n"
             "        std::fs::write(\"written.txt\", \"written\").unwrap();\n"
             "        assert_eq!(std::fs::read_to_string(\"data.txt\").unwrap(), \"written\");\n"
             "    }\n\n    #[test]\n    fn added()")], "affected", {WRITES}, True),
        ("nothing edited after it wrote", [], "affected", set(), True),
    ]
# What a step's output has to say, beside the tests it runs.
SAYS = {} if SANDBOXED else {
    "why a test runs every time": [
        r"Always tests::reads_a_file: it reaches `\w+`, which reads or writes files, by "
        r"tsel::tests::reads_a_file -> std::fs::read_to_string::<…> -> .* -> \w+$",
        r"Skipped \d+ tests, unchanged since the last pass; 1 test run every time \(said above\);",
    ],
}
if SANDBOXED:
    SAYS = {
        "the file a test reads edited": [
            r"Sandboxed tests::reads_a_file: it read `[^`]*/data\.txt`, which changed$",
        ],
    }
# What has to be so of the package after a step, beside what it says.
AFTER = {
    "a test that writes into the package": lambda ws: (
        open(os.path.join(ws, "data.txt")).read() == "tsel tsel\n"
        and not os.path.exists(os.path.join(ws, "written.txt"))) or "IT WROTE INTO THE PACKAGE",
}
RAN = re.compile(r"^test (\S+) \.\.\. (ok|ignored|FAILED)", re.M)
# A library with a `#[bench]`, which is nightly's: built with RUSTC_BOOTSTRAP=1.
BENCHED = {
    "Cargo.toml": '[package]\nname = "benched"\nversion = "0.1.0"\nedition = "2021"\n',
    "src/lib.rs": """#![feature(test)]
extern crate test;

pub fn add(x: u32) -> u32 {
    x + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[bench]
    fn bench_add(b: &mut test::Bencher) {
        b.iter(|| add(1))
    }

    #[test]
    fn adds() {
        assert_eq!(add(1), 2);
    }
}
""",
}
BENCHED_ADDED = ("src/lib.rs", "    #[test]\n    fn adds()",
                 "    #[test]\n    fn added() {\n        assert_eq!(add(2), 3);\n    }\n\n    #[test]\n    fn adds()")


def benched(root, env):
    """A test added to a program with a bench: what the step's line says."""
    ws, env = os.path.join(root, "benched"), dict(env, RUSTC_BOOTSTRAP="1")
    for path, text in BENCHED.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    code, said = test(ws, env, "all")
    if code != 0 or not owned_plan(said):
        return f"INITIAL OWNED PLAN FAILED: {said[-3000:]}"
    path, old, new = BENCHED_ADDED
    text = open(os.path.join(ws, path)).read()
    open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))
    code, said = test(ws, env, "all")
    ran = {name for name, _ in RAN.findall(said)}
    if not owned_plan(said):
        return "NO COMPLETED OWNED TEST PLAN"
    if ran != {"tests::added", "tests::adds", "tests::bench_add"} or code != 0:
        return f"ENDED {code} HAVING RUN {', '.join(sorted(ran)) or 'nothing'}"
    return "ok"


def test(ws, env, mode):
    mode, *args = mode.split()
    ran = subprocess.run(["cargo", "ra-jit", "test", *args], cwd=ws, env=dict(env, RA_JIT_TESTS=mode),
                         capture_output=True, text=True, timeout=TIMEOUT)
    return ran.returncode, ran.stdout + ran.stderr


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "cargo-ra-jit")):
        raise SystemExit(f"no cargo-ra-jit beside {os.environ['RA_JIT_BIN']}")
    if SANDBOXED:
        probe = subprocess.run([os.environ["RA_JIT_BIN"], "sandbox", "--probe"], capture_output=True, text=True)
        if probe.returncode != 0:
            raise SystemExit(f"there is no sandbox for the tests here (`RA_JIT_SANDBOX=off` tests without):\n"
                             f"{probe.stderr.strip()}")
    root = tempfile.mkdtemp(prefix="ra-jit-testselect-")
    # The daemon's sockets are under its home, and a socket's path is short (104 bytes on macOS).
    ws, home = os.path.join(root, "tsel"), tempfile.mkdtemp(prefix="rj-ts-", dir="/tmp")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", RA_JIT_HOME=home,
               RUST_BACKTRACE="0", RA_JIT_STATS="1", RA_JIT_CACHE=os.path.join(home, "store"))
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RA_JIT_TESTS"):
        env.pop(name, None)
    wrong = 0
    try:
        code, said = test(ws, env, "affected")
        if code != 0 or not owned_plan(said, 2):
            raise SystemExit(f"the initial owned test plan failed:\n{said[-4000:]}")
        code, said = test(ws, env, "affected")
        ran = {name for name, _ in RAN.findall(said)}
        if code != 0 or not owned_plan(said, 2) or ran != F:
            raise SystemExit(f"the unchanged owned test plan did not reuse its passes:\n{said[-4000:]}")
        for what, edits, mode, expected, passes in STEPS:
            for path, old, new in edits:
                text = open(os.path.join(ws, path)).read()
                assert old in text, (what, old)
                open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))
            code, said = test(ws, env, mode)
            ran = {name for name, _ in RAN.findall(said)}
            failed = {name for name, end in RAN.findall(said) if end == "FAILED"}
            verdict = "ok"
            if not owned_plan(said, 2):
                verdict = "NO COMPLETED OWNED TEST PLAN"
            elif ran != expected:
                extra, missing = sorted(ran - expected), sorted(expected - ran)
                verdict = f"RAN {', '.join(extra) or 'nothing more'}; DID NOT RUN {', '.join(missing) or '-'}"
            elif code != (0 if passes else 101) or (failed == set()) != passes:
                verdict = f"ENDED {code} WITH {', '.join(sorted(failed)) or 'no test'} FAILED"
            elif unsaid := [it for it in SAYS.get(what, []) if not re.search(it, said, re.M)]:
                verdict = f"DID NOT SAY {' NOR '.join(unsaid)}"
            elif what in AFTER and (after := AFTER[what](ws)) is not True:
                verdict = after
            wrong += verdict != "ok"
            print(f"{what:34} {verdict}", flush=True)
            if verdict != "ok" and os.environ.get("TESTSELECT_VERBOSE"):
                print(said[-4000:])
        verdict = benched(root, env)
        wrong += verdict != "ok"
        print(f"{'a test added beside a bench':34} {verdict}", flush=True)
    finally:
        subprocess.run(["cargo", "ra-jit", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    print(f"{len(STEPS) + 1 - wrong} of {len(STEPS) + 1} right; under {root}, the daemon's home {home}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)
    shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
