#!/usr/bin/env python3
"""Observe a selected Blaze's complete Linux process tree, including its shutdown.

One JSON object (inline, a path, or `-`), compatible with Python 3.10:
    blaze, cwd, home, out  absolute paths; home and out must not exist
    commands              [{"name": "protocol", "args": ["test", ...]}, ...]
    command_seconds       positive bound per command (4500)
    stop_seconds          positive bound for the selected Blaze's stop (60)
    reap_seconds          positive bound after stop for every descendant to exit (30)
    tracer                absolute strace executable (/usr/bin/strace)

The client and its sibling engine are SHA-256 bound before and after the run. The
helper inserts --ignore-pin, uses a new private BLAZE_HOME/store, and runs stop in
the traced tree even when a command fails. HOME and the sandbox are unchanged.

Linux strace 6.8's launch-only -ff and --kill-on-exit use kernel fork/clone/exec
events and PTRACE_O_EXITKILL; a detached daemon remains traced. No PATH wrapper,
process sampling, attach race, seccomp replacement or accepted positive baseline.
Missing/unsupported tracing, lost descendants, incomplete traces and cleanup all
fail. The raw evidence is for build commands without secrets in their arguments.
This observes Linux invocations wired through this helper; it does not establish
C4 for unwrapped jobs or other operating systems.
"""

from __future__ import annotations

import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field, fields, replace
from typing import cast


SCRIPT = "exec_observer.py"
HERE = Path(__file__).resolve().parent
FORBIDDEN = frozenset(("cargo", "rustc", "rustdoc"))
MAX_TRACE_BYTES = 128 * 1024 * 1024
MAX_TRACES = 100_000
HEX_STRING = r'"(?:\\x[0-9a-fA-F]{2})*"'
CALL = re.compile(r"^(execveat|execve|clone3|clone|fork|vfork|exit_group|exit)\(")
RESUMED = re.compile(r"^<\.\.\. (\w+) resumed>(.*)$")
TRANSFER = re.compile(r"^(.*) <pid changed to (\d+) \.\.\.>$")
SUPERSEDED = re.compile(r"^\+\+\+ superseded by execve in pid (\d+) \+\+\+$")
TERMINAL = re.compile(r"^\+\+\+ (?:exited with \d+|killed by SIG[A-Z0-9_+]+(?: \(core dumped\))?) \+\+\+$")
SIGNAL = re.compile(r"^--- SIG[A-Z0-9_+]+ .* ---$")
GROUP_STOP = re.compile(r"^--- stopped by SIG[A-Z0-9_+]+ ---$")


class Failed(Exception):
    def __init__(self, message: str, code: int = 1):
        super().__init__(message)
        self.code = max(1, code) if code >= 0 else 128 - code


@dataclass(frozen=True)
class Command:
    name: str
    args: tuple[str, ...]


@dataclass(frozen=True)
class Input:
    blaze: str
    cwd: str
    home: str
    out: str
    commands: tuple[Command, ...]
    command_seconds: int = 4500
    stop_seconds: int = 60
    reap_seconds: int = 30
    tracer: str = "/usr/bin/strace"


