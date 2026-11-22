#!/usr/bin/env python3
"""Check the pinned seed's inherited sampler signal, readiness and complete shutdown.

One JSON input, before setup-python: keep this helper compatible with Ubuntu 22.04's
Python 3.10. The seed prints its sampler refusal before it starts the daemon's state and
store. Only a status answer from the owned foreground PID permits a stop request; every
command and reap has a deadline. Forced cleanup always accompanies a failed preflight.
"""

from contextlib import contextmanager
from dataclasses import asdict, dataclass, fields, replace
from decimal import Decimal, InvalidOperation
import json
import math
import os
from pathlib import Path
import re
import resource
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


@contextmanager
def deferred_interrupts():
    # Popen can create its child before returning the ownership handle. Deliver a
    # cancellation only after assignment, inside the caller's cleanup region.
    previous = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM, signal.SIGINT})
    try:
        yield previous
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous)


@dataclass(frozen=True)
class MemoryLimit:
    path: str
    maximum: str
    high: str
    swap: str

    def __post_init__(self) -> None:
        if not isinstance(self.path, str) or not Path(self.path).is_absolute() or ".." in Path(self.path).parts:
            raise ValueError("memory limit path must be absolute without '..'")
        for value in (self.maximum, self.high, self.swap):
            if not isinstance(value, str) or (value != "max" and not value.isdecimal()):
                raise ValueError("memory limits must be kernel byte counts or 'max'")


@dataclass(frozen=True)
class WorkspaceScope:
    unit: str
    memory_gb: str
    command: tuple[str, ...]
    seconds: float = 4500.0
    entered: bool = False
    origin: tuple[MemoryLimit, ...] = ()
    memlock: tuple[int, int] | None = None

    def __post_init__(self) -> None:
        if not isinstance(self.unit, str) or not re.fullmatch(r"blaze-workspace-[a-z0-9-]{1,100}\.scope", self.unit):
            raise ValueError("workspace scope must have a unique blaze-workspace-*.scope name")
        if not isinstance(self.memory_gb, str):
            raise ValueError("workspace memory_gb must be the explicit configured string")
        try:
            size = Decimal(self.memory_gb) * (1 << 30)
            if not size.is_finite() or size <= 0 or size != size.to_integral_value():
                raise ValueError("workspace memory_gb must name a positive whole number of bytes")
        except InvalidOperation as error:
            raise ValueError("invalid workspace memory_gb") from error
        if (not isinstance(self.command, tuple) or not self.command
                or any(not isinstance(arg, str) or not arg or "\0" in arg for arg in self.command)
                or not Path(self.command[0]).is_absolute()):
            raise ValueError("workspace command must be an argv with an absolute executable")
        if type(self.seconds) not in (int, float) or not math.isfinite(self.seconds) or self.seconds <= 0:
            raise ValueError("workspace seconds must be a finite positive number")
        if type(self.entered) is not bool or not isinstance(self.origin, tuple):
            raise ValueError("invalid workspace scope phase or origin")
        if self.entered:
            if not self.origin or any(not isinstance(row, MemoryLimit) for row in self.origin):
                raise ValueError("entered scope requires its original ancestor limits")
            if (not isinstance(self.memlock, tuple) or len(self.memlock) != 2
                    or any(type(value) is not int for value in self.memlock)):
                raise ValueError("entered scope requires its inherited memlock limits")
        elif self.origin or self.memlock is not None:
            raise ValueError("scope origin and memlock are captured by its launcher")

    @property
    def memory_bytes(self) -> int:
        return int(Decimal(self.memory_gb) * (1 << 30))


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
    scope: WorkspaceScope | None = None

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
        if self.scope is not None and not isinstance(self.scope, WorkspaceScope):
            raise ValueError("scope must be a workspace scope object")


