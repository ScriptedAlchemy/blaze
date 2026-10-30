#!/usr/bin/env python3
"""check_nix_workflow.py: the real workflow passes; weakening it fails.

  python3 -B harness/dist/test_check_nix_workflow.py

Every case mutates the workflow text itself, so the tests run against what CI reads, not a
fixture written to agree with the checker. The last case is the one that matters: a required
command deleted and replaced with `echo` must fail, or "the gate is still there" is a claim
the checker cannot back.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_nix_workflow as c  # noqa: E402

WORKFLOW = c.WORKFLOW
FAILED = []


def case(name, text, expect_ok, *, must_mention=None):
    try:
        problems = c.check(text)
    except c.Bad as e:
        problems = [f"cannot check: {e}"]
    ok = not problems
    if ok != expect_ok:
        FAILED.append(f"{name}: expected {'pass' if expect_ok else 'fail'}, got {problems or 'pass'}")
        return
    if must_mention and not any(must_mention in p for p in problems):
        FAILED.append(f"{name}: no problem mentions {must_mention!r}; got {problems}")
        return
    print(f"ok   {name}" + (f" -> {problems[0][:90]}" if problems else ""))


def main() -> int:
    text = WORKFLOW.read_text()
    case("the real workflow passes", text, True)

    # The coordinator's mutation: a gate deleted, replaced with a command that always succeeds.
    case(
        "nix flake check replaced with echo",
        text.replace("run: nix flake check --print-build-logs", "run: echo skipped", 1),
        False,
        must_mention="nix flake check",
    )
    case(
        "unit-graph regeneration deleted",
        text.replace("      - run: python3 nix/unit_graph.py\n", "", 1),
        False,
        must_mention="python3 nix/unit_graph.py",
    )
    case(
        "unit-graph freshness diff deleted",
        text.replace(
            '          git diff --exit-code -- nix/units || { echo "::error::nix/units is stale: run nix/unit_graph.py and commit the result"; exit 1; }\n',
            "",
            1,
        ),
        False,
        must_mention="git diff --exit-code -- nix/units",
    )
    case(
        "darwin build replaced with echo",
        text.replace(
            "        run: nix build --print-build-logs --no-link .#daemon-units .#run",
            "        run: echo skipped",
            1,
        ),
        False,
        must_mention="nix build --print-build-logs --no-link .#daemon-units",
    )
    case(
        "live-edit run deleted",
        text.replace('        run: RA_JIT_BIN="$GITHUB_WORKSPACE/target/release/ra-jit" python3 harness/live_e2e.py\n', "", 1),
        False,
        must_mention="harness/live_e2e.py",
    )
    case(
        "a whole required job deleted",
        text.replace("  nix-darwin:\n", "  nix-darwin-gone:\n", 1),
        False,
        must_mention="nix-darwin",
    )

    # A gate that stays but is no longer required.
    case(
        "required step made conditional",
        text.replace(
            "      - name: flake evaluates\n",
            "      - name: flake evaluates\n        if: false\n",
            1,
        ),
        False,
        must_mention="is conditional",
    )
    case(
        "required step allowed to fail",
        text.replace(
            "      - name: flake evaluates\n",
            "      - name: flake evaluates\n        continue-on-error: true\n",
            1,
        ),
        False,
        must_mention="may fail",
    )
    case(
        "required job allowed to fail",
        text.replace("  nix:\n    name:", "  nix:\n    continue-on-error: true\n    name:", 1),
        False,
        must_mention="the job may fail",
    )

    # The cache itself: optional, and never able to take a gate with it.
    case(
        "cache step un-gated",
        text.replace("        if: vars.ENABLE_FLAKEHUB_CACHE == 'true'\n", "", 1),
        False,
        must_mention="ENABLE_FLAKEHUB_CACHE",
    )
    case(
        "cache step allowed to fail a job",
        text.replace("        continue-on-error: true\n", "", 1),
        False,
        must_mention="not literally true",
    )

    # The opt-in predicate and the failure policy are effective values, not identifier
    # spellings: a bare name, an always-true expression and `continue-on-error: false` all
    # leave the cache ungated or able to fail the job, so all three must fail.
    case(
        "cache gate reduced to the identifier as a name",
        text.replace("        if: vars.ENABLE_FLAKEHUB_CACHE == 'true'", "        name: ENABLE_FLAKEHUB_CACHE", 1),
        False,
        must_mention="not gated on ENABLE_FLAKEHUB_CACHE",
    )
    case(
        "cache gate broadened with always()",
        text.replace(
            "        if: vars.ENABLE_FLAKEHUB_CACHE == 'true'",
            "        if: always() || vars.ENABLE_FLAKEHUB_CACHE == 'true'",
            1,
        ),
        False,
        must_mention="not the supported opt-in predicate",
    )
    case(
        "cache failure policy turned off with false",
        text.replace("        continue-on-error: true", "        continue-on-error: false", 1),
        False,
        must_mention="not literally true",
    )
    case(
        "cache policy as an interpolated expression",
        text.replace(
            "        if: vars.ENABLE_FLAKEHUB_CACHE == 'true'",
            "        if: ${{ vars.ENABLE_FLAKEHUB_CACHE == 'true' }}",
            1,
        ),
        True,
    )

    # Consumers must not evict the shared build cache with full store/target snapshots.
    for action in c.ACTIONS_CACHE_ACTIONS:
        case(
            f"consumer Actions cache rejected: {action}",
            text.replace("      - uses: actions/checkout@v7", f"      - uses: {action}@v1\n      - uses: actions/checkout@v7", 1),
            False,
            must_mention="outside the bundle producer",
        )
    case(
        "consumer composite cannot allocate a cache",
        text.replace("      - uses: actions/checkout@v7", "      - uses: ./.github/actions/toolchain\n        with:\n          cache: nix sccache\n      - uses: actions/checkout@v7", 1),
        False,
        must_mention="cache allocation belongs to the bundle producer",
    )

    # A gate that is only mentioned is not a gate. The argument keeps the command's exact
    # spelling, so a substring check would pass it.
    case(
        "gate echoed with its exact spelling",
        text.replace(
            "run: nix flake check --print-build-logs",
            'run: echo "nix flake check --print-build-logs"',
            1,
        ),
        False,
        must_mention="only mentioned, not run",
    )
    case(
        "gate spelled in a comment",
        text.replace(
            "run: nix flake check --print-build-logs",
            "run: |\n          # nix flake check --print-build-logs\n          true",
            1,
        ),
        False,
        must_mention="only mentioned, not run",
    )
    case(
        "gate passed to a different command as an argument",
        text.replace(
            "run: nix flake check --print-build-logs",
            "run: printf '%s\\n' 'nix flake check --print-build-logs'",
            1,
        ),
        False,
        must_mention="only mentioned, not run",
    )

    # A shape the scanner cannot read is refused rather than guessed at. Unknown job keys are
    # tolerated (they cannot delete a command); a step that is not a list entry is not.
    case(
        "unreadable step shape is refused",
        text.replace("      - name: flake evaluates\n", "      name: flake evaluates\n", 1),
        False,
        must_mention="cannot check",
    )

    if FAILED:
        for f in FAILED:
            print(f"FAIL {f}", file=sys.stderr)
        print(f"{len(FAILED)} failed", file=sys.stderr)
        return 1
    print("all cases passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
