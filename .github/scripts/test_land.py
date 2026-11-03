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


if __name__ == "__main__":
    main()
