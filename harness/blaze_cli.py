#!/usr/bin/env python3
"""Exercise the source CLI's real run/check/export commands in a new home.

One JSON object (inline, a file, or `-`): blaze is the copied source executable,
scratch is a new directory, shelf is the canonical source toolchain shelf, and
timeout bounds each command (1800 seconds by default). Unknown fields are refused.

Cold, warm and body-edited hello programs must print the corresponding exact line.
A deliberately invalid body must be refused, restoring it must recover, and an
exported executable must run outside Blaze. The source CLI, launcher and fact shim
must keep the same bytes throughout. This baseline grants no distributed acceptance;
harness/distv2.py retains the full worker/cache/fault oracle.
"""
from __future__ import annotations

from dataclasses import dataclass, fields
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys

FIXTURE = Path(__file__).resolve().with_suffix("") / "hello"
PHASES = ("cold", "warm", "edit", "invalid", "recovered", "checked", "export", "standalone")


class Failed(ValueError):
    pass


@dataclass(frozen=True)
class Input:
    blaze: str
    scratch: str
    shelf: str
    timeout: int = 1800


def load(text: str) -> Input:
    raw = json.loads(sys.stdin.read() if text == "-" else (
        text if text.lstrip().startswith("{") else Path(text).read_text()))
    if not isinstance(raw, dict):
        raise Failed("input must be a JSON object")
    if unknown := set(raw) - {field.name for field in fields(Input)}:
        raise Failed(f"unknown input fields: {', '.join(sorted(unknown))}")
    given = Input(**raw)
    for name in ("blaze", "scratch", "shelf"):
        value = getattr(given, name)
        if not isinstance(value, str) or not value or "\0" in value:
            raise Failed(f"{name} must be a nonempty path without NUL bytes")
    if type(given.timeout) is not int or given.timeout <= 0:
        raise Failed("timeout must be a positive integer")
    return given


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def binaries(blaze: Path) -> dict[str, str]:
    if blaze.name != "blaze":
        raise Failed("the source CLI must be named blaze beside blaze-launcher and blaze-fact-shim")
    result = {}
    for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
        path = blaze.with_name(name)
        if path.is_symlink() or not path.is_file() or not os.access(path, os.X_OK):
            raise Failed(f"{path} is not a copied regular executable")
        result[name] = digest(path)
    return result


def run(argv: list[str], project: Path, env: dict[str, str], out: Path, timeout: int) -> dict:
    """Own the whole process group, including a failed or timed-out compiler's children."""
    child = subprocess.Popen(argv, cwd=project, env=env, stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             text=True, start_new_session=True)
    expired = False
    stdout = stderr = ""
    try:
        stdout, stderr = child.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        expired = True
        os.killpg(child.pid, signal.SIGTERM)
        try:
            stdout, stderr = child.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            stdout, stderr = child.communicate(timeout=10)
    finally:
        # Do not poll/reap the leader before stopping its group on an interrupted wait.
        # Until wait/communicate reaps it, this PID cannot be reused for another group.
        if child.returncode is None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait(timeout=10)
    record = {"argv": argv, "cwd": str(project), "returncode": child.returncode,
              "timed_out": expired, "stdout": stdout, "stderr": stderr}
    out.write_text(json.dumps(record, indent=2) + "\n")
    return record


def require(record: dict, name: str, expected: str | None, *, rejects: bool = False) -> None:
    if record["timed_out"]:
        raise Failed(f"{name}: command timed out")
    if rejects:
        findings, summary = check_reply(record)
        if (record["returncode"] != 1 or summary["errors"] < 1
                or not any(finding.get("severity") == "error" and finding.get("code") == "E0425"
                           and finding.get("path") == "src/main.rs" and finding.get("line") == 2
                           and isinstance(finding.get("message"), str)
                           and "`missing_source_body`" in finding["message"]
                           for finding in findings)):
            raise Failed(f"{name}: missing the planted undefined-body finding and failed check verdict")
    elif record["returncode"] != 0:
        raise Failed(f"{name}: command exited {record['returncode']}: {record['stderr'][-2000:]}")
    elif expected is not None and record["stdout"] != expected:
        raise Failed(f"{name}: stdout {record['stdout']!r} differs from {expected!r}")
    if name == "checked":
        _, summary = check_reply(record)
        if summary["errors"] != 0 or summary.get("not_checked") != []:
            raise Failed("checked: the valid program was not checked completely")


