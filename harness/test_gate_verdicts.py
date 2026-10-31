"""Negative controls for the real unit, UI and distributed-test verdicts.

No Blaze compilation or services are needed. The small Cargo integration test also
runs when Cargo is installed, as it is in the pinned CI toolchain.
"""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import uigate
import unittests

spec = importlib.util.spec_from_file_location("dist_test_contract", HERE / "dist/tests.py")
dist = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dist)


def cargo_build(success=True):
    return json.dumps({"reason": "build-finished", "success": success}) + "\n"


def suite(outcome="FAILED", name="known", crate="fixture"):
    counts = {"ok": (1, 0, 0), "FAILED": (0, 1, 0), "ignored": (0, 0, 1)}[outcome]
    return (f"     Running unittests (target/release/deps/{crate}-0123456789abcdef)\n"
            f"test {name} ... {outcome}\n"
            f"test result: {'FAILED' if outcome == 'FAILED' else 'ok'}. "
            f"{counts[0]} passed; {counts[1]} failed; {counts[2]} ignored; 0 measured; 0 filtered out\n")


def errors(text, status=101, known=frozenset({"fixture::known"})):
    transcript = unittests.Transcript()
    for line in text.splitlines(keepends=True):
        transcript.feed(line)
    return transcript.finish(status, set(known))


