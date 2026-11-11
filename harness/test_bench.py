"""Causal controls for native benchmark execution, retained evidence and truthful readers."""

import contextlib
import copy
import dataclasses
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent / "tools" / "typed-input" / "src"))
import bench
import benchhistory
import benchreport
import benchschema
import blaze_cli
import counters


def identity():
    return {"path": "/selected/blaze-engine", "sha256": "a" * 64, "binaries": {"/selected/blaze-engine": "a" * 64}}


def phase(label, wall=1.0):
    result = {"wall_s": wall, "samples_s": [wall], "built": "daemon", "rss_mb": 100, "compiled": 2,
              "instructions": 2_000_000, "memory": {"blaze": {"peak_mb": 150, "largest_mb": 100}}}
    if label not in ("cold", "no-op"):
        result.update(back_s=wall, link="spliced", back_link="spliced")
    return result


def result(native=True):
    measured = {"phases": {label: phase(label) for label in ("cold", "no-op", "body", "newfn")},
                "wrong": [], "tag": "1.0", "rev": "b" * 40}
    data = {"date": "2026-11-11T00:00:00Z", "commit": "c" * 40, "toolchain": "nightly-test", "runs": 1,
            "cold_runs": 1, "machine": "x86_64", "cores": 4,
            "host": {"os": "Linux", "arch": "x86_64", "name": "fixture", "cores": 4},
            "results": {"ripgrep/cranelift": measured}}
    if native:
        data.update(schema=2, compiler=identity(), compiler_after=identity())
    else:
        for value in measured["phases"].values():
            value["cargo_s"] = 25.0
            value["memory"]["cargo"] = {"peak_mb": 250, "largest_mb": 200}
    return data


class EvidenceTests(unittest.TestCase):
    def test_legacy_cargo_values_never_become_blaze_and_blaze_regressions_remain_gated(self):
        old, new = result(False), result()
        saved = copy.deepcopy(old)
        self.assertEqual(bench.compare(old, new, 25), [])
        self.assertEqual(old, saved)
        old["results"]["ripgrep/cranelift"]["phases"]["body"]["cargo_s"] = 0.0001
        self.assertEqual(bench.compare(old, new, 25), [])
        slower = copy.deepcopy(new)
        slower["results"]["ripgrep/cranelift"]["phases"]["body"]["wall_s"] = 3
        self.assertTrue(any("wall_s" in line for line in bench.compare(old, slower, 25)))

    def test_missing_evidence_failure_and_identity_drift_cannot_pass(self):
        old, valid = result(False), result()
        self.assertEqual(benchschema.errors(valid), [])
        changes = {
            "missing phase": lambda data: data["results"]["ripgrep/cranelift"]["phases"].pop("body"),
            "missing metric": lambda data: data["results"]["ripgrep/cranelift"]["phases"]["body"].pop("instructions"),
            "failed build": lambda data: data["results"]["ripgrep/cranelift"].update(failed="build refused"),
            "wrong output": lambda data: data["results"]["ripgrep/cranelift"]["wrong"].append("program changed"),
            "nonfinite metric": lambda data: data["results"]["ripgrep/cranelift"]["phases"]["body"].update(wall_s=float("nan")),
            "lost splice": lambda data: data["results"]["ripgrep/cranelift"]["phases"]["body"].update(link="full link"),
            "no producer": lambda data: data["results"]["ripgrep/cranelift"]["phases"]["body"].update(built="in-process"),
            "partial checkpoint": lambda data: data.pop("compiler_after"),
            "changed executable": lambda data: data["compiler_after"].update(sha256="d" * 64),
            "invented cargo": lambda data: data["results"]["ripgrep/cranelift"]["phases"]["body"].update(cargo_s=1),
        }
        for name, change in changes.items():
            with self.subTest(name=name):
                broken = copy.deepcopy(valid)
                change(broken)
                self.assertTrue(bench.compare(old, broken, 25))

    def test_history_keeps_native_test_times_without_inventing_cargo_and_gates_a_lost_metric(self):
        new = result()
        test = {"wall_s": 1, "samples_s": [1], "built": "daemon", "built_still": "daemon", "edit_s": 2,
                "first_s": [3, 2, 1], "edit_samples_s": [2], "same": True, "difference": None, "tests": 1}
        new["results"]["ripgrep/cranelift"]["phases"]["test"] = test
        line = benchhistory.build_line(new)
        self.assertEqual(line["compiler"], identity())
        edited = line["results"]["ripgrep/cranelift"]["phases"]["test-edit"]
        self.assertEqual(edited["wall_s"], 2)
        self.assertNotIn("cargo_s", edited)
        self.assertNotIn("cargo_mem_mb", line["results"]["ripgrep/cranelift"]["phases"]["body"])
        later = copy.deepcopy(line)
        later["results"]["ripgrep/cranelift"]["phases"]["body"].pop("instructions")
        rows, _ = benchhistory.compare([line, later], 7, 10)
        self.assertTrue(any(row["verdict"] == "regressed" and row["now"] == "not measured" for row in rows))
        self.assertFalse(benchhistory.regressions([line, copy.deepcopy(line)], 7, 10)[0])

    def test_mixed_reports_keep_each_measurements_actual_schema(self):
        native, legacy = result(), result(False)
        legacy["results"] = {"bun/cranelift": legacy["results"].pop("ripgrep/cranelift")}
        combined = benchreport.fold(native, {"bun": legacy}, "")
        all_results = {"Linux x86-64": combined}
        current = benchreport.schema_results(all_results, True)
        old = benchreport.schema_results(all_results, False)
        self.assertEqual(set(current["Linux x86-64"]["results"]), {"ripgrep/cranelift"})
        self.assertEqual(set(old["Linux x86-64"]["results"]), {"bun/cranelift"})
        rendered = "\n".join(benchreport.native_platform_doc("fixture", native, "/tmp/native.json"))
        self.assertIn("schema 2", rendered)
        self.assertNotIn("cargo", rendered.lower())
        self.assertIn("Historical", benchreport.headline(old))
        self.assertNotIn("faster", benchreport.headline(current))
        old["Linux x86-64"]["results"]["bun/cranelift"]["phases"]["body"]["cargo_s"] = 17
        self.assertEqual(benchreport.edit_speedups(old, "cranelift")[0][1]["Linux x86-64"][0], 17)

    def test_compiler_identity_detects_forwarded_engine_replacement(self):
        with tempfile.TemporaryDirectory() as directory:
            cli, engine = Path(directory) / "blaze", Path(directory) / "blaze-engine"
            cli.write_bytes(b"selected CLI")
            engine.write_bytes(b"selected engine")
            before = bench.compiler_identity(str(cli))
            self.assertEqual(set(before["binaries"]), {str(cli.resolve()), str(engine.resolve())})
            engine.write_bytes(b"different engine")
            self.assertNotEqual(before, bench.compiler_identity(str(cli)))

    def test_rendering_refuses_a_native_checkpoint_instead_of_asserting_verified_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "native.json"
            valid = result()
            path.write_text(json.dumps(valid))
            self.assertEqual(benchreport.load_stored(str(path)), valid)
            valid["compiler_after"] = None
            path.write_text(json.dumps(valid))
            with self.assertRaisesRegex(ValueError, "compiler identity"):
                benchreport.load_stored(str(path))