def check_reply(record: dict) -> tuple[list[dict], dict]:
    try:
        rows = [json.loads(line) for line in record["stdout"].splitlines() if line.strip()]
    except json.JSONDecodeError as error:
        raise Failed("check did not produce machine-readable findings and a closing summary") from error
    summaries = [row for row in rows if isinstance(row, dict) and "errors" in row]
    if (len(summaries) != 1 or rows[-1] is not summaries[0]
            or summaries[0].get("command") != "blaze check"
            or type(summaries[0]["errors"]) is not int
            or summaries[0].get("failures", 0) != 0):
        raise Failed("check has no one closing summary without task/setup failures")
    return [row for row in rows[:-1] if isinstance(row, dict)], summaries[0]


def main(given: Input) -> None:
    selected = Path(given.blaze).absolute()
    before = binaries(selected)
    root = Path(given.scratch).absolute()
    if root.is_symlink() or (root.exists() and any(root.iterdir())):
        raise Failed(f"scratch {root} must be new or empty; earlier evidence is never erased")
    shelf = Path(given.shelf).resolve(strict=True)
    if not shelf.is_dir() or not any(shelf.iterdir()) or any(p.is_symlink() for p in shelf.rglob("*")):
        raise Failed("shelf must be the nonempty canonical directory, without symlinks")
    root.mkdir(parents=True, exist_ok=True)
    receipt: dict = {"schema": 1, "complete": False, "binaries": before, "phases": [], "error": None}
    try:
        home, project = root / "home", root / "project"
        (home / ".blaze/v2").mkdir(parents=True)
        shutil.copytree(shelf, home / ".blaze/v2/toolchain")
        shutil.copytree(FIXTURE, project)
        env = {"HOME": str(home), "PATH": os.environ["PATH"], "LANG": "C.UTF-8", "TERM": "dumb",
               "BLAZE_NO_DAEMON": "1", "BLAZE_CORES": os.environ.get("BLAZE_CORES", "all")}
        receipt["environment"] = env
        source = project / "src/main.rs"
        executable = root / "hello-exported"
        for name in PHASES:
            if name in ("edit", "recovered"):
                shutil.copyfile(project / "stages/edited.rs", source)
            elif name == "invalid":
                shutil.copyfile(project / "stages/rejected.rs", source)
            expected = "hello from source: 1\n" if name in ("cold", "warm") else "hello from source: 7\n"
            argv = [str(selected), "run", "-p", "source-hello"]
            if name in ("invalid", "checked"):
                argv = [str(selected), "check", "--view", "json"]
                expected = None
            elif name == "export":
                argv = [str(selected), "export", "-p", "source-hello", "-o", str(executable)]
                expected = None
            elif name == "standalone":
                if executable.is_symlink() or not executable.is_file() or not os.access(executable, os.X_OK):
                    raise Failed("export did not produce a regular standalone executable")
                receipt["artifact_sha256"] = digest(executable)
                argv = [str(executable)]
            record = run(argv, project, env, root / f"{name}.json", given.timeout)
            record["source_sha256"] = digest(source)
            require(record, name, expected, rejects=name == "invalid")
            receipt["phases"].append({"name": name, "returncode": record["returncode"],
                                      "source_sha256": record["source_sha256"]})
            print(f"source CLI: {name} passed", flush=True)
        if binaries(selected) != before:
            raise Failed("source CLI, launcher or fact shim changed while the smoke was running")
        receipt["complete"] = True
    except BaseException as error:
        receipt["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        (root / "results.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print("source CLI: every check passed", flush=True)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    def interrupted(signum, _frame):
        raise Failed(f"received signal {signum}; stopping the owned command group")
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main(load(sys.argv[1]))
    except (OSError, ValueError, TypeError, subprocess.SubprocessError) as error:
        sys.exit(f"source CLI: {error}")
