#!/usr/bin/env python3
"""Exercise isolated metadata refresh and its fail-closed artifact contract."""
from pathlib import Path
import hashlib
import json
import subprocess
import tempfile
import tomllib
import unittest

import refresh_nix_graphs as refresh


class RefreshTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        self.out = Path(self.temp.name) / "artifact"
        self.git("init", "-q")
        for directory in (self.root, self.root / "demo/life"):
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="0.1.0"\n')
            (directory / "Cargo.lock").write_text('version=4\n[[package]]\nname="fixture"\nversion="0.1.0"\n')
        (self.root / "untouched").write_text("original\n")
        (self.root / "flake.nix").write_text("{}\n")
        (self.root / "flake.lock").write_text("{}\n")
        (self.root / "harness").mkdir()
        (self.root / "harness/toolchain").write_text("nightly-2026-10-27\n")
        self.graphs(self.root)
        self.git("add", ".")
        self.git("-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")
        self.commands = []

    def git(self, *args):
        return refresh.run(["git", *args], self.root)

    def graphs(self, work):
        for project, directory in (("daemon", work), ("life", work / "demo/life")):
            graph = {
                "version": 4,
                "manifest": tomllib.loads((directory / "Cargo.toml").read_text()),
                "lock": tomllib.loads((directory / "Cargo.lock").read_text()),
                "packages": {"fixture@0.1.0": {}},
                "systems": {name: {"units": {"unit": {}}, "roots": ["unit"]} for name in refresh.SYSTEMS},
            }
            target = work / f"tools/nix/units/{project}.json"
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(json.dumps(graph) + "\n")

    def metadata(self, command, cwd):
        self.commands.append((command, cwd))
        if command[0] == "git":
            return refresh.run(command, cwd)
        self.assertEqual(command[:4], ["nix", "develop", str(self.root), "--command"])
        self.assertNotEqual(cwd, self.root)
        if command[4] == "cargo":
            self.assertEqual(command[4:], ["cargo", "metadata", "--format-version", "1", "--manifest-path", "demo/life/Cargo.toml"])
            (cwd / "demo/life/Cargo.lock").write_text('version=4\n[[package]]\nname="fixture"\nversion="0.1.1"\n')
        else:
            self.assertEqual(command[4:], ["python3", "tools/nix/unit_graph.py"])
            self.graphs(cwd)
        return b""

    def test_export_is_bound_to_committed_source_and_leaves_checkout_untouched(self):
        # The ordinary graph check may have already changed a generated file before failing.
        original = self.root / "tools/nix/units/daemon.json"
        original.write_text("dirty output from failed gate\n")
        before = self.git("diff", "--binary")
        refresh.refresh(self.root, refresh.Request(self.out), self.metadata)
        # The real CI checkout may be shallow; the detached worktree must
        # never become the Nix flake input that asks Git for revCount.
        nix = [(cmd, cwd) for cmd, cwd in self.commands if cmd[0] == "nix"]
        self.assertEqual([cmd[4] for cmd, _ in nix], ["cargo", "python3"])
        self.assertTrue(all(cmd[2] == str(self.root) and cwd != self.root for cmd, cwd in nix))
        receipt = json.loads((self.out / "source.json").read_text())
        self.assertEqual(receipt["source_commit"], self.git("rev-parse", "HEAD").decode().strip())
        self.assertEqual(receipt["source_tree"], self.git("rev-parse", "HEAD^{tree}").decode().strip())
        self.assertEqual(self.git("diff", "--binary"), before)
        self.assertEqual(self.git("worktree", "list", "--porcelain").count(b"worktree "), 1)
        for name, digest in receipt["files"].items():
            self.assertEqual(hashlib.sha256((self.out / name).read_bytes()).hexdigest(), digest)
        self.assertIn(b"demo/life/Cargo.lock", (self.out / "refresh.patch").read_bytes())
        self.assertNotIn("dirty output", (self.out / "tools/nix/units/daemon.json").read_text())
        refresh.checked_graphs(self.out_with_manifests())

    def out_with_manifests(self):
        for name in ("Cargo.toml", "Cargo.lock", "demo/life/Cargo.toml"):
            (self.out / name).write_bytes((self.root / name).read_bytes())
        return self.out

    def test_bad_regeneration_never_gets_an_export_receipt(self):
        for fault in ("unrelated", "mismatched", "missing-platform", "process"):
            with self.subTest(fault=fault):
                def faulty(command, cwd):
                    if command[0] == "git" or (command[0] == "nix" and command[4] == "cargo"):
                        return self.metadata(command, cwd)
                    if fault == "process":
                        raise subprocess.CalledProcessError(1, command)
                    self.metadata(command, cwd)
                    if fault == "unrelated":
                        (cwd / "untouched").write_text("bad change\n")
                    else:
                        path = cwd / "tools/nix/units/life.json"
                        graph = json.loads(path.read_text())
                        if fault == "mismatched":
                            graph["lock"] = {}
                        else:
                            graph["systems"].pop("x86_64-darwin")
                        path.write_text(json.dumps(graph))
                    return b""
                with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                    refresh.refresh(self.root, refresh.Request(self.out), faulty)
                self.assertFalse(self.out.exists())
                self.assertEqual(self.git("worktree", "list", "--porcelain").count(b"worktree "), 1)
                self.assertEqual(self.git("diff"), b"")

    def test_changed_shell_pin_refuses_metadata_export(self):
        (self.root / "harness/toolchain").write_text("nightly-2026-10-28\n")
        with self.assertRaisesRegex(ValueError, "harness/toolchain: checkout differs"):
            refresh.refresh(self.root, refresh.Request(self.out), self.metadata)
        self.assertFalse(self.out.exists())
        self.assertEqual(self.git("worktree", "list", "--porcelain").count(b"worktree "), 1)

    def test_request_and_destination_are_strict(self):
        for text in ('{}', '{"out":"relative"}', '{"out":null}', '{"out":"/tmp/a","skip_locked":true}', '[]'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                refresh.Request.parse(text)
        self.assertEqual(refresh.Request.parse(json.dumps({"out": str(self.out)})).out, self.out)
        for out in (self.root, self.root / "output", Path(self.temp.name)):
            with self.subTest(out=out), self.assertRaises(ValueError):
                refresh.refresh(self.root, refresh.Request(out), self.metadata)
        self.assertEqual(self.commands, [])


if __name__ == "__main__":
    unittest.main()
