"""Security and correctness checks for shared CI bundle consumers."""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bundle


class BundleIntegrityTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.workspace = self.root / "workspace"
        self.temp = self.root / "temp"
        self.workspace.mkdir()
        self.temp.mkdir()
        self.base = {"workspace": str(self.workspace), "temp": str(self.temp)}
        self.binary = self.workspace / "target/debug/app"
        self.binary.parent.mkdir(parents=True)
        self.binary.write_bytes(b"original")
        self.manifest = {
            "schema": 1, "platform": "linux-x64",
            "roots": self.base,
            "source": {"tree": "tree", "flake": "flake", "cargo_lock": "lock", "commit": "sha"},
            "files": {}, "sets": {},
        }

    def make_archive(self, name, payload, advertised=None):
        bundle_dir = self.temp / "incoming"
        bundle_dir.mkdir(exist_ok=True)
        key = "workspace/target/debug/app"
        data = payload if advertised is None else advertised
        self.manifest["files"] = {key: {"sha256": hashlib.sha256(data).hexdigest(),
                                        "size": len(data), "executable": True}}
        text = (json.dumps(self.manifest) + "\n").encode()
        (bundle_dir / "bundle.json").write_bytes(text)
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw, mode="w") as tar:
            item = tarfile.TarInfo(name)
            item.size = len(payload)
            tar.addfile(item, io.BytesIO(payload))
        subprocess.run(["zstd", "-q", "-f", "-o", str(bundle_dir / "bundle.tar.zst")],
                       input=raw.getvalue(), check=True)
        return SimpleNamespace(platform="linux-x64", sha256=hashlib.sha256(text).hexdigest(),
                               dir=str(bundle_dir), github_env="")

    def unpack(self, args):
        with mock.patch.object(bundle, "roots", return_value=self.base), \
             mock.patch.object(bundle, "source_identity", return_value=self.manifest["source"]):
            bundle.unpack(args)

    def test_checkout_source_and_other_runner_files_are_not_bundle_artifacts(self):
        self.assertFalse(bundle.allowed_bundle_key("workspace/.github/workflows/pr.yaml"))
        self.assertFalse(bundle.allowed_bundle_key("temp/credentials.json"))
        self.assertTrue(bundle.allowed_bundle_key("workspace/target/debug/app"))
        self.assertTrue(bundle.allowed_bundle_key("temp/owned-runtime/blaze-proc-macro-srv"))

    def test_escape_through_existing_parent_symlink_is_refused(self):
        outside = self.root / "outside"
        outside.mkdir()
        (self.workspace / "target/escape").symlink_to(outside, target_is_directory=True)
        with self.assertRaises(SystemExit):
            bundle.from_key("workspace/target/escape/file", self.base)
        self.assertFalse((outside / "file").exists())

    def test_symlink_inside_workspace_cannot_overwrite_checkout_sources(self):
        source = self.workspace / "src"
        source.mkdir()
        (source / "lib.rs").write_text("source")
        (self.workspace / "target/src-link").symlink_to(source, target_is_directory=True)
        with self.assertRaises(SystemExit):
            bundle.artifact_path("workspace/target/src-link/lib.rs", self.base)
        self.assertEqual((source / "lib.rs").read_text(), "source")

    def test_corrupt_archive_never_truncates_preexisting_hardlink(self):
        linked = self.root / "hardlinked"
        os.link(self.binary, linked)
        args = self.make_archive("workspace/target/debug/app", b"bad", advertised=b"good")
        with self.assertRaises(SystemExit):
            self.unpack(args)
        self.assertEqual(self.binary.read_bytes(), b"original")
        self.assertEqual(linked.read_bytes(), b"original")

    def test_oversized_member_never_replaces_existing_output(self):
        args = self.make_archive("workspace/target/debug/app", b"too-large", advertised=b"a")
        with self.assertRaises(SystemExit):
            self.unpack(args)
        self.assertEqual(self.binary.read_bytes(), b"original")
        self.assertFalse(list(self.binary.parent.glob(".blaze-bundle-*")))

    def test_success_replaces_only_bundle_destination_not_other_hardlinks(self):
        linked = self.root / "hardlinked"
        os.link(self.binary, linked)
        args = self.make_archive("workspace/target/debug/app", b"new")
        self.unpack(args)
        self.assertEqual(self.binary.read_bytes(), b"new")
        self.assertEqual(linked.read_bytes(), b"original")

    def test_path_command_rejects_changed_executable(self):
        self.manifest["sets"] = {"build": {"kind": "build", "executables": [
            {"name": "app", "path": str(self.binary)}]}}
        key = "workspace/target/debug/app"
        self.manifest["files"] = {key: {"sha256": hashlib.sha256(b"expected").hexdigest(),
                                        "size": 8, "executable": True}}
        doc = self.temp / "manifest.json"
        doc.write_text(json.dumps(self.manifest))
        with mock.patch.dict(os.environ, CI_BUNDLE_JSON=str(doc)), \
             self.assertRaises(SystemExit):
            bundle.path(SimpleNamespace(set="build", target="app"))


if __name__ == "__main__":
    unittest.main()