def mapping(value: object, names: set[str], where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ValueError(f"{where} must be a JSON object")
    result = cast(dict[str, object], value)
    if extra := set(result) - names:
        raise ValueError(f"{where} has unknown fields: {', '.join(sorted(extra))}")
    return result


def text_value(value: object, where: str) -> str:
    if not isinstance(value, str) or not value or "\0" in value:
        raise ValueError(f"{where} must be a nonempty string without NUL")
    return value


def path_value(value: object, where: str) -> str:
    result = text_value(value, where)
    if not Path(result).is_absolute():
        raise ValueError(f"{where} must be an absolute path")
    return result


def seconds(value: object, where: str) -> int:
    if type(value) is not int or not 1 <= value <= 21_600:
        raise ValueError(f"{where} must be an integer from 1 to 21600")
    return value


def load(value: str) -> Input:
    if value == "-":
        value = sys.stdin.read()
    elif not value.lstrip().startswith(("{", "[")):
        value = Path(value).read_text(encoding="utf-8")
    raw: object = json.loads(value)
    given = mapping(raw, {item.name for item in fields(Input)}, "observer input")
    raw_commands = given.get("commands")
    if not isinstance(raw_commands, list) or not 1 <= len(cast(list[object], raw_commands)) <= 16:
        raise ValueError("commands must contain 1 to 16 named commands")
    commands: list[Command] = []
    for index, raw_command in enumerate(cast(list[object], raw_commands)):
        where = f"commands[{index}]"
        command = mapping(raw_command, {"name", "args"}, where)
        name = text_value(command.get("name"), where + ".name")
        if not re.fullmatch(r"[a-z][a-z0-9_-]{0,63}", name):
            raise ValueError(f"{where}.name must be a short lowercase file name")
        if name in ("stop", "supervisor", "tracer"):
            raise ValueError(f"{where}.name {name!r} is reserved for observer evidence")
        raw_args = command.get("args")
        if not isinstance(raw_args, list) or not raw_args:
            raise ValueError(f"{where}.args must be a nonempty list")
        args = tuple(text_value(arg, where + ".args") for arg in cast(list[object], raw_args))
        commands.append(Command(name, args))
    if len({command.name for command in commands}) != len(commands):
        raise ValueError("commands must have unique names")
    return Input(
        blaze=path_value(given.get("blaze"), "blaze"),
        cwd=path_value(given.get("cwd"), "cwd"),
        home=path_value(given.get("home"), "home"),
        out=path_value(given.get("out"), "out"),
        commands=tuple(commands),
        command_seconds=seconds(given.get("command_seconds", 4500), "command_seconds"),
        stop_seconds=seconds(given.get("stop_seconds", 60), "stop_seconds"),
        reap_seconds=seconds(given.get("reap_seconds", 30), "reap_seconds"),
        tracer=path_value(given.get("tracer", "/usr/bin/strace"), "tracer"),
    )


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        before = os.fstat(stream.fileno())
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
        after = os.fstat(stream.fileno())
    if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
        after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns
    ) or (path.stat().st_dev, path.stat().st_ino) != (after.st_dev, after.st_ino):
        raise Failed(f"executable {path} changed while its identity was read")
    if before.st_size == 0 or not os.access(path, os.X_OK):
        raise Failed(f"{path} is not a nonempty executable")
    return value.hexdigest()


def compiler_identity(blaze: Path) -> dict[str, str]:
    if blaze.name != "blaze":
        raise Failed(f"selected client {blaze} must be named blaze beside blaze-engine")
    return {name: digest(blaze.with_name(name)) for name in ("blaze", "blaze-engine")}


def unhex(quoted: str) -> str:
    if not re.fullmatch(HEX_STRING, quoted):
        raise Failed("execution path is not the tracer's complete hexadecimal string")
    value = bytes.fromhex(quoted[1:-1].replace("\\x", ""))
    if b"\0" in value:
        raise Failed("execution path contains NUL")
    return os.fsdecode(value)


def fd_path(value: str) -> str:
    # strace's -yy path annotation can contain literal printable bytes as well as
    # escaped nonprinting bytes. Do not mistake a missing fd annotation for a name.
    if not value or value.endswith("...") or "<" in value or ">" in value:
        raise Failed("execveat has no complete decoded descriptor path")
    value = re.sub(r"\\x([0-9a-fA-F]{2})", lambda match: chr(int(match[1], 16)), value)
    if "\\" in value or "\0" in value:
        raise Failed("execveat descriptor path has an unsupported escape")
    return value.removesuffix(" (deleted)")


def execution_path(call: str) -> str:
    if call.startswith("execve("):
        match = re.match(r"execve\((" + HEX_STRING + r"),", call)
        if match is None:
            raise Failed("execve has a missing or truncated executable path")
        return unhex(match[1])
    match = re.match(r"execveat\((?:AT_FDCWD|\d+)(?:<([^>]*)>(?:\(deleted\))?)?, ("
                     + HEX_STRING + r"),", call)
    if match is None:
        raise Failed("execveat has a missing or truncated executable path")
    path = unhex(match[2])
    if Path(path).is_absolute():
        return path
    directory = fd_path(match[1] or "")
    if not path:
        if not re.search(r", (?:AT_SYMLINK_NOFOLLOW\|)?AT_EMPTY_PATH\) = ", call):
            raise Failed("execveat's empty path has no AT_EMPTY_PATH identity")
        return directory
    return str(Path(directory) / path)


def forbidden(path: str) -> bool:
    name = Path(path).name.lower().removesuffix(" (deleted)")
    return any(name in (tool, tool + ".exe", tool + ".real") for tool in FORBIDDEN)


@dataclass
class Trace:
    pid: int
    calls: list[str] = field(default_factory=list)
    # An exec by another thread changes its pid to its thread-group leader. strace
    # closes the old thread file, continues in the leader's file, and names both.
    moved_to: int | None = None
    moved_call: str | None = None
    resumed: list[tuple[int, int, str]] = field(default_factory=list)
    ended: bool = False


