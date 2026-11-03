#!/usr/bin/env bash
# benchrunner.sh: makes this machine the self-hosted runner of indexable-inc/blaze (or of
# BENCH_REPOSITORY) that the bench-readme and bench-bun workflows run one platform on
# (harness/benchplatforms.py):
#
#   linux-x86-64    label bench-x86
#   linux-aarch64   label bench-arm64
#   macos-arm64     label bench-macos
#
# Only the platform's label, and no default labels (--no-default-labels): a job of a fork's pull
# request that asks for `self-hosted`, `linux`, `x64` or `ARM64` must not reach this machine.
# The platform's jobs ask for its label through the matrix of harness/benchplatforms.py.
#
# Before registering, the repository is checked against the GitHub API: a public repository (this
# one is) hands a fork's pull request workflow the choice of `runs-on`, and GitHub's fork-PR
# approval is then the only thing between that workflow and this machine. So a public repository
# must have approval for all external contributors (`all_external_contributors`, Settings →
# Actions → "Fork pull request workflows"); anything else stops here. The check reads the API as
# this machine's `gh` (gh auth login), or with GH_TOKEN/GITHUB_TOKEN, and needs to read the
# repository's Actions settings. BENCH_ALLOW_PUBLIC_REPOSITORY=1 skips the check: do that only
# after verifying by hand that the repository is private or approves every external contributor.
# BENCH_REPOSITORY (default indexable-inc/blaze) is the repository, for the check and for the URL
# the runner is registered to; the registration token must come from that repository too.
#
#   scp harness/benchrunner.sh HOST:
#   gh api -X POST repos/indexable-inc/blaze/actions/runners/registration-token --jq .token \
#     | ssh HOST bash benchrunner.sh linux-aarch64 "'AWS m8g.4xlarge'"
#
# Reads the registration token (valid one hour, needs repo admin) from stdin; the arguments are
# the platform (the machine's own, checked) and the name of the machine for the results
# (BLAZE_BENCH_MACHINE). One argument is the name, on linux-x86-64, as before there were
# platforms. Installs the runner in ~/actions-runner and runs it at boot: on Linux from a user
# systemd unit, blaze-bench-runner.service, that lingers; on macOS as the runner's own launchd
# service (svc.sh), which needs the user logged in, and the Mac kept from sleeping.
# Needs curl, tar and Nix with flakes; on NixOS the runner's own binaries run through nix-ld.
# Run again to re-register (--replace keeps the name). Then add the platform to the repository
# variable BENCH_RUNNERS, which turns its jobs on.
set -euo pipefail
export PATH=/run/current-system/sw/bin:/run/wrappers/bin:/nix/var/nix/profiles/default/bin:$PATH
if [ $# -eq 1 ]; then set -- linux-x86-64 "$1"; fi
platform=${1:?name the platform: linux-x86-64, linux-aarch64 or macos-arm64}
machine=${2:?name the machine, e.g. "AWS m8g.4xlarge"}
case "$platform/$(uname -s)/$(uname -m)" in
  linux-x86-64/Linux/x86_64) label=bench-x86 arch=linux-x64 ;;
  linux-aarch64/Linux/aarch64) label=bench-arm64 arch=linux-arm64 ;;
  macos-arm64/Darwin/arm64) label=bench-macos arch=osx-arm64 ;;
  *) echo "this machine is $(uname -s) $(uname -m), not $platform" >&2; exit 1 ;;
esac
repo=${BENCH_REPOSITORY:-indexable-inc/blaze}
read -r token
[ -n "$token" ] || { echo "no registration token on stdin" >&2; exit 1; }

stop() { echo "benchrunner: $*" >&2; exit 1; }

# The GitHub API as this machine's gh (gh auth login), or with GH_TOKEN/GITHUB_TOKEN, else
# anonymously, which is enough for a public repository's own metadata but not for its Actions
# policies: the preflight needs either.
api() { # api PATH
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    gh api "$1"
  elif [ -n "${GH_TOKEN:-}${GITHUB_TOKEN:-}" ]; then
    curl -fsSL -H "Accept: application/vnd.github+json" \
      -H "Authorization: Bearer ${GH_TOKEN:-$GITHUB_TOKEN}" "https://api.github.com/$1"
  else
    curl -fsSL -H "Accept: application/vnd.github+json" "https://api.github.com/$1"
  fi
}

