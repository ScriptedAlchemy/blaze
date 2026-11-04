#!/usr/bin/env python3
"""unittests.py: the crates' own tests; every failure fails the gate.

usage: unittests.py [cargo test argument ...]

Runs `cargo test --release --no-fail-fast` of the crates below. Every assertion failure,
failed build, crashed process or incomplete transcript fails the gate.
"""
import json, os, re, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATES = ["hir-ty", "hir-ty-borrowck", "hir-ty-wfcheck", "hir-def", "parser", "syntax", "hir-ide", "hir-ty-codegen", "ra-jit-store", "blaze-registry", "blaze-resolve", "blaze-build-script", "blaze-manifest", "ra-jit"]

COLOR = re.compile(r"\x1b\[[0-9;]*m")
RUNNING = re.compile(r"^\s+Running .*[/\\]deps[/\\](\w+?)-[0-9a-f]{16}")
DOCTESTS = re.compile(r"^\s+Doc-tests (\w+)")
RESULT = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored)(?:\s|,|$)")


SUMMARY = re.compile(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;")


class Transcript:
    """Require successful builds and complete, passing test processes."""
    def __init__(self):
        self.seen, self.failed = set(), set()
        self.build_finished = False
        self.errors = []
        self.suite = None
        self.suites = 0
        self.doctest = None

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
            self.doctest = m.group(1) if DOCTESTS.match(line) else None
            self.suite = {"name": m.group(1), "outcomes": {}}
            self.suites += 1
        elif m := RESULT.match(line):
            # Rustdoc can run multiple libtest groups under one Doc-tests banner
            # (for example merged 2024 doctests and separately compiled examples).
            if self.suite is None and self.doctest is not None:
                self.suite = {"name": self.doctest, "outcomes": {}}
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
            if self.suite is None and self.doctest is not None:
                self.suite = {"name": self.doctest, "outcomes": {}}
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

    def finish(self, status):
        self.end_suite()
        if not self.build_finished:
            self.errors.append("missing successful Cargo build-finished record")
        if not self.suites or not self.seen:
            self.errors.append("no test results were executed")
        expected = 101 if self.failed else 0
        if status != expected:
            self.errors.append(f"cargo exited {status}, expected {expected} for the recorded tests")
        for name in sorted(self.failed):
            self.errors.append(f"{name} fails")
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
    errors = transcript.finish(proc.wait())
    print(f"\n{len(transcript.seen)} tests, {len(transcript.failed)} failed")
    for error in errors:
        print(f"::error::{error}")
    sys.exit(bool(errors))


if __name__ == "__main__":
    main()
