"""Isolated behavioral validation for fork-owned content-addressed PGO profile releases."""
import copy
import hashlib
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("pin_pgo_release", HERE / "pin_pgo_release.py")
pin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pin)


class ReleaseBindingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.sha = "e" * 40
        self.artifacts = root / "downloaded"
        self.artifacts.mkdir()
        self.pins = root / "pgo" / "profiles.json"
        self.pins.parent.mkdir()
        self.table = {
            "toolchain": "1.99.0",
            "release": "https://github.com/indexable-inc/blaze/releases/download/pgo-profiles",
            "targets": {},
        }
        for target in pin.TARGETS:
            folder = self.artifacts / f"pgo-{target}"
            folder.mkdir()
            blob = (target + self.sha).encode()
            digest = hashlib.sha256(blob).hexdigest()
            name = f"ra-jit-1.99.0-{target}-{digest[:16]}.profdata"
            (folder / name).write_bytes(blob)
            info = dict(target=target, name=name, sha256=digest, commit=self.sha,
                        count=187041696539, crate="codehash")
            (folder / f"{target}.json").write_text(json.dumps(info))
            self.table["targets"][target] = {k: v for k, v in info.items() if k != "target"}
        self.write()

    def write(self):
        self.pins.write_text(json.dumps(self.table, indent=2) + "\n")

    def bind(self, repo="ScriptedAlchemy/blaze", sha=None):
        return pin.bind_release(repo, sha or self.sha, self.artifacts, self.pins)

    def test_full_training_repoints_upstream_url_to_fork_without_changing_content_pins(self):
        self.assertTrue(self.bind())
        got = json.loads(self.pins.read_text())
        self.assertEqual(got["targets"], self.table["targets"])
        self.assertEqual(got["release"], "https://github.com/ScriptedAlchemy/blaze/releases/download/pgo-profiles")
        self.assertFalse(self.bind())

    def test_missing_target_fails_closed_without_modifying_pins(self):
        next(self.artifacts.iterdir()).rename(self.artifacts / "missing")
        old = self.pins.read_bytes()
        with self.assertRaises(pin.PinError): self.bind()
        self.assertEqual(self.pins.read_bytes(), old)

    def test_stale_training_revision_is_rejected(self):
        next(self.artifacts.rglob("*.json")).write_text('{"target":"x","commit":"old"}')
        with self.assertRaises(pin.PinError): self.bind()

    def test_profile_blob_digest_mismatch_fails_closed(self):
        next(self.artifacts.rglob("*.profdata")).write_bytes(b"corrupted")
        with self.assertRaises(pin.PinError): self.bind()

    def test_mismatched_pin_table_cannot_mix_upstream_and_fork_assets(self):
        target = next(iter(self.table["targets"]))
        self.table["targets"][target]["commit"] = "d" * 40
        self.write()
        with self.assertRaises(pin.PinError): self.bind()

    def test_workflow_fails_closed_before_uploading_any_profile(self):
        workflow = (HERE.parent / "workflows" / "pgo.yaml").read_text()
        verify = workflow.index("- name: Verify and prepare complete training pins")
        upload = workflow.index("- name: Upload validated profiles")
        publish = workflow.index("- name: Publish reviewed pin PR")
        self.assertLess(verify, upload)
        self.assertLess(upload, publish)
        self.assertIn("pin_pgo_release.py", workflow[verify:upload])
        self.assertIn("publish_pgo_pin.py", workflow[publish:])

    def test_malicious_repository_identifier_is_rejected(self):
        with self.assertRaises(pin.PinError): self.bind(repo="owner/repo/../../bad")


if __name__ == "__main__":
    unittest.main()
