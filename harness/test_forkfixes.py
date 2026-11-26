"""Harness-local proof that the ported fork-fix driver preserves its typed input and oracles."""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent / "forkfixes"))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools/typed-input/src"))

import forkfixes  # noqa: E402
from cases import CASES  # noqa: E402
from typed_input import parse  # noqa: E402


EXPECTED_CASES = [
    "raw-duplicate-import",
    "unresolved-paths",
    "unresolved-bare-value",
    "attribute-impl-in-function",
    "object-default-in-body",
    "borrowed-object-keeps-its-lifetime",
    "borrowed-objects-stay-independent",
    "borrowed-object-is-not-static",
    "elided-object-through-a-call",
    "column-counts-scalars",
    "caller-location-counts-display-width",
    "track-caller-through-function-pointers",
    "empty-array-promoted-through-iter-mut",
    "implied-bounds-through-fields",
    "self-of-an-impl-for-dyn",
    "cdylib-allocates",
    "a-required-feature-of-a-dependency",
    "a-test-sees-the-library-beside-its-bin",
    "a-script-searches-where-its-build-dependencys-script-said",
    "caller-environment-changes-and-becomes-unset",
    "two-derived-clones-with-no-debug-info",
    "an-async-fn-and-polling-it-again",
    "packages-own-same-named-native-archives",
]


