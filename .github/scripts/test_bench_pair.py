#!/usr/bin/env python3
"""Reject mismatched benchmark provenance before a comparison can pass."""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("bench_pair", Path(__file__).with_name("bench_pair.py"))
bench_pair = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench_pair)


class PairTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bench-pair-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.out = self.root / "out"
        self.bundle = {"roots": {"workspace": str(self.root)}, "files": {}, "sets": {}}
        for name in ("bench-head", "bench-base"):
            path = self.root / name
            path.write_text("#!/bin/sh\n# " + name + "\nexit 0\n")
            path.chmod(0o755)
            relative = "workspace/" + name
            self.bundle["files"][relative] = {
                "executable": True, "size": path.stat().st_size,
                "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            }
            self.bundle["sets"][name] = {
                "rustflags": ["-C", "llvm-args=-align-all-functions=6", "-C", "llvm-args=-align-all-nofallthru-blocks=5"],
                "kind": "bench", "args": ["-p", "ra-jit-bench", "--bench", "store"],
                "executables": [{"name": "store", "kind": ["bench"], "path": str(path)}],
                "source": name.removeprefix("bench-"), "identity": {"commit": name, "tree": "tree-" + name},
            }

    def prepare(self, base="bench-base"):
        with contextlib.redirect_stdout(io.StringIO()):
            bench_pair.prepare(self.bundle, base, self.out)

    def test_valid_pair_and_evidence(self):
        self.prepare()
        self.assertEqual((self.out / "bench-base").read_bytes(), (self.root / "bench-base").read_bytes())
        self.assertEqual(json.loads((self.out / "tango/bundle.json").read_text()), self.bundle)

    def test_corruption_rejected(self):
        (self.root / "bench-base").write_text("corrupted")
        with self.assertRaises(AssertionError):
            self.prepare()

    def test_wrong_base_rejected(self):
        with self.assertRaisesRegex(AssertionError, "not the PR base"):
            self.prepare("another-commit")

    def test_different_profile_rejected(self):
        self.bundle["sets"]["bench-base"]["args"].extend(["--profile", "dev"])
        with self.assertRaisesRegex(AssertionError, "different flags"):
            self.prepare()

    def test_effective_rustflags_must_match(self):
        self.bundle["sets"]["bench-base"]["rustflags"].append("--cfg=other")
        with self.assertRaisesRegex(AssertionError, "different Rust flags"):
            self.prepare()

    def test_missing_rustflags_are_not_provenance(self):
        del self.bundle["sets"]["bench-head"]["rustflags"]
        with self.assertRaisesRegex(AssertionError, "Rust flags"):
            self.prepare()

    def test_invalid_rustflags_are_rejected(self):
        self.bundle["sets"]["bench-head"]["rustflags"] = "-C different"
        with self.assertRaisesRegex(AssertionError, "Rust flags"):
            self.prepare()

    def test_missing_base_is_not_solo(self):
        del self.bundle["sets"]["bench-base"]
        with self.assertRaisesRegex(AssertionError, "omitted"):
            self.prepare()

    def test_only_explicit_absence_permits_solo(self):
        self.out.mkdir()
        (self.out / "bench-base").write_text("stale prior pair")
        base = self.bundle["sets"]["bench-base"]
        base.update(absent=True, reason="crates/ra-jit-bench does not exist in base", executables=[])
        self.prepare()
        self.assertFalse((self.out / "bench-base").exists())

    def test_absence_with_executable_rejected(self):
        self.bundle["sets"]["bench-base"].update(absent=True, reason="crates/ra-jit-bench does not exist in base")
        with self.assertRaisesRegex(AssertionError, "absent benchmark"):
            self.prepare()

    def test_non_pr_solo(self):
        del self.bundle["sets"]["bench-base"]
        self.prepare("")
        self.assertTrue((self.out / "bench-head").exists())

    def test_multiple_benchmarks_rejected(self):
        self.bundle["sets"]["bench-head"]["executables"] *= 2
        with self.assertRaisesRegex(AssertionError, "exactly one"):
            self.prepare()


if __name__ == "__main__":
    unittest.main()
