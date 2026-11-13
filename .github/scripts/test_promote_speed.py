#!/usr/bin/env python3
"""Promotion consumes complete stored Blaze evidence; no compiler or release action runs.

The real benchmark schema, project recipes and comparator remain in every verdict.
Only external process, network and release actions are blocked.
"""
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
import urllib.request
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / ".github/scripts"))
import promote  # noqa: E402

PROJECT = "fd"
KEY = f"{PROJECT}/cranelift"


class SpeedTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        self.candidate = self.root / "candidate.json"
        self.baseline = self.root / "stable.json"
        self.output = self.root / "speed.json"
        self.summary = self.root / "summary.md"
        stack = contextlib.ExitStack()
        self.addCleanup(stack.close)
        for owner, name in ((subprocess, "run"), (subprocess, "Popen"), (os, "system"),
                            (socket, "create_connection"), (urllib.request, "urlopen")):
            stack.enter_context(patch.object(owner, name, side_effect=AssertionError(
                f"speed must not call {owner.__name__}.{name}")))
        for name in ("gh", "releases", "promote", "pointer", "yank", "issue"):
            stack.enter_context(patch.object(promote, name, side_effect=AssertionError(
                f"speed must not call release action {name}")))
        stack.enter_context(patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(self.summary)}))
        self.schema, self.benchmark = promote.benchmark_tools()
        self.write(self.candidate, self.evidence())
        self.write(self.baseline, self.evidence(stable=True))

    def evidence(self, *, stable=False, legacy=False):
        selected = str(self.root / ("stable" if stable else "canary") / "blaze")
        digest = ("a" if stable else "b") * 64
        engine = str(Path(selected).with_name("blaze-engine"))
        identity = dict(path=selected, sha256=digest, binaries={selected: digest, engine: "d" * 64})
        phases = {}
        spec = self.benchmark.PROJECTS[PROJECT]
        for label in ("cold", "no-op", *spec["edits"]):
            wall = 10.0 if label == "cold" else 0.1 if label == "no-op" else 0.2
            phase = dict(wall_s=wall, samples_s=[wall], built="daemon", rss_mb=400,
                         compiled=1000 if label == "cold" else 0 if label == "no-op" else 2,
                         memory={"blaze": {"peak_mb": 400}}, instructions=2_000_000,
                         cycles=4_000_000)
            if label not in ("cold", "no-op"):
                phase.update(back_s=0.2, link="spliced", back_link="spliced")
            phases[label] = phase
        phases["test"] = dict(wall_s=2.0, samples_s=[2.0], first_s=[3.0], edit_s=0.3,
                              edit_samples_s=[0.3], built="daemon", built_still="daemon",
                              same=True, difference=None, tests=7)
        result = dict(schema=2, compiler=identity, compiler_after=copy.deepcopy(identity),
                      date="2026-11-11T00:00:00Z", runs=1, cold_runs=1, toolchain="fixture",
                      machine="x86_64", cores=4,
                      host=dict(os="Linux", arch="x86_64", name="fixture", cores=4),
                      commit="stable-commit" if stable else "candidate-commit",
                      results={KEY: dict(phases=phases, wrong=[], tag=spec["tag"], rev=spec["rev"])})
        if legacy:
            for field in ("schema", "compiler", "compiler_after"):
                del result[field]
            result["setups"] = {"cargo": {"rustc": "historical Cargo setup"}}
            for phase in phases.values():
                phase["cargo_s"] = 100.0
                phase.setdefault("memory", {})["cargo"] = {"peak_mb": 900}
        return result

    def write(self, path, data):
        path.write_text(json.dumps(data))

    def invoke(self, *, baseline=None, project=PROJECT, threshold=25, main=False):
        given = dict(tag="v0.20261111.0", project=project, os="fixture-linux",
                     bench=str(self.candidate), out=str(self.output), threshold=threshold,
                     baseline=str(self.baseline) if baseline is None else str(baseline))
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout), self.assertRaises(SystemExit) as exited:
            if main:
                with patch.object(sys, "argv", ["promote.py", "speed", json.dumps(given)]):
                    promote.main()
            else:
                promote.speed(promote.Speed(**given))
        self.assertTrue(self.output.is_file(), "a speed verdict must leave a reviewable report")
        result = json.loads(self.output.read_text())
        return exited.exception.code, result, stdout.getvalue()

    def refused(self, **options):
        code, report, stdout = self.invoke(**options)
        self.assertEqual(code, 1, stdout)
        failed = [row for row in report["results"] if not row["ok"]]
        self.assertTrue(failed, report)
        self.assertTrue(all(row["why"] for row in failed), report)
        return "; ".join(row["why"] for row in failed)

    def test_real_policy_imports_work_without_a_script_or_harness_search_path(self):
        harness = str(ROOT / "harness")
        scripts = str(ROOT / ".github/scripts")
        with patch.object(sys, "path", [p for p in sys.path if p not in (harness, scripts)]), \
                patch.dict(sys.modules):
            sys.modules.pop("benchschema", None)
            sys.modules.pop("bench", None)
            schema, benchmark = promote.benchmark_tools()
            self.assertEqual(Path(schema.__file__).resolve(), ROOT / "harness/benchschema.py")
            self.assertEqual(Path(benchmark.__file__).resolve(), ROOT / "harness/bench.py")
            self.assertIs(benchmark.benchschema, schema)
            paths = list(sys.path)
            self.assertEqual(promote.benchmark_tools(), (schema, benchmark))
            self.assertEqual(sys.path, paths)
            self.assertEqual(benchmark.compare(self.evidence(stable=True), self.evidence(), 25), [])

    def test_complete_candidate_and_stable_use_real_policy_and_write_the_verdict(self):
        before = self.baseline.read_bytes()
        with patch.object(self.schema, "errors", wraps=self.schema.errors) as errors, \
                patch.object(self.schema, "native", wraps=self.schema.native) as native, \
                patch.object(self.benchmark, "compare", wraps=self.benchmark.compare) as compare:
            code, report, stdout = self.invoke(main=True)
        self.assertEqual(code, 0, stdout)
        self.assertTrue(all(row["ok"] for row in report["results"]), report)
        self.assertEqual(report["blaze"], "v0.20261111.0 speed, fixture-linux")
        compare.assert_called_once()
        self.assertGreaterEqual(errors.call_count, 2)
        self.assertTrue(native.called)
        self.assertIn("newfn", self.benchmark.PROJECTS[PROJECT]["edits"])
        for label in ("cold", "no-op", *self.benchmark.PROJECTS[PROJECT]["edits"], "test"):
            self.assertTrue(any(row["what"].startswith(f"{PROJECT} {label}: Blaze ")
                                for row in report["results"]), label)
        self.assertIn("against stable (25%)", stdout)
        self.assertEqual(self.summary.read_text(), stdout)
        self.assertEqual(self.baseline.read_bytes(), before)

    def test_each_declared_phase_is_required_in_both_candidate_and_stable(self):
        labels = ("cold", "no-op", *self.benchmark.PROJECTS[PROJECT]["edits"], "test")
        for side in ("candidate", "native stable", "legacy stable"):
            for label in labels:
                with self.subTest(side=side, phase=label):
                    candidate = self.evidence()
                    stable = self.evidence(stable=True, legacy=side == "legacy stable")
                    source = candidate if side == "candidate" else stable
                    del source["results"][KEY]["phases"][label]
                    self.write(self.candidate, candidate)
                    self.write(self.baseline, stable)
                    with patch.object(self.benchmark, "compare", wraps=self.benchmark.compare) as compare:
                        why = self.refused()
                    self.assertIn(label, why)
                    compare.assert_not_called()

    def test_missing_unreadable_or_wrong_project_baselines_cannot_establish_speed(self):
        for path in ("", self.root / "missing.json", self.root):
            with self.subTest(path=str(path)):
                self.assertIn("stored stable Blaze benchmark is required", self.refused(baseline=path))
        for text in ("{", "null", "[]", "{}", '{"results": []}'):
            with self.subTest(baseline=text):
                self.baseline.write_text(text)
                self.assertIn("stable Blaze evidence", self.refused())
        for key in ("fd/llvm", "ripgrep/cranelift"):
            with self.subTest(baseline_key=key):
                stable = self.evidence(stable=True)
                stable["results"][key] = stable["results"].pop(KEY)
                self.write(self.baseline, stable)
                self.assertIn(KEY, self.refused())

    def test_legacy_baseline_keeps_its_blaze_metrics_and_never_relabels_cargo(self):
        for cargo_seconds in (0.001, 100_000.0):
            with self.subTest(cargo_seconds=cargo_seconds):
                stable = self.evidence(stable=True, legacy=True)
                for phase in stable["results"][KEY]["phases"].values():
                    phase["cargo_s"] = cargo_seconds
                self.write(self.baseline, stable)
                original = self.baseline.read_bytes()
                self.write(self.candidate, self.evidence())
                code, report, stdout = self.invoke()
                self.assertEqual(code, 0, stdout)
                self.assertTrue(all(row["ok"] for row in report["results"]))
                candidate = self.evidence()
                candidate["results"][KEY]["phases"]["cold"].update(wall_s=20.0, samples_s=[20.0])
                self.write(self.candidate, candidate)
                self.assertIn("cold: wall_s", self.refused())
                self.assertEqual(self.baseline.read_bytes(), original)
        # A historical Cargo time is not a substitute for a missing Blaze time.
        stable = self.evidence(stable=True, legacy=True)
        del stable["results"][KEY]["phases"]["cold"]["wall_s"]
        self.write(self.baseline, stable)
        self.write(self.candidate, self.evidence())
        self.assertIn("wall_s", self.refused())

    def test_candidate_requires_completed_unchanged_schema_two_identity(self):
        mutations = (
            ("legacy", lambda d: d.pop("schema")),
            ("unknown schema", lambda d: d.update(schema=3)),
            ("no identity", lambda d: d.pop("compiler")),
            ("relative selection", lambda d: d["compiler"].update(path="blaze")),
            ("missing selected hash", lambda d: d["compiler"].update(binaries={})),
            ("invalid hash", lambda d: d["compiler"].update(sha256="bad")),
            ("unfinished", lambda d: d.update(compiler_after=None)),
            ("changed after run", lambda d: d["compiler_after"].update(sha256="c" * 64)),
            ("engine changed after run", lambda d: d["compiler_after"]["binaries"].update({
                str(Path(d["compiler"]["path"]).with_name("blaze-engine")): "e" * 64})),
        )
        for name, mutate in mutations:
            with self.subTest(name=name):
                result = self.evidence()
                mutate(result)
                self.write(self.candidate, result)
                with patch.object(self.benchmark, "compare", wraps=self.benchmark.compare) as compare:
                    self.refused()
                compare.assert_not_called()

    def test_wrong_failed_empty_and_malformed_measurements_are_refused(self):
        for text in ("{", "null", "[]", "true", "{}", '{"results": []}'):
            with self.subTest(candidate=text):
                self.candidate.write_text(text)
                self.refused()
        self.candidate.unlink()
        self.assertIn("cannot read benchmark", self.refused())
        for key in ("fd/llvm", "ripgrep/cranelift"):
            with self.subTest(candidate_key=key):
                result = self.evidence()
                result["results"][key] = result["results"].pop(KEY)
                self.write(self.candidate, result)
                self.assertIn(KEY, self.refused())
        for field, value in (("wrong", ["program output changed"]), ("wrong", "not findings"),
                             ("failed", "compiler refused"), ("phases", {}), ("phases", [])):
            for side in ("candidate", "stable"):
                with self.subTest(side=side, field=field, value=value):
                    self.write(self.candidate, self.evidence())
                    self.write(self.baseline, self.evidence(stable=True))
                    result = self.evidence(stable=side == "stable")
                    result["results"][KEY][field] = value
                    self.write(self.candidate if side == "candidate" else self.baseline, result)
                    self.refused()

    def test_invalid_native_phase_evidence_is_not_a_timing(self):
        mutations = (
            ("cold", "wall_s", None), ("cold", "wall_s", -1),
            ("cold", "wall_s", True), ("cold", "wall_s", float("nan")),
            ("cold", "wall_s", float("inf")), ("cold", "samples_s", []),
            ("cold", "built", "cargo"), ("cold", "rss_mb", None),
            ("cold", "compiled", None), ("cold", "memory", {}),
            ("cold", "cargo_s", 0.1), ("body", "back_s", None),
            ("body", "link", "full link"), ("body", "back_link", None),
            ("test", "same", False), ("test", "difference", "output changed"),
            ("test", "tests", 0), ("test", "first_s", []),
            ("test", "edit_samples_s", []), ("test", "edit_s", None),
        )
        for label, field, value in mutations:
            with self.subTest(phase=label, field=field, value=value):
                result = self.evidence()
                result["results"][KEY]["phases"][label][field] = value
                self.write(self.candidate, result)
                self.refused()
        for field, value in (("same", False), ("difference", "historical test failure")):
            with self.subTest(legacy_test=field):
                self.write(self.candidate, self.evidence())
                stable = self.evidence(stable=True, legacy=True)
                stable["results"][KEY]["phases"]["test"][field] = value
                self.write(self.baseline, stable)
                self.refused()

    def test_runtime_shape_and_stored_blaze_regressions_use_the_shared_policy(self):
        runtime = dict(command=["fixture-app"], blaze=dict(
            median_s=1.0, min_s=1.0, max_s=1.0, runs=1, peak_mb=40, peak_max_mb=40, size_mb=2))
        for side in ("candidate", "native stable", "legacy stable"):
            for malformed in ("bad", [], {"blaze": "bad"}, {"blaze": {"median_s": "bad"}}):
                with self.subTest(side=side, runtime=malformed):
                    candidate = self.evidence()
                    stable = self.evidence(stable=True, legacy=side == "legacy stable")
                    source = candidate if side == "candidate" else stable
                    source["results"][KEY]["runtime"] = malformed
                    self.write(self.candidate, candidate)
                    self.write(self.baseline, stable)
                    self.assertIn("runtime", self.refused())
        stable = self.evidence(stable=True, legacy=True)
        stable["results"][KEY]["runtime"] = copy.deepcopy(runtime)
        # The historical Cargo runtime stays a separate measurement.
        stable["results"][KEY]["runtime"]["cargo"] = {"median_s": 1000.0}
        self.write(self.baseline, stable)
        candidate = self.evidence()
        candidate["results"][KEY]["runtime"] = copy.deepcopy(runtime)
        self.write(self.candidate, candidate)
        code, _, stdout = self.invoke()
        self.assertEqual(code, 0, stdout)
        candidate["results"][KEY]["runtime"]["blaze"].update(median_s=2.0, min_s=2.0, max_s=2.0)
        self.write(self.candidate, candidate)
        self.assertIn("Blaze runtime", self.refused())
        del candidate["results"][KEY]["runtime"]
        self.write(self.candidate, candidate)
        self.assertIn("Blaze runtime", self.refused())

    def test_real_comparator_retains_metric_and_splice_regressions(self):
        changes = (("cold", "wall_s", 20.0), ("cold", "rss_mb", 1000),
                   ("cold", "compiled", 2000), ("body", "back_s", 1.0),
                   ("body", "instructions", 5_000_000), ("body", "instructions", None),
                   ("body", "link", "first link"), ("body", "back_link", "first link"),
                   ("test", "edit_s", 1.0), ("cold", "memory", {"blaze": {"peak_mb": 1000}}))
        for label, field, value in changes:
            with self.subTest(phase=label, field=field):
                result = self.evidence()
                phase = result["results"][KEY]["phases"][label]
                phase[field] = value
                if field == "wall_s":
                    phase["samples_s"] = [value]
                if field == "edit_s":
                    phase["edit_samples_s"] = [value]
                self.write(self.candidate, result)
                self.assertIn("worse than stable (stable-commit)", self.refused())

    def test_real_threshold_and_noise_floor_boundaries_are_preserved(self):
        for label, wall, wanted in (("cold", 12.5, 0), ("cold", 12.6, 1),
                                    ("no-op", 0.14, 0), ("no-op", 0.16, 1)):
            with self.subTest(phase=label, wall=wall):
                result = self.evidence()
                result["results"][KEY]["phases"][label].update(wall_s=wall, samples_s=[wall])
                self.write(self.candidate, result)
                code, _, stdout = self.invoke()
                self.assertEqual(code, wanted, stdout)
        self.write(self.candidate, self.evidence())
        for threshold in (-1, True, 25.5, "25"):
            with self.subTest(threshold=threshold):
                self.assertIn("threshold", self.refused(threshold=threshold))
        self.assertIn("no benchmark project", self.refused(project="not-a-project"))


if __name__ == "__main__":
    unittest.main()
