"""Execute the consumer's real command against a prebuilt test-program fixture."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "harness/dist"))
import check_nix_workflow as workflow

FILE = ROOT / ".github/workflows/blaze-serve-review.yaml"
PREFIX = "nix develop .#bench --command python3 -I .github/scripts/bundle.py test serve-cli".split()


def cli_steps(text):
    found = []
    for step in workflow.scan(text)["server-tests"]["steps"]:
        for segment in workflow.segments(workflow.step_body(step)):
            words = workflow.command(segment)
            if words[:len(PREFIX)] == PREFIX:
                found.append((step, words[len(PREFIX):]))
    return found


class CLICoverageTests(unittest.TestCase):
    def test_cli_suite_runs_once_without_name_filters(self):
        found = cli_steps(FILE.read_text())
        self.assertEqual(len(found), 1, found)
        step, args = found[0]
        self.assertEqual(args, ["--", "--test-threads=2"])
        self.assertFalse(workflow.truthy(workflow.step_value(step, "continue-on-error")))
        self.assertEqual(workflow.step_value(step, "if"), "${{ !cancelled() }}")

    def test_real_bundle_command_includes_previously_omitted_and_future_tests(self):
        names = ["scripts::tests::library_descriptor", "cargo::cache_service::tests::fresh_remote",
                 "child::tests::duplex", "future_module::new_regression"]
        with tempfile.TemporaryDirectory() as directory:
            # macOS exposes temporary directories through /var -> /private/var.
            root = Path(directory).resolve()
            exe = root / "target/test-cli"
            exe.parent.mkdir()
            exe.write_text(f"#!{sys.executable}\nimport sys\nnames={names!r}\n"
                           "filters=[arg for arg in sys.argv[1:] if not arg.startswith('--')]\n"
                           "for name in names:\n"
                           "    if not filters or any(arg in name for arg in filters): print(name)\n")
            exe.chmod(0o755)
            manifest = {"roots": {"workspace": str(root)}, "files": {"workspace/target/test-cli": {
                "sha256": hashlib.sha256(exe.read_bytes()).hexdigest(),
                "size": exe.stat().st_size, "executable": True}}, "sets": {"serve-cli": {
                    "kind": "test", "runs": [{"exe": str(exe), "cwd": str(root), "args": [], "env": {}}]}}}
            path = root / "bundle.json"
            path.write_text(json.dumps(manifest))
            emitted = []
            for _, args in cli_steps(FILE.read_text()):
                result = subprocess.run([sys.executable, "-I", str(ROOT / ".github/scripts/bundle.py"),
                                         "test", "serve-cli", *args],
                                        env={**os.environ, "CI_BUNDLE_JSON": str(path)},
                                        capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                emitted.extend(line for line in result.stdout.splitlines() if line in names)
            self.assertCountEqual(emitted, names)

    def test_other_package_consumers_remain_present(self):
        required = {"serve-server", "serve-project", "serve-resolve", "serve-codegen", "serve-syntax",
                    "serve-diagnostics", "serve-hir-ide", "serve-proc-macro", "serve-cfg", "serve-paths",
                    "serve-hir-def", "serve-wire", "serve-bootstrap"}
        text = FILE.read_text()
        for name in required:
            self.assertIn(f"bundle.py test {name} --", text)

    def test_nested_workspace_producer_keeps_explicit_target_root(self):
        jobs = workflow.scan((ROOT / ".github/workflows/bundle.yaml").read_text())
        steps = [step for job in jobs.values() for step in job["steps"]
                 if "bundle.py build" in workflow.step_body(step)]
        self.assertEqual(len(steps), 1)
        self.assertEqual(workflow.step_value(steps[0], "CARGO_TARGET_DIR"),
                         "${{ github.workspace }}/target")


if __name__ == "__main__":
    unittest.main()
