#!/usr/bin/env python3
"""Cancellation is scoped to PR identity and live head, not branch names."""
import importlib.util
from pathlib import Path
from unittest import TestCase, main, mock
from urllib.error import HTTPError

spec = importlib.util.spec_from_file_location(
    "cancel_pr_runs", Path(__file__).with_name("cancel_pr_runs.py")
)
cancel = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cancel)


def run(id=1, sha="old", event="pull_request", status="queued", number=68):
    return dict(id=id, head_sha=sha, event=event, status=status,
                head_branch="shared-name", pull_requests=[dict(number=number)])


def pr(sha="current", state="open"):
    return dict(head=dict(sha=sha), state=state)


class SelectionTests(TestCase):
    def test_all_active_states_and_closed_pr(self):
        for status in ("queued", "in_progress", "pending", "waiting",
                       "requested", "action_required"):
            self.assertTrue(cancel.obsolete(run(status=status), pr(), 68, 99))
            self.assertTrue(cancel.obsolete(
                run(sha="current", status=status), pr(state="closed"), 68, 99
            ))

    def test_current_head_completed_self_and_other_pr_are_preserved(self):
        for candidate in (run(sha="current"), run(status="completed"),
                          run(id=99), run(number=69)):
            self.assertFalse(cancel.obsolete(candidate, pr(), 68, 99))
        candidate = run()
        candidate["pull_requests"] = []
        self.assertFalse(cancel.obsolete(candidate, pr(), 68, 99))

    def test_push_queue_dispatch_schedule_and_cleanup_are_never_cancelled(self):
        for event in ("push", "merge_group", "schedule", "workflow_dispatch",
                      "pull_request_target", "workflow_run"):
            self.assertFalse(cancel.obsolete(
                run(event=event), pr(state="closed"), 68, 99
            ))

    def test_delayed_event_cannot_kill_newer_head(self):
        # Event SHA is deliberately not an argument to the policy.
        self.assertFalse(cancel.obsolete(run(sha="newer"), pr("newer"), 68, 99))


class ApiTests(TestCase):
    def test_paginate_beyond_first_hundred(self):
        api = cancel.GitHub("ScriptedAlchemy/blaze", "unused")
        with mock.patch.object(api, "request", side_effect=[
            {"workflow_runs": [run(id=i) for i in range(100)]},
            {"workflow_runs": [run(id=100)]},
            {"workflow_runs": []},
            {"workflow_runs": []},
            {"workflow_runs": []},
            {"workflow_runs": []},
            {"workflow_runs": []},
        ]) as request:
            self.assertEqual(len(list(api.runs())), 101)
            self.assertIn("event=pull_request", request.call_args_list[0].args[0])
            self.assertIn("page=2", request.call_args_list[1].args[0])

    def test_deduplicates_runs_that_change_state(self):
        api = cancel.GitHub("ScriptedAlchemy/blaze", "unused")
        with mock.patch.object(api, "request", side_effect=[
            {"workflow_runs": [run()]},
            {"workflow_runs": [run(status="in_progress")]},
            {"workflow_runs": []},
            {"workflow_runs": []},
            {"workflow_runs": []},
            {"workflow_runs": []},
        ]):
            self.assertEqual(len(list(api.runs())), 1)

    def test_refreshes_live_head_for_each_candidate(self):
        api = mock.Mock()
        api.runs.return_value = [run(id=1), run(id=2, sha="newer")]
        api.request.side_effect = [pr(), run(id=1), pr(), None, pr("newer")]
        self.assertEqual(cancel.cancel_obsolete(api, 68, 99), [1])
        self.assertEqual(api.request.call_args_list[3].args,
                         ("/actions/runs/1/force-cancel", "POST"))
        self.assertEqual(api.request.call_args_list[-1].args, ("/pulls/68",))

    def test_reopened_pr_preserves_current_head(self):
        api = mock.Mock()
        api.runs.return_value = [run(sha="current")]
        api.request.return_value = pr()
        self.assertEqual(cancel.cancel_obsolete(api, 68, 99), [])
        api.request.assert_called_once_with("/pulls/68")

    def test_completed_run_is_rechecked(self):
        api = mock.Mock()
        api.runs.return_value = [run()]
        api.request.side_effect = [pr(), run(status="completed"), pr()]
        self.assertEqual(cancel.cancel_obsolete(api, 68, 99), [])
        self.assertEqual(api.request.call_count, 3)

    def test_head_rollback_while_fetching_run_is_preserved(self):
        api = mock.Mock()
        api.runs.return_value = [run()]
        api.request.side_effect = [pr(), run(), pr("old")]
        self.assertEqual(cancel.cancel_obsolete(api, 68, 99), [])

    def test_only_completed_conflict_is_tolerated(self):
        for code, status in ((409, "completed"), (409, "queued"), (403, "queued")):
            api = mock.Mock()
            api.runs.return_value = [run()]
            error = HTTPError("https://example.test", code, "error", {}, None)
            api.request.side_effect = [pr(), run(), pr(), error, run(status=status)]
            if code == 409 and status == "completed":
                self.assertEqual(cancel.cancel_obsolete(api, 68, 99), [])
            else:
                with self.assertRaises(HTTPError):
                    cancel.cancel_obsolete(api, 68, 99)


class WorkflowTests(TestCase):
    def test_every_pr_entry_point_has_concurrency(self):
        root = Path(__file__).resolve().parents[1] / "workflows"
        for path in root.glob("*.yaml"):
            text = path.read_text()
            if "\n  pull_request:" in text:
                with self.subTest(path=path.name):
                    self.assertIn("\nconcurrency:", text)
                    self.assertIn("cancel-in-progress:", text)

    def test_privileged_cleanup_checks_out_only_base(self):
        text = (Path(__file__).resolve().parents[1] / "workflows"
                / "cancel-pr-runs.yaml").read_text()
        self.assertIn("pull_request_target:", text)
        self.assertIn("ref: ${{ github.event.pull_request.base.sha }}", text)
        self.assertNotIn("pull_request.head", text)
        self.assertIn("persist-credentials: false", text)


if __name__ == "__main__":
    main()
