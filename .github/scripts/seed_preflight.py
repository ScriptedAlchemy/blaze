#!/usr/bin/env python3
"""Check the pinned seed's inherited sampler signal, readiness and complete shutdown.

One JSON input, before setup-python: keep this helper compatible with Ubuntu 22.04's
Python 3.10. The seed prints its sampler refusal before it starts the daemon's state and
store. Only a status answer from the owned foreground PID permits a stop request; every
command and reap has a deadline. Forced cleanup always accompanies a failed preflight.
"""

from dataclasses import dataclass, fields
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


class Failed(Exception):
    def __init__(self, message: str, returncode: int = 1):
        super().__init__(message)
        self.returncode = max(1, returncode) if returncode >= 0 else 128 - returncode


class CommandTimeout(Failed):
    def __init__(self, message: str):
        super().__init__(message, 124)


@dataclass(frozen=True)
class Input:
    blaze: str
    log: str
    ready_seconds: float = 120.0
    # The seed binds its socket before setup ends, so a status RPC can need the
    # remaining startup budget. ready() caps it by the overall readiness deadline.
    status_seconds: float = 120.0
    stop_seconds: float = 60.0
    reap_seconds: float = 30.0
    cleanup_seconds: float = 2.0

    def __post_init__(self) -> None:
        for name in ("blaze", "log"):
            value = getattr(self, name)
            if type(value) is not str or not Path(value).is_absolute() or "\0" in value:
                raise ValueError(f"{name} must be an absolute path")
        for field in fields(self):
            if field.name.endswith("_seconds"):
                value = getattr(self, field.name)
                if type(value) not in (int, float) or not math.isfinite(value) or value <= 0:
                    raise ValueError(f"{field.name} must be a finite positive number")


def load(value: str) -> Input:
    if value == "-":
        value = sys.stdin.read()
    elif not value.lstrip().startswith(("{", "[")):
        value = Path(value).read_text()
    raw = json.loads(value)
    if not isinstance(raw, dict):
        raise ValueError("seed preflight input must be one JSON object")
    return Input(**raw)


def exited(process: subprocess.Popen) -> int | None:
    """Observe this child without reaping it: its PID must still pin our process group.

    After wait()/poll() reaps a leader, its PID can name an unrelated new group. Keep
    the leader waitable until group cleanup finishes, including when it exits first.
    """
    if process.returncode is not None:
        raise Failed(f"owned process {process.pid} was reaped before process-group cleanup")
    try:
        result = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    except ChildProcessError as error:
        raise Failed(f"owned process {process.pid} is no longer waitable; refusing group signals") from error
    if result is None:
        return None
    if result.si_code == os.CLD_EXITED:
        return result.si_status
    if result.si_code in (os.CLD_KILLED, os.CLD_DUMPED):
        return -result.si_status
    raise Failed(f"owned process {process.pid} has an unexpected wait status {result.si_code}")


def wait_exit(process: subprocess.Popen, seconds: float) -> int:
    deadline = time.monotonic() + seconds
    while True:
        result = exited(process)
        if result is not None:
            return result
        left = deadline - time.monotonic()
        if left <= 0:
            raise subprocess.TimeoutExpired(process.args, seconds)
        time.sleep(min(0.025, left))


def live_members(process: subprocess.Popen) -> list[int]:
    # A waitable child pins the PID/PGID throughout this scan and the following
    # signal. A new session cannot reuse it, and outsiders cannot join its session.
    exited(process)
    members = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            # comm may contain spaces or ')'; fields after its final ')' start at
            # state (3), then ppid (4), pgrp (5), session (6).
            state = (entry / "stat").read_bytes().rsplit(b")", 1)[1].split()
        except (FileNotFoundError, ProcessLookupError):
            continue
        if int(state[2]) == process.pid and int(state[3]) == process.pid:
            # killpg(pgid, 0) also sees orphan zombies. They are already exited;
            # waiting for PID 1 to reap them would turn clean shutdown into a hang.
            if state[0] not in (b"Z", b"X"):
                members.append(int(entry.name))
    return members