def parse_trace(pid: int, data: str) -> Trace:
    if not data or not data.endswith("\n"):
        raise Failed(f"trace for pid {pid} is missing or truncated")
    result = Trace(pid)
    pending: str | None = None
    switched: int | None = None
    for number, line in enumerate(data.splitlines(), 1):
        where = f"trace pid {pid}, line {number}"
        if result.ended or result.moved_to is not None:
            raise Failed(f"{where}: records follow process completion")
        if SIGNAL.fullmatch(line) or GROUP_STOP.fullmatch(line):
            continue
        if match := SUPERSEDED.fullmatch(line):
            if switched is not None:
                raise Failed(f"{where}: two exec transfers without completion")
            switched = int(match[1])
            continue
        if match := TRANSFER.fullmatch(line):
            if pending is not None or switched is not None or not match[1].startswith(("execve(", "execveat(")):
                raise Failed(f"{where}: invalid thread exec transfer")
            result.moved_to, result.moved_call = int(match[2]), match[1]
            continue
        if TERMINAL.fullmatch(line):
            if pending is not None or switched is not None:
                raise Failed(f"{where}: process ended with an incomplete syscall")
            result.ended = True
            continue
        if match := RESUMED.fullmatch(line):
            if switched is not None:
                if pending is not None or match[1] not in ("execve", "execveat"):
                    raise Failed(f"{where}: invalid resumed thread exec")
                result.resumed.append((len(result.calls), switched, match[2]))
                result.calls.append("")
                switched = None
                continue
            if pending is None or not pending.startswith(match[1] + "("):
                raise Failed(f"{where}: resumed syscall has no matching entry")
            line, pending = pending + match[2], None
        elif line.endswith(" <unfinished ...>"):
            if pending is not None or switched is not None or CALL.match(line) is None:
                raise Failed(f"{where}: invalid unfinished syscall")
            pending = line.removesuffix(" <unfinished ...>")
            continue
        elif pending is not None or switched is not None:
            raise Failed(f"{where}: interrupted syscall has no matching completion")
        if CALL.match(line) is None or " = " not in line:
            raise Failed(f"{where}: unknown or incomplete process record")
        result.calls.append(line)
    if pending is not None or switched is not None or not (result.ended or result.moved_to is not None):
        raise Failed(f"trace for pid {pid} has no complete process termination")
    return result


def inspect_traces(directory: Path, root_pid: int, python: str) -> dict[str, object]:
    files = sorted(directory.glob("exec.trace.*"))
    if not files or len(files) > MAX_TRACES:
        raise Failed("execution tracing produced no bounded process evidence")
    total = sum(path.stat().st_size for path in files)
    if total > MAX_TRACE_BYTES:
        raise Failed(f"execution traces exceeded {MAX_TRACE_BYTES} bytes")
    traces: dict[int, Trace] = {}
    for path in files:
        suffix = path.name.removeprefix("exec.trace.")
        if not suffix.isdecimal() or int(suffix) <= 0:
            raise Failed(f"unexpected execution trace file {path.name}")
        pid = int(suffix)
        if pid in traces:
            raise Failed(f"two execution traces name pid {pid}")
        traces[pid] = parse_trace(pid, path.read_text(encoding="utf-8"))
    if root_pid not in traces:
        raise Failed("the supervised command's process has no execution trace")
    used: set[int] = set()
    for trace in traces.values():
        for at, old, suffix in trace.resumed:
            previous = traces.get(old)
            if previous is None or previous.moved_to != trace.pid or previous.moved_call is None or old in used:
                raise Failed(f"pid {trace.pid} has an unpaired thread exec transfer from {old}")
            trace.calls[at] = previous.moved_call + suffix
            used.add(old)
    if used != {pid for pid, trace in traces.items() if trace.moved_to is not None}:
        raise Failed("an executing thread disappeared without a matching process identity transfer")
    forks = 0
    executions: list[dict[str, object]] = []
    violations: list[str] = []
    for trace in traces.values():
        for call in trace.calls:
            name = call.split("(", 1)[0]
            result = call.rsplit(" = ", 1)[1]
            numeric = re.fullmatch(r"(-?\d+)(?: [A-Z0-9_]+ \([^\n]*\))?", result)
            if name in ("exit", "exit_group"):
                if result != "?":
                    raise Failed(f"pid {trace.pid} has an unknown exit result")
                continue
            if numeric is None:
                raise Failed(f"pid {trace.pid} has an unknown {name} result")
            returned = int(numeric[1])
            if name in ("clone", "clone3", "fork", "vfork"):
                if returned < -1:
                    raise Failed(f"pid {trace.pid} has an unknown process creation result")
                forks += int(returned > 0)
                continue
            path = execution_path(call)
            if returned not in (0, -1):
                raise Failed(f"pid {trace.pid} has an unknown exec result")
            if forbidden(path):
                violations.append(f"pid {trace.pid}: {path}")
            executions.append({"pid": trace.pid, "path": path, "succeeded": returned == 0})
    # Each successful fork/clone creates exactly one tracee, including threads and
    # PID-namespace children. Counting avoids confusing namespace-local return PIDs
    # with strace's file names; every file must also terminate or pair its exec move.
    if forks + 1 != len(traces):
        raise Failed(f"incomplete descendant evidence: {forks} creations but {len(traces)} process traces")
    first = next((item for item in executions if item["pid"] == root_pid), None)
    if first is None or first["path"] != python or first["succeeded"] is not True:
        raise Failed("execution tracing did not begin at the supervised helper's launch")
    if violations:
        raise Failed("forbidden compiler execution: " + "; ".join(violations))
    return {"processes": len(traces), "creations": forks, "bytes": total, "executions": executions}


