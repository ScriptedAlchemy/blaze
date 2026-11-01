#!/usr/bin/env python3
"""The verdict on a `uitest.py --json` run: every test that ran has to match rustc, except the
ones named in `ui-known.txt` (one test per line, `#` for why), which differ in a way that is
understood. A test that is listed and now matches is said too, so the list shrinks. A test
followed by backends (`panics/x.rs llvm  # why`) is known on those alone, the one
`RA_JIT_BACKEND` names.

usage: uigate.py results.json
"""
import collections
import json
import os
import sys
from pathlib import Path

KNOWN = Path(__file__).with_name("ui-known.txt")


OUTCOMES = {"match", "excluded", "mismatch", "unsteady", "crash", "hang", "link"}


def verdict(results, known):
    """Allow only documented runtime mismatches, never compiler or harness failures."""
    errors, seen = [], set()
    if not isinstance(results, list) or not results:
        return ["no UI comparisons were recorded"]
    executed = 0
    for row in results:
        if not isinstance(row, dict):
            errors.append("invalid UI result record")
            continue
        name, outcome, revision = row.get("test"), row.get("outcome"), row.get("revision")
        if (not isinstance(name, str) or not name
                or not isinstance(outcome, str) or outcome not in OUTCOMES
                or revision is not None and not isinstance(revision, str)):
            errors.append("invalid UI test, revision or outcome")
            continue
        key = (name, revision or "")
        if key in seen:
            errors.append(f"duplicate UI result: {key}")
        seen.add(key)
        if outcome == "excluded":
            continue
        executed += 1
        if outcome == "match":
            continue
        # 'crash' is the compiler crashing; a known program-level difference
        # (including issue-28950's runtime SIGSEGV) is recorded as 'mismatch'.
        if outcome != "mismatch" or name not in known:
            errors.append(f"{outcome}: {name}" + (f" [{revision}]" if revision else ""))
    if not executed:
        errors.append("no UI comparisons were executed")
    return errors


def main() -> None:
    try:
        with open(sys.argv[1]) as file:
            results = json.load(file)
    except (OSError, ValueError) as error:
        print(f"invalid UI results: {error}")
        sys.exit(1)
    backend = os.environ.get("RA_JIT_BACKEND", "cranelift")
    entries = [line.split("#")[0].split() for line in KNOWN.read_text().splitlines()]
    known = {test for test, *on in filter(None, entries) if not on or backend in on}
    errors = verdict(results, known)
    if errors:
        for error in errors:
            print(f"UI gate: {error}")
        sys.exit(1)
    counts = collections.Counter(r["outcome"] for r in results)
    print("tests/ui run-pass:", ", ".join(f"{n} {k}" for k, n in sorted(counts.items())))
    for test in sorted(known):
        rows = [r for r in results if r["test"] == test]
        if rows and all(r["outcome"] == "match" for r in rows):
            print(f"known difference now matches (remove from ui-known.txt): {test}")


if __name__ == "__main__":
    main()