def load(value: str) -> Input:
    if value == "-":
        value = sys.stdin.read()
    elif not value.lstrip().startswith(("{", "[")):
        value = Path(value).read_text()
    raw = json.loads(value)
    if not isinstance(raw, dict):
        raise ValueError("seed preflight input must be one JSON object")
    if raw.get("scope") is not None:
        scope = raw["scope"]
        if not isinstance(scope, dict):
            raise ValueError("scope must be one JSON object")
        scope = dict(scope)
        if isinstance(scope.get("command"), list):
            scope["command"] = tuple(scope["command"])
        if isinstance(scope.get("origin", []), list):
            scope["origin"] = tuple(MemoryLimit(**row) for row in scope.get("origin", []))
        if isinstance(scope.get("memlock"), list):
            scope["memlock"] = tuple(scope["memlock"])
        raw["scope"] = WorkspaceScope(**scope)
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
        process = None
        failure = None
        try:
            with deferred_interrupts() as inherited:
                process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                                           start_new_session=True,
                                           preexec_fn=lambda: signal.pthread_sigmask(signal.SIG_SETMASK, inherited))
            result = wait_exit(process, seconds)
            if result != 0:
                failure = Failed(f"blaze {' '.join(args)} exited with status {result}", result)
        except subprocess.TimeoutExpired:
            failure = CommandTimeout(f"blaze {' '.join(args)} did not finish within {seconds:g} seconds")
        except (Failed, OSError) as error:
            failure = error
        finally:
            try:
                forced = process is not None and cleanup(process, given.cleanup_seconds, log)
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
            print(f"stdout:\n{stdout}\nstderr:\n{stderr}\nexit: {None if process is None else process.returncode}",
                  file=log, flush=True)
        if failure is not None:
            raise failure
        return stdout


CGROUPS = Path("/sys/fs/cgroup")


def process_cgroup(pid: int) -> Path:
    rows = Path(f"/proc/{pid}/cgroup").read_text().splitlines()
    groups = [line[3:] for line in rows if line.startswith("0::")]
    if len(groups) != 1 or not groups[0].startswith("/") or ".." in Path(groups[0]).parts:
        raise Failed(f"process {pid} has no unambiguous cgroup v2 path: {rows}")
    if groups[0].endswith(" (deleted)"):
        raise Failed(f"process {pid} belongs to a deleted cgroup: {groups[0]}")
    return CGROUPS / groups[0].lstrip("/")


def memory_limits(group: Path, root: Path = CGROUPS) -> tuple[MemoryLimit, ...]:
    if not group.is_relative_to(root) or ".." in group.parts:
        raise Failed(f"cgroup {group} is outside {root}")
    rows = []
    for _ in range(32):
        if group == root:
            return tuple(rows)  # The unified root has no memory.max/high/swap.max.
        rows.append(MemoryLimit(str(group), *(group.joinpath(name).read_text().strip()
                                              for name in ("memory.max", "memory.high", "memory.swap.max"))))
        group = group.parent
    raise Failed("cgroup ancestry exceeded 32 levels")


def preserve_ancestors(origin: tuple[MemoryLimit, ...], destination: Path) -> None:
    # Moving out of a finite shared ancestor and merely copying its per-child limit
    # would lose the ancestor's aggregate ceiling. Refuse that handoff entirely.
    for row in origin:
        if destination.is_relative_to(Path(row.path)):
            continue
        for name, value in (("memory.max", row.maximum), ("memory.high", row.high), ("memory.swap.max", row.swap)):
            if value != "max":
                raise Failed(f"scope {destination} would leave {row.path}'s {name}={value}")


def systemd(*args: str, seconds: float = 10.0, absent: bool = False) -> dict[str, str]:
    argv = ["systemctl", "--user", "--no-pager", "--no-ask-password", *args]
    try:
        result = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True,
                                text=True, timeout=seconds)
    except subprocess.TimeoutExpired as error:
        raise CommandTimeout(f"systemd command {args} did not finish within {seconds:g} seconds") from error
    properties = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
    if result.returncode != 0 and not (absent and properties.get("LoadState") == "not-found"):
        raise Failed(f"systemd command {args} exited {result.returncode}: {result.stderr.strip()}",
                     result.returncode)
    return properties