def tracer_argv(given: Input, config: Path, python: str) -> list[str]:
    return [given.tracer, "--kill-on-exit", "-ff", "-q", "-xx", "-s", "0", "-yy",
            "-e", "trace=execve,execveat,clone,clone3,fork,vfork,exit,exit_group",
            "-o", str(Path(given.out) / "exec.trace"), "--", python,
            str(HERE / "exec_observer_child.py"), str(config)]


def parent_death(parent: int) -> None:
    # This launch-only CLI has no threads. The single-threaded pre-exec child sets
    # PDEATHSIG before execing strace; EXITKILL in strace then owns its descendants.
    # Killing this observer cannot leave a tracer and detached Blaze daemon behind.
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(1, signal.SIGKILL, 0, 0, 0) != 0:  # PR_SET_PDEATHSIG
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))
    if os.getppid() != parent:
        os.kill(os.getpid(), signal.SIGKILL)


def interrupted(number: int, _frame: object) -> None:
    raise Failed(f"observer interrupted by signal {number}", 128 + number)


def wait_tree(process: subprocess.Popen[bytes], given: Input) -> int:
    """Bound the commands, and start the separate descendant bound after stop."""
    total = len(given.commands) * given.command_seconds + given.stop_seconds + given.reap_seconds
    deadline = time.monotonic() + total
    receipt = Path(given.out) / "supervisor.json"
    stopping = False
    while True:
        now = time.monotonic()
        if not stopping and receipt.exists():
            deadline = min(deadline, now + given.reap_seconds)
            stopping = True
        left = deadline - now
        if left <= 0:
            phase = "descendants after Blaze stop" if stopping else "commands and shutdown"
            raise Failed(f"execution tree exceeded its bound waiting for {phase}", 124)
        try:
            return process.wait(timeout=min(left, 0.1))
        except subprocess.TimeoutExpired:
            pass


def read_receipt(path: Path, given: Input) -> tuple[int, int]:
    value: object = json.loads(path.read_text(encoding="utf-8"))
    receipt = mapping(value, {"pid", "commands", "stop", "complete"}, "supervisor receipt")
    pid = receipt.get("pid")
    if type(pid) is not int or pid <= 0 or receipt.get("complete") is not True:
        raise Failed("supervised process has no complete receipt")
    rows = receipt.get("commands")
    if not isinstance(rows, list):
        raise Failed("supervised process has no command outcomes")
    outcomes = cast(list[object], rows)
    code = 0
    for index, value in enumerate(outcomes):
        row = mapping(value, {"name", "returncode"}, "command receipt")
        if index >= len(given.commands) or row.get("name") != given.commands[index].name:
            raise Failed("supervised command receipt changed the command sequence")
        status = row.get("returncode")
        if type(status) is not int:
            raise Failed("supervised command has no exit status")
        if status != 0:
            code = status
            if index + 1 != len(outcomes):
                raise Failed("supervised command sequence continued after failure")
    if len(outcomes) != len(given.commands) and code == 0:
        raise Failed("supervised command receipt omitted a required command")
    stop = receipt.get("stop")
    if type(stop) is not int:
        raise Failed("supervised command receipt has no shutdown status")
    if code == 0:
        code = stop
    return pid, code


