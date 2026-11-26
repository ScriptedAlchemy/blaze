"""Lightweight harness checks: fake processes only; never starts Blaze or a compiler.

    python3 -m unittest discover -s harness/forkfixes -p 'test_*.py'
"""
from __future__ import annotations

import importlib.util
import json
import os
import signal
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
spec = importlib.util.spec_from_file_location("forkfixes_runner", HARNESS / "forkfixes.py")
runner = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = runner
spec.loader.exec_module(runner)


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
            given = runner.parse(runner.Input, json.dumps(self.fixture_input(Path(directory))), "test")
            checked = runner.validate(given, directory)
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
                runner.validate(runner.parse(runner.Input, json.dumps(raw), "test"), directory)
            raw = self.fixture_input(root)
            scratch = Path(raw["scratch"])
            scratch.mkdir()
            (scratch / "kept-evidence").write_text("do not erase")
            with self.assertRaisesRegex(ValueError, "must be new or empty"):
                runner.validate(runner.parse(runner.Input, json.dumps(raw), "test"), directory)

    def test_all_original_cases_and_their_step_oracles_are_preserved(self):
        self.assertEqual([case["name"] for case in runner.CASES], EXPECTED_CASES)
        self.assertEqual(len(runner.CASES), 23)
        self.assertTrue(all(isinstance(case["fixes"], list) for case in runner.CASES))
        for case in runner.CASES:
            with self.subTest(case=case["name"]):
                steps = runner.steps_for(case)
                self.assertTrue(steps)
                if "steps" not in case:
                    self.assertEqual(len(steps), 1)
                    expected = {key: case[key] for key in ("prints", "diagnostic", "error", "test") if key in case}
                    self.assertEqual(steps[0], expected)
        cdylib = next(case for case in runner.CASES if case["name"] == "cdylib-allocates")
        self.assertEqual([bool(step.get("replay")) for step in cdylib["steps"]], [False, True])
        self.assertTrue(runner.case_exports_artifacts(cdylib))
        feature = next(case for case in runner.CASES if case["name"] == "a-required-feature-of-a-dependency")
        self.assertFalse(runner.case_exports_artifacts(feature))
        self.assertEqual(sum("needs" in step.get("absent", ()) for step in feature["steps"]), 3)
        environment = next(case for case in runner.CASES if case["name"] == "caller-environment-changes-and-becomes-unset")
        self.assertEqual([step.get("env", {}).get("FORKFIXES_CALLER_VALUE") for step in environment["steps"]],
                         [None, "first", "second", None, "first"])

    def test_target_selection_uses_package_and_binary_from_the_retained_manifest(self):
        case = next(case for case in runner.CASES if case["name"] == "a-required-feature-of-a-dependency")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner.write_files(root, runner.initial_files(case))
            self.assertEqual(runner.choose_target(root, case, {}), ("app", "app"))
            self.assertEqual(runner.choose_target(root, case, {"program": "host-needs"}), ("app", "host-needs"))
            self.assertEqual(runner.choose_target(root, case, {"args": ["--bin", "needs"]}), ("app", "needs"))
            args = runner.target_args(root, case, {"args": ["--bin", "needs", "--features", "dep/extra"]})
            self.assertEqual(args, ["-p", "app", "--bin", "needs", "--features", "dep/extra"])

    def test_daemon_status_json_null_and_pid_are_distinct(self):
        self.assertIsNone(runner.daemon_status_pid('{"daemon":null}'))
        self.assertEqual(runner.daemon_status_pid('{"daemon":{"pid":42}}'), 42)
        self.assertIsNone(runner.daemon_status_pid('not json'))

    def test_check_json_requires_one_complete_summary_and_preserves_e_codes(self):
        record = {"stdout": '{"severity":"error","code":"types","message":"cannot find value `x` in this scope (E0425 UnresolvedPath)"}\n'
                            '{"errors":1,"warnings":0,"fixes":0,"command":"blaze check","not_checked":[]}\n'}
        rows, summary = runner.check_rows(record)
        self.assertEqual(summary["errors"], 1)
        self.assertTrue(runner.diagnostic_matches(rows, "E0425"))
        self.assertFalse(runner.diagnostic_matches(rows, "E0252"))
        incomplete = {"stdout": '{"errors":1,"not_checked":["types"]}\n'}
        rows, summary = runner.check_rows(incomplete)
        self.assertEqual(summary["not_checked"], ["types"])
        malformed = {"stdout": "not json\n"}
        with self.assertRaisesRegex(ValueError, "only JSON"):
            runner.check_rows(malformed)

    def test_cdylib_export_uses_explicit_lib_and_replay_diffs_are_exact(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            given = runner.parse(runner.Input, json.dumps(self.fixture_input(root)), "test")
            args = runner.export_args(given, "exported", None, root / "libexported.dylib", [], library=True)
            self.assertEqual(args[:5], [given.blaze, "export", "-p", "exported", "--lib"])
            self.assertNotIn("--bin", args)
            program = runner.export_args(given, "app", "app", root / "program", [])
            self.assertEqual(program[:6], [given.blaze, "export", "-p", "app", "--bin", "app"])
            self.assertNotIn("--lib", program)
        diff = runner.artifact_identity_diff({"program": "a", "libx.dylib": "b"},
                                                {"program": "c", "other": "d"})
        self.assertIn("missing ['libx.dylib']", diff)
        self.assertIn("extra ['other']", diff)
        self.assertIn("changed bytes ['program']", diff)
        self.assertEqual(runner.artifact_identity_diff({"program": "a"}, {"program": "a"}), "")

    def test_replay_store_clone_verifies_f5s3_bytes_as_independent_copies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            initial = root / "initial"
            replay = root / "replay"
            cas = initial / ".blaze/v2/store/f5s3"
            cas.mkdir(parents=True)
            (cas / "seg-one").write_bytes(b"cas bytes")
            (initial / ".blaze/v2/store/CACHEDIR.TAG").write_bytes(b"tag")
            evidence = runner.clone_store(initial, replay)
            copied = replay / ".blaze/v2/store/f5s3/seg-one"
            self.assertEqual(copied.read_bytes(), b"cas bytes")
            self.assertFalse(os.path.samefile(cas / "seg-one", copied))
            self.assertEqual(evidence["f5s3_files"], 1)
            self.assertEqual(evidence["store_files"], 2)
            self.assertEqual(len(evidence["f5s3_sha256"]), 64)
            broken = root / "broken"
            (broken / ".blaze/v2/store").mkdir(parents=True)
            with self.assertRaisesRegex(ValueError, "f5s3"):
                runner.clone_store(broken, root / "other-replay")

    def test_required_feature_absence_is_a_machine_note(self):
        rows = [{"note": "bin `needs` of package `app` is not built: its required feature is off; 2 file(s) not checked"}]
        self.assertTrue(runner.skipped_target_note(rows, "needs", "app"))
        self.assertFalse(runner.skipped_target_note(rows, "host-needs", "app"))
        self.assertFalse(runner.skipped_target_note(rows, "needs", "other"))
        self.assertFalse(runner.skipped_target_note([{"note": "something else"}], "needs", "app"))

    def test_export_stats_and_step_environment_are_exact(self):
        record = {"stderr": "noise\nexport stats: functions=3 folded=1 stubs=0 data=2 tls=0 compiled=0 cached=3 unkeyed=0 bytes=9 wall=4ms\n"}
        self.assertEqual(runner.parse_export_stats(record)["compiled"], 0)
        self.assertEqual(runner.parse_export_stats(record)["cached"], 3)
        with self.assertRaisesRegex(ValueError, "no `export stats:`"):
            runner.parse_export_stats({"stderr": ""})
        env = runner.step_environment({"KEEP": "yes", "FORKFIXES_CALLER_VALUE": "old"},
                                         {"env": {"FORKFIXES_CALLER_VALUE": None}})
        self.assertNotIn("FORKFIXES_CALLER_VALUE", env)
        self.assertEqual(env["KEEP"], "yes")
        env = runner.step_environment({}, {"env": {"FORKFIXES_CALLER_VALUE": "first"}})
        self.assertEqual(env["FORKFIXES_CALLER_VALUE"], "first")

    def test_commands_write_exact_bounded_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            record = runner.run_command(["/bin/sh", "-c", "printf out; printf err >&2"], root,
                                           {"PATH": "/usr/bin:/bin"}, 10, root / "evidence.json", "probe")
            self.assertEqual((record["returncode"], record["stdout"], record["stderr"], record["timed_out"]),
                             (0, "out", "err", False))
            loaded = json.loads((root / "evidence.json").read_text())
            self.assertEqual(loaded["argv"], ["/bin/sh", "-c", "printf out; printf err >&2"])
            timed = runner.run_command(["/bin/sh", "-c", "sleep 5"], root, {"PATH": "/usr/bin:/bin"},
                                          1, root / "timeout.json", "timeout-probe")
            self.assertTrue(timed["timed_out"])


    def test_fixture_manifests_and_scalar_positions(self):
        for case in runner.CASES:
            for name, text in case["files"].items():
                if name.endswith("Cargo.toml"):
                    with self.subTest(case=case["name"], file=name):
                        tomllib.loads(text)
        columns = next(case for case in runner.CASES if case["name"] == "column-counts-scalars")
        source = columns["files"]["src/main.rs"].splitlines()
        expected = [line.index("column!()") + 1 for line in source[:4]]
        expected.append(source[6].index("outer!()") + 1)
        self.assertEqual(columns["prints"], [" ".join(map(str, expected))])
        pointer = next(case for case in runner.CASES if case["name"] == "track-caller-through-function-pointers")
        lines = pointer["files"]["src/main.rs"].splitlines()
        spans = ("tracked());", "fn tracked()", "0u8.m());", "d.m());", "trait T", "fn n(&self)", "0u8.n());")
        for location, source in zip(pointer["prints"], spans, strict=True):
            self.assertIn(source, lines[int(location) - 1])

    def test_typed_input_rejects_unknown_names_types_and_nonpositive_bounds(self):
        for extra in ({"only": ["typo"]}, {"timeout": 0}, {"stall_bound": -1}, {"workers": 0}):
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                runner.parse(runner.Input, json.dumps({"blaze": "blaze", "scratch": "scratch", **extra}), "test")
        for extra in ({"unknown": True}, {"only": "raw-duplicate-import"}, {"timeout": True}):
            with self.subTest(extra=extra), self.assertRaises(SystemExit):
                runner.parse(runner.Input, json.dumps({"blaze": "blaze", "scratch": "scratch", **extra}), "test")

    def test_scratch_evidence_is_never_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            given = runner.Input(**self.fixture_input(Path(directory)))
            scratch = Path(given.scratch)
            scratch.mkdir()
            evidence = scratch / "results.json"
            evidence.write_text("kept")
            with self.assertRaisesRegex(ValueError, "new or empty"):
                runner.main(given)
            self.assertEqual(evidence.read_text(), "kept")

    def test_environment_preserves_library_and_assigned_resources_without_delegation(self):
        assigned = {"BLAZE_LIBRARY": "/library", "BLAZE_CORES": "12", "BLAZE_DAEMON_MEMORY_GB": "64", "BLAZE_HUGE_PAGES": "off"}
        foreign = {"BLAZE_DIST_SOCKET": "foreign", "RUSTC": "foreign", "CARGO_TARGET_DIR": "foreign"}
        with patch.dict(runner.os.environ, {**assigned, **foreign, "BLAZE_LOCK": "locked"}, clear=True):
            actual = runner.environment(Path("/isolated"))
        self.assertEqual({k: actual[k] for k in assigned}, assigned)
        self.assertEqual(actual["BLAZE_LOCK"], "write")
        self.assertFalse(foreign.keys() & actual.keys())

    def test_timeout_keeps_full_output_and_bounds_kill_and_reap(self):
        process = Mock(pid=123, returncode=-9)
        process.wait.side_effect = [subprocess.TimeoutExpired("fake", 1), subprocess.TimeoutExpired("fake", 5), -9]
        def spawn(*args, **kwargs):
            kwargs["stdout"].write(b"all output\n" * 1000)
            kwargs["stderr"].write(b"the cause\n")
            return process
        with tempfile.TemporaryDirectory() as directory, patch.object(runner.subprocess, "Popen", side_effect=spawn), patch.object(runner.os, "killpg") as kill:
            root = Path(directory)
            log = runner.CommandLog(root, "timeout")
            with self.assertRaisesRegex(runner.Failed, "timed out"):
                log(["fake"], root, {}, 1, "build")
            evidence = log.directory / "001-build.json"
            self.assertEqual(evidence.with_suffix(".stdout").read_text(), "all output\n" * 1000)
            self.assertEqual(evidence.with_suffix(".stderr").read_text(), "the cause\n")
            self.assertTrue(json.loads(evidence.read_text())["timed_out"])
            self.assertEqual(kill.call_args_list, [unittest.mock.call(123, signal.SIGTERM), unittest.mock.call(123, signal.SIGKILL)])
            self.assertTrue(all(call.kwargs["timeout"] <= 5 for call in process.wait.call_args_list))

    def test_daemon_start_requires_its_own_pid_before_the_deadline(self):
        for label, clock, pid, failure in (("foreign", [0, 0], 2, "expected 1"), ("deadline", [0, 31], None, "30 s")):
            with self.subTest(label=label), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                daemon = runner.CheckoutDaemon(Path("fake"), root, root, {}, runner.CommandLog(root, "case"))
                with patch.object(runner.subprocess, "Popen", return_value=Mock(pid=1, poll=Mock(return_value=None))), \
                        patch.object(daemon, "status", return_value=pid), patch.object(runner.time, "monotonic", side_effect=clock):
                    with self.assertRaisesRegex(runner.Failed, failure):
                        daemon.start()

    def test_startup_failure_and_cleanup_failure_are_checkpointed(self):
        with tempfile.TemporaryDirectory() as directory:
            given = runner.Input(**self.fixture_input(Path(directory)))
            with patch.object(runner.CheckoutDaemon, "start", side_effect=runner.Failed("startup bound")), \
                    patch.object(runner.CheckoutDaemon, "stop", side_effect=runner.Failed("stop bound")) as stop, \
                    patch.object(runner, "perform_step") as step, patch.object(runner, "provenance", return_value={"id": "same"}):
                self.assertEqual(runner.main(given), 1)
            step.assert_not_called()
            stop.assert_called_once()
            scratch = Path(given.scratch)
            report = json.loads((scratch / "run.json").read_text())
            self.assertFalse(report["complete"])
            self.assertEqual(report["errors"], ["startup bound", "cleanup: stop bound"])
            results = json.loads((scratch / "results.json").read_text())
            self.assertEqual(results[runner.CASES[1]["name"]]["why"], "not run")

    def test_timeout_aborts_later_cases_and_keeps_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            given = runner.Input(**self.fixture_input(Path(directory)))
            with patch.object(runner.CheckoutDaemon, "start"), patch.object(runner.CheckoutDaemon, "check_alive"), \
                    patch.object(runner.CheckoutDaemon, "stop") as stop, patch.object(runner, "provenance", return_value={"id": "same"}), \
                    patch.object(runner, "run_command", return_value={"timed_out": True}) as command:
                self.assertEqual(runner.main(given), 1)
            command.assert_called_once()
            stop.assert_called_once()
            results = json.loads((Path(given.scratch) / "results.json").read_text())
            self.assertIn("timed out", results[runner.CASES[0]["name"]]["why"])
            self.assertEqual(results[runner.CASES[1]["name"]]["why"], "not run")

    def test_binary_or_source_drift_invalidates_an_otherwise_passing_run(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            def git(*args):
                subprocess.run(["git", "-C", str(source), *args], check=True, capture_output=True, timeout=5)
            git("init")
            tracked = source / "tracked.rs"
            tracked.write_text("before")
            git("add", ".")
            git("-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "fixture")
            for plant in ("tracked", "untracked", "executable"):
                with self.subTest(plant=plant):
                    inputs = self.fixture_input(root)
                    scratch = root / plant
                    given = runner.Input(**{**inputs, "source": str(source), "scratch": str(scratch), "only": ("raw-duplicate-import",)})
                    def edit(case, given, scratch):
                        path = {"tracked": tracked, "untracked": source / "new.rs", "executable": Path(given.blaze)}[plant]
                        path.write_text("after")
                        return {"ok": True, "why": "as stated", "fixes": case["fixes"]}
                    with patch.object(runner, "run_case", side_effect=edit):
                        self.assertEqual(runner.main(given), 1)
                    self.assertIn("changed during acceptance", (scratch / "run.json").read_text())
                    self.assertTrue(json.loads((scratch / "results.json").read_text())["raw-duplicate-import"]["ok"])

    def test_successful_stop_command_cannot_hide_the_owned_daemons_crash(self):
        for initial, status in ((None, 0), (None, 77), (None, -9), (0, 0), (-9, -9)):
            with self.subTest(initial=initial, status=status), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                daemon = runner.CheckoutDaemon(Path("fake"), root, root, {}, runner.CommandLog(root, "case"))
                process = Mock(pid=123, returncode=initial)
                process.poll.return_value = initial
                process.wait.return_value = status
                daemon.process = process
                with patch.object(daemon, "status", return_value=123), patch.object(runner.CommandLog, "__call__", return_value={"returncode": 0}) as command, \
                        patch.object(runner, "terminate") as terminate:
                    if status == 0 and initial is None:
                        daemon.stop()
                    else:
                        with self.assertRaisesRegex(runner.Failed, f"owned daemon exited {status}"):
                            daemon.stop()
                    self.assertEqual(command.call_count, int(initial is None))
                    terminate.assert_called_once_with(process)
                    self.assertIsNone(daemon.process)

    def test_compiler_rejection_requires_an_error_diagnostic_and_failed_verdict(self):
        row = {"severity": "error", "code": "E0252", "message": "duplicate import"}
        summary = {"errors": 1, "not_checked": []}
        for rows, code, ok in ((["E0252", summary], 1, False), ([{**row, "severity": "warning"}, summary], 1, False),
                ([{**row, "code": "E0425", "message": "unknown name E0252"}, summary], 1, False),
                ([row], 1, False), ([row, {**summary, "errors": 0}], 1, False), ([row, summary], 0, False),
                ([row, summary], 1, True)):
            record = {"returncode": code, "stdout": "\n".join(map(json.dumps, rows)), "timed_out": False}
            with self.subTest(rows=rows, code=code):
                log = Mock(return_value=record)
                if ok:
                    runner.perform_diagnostic(runner.Input("fake", "scratch"), Path("."), {}, log, "E0252")
                else:
                    with self.assertRaises(runner.CaseFailed):
                        runner.perform_diagnostic(runner.Input("fake", "scratch"), Path("."), {}, log, "E0252")

    def test_all_blaze_commands_use_selected_binary_and_fresh_fixtures_can_write_locks(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            given = runner.Input(**self.fixture_input(root), only=("raw-duplicate-import",))
            commands = []
            def command(argv, cwd, env, timeout, evidence, label):
                commands.append([str(a) for a in argv])
                self.assertEqual(str(argv[0]), given.blaze)
                self.assertEqual(env["BLAZE_LOCK"], "write")
                code = 0
                if "--status" in argv:
                    stdout = '{"daemon":{"pid":42}}'
                elif "check" in argv:
                    self.assertIn("use b::x;", (cwd / "src/main.rs").read_text())
                    code = 1
                    stdout = '{"code":"E0252","severity":"error"}\n{"errors":1,"not_checked":[]}'
                else:
                    if "run" in argv:
                        self.assertNotIn("use b::x;", (cwd / "src/main.rs").read_text())
                    stdout = ""
                return {"returncode": code, "stdout": stdout, "stderr": "", "timed_out": False}
            process = Mock(pid=42, poll=Mock(return_value=None), wait=Mock(return_value=0))
            with patch.dict(os.environ, {"CI": "true", "BLAZE_LOCK": "locked"}), \
                    patch.object(runner, "run_command", side_effect=command), patch.object(runner, "terminate"), \
                    patch.object(runner.subprocess, "Popen", return_value=process) as spawn:
                result = runner.run_case(runner.CASES[0], given, root)
            self.assertTrue(result["ok"], result)
            self.assertEqual(spawn.call_args.args[0], [given.blaze, "daemon"])
            self.assertEqual(spawn.call_args.kwargs["env"]["BLAZE_LOCK"], "write")
            self.assertEqual([call[1] for call in commands if call[1] != "daemon"], ["check", "run", "check"])
            self.assertEqual(commands[-1], [given.blaze, "daemon", "--stop"])

    def test_caller_environment_changes_and_unsets_without_changing_the_daemon(self):
        case = next(case for case in runner.CASES if case["name"] == "caller-environment-changes-and-becomes-unset")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            given = runner.Input(**self.fixture_input(root))
            observed = []
            daemon_environment = []
            def command(argv, cwd, env, timeout, evidence, label):
                value = env.get("FORKFIXES_CALLER_VALUE", "unset")
                observed.append(value)
                return {"returncode": 0, "stdout": f"{value} {value}\n", "stderr": "", "timed_out": False}
            with patch.dict(os.environ, {"FORKFIXES_CALLER_VALUE": "ambient"}), \
                    patch.object(runner.CheckoutDaemon, "start", lambda d: daemon_environment.append(d.env)), \
                    patch.object(runner.CheckoutDaemon, "check_alive"), patch.object(runner.CheckoutDaemon, "stop"), \
                    patch.object(runner, "run_command", side_effect=command):
                result = runner.run_case(case, given, root)
            self.assertTrue(result["ok"], result)
            self.assertEqual(observed, ["unset", "first", "second", "unset", "first"])
            self.assertEqual([env["FORKFIXES_CALLER_VALUE"] for env in daemon_environment], ["ambient"])
    def test_replay_requires_independent_store_homes_complete_identical_artifacts_and_cache_hits(self):
        case = next(case for case in runner.CASES if case["name"] == "cdylib-allocates")
        for plant in (None, "missing-library", "missing-program", "library-bytes", "extra", "compiled-library",
                      "compiled-program", "zero-hits", "no-stats", "repeated"):
            with self.subTest(plant=plant), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                given = runner.Input(**self.fixture_input(root))
                homes = []
                active = []
                executions = []
                exports = []
                def start(daemon):
                    self.assertFalse(active, "replays must run one owned daemon at a time")
                    active.append(daemon)
                    physical = daemon.home.resolve()
                    cas = physical / ".blaze/v2/store/f5s3/pack"
                    if homes:
                        self.assertEqual({p.name for p in (physical / ".blaze/v2").iterdir()}, {"toolchain", "store"})
                        self.assertEqual(cas.read_bytes(), b"seed")
                        for earlier in homes:
                            self.assertFalse(cas.samefile(earlier / ".blaze/v2/store/f5s3/pack"))
                    else:
                        cas.parent.mkdir(parents=True)
                        cas.write_bytes(b"seed")
                    homes.append(physical)
                def stop(daemon):
                    if daemon in active:
                        active.remove(daemon)
                def command(argv, cwd, env, timeout, evidence, label):
                    self.assertEqual(Path(env["HOME"]).resolve(), homes[-1])
                    stdout = stderr = ""
                    if argv[0] == given.blaze:
                        self.assertEqual(argv[1], "export")
                        out = Path(argv[argv.index("-o") + 1])
                        library = "--lib" in argv
                        if library:
                            self.assertEqual(out.name, "libexported.dylib" if sys.platform == "darwin" else "libexported.so")
                        exports.append(label)
                        replay = len(homes) != 1
                        missing = replay and plant == ("missing-library" if library else "missing-program")
                        if not missing:
                            out.write_bytes(b"corrupt" if library and replay and plant == "library-bytes" else out.name.encode())
                            out.chmod(0o755)
                        if replay and plant == "extra":
                            (out.parent / "unexpected").write_bytes(b"extra")
                        compiled = int(not replay or plant == ("compiled-library" if library else "compiled-program"))
                        if not replay or plant != "no-stats":
                            stderr = f"export stats: compiled={compiled} cached={int(replay and plant != 'zero-hits')}\n"
                    else:
                        executions.append(Path(argv[0]))
                        stdout = "12\n"
                    return {"returncode": 0, "stdout": stdout, "stderr": stderr, "timed_out": False}
                with patch.object(runner.CheckoutDaemon, "start", start), patch.object(runner.CheckoutDaemon, "stop", stop), \
                        patch.object(runner.CheckoutDaemon, "check_alive"), patch.object(runner, "run_command", side_effect=command):
                    repeated = plant == "repeated"
                    steps = case["steps"] + ([case["steps"][-1]] if repeated else [])
                    result = runner.run_case({**case, "steps": steps}, given, root)
                self.assertEqual(result["ok"], plant in (None, "repeated"), result)
                self.assertEqual(len(homes), 5 if repeated else 3)
                self.assertEqual(len(set(homes)), len(homes))
                self.assertFalse(active)
                self.assertEqual(sum(label.endswith("-initial") for label in exports), 2)
                self.assertEqual(len(executions), len(homes) if result["ok"] else 1)
                if plant is None:
                    self.assertEqual(executions[0], executions[2])
                    self.assertNotEqual(executions[0], executions[1])


if __name__ == "__main__":
    unittest.main()