def unit_properties(unit: str, *, absent: bool = False, seconds: float = 10.0) -> dict[str, str]:
    return systemd("show", unit, "--property=Id", "--property=LoadState", "--property=ActiveState",
                   "--property=ControlGroup", "--property=InvocationID", absent=absent, seconds=seconds)


def scope_path(properties: dict[str, str], unit: str) -> Path:
    value = properties.get("ControlGroup", "")
    if (properties.get("Id") != unit or not value.startswith("/") or ".." in Path(value).parts
            or Path(value).name != unit):
        raise Failed(f"systemd did not identify the requested scope {unit}: {properties}")
    return CGROUPS / value.lstrip("/")


def evidence(given: Input, name: str, value: object) -> None:
    path = Path(given.log).with_name(name)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def configured_scope(scope: WorkspaceScope) -> None:
    if os.environ.get("BLAZE_DAEMON_MEMORY_GB") != scope.memory_gb:
        raise Failed("workspace scope budget differs from explicit BLAZE_DAEMON_MEMORY_GB")


def scope_proof(scope: WorkspaceScope, properties: dict[str, str], group: Path,
                limits: tuple[MemoryLimit, ...], memlock: tuple[int, int]) -> dict[str, object]:
    expected = scope_path(properties, scope.unit)
    if group != expected:
        raise Failed(f"workspace supervisor is in {group}, expected owned scope {expected}")
    invocation = properties.get("InvocationID", "")
    if not re.fullmatch(r"[0-9a-f]{32}", invocation) or properties.get("ActiveState") != "active":
        raise Failed(f"scope {scope.unit} has no active invocation identity: {properties}")
    if not limits or limits[0].path != str(group) or limits[0].maximum != str(scope.memory_bytes):
        raise Failed(f"scope {scope.unit} does not enforce memory.max={scope.memory_bytes}: {limits}")
    preserve_ancestors(scope.origin, group)
    if memlock != scope.memlock:
        raise Failed(f"scope lost inherited locked-memory limits: {memlock}, expected {scope.memlock}")
    effective = {}
    for name, field in (("memory.max", "maximum"), ("memory.high", "high"), ("memory.swap.max", "swap")):
        values = [int(value) for row in limits if (value := getattr(row, field)) != "max"]
        effective[name] = min(values) if values else "max"
    if int(effective["memory.max"]) > scope.memory_bytes:
        raise Failed(f"scope effective memory limit exceeds {scope.memory_bytes}: {effective}")
    return {"unit": scope.unit, "invocation_id": invocation, "cgroup": str(group),
            "supervisor_pid": os.getpid(), "configured_memory_bytes": scope.memory_bytes,
            "effective_limits": effective, "limits": [asdict(row) for row in limits],
            "original_limits": [asdict(row) for row in scope.origin], "memlock": list(memlock)}