def observe(given: Input) -> int:
    if sys.platform != "linux":
        raise Failed(f"kernel execution observer requires Linux, not {sys.platform}")
    blaze = Path(given.blaze).resolve(strict=True)
    identity = compiler_identity(blaze)
    tracer = Path(given.tracer).resolve(strict=True)
    tracer_identity = digest(tracer)
    cwd = Path(given.cwd).resolve(strict=True)
    if not cwd.is_dir():
        raise Failed(f"working directory {given.cwd} does not exist")
    out, home = Path(given.out), Path(given.home)
    if out.exists() or out.is_symlink() or home.exists() or home.is_symlink():
        raise Failed("out and home must both be new: prior processes or evidence cannot establish C4")
    out, home = out.resolve(), home.resolve()
    if out == home or out in home.parents or home in out.parents:
        raise Failed("out and home must be separate new directories")
    # Bind what is launched, not merely what an input symlink happened to name
    # when its digest was taken. All later phases use these exact canonical paths.
    given = replace(given, blaze=str(blaze), tracer=str(tracer), cwd=str(cwd), home=str(home), out=str(out))
    out.mkdir(mode=0o700)
    home.mkdir(mode=0o700)
    config = out / "input.json"
    config.write_text(json.dumps(asdict(given), indent=2) + "\n", encoding="utf-8")
    python = str(Path(sys.executable).resolve(strict=True))
    record: dict[str, object] = {
        "schema": 1, "scope": "linux-launched-descendants-through-blaze-stop",
        "compiler": identity, "tracer_sha256": tracer_identity, "passed": False,
    }
    process: subprocess.Popen[bytes] | None = None
    failure: Failed | None = None
    status = 1
    previous = {number: signal.signal(number, interrupted) for number in (signal.SIGINT, signal.SIGTERM)}
    parent = os.getpid()
    try:
        with (out / "tracer.log").open("xb") as tracer_log, (out / "supervisor.log").open("xb") as output:
            process = subprocess.Popen(tracer_argv(given, config, python), stdin=subprocess.DEVNULL,
                                       stdout=output, stderr=tracer_log, cwd=given.cwd,
                                       start_new_session=True, preexec_fn=lambda: parent_death(parent))
            status = wait_tree(process, given)
        if (out / "tracer.log").stat().st_size:
            raise Failed("strace reported a diagnostic; execution evidence is not complete (see tracer.log)")
        pid, command_status = read_receipt(out / "supervisor.json", given)
        record["trace"] = inspect_traces(out, pid, python)
        record["command_status"] = command_status
        if command_status != 0:
            raise Failed(f"selected Blaze command or shutdown failed with status {command_status}", command_status)
        if status != 0:
            raise Failed(f"execution tracer exited with status {status}", status)
    except Failed as error:
        failure = error
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        failure = Failed(f"execution evidence failed: {error}")
    finally:
        blocked = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
        try:
            if process is not None and process.returncode is None:
                # The child has not been reaped, so its pid cannot have been reused.
                # EXITKILL covers descendants that made new sessions/process groups.
                process.kill()
                try:
                    process.wait(timeout=given.reap_seconds)
                except subprocess.TimeoutExpired:
                    failure = failure or Failed("strace did not reap after forced shutdown", 124)
                failure = failure or Failed("execution tracing needed forced shutdown")
            try:
                if compiler_identity(blaze) != identity or digest(tracer) != tracer_identity:
                    failure = failure or Failed("selected compiler or tracer changed during the observed run")
            except (OSError, Failed) as error:
                failure = failure or Failed(f"executable identity after shutdown: {error}")
            record["passed"] = failure is None
            if failure is not None:
                record["error"] = str(failure)
                record["returncode"] = failure.code
            else:
                record["returncode"] = 0
            (out / "result.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        finally:
            for number, handler in previous.items():
                signal.signal(number, handler)
            signal.pthread_sigmask(signal.SIG_SETMASK, blocked)
    if failure is not None:
        print(f"{SCRIPT}: {failure}; evidence {out}", file=sys.stderr)
        return failure.code
    print(f"{SCRIPT}: complete descendant execution evidence, no cargo/rustc/rustdoc; evidence {out}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        sys.exit(observe(load(sys.argv[1])))
    except (Failed, OSError, ValueError) as error:
        sys.exit(f"{SCRIPT}: {error}")
