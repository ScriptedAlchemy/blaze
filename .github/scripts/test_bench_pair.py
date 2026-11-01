#!/usr/bin/env python3
"""Reject mismatched benchmark provenance before a comparison can pass, and tell an identical pair
from one to judge."""
import contextlib
import copy
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

ROOT = Path(__file__).resolve().parents[2]
FLAGS = ["-C", "llvm-args=-align-all-functions=6"]
PACKAGES = {"store": "ra-jit-bench-store", "codegen": "ra-jit-bench-codegen"}


def sets_spec():
    spec = {"serve-cli": {"platforms": ["linux-x64"], "test": ["-p", "ra-jit"]}}
    for bench, package in PACKAGES.items():
        for side in ("head", "base"):
            entry = {"platforms": ["linux-x64"], "bench": ["-p", package, "--bench", bench],
                     "rustflags": FLAGS, "remap-paths": True, "source": side}
            if side == "base":
                entry["requires"] = {"package": package, "bench": bench}
            spec[f"bench-{bench}-{side}"] = entry
    return spec


class PairTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bench-pair-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.out = self.root / "out"
        self.spec = sets_spec()
        self.bundle = {"roots": {"workspace": str(self.root)}, "files": {}, "sets": {}}
        for bench in PACKAGES:
            for side in ("head", "base"):
                # By default every pair differs: each binary has its own bytes.
                self.binary(bench, side, f"#!/bin/sh\n# {bench} {side}\nexit 0\n")

    def binary(self, bench, side, text):
        name = f"bench-{bench}-{side}"
        path = self.root / name
        path.write_text(text)
        path.chmod(0o755)
        self.bundle["files"]["workspace/" + name] = {
            "executable": True, "size": path.stat().st_size,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        }
        entry = {key: copy.deepcopy(value) for key, value in self.spec[name].items()
                 if key in ("rustflags", "remap-paths", "requires", "source")}
        entry.update(kind="bench", args=list(self.spec[name]["bench"]),
                     executables=[{"name": bench, "kind": ["bench"], "path": str(path)}])
        if side == "base":
            entry["identity"] = {"commit": "base-sha", "tree": "tree-base"}
        self.bundle["sets"][name] = entry

    def absent(self, bench, reason=None):
        base = self.bundle["sets"][f"bench-{bench}-base"]
        package = PACKAGES[bench]
        base.update(absent=True,
                    reason=reason or f"the base has no package {package} with bench target {bench}")
        del base["executables"]

    def prepare(self, base="base-sha"):
        with contextlib.redirect_stdout(io.StringIO()):
            return {row["bench"]: row for row in bench_pair.prepare(self.bundle, self.spec, base, self.out)}

    def test_differing_pair_is_judged(self):
        rows = self.prepare()
        self.assertEqual({bench: row["state"] for bench, row in rows.items()},
                         {"store": "differs", "codegen": "differs"})
        self.assertEqual((self.out / "bench/store-base").read_bytes(), (self.root / "bench-store-base").read_bytes())
        self.assertEqual((self.out / "bench/codegen-head").read_bytes(), (self.root / "bench-codegen-head").read_bytes())
        self.assertEqual((self.out / "bench/pairs.txt").read_text(), "codegen differs\nstore differs\n")
        self.assertEqual(json.loads((self.out / "tango/bundle.json").read_text()), self.bundle)
        pairs = json.loads((self.out / "tango/pairs.json").read_text())
        self.assertEqual([row["base"] for row in pairs],
                         [self.bundle["files"][f"workspace/bench-{b}-base"]["sha256"] for b in ("codegen", "store")])

    def test_identical_pair_is_not_judged(self):
        # The store left alone by a change to the code generator: its two builds are one binary.
        self.binary("store", "base", "#!/bin/sh\n# store\nexit 0\n")
        self.binary("store", "head", "#!/bin/sh\n# store\nexit 0\n")
        rows = self.prepare()
        self.assertEqual((rows["store"]["state"], rows["codegen"]["state"]), ("identical", "differs"))
        self.assertEqual(rows["store"]["head"], rows["store"]["base"])
        self.assertIn("store identical\n", (self.out / "bench/pairs.txt").read_text())

    def test_base_without_the_package_runs_the_head_alone(self):
        self.out.mkdir()
        (self.out / "bench").mkdir()
        (self.out / "bench/store-base").write_text("stale prior pair")
        self.absent("store")
        rows = self.prepare()
        self.assertEqual((rows["store"]["state"], rows["codegen"]["state"]), ("head-only", "differs"))
        self.assertFalse((self.out / "bench/store-base").exists())
        self.assertTrue((self.out / "bench/store-head").exists())

    def test_base_with_the_old_layout_runs_both_heads_alone(self):
        # A base before the split has crates/ra-jit-bench, and so neither package: the producer
        # records both bases absent (test_bundle_build.py), the first run after the split.
        for bench in PACKAGES:
            self.absent(bench)
        rows = self.prepare()
        self.assertEqual({row["state"] for row in rows.values()}, {"head-only"})
        self.assertEqual(sorted(p.name for p in (self.out / "bench").iterdir()),
                         ["codegen-head", "pairs.txt", "store-head"])

    def test_absence_for_another_reason_rejected(self):
        self.absent("store", reason="crates/ra-jit-bench does not exist in base")
        with self.assertRaises(AssertionError):
            self.prepare()

    def test_absence_with_executable_rejected(self):
        self.absent("store")
        self.bundle["sets"]["bench-store-base"]["executables"] = [{"name": "store", "kind": ["bench"], "path": "x"}]
        with self.assertRaisesRegex(AssertionError, "absent benchmark"):
            self.prepare()

    def test_corruption_rejected(self):
        (self.root / "bench-store-base").write_text("corrupted")
        with self.assertRaises(AssertionError):
            self.prepare()

    def test_wrong_base_rejected(self):
        with self.assertRaisesRegex(AssertionError, "not the PR base"):
            self.prepare("another-commit")

    def test_different_profile_rejected(self):
        self.bundle["sets"]["bench-codegen-base"]["args"].extend(["--profile", "dev"])
        with self.assertRaisesRegex(AssertionError, "different flags"):
            self.prepare()

    def test_different_rustflags_rejected(self):
        for side in ("base", "head"):
            with self.subTest(side=side):
                self.setUp()
                self.bundle["sets"][f"bench-store-{side}"]["rustflags"] = []
                with self.assertRaisesRegex(AssertionError, "other rustflags|different flags"):
                    self.prepare()

    def test_head_built_otherwise_than_test_sets_rejected(self):
        self.bundle["sets"]["bench-store-head"]["args"] = ["-p", "ra-jit-bench", "--bench", "store"]
        self.bundle["sets"]["bench-store-base"]["args"] = ["-p", "ra-jit-bench", "--bench", "store"]
        with self.assertRaisesRegex(AssertionError, "configuration changed"):
            self.prepare()

    def test_missing_base_is_not_solo(self):
        del self.bundle["sets"]["bench-codegen-base"]
        with self.assertRaisesRegex(AssertionError, "omitted"):
            self.prepare()

    def test_missing_head_rejected(self):
        del self.bundle["sets"]["bench-codegen-head"]
        with self.assertRaisesRegex(AssertionError, "omitted"):
            self.prepare()

    def test_non_pr_solo(self):
        for bench in PACKAGES:
            self.bundle["sets"][f"bench-{bench}-base"] = {
                "kind": "bench", "source": "base", "absent": True, "reason": bench_pair.NO_BASE}
        rows = self.prepare("")
        self.assertEqual({row["state"] for row in rows.values()}, {"head-only"})
        del self.bundle["sets"]["bench-store-base"]
        self.assertEqual(self.prepare("")["store"]["state"], "head-only")

    def test_non_pr_with_a_base_rejected(self):
        with self.assertRaisesRegex(AssertionError, "non-PR"):
            self.prepare("")

    def test_multiple_benchmarks_rejected(self):
        self.bundle["sets"]["bench-store-head"]["executables"] *= 2
        with self.assertRaisesRegex(AssertionError, "exactly one"):
            self.prepare()

    def test_the_checkouts_test_sets_are_paired(self):
        spec = json.loads((ROOT / ".github/ci/test-sets.json").read_text())
        self.assertEqual(bench_pair.pairs(spec), {
            "store": ("bench-store-head", "bench-store-base"),
            "codegen": ("bench-codegen-head", "bench-codegen-base")})


if __name__ == "__main__":
    unittest.main()
