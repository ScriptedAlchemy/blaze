#!/usr/bin/env python3
"""Which areas of checks a change runs (pr.yaml, job `changes`), from .github/ci/areas.json.

Reads the changed files, one per line, on stdin; an empty list means they could not be told,
and runs everything. A change to the CI itself (area `ci`) runs every area. Writes
`<area>=true|false`, `code` (any area) and `macos` (an area with macOS checks) to stdout.
"""
from fnmatch import fnmatchcase
import json
from pathlib import Path
import sys


def matches(path, patterns):
    return any(fnmatchcase(path, p) for p in patterns)


def main():
    areas = {k: v for k, v in json.loads(Path(".github/ci/areas.json").read_text()).items()
             if not k.startswith("_")}
    files = [line.strip() for line in sys.stdin if line.strip()]
    hit = {name: (not files) or any(matches(f, a["paths"]) and not matches(f, a.get("ignore", []))
                                     for f in files)
           for name, a in areas.items()}
    if hit.pop("ci"):
        hit = {name: True for name in hit}
    for name, value in hit.items():
        print(f"{name}={str(value).lower()}")
    print(f"code={str(any(hit.values())).lower()}")
    print(f"macos={str(any(v and areas[n].get('macos') for n, v in hit.items())).lower()}")


if __name__ == "__main__":
    main()
