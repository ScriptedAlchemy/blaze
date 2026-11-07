#!/usr/bin/env python3
"""Retire only obsolete pull_request runs; never run code from a PR head."""
import json
import os
import re
from urllib.error import HTTPError
from urllib.request import Request, urlopen


class GitHub:
    def __init__(self, repository, token):
        if not re.fullmatch(r"[\w.-]+/[\w.-]+", repository):
            raise ValueError("invalid repository")
        self.root = f"https://api.github.com/repos/{repository}"
        self.token = token

    def request(self, path, method="GET"):
        req = Request(self.root + path, method=method, headers={
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        })
        with urlopen(req, timeout=30) as response:
            body = response.read()
            return json.loads(body) if body else None

    def runs(self):
        # Do not walk the entire completed-run history on every push. Deduplicate
        # runs moving between states while these paginated requests are made.
        seen = set()
        for status in ("queued", "in_progress", "waiting", "pending",
                       "requested", "action_required"):
            page = 1
            while True:
                runs = self.request(
                    f"/actions/runs?event=pull_request&status={status}"
                    f"&per_page=100&page={page}"
                )["workflow_runs"]
                for run in runs:
                    if run["id"] not in seen:
                        seen.add(run["id"])
                        yield run
                if len(runs) < 100:
                    break
                page += 1


def obsolete(run, pr, number, self_id):
    # Branch names and SHAs alone are not PR identity: forks can reuse both.
    # Missing PR association is skipped, not guessed. Only the live head counts,
    # never the potentially stale synchronize/closed webhook payload.
    return (
        run["id"] != self_id
        and run["event"] == "pull_request"
        and run["status"] != "completed"
        and any(p["number"] == number for p in run.get("pull_requests", []))
        and (pr["state"] == "closed" or run["head_sha"] != pr["head"]["sha"])
    )


def cancel_obsolete(api, number, self_id):
    cancelled = []
    # Snapshot pagination before mutating run status, otherwise cancellations
    # shift later pages and skip active runs.
    for candidate in list(api.runs()):
        if (candidate["event"] != "pull_request"
                or candidate["id"] == self_id
                or candidate["status"] == "completed"
                or not any(p["number"] == number
                           for p in candidate.get("pull_requests", []))):
            continue
        # Refresh before every cancellation; a delayed webhook, reopened PR,
        # head rollback, completed run, or newer push must not kill valid work.
        pr = api.request(f"/pulls/{number}")
        if not obsolete(candidate, pr, number, self_id):
            continue
        run = api.request(f"/actions/runs/{candidate['id']}")
        pr = api.request(f"/pulls/{number}")
        if not obsolete(run, pr, number, self_id):
            continue
        try:
            api.request(f"/actions/runs/{run['id']}/force-cancel", "POST")
        except HTTPError as error:
            # GitHub returns 409 when the run finished between GET and POST.
            # Do not hide permissions failures or a genuinely stuck active run.
            if error.code != 409 or api.request(
                f"/actions/runs/{run['id']}"
            )["status"] != "completed":
                raise
            continue
        print(f"Force-cancelled obsolete PR #{number} run {run['id']}", flush=True)
        cancelled.append(run["id"])
    return cancelled


if __name__ == "__main__":
    cancel_obsolete(
        GitHub(os.environ["GITHUB_REPOSITORY"], os.environ["GH_TOKEN"]),
        int(os.environ["PR_NUMBER"]),
        int(os.environ["GITHUB_RUN_ID"]),
    )
