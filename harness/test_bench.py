"""Causal controls for the retained native/historical benchmark evidence readers.

The V1 benchmark executor has been retired. These controls cover the schema, history and
report modules that remain, and do not imply a new source compiler benchmark was run.
"""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent / "tools/typed-input/src")]
import benchhistory
import benchreport
import benchschema

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
    def test_missing_evidence_failure_and_identity_drift_cannot_pass(self):
        valid = result()
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
                if name in ("missing phase", "missing metric"):
                    history = [benchhistory.build_line(value) for value in (valid, broken)]
                    self.assertTrue(benchhistory.regressions(history, 7, 10)[0])
                else:
                    self.assertTrue(benchschema.errors(broken))


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


    def test_reader_inputs_refuse_legacy_and_malformed_options(self):
        self.assertIsInstance(benchreport.load('{"result":"new.json","check":true}'), benchreport.Publish)
        self.assertIsInstance(benchhistory.load('{"action":"live","files":["one.json"]}'), benchhistory.Live)
        for loader, value in ((benchreport.load, '{"action":"render","result":"ignored"}'),
                              (benchhistory.load, '{"action":"check","history":"one","last":false}')):
            with self.assertRaises(SystemExit):
                loader(value)


if __name__ == "__main__":
    unittest.main()
