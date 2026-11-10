"""A filtered, incomplete, stale or failed migration run cannot satisfy the CI gate."""
import json
from pathlib import Path
import tempfile
import unittest

import forkfixes_acceptance as acceptance


class ForkMigrationReceipt(unittest.TestCase):
    def fixture(self):
        return {
            "run.json": {"input": {"only": []}, "errors": [], "complete": True},
            "results.json": {case["name"]: {"ok": True, "fixes": case["fixes"], "why": "as stated"}
                             for case in acceptance.CASES},
            "provenance.json": {"head": "source fixture", "binaries": {"blaze": "fixture digest"}},
            "provenance-after.json": {"head": "source fixture", "binaries": {"blaze": "fixture digest"}},
        }

    def verify(self, files):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, content in files.items():
                (root / name).write_text(json.dumps(content))
            return acceptance.verify(root)

    def test_only_the_complete_successful_run_has_a_receipt(self):
        self.assertEqual(self.verify(self.fixture()), len(acceptance.CASES))
        for filename in self.fixture():
            with self.subTest(missing=filename):
                files = self.fixture()
                del files[filename]
                with self.assertRaises(OSError):
                    self.verify(files)
        name = acceptance.CASES[0]["name"]
        changes = (
            ("incomplete", lambda f: f["run.json"].update(complete=False)),
            ("cleanup failed", lambda f: f["run.json"].update(errors=["cleanup: daemon did not stop"])),
            ("filtered", lambda f: f["run.json"]["input"].update(only=[name])),
            ("missing case", lambda f: f["results.json"].pop(name)),
            ("unknown case", lambda f: f["results.json"].update(unrelated={"ok": True})),
            ("case failed", lambda f: f["results.json"][name].update(ok=False)),
            ("case not run", lambda f: f["results.json"][name].update(ok=False, why="not run")),
            ("integer verdict", lambda f: f["results.json"][name].update(ok=1)),
            ("mapping stale", lambda f: f["results.json"][name].update(fixes=["another commit"])),
            ("binary changed", lambda f: f["provenance-after.json"]["binaries"].update(blaze="changed")),
            ("source changed", lambda f: f["provenance-after.json"].update(head="another source")),
            ("empty identity", lambda f: f.update({"provenance.json": {}, "provenance-after.json": {}})),
        )
        for cause, change in changes:
            with self.subTest(cause=cause):
                files = self.fixture()
                change(files)
                with self.assertRaises(ValueError):
                    self.verify(files)


if __name__ == "__main__":
    unittest.main()
