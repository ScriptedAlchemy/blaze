#!/usr/bin/env python3
"""`bundle.py build` for the benchmark sets, with cargo and git stood in for: which base sets are
absent and why, and the flags each side is built with. Also the contract of the benchmark sets
in .github/ci/test-sets.json, which bench_pair.py pairs."""
import contextlib
import io
import json
import os
from pathlib import Path
import re
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bundle

ROOT = Path(__file__).resolve().parents[2]
ALIGN = ["-C", "llvm-args=-align-all-functions=6"]
# What `cargo metadata` lists for each layout of the benchmarks.
SPLIT = {("ra-jit-bench-store", "bench", "store"), ("ra-jit-bench-codegen", "bench", "codegen"),
         ("ra-jit-store", "lib", "ra_jit_store")}
OLD = {("ra-jit-bench", "bench", "store"), ("ra-jit-bench", "lib", "ra_jit_bench"),
       ("ra-jit-store", "lib", "ra_jit_store")}


def bench_set(source, package="ra-jit-bench-store", bench="store"):
    spec = {"platforms": ["linux-x64"], "bench": ["-p", package, "--bench", bench],
            "rustflags": ALIGN, "remap-paths": True, "source": source}
    if source == "base":
        spec["requires"] = {"package": package, "bench": bench}
    return spec


class BuildTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="bundle-build-test-")
        self.addCleanup(self.tmp.cleanup)
        root = Path(os.path.realpath(self.tmp.name))
        self.base = {"workspace": str(root / "workspace"), "temp": str(root / "temp")}
        for path in self.base.values():
            Path(path).mkdir()
        (root / "temp/base-tree").mkdir()
        self.sets = root / "sets.json"
        self.out = root / "out.json"
        self.calls = []
        self.layouts = {self.base["workspace"]: SPLIT, self.base["temp"] + "/base-tree": SPLIT}

    def cargo(self, command, cwd, env):
        """Cargo, as far as `build` reads it: one executable per bench set, in its target dir."""
        self.calls.append((command, cwd, env))
        target = Path(cwd) / env.get("CARGO_TARGET_DIR", "target")
        bench = command[command.index("--bench") + 1]
        exe = target / "release/deps" / f"{bench}-0123456789abcdef"
        exe.parent.mkdir(parents=True, exist_ok=True)
        exe.write_bytes(b"bench")
        return [{"reason": "compiler-artifact", "executable": str(exe), "filenames": [str(exe)],
                 "target": {"name": bench, "kind": ["bench"]}}]

    def build(self, sets, base_sha="b" * 40, environ=None):
        self.sets.write_text(json.dumps(sets))
        env = {k: v for k, v in os.environ.items() if k not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")}
        env.update(environ or {}, CARGO_TARGET_DIR=self.base["workspace"] + "/target")
        with mock.patch.object(bundle, "roots", return_value=self.base), \
             mock.patch.object(bundle, "host_triple", return_value="x86_64-unknown-linux-gnu"), \
             mock.patch.object(bundle, "source_identity", return_value={"commit": base_sha, "tree": "t"}), \
             mock.patch.object(bundle, "git", side_effect=AssertionError("worktree exists")), \
             mock.patch.object(bundle, "workspace_targets", side_effect=lambda cwd: self.layouts[cwd]), \
             mock.patch.object(bundle, "cargo_json", side_effect=self.cargo), \
             mock.patch.dict(os.environ, env, clear=True), \
             contextlib.redirect_stdout(io.StringIO()):
            bundle.build(SimpleNamespace(sets=str(self.sets), platform="linux-x64",
                                         base_sha=base_sha, out=str(self.out)))
        return json.loads(self.out.read_text())

    def flags(self, call):
        return call[2]["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")

    def test_head_and_base_differ_only_in_the_remapped_prefixes(self):
        out = self.build({"head": bench_set("head"), "base": bench_set("base")})
        self.assertFalse(out["base"].get("absent"))
        self.assertEqual(out["base"]["requires"], {"package": "ra-jit-bench-store", "bench": "store"})
        self.assertEqual((out["head"]["rustflags"], out["head"]["remap-paths"]), (ALIGN, True))
        (_, head_cwd, _), (_, base_cwd, base_env) = self.calls
        self.assertEqual((head_cwd, base_cwd), (self.base["workspace"], self.base["temp"] + "/base-tree"))
        self.assertEqual(base_env["CARGO_TARGET_DIR"], self.base["workspace"] + "/target/bundle-base")
        head, base = (self.flags(call) for call in self.calls)
        self.assertEqual(head[:len(ALIGN)], ALIGN)
        self.assertEqual(head, ALIGN + [f"--remap-path-prefix={self.base['workspace']}=/source",
                                        f"--remap-path-prefix={self.base['workspace']}/target=/target"])
        self.assertEqual(base, ALIGN + [f"--remap-path-prefix={base_cwd}=/source",
                                        f"--remap-path-prefix={self.base['workspace']}/target/bundle-base=/target"])
        strip = lambda flags: [re.sub(r"=/.*=", "=", flag) for flag in flags]
        self.assertEqual(strip(head), strip(base))
        self.assertNotEqual(out["head"]["executables"][0]["path"], out["base"]["executables"][0]["path"])

    def test_base_without_the_package_is_absent(self):
        self.layouts[self.base["temp"] + "/base-tree"] = {("ra-jit-store", "lib", "ra_jit_store")}
        out = self.build({"head": bench_set("head"), "base": bench_set("base")})
        self.assertEqual(out["base"]["reason"], "the base has no package ra-jit-bench-store with bench target store")
        self.assertTrue(out["base"]["absent"])
        self.assertNotIn("executables", out["base"])
        self.assertEqual(len(self.calls), 1, "the absent base was built")

    def test_base_with_the_old_single_package_is_absent(self):
        # A base before the split: crates/ra-jit-bench has a bench `store` of the same name.
        self.layouts[self.base["temp"] + "/base-tree"] = OLD
        out = self.build({"head": bench_set("head"), "base": bench_set("base"),
                          "cg-head": bench_set("head", "ra-jit-bench-codegen", "codegen"),
                          "cg-base": bench_set("base", "ra-jit-bench-codegen", "codegen")})
        self.assertEqual(out["base"]["reason"], "the base has no package ra-jit-bench-store with bench target store")
        self.assertEqual(out["cg-base"]["reason"],
                         "the base has no package ra-jit-bench-codegen with bench target codegen")
        self.assertEqual([call[1] for call in self.calls], [self.base["workspace"]] * 2)

    def test_no_base_commit(self):
        out = self.build({"head": bench_set("head"), "base": bench_set("base")}, base_sha="")
        self.assertEqual((out["base"]["absent"], out["base"]["reason"]),
                         (True, "no base commit (not a pull request)"))
        self.assertEqual(out["base"]["requires"], {"package": "ra-jit-bench-store", "bench": "store"})

    def test_rustflags_of_the_job_are_refused(self):
        for var in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
            with self.subTest(var=var), self.assertRaises(SystemExit):
                self.build({"head": bench_set("head")}, environ={var: "-Copt-level=0"})

    def test_a_malformed_requirement_is_refused(self):
        for requires in ({"bench": "store"}, {"package": "p", "bench": "a", "bin": "b"}, {"package": "p", "dir": "x"}):
            with self.subTest(requires=requires), self.assertRaises(SystemExit), \
                 contextlib.redirect_stdout(io.StringIO()):
                bundle.missing(requires, "base", self.base["workspace"])


class SetsContractTest(unittest.TestCase):
    """bench_pair.py pairs `bench-<name>-head` with `bench-<name>-base`: the two must be one build."""

    def test_every_benchmark_is_a_pair_built_alike(self):
        sets = json.loads((ROOT / ".github/ci/test-sets.json").read_text())
        heads = {m.group(1) for name in sets if (m := re.fullmatch(r"bench-(.+)-head", name))}
        self.assertEqual(heads, {"store", "codegen"})
        for bench in heads:
            head, base = sets[f"bench-{bench}-head"], sets[f"bench-{bench}-base"]
            with self.subTest(bench=bench):
                package = head["bench"][head["bench"].index("-p") + 1]
                self.assertEqual(head["bench"], ["-p", package, "--bench", bench])
                for field in ("platforms", "bench", "rustflags", "remap-paths"):
                    self.assertEqual(head[field], base[field], field)
                self.assertTrue(head["remap-paths"])
                self.assertEqual((head["source"], base["source"]), ("head", "base"))
                self.assertNotIn("requires", head)
                self.assertEqual(base["requires"], {"package": package, "bench": bench})
                self.assertIn("llvm-args=-align-all-functions=6", head["rustflags"])
        self.assertFalse([name for name in sets if name.startswith("bench-")
                          and not re.fullmatch(r"bench-(.+)-(head|base)", name)])


if __name__ == "__main__":
    unittest.main()