def workload(given: Input, daemon: subprocess.Popen, log, proof: dict[str, object]) -> None:
    scope = given.scope
    assert scope is not None
    group = Path(str(proof["cgroup"]))
    if process_cgroup(daemon.pid) != group:
        raise Failed(f"owned daemon {daemon.pid} is outside workspace scope {group}")
    # ready() has already tied the real server's status PID to this waitable child.
    proof.update(daemon_pid=daemon.pid, daemon_cgroup=str(group))
    evidence(given, "workspace-scope.json", proof)
    print(f"workspace scope proved: {scope.unit}; daemon {daemon.pid}; "
          f"memory.max {scope.memory_bytes} bytes", flush=True)
    process = None
    failure = None
    try:
        with deferred_interrupts() as inherited:
            process = subprocess.Popen(scope.command, stdin=subprocess.DEVNULL, start_new_session=True,
                                       preexec_fn=lambda: signal.pthread_sigmask(signal.SIG_SETMASK, inherited))
        if process_cgroup(process.pid) != group:
            raise Failed(f"workspace command {process.pid} did not inherit scope {group}")
        proof["workload_pid"] = process.pid
        evidence(given, "workspace-scope.json", proof)
        deadline = time.monotonic() + scope.seconds
        while True:
            result = exited(process)
            if result is not None:
                if result != 0:
                    raise Failed(f"workspace command exited with status {result}", result)
                alive(daemon, "after workspace commands")
                if process_cgroup(daemon.pid) != group:
                    raise Failed(f"owned daemon {daemon.pid} left workspace scope {group}")
                break
            alive(daemon, "during workspace commands")
            left = deadline - time.monotonic()
            if left <= 0:
                raise CommandTimeout(f"workspace command did not finish within {scope.seconds:g} seconds")
            time.sleep(min(0.05, left))
    except (Failed, OSError) as error:
        failure = error
    finally:
        try:
            forced = process is not None and cleanup(process, given.cleanup_seconds, log)
            if forced and failure is None:
                failure = Failed("workspace command needed forced process-group cleanup")
        except (Failed, OSError) as error:
            print(f"workspace cleanup error: {error}", file=log, flush=True)
            if failure is None:
                failure = error
        proof["workload_status"] = None if process is None else process.returncode
        try:
            evidence(given, "workspace-scope.json", proof)
        except OSError as error:
            print(f"workspace evidence error: {error}", file=log, flush=True)
            if failure is None:
                failure = error
    if failure is not None:
        raise failure


def entered_scope(given: Input) -> None:
    scope = given.scope
    assert scope is not None and scope.entered
    configured_scope(scope)
    group = process_cgroup(os.getpid())
    proof = scope_proof(scope, unit_properties(scope.unit), group, memory_limits(group),
                        resource.getrlimit(resource.RLIMIT_MEMLOCK))
    # Write ownership before any daemon/command can be spawned. The outer owner
    # requires this exact supervisor PID and InvocationID before unit cleanup.
    evidence(given, "workspace-scope.json", proof)
    with Path(given.log).with_name("scope-entry.commands.log").open("w") as log:
        if status_pid(command(given, ["status", "--json"], given.status_seconds, log)) is not None:
            raise Failed("a daemon already exists before the owned workspace daemon; refusing to use or stop it")
    failure = None
    try:
        run(given, work=lambda daemon, log: workload(given, daemon, log, proof))
    except (Failed, OSError) as error:
        failure = error
    finally:
        try:
            final = scope_proof(scope, unit_properties(scope.unit), process_cgroup(os.getpid()),
                                memory_limits(group), resource.getrlimit(resource.RLIMIT_MEMLOCK))
            if final["invocation_id"] != proof["invocation_id"]:
                raise Failed(f"workspace scope {scope.unit} changed invocation identity")
            proof["final_limits"] = final["limits"]
            proof["memory_events"] = (group / "memory.events").read_text()
            proof["memory_peak"] = (group / "memory.peak").read_text().strip()
            remaining = [int(pid) for pid in (group / "cgroup.procs").read_text().split() if int(pid) != os.getpid()]
            proof["remaining_pids"] = remaining
            evidence(given, "workspace-scope.json", proof)
            if remaining:
                raise Failed(f"workspace scope {scope.unit} still contains processes {remaining}")
            if any(int(line.split()[1]) for line in str(proof["memory_events"]).splitlines()
                   if line.split()[0] in ("oom", "oom_kill", "oom_group_kill")):
                raise Failed(f"workspace scope {scope.unit} recorded an OOM: {proof['memory_events']}")
        except (Failed, OSError) as error:
            print(f"scope final proof error: {error}", file=sys.stderr, flush=True)
            if failure is None:
                failure = error
    if failure is not None:
        raise failure