class ForkfixHarness(unittest.TestCase):
    def fixture_input(self, root: Path) -> dict:
        blaze = root / "blaze"
        blaze.write_text("#!/bin/sh\nexit 0\n")
        blaze.chmod(0o755)
        shelf = root / "shelf"
        shelf.mkdir(exist_ok=True)
        (shelf / ("0" * 64)).write_bytes(b"manifest")
        return {"blaze": str(blaze), "scratch": str(root / "scratch"), "shelf": str(shelf),
                "timeout": 90, "stall_bound": 300, "workers": 2}

    def test_input_accepts_the_ci_shape_and_reports_legacy_stall_bound(self):
        with tempfile.TemporaryDirectory() as directory:
            given = parse(forkfixes.Input, json.dumps(self.fixture_input(Path(directory))), "test")
            checked = forkfixes.validate(given, directory)
            self.assertEqual(checked.timeout, 90)
            self.assertEqual(checked.stall_bound, 300)
            self.assertEqual(checked.workers, 2)
            self.assertEqual(checked.only, ())

    def test_input_refuses_unknown_cases_and_nonempty_scratch(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw = self.fixture_input(root)
            raw["only"] = ["not-a-fixture"]
            with self.assertRaisesRegex(ValueError, "unknown case"):
                forkfixes.validate(parse(forkfixes.Input, json.dumps(raw), "test"), directory)
            raw = self.fixture_input(root)
            scratch = Path(raw["scratch"])
            scratch.mkdir()
            (scratch / "kept-evidence").write_text("do not erase")
            with self.assertRaisesRegex(ValueError, "must be new or empty"):
                forkfixes.validate(parse(forkfixes.Input, json.dumps(raw), "test"), directory)

    def test_all_original_cases_and_their_step_oracles_are_preserved(self):
        self.assertEqual([case["name"] for case in CASES], EXPECTED_CASES)
        self.assertEqual(len(CASES), 23)
        self.assertTrue(all(isinstance(case["fixes"], list) for case in CASES))
        for case in CASES:
            with self.subTest(case=case["name"]):
                steps = forkfixes.steps_for(case)
                self.assertTrue(steps)
                if "steps" not in case:
                    self.assertEqual(len(steps), 1)
                    expected = {key: case[key] for key in ("prints", "diagnostic", "error", "test") if key in case}
                    self.assertEqual(steps[0], expected)
        cdylib = next(case for case in CASES if case["name"] == "cdylib-allocates")
        self.assertEqual([bool(step.get("replay")) for step in cdylib["steps"]], [False, True])
        self.assertTrue(forkfixes.case_exports_artifacts(cdylib))
        feature = next(case for case in CASES if case["name"] == "a-required-feature-of-a-dependency")
        self.assertFalse(forkfixes.case_exports_artifacts(feature))
        self.assertEqual(sum("needs" in step.get("absent", ()) for step in feature["steps"]), 3)
        environment = next(case for case in CASES if case["name"] == "caller-environment-changes-and-becomes-unset")
        self.assertEqual([step.get("env", {}).get("FORKFIXES_CALLER_VALUE") for step in environment["steps"]],
                         [None, "first", "second", None, "first"])

    def test_target_selection_uses_package_and_binary_from_the_retained_manifest(self):
        case = next(case for case in CASES if case["name"] == "a-required-feature-of-a-dependency")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            forkfixes.write_files(root, forkfixes.initial_files(case))
            self.assertEqual(forkfixes.choose_target(root, case, {}), ("app", "app"))
            self.assertEqual(forkfixes.choose_target(root, case, {"program": "host-needs"}), ("app", "host-needs"))
            self.assertEqual(forkfixes.choose_target(root, case, {"args": ["--bin", "needs"]}), ("app", "needs"))
            args = forkfixes.target_args(root, case, {"args": ["--bin", "needs", "--features", "dep/extra"]})
            self.assertEqual(args, ["-p", "app", "--bin", "needs", "--features", "dep/extra"])

    def test_daemon_status_json_null_and_pid_are_distinct(self):
        self.assertIsNone(forkfixes.daemon_status_pid('{"daemon":null}'))
        self.assertEqual(forkfixes.daemon_status_pid('{"daemon":{"pid":42}}'), 42)
        self.assertIsNone(forkfixes.daemon_status_pid('not json'))

    def test_check_json_requires_one_complete_summary_and_preserves_e_codes(self):
        record = {"stdout": '{"severity":"error","code":"types","message":"cannot find value `x` in this scope (E0425 UnresolvedPath)"}\n'
                            '{"errors":1,"warnings":0,"fixes":0,"command":"blaze check","not_checked":[]}\n'}
        rows, summary = forkfixes.check_rows(record)
        self.assertEqual(summary["errors"], 1)
        self.assertTrue(forkfixes.diagnostic_matches(rows, "E0425"))
        self.assertFalse(forkfixes.diagnostic_matches(rows, "E0252"))
        incomplete = {"stdout": '{"errors":1,"not_checked":["types"]}\n'}
        rows, summary = forkfixes.check_rows(incomplete)
        self.assertEqual(summary["not_checked"], ["types"])
        malformed = {"stdout": "not json\n"}
        with self.assertRaisesRegex(ValueError, "only JSON"):
            forkfixes.check_rows(malformed)

    def test_cdylib_export_uses_explicit_lib_and_replay_diffs_are_exact(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            given = parse(forkfixes.Input, json.dumps(self.fixture_input(root)), "test")
            args = forkfixes.export_args(given, "exported", None, root / "libexported.dylib", [], library=True)
            self.assertEqual(args[:5], [given.blaze, "export", "-p", "exported", "--lib"])
            self.assertNotIn("--bin", args)
            program = forkfixes.export_args(given, "app", "app", root / "program", [])
            self.assertEqual(program[:6], [given.blaze, "export", "-p", "app", "--bin", "app"])
            self.assertNotIn("--lib", program)
        diff = forkfixes.artifact_identity_diff({"program": "a", "libx.dylib": "b"},
                                                {"program": "c", "other": "d"})
        self.assertIn("missing ['libx.dylib']", diff)
        self.assertIn("extra ['other']", diff)
        self.assertIn("changed bytes ['program']", diff)
        self.assertEqual(forkfixes.artifact_identity_diff({"program": "a"}, {"program": "a"}), "")

    def test_replay_store_clone_verifies_f5s3_bytes_as_independent_copies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            initial = root / "initial"
            replay = root / "replay"
            cas = initial / ".blaze/v2/store/f5s3"
            cas.mkdir(parents=True)
            (cas / "seg-one").write_bytes(b"cas bytes")
            (initial / ".blaze/v2/store/CACHEDIR.TAG").write_bytes(b"tag")
            evidence = forkfixes.clone_store(initial, replay)
            copied = replay / ".blaze/v2/store/f5s3/seg-one"
            self.assertEqual(copied.read_bytes(), b"cas bytes")
            self.assertFalse(os.path.samefile(cas / "seg-one", copied))
            self.assertEqual(evidence["f5s3_files"], 1)
            self.assertEqual(evidence["store_files"], 2)
            self.assertEqual(len(evidence["f5s3_sha256"]), 64)
            broken = root / "broken"
            (broken / ".blaze/v2/store").mkdir(parents=True)
            with self.assertRaisesRegex(ValueError, "f5s3"):
                forkfixes.clone_store(broken, root / "other-replay")

    def test_required_feature_absence_is_a_machine_note(self):
        rows = [{"note": "bin `needs` of package `app` is not built: its required feature is off; 2 file(s) not checked"}]
        self.assertTrue(forkfixes.skipped_target_note(rows, "needs", "app"))
        self.assertFalse(forkfixes.skipped_target_note(rows, "host-needs", "app"))
        self.assertFalse(forkfixes.skipped_target_note(rows, "needs", "other"))
        self.assertFalse(forkfixes.skipped_target_note([{"note": "something else"}], "needs", "app"))

    def test_export_stats_and_step_environment_are_exact(self):
        record = {"stderr": "noise\nexport stats: functions=3 folded=1 stubs=0 data=2 tls=0 compiled=0 cached=3 unkeyed=0 bytes=9 wall=4ms\n"}
        self.assertEqual(forkfixes.parse_export_stats(record)["compiled"], 0)
        self.assertEqual(forkfixes.parse_export_stats(record)["cached"], 3)
        with self.assertRaisesRegex(ValueError, "no `export stats:`"):
            forkfixes.parse_export_stats({"stderr": ""})
        env = forkfixes.step_environment({"KEEP": "yes", "FORKFIXES_CALLER_VALUE": "old"},
                                         {"env": {"FORKFIXES_CALLER_VALUE": None}})
        self.assertNotIn("FORKFIXES_CALLER_VALUE", env)
        self.assertEqual(env["KEEP"], "yes")
        env = forkfixes.step_environment({}, {"env": {"FORKFIXES_CALLER_VALUE": "first"}})
        self.assertEqual(env["FORKFIXES_CALLER_VALUE"], "first")

    def test_commands_write_exact_bounded_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            record = forkfixes.run_command(["/bin/sh", "-c", "printf out; printf err >&2"], root,
                                           {"PATH": "/usr/bin:/bin"}, 10, root / "evidence.json", "probe")
            self.assertEqual((record["returncode"], record["stdout"], record["stderr"], record["timed_out"]),
                             (0, "out", "err", False))
            loaded = json.loads((root / "evidence.json").read_text())
            self.assertEqual(loaded["argv"], ["/bin/sh", "-c", "printf out; printf err >&2"])
            timed = forkfixes.run_command(["/bin/sh", "-c", "sleep 5"], root, {"PATH": "/usr/bin:/bin"},
                                          1, root / "timeout.json", "timeout-probe")
            self.assertTrue(timed["timed_out"])


if __name__ == "__main__":
    unittest.main()
