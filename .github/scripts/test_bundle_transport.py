"""Exercise real tar/zstd transport and recorded Cargo process environments."""
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bundle


class BundleTransportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.ws, self.temp = self.root / "workspace", self.root / "temp"
        self.ws.mkdir()
        self.temp.mkdir()
        self.roots = {"workspace": str(self.ws), "temp": str(self.temp)}
        self.identity = {"commit": "commit", "tree": "tree", "flake": "flake", "cargo_lock": "lock"}
        self.source = self.temp / "archive"
        self.source.mkdir()

    def archive(self, payload, padding=0):
        key = "workspace/target/app"
        manifest = {"schema": bundle.SCHEMA, "platform": "linux-x64", "source": self.identity,
                    "roots": self.roots, "sets": {}, "files": {key: {
                        "size": len(payload), "sha256": hashlib.sha256(payload).hexdigest(),
                        "executable": True}}}
        text = json.dumps(manifest).encode()
        (self.source / "bundle.json").write_bytes(text)
        data = io.BytesIO()
        with tarfile.open(fileobj=data, mode="w") as archive:
            member = tarfile.TarInfo(key)
            member.size = len(payload)
            archive.addfile(member, io.BytesIO(payload))
        subprocess.run(["zstd", "-q", "-f", "-o", str(self.source / "bundle.tar.zst")],
                       input=data.getvalue() + bytes(padding), check=True)
        return SimpleNamespace(dir=str(self.source), platform="linux-x64",
                               sha256=hashlib.sha256(text).hexdigest(), github_env="")

    def unpack(self, args):
        with mock.patch.object(bundle, "roots", return_value=self.roots), \
             mock.patch.object(bundle, "source_identity", return_value=self.identity):
            bundle.unpack(args)

    def test_valid_tar_padding_does_not_kill_decompressor(self):
        # The tar reader stops at its end marker; zstd still has more valid output.
        args = self.archive(b"program", padding=2 * 1024 * 1024)
        self.unpack(args)
        self.assertEqual((self.ws / "target/app").read_bytes(), b"program")

    def test_pack_hardlinked_executables_round_trip_as_regular_members(self):
        release = self.ws / "target/release"
        runtime = self.temp / "owned-runtime"
        release.mkdir(parents=True)
        runtime.mkdir()
        original = release / bundle.RELEASE_BINS[0]
        original.write_bytes(b"same executable")
        original.chmod(0o755)
        for name in bundle.RELEASE_BINS[1:]:
            os.link(original, release / name)
        (runtime / "blaze-proc-macro-srv").write_bytes(b"runtime")
        sets = self.temp / "sets.json"
        sets.write_text("{}")
        run = subprocess.run
        def tools(command, *args, **kwargs):
            if command[0] in ("rustc", "cargo", "llvm-config"):
                return SimpleNamespace(stdout="pinned toolchain")
            return run(command, *args, **kwargs)
        args = SimpleNamespace(platform="linux-x64", sets_record=str(sets), shell="bench",
                               out=str(self.source), github_output="")
        with mock.patch.object(bundle, "roots", return_value=self.roots), \
             mock.patch.object(bundle, "source_identity", return_value=self.identity), \
             mock.patch.object(bundle.shutil, "which", return_value="tool"), \
             mock.patch.object(bundle.subprocess, "run", side_effect=tools):
            bundle.pack(args)
        text = (self.source / "bundle.json").read_bytes()
        self.unpack(SimpleNamespace(dir=str(self.source), platform="linux-x64", github_env="",
                                    sha256=hashlib.sha256(text).hexdigest()))
        for name in bundle.RELEASE_BINS:
            self.assertEqual((release / name).read_bytes(), b"same executable")

    def test_recorded_removed_environment_is_not_resurrected(self):
        base = self.temp / "environment.json"
        record = self.temp / "record.jsonl"
        base.write_text(json.dumps({"CARGO_TEST_REMOVED": "parent value"}))
        with mock.patch.dict(os.environ, {"BUNDLE_RECORD_BASE": str(base),
                                         "BUNDLE_RECORD": str(record)}, clear=True):
            bundle.record([sys.executable])
        row = json.loads(record.read_text())
        self.assertIn("CARGO_TEST_REMOVED", row.get("env_remove", []))
        manifest = {"files": {}, "roots": self.roots}
        entry = {"kind": "test", "runs": [row]}
        with mock.patch.dict(os.environ, {"CARGO_TEST_REMOVED": "consumer value"}), \
             mock.patch.object(bundle, "load_set", return_value=(manifest, entry)), \
             mock.patch.object(bundle, "to_key", return_value="workspace/target/test"), \
             mock.patch.object(bundle, "verified_executable"), \
             mock.patch.object(bundle.subprocess, "run", return_value=SimpleNamespace(returncode=0)) as run:
            bundle.test(SimpleNamespace(set="test", libtest=[]))
        self.assertNotIn("CARGO_TEST_REMOVED", run.call_args.kwargs["env"])

    def test_path_rejects_nonexecutable_even_when_bytes_still_match(self):
        executable = self.ws / "target/app"
        executable.parent.mkdir()
        executable.write_bytes(b"program")
        executable.chmod(0o644)
        manifest = {"files": {"workspace/target/app": {
            "sha256": hashlib.sha256(b"program").hexdigest(), "size": 7, "executable": True}}}
        with self.assertRaises(SystemExit):
            bundle.verified_executable(str(executable), "workspace/target/app", manifest)


if __name__ == "__main__":
    unittest.main()
