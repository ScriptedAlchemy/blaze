#!/usr/bin/env python3
"""Test that landing requests cannot accidentally target the upstream fork."""
import importlib.util
import json
from pathlib import Path
from unittest import TestCase, main, mock

spec = importlib.util.spec_from_file_location("land", Path(__file__).with_name("land.py"))
land = importlib.util.module_from_spec(spec)
spec.loader.exec_module(land)


class RepositoryTests(TestCase):
    def test_common_origin_urls(self):
        for remote in ("git@github.com:ScriptedAlchemy/blaze.git",
                       "https://github.com/ScriptedAlchemy/blaze.git",
                       "ssh://git@github.com/ScriptedAlchemy/blaze",
                       "https://github.com/ScriptedAlchemy/blaze/"):
            with self.subTest(remote=remote):
                self.assertEqual(land.repo_from_remote(remote), "ScriptedAlchemy/blaze")
        self.assertEqual(land.repo_from_remote("git@github.com:indexable-inc/blaze.git"),
                         "indexable-inc/blaze")

    def test_unrecognized_origin_is_error(self):
        for remote in ("https://evil.com/ScriptedAlchemy/blaze",
                       "https://github.com/ScriptedAlchemy/blaze/extra",
                       "https://github.com/ScriptedAlchemy",
                       "https://github.com/a/b?x=1", ""):
            with self.subTest(remote=remote), self.assertRaises(ValueError):
                land.repo_from_remote(remote)

    def test_origin_is_read_once(self):
        land.repository.cache_clear()
        try:
            with mock.patch.object(land, "run", return_value="git@github.com:ScriptedAlchemy/blaze.git") as run:
                self.assertEqual(land.repository(), "ScriptedAlchemy/blaze")
                self.assertEqual(land.repository(), "ScriptedAlchemy/blaze")
                run.assert_called_once_with("git", "remote", "get-url", "origin")
        finally:
            land.repository.cache_clear()

    def test_state_queries_own_repository(self):
        result = {"data": {"repository": {"pullRequest": {"state": "OPEN"}}}}
        with mock.patch.object(land, "repository", return_value="ScriptedAlchemy/blaze"):
            with mock.patch.object(land, "gh", return_value=json.dumps(result)) as gh:
                self.assertEqual(land.state(64), {"state": "OPEN"})
                args = gh.call_args.args
                self.assertEqual(args[:3], ("api", "graphql", "-f"))
                self.assertIn('repository(owner: "ScriptedAlchemy", name: "blaze")', args[3])
                self.assertEqual(args[-2:], ("-F", "number=64"))

    def test_required_checks_are_scoped(self):
        checks = [{"name": "ci", "bucket": "fail"}, {"name": "macOS", "bucket": "pass"}]
        with mock.patch.object(land, "repository", return_value="ScriptedAlchemy/blaze"):
            with mock.patch.object(land, "run", return_value=json.dumps(checks)) as run:
                self.assertEqual(land.failed_checks(64), checks[:1])
                self.assertIn("ScriptedAlchemy/blaze", run.call_args.args)


class MergeQueueTests(TestCase):
    def test_disabled_auto_merge_enqueues_scoped_exact_head(self):
        current = dict(id="PR_node", state="OPEN", baseRefName="main",
                       headRefOid="deadbeef", isInMergeQueue=False)
        receipt = {"data": {"enqueuePullRequest": {"mergeQueueEntry": {"id": "MQ_node"}}}}
        denied = mock.Mock(returncode=1,
                           stderr="GraphQL: Auto merge is not allowed for this repository")
        with mock.patch.object(land, "repository", return_value="ScriptedAlchemy/blaze"), \
             mock.patch.object(land.subprocess, "run", return_value=denied) as run, \
             mock.patch.object(land, "state", return_value=current), \
             mock.patch.object(land, "gh", return_value=json.dumps(receipt)) as gh:
            land.queue_merge(79, "deadbeef")
        args = run.call_args.args[0]
        self.assertIn("--match-head-commit", args)
        self.assertEqual(args[args.index("--match-head-commit") + 1], "deadbeef")
        graph = gh.call_args.args
        self.assertIn("enqueuePullRequest", graph[3])
        self.assertIn("expectedHeadOid", graph[3])
        self.assertIn("pr=PR_node", graph)
        self.assertIn("head=deadbeef", graph)

    def test_changed_head_wrong_base_and_closed_pr_are_refused(self):
        current = dict(id="PR_node", state="OPEN", baseRefName="main",
                       headRefOid="deadbeef", isInMergeQueue=False)
        for changed in ({"headRefOid": "another"}, {"baseRefName": "dist/main"},
                        {"state": "CLOSED"}):
            with self.subTest(changed=changed), \
                 mock.patch.object(land, "state", return_value={**current, **changed}), \
                 mock.patch.object(land, "gh") as gh:
                with self.assertRaises(land.GhFailed):
                    land.enqueue_with_head_guard(79, "deadbeef")
                gh.assert_not_called()

    def test_invalid_enqueue_receipt_is_not_a_success(self):
        current = dict(id="PR_node", state="OPEN", baseRefName="main",
                       headRefOid="deadbeef", isInMergeQueue=False)
        with mock.patch.object(land, "state", return_value=current), \
             mock.patch.object(land, "gh", return_value=json.dumps({
                 "data": {"enqueuePullRequest": {"mergeQueueEntry": None}}})), \
             self.assertRaises(land.GhFailed):
            land.enqueue_with_head_guard(79, "deadbeef")

    def test_other_cli_errors_never_bypass_to_graphql(self):
        denied = mock.Mock(returncode=1, stderr="review required")
        with mock.patch.object(land, "repository", return_value="ScriptedAlchemy/blaze"), \
             mock.patch.object(land.subprocess, "run", return_value=denied), \
             mock.patch.object(land, "state", return_value={"isInMergeQueue": False}), \
             mock.patch.object(land, "gh") as gh:
            with self.assertRaises(land.GhFailed):
                land.queue_merge(79, "deadbeef")
            gh.assert_not_called()

    def test_already_queued_pr_does_not_reenqueue(self):
        denied = mock.Mock(returncode=1, stderr="already in merge queue")
        with mock.patch.object(land, "repository", return_value="ScriptedAlchemy/blaze"), \
             mock.patch.object(land.subprocess, "run", return_value=denied), \
             mock.patch.object(land, "state", return_value={"isInMergeQueue": True}), \
             mock.patch.object(land, "gh") as gh:
            land.queue_merge(79, "deadbeef")
            gh.assert_not_called()


if __name__ == "__main__":
    main()
