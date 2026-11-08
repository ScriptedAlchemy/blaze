#!/usr/bin/env python3
"""distv2.py: a real V2 build of a workspace across two real daemons, and its cache.

Input, one JSON value (`python3 harness/distv2.py '{...}'`):
    blaze        the blaze binary under test
    dist_local   the `blaze-dist-local` binary (coordinator)
    serve        the `blaze-serve` binary (cache server)
    scratch      a new directory for homes, stores, sockets and retained evidence
    project      (optional) the workspace to build; default harness/distv2/fixture
    args         (optional) arguments of `blaze build`
    program      (optional) the path of the program, relative to its build directory's debug/;
                 default "app"
    run          (optional) the arguments it is run with; default ["11"]
    faults       (optional) run the fault cases; default true
    profile      (optional) the directory under the target directory programs are in; default "debug"
    stall_bound  (optional) seconds a daemon's loop may make no progress (`blaze serve --stall-bound`):
                 an unoptimized blaze under test needs more than the default
    timeout      (optional) bound in seconds for each build/fault stage; default 3600
    command_timeout (optional) seconds for status, stop and running the program; default 60
    env          (optional) explicit toolchain/runtime environment for every child
    cores, daemon_memory_gb (optional) per-daemon resource hints; otherwise preserve the caller's
                 explicit BLAZE_CORES / BLAZE_DAEMON_MEMORY_GB; no harness resource cap

Checks, each on what the build itself reports (BLAZE_DIST_REPORT) and on running the program:
    solo         one daemon, no coordinator: the program the others are held to
    two workers  two daemons build at once: every unit done once by agreement, both compile, both
                 programs behave as solo's; no store key is compiled twice, including the tail
    cold cache   a third daemon with an empty store takes published units and compiles none
    warm         a build with nothing changed reports no execution
    edit         one edited function: only what it reaches is worked again
    other PR     another checkout, in a pull request's namespace, with one crate edited: the
                 units it shares with what was published come from the cache, and the edited
                 ones compile; a newer revision of that pull request supersedes an older one
    worker lost  a daemon killed in the middle: the other finishes alone, same program
    fenced       a daemon stopped past its lease and resumed: its work is taken back, it cannot
                 commit after that, and both end with the program of a build alone
    no carrier   with no coordinator the build stops naming it; `fallback` compiles here
    bad cache    a pack on the cache server damaged: the check refuses it, and `fallback` builds

Every program is run: what it prints covers a panic unwinding across units, a static of another
crate, and a build script's output.
"""
from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

from distv2 import evidence
from distv2.evidence import Failed

HERE = Path(__file__).resolve().parent


@dataclasses.dataclass(frozen=True)
class Input:
    blaze: str
    dist_local: str
    serve: str
    scratch: str
    project: str = str(HERE / "distv2" / "fixture")
    args: tuple[str, ...] = ()
    program: str = "app"
    run: tuple[str, ...] = ("11",)
    faults: bool = True
    profile: str = "debug"
    stall_bound: int | None = None
    timeout: int = 3600
    command_timeout: int = 60
    env: dict[str, str] = dataclasses.field(default_factory=dict)
    cores: int | None = None
    daemon_memory_gb: int | None = None


def load(text: str) -> Input:
    raw = json.loads(sys.stdin.read() if text == "-" else (Path(text).read_text() if not text.lstrip().startswith("{") else text))
    for name in ("args", "run"):
        if name in raw:
            raw[name] = tuple(raw[name])
    given = Input(**raw)
    if any(type(v) is not int or v <= 0 for v in (given.timeout, given.command_timeout)):
        raise Failed("timeout and command_timeout must be positive")
    if any(v is not None and (type(v) is not int or v <= 0) for v in (given.cores, given.daemon_memory_gb)):
        raise Failed("cores and daemon_memory_gb must be positive integers when supplied")
    if not isinstance(given.env, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in given.env.items()):
        raise Failed("env must map strings to strings")
    if any(name.startswith("BLAZE_DIST_") for name in given.env):
        raise Failed("env must not override BLAZE_DIST_*: distribution and policies are controlled case inputs")
    return given


def check(ok: bool, what: str) -> None:
    print(("ok   " if ok else "FAIL ") + what, flush=True)
    if not ok:
        raise Failed(what)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def host_triple(blaze: str, env: dict, timeout: int) -> str:
    out = subprocess.run([blaze, "toolchain", "--json"], env=env, capture_output=True, text=True, check=True, timeout=timeout)
    return json.loads(out.stdout)["host"]


