"""Lightweight harness checks: fake processes only; never starts Blaze or a compiler.

    python3 -m unittest discover -s harness/forkfixes -p 'test_*.py'
"""
from __future__ import annotations

import importlib.util
import json
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


class HarnessChecks(unittest.TestCase):
    def test_typed_input_rejects_unknown_names_types_and_nonpositive_bounds(self):
        for extra in ({"only": ["typo"]}, {"timeout": 0}, {"stall_bound": -1}):
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                runner.parse(runner.Input, json.dumps({"blaze": "blaze", "scratch": "scratch", **extra}), "test")
        for extra in ({"unknown": True}, {"only": "raw-duplicate-import"}, {"timeout": True}):
            with self.subTest(extra=extra), self.assertRaises(SystemExit):
                runner.parse(runner.Input, json.dumps({"blaze": "blaze", "scratch": "scratch", **extra}), "test")

    def test_scratch_evidence_is_never_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory) / "results.json"
            evidence.write_text("kept")
            with self.assertRaisesRegex(ValueError, "preserving existing evidence"):
                runner.main(runner.Input("unused", directory))
            self.assertEqual(evidence.read_text(), "kept")

    def test_environment_preserves_library_and_assigned_resources_without_delegation(self):
        env = {"BLAZE_LIBRARY": "/library", "BLAZE_CORES": "12", "BLAZE_DAEMON_MEMORY_GB": "64", "BLAZE_HUGE_PAGES": "off",
               "BLAZE_DIST_SOCKET": "foreign", "BLAZE_LOCK": "locked", "RUSTC": "foreign", "CARGO_TARGET_DIR": "foreign"}
        with tempfile.TemporaryDirectory() as directory, patch.dict(runner.os.environ, env, clear=True):
            actual = runner.env_for(Path(directory))
        self.assertEqual(actual["BLAZE_LIBRARY"], "/library")
        self.assertEqual(actual["BLAZE_CORES"], "12")
        self.assertEqual(actual["BLAZE_DAEMON_MEMORY_GB"], "64")
        self.assertEqual(actual["BLAZE_HUGE_PAGES"], "off")
        self.assertEqual(actual["BLAZE_LOCK"], "write")
        self.assertFalse({"BLAZE_DIST_SOCKET", "RUSTC", "CARGO_TARGET_DIR"} & actual.keys())

    def test_timeout_keeps_full_output_and_bounds_kill_and_reap(self):
        process = Mock(pid=123, returncode=-9)
        process.wait.side_effect = [subprocess.TimeoutExpired("fake", 1), None, None, None]
        process.poll.return_value = None

        def spawn(*args, **kwargs):
            kwargs["stdout"].write("all output\n" * 1000)
            kwargs["stderr"].write("the cause\n")
            return process

        with tempfile.TemporaryDirectory() as directory, patch.object(runner.subprocess, "Popen", side_effect=spawn), patch.object(runner.os, "killpg") as kill:
            log = Path(directory) / "timed-out"
            with self.assertRaises(runner.Failed):
                runner.command(["fake"], Path(directory), {}, log, 1)
            self.assertEqual(log.with_suffix(".stdout").read_text(), "all output\n" * 1000)
            self.assertEqual(log.with_suffix(".stderr").read_text(), "the cause\n")
            self.assertTrue(json.loads(log.with_suffix(".json").read_text())["timed_out"])
            self.assertEqual(kill.call_args_list, [unittest.mock.call(123, signal.SIGTERM), unittest.mock.call(123, signal.SIGKILL)])
            self.assertTrue(all(call.kwargs["timeout"] <= 5 for call in process.wait.call_args_list))

    def test_startup_refuses_wrong_pid_and_does_not_build(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "logs").mkdir()
            daemon = runner.Daemon(runner.Input("fake", directory), root)
            with patch.object(runner.subprocess, "Popen", return_value=Mock(pid=1, poll=Mock(return_value=None))), patch.object(daemon, "status", return_value=2):
                with self.assertRaisesRegex(runner.Failed, "expected 1"):
                    daemon.start()

    def test_startup_failure_and_cleanup_failure_are_checkpointed(self):
        with tempfile.TemporaryDirectory() as directory:
            scratch = Path(directory) / "run"
            daemon = Mock()
            daemon.env = {}
            daemon.start.side_effect = runner.Failed("startup bound")
            daemon.stop.side_effect = runner.Failed("stop bound")
            with patch.object(runner, "Daemon", return_value=daemon), patch.object(runner, "provenance", return_value={}), patch.object(runner, "run_case") as case:
                status = runner.main(runner.Input(sys.executable, str(scratch), only=("raw-duplicate-import",)))
            self.assertEqual(status, 1)
            case.assert_not_called()
            result = json.loads((scratch / "results.json").read_text())["raw-duplicate-import"]
            self.assertEqual(result["why"], "not run")
            report = json.loads((scratch / "run.json").read_text())
            self.assertFalse(report["complete"])
            self.assertEqual(len(report["errors"]), 2)
            daemon.stop.assert_called_once()

    def test_timeout_aborts_later_cases_and_keeps_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            scratch = Path(directory) / "run"
            daemon = Mock(env={})
            with patch.object(runner, "Daemon", return_value=daemon), patch.object(runner, "provenance", return_value={}), patch.object(runner, "run_case", side_effect=runner.Failed("timed out: logs/build.*")) as case:
                status = runner.main(runner.Input(sys.executable, str(scratch)))
            self.assertEqual(status, 1)
            self.assertEqual(case.call_count, 1)
            results = json.loads((scratch / "results.json").read_text())
            self.assertIn("timed out", results[runner.CASES[0]["name"]]["why"])
            self.assertEqual(results[runner.CASES[1]["name"]]["why"], "not run")

    def test_binary_or_source_drift_invalidates_an_otherwise_passing_run(self):
        with tempfile.TemporaryDirectory() as directory:
            scratch = Path(directory) / "run"
            with patch.object(runner, "Daemon", return_value=Mock(env={})), patch.object(runner, "provenance", side_effect=[{"hash": "before"}, {"hash": "after"}]), patch.object(runner, "run_case", return_value=(True, "as stated")):
                status = runner.main(runner.Input(sys.executable, str(scratch), only=("raw-duplicate-import",)))
            self.assertEqual(status, 1)
            self.assertIn("changed during acceptance", (scratch / "run.json").read_text())
            self.assertTrue(json.loads((scratch / "results.json").read_text())["raw-duplicate-import"]["ok"])

    def test_startup_deadline_is_a_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "logs").mkdir()
            daemon = runner.Daemon(runner.Input("fake", directory), root)
            with patch.object(runner.subprocess, "Popen", return_value=Mock(pid=1)), patch.object(runner.time, "monotonic", side_effect=[0, 601]):
                with self.assertRaisesRegex(runner.Failed, "600 s"):
                    daemon.start()

    def test_warm_replay_must_report_store_hits(self):
        case = {"name": "replay", "files": {"src/main.rs": "fn main() {}"},
                "steps": [{"replay": True, "prints": []}]}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            daemon = Mock(given=runner.Input("fake", directory), scratch=root, env={})

            def fake_command(argv, cwd, env, log, timeout):
                log.parent.mkdir(parents=True, exist_ok=True)
                log.with_suffix(".stdout").write_text("")
                log.with_suffix(".stderr").write_text("")
                return {"returncode": 0}

            with patch.object(runner, "command", side_effect=fake_command):
                ok, why = runner.run_case(daemon, case)
            self.assertFalse(ok)
            self.assertIn("store replay must report cached functions and compile none", why)
            daemon.replay.assert_called_once()

    def test_compiler_rejection_requires_an_error_diagnostic_and_failed_verdict(self):
        case = {"name": "reject", "files": {"src/main.rs": "invalid"}, "diagnostic": "E0252"}
        error = {"reason": "compiler-message", "message": {
            "level": "error", "code": {"code": "E0252"}, "message": "duplicate import",
        }}
        failed = json.dumps({"reason": "build-finished", "success": False})
        diagnostic = json.dumps(error)
        warning = json.dumps({**error, "message": {**error["message"], "level": "warning"}})
        unrelated = json.dumps({**error, "message": {
            **error["message"], "code": {"code": "E0425"}, "rendered": "source line mentioning E0252",
        }})
        variants = (
            ("plain log", "E0252\n" + failed, False),
            ("warning", warning + "\n" + failed, False),
            ("quoted source", unrelated + "\n" + failed, False),
            ("no verdict", diagnostic, False),
            ("successful verdict", diagnostic + "\n" + json.dumps({"reason": "build-finished", "success": True}), False),
            ("compiler refusal", diagnostic + "\n" + failed, True),
        )
        for name, output, expected in variants:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                daemon = Mock(given=runner.Input("fake", directory), scratch=Path(directory), env={})

                def fake_command(argv, cwd, env, log, timeout):
                    log.parent.mkdir(parents=True, exist_ok=True)
                    log.with_suffix(".stdout").write_text(output)
                    log.with_suffix(".stderr").write_text("")
                    return {"returncode": 101}

                with patch.object(runner, "command", side_effect=fake_command):
                    self.assertEqual(runner.run_case(daemon, case)[0], expected)

    def test_all_blaze_commands_ignore_pins_and_fresh_fixtures_can_write_locks(self):
        case = {"name": "reject", "files": {"src/main.rs": "invalid"}, "diagnostic": "E0252"}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "logs").mkdir()
            daemon = runner.Daemon(runner.Input("fake", directory), root)
            process = Mock(pid=42, poll=Mock(return_value=None))
            commands = []

            def fake_command(argv, cwd, env, log, timeout):
                commands.append(argv)
                self.assertEqual(env["BLAZE_LOCK"], "write")
                log.parent.mkdir(parents=True, exist_ok=True)
                building = "build" in argv
                output = "{}"
                if building:
                    output = json.dumps({"reason": "compiler-message", "message": {
                        "level": "error", "code": {"code": "E0252"}, "message": "duplicate import",
                    }}) + "\n" + json.dumps({"reason": "build-finished", "success": False})
                log.with_suffix(".stdout").write_text(output)
                log.with_suffix(".stderr").write_text("")
                return {"returncode": 101 if building else 0}

            with patch.dict(runner.os.environ, {"BLAZE_LOCK": "locked", "CI": "true"}, clear=True), \
                    patch.object(runner.subprocess, "Popen", return_value=process) as spawn, \
                    patch.object(runner, "command", side_effect=fake_command), \
                    patch.object(runner, "blaze_status_of", return_value=Mock(daemon=Mock(pid=42))), \
                    patch.object(runner.os, "killpg"):
                daemon.start()
                self.assertTrue(runner.run_case(daemon, case)[0])
                daemon.stop()
                self.assertEqual(spawn.call_args.args[0], ["fake", "--ignore-pin", "serve"])
                self.assertEqual(spawn.call_args.kwargs["env"]["BLAZE_LOCK"], "write")
            self.assertEqual({argv[2] for argv in commands}, {"status", "build", "stop"})
            self.assertTrue(all(argv[:2] == ["fake", "--ignore-pin"] for argv in commands))

    def test_replay_checks_library_bytes_and_the_complete_artifact_set(self):
        case = {"name": "replay", "files": {"src/main.rs": "fn main() {}"},
                "steps": [{"prints": []}, {"replay": True, "prints": []}]}
        for plant in (None, "library-bytes", "missing-library", "compiled"):
            with self.subTest(plant=plant), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                daemon = Mock(given=runner.Input("fake", directory), scratch=root, env={})
                runs = []

                def fake_command(argv, cwd, env, log, timeout):
                    log.parent.mkdir(parents=True, exist_ok=True)
                    output = ""
                    if argv[0] == "fake":
                        target = Path(argv[argv.index("--target-dir") + 1]) / "debug"
                        target.mkdir(parents=True, exist_ok=True)
                        restored = "replay-1" in target.parts
                        program, library = target / "replay", target / "libexported.so"
                        program.write_text("same executable")
                        library.write_text("changed library" if restored and plant == "library-bytes" else "same library")
                        outputs = [{"reason": "compiler-artifact", "target": {"name": "replay"},
                                    "executable": str(program), "filenames": [str(program)]}]
                        if not (restored and plant == "missing-library"):
                            outputs.append({"reason": "compiler-artifact", "target": {"name": "exported"},
                                            "executable": None, "filenames": [str(library)]})
                        outputs.append({"reason": "blaze-work", "reran": 0, "green": 0,
                                        "compiled": int(restored and plant == "compiled"), "found": 2})
                        output = "\n".join(map(json.dumps, outputs))
                    else:
                        runs.append(argv[0])
                    log.with_suffix(".stdout").write_text(output)
                    log.with_suffix(".stderr").write_text("")
                    return {"returncode": 0}

                with patch.object(runner, "command", side_effect=fake_command):
                    ok, why = runner.run_case(daemon, case)
                self.assertEqual(ok, plant is None, why)
                self.assertEqual(len(runs), 2 if plant is None else 1)

    def test_a_reported_artifact_cannot_be_silently_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            program = root / "program"
            program.write_text("present")
            output = json.dumps({"reason": "compiler-artifact", "filenames": [str(root / "missing.so")]})
            with self.assertRaisesRegex(runner.Failed, "reported artifact does not exist"):
                runner.artifact_hashes(output, str(program), root)

    def test_repeated_replays_keep_the_store_and_get_distinct_empty_homes(self):
        with tempfile.TemporaryDirectory() as directory:
            daemon = runner.Daemon(runner.Input("fake", directory), Path(directory).resolve())
            store = Path(daemon.env["BLAZE_HOME"]) / "v2" / "store"
            store.mkdir(parents=True)
            daemon.generation = 1

            def start():
                daemon.generation += 1

            homes = []
            with patch.object(daemon, "stop"), patch.object(daemon, "start", side_effect=start):
                for _ in range(2):
                    daemon.replay()
                    home = Path(daemon.env["BLAZE_HOME"]) / "v2"
                    self.assertEqual([path.name for path in home.iterdir()], ["store"])
                    self.assertEqual((home / "store").resolve(), store)
                    homes.append(home)
            self.assertNotEqual(*homes)

    def test_caller_environment_changes_and_unsets_without_changing_the_daemon(self):
        case = next(case for case in runner.CASES if case["name"] == "caller-environment-changes-and-becomes-unset")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            daemon = Mock(given=runner.Input("fake", directory), scratch=root,
                          env={"FORKFIXES_CALLER_VALUE": "ambient"})
            observed = []

            def fake_command(argv, cwd, env, log, timeout):
                log.parent.mkdir(parents=True, exist_ok=True)
                value = env.get("FORKFIXES_CALLER_VALUE", "unset")
                observed.append(value)
                if argv[0] == "fake":
                    target = Path(argv[argv.index("--target-dir") + 1])
                    target.mkdir(parents=True, exist_ok=True)
                    program = target / "program"
                    program.write_text("fake")
                    output = json.dumps({"reason": "compiler-artifact", "executable": str(program),
                                         "target": {"name": case["name"].replace("-", "_")}})
                else:
                    output = f"{value} {value}\n"
                log.with_suffix(".stdout").write_text(output)
                log.with_suffix(".stderr").write_text("")
                return {"returncode": 0}

            with patch.object(runner, "command", side_effect=fake_command):
                self.assertTrue(runner.run_case(daemon, case)[0])
            self.assertEqual(observed, [value for value in ("unset", "first", "second", "unset", "first") for _ in range(2)])
            self.assertEqual(daemon.env, {"FORKFIXES_CALLER_VALUE": "ambient"})

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

    def test_duplicate_control_edits_cause_and_restores_it(self):
        case = next(case for case in runner.CASES if case["name"] == "raw-duplicate-import")
        observed = []
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            daemon = Mock(given=runner.Input("fake", directory), scratch=root, env={})
            def fake_command(argv, cwd, env, log, timeout):
                log.parent.mkdir(parents=True, exist_ok=True)
                duplicate = "use b::x;" in (cwd / "src/main.rs").read_text()
                if argv[0] == "fake":
                    observed.append(duplicate)
                    if duplicate:
                        text = json.dumps({"reason": "compiler-message", "message": {
                            "level": "error", "message": "duplicate import", "code": {"code": "E0252"},
                        }}) + "\n" + json.dumps({"reason": "build-finished", "success": False})
                    else:
                        target = Path(argv[argv.index("--target-dir") + 1])
                        target.mkdir(parents=True, exist_ok=True)
                        program = target / "program"
                        program.write_text("fake program")
                        text = json.dumps({"reason": "compiler-artifact", "target": {"name": "raw_duplicate_import"}, "executable": str(program)})
                    code = int(duplicate)
                else:
                    text, code = "", 0
                log.with_suffix(".stdout").write_text(text)
                log.with_suffix(".stderr").write_text("")
                return {"returncode": code}

            with patch.object(runner, "command", side_effect=fake_command):
                self.assertTrue(runner.run_case(daemon, case)[0])
            self.assertEqual(observed, [True, False, True])


if __name__ == "__main__":
    unittest.main()