def cleanup(process: subprocess.Popen, seconds: float, log) -> bool:
    """Bound TERM, KILL and the reap; return whether any live member needed a signal."""
    # A second cancellation must not interrupt cleanup halfway through. Any pending
    # signal is delivered when the old mask is restored, after the bounded cleanup.
    previous = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM, signal.SIGINT})
    try:
        forced = False
        for signum in (signal.SIGTERM, signal.SIGKILL):
            if not live_members(process):
                break
            forced = True
            print(f"cleanup: signal {signum} to owned process group {process.pid}", file=log, flush=True)
            try:
                os.killpg(process.pid, signum)
            except ProcessLookupError:
                pass
            deadline = time.monotonic() + seconds
            while live_members(process):
                left = deadline - time.monotonic()
                if left <= 0:
                    break
                time.sleep(min(0.025, left))
        remaining = live_members(process)
        if remaining:
            raise Failed(f"owned process group {process.pid} still has live members {remaining} "
                         f"after SIGKILL within {seconds:g} seconds", 124)
        # Only now is releasing the leader's PID safe. Never poll/wait it earlier.
        try:
            process.wait(timeout=seconds)
        except subprocess.TimeoutExpired as error:
            raise Failed(f"owned process {process.pid} was not reaped within {seconds:g} seconds", 124) from error
        return forced
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous)


def command(given: Input, args: list[str], seconds: float, log) -> str:
    """Keep complete command output without a pipe a descendant could hold open forever."""
    argv = [given.blaze, "--ignore-pin", *args]
    print(f"command (within {seconds:g} s): {json.dumps(argv)}", file=log, flush=True)
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                                   start_new_session=True)
        failure = None
        try:
            result = wait_exit(process, seconds)
            if result != 0:
                failure = Failed(f"blaze {' '.join(args)} exited with status {result}", result)
        except subprocess.TimeoutExpired:
            failure = CommandTimeout(f"blaze {' '.join(args)} did not finish within {seconds:g} seconds")
        except (Failed, OSError) as error:
            failure = error
        finally:
            try:
                forced = cleanup(process, given.cleanup_seconds, log)
                if forced and failure is None:
                    failure = Failed(f"blaze {' '.join(args)} needed forced process-group cleanup")
            except (Failed, OSError) as error:
                print(f"cleanup error: {error}", file=log, flush=True)
                if failure is None:
                    failure = error
            out.seek(0)
            err.seek(0)
            stdout = out.read().decode(errors="replace")
            stderr = err.read().decode(errors="replace")
            print(f"stdout:\n{stdout}\nstderr:\n{stderr}\nexit: {process.returncode}",
                  file=log, flush=True)
        if failure is not None:
            raise failure
        return stdout


def alive(process: subprocess.Popen, phase: str) -> None:
    result = exited(process)
    if result is not None:
        raise Failed(f"the owned seed daemon {process.pid} exited with status {result} {phase}", result or 1)


def status_pid(stdout: str) -> int | None:
    try:
        value = json.loads(stdout)
    except json.JSONDecodeError as error:
        raise Failed(f"blaze status --json did not return JSON: {error}") from error
    if not isinstance(value, dict) or "daemon" not in value:
        raise Failed("blaze status --json has no daemon field")
    daemon = value["daemon"]
    if daemon is None:
        return None
    if not isinstance(daemon, dict) or type(daemon.get("pid")) is not int or daemon["pid"] <= 0:
        raise Failed("blaze status --json has no positive integer daemon.pid")
    return daemon["pid"]


def ready(given: Input, process: subprocess.Popen, log) -> None:
    deadline = time.monotonic() + given.ready_seconds
    readiness_timeout = (f"the owned seed daemon {process.pid} did not answer status within "
                         f"{given.ready_seconds:g} seconds")
    while True:
        alive(process, "before readiness")
        left = deadline - time.monotonic()
        if left <= 0:
            raise Failed(readiness_timeout, 124)
        # command() finishes cleanup before raising. That work can end after the
        # readiness deadline, so this classifies from the grant.
        allowance = min(left, given.status_seconds)
        try:
            pid = status_pid(command(given, ["status", "--json"], allowance, log))
        except CommandTimeout as error:
            if allowance < given.status_seconds:
                raise Failed(f"{readiness_timeout}; {error}", 124) from error
            raise
        alive(process, "before readiness")
        if pid is not None:
            if pid != process.pid:
                raise Failed(f"blaze status answered from daemon {pid}, expected owned daemon {process.pid}")
            print(f"ready: owned daemon {pid} answered status", file=log, flush=True)
            return
        time.sleep(min(0.25, max(0, deadline - time.monotonic())))


