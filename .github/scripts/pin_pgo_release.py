#!/usr/bin/env python3
"""Bind one full, immutable PGO training run to the release that actually hosts its blobs.

build_release.py --pin retains the previous profile-release URL, which may be an
upstream repository. A fork's PGO workflow uploads to its OWN GitHub release.
Before publishing a profile-pin PR, verify every target comes from this run and
switch the global URL only when the entire target set has been retrained.
"""

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

TARGETS = frozenset(("aarch64-apple-darwin", "x86_64-unknown-linux-gnu",
                     "aarch64-unknown-linux-gnu"))


class PinError(Exception):
    pass


def bind_release(repo: str, trained_sha: str, artifacts: Path, pins_path: Path) -> bool:
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise PinError("invalid release repository")
    if not re.fullmatch(r"[0-9a-f]{40}", trained_sha):
        raise PinError("invalid training revision")
    data = json.loads(pins_path.read_text(encoding="utf-8"))
    if not isinstance(data, dict) or not isinstance(data.get("targets"), dict):
        raise PinError("invalid profile pin table")
    actual = {}
    for folder in sorted(artifacts.iterdir()):
        if not folder.is_dir() or not folder.name.startswith("pgo-"):
            raise PinError("unexpected PGO artifact entry")
        descriptors = sorted(folder.glob("*.json"))
        if len(descriptors) != 1:
            raise PinError(f"expected one training manifest in {folder.name}")
        info = json.loads(descriptors[0].read_text(encoding="utf-8"))
        target = info.get("target")
        if target not in TARGETS or folder.name != f"pgo-{target}" or target in actual:
            raise PinError("unknown, duplicate, or incorrectly located PGO target")
        if info.get("commit") != trained_sha:
            raise PinError(f"{target} was trained at a different revision")
        name = info.get("name")
        sha256 = info.get("sha256")
        if (not isinstance(name, str) or Path(name).name != name or not name.endswith(".profdata")
                or not isinstance(sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", sha256)
                or not name.endswith(f"-{sha256[:16]}.profdata")):
            raise PinError(f"invalid content-addressed PGO artifact for {target}")
        blob = folder / name
        if not blob.is_file() or hashlib.sha256(blob.read_bytes()).hexdigest() != sha256:
            raise PinError(f"profile bytes do not match training manifest for {target}")
        actual[target] = {key: val for key, val in info.items() if key != "target"}
    if actual.keys() != TARGETS or data["targets"] != actual:
        raise PinError("incomplete or mismatched PGO target pins; cannot switch release URL")
    release = f"https://github.com/{repo}/releases/download/pgo-profiles"
    if data.get("release") == release:
        return False
    data["release"] = release
    pins_path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    return True


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", required=True)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--pins", type=Path, default=Path("pgo/profiles.json"))
    args = parser.parse_args()
    try:
        changed = bind_release(args.repo, args.sha, args.artifacts, args.pins)
    except (PinError, OSError, ValueError, TypeError, KeyError) as exc:
        print(f"pgo release pinning: {exc}", file=sys.stderr)
        sys.exit(1)
    print("PGO release URL bound to this repository" if changed else "PGO release URL already correct")
