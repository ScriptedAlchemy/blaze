#!/usr/bin/env python3
"""Execute benchmark caller shells with command doubles and the real typed readers."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
for directory in (".github/scripts", "harness", "tools/typed-input/src"):
    sys.path.insert(0, str(ROOT / directory))
import bench  # noqa: E402
import benchhistory  # noqa: E402
import benchreport  # noqa: E402
from test_distributed_ci import shell_step  # noqa: E402

SPY = r'''
import json, os, pathlib, sys
args = sys.argv[1:]
if args and args[0] == "-I":
    args.pop(0)
script = pathlib.Path(args.pop(0)).name
action = args.pop(0) if script == "promote.py" else None
if len(args) != 1:
    raise SystemExit("caller did not pass one JSON input: " + repr(args))
source = args[0]
raw = sys.stdin.read() if source == "-" else pathlib.Path(source).read_text()
value = json.loads(raw)
with open(os.environ["CALLS"], "a") as stream:
    stream.write(json.dumps({"script": script, "action": action, "input": value}) + "\n")
if script == "benchhistory.py" and value.get("action") == "check":
    pathlib.Path(value["report"]).write_text("fixture history report\n")
if script == "bench.py":
    raise SystemExit(int(os.environ.get("BENCH_STATUS", "0")))
'''


class CallerTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        commands = self.root / "commands"
        commands.mkdir()
        for name, source in {
            "python3": f"#!{sys.executable}\n" + SPY,
            "nix": '#!/bin/sh\nwhile [ "$#" -gt 0 ] && [ "$1" != -c ]; do shift; done\nshift\nexec "$@"\n',
            "gh": "#!/bin/sh\nexit 1\n",
        }.items():
            path = commands / name
            path.write_text(source)
            path.chmod(0o700)
        self.calls, self.output = self.root / "calls.jsonl", self.root / "github-output"
        self.output.touch()
        engine = self.root / "cargo/bin/blaze-engine"
        engine.parent.mkdir(parents=True)
        engine.write_text("compiler identity fixture; never executed\n")
        engine.chmod(0o700)
        (self.root / "out").mkdir()
        self.env = {**os.environ, "PATH": str(commands) + os.pathsep + os.environ["PATH"],
                    "CALLS": str(self.calls), "RUNNER_TEMP": str(self.root), "RUNNER_OS": "Linux",
                    "BENCH_HOME": str(self.root / "bench-home"), "BENCH_MACHINE": "GitHub-hosted fixture", "FULL": "false", "FORCE": "false",
                    "GITHUB_OUTPUT": str(self.output), "GITHUB_STEP_SUMMARY": str(self.root / "summary"),
                    "GITHUB_SHA": "a" * 40, "GITHUB_SERVER_URL": "https://github.com",
                    "GITHUB_REPOSITORY": "fixture/blaze", "GITHUB_RUN_ID": "17", "GITHUB_RUN_ATTEMPT": "2",
                    "GITHUB_WORKSPACE": str(ROOT), "PYTHON": "python3", "SHELL_OF_APP": "",
                    "CARGO_HOME": str(self.root / "cargo"), "APP": "fd", "OS": "ubuntu-24.04-arm",
                    "OUT": str(self.root / "out"), "WORK": str(self.root / "work"), "TAG": "candidate", "STABLE": "stable"}

    def run_step(self, workflow, step, status=0):
        if self.calls.exists():
            self.calls.unlink()
        self.output.write_text("")
        body = shell_step(ROOT / ".github/workflows" / workflow, step)
        body = body.replace("${{ needs.release.outputs.sha }}", "a" * 40)
        done = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", body], cwd=self.root,
                              env=dict(self.env, BENCH_STATUS=str(status)), capture_output=True, text=True, timeout=10)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []
        readers = {"bench.py": bench.load, "benchhistory.py": benchhistory.load, "benchreport.py": benchreport.load}
        for call in calls:
            if call["script"] in readers:
                readers[call["script"]](json.dumps(call["input"]))
        return done, calls

    def test_scheduled_bench_callers_preserve_failures_and_pass_one_typed_input(self):
        for workflow, projects in (("bench-readme.yaml", ["ripgrep", "fd", "bat", "hyperfine", "just", "axum", "nushell", "helix", "bevy", "biome"]),
                                   ("bench-bun.yaml", ["bun"]), ("bench-counts.yaml", ["fd", "hyperfine"])):
            for status in (0, 3, 29):
                with self.subTest(workflow=workflow, status=status):
                    done, calls = self.run_step(workflow, "Bench", status)
                    self.assertEqual(done.returncode, 29 if status == 29 else 0, done.stdout + done.stderr)
                    self.assertEqual(len(calls), 1)
                    value = calls[0]["input"]
                    self.assertEqual(value["projects"], projects)
                    self.assertEqual(value["blaze"], self.env["BENCH_HOME"] + "/compiler/release/blaze-engine")
                    self.assertEqual(value["commit"], self.env["GITHUB_SHA"])
                    self.assertEqual("wrong=true" in self.output.read_text(), status == 3)
                    self.assertNotIn("reference", value)
                    self.assertNotIn("setups", value)

    def test_train_keeps_stage1_and_never_compares_a_failed_measurement(self):
        for status in (0, 3, 29):
            with self.subTest(status=status):
                done, calls = self.run_step("blaze-platform.yaml", "Bench", status)
                self.assertEqual(done.returncode, status, done.stdout + done.stderr)
                self.assertEqual(len(calls), 2 if status == 0 else 1)
                self.assertEqual(calls[0]["input"]["blaze"], str(self.root / "bench-binaries/blaze"))
                self.assertEqual(calls[0]["input"]["backends"], ["cranelift", "llvm"])
                if status == 0:
                    self.assertEqual(calls[1]["input"]["action"], "compare")

    def test_speed_receives_failed_measurements_but_rejects_other_runner_failures(self):
        for status in (0, 3, 29):
            with self.subTest(status=status):
                done, calls = self.run_step("promote.yaml", "${{ matrix.app }} measured Blaze speed", status)
                self.assertEqual(done.returncode, 29 if status == 29 else 0, done.stdout + done.stderr)
                self.assertEqual(len(calls), 1 if status == 29 else 2)
                self.assertEqual(calls[0]["input"]["blaze"], str(self.root / "cargo/bin/blaze-engine"))
                self.assertIs(calls[0]["input"]["tests"], True)
                if status != 29:
                    self.assertEqual(calls[1]["action"], "speed")
                    self.assertEqual(calls[1]["input"]["baseline"], "")

    def test_publication_and_history_callers_use_the_strict_readers(self):
        for prefix, filename in (("bench", "bench.json"), ("bench-bun", "bench-bun.json"), ("counts", "bench.json")):
            result = self.root / f"results/{prefix}-fixture/{filename}"
            result.parent.mkdir(parents=True, exist_ok=True)
            result.write_text("{}\n")
        for workflow, step, scripts in (
            ("bench-readme.yaml", "README and docs/benchmarks.md", ["benchreport.py"]),
            ("bench-bun.yaml", "README", ["benchreport.py"]),
            ("bench-readme.yaml", "History and regressions", ["benchhistory.py", "benchhistory.py"]),
            ("bench-counts.yaml", "History and check", ["benchhistory.py", "benchhistory.py"]),
        ):
            with self.subTest(workflow=workflow, step=step):
                done, calls = self.run_step(workflow, step)
                self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
                self.assertEqual([call["script"] for call in calls], scripts)
        latency = self.root / "latency/fixture/latency.json"
        latency.parent.mkdir(parents=True)
        latency.write_text("{}\n")
        done, calls = self.run_step("live-latency.yaml", "History and regressions")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(calls[0]["input"]["files"], [str(latency)])
        self.assertEqual(calls[1]["input"]["least"], 3)


if __name__ == "__main__":
    unittest.main()