def populated(group: Path) -> bool:
    try:
        rows = (group / "cgroup.events").read_text().splitlines()
    except FileNotFoundError:
        if not group.exists():
            return False  # systemd already removed this empty cgroup.
        raise  # Missing controls in a still-existing group are not an empty proof.
    values = [line.split()[1] for line in rows if line.startswith("populated ")]
    if values not in (["0"], ["1"]):
        raise Failed(f"cgroup {group} has no unambiguous populated state: {rows}")
    return values == ["1"]


def clean_scope(given: Input, process: subprocess.Popen, log) -> bool:
    scope = given.scope
    assert scope is not None
    proof_file = Path(given.log).with_name("workspace-scope.json")
    if not proof_file.exists():
        return False  # Creation/entry failed; no authority to stop any named unit.
    proof = json.loads(proof_file.read_text())
    if proof.get("unit") != scope.unit or proof.get("supervisor_pid") != process.pid:
        raise Failed("scope cleanup refused evidence from a different supervisor or unit")
    properties = unit_properties(scope.unit, absent=True, seconds=given.cleanup_seconds)
    if properties.get("LoadState") == "not-found":
        return False
    if properties.get("InvocationID") and properties["InvocationID"] != proof.get("invocation_id"):
        raise Failed(f"scope cleanup refused a different invocation of {scope.unit}")
    if properties.get("ActiveState") == "inactive" and not properties.get("ControlGroup"):
        return False  # The unit has already released its empty cgroup; no signals.
    if (properties.get("InvocationID") != proof.get("invocation_id")
            or str(scope_path(properties, scope.unit)) != proof.get("cgroup")):
        raise Failed(f"scope cleanup refused a different invocation of {scope.unit}")
    group = Path(str(proof["cgroup"]))
    if not populated(group):
        return False
    print(f"cleanup: terminate proved owned scope {scope.unit}", file=log, flush=True)
    systemd("kill", "--kill-whom=all", "--signal=SIGKILL", scope.unit, seconds=given.cleanup_seconds)
    deadline = time.monotonic() + given.cleanup_seconds
    while populated(group):
        if time.monotonic() >= deadline:
            raise CommandTimeout(f"owned scope {scope.unit} did not empty after SIGKILL")
        time.sleep(0.025)
    return True


