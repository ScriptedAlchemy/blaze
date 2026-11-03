"""Regressions must fail CI even when a fork has GitHub Issues disabled."""
import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

WORKFLOW = Path(__file__).resolve().parents[1] / "workflows" / "live-latency.yaml"
SECTION = WORKFLOW.read_text().split("      - name: Open or update the regression issue\n", 1)[1]
SCRIPT = textwrap.dedent(SECTION.split("        run: |\n", 1)[1])


class RegressionIssueTests(unittest.TestCase):
    def run_report(self, available, open_issue=""):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "regressions.md").write_text("Measured latency regression.\n")
            gh = root / "gh"
            gh.write_text("""#!/bin/sh
printf '%s\\n' "$*" >> "$MOCK_CALLS"
case "$1:$2" in
  api:repos/*) printf '%s\\n' "$MOCK_AVAILABLE" ;;
  label:create) : ;;
  issue:list) printf '%s\\n' "$MOCK_OPEN" ;;
  issue:create|issue:comment) : ;;
  *) echo "unexpected gh invocation: $*" >&2; exit 79 ;;
esac
""")
            gh.chmod(0o755)
            calls = root / "calls"
            env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                       MOCK_CALLS=str(calls), MOCK_AVAILABLE=available,
                       MOCK_OPEN=open_issue, GITHUB_REPOSITORY="ScriptedAlchemy/blaze",
                       GITHUB_SHA="abcdef12345678", RUNNER_TEMP=str(root))
            proc = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", SCRIPT],
                                  env=env, capture_output=True, text=True)
            return proc, calls.read_text()

    def test_disabled_issues_preserve_regression_failure(self):
        proc, calls = self.run_report("false")
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn("Issues disabled", proc.stdout)
        self.assertIn("regressed beyond its noise band", proc.stdout)
        self.assertFalse(any(line.startswith(("label ", "issue "))
                             for line in calls.splitlines()), calls)

    def test_enabled_issues_create_report_without_hiding_failure(self):
        proc, calls = self.run_report("true")
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn("label create", calls)
        self.assertIn("issue create", calls)

    def test_existing_issue_receives_comment_not_duplicate(self):
        proc, calls = self.run_report("true", "42")
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn("issue comment 42", calls)
        self.assertNotIn("issue create", calls)

    def test_unexpected_issues_setting_fails_without_reporting_request(self):
        proc, calls = self.run_report("null")
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn("Unexpected Issues availability", proc.stdout)
        self.assertFalse(any(line.startswith(("label ", "issue "))
                             for line in calls.splitlines()), calls)


if __name__ == "__main__":
    unittest.main()
