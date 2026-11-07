"""Nightly failures remain visible when GitHub issues are disabled."""

import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


class NightlyIssueTests(unittest.TestCase):
    def run_action(self, has_issues):
        action = (Path(__file__).resolve().parent.parent / ".github/actions/nightly-issue/action.yml").read_text()
        script = textwrap.dedent(action.split("      run: |\n", 1)[1])
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            gh = root / "gh"
            gh.write_text("""#!/bin/sh
printf '%s\\n' "$*" >> "$MOCK_CALLS"
case "$1:$2" in
  api:--paginate) printf 'failure\\tcompile\\thttps://example.test/job/3\\n' ;;
  api:repos/*) printf '%s\\n' "$MOCK_HAS_ISSUES" ;;
  issue:list) : ;;
  issue:create) : ;;
  issue:comment) : ;;
  *) echo "unexpected gh invocation: $*" >&2; exit 79 ;;
esac
""")
            gh.chmod(0o755)
            summary, calls = root / "summary", root / "calls"
            env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}", MOCK_CALLS=str(calls),
                       MOCK_HAS_ISSUES=has_issues, GITHUB_WORKFLOW="claims",
                       GITHUB_REPOSITORY="ScriptedAlchemy/blaze", GITHUB_RUN_ID="77",
                       GITHUB_RUN_ATTEMPT="1", GITHUB_SHA="0123456789abcdef",
                       GITHUB_STEP_SUMMARY=str(summary), RUN="https://example.test/run/77")
            proc = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", script],
                                  env=env, capture_output=True, text=True)
            return proc, summary.read_text() if summary.exists() else "", calls.read_text()

    def test_issues_disabled_preserves_run_summary_without_issue_call(self):
        proc, summary, calls = self.run_action("false")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("Nightly claims failed", summary)
        self.assertIn("https://example.test/job/3", summary)
        self.assertFalse(any(line.startswith("issue ") for line in calls.splitlines()), calls)

    def test_enabled_repository_still_creates_issue(self):
        proc, summary, calls = self.run_action("true")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("https://example.test/job/3", summary)
        self.assertIn("issue create", calls)

    def test_unexpected_issue_state_fails_instead_of_silently_skipping(self):
        proc, _, calls = self.run_action("null")
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(any(line.startswith("issue ") for line in calls.splitlines()), calls)


if __name__ == "__main__":
    unittest.main()
