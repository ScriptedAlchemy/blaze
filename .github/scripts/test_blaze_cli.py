#!/usr/bin/env python3
"""Controls for source smoke evidence; fixtures below do not compile Rust."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("source_cli_smoke", ROOT / "harness/blaze_cli.py")
smoke = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = smoke
SPEC.loader.exec_module(smoke)

DOUBLE = r'''#!PYTHON
import json
from pathlib import Path
import sys
binary = Path(__file__).parent
config = json.loads((binary / "control.json").read_text())
command = sys.argv[1]
source = Path("src/main.rs").read_text()
with (binary / "commands.jsonl").open("a") as out:
    out.write(json.dumps(sys.argv[1:]) + "\n")
if command == "check":
    invalid = "missing_source_body" in source
    if config.get("setup_failure"):
        print("error: could not open toolchain shelf", file=sys.stderr)
        sys.exit(1)
    if invalid and not config.get("accept_invalid"):
        print(json.dumps({"severity":"error","code":"types","path":"src/main.rs","line":2,
                          "col":5,"end_line":2,"end_col":24,
                          "message":"cannot find function `missing_source_body` in this scope (segment 0) (E0425 UnresolvedPath)",
                          "fix":None}))
    print(json.dumps({"errors":int(invalid),"warnings":0,"fixes":0,"command":"blaze check",
                      "not_checked":["body"] if config.get("incomplete") else [],
                      "failures":1 if config.get("task_failure") else 0}))
    sys.exit(0 if config.get("accept_invalid") or not invalid else config.get("rejection_exit",1))
value = 7 if "    7\n" in source else 1
if config.get("stale_edit"):
    value = 1
if command == "run":
    print(f"hello from source: {value}")
    if config.get("mutate"):
        with (binary / config["mutate"]).open("ab") as out:
            out.write(b"\n# changed source executable\n")
    sys.exit(config.get("run_exit",0))
if command == "export":
    if not config.get("missing_export"):
        out = Path(sys.argv[-1])
        value = 9 if config.get("wrong_export") else value
        out.write_text("#!/bin/sh\nprintf 'hello from source: " + str(value) + "\\n'\n")
        out.chmod(0o700)
    sys.exit(0)
sys.exit(99)
'''


class SourceSmokeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
            path = self.bin / name
            path.write_text(DOUBLE.replace("#!PYTHON", "#!" + sys.executable))
            path.chmod(0o700)
        (self.bin / "control.json").write_text("{}")
        shelf = self.root / "shelf"
        shelf.mkdir()
        (shelf / "content-addressed-fixture").write_text("shelf input fixture, never loaded by a compiler")
        self.given = smoke.Input(str(self.bin / "blaze"), str(self.root / "run"), str(shelf), 5)

    def exercise(self, **control):
        (self.bin / "control.json").write_text(json.dumps(control))
        with contextlib.redirect_stdout(io.StringIO()):
            smoke.main(self.given)
        return json.loads((self.root / "run/results.json").read_text())

    def rejected_check(self, finding=None, summary=None, **status):
        """The source CLI's observed JSON shape, independent of the matcher implementation."""
        detail = {"id":"q88tn", "severity":"error", "code":"types", "path":"src/main.rs",
                  "line":2, "col":5, "end_line":2, "end_col":24,
                  "message":"cannot find function `missing_source_body` in this scope (segment 0) (E0425 UnresolvedPath)",
                  "fix":None}
        verdict = {"errors":1, "warnings":0, "fixes":0, "command":"blaze check", "not_checked":[]}
        detail.update(finding or {})
        verdict.update(summary or {})
        return {"returncode":1, "timed_out":False, "stderr":"",
                "stdout":json.dumps(detail) + "\n" + json.dumps(verdict) + "\n", **status}

    def test_canonical_types_diagnostic_proves_the_planted_rejection(self):
        record = self.rejected_check()
        self.assertIsNone(smoke.require(record, "invalid", None, rejects=True))
        findings, summary = smoke.check_reply(record)
        self.assertEqual(findings[0]["code"], "types")
        self.assertEqual(summary["errors"], 1)
        self.assertEqual(summary["not_checked"], [])

    def test_arbitrary_types_errors_cannot_replace_the_planted_finding(self):
        prefix = "cannot find function `missing_source_body` in this scope (segment 0) "
        controls = ({"code":"E0425"}, {"severity":"warning"}, {"path":"src/other.rs"},
                    {"line":3}, {"col":6}, {"end_line":3}, {"end_col":23},
                    {"message":None}, {"message":prefix.rstrip()},
                    {"message":prefix + "(E0308 TypeMismatch)"},
                    {"message":prefix + "(E0425 TypeMismatch)"},
                    {"message":prefix + "(E0433 UnresolvedPath)"},
                    {"message":"mismatched type of `missing_source_body` (E0308 TypeMismatch)"},
                    {"message":"unrelated type error mentioning `missing_source_body` (E0425 UnresolvedPath)"},
                    {"message":"cannot find function `another_body` in this scope (segment 0) (E0425 UnresolvedPath)"})
        for finding in controls:
            with self.subTest(finding=finding), self.assertRaises(smoke.Failed):
                smoke.require(self.rejected_check(finding=finding), "invalid", None, rejects=True)

    def test_the_rejection_requires_a_complete_failed_check_verdict(self):
        for summary in ({"errors":0}, {"errors":2}, {"errors":True}, {"failures":1},
                        {"not_checked":["body"]}, {"not_checked":None}, {"command":"blaze run"}):
            with self.subTest(summary=summary), self.assertRaises(smoke.Failed):
                smoke.require(self.rejected_check(summary=summary), "invalid", None, rejects=True)
        for status in ({"returncode":0}, {"returncode":3}, {"timed_out":True}):
            with self.subTest(status=status), self.assertRaises(smoke.Failed):
                smoke.require(self.rejected_check(**status), "invalid", None, rejects=True)

    def test_every_source_phase_runs_and_exported_program_is_executed(self):
        result = self.exercise()
        self.assertIs(result["complete"], True)
        self.assertEqual([row["name"] for row in result["phases"]], list(smoke.PHASES))
        self.assertEqual(set(result["binaries"]), {"blaze", "blaze-launcher", "blaze-fact-shim"})
        self.assertEqual(len(result["artifact_sha256"]), 64)
        commands = [json.loads(line) for line in (self.bin / "commands.jsonl").read_text().splitlines()]
        self.assertEqual([row[0] for row in commands], ["run", "run", "run", "check", "run", "check", "export"])
        self.assertEqual([row for row in commands if row[0] == "check"], [["check", "--view", "json"]] * 2)
        self.assertEqual((self.root / "run/standalone.json").exists(), True)
        self.assertNotEqual(result["phases"][0]["source_sha256"], result["phases"][2]["source_sha256"])

    def test_prior_evidence_is_never_erased(self):
        self.exercise()
        original = (self.root / "run/results.json").read_bytes()
        with self.assertRaisesRegex(smoke.Failed, "new or empty"):
            self.exercise()
        self.assertEqual((self.root / "run/results.json").read_bytes(), original)

    def test_wrong_output_and_successful_setup_failures_cannot_count(self):
        controls = ({"stale_edit":True}, {"run_exit":23}, {"setup_failure":True},
                    {"task_failure":True}, {"accept_invalid":True}, {"rejection_exit":3},
                    {"incomplete":True}, {"missing_export":True}, {"wrong_export":True})
        for control in controls:
            with self.subTest(control=control):
                # Every plant has independent sources/evidence; no baseline from a previous run.
                self.given = smoke.Input(self.given.blaze, str(self.root / ("run-" + next(iter(control)))), self.given.shelf, 5)
                with self.assertRaises(smoke.Failed):
                    self.exercise(**control)
                result = json.loads((Path(self.given.scratch) / "results.json").read_text())
                self.assertIs(result["complete"], False)
                self.assertTrue(result["error"])

    def test_a_changed_helper_cannot_earn_acceptance(self):
        for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
            with self.subTest(binary=name):
                self.given = smoke.Input(self.given.blaze, str(self.root / ("mutate-" + name)), self.given.shelf, 5)
                with self.assertRaisesRegex(smoke.Failed, "changed"):
                    self.exercise(mutate=name)
                result = json.loads((Path(self.given.scratch) / "results.json").read_text())
                self.assertIs(result["complete"], False)

    def test_missing_helper_and_symlink_are_refused_before_running(self):
        helper = self.bin / "blaze-launcher"
        helper.unlink()
        with self.assertRaisesRegex(smoke.Failed, "regular executable"):
            self.exercise()
        helper.symlink_to(self.bin / "blaze")
        with self.assertRaisesRegex(smoke.Failed, "regular executable"):
            self.exercise()
        self.assertFalse((self.bin / "commands.jsonl").exists())

    def test_input_is_typed_and_has_no_phase_filter(self):
        good = {"blaze":"/bin/blaze", "scratch":"/new", "shelf":"/shelf"}
        self.assertEqual(smoke.load(json.dumps(good)).timeout, 1800)
        for changed in ({"timeout":True}, {"timeout":0}, {"only":["warm"]}, {"blaze":""}):
            with self.subTest(changed=changed), self.assertRaises((smoke.Failed, TypeError)):
                smoke.load(json.dumps({**good, **changed}))


if __name__ == "__main__":
    unittest.main()
