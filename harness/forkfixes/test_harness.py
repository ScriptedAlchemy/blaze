"""Lightweight harness checks: fake processes only; never starts Blaze or a compiler.

    python3 -m unittest discover -s harness/forkfixes -p 'test_*.py'
"""
from __future__ import annotations

import importlib.util
import hashlib
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

    def test_check_json_requires_one_complete_summary_and_preserves_e_codes(self):
        record = {"stdout": '{"severity":"error","code":"types","message":"cannot find value `x` in this scope (E0425 UnresolvedPath)"}\n'
                            '{"errors":1,"warnings":0,"fixes":0,"command":"blaze check","not_checked":[]}\n'}
        rows, summary = runner.check_rows(record)
        self.assertEqual(summary["errors"], 1)
        self.assertTrue(runner.diagnostic_matches(rows, "E0425"))
        self.assertFalse(runner.diagnostic_matches(rows, "E0252"))
        reported = [{"severity": "error", "code": "borrows", "diagnostic_code": "E0515", "message": "reference to local data returned"}]
        self.assertTrue(runner.diagnostic_matches(reported, "E0515"))
        self.assertFalse(runner.diagnostic_matches(reported, "E0597"))
        self.assertFalse(runner.diagnostic_matches([dict(rows[0], diagnostic_code="E0308")], "E0425"))
        message = ("found `u32`, about `value` (E0599 NoMethod), at expression #5 "
                   "(a method call; its range is shared by 7 nodes): `{integer}.value()`")
        self.assertTrue(runner.diagnostic_matches([dict(rows[0], message=message)], "E0599"))
        self.assertFalse(runner.diagnostic_matches([dict(rows[0], message=message + " E0252")], "E0252"))
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

    def test_replay_store_clone_verifies_cas_bytes_as_independent_copies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            initial = root / "initial"
            replay = root / "replay"
            cas = initial / ".blaze/v2/store/f6s3"
            cas.mkdir(parents=True)
            (cas / "seg-one").write_bytes(b"cas bytes")
            (initial / ".blaze/v2/store/CACHEDIR.TAG").write_bytes(b"tag")
            evidence = runner.clone_store(initial, replay)
            copied = replay / ".blaze/v2/store/f6s3/seg-one"
            self.assertEqual(copied.read_bytes(), b"cas bytes")
            self.assertFalse(os.path.samefile(cas / "seg-one", copied))
            self.assertEqual(evidence["cas_files"], 1)
            self.assertEqual(evidence["cas_format"], "f6s3")
            self.assertEqual(evidence["store_files"], 2)
            self.assertEqual(len(evidence["cas_sha256"]), 64)
            broken = root / "broken"
            (broken / ".blaze/v2/store").mkdir(parents=True)
            with self.assertRaisesRegex(ValueError, "must have one CAS format"):
                runner.clone_store(broken, root / "other-replay")
            (cas.parent / "f5s3").mkdir()
            with self.assertRaisesRegex(ValueError, "f5s3.*f6s3"):
                runner.clone_store(initial, root / "ambiguous-replay")

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

    def test_setup_failure_and_cleanup_failure_are_checkpointed(self):
        with tempfile.TemporaryDirectory() as directory:
            given = runner.Input(**self.fixture_input(Path(directory)))
            with patch.object(runner, "write_files", side_effect=runner.Failed("setup refused")), \
                    patch.object(runner, "cleanup_logical_home", side_effect=OSError("cleanup refused")) as cleanup, \
                    patch.object(runner, "perform_step") as step, patch.object(runner, "provenance", return_value={"id": "same"}):
                self.assertEqual(runner.main(given), 1)
            step.assert_not_called()
            cleanup.assert_called_once()
            scratch = Path(given.scratch)
            report = json.loads((scratch / "run.json").read_text())
            self.assertFalse(report["complete"])
            self.assertEqual(report["errors"], ["setup refused", "cleanup logical HOME: cleanup refused"])
            results = json.loads((scratch / "results.json").read_text())
            self.assertEqual(results[runner.CASES[1]["name"]]["why"], "not run")

    def test_timeout_aborts_later_cases_and_keeps_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            given = runner.Input(**self.fixture_input(Path(directory)))
            with patch.object(runner, "provenance", return_value={"id": "same"}), \
                    patch.object(runner, "run_command", return_value={"timed_out": True}) as command:
                self.assertEqual(runner.main(given), 1)
            command.assert_called_once()
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

    def test_compiler_rejection_requires_an_error_diagnostic_and_failed_verdict(self):
        row = {"severity": "error", "code": "E0252", "message": "duplicate import"}
        summary = {"errors": 1, "not_checked": []}
        for rows, code, ok in ((["E0252", summary], 1, False), ([{**row, "severity": "warning"}, summary], 1, False),
                ([{**row, "code": "E0425", "message": "unknown name E0252"}, summary], 1, False),
                ([row], 1, False), ([row, {**summary, "errors": 0}], 1, False), ([row, summary], 0, False),
                ([row, summary, summary], 1, False), ([summary, row], 1, False),
                ([row, summary, {"note": "the check is incomplete: blaze could not check 1 item"}], 1, False),
                ([row, summary, {"note": "a note following the summary"}], 1, True),
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
                if "check" in argv:
                    self.assertIn("use b::x;", (cwd / "src/main.rs").read_text())
                    code = 1
                    stdout = '{"code":"E0252","severity":"error"}\n{"errors":1,"not_checked":[]}'
                else:
                    if "run" in argv:
                        self.assertNotIn("use b::x;", (cwd / "src/main.rs").read_text())
                    stdout = ""
                return {"returncode": code, "stdout": stdout, "stderr": "", "timed_out": False}
            with patch.dict(os.environ, {"CI": "true", "BLAZE_LOCK": "locked"}), \
                    patch.object(runner, "run_command", side_effect=command):
                result = runner.run_case(runner.CASES[0], given, root)
            self.assertTrue(result["ok"], result)
            self.assertEqual([call[1] for call in commands], ["check", "run", "check"])

    def test_caller_environment_changes_and_unsets_for_each_command(self):
        case = next(case for case in runner.CASES if case["name"] == "caller-environment-changes-and-becomes-unset")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            given = runner.Input(**self.fixture_input(root), module_builder="/fixture/module-builder")
            observed = []
            def command(argv, cwd, env, timeout, evidence, label):
                if argv[0] == given.module_builder:
                    self.assertNotIn("FORKFIXES_CALLER_VALUE", env)
                    self.make_modules(json.loads(argv[1]))
                    return {"returncode": 0, "timed_out": False}
                value = env.get("FORKFIXES_CALLER_VALUE", "unset")
                self.assertEqual(argv, [given.blaze, "run", "-p", "caller_environment_changes_and_becomes_unset",
                                        "--bin", "caller_environment_changes_and_becomes_unset", "--workers", "2"])
                observed.append(value)
                return {"returncode": 0, "stdout": f"{value} {value}\n", "stderr": "", "timed_out": False}
            with patch.dict(os.environ, {"FORKFIXES_CALLER_VALUE": "ambient"}), \
                    patch.object(runner, "run_command", side_effect=command):
                result = runner.run_case(case, given, root)
            self.assertTrue(result["ok"], result)
            self.assertEqual(observed, ["unset", "first", "second", "unset", "first"])
            self.assertEqual(len(result["modules"]), 1)

    def make_modules(self, payload, plant=None):
        request = payload.get("build_scripts", payload)
        self.assertIs(request["unsandboxed_build_scripts"], False)
        kind = "build-script" if "build_scripts" in payload else "proc-macro"
        for krate in request["crates"]:
            name = krate["name"]
            self.assertEqual(krate, {"name": name, "version": "0.1.0", "features": []})
            changed = Path(request["checkout"]) / "dep/build.rs"
            source = runner.digest(changed) if changed.exists() else ""
            identity = f"name {name}\nrevision {source}\n"
            key = hashlib.sha256(identity.encode()).hexdigest()
            if plant == "missing":
                continue
            record = Path(request["store"]) / kind / "modules" / key
            record.mkdir(parents=True)
            (record / "identity").write_text(identity)
            (record / "module.wasm").write_bytes(b"test record")
            (record / "wasm-sha256").write_text("bad" if plant == "corrupt" else runner.digest(record / "module.wasm"))

    def test_d52_reprovisions_changed_local_build_dependency_and_reuses_restored_records(self):
        case = next(c for c in runner.CASES if c["name"] == "a-script-searches-where-its-build-dependencys-script-said")
        for plant in (None, "missing", "corrupt"):
            with self.subTest(plant=plant), tempfile.TemporaryDirectory() as directory:
                base = Path(directory)
                root = base / "project"
                runner.write_files(root, runner.initial_files(case))
                state = {"physical_home": base / "home"}
                given = runner.Input(**self.fixture_input(base), module_builder="/fixture/module-builder")
                def command(argv, *args):
                    payload = json.loads(argv[1])
                    self.assertEqual([k["name"] for k in payload["build_scripts"]["crates"]], ["dep", "app"])
                    self.make_modules(payload, plant)
                    return {"returncode": 0}
                log = Mock(side_effect=command)
                if plant:
                    with self.assertRaises(runner.CaseFailed):
                        runner.prepare_modules(root, case, given, log, state)
                    self.assertEqual(state["modules"], {})
                    continue
                for step in runner.steps_for(case):
                    runner.write_files(root, step.get("files", {}))
                    runner.prepare_modules(root, case, given, log, state)
                self.assertEqual(log.call_count, 2)
                records = state["physical_home"] / ".blaze/v2/build-script/modules"
                self.assertEqual({p.name for p in records.iterdir()},
                                 {key for event in state["modules"].values() for key in event.values()})
                self.assertEqual(len(list(records.iterdir())), 4)

    def test_archive_fixture_locks_real_cc_and_its_registry_dependencies(self):
        case = next(c for c in runner.CASES if c.get("clang"))
        lock = tomllib.loads(runner.initial_files(case)["Cargo.lock"])
        packages = {p["name"]: p for p in lock["package"]}
        self.assertEqual(packages["cc"]["version"], "1.4.3")
        self.assertEqual(packages["cc"]["checksum"], "509591b7bcd67f4ef775afad7662703b4935daaa6ec0e5605cfb1090b32a2b6d")
        self.assertEqual(packages["cc"]["dependencies"], ["find-msvc-tools", "shlex"])
        for name in ("package_a", "package_b"):
            self.assertEqual(packages[name]["dependencies"], ["cc"])
        for name in ("cc", *packages["cc"]["dependencies"]):
            self.assertEqual(packages[name]["source"], "registry+https://github.com/rust-lang/crates.io-index")
            self.assertRegex(packages[name]["checksum"], r"^[a-f0-9]{64}$")

    def test_clang_copies_the_verified_host_pin_and_internal_links_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            given = runner.Input(**self.fixture_input(root), source=str(root))
            archive = Path(given.shelf) / "clang" / ("a" * 64)
            clang = archive / "bin/clang-23"
            clang.parent.mkdir(parents=True)
            clang.write_text("pinned clang")
            link = archive / "bin/clang"
            link.symlink_to("clang-23")
            host = "aarch64-apple-darwin"
            linux = "aarch64-unknown-linux-gnu"
            pins = {"targets": {host: {"sha256": archive.name}}, "sysroots": {host: "macos-sdk"}, "shelf_programs": {host: {
                "names": "clang-23", "clang-23": {"member": "LLVM/bin/clang-23", "sha256": runner.digest(clang)}}}}
            (root / "pins").mkdir()
            sysroot = Path(given.shelf) / "sysroot" / ("b" * 64)
            sysroot.mkdir(parents=True)
            (sysroot / "stdlib.h").write_text("library")
            pins["targets"][linux] = pins["targets"][host]
            pins["shelf_programs"][linux] = pins["shelf_programs"][host]
            pins["sysroots"][linux] = sysroot.name
            (root / "pins/clang.json").write_text(json.dumps(pins))
            with patch.object(runner.platform, "machine", return_value="aarch64"), patch.object(sys, "platform", "linux"):
                home = root / "linux-home"
                runner.copy_shelf(Path(given.shelf), home)
                self.assertFalse((home / ".blaze/v2/toolchain/sysroot").exists())
                self.assertEqual(runner.copy_clang(given, home)["sysroot"], sysroot.name)
                self.assertEqual((home / ".blaze/v2/toolchain/sysroot" / sysroot.name / "stdlib.h").read_text(), "library")
            with patch.object(runner.platform, "machine", return_value="arm64"), patch.object(sys, "platform", "darwin"):
                runner.validate(given, directory)
                home = root / "home"
                runner.copy_shelf(Path(given.shelf), home)
                self.assertFalse((home / ".blaze/v2/toolchain/clang").exists())
                installed = runner.copy_clang(given, home)
                self.assertEqual(installed["members"], {"clang-23": runner.digest(clang)})
                copied = home / ".blaze/v2/toolchain/clang" / archive.name / "bin/clang"
                self.assertEqual(copied.readlink(), Path("clang-23"))
                self.assertEqual(copied.read_text(), "pinned clang")
                link.unlink()
                link.symlink_to(root / "blaze")
                with self.assertRaisesRegex(runner.CaseFailed, "escapes its pinned directory"):
                    runner.copy_clang(given, root / "escape")
                link.unlink()
                clang.write_text("different compiler")
                with self.assertRaisesRegex(runner.CaseFailed, "differs from pins/clang.json"):
                    runner.copy_clang(given, root / "changed")
                archive.rename(archive.with_name("wrong-host"))
                with self.assertRaisesRegex(runner.CaseFailed, "missing at"):
                    runner.copy_clang(given, root / "missing")

    def test_replay_requires_independent_store_homes_complete_identical_artifacts_and_cache_hits(self):
        case = next(case for case in runner.CASES if case["name"] == "cdylib-allocates")
        for plant in (None, "missing-library", "missing-program", "library-bytes", "extra", "compiled-library",
                      "compiled-program", "zero-hits", "no-stats", "repeated"):
            with self.subTest(plant=plant), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                given = runner.Input(**self.fixture_input(root))
                homes = []
                executions = []
                exports = []
                def see_home(physical):
                    if physical in homes:
                        self.assertEqual(physical, homes[-1])
                        return
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
                def command(argv, cwd, env, timeout, evidence, label):
                    see_home(Path(env["HOME"]).resolve())
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
                with patch.object(runner, "run_command", side_effect=command):
                    repeated = plant == "repeated"
                    steps = case["steps"] + ([case["steps"][-1]] if repeated else [])
                    result = runner.run_case({**case, "steps": steps}, given, root)
                self.assertEqual(result["ok"], plant in (None, "repeated"), result)
                self.assertEqual(len(homes), 5 if repeated else 3)
                self.assertEqual(len(set(homes)), len(homes))
                self.assertEqual(sum(label.endswith("-initial") for label in exports), 2)
                self.assertEqual(len(executions), len(homes) if result["ok"] else 1)
                if plant is None:
                    self.assertEqual(executions[0], executions[2])
                    self.assertNotEqual(executions[0], executions[1])


if __name__ == "__main__":
    unittest.main()