def launch_scope(given: Input) -> None:
    scope = given.scope
    assert scope is not None and not scope.entered
    configured_scope(scope)
    proof_file = Path(given.log).with_name("workspace-scope.json")
    if proof_file.exists():
        raise Failed(f"scope evidence already exists: {proof_file}")
    origin = memory_limits(process_cgroup(os.getpid()))
    parent = systemd("show", "app.slice", "--property=ControlGroup").get("ControlGroup", "")
    if not parent.startswith("/") or ".." in Path(parent).parts:
        raise Failed(f"user app.slice has no safe control group: {parent}")
    destination = CGROUPS / parent.lstrip("/") / scope.unit
    preserve_ancestors(origin, destination)
    child_scope = replace(scope, entered=True, origin=origin, memlock=resource.getrlimit(resource.RLIMIT_MEMLOCK))
    child = replace(given, scope=child_scope)
    maximum = scope.seconds + given.ready_seconds + given.stop_seconds + given.reap_seconds + 60
    # tightmemory.py uses the same synchronous user-scope interface. Keep this
    # job's existing swap policy: setting its experimental MemorySwapMax=0 here
    # would change the released-compiler gate's separately provisioned swap.
    argv = ["systemd-run", "--user", "--scope", "--quiet", "--no-ask-password",
            "--expand-environment=no", "--slice=app.slice", f"--unit={scope.unit}",
            f"--property=MemoryMax={scope.memory_bytes}", "--property=MemoryAccounting=yes",
            f"--property=RuntimeMaxSec={maximum:g}s", "--", sys.executable,
            str(Path(__file__).resolve()), json.dumps(asdict(child))]
    with Path(given.log).with_name("scope-launch.commands.log").open("w") as log:
        # The setup action has explicitly stopped/reaped its preflight. Refuse a
        # later/outside daemon instead of sending a stop to a process we do not own.
        if status_pid(command(given, ["status", "--json"], given.status_seconds, log)) is not None:
            raise Failed("a daemon exists after the setup preflight; refusing an outside workspace daemon")
        evidence(given, "workspace-scope-launch.json",
                 {"argv": argv, "destination": str(destination), "origin": [asdict(row) for row in origin],
                  "memory_bytes": scope.memory_bytes, "memlock": list(child_scope.memlock),
                  "outside_daemon": None})
        process = None
        failure = None
        try:
            with deferred_interrupts() as inherited:
                process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, start_new_session=True,
                                           preexec_fn=lambda: signal.pthread_sigmask(signal.SIG_SETMASK, inherited))
            result = wait_exit(process, maximum)
            if result != 0:
                raise Failed(f"workspace scope command exited with status {result}", result)
            if not proof_file.exists():
                raise Failed("workspace scope returned success without its cap/membership proof")
        except subprocess.TimeoutExpired:
            failure = CommandTimeout(f"workspace scope did not finish within {maximum:g} seconds")
        except (Failed, OSError) as error:
            failure = error
        finally:
            try:
                with deferred_interrupts():
                    try:
                        # Keep the waitable leader pinned while checking scope ownership.
                        forced = process is not None and clean_scope(given, process, log)
                        if forced and failure is None:
                            failure = Failed("workspace scope needed forced unit cleanup")
                    except (Failed, OSError, ValueError) as error:
                        print(f"scope cleanup error: {error}", file=log, flush=True)
                        if failure is None:
                            failure = error
                    try:
                        forced = process is not None and cleanup(process, given.cleanup_seconds, log)
                        if forced and failure is None:
                            failure = Failed("workspace scope needed forced process-group cleanup")
                    except (Failed, OSError) as error:
                        print(f"scope process cleanup error: {error}", file=log, flush=True)
                        if failure is None:
                            failure = error
            except (Failed, OSError) as error:
                # A queued cancellation is delivered when cleanup restores the
                # signal mask. Cleanup is complete; retain any earlier work error.
                print(f"scope cleanup interruption: {error}", file=log, flush=True)
                if failure is None:
                    failure = error
        if failure is not None:
            raise failure


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


def run(given: Input, work=None) -> None:
    daemon_log = Path(given.log)
    commands_log = daemon_log.with_name(daemon_log.name + ".commands.log")
    failure = None
    process = None
    try:
        with daemon_log.open("wb") as output, commands_log.open("w") as commands:
            try:
                with deferred_interrupts() as inherited:
                    process = subprocess.Popen([given.blaze, "--ignore-pin", "serve"],
                                               stdin=subprocess.DEVNULL, stdout=output, stderr=output,
                                               start_new_session=True,
                                               preexec_fn=lambda: signal.pthread_sigmask(signal.SIG_SETMASK, inherited))
                print(f"starting owned seed daemon {process.pid}", file=commands, flush=True)
                ready(given, process, commands)
                sampler(given, process, commands)
                if work is not None:
                    try:
                        work(process, commands)
                    except (Failed, OSError) as error:
                        # A failed build still requests the owned daemon's normal
                        # shutdown; a later shutdown failure cannot replace it.
                        failure = error
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
                if failure is None:
                    failure = error
                else:
                    print(f"shutdown error after workspace failure: {error}", file=commands, flush=True)
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
        if given.scope is None:
            run(given)
        elif given.scope.entered:
            entered_scope(given)
        else:
            launch_scope(given)
        return 0
    except (Failed, OSError, TypeError, ValueError) as error:
        print(f"::error::seed preflight: {error}", file=sys.stderr, flush=True)
        return error.returncode if isinstance(error, Failed) else 1


if __name__ == "__main__":
    raise SystemExit(main())
