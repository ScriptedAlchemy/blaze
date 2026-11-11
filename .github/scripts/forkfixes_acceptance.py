#!/usr/bin/env python3
"""Refuse incomplete or filtered fork migration evidence before CI issues its receipt.

One typed JSON object, inline, a file, or stdin (`-`): {"scratch": "/path/to/run"}.
The harness owns execution and provenance. This checks that its successful run covered
every case in this checkout, including each case's exact migration mapping.
"""
from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / "harness/forkfixes"), str(ROOT / "tools/typed-input/src")]
from cases import CASES  # noqa: E402
from typed_input import parse  # noqa: E402


@dataclass(frozen=True)
class Input:
    scratch: str


def verify(scratch: Path) -> int:
    report = json.loads((scratch / "run.json").read_text())
    results = json.loads((scratch / "results.json").read_text())
    if not isinstance(report, dict) or report.get("complete") is not True or report.get("errors") != []:
        raise ValueError("migration run did not complete without setup, execution, provenance or cleanup errors")
    given = report.get("input")
    if not isinstance(given, dict) or given.get("only") != []:
        raise ValueError("migration acceptance requires the full suite, with no case filter")
    expected = {case["name"]: case["fixes"] for case in CASES}
    if not isinstance(results, dict) or set(results) != set(expected):
        raise ValueError("migration results do not contain exactly this checkout's complete fixture set")
    for name, fixes in expected.items():
        result = results[name]
        if not isinstance(result, dict) or result.get("ok") is not True or result.get("fixes") != fixes:
            raise ValueError(f"migration fixture {name} did not pass for its declared fork commits")
    before = json.loads((scratch / "provenance.json").read_text())
    after = json.loads((scratch / "provenance-after.json").read_text())
    if not isinstance(before, dict) or not before or before != after:
        raise ValueError("migration source, harness or binary identity is missing or changed")
    return len(expected)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        given = parse(Input, sys.argv[1], "forkfixes_acceptance.py")
        count = verify(Path(given.scratch))
        print(f"forkfixes acceptance: all {count} cases passed with unchanged source and binaries")
    except (OSError, ValueError) as error:
        sys.exit(f"forkfixes acceptance: {error}")
