"""End-to-end contract tests: local Git repositories and a strict mocked gh executable.

The production publisher is executed in a subprocess against a temporary bare origin.
No GitHub API, production branch, or real credentials are touched.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

PUBLISHER = Path(__file__).with_name("publish_pgo_pin.py").resolve()


class PinPublicationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.origin = self.root / "origin.git"
        self.work = self.root / "work"
        self.work.mkdir()
        subprocess.run(["git", "init", "--bare", "-q", str(self.origin)], check=True)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "test")
        self.git("config", "user.email", "test@example.test")
        (self.work / "pgo").mkdir()
        (self.work / "pgo" / "profiles.json").write_text('{"before": true}\n')
        self.git("add", ".")
        self.git("commit", "-qm", "source")
        self.sha = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("remote", "add", "origin", str(self.origin))
        self.git("push", "-q", "-u", "origin", "main")
        (self.work / "pgo" / "profiles.json").write_text('{"trained": true}\n')
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.calls = self.root / "gh-calls.txt"
        gh = self.bin / "gh"
        gh.write_text('''#!/usr/bin/env python3
import os,sys,json
from pathlib import Path
with Path(os.environ["MOCK_GH_CALLS"]).open("a") as output:
    output.write(json.dumps(sys.argv[1:]) + "\\n")
if os.environ.get("MOCK_GH_FAIL") == "1":
    sys.stderr.write("GitHub unavailable\\n")
    sys.exit(12)
if sys.argv[1:3] == ["pr", "list"]:
    print(os.environ.get("MOCK_PR_LIST", "[]"))
elif sys.argv[1:3] == ["pr", "create"]:
    print(os.environ.get("MOCK_PR_URL", "https://github.com/ScriptedAlchemy/blaze/pull/123"))
else:
    sys.stderr.write("unexpected gh operation\\n")
    sys.exit(13)
''')
        gh.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                        GH_TOKEN="mock", GITHUB_REPOSITORY="ScriptedAlchemy/blaze",
                        GITHUB_RUN_ID="4567", GITHUB_SHA=self.sha,
                        GITHUB_REF="refs/heads/main", MOCK_GH_CALLS=str(self.calls),
                        GITHUB_STEP_SUMMARY=str(self.root / "summary"))

    def git(self, *args, check=True):
        return subprocess.run(["git", *args], cwd=self.work, text=True,
                              capture_output=True, check=check)

    def publish(self):
        return subprocess.run([sys.executable, str(PUBLISHER)], cwd=self.work,
                              env=self.env, text=True, capture_output=True)

    def remote(self, branch="pgo/pins-4567"):
        return subprocess.run(["git", f"--git-dir={self.origin}", "rev-parse", f"refs/heads/{branch}"],
                              capture_output=True, text=True)

    def gh_calls(self):
        return [json.loads(x) for x in self.calls.read_text().splitlines()] if self.calls.exists() else []

    def test_new_run_creates_review_pr_and_never_modifies_main(self):
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("awaits review", result.stdout)
        self.assertEqual(self.remote().returncode, 0)
        self.assertEqual(self.remote("main").stdout.strip(), self.sha)
        self.assertEqual(self.git("diff", "--name-only", self.sha, "HEAD").stdout.strip(),
                         "pgo/profiles.json")
        calls = self.gh_calls()
        self.assertEqual([c[:2] for c in calls], [["pr", "list"], ["pr", "create"]])
        self.assertIn("main", calls[1])
        self.assertNotIn("HEAD:main", result.stdout + result.stderr)

    def test_identical_pins_result_in_no_branch_or_pr(self):
        (self.work / "pgo/profiles.json").write_text('{"before": true}\n')
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("unchanged", result.stdout)
        self.assertNotEqual(self.remote().returncode, 0)
        self.assertEqual([c[:2] for c in self.gh_calls()], [["pr", "list"]])

    def test_advanced_main_fails_closed(self):
        self.git("switch", "-c", "later")
        (self.work / "README").write_text("later")
        self.git("add", ".")
        self.git("commit", "-qm", "other")
        self.git("push", "-q", "origin", "HEAD:refs/heads/main")
        self.git("switch", "main")
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("main advanced", result.stderr)
        self.assertNotEqual(self.remote().returncode, 0)

    def test_mismatched_checked_out_head_fails_closed(self):
        self.git("commit", "--allow-empty", "-qm", "wrong checkout")
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Checkout does not match", result.stderr)

    def test_existing_identical_unpublished_branch_recovers_pr(self):
        self.git("switch", "-c", "pgo/pins-4567")
        self.git("add", "pgo/profiles.json")
        self.git("commit", "-qm", "pin")
        self.git("push", "-q", "origin", "HEAD:refs/heads/pgo/pins-4567")
        branch_sha = self.remote().stdout.strip()
        self.git("switch", "main")
        (self.work / "pgo/profiles.json").write_text('{"trained": true}\n')
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote().stdout.strip(), branch_sha)
        self.assertEqual(self.gh_calls()[-1][:2], ["pr", "create"])

    def test_existing_branch_with_different_pins_is_not_overwritten(self):
        self.git("switch", "-c", "pgo/pins-4567")
        (self.work / "pgo/profiles.json").write_text('{"unexpected": true}\n')
        self.git("add", "pgo/profiles.json")
        self.git("commit", "-qm", "wrong pin")
        self.git("push", "-q", "origin", "HEAD:refs/heads/pgo/pins-4567")
        branch_sha = self.remote().stdout.strip()
        self.git("switch", "main")
        (self.work / "pgo/profiles.json").write_text('{"trained": true}\n')
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("differs", result.stderr)
        self.assertEqual(self.remote().stdout.strip(), branch_sha)

    def test_existing_branch_with_unrelated_change_is_rejected(self):
        self.git("switch", "-c", "pgo/pins-4567")
        self.git("add", "pgo/profiles.json")
        (self.work / "other").write_text("unsafe")
        self.git("add", "other")
        self.git("commit", "-qm", "extra")
        self.git("push", "-q", "origin", "HEAD:refs/heads/pgo/pins-4567")
        self.git("switch", "main")
        (self.work / "pgo/profiles.json").write_text('{"trained": true}\n')
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected file changes", result.stderr)

    def test_existing_approved_looking_pr_is_only_reported_not_merged(self):
        self.git("switch", "-c", "pgo/pins-4567")
        self.git("add", "pgo/profiles.json")
        self.git("commit", "-qm", "pin")
        self.git("push", "-q", "origin", "HEAD:refs/heads/pgo/pins-4567")
        self.git("switch", "main")
        (self.work / "pgo/profiles.json").write_text('{"trained": true}\n')
        self.env["MOCK_PR_LIST"] = json.dumps([{"state": "OPEN", "baseRefName": "main",
            "url": "https://github.com/ScriptedAlchemy/blaze/pull/10"}])
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("awaiting review", result.stdout)
        self.assertEqual([c[:2] for c in self.gh_calls()], [["pr", "list"]])
        self.assertEqual(self.remote().returncode, 0)

    def test_open_pr_branch_without_matching_source_is_rejected(self):
        self.git("switch", "-c", "pgo/pins-4567")
        (self.work / "pgo/profiles.json").write_text('{"wrong": true}\n')
        self.git("add", "pgo/profiles.json")
        self.git("commit", "-qm", "malicious update")
        self.git("push", "-q", "origin", "HEAD:refs/heads/pgo/pins-4567")
        self.git("switch", "main")
        (self.work / "pgo/profiles.json").write_text('{"trained": true}\n')
        self.env["MOCK_PR_LIST"] = json.dumps([{"state": "OPEN", "baseRefName": "main",
            "url": "https://github.com/ScriptedAlchemy/blaze/pull/10"}])
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("differs", result.stderr)

    def test_already_merged_run_is_idempotent_even_if_main_advanced(self):
        self.env["MOCK_PR_LIST"] = json.dumps([{"state": "MERGED", "baseRefName": "main",
            "url": "https://github.com/ScriptedAlchemy/blaze/pull/10"}])
        self.env["GITHUB_SHA"] = "a" * 40
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("already merged", result.stdout)

    def test_closed_pr_must_not_be_reopened(self):
        self.env["MOCK_PR_LIST"] = json.dumps([{"state": "CLOSED", "baseRefName": "main",
            "url": "https://github.com/ScriptedAlchemy/blaze/pull/10"}])
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("closed", result.stderr)

    def test_unexpected_pr_base_and_ambiguous_results_are_errors(self):
        self.env["MOCK_PR_LIST"] = json.dumps([{"state": "OPEN", "baseRefName": "dist/main",
            "url": "https://github.com/ScriptedAlchemy/blaze/pull/10"}])
        self.assertIn("unexpected base", self.publish().stderr)
        self.env["MOCK_PR_LIST"] = "[{}, {}]"
        self.assertIn("Ambiguous", self.publish().stderr)

    def test_github_failures_do_not_produce_branches(self):
        self.env["MOCK_GH_FAIL"] = "1"
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("GitHub unavailable", result.stderr)
        self.assertNotEqual(self.remote().returncode, 0)

    def test_invalid_or_missing_authentication_and_run_identity_refused(self):
        for key, bad in [("GH_TOKEN", ""), ("GITHUB_REF", "refs/heads/not-main"),
                         ("GITHUB_REPOSITORY", "bad"), ("GITHUB_RUN_ID", "../evil")]:
            with self.subTest(key=key):
                original = self.env[key]
                self.env[key] = bad
                self.assertNotEqual(self.publish().returncode, 0)
                self.env[key] = original
        self.assertNotEqual(self.remote().returncode, 0)

    def test_failed_pr_creation_keeps_nonforce_branch_for_safe_recovery(self):
        self.env["MOCK_PR_URL"] = "not-a-confirmed-pr"
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("did not confirm", result.stderr)
        sha = self.remote().stdout.strip()
        self.assertTrue(sha)
        self.git("switch", "main")
        self.env["MOCK_PR_URL"] = "https://github.com/ScriptedAlchemy/blaze/pull/234"
        recovered = self.publish()
        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertEqual(self.remote().stdout.strip(), sha)


if __name__ == "__main__":
    unittest.main()