class InputTests(unittest.TestCase):
    def test_inputs_are_frozen_and_legacy_flags_or_compiler_setups_are_refused(self):
        run = {"blaze": "/selected/blaze-engine", "work": "/tmp/bench", "out": "/tmp/bench.json", "projects": ["cargo"]}
        given = bench.load(json.dumps(run))
        self.assertEqual(given.projects, ("cargo",))
        with self.assertRaises(dataclasses.FrozenInstanceError):
            given.runs = 8
        for extra in ({"setups": ["nightly"]}, {"reference": "/tmp/ref"}, {"runs": True},
                      {"runs": 0}, {"backends": []}, {"unknown": 1}):
            with self.subTest(extra=extra), self.assertRaises(SystemExit):
                bench.load(json.dumps(dict(run, **extra)))
        self.assertIsInstance(bench.load('{"action":"compare","baseline":"old","result":"new"}'), bench.Compare)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "input.json"
            path.write_text(json.dumps(run))
            self.assertEqual(bench.load(str(path)), given)
        with mock.patch("sys.stdin", io.StringIO(json.dumps(run))):
            self.assertEqual(bench.load("-"), given)
        self.assertIsInstance(benchreport.load('{"result":"new.json","check":true}'), benchreport.Publish)
        self.assertIsInstance(benchhistory.load('{"action":"live","files":["one.json"]}'), benchhistory.Live)
        for loader, value in ((benchreport.load, '{"action":"render","result":"ignored"}'),
                              (benchhistory.load, '{"action":"check","history":"one","last":false}')):
            with self.assertRaises(SystemExit):
                loader(value)


