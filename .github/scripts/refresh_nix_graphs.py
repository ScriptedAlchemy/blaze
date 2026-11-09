#!/usr/bin/env python3
"""Export a reviewable Nix graph refresh without changing the checked-out source.

Takes one JSON value, inline, from a file, or from stdin ("-"):
    {"out": "/absolute/path/to/new/artifact-directory"}

This is a metadata operation, not a compiler bootstrap or a validation bypass.
The normal locked graph check must still pass after the exported files are committed.
"""
from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
from typing import Callable

ROOT = Path(__file__).resolve().parents[2]
FILES = ("demo/life/Cargo.lock", "tools/nix/units/daemon.json", "tools/nix/units/life.json")
SYSTEMS = {"x86_64-linux", "aarch64-linux", "aarch64-darwin", "x86_64-darwin"}
Run = Callable[[list[str], Path], bytes]


@dataclass(frozen=True)
class Request:
    out: Path

    @classmethod
    def parse(cls, text: str) -> Request:
        value = json.loads(text)
        if not isinstance(value, dict) or set(value) != {"out"}:
            raise ValueError('expected exactly {"out": "/absolute/new/directory"}')
        out = value["out"]
        if not isinstance(out, str) or not out or not Path(out).is_absolute():
            raise ValueError("out must be an absolute directory path")
        return cls(Path(out))


def run(command: list[str], cwd: Path) -> bytes:
    return subprocess.run(command, cwd=cwd, check=True, stdout=subprocess.PIPE, timeout=3600).stdout


def checked_graphs(work: Path) -> None:
    for project, directory in (("daemon", work), ("life", work / "demo/life")):
        graph = json.loads((work / f"tools/nix/units/{project}.json").read_text())
        if graph.get("version") != 4 or set(graph.get("systems", {})) != SYSTEMS:
            raise ValueError(f"{project}: incomplete generated platform graph")
        for name, filename in (("lock", "Cargo.lock"), ("manifest", "Cargo.toml")):
            if graph.get(name) != tomllib.loads((directory / filename).read_text()):
                raise ValueError(f"{project}: generated {name} does not match its source")
        if not graph.get("packages") or any(
            not system.get("units") or not system.get("roots")
            for system in graph["systems"].values()
        ):
            raise ValueError(f"{project}: generated graph has no build units or roots")


def refresh(root: Path, request: Request, execute: Run = run) -> None:
    root = root.resolve()
    out = request.out.resolve()
    if out == root or root in out.parents or out.exists():
        raise ValueError("out must be a new directory outside the checkout")
    source = execute(["git", "rev-parse", "HEAD"], root).decode().strip()
    tree = execute(["git", "rev-parse", "HEAD^{tree}"], root).decode().strip()
    with tempfile.TemporaryDirectory(prefix="blaze-nix-refresh-") as scratch:
        work = Path(scratch) / "source"
        execute(["git", "worktree", "add", "--detach", str(work), source], root)
        try:
            # Resolve the complete standalone graph, preserving locked versions where possible.
            # Cargo metadata does not compile; no compiler is used to bootstrap Blaze here.
            execute([
                "nix", "develop", str(work), "--command", "cargo", "metadata",
                "--format-version", "1", "--manifest-path", "demo/life/Cargo.toml",
            ], work)
            execute([sys.executable, "tools/nix/unit_graph.py"], work)
            changed = set(execute(["git", "diff", "--name-only", "-z"], work).decode().split("\0")) - {""}
            if changed - set(FILES):
                raise ValueError(f"graph refresh changed unrelated files: {sorted(changed - set(FILES))}")
            checked_graphs(work)
            patch = execute(["git", "diff", "--binary", "--", *FILES], work)
            payload = {name: (work / name).read_bytes() for name in FILES}
            receipt = {
                "source_commit": source,
                "source_tree": tree,
                "files": {name: hashlib.sha256(data).hexdigest() for name, data in payload.items()},
                "changed": sorted(changed),
                "validation": "metadata regenerated; locked source gate and builds still required",
            }
        finally:
            execute(["git", "worktree", "remove", "--force", str(work)], root)
    out.mkdir(parents=True)
    for name, data in payload.items():
        target = out / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    (out / "refresh.patch").write_bytes(patch)
    # The receipt is last: consumers must not accept a partial export.
    (out / "source.json").write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
    print(f"exported Nix metadata for {source} to {out}; commit and revalidate the locked source gate")


def main() -> None:
    if len(sys.argv) != 2:
        raise ValueError("supply one JSON value, file path, or -")
    arg = sys.argv[1]
    text = sys.stdin.read() if arg == "-" else arg if arg.lstrip().startswith("{") else Path(arg).read_text()
    refresh(ROOT, Request.parse(text))


if __name__ == "__main__":
    main()