def sampler(given: Input, process: subprocess.Popen, log) -> None:
    rtmin6 = signal.SIGRTMIN + 6
    try:
        status = Path(f"/proc/{process.pid}/status").read_text()
    except OSError:
        alive(process, "before its sampler could be checked")
        raise
    ignored = next((line.split()[1] for line in status.splitlines() if line.startswith("SigIgn:")), None)
    if ignored is None or not int(ignored, 16) & (1 << (rtmin6 - 1)):
        raise Failed(f"the owned seed daemon {process.pid} does not ignore SIGRTMIN+6 "
                     f"(SigIgn {ignored}); BASH_ENV's trap did not reach it")
    if "no CPU samples" not in Path(given.log).read_text(errors="replace"):
        raise Failed(f"the owned seed daemon {process.pid} did not say its sampler is off")
    print(f"sampler: daemon {process.pid}; SigIgn {ignored}; SIGRTMIN+6 is {rtmin6}", file=log, flush=True)


def run(given: Input) -> None:
    daemon_log = Path(given.log)
    commands_log = daemon_log.with_name(daemon_log.name + ".commands.log")
    failure = None
    process = None
    try:
        with daemon_log.open("wb") as output, commands_log.open("w") as commands:
            try:
                process = subprocess.Popen([given.blaze, "--ignore-pin", "serve"],
                                           stdin=subprocess.DEVNULL, stdout=output, stderr=output,
                                           start_new_session=True)
                print(f"starting owned seed daemon {process.pid}", file=commands, flush=True)
                ready(given, process, commands)
                sampler(given, process, commands)
                alive(process, "before shutdown")
                command(given, ["stop"], given.stop_seconds, commands)
                try:
                    result = wait_exit(process, given.reap_seconds)
                except subprocess.TimeoutExpired as error:
                    raise Failed(f"the owned seed daemon {process.pid} did not exit within "
                                 f"{given.reap_seconds:g} seconds after blaze stop", 124) from error
                if result != 0:
                    raise Failed(f"the owned seed daemon {process.pid} exited with status {result} "
                                 "after blaze stop", result)
            except (Failed, OSError) as error:
                failure = error
            finally:
                if process is not None:
                    try:
                        forced = cleanup(process, given.cleanup_seconds, commands)
                        # A successful leader exit cannot hide a surviving descendant.
                        if forced and failure is None:
                            failure = Failed(f"the owned seed daemon {process.pid} needed forced cleanup")
                    except (Failed, OSError) as error:
                        print(f"cleanup error: {error}", file=commands, flush=True)
                        if failure is None:
                            failure = error
    finally:
        # Read after shutdown/cleanup: the diagnostics emitted while stopping matter too.
        for path in (daemon_log, commands_log):
            if path.exists():
                print(f"::group::{path.name}", flush=True)
                try:
                    print(path.read_text(errors="replace"), end="", flush=True)
                except OSError as error:
                    print(f"::error::could not read {path}: {error}", file=sys.stderr, flush=True)
                    if failure is None:
                        failure = error
                print("::endgroup::", flush=True)
    if failure is not None:
        raise failure
    print("seed preflight: owned daemon ready, sampler off, clean shutdown", flush=True)


def interrupted(signum, _frame) -> None:
    raise Failed(f"seed preflight interrupted by signal {signum}", 128 + signum)


def main() -> int:
    try:
        if len(sys.argv) != 2:
            raise ValueError("usage: seed_preflight.py CONFIG (one JSON object)")
        given = load(sys.argv[1])
        signal.signal(signal.SIGTERM, interrupted)
        signal.signal(signal.SIGINT, interrupted)
        run(given)
        return 0
    except (Failed, OSError, TypeError, ValueError) as error:
        print(f"::error::seed preflight: {error}", file=sys.stderr, flush=True)
        return error.returncode if isinstance(error, Failed) else 1


if __name__ == "__main__":
    raise SystemExit(main())
