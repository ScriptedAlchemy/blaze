#!/usr/bin/env python3
"""Private traced supervisor for exec_observer.py; receives its exact saved input.

Every selected command is a child of this process. strace follows all its descendants,
including detached helpers and their late children, until they have all exited. A
receipt records command outcomes only; complete tree termination is the observer's job.
"""

from __future__ import annotations

from dataclasses import asdict
import json
import os
from pathlib import Path
import subprocess
import sys

from exec_observer import SCHEMA, SCOPE, Command, CommandResult, Failed, Input, load


def command(given: Input, item: Command, log: Path) -> CommandResult:
    environment = dict(os.environ)
    environment["HOME"] = given.home
    # daemon_client::try_run and spawn_detached_daemon honor this before connecting
    # to or spawning the resident check daemon. Other helpers are still fully traced.
    environment["BLAZE_NO_DAEMON"] = "1"
    # Keep machine-readable stdout intact; diagnostic stderr remains a separate full artifact.
    with log.open("xb") as output, log.with_suffix(".stderr").open("xb") as errors:
        try:
            process = subprocess.Popen([given.blaze, *item.args], cwd=given.cwd,
                                       env=environment, stdin=subprocess.DEVNULL, stdout=output,
                                       stderr=errors)
        except OSError as error:
            output.write(f"could not execute selected Blaze: {error}\n".encode())
            return CommandResult(item.name, None, 127, False)
        try:
            status = process.wait(timeout=given.command_seconds)
            return CommandResult(item.name, process.pid, status, False)
        except subprocess.TimeoutExpired:
            process.kill()
            status = process.wait(timeout=given.reap_seconds)
            return CommandResult(item.name, process.pid, status, True)
        finally:
            if process.returncode is None:
                process.kill()
                process.wait(timeout=given.reap_seconds)


def main(given: Input) -> int:
    out = Path(given.out)
    result: dict[str, object] = {"schema": SCHEMA, "scope": SCOPE, "pid": os.getpid(),
                                 "commands": [], "complete": False}
    rows: list[dict[str, object]] = []
    result["commands"] = rows
    status = 0
    try:
        for item in given.commands:
            print(f"observe {item.name}: {json.dumps(asdict(item))}", flush=True)
            outcome = command(given, item, out / f"{item.name}.log")
            rows.append(asdict(outcome))
            status = outcome.status
            if status != 0:
                break
        result["complete"] = True
    finally:
        # Publication ends the command interval; no reader sees a partial receipt.
        temporary = out / "supervisor.tmp"
        with temporary.open("x", encoding="utf-8") as output:
            json.dump(result, output, sort_keys=True)
            output.write("\n")
        temporary.replace(out / "supervisor.json")
    return status if status >= 0 else 128 - status


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        sys.exit(main(load(sys.argv[1])))
    except (Failed, OSError, ValueError) as error:
        sys.exit(f"exec_observer_child.py: {error}")
