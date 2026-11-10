#!/usr/bin/env python3
"""Bind the distributed acceptance binaries to the source tree that built them.

One typed JSON input, inline, a file, or stdin (`-`):
    {"command": "seal" | "verify", "binaries": "/path/to/bin", "services": false}

The build job seals the Blaze pair before uploading it. The acceptance job verifies
that handoff before running either program; after that Blaze builds the coordinator
and cache server, `services: true` seals and verifies all four programs. This records
build provenance, not a successful distributed run.
"""
from __future__ import annotations

from dataclasses import dataclass, fields
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Literal

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = "blaze-build.json"


class Refused(ValueError):
    pass


@dataclass(frozen=True)
class Input:
    command: Literal["seal", "verify"]
    binaries: str
    services: bool = False


def load(text: str) -> Input:
    text = sys.stdin.read() if text == "-" else (
        text if text.lstrip().startswith("{") else Path(text).read_text())
    raw = json.loads(text)
    if not isinstance(raw, dict):
        raise Refused("input must be a JSON object")
    unknown = sorted(set(raw) - {field.name for field in fields(Input)})
    if unknown:
        raise Refused(f"unknown input fields: {', '.join(unknown)}")
    given = Input(**raw)
    if given.command not in ("seal", "verify"):
        raise Refused("command must be seal or verify")
    if not isinstance(given.binaries, str) or not given.binaries:
        raise Refused("binaries must name the build's directory")
    if type(given.services) is not bool:
        raise Refused("services must be boolean")
    return given


def source() -> dict[str, str]:
    def git(*args: str) -> str:
        return subprocess.check_output(
            ["git", "-C", str(ROOT), *args], text=True, timeout=30).strip()

    if git("status", "--porcelain", "--untracked-files=all"):
        raise Refused("source checkout has changes or untracked files; binaries need a clean named tree")
    return {"commit": git("rev-parse", "HEAD"), "tree": git("rev-parse", "HEAD^{tree}")}


def binaries(directory: Path, services: bool) -> dict[str, str]:
    names = ["blaze", "blaze-engine"]
    if services:
        names += ["blaze-dist-local", "blaze-serve"]
    suffix = ".exe" if sys.platform == "win32" else ""
    result = {}
    for stem in names:
        name = stem + suffix
        path = directory / name
        if path.is_symlink() or not path.is_file() or not os.access(path, os.X_OK):
            raise Refused(f"{path} is not a regular executable from the build")
        with path.open("rb") as file:
            result[name] = hashlib.file_digest(file, "sha256").hexdigest()
    return result


def identity(directory: Path, services: bool) -> dict:
    return {"schema": 1, "source": source(), "binaries": binaries(directory, services)}


def verify(record: object, actual: dict) -> None:
    if not isinstance(record, dict) or set(record) != {"schema", "source", "binaries"}:
        raise Refused("build identity must contain exactly schema, source and binaries")
    if type(record["schema"]) is not int or record["schema"] != 1:
        raise Refused("build identity has an unsupported schema")
    if record["source"] != actual["source"]:
        raise Refused(f"build source {record['source']!r} differs from checkout {actual['source']!r}")
    if record["binaries"] != actual["binaries"]:
        raise Refused("build binaries differ from their recorded SHA-256 identities or executable set")


def main(given: Input) -> None:
    directory = Path(given.binaries).resolve(strict=True)
    actual = identity(directory, given.services)
    manifest = directory / MANIFEST
    if given.command == "seal":
        manifest.write_text(json.dumps(actual, indent=2, sort_keys=True) + "\n")
    else:
        verify(json.loads(manifest.read_text()), actual)
    print(f"dist build identity: {given.command} {len(actual['binaries'])} binaries at "
          f"{actual['source']['commit']} (tree {actual['source']['tree']})")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        main(load(sys.argv[1]))
    except (OSError, ValueError, TypeError, subprocess.SubprocessError) as error:
        sys.exit(f"dist build identity: {error}")