class Run:
    def __init__(self, given: Input):
        self.given = given
        self.root = Path(given.scratch).resolve()
        # Never wipe an earlier measurement or another lane's state.
        if self.root.exists() and any(self.root.iterdir()):
            raise Failed(f"scratch {self.root} is not empty; use a new run directory")
        (self.root / "logs").mkdir(parents=True, exist_ok=True)
        self.socket = Path("/tmp") / f"blzd-{os.getpid()}.sock"
        self.port = free_port()
        self.daemons: dict[str, subprocess.Popen] = {}
        self.services: list[subprocess.Popen] = []
        self.clients: list[tuple[subprocess.Popen, float, str]] = []
        self.client_homes: dict[int, str] = {}
        self.suspended: set[int] = set()
        self.lease: list[str] = []
        self.sequence = 0
        self.artifacts: list[dict] = []

    def provenance(self) -> None:
        repository = HERE.parent
        def git(*args):
            return subprocess.check_output(["git", "-C", str(repository), *args], timeout=self.given.command_timeout)
        diff = git("diff", "HEAD", "--binary")
        (self.root / "source.patch").write_bytes(diff)
        identities = {}
        for name in ("blaze", "dist_local", "serve"):
            path = Path(getattr(self.given, name)).resolve(strict=True)
            identities[name] = {"path": str(path), "sha256": file_hash(path)}
        project = Path(self.given.project).resolve()
        files = sorted(project.rglob("*"))
        fixture = hashlib.sha256()
        for path in files:
            if path.is_file() and not {"target", ".git"}.intersection(path.relative_to(project).parts):
                fixture.update(str(path.relative_to(project)).encode() + b"\0" + path.read_bytes())
        record = {"input": dataclasses.asdict(self.given), "commit": git("rev-parse", "HEAD").decode().strip(),
                  "tree": git("rev-parse", "HEAD^{tree}").decode().strip(), "patch_sha256": hashlib.sha256(diff).hexdigest(),
                  "binaries": identities, "fixture_sha256": fixture.hexdigest(),
                  "resources": {k: v for k, v in self.env("solo", False).items()
                                if k in ("BLAZE_CORES", "BLAZE_DAEMON_MEMORY_GB")}}
        (self.root / "provenance.json").write_text(json.dumps(record, indent=2) + "\n")
        self.identities = identities

    def events(self) -> list[dict]:
        events = evidence.jsonl(self.root / "logs" / "events.jsonl", live=True)
        evidence.validate_events(events)
        return events

    def until(self, what: str, predicate, processes=(), timeout: float | None = None):
        deadline = time.monotonic() + (timeout or self.given.timeout)
        while time.monotonic() < deadline:
            answer = predicate()
            if answer:
                return answer
            ended = [p.pid for p in processes if p.poll() is not None]
            if ended:
                raise Failed(f"{what}: children {ended} exited before the required evidence; see {self.root / 'logs'}")
            time.sleep(0.02)
        raise Failed(f"{what}: timed out; see {self.root / 'logs'}")

    def wait(self, process: subprocess.Popen) -> int:
        _, deadline, log = next(it for it in self.clients if it[0] is process)
        try:
            code = process.wait(timeout=max(0, deadline - time.monotonic()))
            # Keep stores and targets, but release idle front ends before the next stage.
            self.terminate(self.daemons[self.client_homes[process.pid]], timeout=self.given.command_timeout)
            return code
        except subprocess.TimeoutExpired as why:
            raise Failed(f"build pid {process.pid} exceeded {self.given.timeout}s; log {log}") from why

    @staticmethod
    def terminate(process: subprocess.Popen, timeout: int = 10) -> None:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGCONT)
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=10)
        if process.stdin:
            process.stdin.close()

    def env(self, home: str, dist: bool, **extra) -> dict:
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CARGO", "RUST", "BLAZE_"))}
        env.update(self.given.env)
        for name, value in (("BLAZE_CORES", self.given.cores), ("BLAZE_DAEMON_MEMORY_GB", self.given.daemon_memory_gb)):
            if value is not None:
                env[name] = str(value)
            elif name not in env and name in os.environ:
                env[name] = os.environ[name]
        env.update(BLAZE_HOME=str(self.root / home), TMPDIR=str(self.root / "tmp"), RUST_BACKTRACE="0", BLAZE_DIST_EVENTS=str(self.root / "logs" / "events.jsonl"))
        (self.root / "tmp").mkdir(exist_ok=True)
        if dist:
            env.update(
                BLAZE_DIST_SOCKET=str(self.socket),
                BLAZE_DIST_BLOBS=f"127.0.0.1:{self.port}",
                BLAZE_DIST_REPORT=str(self.report_path(home)),
            )
        env.update(extra)
        return env

    def report_path(self, home: str) -> Path:
        return self.root / "logs" / f"{home}.report.jsonl"

    def report(self, home: str) -> list[dict]:
        path = self.report_path(home)
        return evidence.jsonl(path)

    def start_services(self) -> None:
        log = open(self.root / "logs" / "serve.log", "w")
        self.services.append(
            subprocess.Popen(
                [self.given.serve, "--dir", str(self.root / "blobs"), "--listen", f"127.0.0.1:{self.port}"],
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        )
        log.close()
        self.start_coordinator()
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)
        else:
            raise Failed("the cache server did not listen")

    def start_coordinator(self) -> None:
        triple = host_triple(self.given.blaze, self.env("solo", False), self.given.command_timeout)
        log = open(self.root / "logs" / "coordinator.log", "a")
        self.socket.unlink(missing_ok=True)
        coordinator = subprocess.Popen(
            [self.given.dist_local, "coordinate", str(self.socket), triple, "64", *self.lease],
            stdin=subprocess.PIPE,
            env=self.env("coordinator", False),
            start_new_session=True,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        log.close()
        self.coordinator = coordinator
        self.services.append(coordinator)
        self.until("coordinator binds its socket", self.socket.exists, [coordinator], timeout=30)

    def daemon(self, home: str) -> None:
        """Own every daemon explicitly, including ones whose startup fails."""
        if home in self.daemons and self.daemons[home].poll() is None:
            return
        # Status is only for this run's private home, never a shared daemon.
        log = open(self.root / "logs" / f"{home}.daemon.log", "a")
        args = [self.given.blaze, "serve"]
        if self.given.stall_bound is not None:
            args += ["--stall-bound", str(self.given.stall_bound)]
        with log:
            daemon = subprocess.Popen(args, cwd=self.project, env=self.env(home, False),
                                      stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        self.daemons[home] = daemon
        self.until(f"{home} daemon becomes ready", lambda: daemon_pid(self, home) == daemon.pid, [daemon], timeout=300)

    def build(self, home: str, dist: bool = True, wait: bool = True, project: Path | None = None, **extra):
        self.daemon(home)
        self.sequence += 1
        log = open(self.root / "logs" / f"{home}.{self.sequence}.log", "w")
        process = subprocess.Popen(
            [self.given.blaze, "build", *self.given.args],
            cwd=project or self.project,
            env={**self.env(home, dist, **extra), "CARGO_TARGET_DIR": str(self.target(home, project))},
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        log.close()
        self.clients.append((process, time.monotonic() + self.given.timeout, log.name))
        self.client_homes[process.pid] = home
        if not wait:
            return process, log.name
        code = self.wait(process)
        return code, Path(log.name).read_text()

    def target(self, home: str, project: Path | None = None) -> Path:
        """Each daemon's target directory of a checkout, as a person on that machine would have."""
        return self.root / "targets" / home / (project or self.project).name

    def program(self, home: str, project: Path | None = None) -> Path:
        return self.target(home, project) / self.given.profile / self.given.program

    def behaviour(self, home: str, project: Path | None = None) -> tuple[str, str]:
        program = self.program(home, project)
        raw_hash = file_hash(program)
        saved = self.root / "artifacts" / raw_hash
        saved.parent.mkdir(exist_ok=True)
        if not saved.exists():
            shutil.copy2(program, saved)
        ran = subprocess.run([str(program), *self.given.run], capture_output=True, text=True,
                             timeout=self.given.command_timeout)
        self.artifacts.append({"home": home, "project": str(project or self.project), "path": str(program),
                               "sha256": raw_hash, "retained": str(saved), "stdout": ran.stdout, "stderr": ran.stderr, "exit": ran.returncode})
        (self.root / "artifacts.json").write_text(json.dumps(self.artifacts, indent=2) + "\n")
        check(ran.returncode == 0, f"{home} artifact runs successfully (exit {ran.returncode}; see artifacts.json)")
        return ran.stdout, raw_hash

    def stop(self) -> None:
        # A failure while a daemon is SIGSTOPped must not strand it or its client.
        for pid in self.suspended:
            try:
                os.kill(pid, signal.SIGCONT)
            except ProcessLookupError:
                pass
        failures = []
        for process in [*(p for p, _, _ in self.clients), *self.daemons.values(), *self.services]:
            try:
                self.terminate(process)
            except (OSError, subprocess.TimeoutExpired) as why:
                failures.append(f"pid {process.pid}: {why}")
        self.socket.unlink(missing_ok=True)
        if failures:
            raise Failed("owned-process cleanup failed: " + "; ".join(failures))

    def suspend_grant(self, home: str, process: subprocess.Popen, after: int) -> dict:
        pid = self.daemons[home].pid
        def suspend():
            events = self.events()[after:]
            for grant in reversed(events):
                if grant["event"] != "grant" or grant["pid"] != pid or not grant["work"]:
                    continue
                if evidence.accepted(events, grant):
                    continue
                os.kill(pid, signal.SIGSTOP)
                self.suspended.add(pid)
                self.until(f"{home} acknowledges SIGSTOP",
                           lambda: stopped_status(pid), [process], timeout=self.given.command_timeout)
                # Catch the race where a result was accepted between observing and stopping.
                # No next batch can run until SIGCONT, so retry only a genuinely live grant.
                if evidence.accepted(self.events()[after:], grant):
                    os.kill(pid, signal.SIGCONT)
                    self.suspended.remove(pid)
                    return None
                return grant
            return None
        return self.until(f"{home} holds uncommitted compile work (requires BLAZE_DIST_EVENTS)", suspend, [process])


def daemon_pid(run: Run, home: str) -> int | None:
    out = subprocess.run(
        [run.given.blaze, "status", "--json"], cwd=run.project, env=run.env(home, False), capture_output=True, text=True, timeout=run.given.command_timeout
    )
    try:
        return (json.loads(out.stdout).get("daemon") or {}).get("pid")
    except json.JSONDecodeError:
        return None


def file_hash(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def stopped_status(pid: int) -> bool:
    child, status = os.waitpid(pid, os.WUNTRACED | os.WNOHANG)
    if child and not os.WIFSTOPPED(status):
        raise Failed(f"daemon {pid} exited while injecting SIGSTOP: status {status}")
    return bool(child)


def sums(rows: list[dict], name: str) -> int:
    return sum(row[name] for row in rows)


def edit(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    if old not in text:
        raise Failed(f"{path} has no {old!r}")
    path.write_text(text.replace(old, new))


def control(run: Run, project: Path | None = None) -> tuple[str, str]:
    code, text = run.build("solo", dist=False, project=project)
    check(code == 0, f"solo control builds: {text[-800:] if code else ''}")
    return run.behaviour("solo", project)


def faults(run: Run, edited: Path) -> None:
    # Freeze a known uncommitted grant before killing: a completed unit/report is no evidence
    # that the victim had outstanding work. Give the survivor the exact same edited input.
    edit(edited, 'format!("7:{sum}', 'format!("7 lost:{sum}')
    wanted = control(run)
    run.daemon("a")
    run.daemon("b")
    mark = len(run.events())
    lost, lost_log = run.build("a", wait=False)
    survivor, survivor_log = run.build("b", wait=False)
    grant = run.suspend_grant("a", lost, mark)
    os.kill(grant["pid"], signal.SIGKILL)
    killed_at = len(run.events()) - mark
    run.suspended.discard(grant["pid"])
    check(run.wait(lost) != 0, f"the client of killed daemon {grant['pid']} fails (log {lost_log})")
    run.until("survivor commits every name held by the killed worker",
              lambda: evidence.recovered(run.events()[mark:], grant, accepted_after=killed_at), [survivor])
    code = run.wait(survivor)
    check(code == 0, f"the survivor finishes within the bound: {Path(survivor_log).read_text()[-800:] if code else ''}")
    check(not evidence.accepted(run.events()[mark:], grant), "the killed worker's outstanding result was never committed")
    check(run.behaviour("b") == wanted, "worker-death recovery equals solo in bytes and behavior")
    edit(edited, 'format!("7 lost:{sum}', 'format!("7:{sum}')

    # A short lease is a test input, not a timing inference: keep the victim stopped until
    # the coordinator accepts replacement results for every one of its outstanding names.
    run.terminate(run.coordinator)
    run.lease = ["2000"]
    run.start_coordinator()
    edit(edited, 'format!("8:{sum}', 'format!("8 fence:{sum}')
    wanted = control(run)
    run.daemon("a")
    run.daemon("b")
    mark = len(run.events())
    stalled, stalled_log = run.build("a", wait=False)
    healthy, healthy_log = run.build("b", wait=False)
    grant = run.suspend_grant("a", stalled, mark)
    run.until("replacement commits while stale holder remains stopped",
              lambda: evidence.recovered(run.events()[mark:], grant), [healthy])
    check(not evidence.accepted(run.events()[mark:], grant), "the stopped lease was uncommitted when its work was taken back")
    os.kill(grant["pid"], signal.SIGCONT)
    run.suspended.remove(grant["pid"])
    run.until("resumed holder's actual stale result is explicitly refused",
              lambda: evidence.stale_refused(run.events()[mark:], grant), [stalled])
    code_h, code_s = run.wait(healthy), run.wait(stalled)
    check(code_h == 0, f"the healthy worker finishes: {Path(healthy_log).read_text()[-800:] if code_h else ''}")
    check(code_s == 0, f"the resumed worker finishes: {Path(stalled_log).read_text()[-800:] if code_s else ''}")
    check(not evidence.accepted(run.events()[mark:], grant), "DeadLease/AttemptMoved refused the exact stale batch; it never committed")
    for home in ("a", "b"):
        check(run.behaviour(home) == wanted, f"{home}'s fenced artifact equals solo in bytes and behavior")
    edit(edited, 'format!("8 fence:{sum}', 'format!("8:{sum}')

    # Damage a referenced pack, not a guessed file or an empty glob. First prove a fresh
    # consumer of this exact manifest succeeds, then change only its stored bytes.
    edit(edited, 'format!("2:{sum}', 'format!("2 damaged:{sum}')
    wanted = control(run)
    before = len(run.report("a"))
    code, text = run.build("a")
    check(code == 0, f"producer publishes the corruption case: {text[-800:] if code else ''}")
    produced = run.report("a")[before:]
    candidates = [r for r in produced if r["unit"] == "mathy" and r["source"] == "executed"]
    check(len(candidates) == 1, "the edited mathy unit was actually executed and published")
    unit = candidates[0]["unit_digest"]
    evidence.publication(run.root / "blobs", produced, run.events())
    packs, _ = evidence.manifest(run.root / "blobs", unit)
    check(bool(packs), "the edited unit's actual manifest references at least one stored pack")
    code, text = run.build("intact", BLAZE_DIST_LOCAL="never")
    check(code == 0, f"intact-cache control builds: {text[-800:] if code else ''}")
    evidence.cached(run.report("intact"))
    check(run.behaviour("intact") == wanted, "the intact-cache artifact equals solo")
    target = packs[sorted(packs)[0]]
    original = target.read_bytes()
    damaged = bytearray(original)
    damaged[len(damaged) // 2] ^= 0xff
    target.write_bytes(damaged)
    (run.root / "corruption.json").write_text(json.dumps({"unit_digest": unit, "pack": target.stem,
        "path": str(target), "before_sha256": hashlib.sha256(original).hexdigest(),
        "damaged_sha256": file_hash(target)}, indent=2) + "\n")
    check(file_hash(target) != target.stem, f"changed real stored pack {target.name}; its hash now disagrees")
    try:
        code, text = run.build("corrupt-never", BLAZE_DIST_LOCAL="never")
        check(code != 0 and "distributed build failed" in text and target.stem in text,
              "explicit never refuses the damaged referenced pack, naming its digest")
        check(not run.program("corrupt-never").exists(), "never produced no artifact from the corrupt cache")
        check(file_hash(target) != target.stem, "never left the same corruption in place for the fallback control")
        code, text = run.build("corrupt-fallback", BLAZE_DIST_LOCAL="fallback")
        check(code == 0, f"explicit fallback builds with the same damaged pack: {text[-800:] if code else ''}")
        check(run.behaviour("corrupt-fallback") == wanted, "fallback's artifact equals solo despite the damaged cache")
    finally:
        # Restore only this harness's byte fault, preserving all manifests and other evidence.
        target.write_bytes(original)
    # Removing the fault must remove the refusal, under the same explicit Never policy.
    code, text = run.build("repaired", BLAZE_DIST_LOCAL="never")
    check(code == 0, f"restoring only pack bytes removes the failure: {text[-800:] if code else ''}")
    evidence.cached(run.report("repaired"))
    check(run.behaviour("repaired") == wanted, "repaired-cache artifact equals solo")
    edit(edited, 'format!("2 damaged:{sum}', 'format!("2:{sum}')

    # No carrier: fresh homes/targets prevent a retained artifact or local cache hit from
    # satisfying either side of the policy check. Both see exactly the same edited input.
    edit(edited, 'format!("9:{sum}', 'format!("9 down:{sum}')
    wanted = control(run)
    run.terminate(run.coordinator)
    run.socket.unlink(missing_ok=True)
    for home, extra in (("down-default", {}), ("down-never", {"BLAZE_DIST_LOCAL": "never"})):
        code, text = run.build(home, **extra)
        check(code != 0 and "distributed build failed" in text, f"{home} stops and names the unavailable carrier")
        check(not run.program(home).exists(), f"{home} emits no artifact")
    code, text = run.build("down-fallback", BLAZE_DIST_LOCAL="fallback")
    check(code == 0, f"explicit fallback builds with no carrier: {text[-800:] if code else ''}")
    check(run.behaviour("down-fallback") == wanted, "no-carrier fallback equals solo in bytes and behavior")
    edit(edited, 'format!("9 down:{sum}', 'format!("9:{sum}')


def main(given: Input) -> None:
    def interrupted(signum, _frame):
        raise Failed(f"received signal {signum}; stopping owned children")
    signal.signal(signal.SIGTERM, interrupted)
    run = Run(given)
    run.project = run.root / "project"
    shutil.copytree(given.project, run.project, ignore=shutil.ignore_patterns("target", ".git"), dirs_exist_ok=True)
    edited = run.project / "mathy" / "src" / "lib.rs"
    try:
        run.provenance()
        resource_env = run.env("solo", False)
        check(all(resource_env.get(name, "").strip() for name in ("BLAZE_CORES", "BLAZE_DAEMON_MEMORY_GB")),
              "root supplied explicit per-daemon core and memory hints before starting children")
        run.start_services()
        code, text = run.build("solo", dist=False)
        check(code == 0, f"solo builds: {text[-400:] if code else ''}")
        solo = run.behaviour("solo")
        print("     solo:", solo[0].strip().replace("\n", " | "), solo[1][:16], flush=True)
        n = int(given.run[0])
        expected = (f"caught {str(n % 2 == 1).lower()}, dropped {n}, script says from the script\n"
                    "included as it was checked in\nseen by the macro 1\n")
        check(solo[0].startswith("40 functions, ") and solo[0].endswith(expected),
              "solo executes 40 functions, the expected unwind/drop, script, included file and proc macro")

        run.daemon("a")
        run.daemon("b")
        a, a_log = run.build("a", wait=False)
        b, b_log = run.build("b", wait=False)
        code_a, code_b = run.wait(a), run.wait(b)
        check(code_a == 0, f"worker a builds: {Path(a_log).read_text()[-800:] if code_a else ''}")
        check(code_b == 0, f"worker b builds: {Path(b_log).read_text()[-800:] if code_b else ''}")
        rows_a, rows_b = run.report("a"), run.report("b")
        check(bool(rows_a) and bool(rows_b), f"both report units: {len(rows_a)} and {len(rows_b)}")
        compiled_a, compiled_b = sums(rows_a, "compiled_for_work"), sums(rows_b, "compiled_for_work")
        print(f"     a compiled {compiled_a} for the work, b {compiled_b}", flush=True)
        check(compiled_a > 0 and compiled_b > 0, "both workers compiled functions for the others")
        for name, rows in (("a", rows_a), ("b", rows_b)):
            check({"pm", "shapes", "mathy", "app"} <= {r["unit"] for r in rows}, f"{name} reports every fixture crate")
        evidence.no_duplicate_compile(rows_a + rows_b)
        by_a = {row["unit_digest"]: row for row in rows_a}
        shared = [
            row
            for row in rows_b
            if row["unit_digest"] in by_a
            and by_a[row["unit_digest"]]["execution"] == row["execution"] and row["execution"] >= 0
            and by_a[row["unit_digest"]]["attempt"] == row["attempt"]
            and row["source"] == by_a[row["unit_digest"]]["source"] == "executed"
            and row["joined"] and by_a[row["unit_digest"]]["joined"]
            and row["compiled_for_work"] > 0 and by_a[row["unit_digest"]]["compiled_for_work"] > 0
        ]
        print(f"     one execution of both: {sorted(row['unit'] for row in shared)}", flush=True)
        check(bool(shared), "both daemons actually compile the same unit in the same execution and attempt")
        evidence.publication(run.root / "blobs", rows_a + rows_b, run.events())
        check(True, "no store key was compiled twice, including the tail; every compiled key was committed in the full manifest")
        for name, behaviour in (("a", run.behaviour("a")), ("b", run.behaviour("b"))):
            check(behaviour[0] == solo[0], f"worker {name}'s program behaves as solo's")
            check(behaviour[1] == solo[1], f"worker {name}'s program is solo's, byte for byte")

        code, text = run.build("cold")
        check(code == 0, f"a third daemon builds from the cache: {text[-400:] if code else ''}")
        rows = run.report("cold")
        published = [row for row in rows if row["source"] == "published"]
        print(f"     cold: {len(published)}/{len(rows)} published; compiled {sums(rows, 'tail_compiled')} of {sums(rows, 'instances')}", flush=True)
        check(len(published) == len(rows) and rows, f"every unit of the cold daemon was published: {len(published)}/{len(rows)}")
        evidence.cached(rows)
        evidence.publication(run.root / "blobs", rows, run.events())
        check(True, "cold units schedule no execution or grants and compile no work or tail keys")
        behaviour = run.behaviour("cold")
        check(behaviour[0] == solo[0] and behaviour[1] == solo[1], "its program is solo's, byte for byte")

        before = len(run.report("cold"))
        code, _ = run.build("cold")
        check(code == 0 and len(run.report("cold")) == before, "a build with nothing changed runs no unit")

        # One function edited: what it reaches is worked again, by the same keys.
        edit(edited, 'format!("3:{sum}', 'format!("3 edited:{sum}')
        rows_before = len(run.report("a"))
        code, text = run.build("a")
        check(code == 0, f"an edit builds: {text[-400:] if code else ''}")
        new = run.report("a")[rows_before:]
        work = sums(new, "compiled_for_work") + sums(new, "tail_compiled")
        check(0 < work < sums(rows_a, "compiled_for_work") + sums(rows_b, "compiled_for_work"), f"the edit recompiled {work} functions, not the whole workspace")
        check(run.behaviour("a")[0] != solo[0], "the edited program changes its output")
        code, text = run.build("solo", dist=False)
        check(code == 0, f"the edit control builds: {text[-400:] if code else ''}")
        check(run.behaviour("a") == run.behaviour("solo"), "the edited artifact equals solo in bytes and behavior")
        edit(edited, 'format!("3 edited:{sum}', 'format!("3:{sum}')

        # A file a unit includes is an input of its keys, and so a new unit: nothing stale is taken.
        included = run.project / "shapes" / "included.txt"
        keep = included.read_text()
        included.write_text("changed by the edit")
        code, text = run.build("cold")
        check(code == 0, f"an edit of an included file builds: {text[-400:] if code else ''}")
        check("included changed by the edit" in run.behaviour("cold")[0], "and the program has the new text, not a published one's")
        code, text = run.build("solo", dist=False)
        check(code == 0, f"the include control builds: {text[-400:] if code else ''}")
        check(run.behaviour("cold") == run.behaviour("solo"), "the include edit equals solo in bytes and behavior")
        included.write_text(keep)

        # Another pull request's checkout, somewhere else, with one crate edited: what it shares
        # by content comes from the cache, what it does not is compiled and published.
        other = run.root / "project-pr"
        shutil.copytree(run.project, other, ignore=shutil.ignore_patterns("target", ".git"))
        edit(other / "mathy" / "src" / "lib.rs", 'format!("5:{sum}', 'format!("5 pr:{sum}')
        code, text = run.build("pr", project=other, BLAZE_DIST_PR="17", BLAZE_DIST_SOURCE="rev1")
        check(code == 0, f"another pull request builds: {text[-400:] if code else ''}")
        rows = run.report("pr")
        taken = [row for row in rows if row["source"] == "published"]
        print(f"     pr: {len(taken)}/{len(rows)} units published already: {sorted(row['unit'] for row in taken)}", flush=True)
        check(any(row["unit"] == "shapes" for row in taken), "the crate it shares came from the cache")
        evidence.cached(taken)
        check(True, "the PR shares content from main without joining its execution")
        check(any(row["unit"] == "mathy" and row["source"] == "executed" for row in rows), "the edited crate was built")
        check(run.behaviour("pr", other)[0] != solo[0], "its program is the edited one")
        code, text = run.build("solo", dist=False, project=other)
        check(code == 0, f"the PR control builds: {text[-400:] if code else ''}")
        check(run.behaviour("pr", other) == run.behaviour("solo", other), "the PR artifact equals its solo control")

        # An independent PR reuses exactly the first PR's published edited content.
        code, text = run.build("pr-peer", project=other, BLAZE_DIST_PR="19", BLAZE_DIST_SOURCE="peer")
        check(code == 0, f"an independent PR builds the same content: {text[-400:] if code else ''}")
        peer = run.report("pr-peer")
        evidence.cached(peer)
        check({r["unit_digest"] for r in peer} == {r["unit_digest"] for r in rows}, "cross-PR reuse covers the same content identities")
        check(run.behaviour("pr-peer", other) == run.behaviour("pr", other), "cross-PR artifacts agree in bytes and behavior")
        before = len(run.report("pr"))
        code, text = run.build("pr", project=other, BLAZE_DIST_PR="17", BLAZE_DIST_SOURCE="rev1")
        check(code == 0 and len(run.report("pr")) == before, "nothing changed: no unit runs")

        # A newer revision of the pull request while the older builds: the newer one finishes, and
        # the older either finished first or stopped, naming why, with nothing of the newer lost.
        older_at, newer_at = run.root / "project-older", run.root / "project-newer"
        for at, change in ((older_at, 'format!("6 older:{sum}'), (newer_at, 'format!("6 newer:{sum}')):
            shutil.copytree(run.project, at, ignore=shutil.ignore_patterns("target", ".git"))
            edit(at / "mathy" / "src" / "lib.rs", 'format!("6:{sum}', change)
        older, older_log = run.build("a", wait=False, project=older_at, BLAZE_DIST_PR="18", BLAZE_DIST_SOURCE="old")
        time.sleep(0.2)
        newer, newer_log = run.build("b", wait=False, project=newer_at, BLAZE_DIST_PR="18", BLAZE_DIST_SOURCE="new")
        code_newer, code_older = run.wait(newer), run.wait(older)
        check(code_newer == 0, f"the newer revision builds: {Path(newer_log).read_text()[-800:] if code_newer else ''}")
        check(run.behaviour("b", newer_at) == control(run, newer_at), "the newer revision equals its solo control")
        if code_older == 0:
            check(run.behaviour("a", older_at) == control(run, older_at), "the older revision finished as its own exact artifact")
        else:
            check("distributed build failed" in Path(older_log).read_text(), "the older revision stopped naming the distributed path")

        if given.faults:
            faults(run, edited)
        for name, identity in run.identities.items():
            check(file_hash(Path(identity["path"])) == identity["sha256"], f"{name} binary did not change during acceptance")
        provenance = json.loads((run.root / "provenance.json").read_text())
        current_commit = subprocess.check_output(["git", "-C", str(HERE.parent), "rev-parse", "HEAD"],
                                                 timeout=given.command_timeout).decode().strip()
        current_diff = subprocess.check_output(["git", "-C", str(HERE.parent), "diff", "HEAD", "--binary"],
                                               timeout=given.command_timeout)
        check(current_commit == provenance["commit"] and hashlib.sha256(current_diff).hexdigest() == provenance["patch_sha256"],
              "source commit and patch did not change during acceptance")
        print("distv2: every check passed" if given.faults else "distv2: non-fault checks passed; fault acceptance NOT RUN", flush=True)
    finally:
        run.stop()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        main(load(sys.argv[1]))
    except (Failed, OSError, ValueError, TypeError, subprocess.SubprocessError) as failed:
        sys.exit(f"distv2: {failed}")
