#!/usr/bin/env bash
# Publish one immutable run's benchmark sample without bypassing main's required checks.
set -euo pipefail
git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git add docs/benchmarks/history/macos-arm64-live.jsonl
if git diff --cached --quiet; then
  echo "No new history line to publish"
  exit 0
fi
branch="bench/live-latency-${GITHUB_RUN_ID}"
# A merged PR's branch is normally deleted. Check PR history first, so re-running
# the same workflow cannot publish the same measurement in a second PR.
existing=$(gh pr list --repo "$GITHUB_REPOSITORY" --head "$branch" --state all --limit 100 \
  --json state,url --jq '.[0] | if . == null then empty else [.state, .url] | @tsv end')
if [ -n "$existing" ]; then
  read -r state url <<< "$existing"
  case "$state" in
    MERGED)
      printf 'Benchmark history already merged: %s\n' "$url" >> "$GITHUB_STEP_SUMMARY"
      exit 0 ;;
    OPEN)
      printf 'Benchmark history awaits review and merge: %s\n' "$url" >> "$GITHUB_STEP_SUMMARY"
      exit 0 ;;
    CLOSED)
      echo "Benchmark history PR $url was closed without merging; refusing to duplicate this run" >&2
      exit 1 ;;
    *)
      echo "Unexpected benchmark history PR state $state for $url" >&2
      exit 1 ;;
  esac
fi
# Same-run retries never force-push an existing branch or duplicate a history line.
if git ls-remote --exit-code --heads origin "refs/heads/$branch" >/dev/null; then
  echo "Preserving the existing history branch $branch"
else
  status=$?
  # Exit 2 means no match; authentication, network and server failures are not absence.
  if [ "$status" -ne 2 ]; then
    echo "Cannot verify whether $branch exists (git ls-remote exit $status)" >&2
    exit "$status"
  fi
  git switch -c "$branch"
  git commit -q -m "bench: live latency for ${GITHUB_SHA::10}"
  git push -q origin "HEAD:refs/heads/$branch"
fi
url=$(gh pr create --repo "$GITHUB_REPOSITORY" --base main --head "$branch" \
  --title "bench: live latency for ${GITHUB_SHA::10}" \
  --body "History from [live edit latency run](${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}) on ${GITHUB_SHA}. Land through the merge queue; do not push directly to main.")
printf 'Benchmark history awaits review and merge: %s\n' "$url" >> "$GITHUB_STEP_SUMMARY"