class NativeExecutionTests(unittest.TestCase):
    def measure(self, producer="daemon"):
        """Exercise every benchmark path with fake command outcomes, never real compilers."""
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        source, work = root / "source", root / "work"
        source.mkdir()
        work.mkdir()
        (source / "lib.rs").write_text("fn fixture() {}\n")
        spec = {"tag": "fixture", "rev": "1" * 40, "run": ["--version"], "test": [],
                "runtime": {"bin": "fixture", "args": []}, "edits": {"body": [("lib.rs", "{}", "{ let x = 1; }")]}}
        selected = str(root / "selected-blaze-engine")
        calls = []

        def command(argv, cwd=None, env=None, timeout=None, **unused):
            calls.append((list(argv), copy.deepcopy(env)))
            if argv[0] != selected:
                self.assertEqual(Path(argv[0]).name, "fixture", f"unexpected executable: {argv}")
                return 0.2, subprocess.CompletedProcess(argv, 0, b"fixture\n", b""), {"largest_mb": 3}
            self.assertEqual(argv[1], "--ignore-pin")
            action = argv[2]
            if action == "stop":
                return 0.2, subprocess.CompletedProcess(argv, 0, "", ""), {"largest_mb": 3}
            self.assertIn(action, {"build", "run", "test"})
            self.assertEqual(env["BLAZE_CACHE"], bench.storelib.store_of(env["BLAZE_HOME"]))
            target = Path(env["CARGO_TARGET_DIR"])
            first = not (target / "debug" / "fixture").exists()
            (target / "debug").mkdir(parents=True, exist_ok=True)
            (target / "debug" / "fixture").write_bytes(b"built by fake selected Blaze")
            Path(env["BLAZE_CACHE"]).mkdir(parents=True, exist_ok=True)
            work_line = {"reason": "blaze-work", "reran": 1, "green": 0, "compiled": 2, "found": 0, "link": "patched",
                         "links": [{"path": str(target / "debug" / "fixture"), "how": "first" if first else "spliced", "why": []}]}
            finished = {"reason": "build-finished", "success": True, "profile": "dev", "served": producer,
                        "took_ms": 200, "build_id": 1}
            output = "\n".join(json.dumps(line, separators=(",", ":")) for line in (work_line, finished)) + "\n"
            if action == "run":
                output += "fixture\n"
            if action == "test":
                output += "test fixture ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
            return 0.2, subprocess.CompletedProcess(argv, 0, output, ""), {"largest_mb": 3}

        class Side(blaze_cli.Side):
            def cargo(self, action, *args, env=None, timeout=7200):
                own = dict(self.env, **(env or {}))
                took, ran, _ = command(self.argv(action, *args), self.ws, own, timeout)
                blaze_cli.read_build(ran, cut=bool(blaze_cli.machine_format(action, args)))
                ran.kept = []
                return took, ran

        class Sampler:
            peak_mb, largest_mb = 4, 3
            def start(self): pass
            def stop(self): pass
            def add(self, pid): pass
            def __enter__(self): return self
            def __exit__(self, *args): pass

        class Meter:
            def settle(self): return 0
            def snapshot(self): return 0
            def since(self, *args): return counters.Counts(2_000_000, 1_000_000)
            def close(self): pass
            def adopt(self, pid): pass
            def __enter__(self): return self
            def __exit__(self, *args): pass

        class Daemon:
            sampled = 0
            def __init__(self, side): self.side = side
            def pid(self): return None
            def resident_mb(self): return 2
            def peak_mb(self): return 4

        with contextlib.ExitStack() as stack:
            for target, value in (("bench.fetch", lambda *args: str(source)), ("bench.Side", Side),
                                  ("bench.Daemon", Daemon), ("bench.memory.Sampler", Sampler),
                                  ("bench.memory.run", command), ("bench.memory.disk_mb", lambda path: 1),
                                  ("bench.counters.Meter", Meter), ("blaze_cli.BLAZE", selected)):
                stack.enter_context(mock.patch(target, value))
            stack.enter_context(mock.patch("subprocess.run", side_effect=AssertionError("unexpected ambient subprocess")))
            measured = bench.measure("fixture", spec, "cranelift", 1, str(work), cold_runs=2, tests=True)
        return measured, calls, selected

    def test_cold_edits_tests_runtime_and_warm_store_use_only_selected_blaze(self):
        measured, calls, selected = self.measure()
        self.assertEqual(measured["wrong"], [])
        self.assertEqual(set(measured["phases"]), {"cold", "no-op", "body", "test"})
        self.assertEqual(set(measured["runtime"]), {"command", "blaze"})
        self.assertNotIn("cargo", json.dumps(measured).lower())
        build_calls = [(argv, env) for argv, env in calls if argv[0] == selected and argv[2] == "build"]
        homes = {env["BLAZE_HOME"] for _, env in build_calls}
        targets = {env["CARGO_TARGET_DIR"] for _, env in build_calls}
        self.assertGreaterEqual(len(homes), 4)
        self.assertEqual(len(targets), 2)
        self.assertTrue(any(target.endswith("-b/target") for target in targets))
        self.assertTrue(any(argv[2] == "test" for argv, _ in calls if argv[0] == selected))

    def test_the_execution_control_catches_an_in_process_producer(self):
        with self.assertRaisesRegex(bench.Failed, "not by the daemon"):
            self.measure(producer="in-process")


if __name__ == "__main__":
    unittest.main()
