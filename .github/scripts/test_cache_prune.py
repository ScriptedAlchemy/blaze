"""Cache maintenance must never broaden its authority after a partial setup/save."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest import mock

FILE = Path(__file__).resolve().parents[1] / "actions/toolchain/save/prune.py"
spec = importlib.util.spec_from_file_location("cache_prune", FILE)
prune = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prune)
SCOPE = "sccache-v1-bundle-Linux-X64-"
KEY = SCOPE + "a" * 16 + "-" + "b" * 40
REF = "refs/pull/32/merge"
OLD = SCOPE + "c" * 16 + "-" + "d" * 40


def row(cache_id, key=KEY, ref=REF, created="2026-10-31T20:00:00Z", version="v1", size=100):
    return dict(id=cache_id, key=key, ref=ref, created_at=created, version=version, size_in_bytes=size)


def select(*rows, **kwargs):
    return prune.superseded([{"actions_caches": list(rows)}],
                            kwargs.get("key", KEY), kwargs.get("scope", SCOPE), kwargs.get("ref", REF))


class CachePruneTests(unittest.TestCase):
    def test_only_older_same_scope_ref_and_version_are_selected(self):
        old = row(2, OLD, created="2026-10-30T00:00:00Z")
        self.assertEqual(select(row(1), old, row(3, "nix-shell-cache"),
                                row(4, OLD, ref="refs/heads/main"),
                                row(5, SCOPE + "e" * 16 + "-" + "f" * 40, created="2026-11-01T00:00:00Z"),
                                row(6, OLD, version="v2")), [2])

    def test_malformed_old_key_is_not_in_the_cache_namespace(self):
        self.assertEqual(select(row(1), row(2, KEY + "-extra", created="2026-01-01T00:00:00Z")), [])

    def test_absent_replacement_preserves_old_entries(self):
        self.assertEqual(select(row(2, OLD)), [])

    def test_zero_size_replacement_preserves_old_entries(self):
        self.assertEqual(select(row(1, size=0), row(2, OLD)), [])

    def test_replacement_on_other_ref_is_not_authority(self):
        self.assertEqual(select(row(1, ref="refs/heads/main"), row(2, OLD)), [])

    def test_empty_broad_or_mismatched_scope_is_rejected(self):
        for kwargs in ({"scope": ""}, {"scope": "sccache-v1-"}, {"key": ""},
                       {"key": "nix-cache"}, {"ref": ""}, {"ref": "refs/heads/main&key="}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                select(row(1), **kwargs)

    def test_missing_timestamps_or_versions_never_authorize_pruning(self):
        for replacement in (row(1, created="bad"), row(1, version=""), row(1, created="2026-01-01")):
            self.assertEqual(select(replacement, row(2, OLD)), [])

    def test_same_time_or_newer_cache_is_preserved(self):
        self.assertEqual(select(row(1), row(2, OLD)), [])

    def test_all_pages_are_considered_before_deletion(self):
        pages = [{"actions_caches": [row(2, OLD, created="2026-01-01T00:00:00Z")]},
                 {"actions_caches": [row(1)]}]
        self.assertEqual(prune.superseded(pages, KEY, SCOPE, REF), [2])

    def test_malformed_or_conflicting_inventory_fails_closed(self):
        for pages in ([], [{}], [{"actions_caches": [None]}],
                      [{"actions_caches": [row(1), row(1, ref="refs/heads/main")]}]):
            with self.subTest(pages=pages), self.assertRaises(ValueError):
                prune.superseded(pages, KEY, SCOPE, REF)

    def test_invalid_filters_make_no_api_call(self):
        with mock.patch.dict(os.environ, {"KEY": "", "SCOPE": "", "REF": REF}), \
             mock.patch.object(prune.subprocess, "check_output") as listing, \
             mock.patch.object(prune.subprocess, "run") as deletion, \
             contextlib.redirect_stdout(io.StringIO()):
            prune.main()
        listing.assert_not_called()
        deletion.assert_not_called()

    def test_noop_save_makes_no_delete_request(self):
        with mock.patch.dict(os.environ, {"KEY": KEY, "SCOPE": SCOPE, "REF": REF,
                                          "GITHUB_REPOSITORY": "owner/repo"}), \
             mock.patch.object(prune.subprocess, "check_output", return_value=json.dumps([
                 {"actions_caches": [row(2, OLD)]}])), \
             mock.patch.object(prune.subprocess, "run") as deletion:
            prune.main()
        deletion.assert_not_called()

    def test_delete_request_names_only_validated_numeric_identity(self):
        data = [{"actions_caches": [row(1), row(2, OLD, created="2026-01-01T00:00:00Z")]}]
        with mock.patch.dict(os.environ, {"KEY": KEY, "SCOPE": SCOPE, "REF": REF,
                                          "GITHUB_REPOSITORY": "owner/repo"}), \
             mock.patch.object(prune.subprocess, "check_output", return_value=json.dumps(data)) as listing, \
             mock.patch.object(prune.subprocess, "run", return_value=SimpleNamespace(returncode=0)) as deletion, \
             contextlib.redirect_stdout(io.StringIO()):
            prune.main()
        self.assertIn("--paginate", listing.call_args.args[0])
        self.assertEqual(deletion.call_args.args[0],
                         ["gh", "api", "--method", "DELETE", "repos/owner/repo/actions/caches/2"])


if __name__ == "__main__":
    unittest.main()
