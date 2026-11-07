"""Behavioral checks for protected-main-safe benchmark history publication."""
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("publish_live_history.sh").read_text()


class HistoryPublication(unittest.TestCase):
    def run_publisher(self, *, existing_branch=False, existing_pr=False, existing_state="OPEN", clean=False,
                      push_fail=False, lookup_fail=False, pr_list_fail=False):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            git = root / "git"
            git.write_text('''#!/bin/sh
printf '%s\\n' "$*" >> "$MOCK_GIT_CALLS"
case "$1:$2" in
  diff:--cached) [ "$MOCK_CLEAN" = 1 ] ;;
  ls-remote:--exit-code)
    if [ "$MOCK_LOOKUP_FAIL" = 1 ]; then exit 128; fi
    [ "$MOCK_BRANCH_EXISTS" = 1 ] || exit 2 ;;
  push:-q) [ "$MOCK_PUSH_FAIL" != 1 ] ;;
  *) exit 0 ;;
esac
''')
            git.chmod(0o755)
            gh = root / "gh"
            gh.write_text('''#!/bin/sh
printf '%s\\n' "$*" >> "$MOCK_GH_CALLS"
case "$1:$2" in
  pr:list)
    [ "$MOCK_PR_LIST_FAIL" = 1 ] && exit 128
    [ "$MOCK_PR_EXISTS" = 1 ] && printf '%s\\t%s\\n' "$MOCK_PR_STATE" https://github.com/ScriptedAlchemy/blaze/pull/99
    exit 0 ;;
  pr:create) echo https://github.com/ScriptedAlchemy/blaze/pull/100 ;;
  *) echo "unexpected gh request: $*" >&2; exit 10 ;;
esac
''')
            gh.chmod(0o755)
            env = dict(os.environ, PATH=f"{tmp}:{os.environ['PATH']}", MOCK_GIT_CALLS=str(root / "git.log"),
                       MOCK_GH_CALLS=str(root / "gh.log"), MOCK_CLEAN="1" if clean else "0",
                       MOCK_BRANCH_EXISTS="1" if existing_branch else "0",
                       MOCK_PR_EXISTS="1" if existing_pr else "0", MOCK_PR_STATE=existing_state,
                       MOCK_PR_LIST_FAIL="1" if pr_list_fail else "0",
                       MOCK_PUSH_FAIL="1" if push_fail else "0",
                       MOCK_LOOKUP_FAIL="1" if lookup_fail else "0",
                       GITHUB_REPOSITORY="ScriptedAlchemy/blaze", GITHUB_RUN_ID="77",
                       GITHUB_SHA="abcdef12345678", GITHUB_SERVER_URL="https://github.com",
                       GITHUB_STEP_SUMMARY=str(root / "summary"))
            proc = subprocess.run(["bash", "-c", SCRIPT], capture_output=True, text=True, env=env)
            files = []
            for file in ("git.log", "gh.log", "summary"):
                target = root / file
                files.append(target.read_text() if target.exists() else "")
            return proc, *files

    def test_workflow_uses_bot_identity_for_pr_checks(self):
        workflow = (Path(__file__).resolve().parents[1] / "workflows/live-latency.yaml").read_text()
        history = workflow.split("\n  history:\n", 1)[1]
        identity = "secrets.RA_JIT_BOT_TOKEN || github.token"
        self.assertIn("GH_TOKEN: ${{ " + identity + " }}", history)
        self.assertIn("token: ${{ " + identity + " }}", history)

    def test_new_run_pushes_only_a_pr_branch(self):
        result, git, gh, summary = self.run_publisher()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("push -q origin HEAD:refs/heads/bench/live-latency-77", git)
        self.assertNotIn("HEAD:main", git)
        self.assertIn("pr create --repo ScriptedAlchemy/blaze --base main --head bench/live-latency-77", gh)
        self.assertIn("/pull/100", summary)

    def test_retry_reuses_existing_pr_without_rewriting_ref(self):
        result, git, gh, summary = self.run_publisher(existing_branch=True, existing_pr=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("push ", git)
        self.assertNotIn("pr create", gh)
        self.assertIn("/pull/99", summary)

    def test_partial_push_recovers_missing_pr(self):
        result, git, gh, _ = self.run_publisher(existing_branch=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("push ", git)
        self.assertIn("pr create", gh)

    def test_lookup_failure_does_not_publish_or_treat_branch_as_missing(self):
        result, git, gh, _ = self.run_publisher(lookup_fail=True)
        self.assertEqual(result.returncode, 128)
        self.assertIn("Cannot verify whether", result.stderr)
        self.assertNotIn("switch -c", git)
        self.assertNotIn("push ", git)
        self.assertNotIn("pr create", gh)

    def test_push_rejection_does_not_create_pr(self):
        result, git, gh, _ = self.run_publisher(push_fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("push -q origin", git)
        self.assertNotIn("pr create", gh)

    def test_unchanged_history_skips_publication(self):
        result, git, gh, _ = self.run_publisher(clean=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("push ", git)
        self.assertEqual(gh, "")

    def test_rerun_after_merge_does_not_duplicate_deleted_branch(self):
        result, git, gh, summary = self.run_publisher(existing_pr=True, existing_state="MERGED")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("ls-remote", git)
        self.assertNotIn("push ", git)
        self.assertNotIn("pr create", gh)
        self.assertIn("already merged", summary)

    def test_closed_without_merge_is_not_silently_republished(self):
        result, git, gh, _ = self.run_publisher(existing_pr=True, existing_state="CLOSED", existing_branch=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("closed without merging", result.stderr)
        self.assertNotIn("ls-remote", git)
        self.assertNotIn("push ", git)
        self.assertNotIn("pr create", gh)

    def test_pr_lookup_failure_does_not_publish(self):
        result, git, gh, _ = self.run_publisher(pr_list_fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("ls-remote", git)
        self.assertNotIn("push ", git)
        self.assertNotIn("pr create", gh)


if __name__ == "__main__":
    unittest.main()