# A string field of the API's flat JSON.
field() { sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p"; }

# The check that makes a public repository safe to register to: on a public repository a fork's
# pull request runs the workflow from the pull request, `runs-on` and all, and GitHub's fork-PR
# approval is then the only thing that keeps that workflow off this machine. A repository is
# safe when it is private, or when it approves every external contributor's workflow before it
# runs. Anything this cannot read stops here, rather than register on a guess.
preflight() {
  hint="Run this where gh is signed in (gh auth login) or with GH_TOKEN/GITHUB_TOKEN that can read the repository's Actions settings. Nothing was registered. BENCH_ALLOW_PUBLIC_REPOSITORY=1 overrides this, after checking by hand."
  if [ "${BENCH_ALLOW_PUBLIC_REPOSITORY:-}" = 1 ]; then
    echo "benchrunner: BENCH_ALLOW_PUBLIC_REPOSITORY=1: not checking that $repo keeps a fork's pull request off this machine" >&2
    return
  fi
  repository=$(api "repos/$repo" 2>/dev/null) || repository=
  visibility=$(printf '%s' "$repository" | field visibility)
  case "$visibility" in
    private | internal)
      echo "benchrunner: $repo is $visibility: a fork's pull request cannot run a workflow on this machine" >&2
      return
      ;;
    public) ;;
    *) stop "cannot read $repo from the GitHub API, so cannot tell whether a fork's pull request could run a workflow on this machine. $hint" ;;
  esac
  policy=$(api "repos/$repo/actions/permissions/fork-pr-contributor-approval" 2>/dev/null) || policy=
  policy=$(printf '%s' "$policy" | field approval_policy)
  [ -n "$policy" ] || stop "cannot read $repo's fork-PR approval policy, and it is public, so cannot tell whether a fork's pull request could run its own workflow, and so choose this machine. $hint"
  [ "$policy" = all_external_contributors ] || stop "$repo is public and its fork-PR approval policy is '$policy', not all_external_contributors: a fork's pull request could run its own workflow, and so choose this machine, before anyone approves it. On a public repository the only safe setting is Settings -> Actions -> 'Require approval for all external contributors'. $hint"
  echo "benchrunner: $repo is public and requires approval from every external contributor, so a fork's pull request cannot choose this machine" >&2
}
preflight

dir=$HOME/actions-runner
version=$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p')
mkdir -p "$dir"
cd "$dir"
if [ ! -x ./run.sh ] || [ "$(cat .version 2>/dev/null)" != "$version" ]; then
  curl -fsSL "https://github.com/actions/runner/releases/download/v$version/actions-runner-$arch-$version.tar.gz" | tar xz
  echo "$version" > .version
fi
# The runner is .NET: without ICU on the host it has to be told to do without.
export DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
# Only the platform's label, and no default labels: an unlabelled job, or one that asks for
# `self-hosted` or the machine's own OS and architecture, must not match a benchmark machine.
./config.sh --unattended --replace --url "https://github.com/$repo" --token "$token" \
  --name "$(hostname -s)" --no-default-labels --labels "$label" --work _work
cat > .env <<EOF
BLAZE_BENCH_MACHINE=$machine
DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
EOF
if [ "$(uname -s)" = Darwin ]; then
  # The service's PATH is the runner's .path file: Nix's profile first, for `nix develop`.
  echo "$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" > .path
  ./svc.sh install
  ./svc.sh start
  ./svc.sh status | head -5
  exit 0
fi
mkdir -p "$HOME/.config/systemd/user"
cat > "$HOME/.config/systemd/user/blaze-bench-runner.service" <<EOF
[Unit]
Description=GitHub Actions runner for blaze benchmarks ($label)
After=network-online.target

[Service]
WorkingDirectory=$dir
ExecStart=$dir/run.sh
Environment=PATH=$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:/run/wrappers/bin
Environment=DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now blaze-bench-runner.service
loginctl enable-linger "$USER" 2>/dev/null || sudo loginctl enable-linger "$USER"
systemctl --user --no-pager status blaze-bench-runner.service | head -5