class UnitVerdictTests(unittest.TestCase):
    def test_completed_known_assertion_failure_is_allowed(self):
        self.assertFalse(errors(cargo_build() + suite() +
                                "error: test failed, to rerun pass `-p fixture --lib`\n"
                                "error: 1 target failed:\n    `-p fixture --lib`\n"))

    def test_real_pass_without_allowlisted_failures_is_allowed(self):
        self.assertFalse(errors(cargo_build() + suite("ok"), status=0, known=set()))

    def test_known_failures_do_not_hide_later_compiler_error(self):
        diagnostic = json.dumps({"reason": "compiler-message", "message": {
            "level": "error", "message": "cannot find value production_regression"}})
        result = errors(cargo_build() + suite() + diagnostic + "\n" + cargo_build(False))
        self.assertTrue(any("production_regression" in item for item in result))

    def test_known_failure_does_not_hide_unstructured_cargo_error(self):
        self.assertTrue(errors(cargo_build() + suite() + "error: could not compile `other`\n"))

    def test_signal_and_infrastructure_exit_are_not_assertion_failure(self):
        for status in (-11, -9, 137, 1, 0):
            with self.subTest(status=status):
                self.assertTrue(errors(cargo_build() + suite(), status))

    def test_child_signal_after_a_valid_summary_is_rejected(self):
        self.assertTrue(errors(cargo_build() + suite() +
                               "  process didn't exit successfully: test (signal: 6, SIGABRT)\n"))

    def test_failure_without_complete_summary_is_rejected(self):
        text = cargo_build() + suite().split("test result:")[0]
        self.assertTrue(errors(text))
        self.assertTrue(errors(text + suite("ok", "other", "second")))

    def test_wrong_or_duplicate_summary_data_is_rejected(self):
        self.assertTrue(errors(cargo_build() + suite().replace("0 passed; 1 failed", "2 passed; 1 failed")))
        self.assertTrue(errors(cargo_build() + suite().replace("test known ... FAILED\n",
                                                        "test known ... FAILED\ntest known ... FAILED\n")))

    def test_missing_or_failed_build_record_is_not_a_green_run(self):
        self.assertTrue(errors(suite()))
        self.assertTrue(errors(cargo_build(False) + suite()))

    def test_empty_run_is_rejected(self):
        self.assertTrue(errors(cargo_build(), status=0, known=set()))

    def test_fixed_missing_and_ignored_known_failures_still_fail(self):
        for text, status in ((suite("ok"), 0), (suite("ignored"), 0),
                             (suite("ok", "other"), 0)):
            with self.subTest(text=text):
                self.assertTrue(errors(cargo_build() + text, status))

    def test_passing_doctests_and_empty_target_are_allowed_beside_known_failures(self):
        text = (cargo_build() + suite() + "   Doc-tests other\n"
                "test src/lib.rs - demo (line 1) ... ok\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
                "     Running unittests (target/release/deps/empty-0123456789abcdef)\n"
                "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n")
        self.assertFalse(errors(text))

    def test_cli_rejects_a_compile_failure_after_all_known_failures(self):
        # Test the script's exit status, not just the parser in isolation.
        text = cargo_build()
        for name in sorted(unittests.OPEN):
            crate, test = name.split("::", 1)
            text += suite(name=test, crate=crate)
        text += "error: could not compile `ra-jit` (bin test) due to 1 previous error\n"
        with tempfile.TemporaryDirectory() as directory:
            cargo = Path(directory) / "cargo"
            cargo.write_text(f"#!{sys.executable}\nimport sys\nprint({text!r})\nsys.exit(101)\n")
            cargo.chmod(0o755)
            result = subprocess.run([sys.executable, "-I", str(HERE / "unittests.py")],
                                    env={**os.environ, "PATH": directory + os.pathsep + os.environ.get("PATH", "")},
                                    capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    @unittest.skipUnless(shutil.which("cargo"), "Cargo is installed by the CI toolchain")
    def test_actual_cargo_json_and_libtest_transcript(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="0.0.0"\nedition="2021"\n')
            (root / "src/lib.rs").write_text('#[test] fn known() { assert!(false); }\n')
            run = subprocess.run(["cargo", "test", "--offline", "--no-fail-fast", "--message-format=json"],
                                 cwd=root, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                 text=True, timeout=90,
                                 env={**os.environ, "CARGO_TARGET_DIR": str(root / "target"),
                                      "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "",
                                      "RUST_BACKTRACE": "0"})
            self.assertEqual(run.returncode, 101, run.stdout)
            self.assertFalse(errors(run.stdout, run.returncode), run.stdout)


class UIVerdictTests(unittest.TestCase):
    def test_match_and_known_program_mismatch_are_allowed(self):
        self.assertFalse(uigate.verdict([{"test": "new.rs", "outcome": "match"},
                                        {"test": "known.rs", "outcome": "mismatch"}], {"known.rs"}))

    def test_known_path_never_excuses_compiler_failure_or_instability(self):
        for outcome in ("crash", "hang", "link", "unsteady"):
            with self.subTest(outcome=outcome):
                self.assertTrue(uigate.verdict([{"test": "known.rs", "outcome": outcome}], {"known.rs"}))

    def test_empty_and_all_excluded_runs_are_rejected(self):
        for rows in ([], [{"test": "known.rs", "outcome": "excluded"}]):
            self.assertTrue(uigate.verdict(rows, {"known.rs"}))

    def test_new_mismatch_is_rejected(self):
        self.assertTrue(uigate.verdict([{"test": "new.rs", "outcome": "mismatch"}], {"known.rs"}))

    def test_duplicate_revisions_are_rejected_but_distinct_revisions_are_allowed(self):
        row = {"test": "test.rs", "outcome": "match", "revision": "a"}
        self.assertTrue(uigate.verdict([row, row], set()))
        self.assertFalse(uigate.verdict([row, {**row, "revision": "b"}], set()))

    def test_invalid_shape_or_unknown_outcome_is_rejected(self):
        for rows in ({}, [None], [{}], [{"test": "x", "outcome": "unknown"}],
                     [{"test": "x", "outcome": []}], [{"test": "x", "outcome": "match", "revision": []}]):
            with self.subTest(rows=rows):
                self.assertTrue(uigate.verdict(rows, set()))

    def test_cli_keeps_backend_specific_allowlist_scope(self):
        with tempfile.TemporaryDirectory() as directory:
            results = Path(directory) / "results.json"
            results.write_text(json.dumps([{"test": "panics/panic-abort-backtrace-without-debuginfo.rs",
                                            "outcome": "mismatch"}]))
            for backend, expected in (("llvm", 0), ("cranelift", 1)):
                result = subprocess.run([sys.executable, "-I", str(HERE / "uigate.py"), str(results)],
                                        env={**os.environ, "RA_JIT_BACKEND": backend}, capture_output=True,
                                        text=True, timeout=15)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)


class DistributedVerdictTests(unittest.TestCase):
    def setUp(self):
        self.harness = dist.Harness.__new__(dist.Harness)

    def test_expected_assertion_failure_requires_exit_101(self):
        for code in (101, -11, -9, -6, 137, 124, 1, 0):
            with self.subTest(code=code):
                run = dist.Run(code, "[ra-jit]\ntest a ... FAILED\n")
                self.assertEqual(self.harness.verdict(run, {"a"}, False) == "ok", code == 101)

    def test_successful_cached_and_retried_runs_still_pass(self):
        self.assertEqual(self.harness.verdict(dist.Run(0, "[ra-jit]\n"), set(), True), "ok")
        run = dist.Run(0, "[ra-jit]\ntest a ... FAILED\ntest a ... ok\n")
        self.assertEqual(self.harness.verdict(run, {"a"}, True), "ok")

    def test_refused_invocation_is_separate_from_test_failure(self):
        run = dist.Run(101, "[ra-jit]\nerror: Option 'quiet' given more than once\n")
        self.assertEqual(self.harness.verdict(run, set(), dist.REJECTED), "ok")
        self.assertNotEqual(self.harness.verdict(run, set(), False), "ok")


if __name__ == "__main__":
    unittest.main()
