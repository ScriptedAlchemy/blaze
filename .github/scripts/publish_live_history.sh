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
# Same-run retries never force-push an existing branch or duplicate a history line.
if git ls-remote --exit-code --heads origin "refs/heads/$branch" >/dev/null; then
  echo "Preserving the existing history branch $branch"
else
  git switch -c "$branch"
  git commit -q -m "bench: live latency for ${GITHUB_SHA::10}"
  git push -q origin "HEAD:refs/heads/$branch"
fi
existing=$(gh pr list --repo "$GITHUB_REPOSITORY" --head "$branch" --state all --json url --jq '.[0].url // empty')
if [ -n "$existing" ]; then
  url="$existing"
else
  url=$(gh pr create --repo "$GITHUB_REPOSITORY" --base main --head "$branch" \
    --title "bench: live latency for ${GITHUB_SHA::10}" \
    --body "History from [live edit latency run](${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}) on ${GITHUB_SHA}. Land through the merge queue; do not push directly to main.")
fi
printf 'Benchmark history awaits review and merge: %s\n' "$url" >> "$GITHUB_STEP_SUMMARY"
