#!/usr/bin/env python3
"""Publish one immutable PGO training run as a reviewed PR, never a direct main push.

Runs on a trusted main-branch workflow after build_release.py --pin has changed
pgo/profiles.json. The run ID is the idempotency key. The workflow's checked-out
GITHUB_SHA must still be the tip of origin/main before we publish training pins.
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path


class PublicationError(Exception):
    pass


def run(*command, allowed=(0,)):
    completed = subprocess.run(command, capture_output=True, text=True)
    if completed.returncode not in allowed:
        message = completed.stderr.strip() or completed.stdout.strip() or "no diagnostics"
        raise PublicationError(f"{command[0]} {command[1] if len(command) > 1 else ''}: {message}")
    return completed.returncode, completed.stdout.strip()


def git(*args, allowed=(0,)):
    return run("git", *args, allowed=allowed)


def gh(*args):
    return run("gh", *args)[1]


def record(message):
    print(message)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(message + "\n")


def parse_prs(data, repo):
    try:
        prs = json.loads(data)
    except json.JSONDecodeError as e:
        raise PublicationError("Cannot parse GitHub PR response") from e
    if not isinstance(prs, list) or len(prs) > 1:
        raise PublicationError("Ambiguous GitHub PR response for this training run")
    if prs:
        pr = prs[0]
        if not isinstance(pr, dict) or pr.get("state") not in {"OPEN", "CLOSED", "MERGED"}:
            raise PublicationError("Unexpected PR state")
        if pr.get("baseRefName") != "main":
            raise PublicationError("PGO PR targets an unexpected base")
        if not re.fullmatch(rf"https://github\.com/{re.escape(repo)}/pull/\d+", pr.get("url", "")):
            raise PublicationError("PGO PR has an invalid URL")
        return pr
    return None


def remote_branch(branch):
    code, out = git("ls-remote", "--exit-code", "--heads", "origin", f"refs/heads/{branch}", allowed=(0, 2))
    if code == 2:
        return False
    match = re.fullmatch(r"([a-f0-9]{40})\s+refs/heads/([\w./-]+)", out)
    if not match or match[2] != branch:
        raise PublicationError("Unexpected remote branch lookup response")
    return True


def verify_recovery(branch, trained_sha, expected_bytes):
    # Do not trust or overwrite a same-name branch that another worker published.
    git("fetch", "--no-tags", "origin", f"refs/heads/{branch}")
    result, _ = git("merge-base", "--is-ancestor", trained_sha, "FETCH_HEAD", allowed=(0, 1))
    if result != 0:
        raise PublicationError("Existing PGO branch does not descend from the training source")
    _, paths = git("diff", "--name-only", trained_sha, "FETCH_HEAD")
    if paths != "pgo/profiles.json":
        raise PublicationError("Existing PGO branch has unexpected file changes")
    content = subprocess.run(["git", "show", "FETCH_HEAD:pgo/profiles.json"],
                             capture_output=True)
    if content.returncode != 0 or content.stdout != expected_bytes:
        raise PublicationError("Existing PGO branch differs from this run's profile pins")


def publish():
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    run_id = os.environ.get("GITHUB_RUN_ID", "")
    sha = os.environ.get("GITHUB_SHA", "")
    if (not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo)
            or not re.fullmatch(r"[1-9][0-9]*", run_id)
            or not re.fullmatch(r"[0-9a-f]{40}", sha)):
        raise PublicationError("Missing or invalid GitHub repository, run ID, or source SHA")
    if os.environ.get("GITHUB_REF") != "refs/heads/main":
        raise PublicationError("PGO pins may only publish from a trusted main-branch run")
    if not os.environ.get("GH_TOKEN"):
        raise PublicationError("No GitHub token available for queue-reviewed PR publication")

    branch = f"pgo/pins-{run_id}"
    pr = parse_prs(gh("pr", "list", "--repo", repo, "--head", branch, "--state", "all",
                      "--limit", "100", "--json", "state,url,baseRefName"), repo)
    if pr:
        if pr["state"] == "CLOSED":
            raise PublicationError(f"PGO pin PR {pr['url']} was closed; refusing duplicate publication")
        if pr["state"] == "OPEN":
            if not remote_branch(branch):
                raise PublicationError("Open PGO PR has no accessible branch")
            verify_recovery(branch, sha, Path("pgo/profiles.json").read_bytes())
        record(f"PGO pin PR already {'merged' if pr['state'] == 'MERGED' else 'awaiting review'}: {pr['url']}")
        return

    if git("rev-parse", "HEAD")[1] != sha:
        raise PublicationError("Checkout does not match the source used for training")
    # Compare exactly: never rebase a trained profile onto a newer source tree.
    git("fetch", "--no-tags", "origin", "main")
    if git("rev-parse", "refs/remotes/origin/main")[1] != sha:
        raise PublicationError("main advanced since training; refusing stale profile pins")
    git("add", "--", "pgo/profiles.json")
    staged, _ = git("diff", "--cached", "--quiet", "--exit-code", "--", "pgo/profiles.json", allowed=(0, 1))
    if staged == 0:
        record("PGO pin contents unchanged; nothing to publish")
        return
    paths = git("diff", "--cached", "--name-only")[1]
    if paths != "pgo/profiles.json":
        raise PublicationError("Unexpected staged files alongside PGO pins")
    expected = Path("pgo/profiles.json").read_bytes()

    if remote_branch(branch):
        verify_recovery(branch, sha, expected)
    else:
        # Recheck immediately before pushing; an unrelated change must not make
        # the trained profiles appear current just because a PR can be rebased.
        git("fetch", "--no-tags", "origin", "main")
        if git("rev-parse", "refs/remotes/origin/main")[1] != sha:
            raise PublicationError("main advanced during PGO publication")
        git("switch", "-c", branch)
        git("-c", "user.name=github-actions[bot]",
            "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit", "-m", f"pgo: profiles trained at {sha[:10]}", "--", "pgo/profiles.json")
        # No force, no HEAD:main. A concurrent publication must be reconciled.
        git("push", "origin", f"HEAD:refs/heads/{branch}")

    # Existing-branch recovery is safe only for the exact expected pin file.
    url = gh("pr", "create", "--repo", repo, "--base", "main", "--head", branch,
             "--title", f"pgo: pin profiles from run {run_id}",
             "--body", f"Profiles trained on immutable main source `{sha}` in "
             f"https://github.com/{repo}/actions/runs/{run_id}. "
             "Land via the protected-main merge queue after all applicable checks pass. "
             "Never push directly to main.")
    if not re.fullmatch(rf"https://github\.com/{re.escape(repo)}/pull/[0-9]+", url):
        raise PublicationError("GitHub did not confirm a new PGO PR URL")
    record(f"PGO pin PR awaits review and merge: {url}")


if __name__ == "__main__":
    try:
        publish()
    except (PublicationError, OSError) as exc:
        print(f"pgo publish: {exc}", file=sys.stderr)
        sys.exit(1)
