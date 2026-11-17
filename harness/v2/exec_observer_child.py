#!/usr/bin/env python3
"""Private traced supervisor for exec_observer.py; receives its exact saved input.

Every selected command and the unconditional stop is a child of this process, so
strace follows both the daemon and its late children until they have all exited.
No pipe can stay open in a descendant and prevent writing the shutdown receipt.
"""

from __future__ import annotations

from dataclasses import asdict
import json
import os
from pathlib import Path
import subprocess
import sys

from exec_observer import Failed, Input, load


def command(given: Input, args: tuple[str, ...], log: Path, seconds: int) -> int:
    environment = dict(os.environ)
    environment["BLAZE_HOME"] = given.home
    environment["BLAZE_CACHE"] = str(Path(given.home) / "v2" / "store")
    with log.open("xb") as output:
        try:
            return subprocess.run([given.blaze, "--ignore-pin", *args], cwd=given.cwd,
                                  env=environment, stdin=subprocess.DEVNULL, stdout=output,
                                  stderr=subprocess.STDOUT, timeout=seconds, check=False).returncode
        except subprocess.TimeoutExpired:
            return 124
        except OSError as error:
            output.write(f"could not execute selected Blaze: {error}\n".encode())
            return 127


def main(given: Input) -> int:
    out = Path(given.out)
    result: dict[str, object] = {"pid": os.getpid(), "commands": [], "stop": None, "complete": False}
    rows: list[dict[str, object]] = []
    result["commands"] = rows
    status = 0
    stop = 1
    try:
        for item in given.commands:
            print(f"observe {item.name}: {json.dumps(asdict(item))}", flush=True)
            status = command(given, item.args, out / f"{item.name}.log", given.command_seconds)
            rows.append({"name": item.name, "returncode": status})
            if status != 0:
                break
    finally:
        stop = command(given, ("stop",), out / "stop.log", given.stop_seconds)
        result["stop"], result["complete"] = stop, True
        with (out / "supervisor.json").open("x", encoding="utf-8") as output:
            json.dump(result, output, sort_keys=True)
            output.write("\n")
    code = status or stop
    return code if code >= 0 else 128 - code


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        sys.exit(main(load(sys.argv[1])))
    except (Failed, OSError, ValueError) as error:
        sys.exit(f"exec_observer_child.py: {error}")
