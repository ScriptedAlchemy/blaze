#!/usr/bin/env python3
"""unittests.py: the crates' own tests, with the ones known to fail named.

usage: unittests.py [cargo test argument ...]

Runs `cargo test --release --no-fail-fast` of the crates below. A failing test that is not in
OPEN fails the run, and so does a test in OPEN that passes or no longer exists: take it out.
`hir-ide` is rust-analyzer's type inference with its own tests; the ones in OPEN are what the
fork does differently from upstream that its expectations have not caught up with.
"""
import json, os, re, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATES = ["hir-ty", "hir-ty-borrowck", "hir-ty-wfcheck", "hir-def", "parser", "syntax", "hir-ide", "hir-ty-codegen", "ra-jit-store", "blaze-registry", "blaze-resolve", "blaze-build-script", "blaze-manifest", "ra-jit"]

# crate::test, as libtest names it under the crate's test binary.
OPEN = {
    "hir_ide::tests::consteval::bit_op",
    "hir_ide::tests::display_source_code::render_dyn_ty_independent_of_order",
    "hir_ide::tests::display_source_code::render_dyn_ty_under_enclosing_binder",
    "hir_ide::tests::incremental::add_struct_invalidates_trait_solve",
    "hir_ide::tests::incremental::adding_enum_query_log",
    "hir_ide::tests::incremental::adding_impl_query_log",
    "hir_ide::tests::incremental::adding_struct_invalidates_infer",
    "hir_ide::tests::incremental::adding_use_query_log",
    "hir_ide::tests::mir::eval::caller_location_intrinsic",
    "hir_ide::tests::regression::issue_6628",
    "hir_ide::tests::regression::new_solver::trait_solving_with_error",
    "hir_ide::tests::regression::regression_21605",
    "hir_ide::tests::simple::box_into_vec",
    "hir_ide::tests::traits::assoc_types_from_bounds",
    "hir_ide::tests::traits::closure_signature_from_struct_where_clause",
    "hir_ide::tests::traits::infer_box_fn_arg",
    "hir_ide::tests::traits::infer_from_return_pos_impl_trait",
    "hir_ide::tests::traits::nested_impl_traits",
}

COLOR = re.compile(r"\x1b\[[0-9;]*m")
RUNNING = re.compile(r"^\s+Running .*[/\\]deps[/\\](\w+?)-[0-9a-f]{16}")
DOCTESTS = re.compile(r"^\s+Doc-tests (\w+)")
RESULT = re.compile(r"^test (\S+)(?: - .*)? \.\.\. (ok|FAILED|ignored)")


SUMMARY = re.compile(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;")


class Transcript:
    """Known assertion failures never excuse a failed build or an incomplete test process."""
    def __init__(self):
        self.seen, self.failed = set(), set()
        self.build_finished = False
        self.errors = []
        self.suite = None
        self.suites = 0

    def end_suite(self):
        if self.suite is not None:
            self.errors.append(f"{self.suite['name']}: no complete libtest summary")
            self.suite = None

    def feed(self, line):
        line = COLOR.sub("", line)
        # Cargo's build status is independent of libtest's ordinary text output.
        # Do not use json-render-diagnostics: we need structured compiler errors too.
        if line.startswith("{"):
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                message = None
            if isinstance(message, dict):
                reason = message.get("reason")
                if reason == "build-finished":
                    self.build_finished = message.get("success") is True
                    if not self.build_finished:
                        self.errors.append("Cargo did not finish building every test target")
                    return
                if reason == "compiler-message":
                    diagnostic = message.get("message", {})
                    if diagnostic.get("level") in ("error", "failure-note"):
                        self.errors.append("compiler: " + diagnostic.get("message", "error"))
                    return
        if m := RUNNING.match(line) or DOCTESTS.match(line):
            self.end_suite()
            self.suite = {"name": m.group(1), "outcomes": {}}
            self.suites += 1
        elif m := RESULT.match(line):
            if self.suite is None:
                self.errors.append("test result outside a running test target")
                return
            name = f"{self.suite['name']}::{m.group(1)}"
            if name in self.suite["outcomes"]:
                self.errors.append(f"duplicate terminal result: {name}")
            self.suite["outcomes"][name] = m.group(2)
            self.seen.add(name)
            if m.group(2) == "FAILED":
                self.failed.add(name)
        elif m := SUMMARY.match(line):
            if self.suite is None:
                self.errors.append("libtest summary without a running target")
                return
            outcomes = list(self.suite["outcomes"].values())
            counts = tuple(outcomes.count(kind) for kind in ("ok", "FAILED", "ignored"))
            if counts != tuple(map(int, m.group(2, 3, 4))) or (m.group(1) == "FAILED") != bool(counts[1]):
                self.errors.append(f"{self.suite['name']}: incomplete or inconsistent libtest results")
            self.suite = None
        elif self.suite is None and (
                re.match(r"^error(?:\[E\d+\])?:", line)
                and not re.match(r"^error: (?:(?:doc )?test failed, to rerun pass |\d+ targets? failed:)", line)
                or "process didn't exit successfully" in line):
            self.errors.append(line.strip())

    def finish(self, status, known):
        self.end_suite()
        if not self.build_finished:
            self.errors.append("missing successful Cargo build-finished record")
        if not self.suites or not self.seen:
            self.errors.append("no test results were executed")
        expected = 101 if self.failed else 0
        if status != expected:
            self.errors.append(f"cargo exited {status}, expected {expected} for the recorded tests")
        for name in sorted(self.failed - known):
            self.errors.append(f"{name} fails, and is not in OPEN in harness/unittests.py")
        for name in sorted(known & self.seen - self.failed):
            self.errors.append(f"{name} passes or is ignored now: take it out of OPEN in harness/unittests.py")
        for name in sorted(known - self.seen):
            self.errors.append(f"{name} did not run: take it out of OPEN in harness/unittests.py")
        return self.errors


def main():
    command = ["cargo", "test", "--release", "--no-fail-fast", "--message-format=json",
               *(f"-p{c}" for c in CRATES), *sys.argv[1:]]
    print(" ".join(command), flush=True)
    # Slow rust-analyzer tests need the root workspace's target directory.
    env = {"CARGO_TARGET_DIR": os.path.join(ROOT, "target"), **os.environ}
    proc = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, errors="replace")
    transcript = Transcript()
    for line in proc.stdout:
        sys.stdout.write(line)
        transcript.feed(line)
    errors = transcript.finish(proc.wait(), OPEN)
    print(f"\n{len(transcript.seen)} tests, {len(transcript.failed)} failed, "
          f"{len(OPEN & transcript.failed)} of them known (OPEN in harness/unittests.py)")
    for error in errors:
        print(f"::error::{error}")
    sys.exit(bool(errors))


if __name__ == "__main__":
    main()
